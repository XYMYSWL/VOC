//! 语音链路：Opus 编解码、VAD、UDP 收发、P2P 打洞直连、UDP→TCP 自动回退。
//!
//! - 一个 UDP 套接字（尽量双栈 IPv4+IPv6）同时用于和服务器通信、和其他成员直连。
//!   打洞必须用同一个套接字：服务器看到的公网地址就是它的 NAT 映射。
//! - 能直连的成员：语音直接发给对方（成对 ML-KEM 密钥加密，服务器看不到）；
//!   直连不了的成员：发一份给服务器中转。全部直连时服务器完全不经手语音。
//! - UDP 被封锁/严重丢包时（4 秒收不到服务器 pong）语音改走加密 TCP；恢复后自动切回。
//! - 成员中继：互相打不通的两人，由服务器安排一个和双方都直连良好、延迟最低的成员转发语音
//!   （每 2 秒上报链路质量，服务器规划路线，见 voc-server/src/relay.rs）。转发的是端到端加密后的原包。

use crate::audio::{Mixer, PushResampler, Settings, CAPTURE_RATE, LOOPBACK_ID};
use crate::denoise::DenoiseSlot;
use crate::e2e::E2e;
use crate::net::Outgoing;
use crate::video::VideoRx;
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use voc_proto::crypto::{DatagramOpener, DatagramSealer, SessionKeys};
use voc_proto::p2p;
use voc_proto::udp::seq_newer;
use voc_proto::{now_millis, udp, ClientMsg, NatInfo, PeerInfo, PeerStat, RelayRoute, FRAME_SAMPLES, SAMPLE_RATE};

/// VAD 关闭前的保持帧数（20 ms × 15 = 300 ms），避免句尾被截断
const VAD_HANGOVER: u32 = 15;
/// 松开说话键后再多发这么多帧（20 ms × 10 = 200 ms），避免句尾被截断
const PTT_HANGOVER: u32 = 10;

/// 某个键（或鼠标键）现在是否按着。用全局按键状态查询，游戏在前台时也有效，不装键盘钩子。
pub fn key_down(vk: u32) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        return (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0;
    }
    #[allow(unreachable_code)]
    false
}

/// 超过这么久没有服务器 UDP pong 就切到 TCP
const UDP_DEAD_MS: u64 = 4000;
/// 直连对端多久没消息算断开
const PEER_DEAD: Duration = Duration::from_secs(4);
/// 直连对端这么久没消息（连丢两个 ping）就先改走服务器 / 中继，不等到判定断开
const PEER_SUSPECT: Duration = Duration::from_millis(1200);
/// 服务器 UDP 这么久没回 pong：语音同时也走 TCP 发一份，确认之前不断音
const UDP_SUSPECT_MS: u64 = 1500;
/// 直连比经服务器明显绕远时不用直连：直连往返 > 2 × 到服务器往返 + 30 ms
const DIRECT_DETOUR_MS: u32 = 30;
/// 屏幕画面匀速发送：按画面码率的 3 倍（至少 4 Mbps）发，最多连发 4 个包
const PACE_FACTOR: u64 = 3;
const PACE_MIN_BPS: u64 = 4_000_000;
const PACE_BURST: usize = 4 * 1200;
/// 画面包在发送队列里超过这么久就丢掉（宁可丢一帧请求关键帧，也不让延迟越积越多）
const PACE_STALE: Duration = Duration::from_millis(250);
/// Opus 码率
const OPUS_BITRATE: i32 = 24_000;
/// 低带宽模式的语音码率（Opus 在 12 kbps 下人声仍然清楚）
const LOW_OPUS_BITRATE: i32 = 12_000;
/// 麦克风连续这么多帧（20 ms 一帧，共 5 秒）全 0 就提示
const MIC_SILENT_FRAMES: u32 = 250;
/// 端口预测：对方公网端口上下各试几个
const PORT_SPRAY: u16 = 8;
/// 每路直连语音估算占用的上行（Opus 24 kbps + 各层包头 + 随机填充，按 60 kbps 算）
pub const VOICE_COST_BPS: u64 = 60_000;
/// 默认 P2P 上行预算
pub const DEFAULT_BUDGET_BPS: u32 = 15_000_000;
/// 链路质量上报间隔（成员中继用）
const STATS_INTERVAL: Duration = Duration::from_secs(2);
/// 最多帮别人中转几路语音
const MAX_RELAY_SLOTS: u64 = 8;

/// 按上行预算扣掉自己的直连开销后，还能帮别人中转几路语音
pub fn relay_slots(budget_bps: u32, voice_direct: usize, others: usize, video_bps: u32, video_direct: usize, watchers: usize) -> u16 {
    let video_cost = video_bps as u64 * 105 / 100;
    let mut used = voice_direct as u64 * VOICE_COST_BPS + video_direct as u64 * video_cost;
    if voice_direct < others {
        used += VOICE_COST_BPS; // 给服务器的一份
    }
    if video_direct < watchers {
        used += video_cost;
    }
    ((budget_bps as u64).saturating_sub(used) / VOICE_COST_BPS).min(MAX_RELAY_SLOTS) as u16
}

/// 按上行带宽预算决定直连哪些人。
/// - `voice_peers` / `video_peers`：已打通、按延迟从低到高排好的候选（同频道成员 / 观看者）
/// - `others_total` / `watchers_total`：总人数（含打不通的）。只要有人不直连，就要多传一份给服务器，这份也计入预算。
/// 语音优先；剩下的预算给屏幕共享画面。返回 (语音直连名单, 画面直连名单)。
pub fn allocate_direct(
    budget_bps: u32,
    voice_peers: &[u32],
    others_total: usize,
    video_bps: u32,
    video_peers: &[u32],
    watchers_total: usize,
) -> (Vec<u32>, Vec<u32>) {
    fn fit(budget: u64, cost: u64, candidates: usize, total: usize) -> (usize, u64) {
        if total == 0 || cost == 0 {
            return (0, 0);
        }
        let mut k = candidates.min(total);
        loop {
            let used = k as u64 * cost + if k < total { cost } else { 0 };
            if used <= budget || k == 0 {
                return (k, used.min(budget));
            }
            k -= 1;
        }
    }
    let budget = budget_bps as u64;
    let (kv, used) = fit(budget, VOICE_COST_BPS, voice_peers.len(), others_total);
    let video_cost = video_bps as u64 * 105 / 100;
    let (kd, _) = fit(budget.saturating_sub(used), video_cost, video_peers.len(), watchers_total);
    (voice_peers[..kv].to_vec(), video_peers[..kd].to_vec())
}
const MAX_OPUS_PACKET: usize = 400;

// ------------------------------------------------------------ 地址工具

/// 双栈套接字发往 IPv4 地址时要用 IPv4 映射地址
fn wire(addr: SocketAddr, dual: bool) -> SocketAddr {
    match addr {
        SocketAddr::V4(v4) if dual => SocketAddr::new(IpAddr::V6(v4.ip().to_ipv6_mapped()), v4.port()),
        a => a,
    }
}

/// 收到的 IPv4 映射地址还原成 IPv4，便于比较
fn norm(addr: SocketAddr) -> SocketAddr {
    if let SocketAddr::V6(v6) = addr {
        if let Some(v4) = v6.ip().to_ipv4_mapped() {
            return SocketAddr::new(IpAddr::V4(v4), v6.port());
        }
    }
    addr
}

fn make_socket(server: SocketAddr) -> std::io::Result<(UdpSocket, bool)> {
    use socket2::{Domain, Protocol, Socket, Type};
    let dual = (|| -> std::io::Result<UdpSocket> {
        let s = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        s.set_only_v6(false)?;
        // 屏幕共享的关键帧是几十个包一起到，缓冲要大
        let _ = s.set_recv_buffer_size(4 << 20);
        let _ = s.set_send_buffer_size(4 << 20);
        s.bind(&"[::]:0".parse::<SocketAddr>().unwrap().into())?;
        Ok(s.into())
    })();
    match dual {
        Ok(s) => Ok((s, true)),
        Err(_) => {
            let bind = if server.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
            Ok((UdpSocket::bind(bind)?, false))
        }
    }
}

/// 本机候选地址：局域网 IPv4、公网/ULA IPv6（同一端口）。
/// 用「连接」UDP 套接字的方式查询本机路由选择的源地址——UDP connect 不会发出任何数据包。
fn local_candidates(port: u16, dual: bool) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
        if s.connect("8.8.8.8:53").is_ok() {
            if let Ok(a) = s.local_addr() {
                if !a.ip().is_unspecified() && !a.ip().is_loopback() {
                    out.push(SocketAddr::new(a.ip(), port).to_string());
                }
            }
        }
    }
    if dual {
        if let Ok(s) = UdpSocket::bind("[::]:0") {
            if s.connect("[2001:4860:4860::8888]:53").is_ok() {
                if let Ok(SocketAddr::V6(a)) = s.local_addr() {
                    let ip = *a.ip();
                    let link_local = (ip.segments()[0] & 0xffc0) == 0xfe80;
                    if !ip.is_unspecified() && !ip.is_loopback() && !link_local {
                        out.push(SocketAddr::new(IpAddr::V6(ip), port).to_string());
                    }
                }
            }
        }
    }
    out
}

// ------------------------------------------------------------ 直连质量

/// 用户对某个队友手动选的线路
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RoutePref {
    /// 自动：能直连就直连，不稳 / 绕远 / 超出上行预算时走服务器或队友中转
    #[default]
    Auto,
    /// 强制 P2P 直连：打通了就一直直连（不因丢包、绕远、上行预算切走）；打不通时仍走服务器
    Direct,
    /// 走服务器：不和他打洞，也不经队友中转
    Server,
}

impl RoutePref {
    pub fn label(self) -> &'static str {
        match self {
            RoutePref::Auto => "自动",
            RoutePref::Direct => "强制直连",
            RoutePref::Server => "走服务器",
        }
    }
}

/// 直连保活 / 测量间隔
const PEER_PING_INTERVAL: Duration = Duration::from_millis(500);
/// 质量统计窗口
const QUALITY_WINDOW: Duration = Duration::from_secs(10);
/// 发出后这么久还没回就算丢
const PING_TIMEOUT: Duration = Duration::from_millis(1500);
/// 判定不稳后至少走服务器中转这么久
const UNSTABLE_HOLD: Duration = Duration::from_secs(30);
/// 恢复直连前需要连续稳定这么久
const STABLE_BEFORE_RETURN: Duration = Duration::from_secs(10);

/// 一条直连链路最近 10 秒的质量
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeerQuality {
    pub rtt_ms: u32,
    pub loss_pct: u8,
    pub jitter_ms: u32,
    /// 质量差：暂时改走服务器中转（链路仍保持测量，稳定后自动切回）
    pub unstable: bool,
}

/// 和某个成员之间的打洞状态（显示用）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PunchState {
    /// 端到端成对密钥还没就绪（刚进频道，几秒内就会好）
    WaitingKeys,
    /// 正在打洞（已经打了多少秒）
    Punching(u32),
    /// 打了 15 秒以上还没打通（之后每 2 秒继续试）
    Failed(u32),
    /// 已打通
    Connected(PeerQuality),
}

/// 打洞超过这么久没通就算「打不通」（之后仍每 2 秒重试）
pub const PUNCH_FAST_SECS: u32 = 15;

/// 丢包 ≥ 15%、平均延迟 > 350 ms 或抖动 > 80 ms 视为不稳
pub fn quality_is_bad(loss_pct: u8, rtt_ms: u32, jitter_ms: u32) -> bool {
    loss_pct >= 15 || rtt_ms > 350 || jitter_ms > 80
}

#[derive(Clone, Copy)]
struct PingLog {
    seq: u32,
    sent: Instant,
    rtt: Option<u32>,
}

/// 统计窗口内（已过超时时间的）ping：返回 (丢包率%, 平均 RTT, 抖动)。样本太少返回 None。
fn evaluate_pings(pings: &VecDeque<PingLog>, now: Instant) -> Option<(u8, u32, u32)> {
    let done: Vec<&PingLog> = pings
        .iter()
        .filter(|p| now.duration_since(p.sent) >= PING_TIMEOUT && now.duration_since(p.sent) <= QUALITY_WINDOW)
        .collect();
    if done.len() < 6 {
        return None;
    }
    let rtts: Vec<u32> = done.iter().filter_map(|p| p.rtt).collect();
    let loss = ((done.len() - rtts.len()) * 100 / done.len()) as u8;
    let avg = if rtts.is_empty() { 0 } else { rtts.iter().sum::<u32>() / rtts.len() as u32 };
    let jitter = if rtts.len() < 2 {
        0
    } else {
        rtts.windows(2).map(|w| w[0].abs_diff(w[1])).sum::<u32>() / (rtts.len() as u32 - 1)
    };
    Some((loss, avg, jitter))
}

// ------------------------------------------------------------ 直连对端

struct Peer {
    cands: Vec<SocketAddr>,
    sealer: Option<DatagramSealer>,
    opener: Option<DatagramOpener>,
    path: Option<SocketAddr>,
    confirmed: bool,
    last_rx: Instant,
    last_punch: Instant,
    last_ping: Instant,
    /// 本轮打洞开始时间（连通后清空）
    punch_started: Option<Instant>,
    punch_round: u32,
    rtt_ms: u32,
    ping_seq: u32,
    pings: VecDeque<PingLog>,
    unstable_until: Option<Instant>,
    good_since: Option<Instant>,
    quality: PeerQuality,
    /// 对方自测的 NAT 类型
    nat: Option<NatInfo>,
}

impl Peer {
    fn new(cands: Vec<SocketAddr>) -> Self {
        let past = Instant::now() - Duration::from_secs(10);
        Self {
            cands,
            sealer: None,
            opener: None,
            path: None,
            confirmed: false,
            last_rx: past,
            last_punch: past,
            last_ping: past,
            punch_started: None,
            punch_round: 0,
            rtt_ms: 0,
            ping_seq: 0,
            pings: VecDeque::new(),
            unstable_until: None,
            good_since: None,
            quality: PeerQuality::default(),
            nat: None,
        }
    }
}

/// 按两次探测（服务器主端口、探测端口）看到的我的公网地址判断 NAT 类型
pub fn classify_nat(main: SocketAddr, probe: SocketAddr, lan: &[SocketAddr]) -> NatInfo {
    if lan.iter().any(|l| l.ip() == main.ip().to_canonical()) {
        return NatInfo { kind: voc_proto::NAT_OPEN, port: main.port(), step: 0 };
    }
    if main == probe {
        return NatInfo { kind: voc_proto::NAT_CONE, port: main.port(), step: 0 };
    }
    // 换个目标端口公网端口就变：对称型。差得不多说明是按顺序分配的，可以预测
    let step = probe.port() as i32 - main.port() as i32;
    if main.ip() == probe.ip() && step != 0 && step.abs() <= 64 {
        NatInfo { kind: voc_proto::NAT_SYM_PREDICTABLE, port: probe.port(), step: step.signum() as i16 * (step.abs().min(8)) as i16 }
    } else {
        NatInfo { kind: voc_proto::NAT_SYM_RANDOM, port: probe.port(), step: 0 }
    }
}

fn is_symmetric(n: Option<NatInfo>) -> bool {
    n.is_some_and(|n| n.kind == voc_proto::NAT_SYM_PREDICTABLE || n.kind == voc_proto::NAT_SYM_RANDOM)
}

/// 两边都是对称型、且至少一边端口随机：基本打不通，直接走服务器，不白等
pub fn hopeless_pair(me: Option<NatInfo>, peer: Option<NatInfo>) -> bool {
    is_symmetric(me) && is_symmetric(peer) && [me, peer].iter().any(|n| n.is_some_and(|n| n.kind == voc_proto::NAT_SYM_RANDOM))
}

/// 对方是可预测的对称型：按它的步长猜接下来几个端口
pub fn predicted_ports(n: NatInfo) -> Vec<u16> {
    if n.kind != voc_proto::NAT_SYM_PREDICTABLE || n.step == 0 {
        return Vec::new();
    }
    (1..=16i32).map(|i| n.port as i32 + n.step as i32 * i).filter(|p| (1024..=65535).contains(p)).map(|p| p as u16).collect()
}

impl Peer {
    /// 下一个测量 ping 的内容：seq(4) | 发送时间 ms(8)
    fn next_ping(&mut self, now: Instant) -> [u8; 12] {
        self.ping_seq = self.ping_seq.wrapping_add(1);
        self.pings.push_back(PingLog { seq: self.ping_seq, sent: now, rtt: None });
        while self.pings.front().is_some_and(|p| now.duration_since(p.sent) > QUALITY_WINDOW + PING_TIMEOUT) {
            self.pings.pop_front();
        }
        let mut b = [0u8; 12];
        b[..4].copy_from_slice(&self.ping_seq.to_le_bytes());
        b[4..].copy_from_slice(&now_millis().to_le_bytes());
        b
    }

    fn on_pong(&mut self, data: &[u8]) {
        let (seq, ts) = match data.len() {
            12 => (Some(u32::from_le_bytes(data[..4].try_into().unwrap())), u64::from_le_bytes(data[4..12].try_into().unwrap())),
            8 => (None, u64::from_le_bytes(data[..8].try_into().unwrap())),
            _ => return,
        };
        let rtt = now_millis().saturating_sub(ts) as u32;
        self.rtt_ms = if self.rtt_ms == 0 { rtt } else { (self.rtt_ms * 3 + rtt) / 4 };
        if let Some(seq) = seq {
            if let Some(p) = self.pings.iter_mut().find(|p| p.seq == seq) {
                p.rtt = Some(rtt);
            }
        }
    }

    /// 根据最近 10 秒的测量更新质量；不稳 → 至少中转 30 秒，之后连续稳定 10 秒才切回直连
    fn update_quality(&mut self, now: Instant) {
        if let Some((loss, rtt, jitter)) = evaluate_pings(&self.pings, now) {
            self.quality.loss_pct = loss;
            self.quality.rtt_ms = rtt;
            self.quality.jitter_ms = jitter;
            if quality_is_bad(loss, rtt, jitter) {
                let until = now + UNSTABLE_HOLD;
                self.unstable_until = Some(self.unstable_until.map_or(until, |u| u.max(until)));
                self.good_since = None;
            } else {
                let since = *self.good_since.get_or_insert(now);
                if self.unstable_until.is_some_and(|u| now >= u) && now.duration_since(since) >= STABLE_BEFORE_RETURN {
                    self.unstable_until = None;
                }
            }
        }
        self.quality.unstable = self.unstable_until.is_some();
        if self.quality.rtt_ms == 0 {
            self.quality.rtt_ms = self.rtt_ms;
        }
    }

    /// 可以用来直连发送（已打通且质量稳定）
    fn usable(&self) -> bool {
        self.confirmed && self.unstable_until.is_none()
    }

    /// 最近还收得到对方的包（1.2 秒内）
    fn alive(&self, now: Instant) -> bool {
        now.duration_since(self.last_rx) < PEER_SUSPECT
    }
}

#[derive(Default)]
struct Peers {
    map: HashMap<u32, Peer>,
    last_direct: Vec<u32>,
    last_direct_video: Vec<u32>,
}

// ------------------------------------------------------------ 链路

pub struct Link {
    sock: UdpSocket,
    dual: bool,
    server: SocketAddr,
    sealer: Mutex<DatagramSealer>,
    tcp: Mutex<Sender<Outgoing>>,
    seq: AtomicU16,
    closed: AtomicBool,
    last_pong_ms: AtomicU64,
    udp_ok: AtomicBool,
    /// 是否在语音频道中（不在则不发送）
    pub in_channel: AtomicBool,
    my_id: u32,
    e2e: Arc<Mutex<E2e>>,
    p2p_enabled: AtomicBool,
    local_addrs: Mutex<Vec<String>>,
    peers: Mutex<Peers>,
    /// 同频道的其他成员
    others: Mutex<Vec<u32>>,
    /// 正在看我屏幕的人（我在共享时）
    watchers: Mutex<Vec<u32>>,
    watchers_ver: AtomicU32,
    /// 其中要低画质层的人
    watchers_low: Mutex<Vec<u32>>,
    /// 观看者自己选的画面线路：(选了 P2P 直连的, 选了走服务器的)
    watcher_routes: Mutex<(Vec<u32>, Vec<u32>)>,
    /// 会自己画鼠标的观看者
    watchers_cursor: Mutex<Vec<u32>>,
    /// P2P 上行带宽预算（bps）
    budget_bps: AtomicU32,
    /// 正在共享的屏幕码率（bps），没在共享为 0
    video_bps: AtomicU32,
    /// 当前分配结果：(语音直连, 画面直连)
    alloc: Mutex<(Vec<u32>, Vec<u32>)>,
    /// 到服务器的往返延迟（ms），0 = 未知
    server_rtt: AtomicU32,
    /// 允许帮队友中转语音
    relay_enabled: AtomicBool,
    /// 服务器安排的本频道中继路线
    relays: Mutex<Vec<RelayRoute>>,
    /// 屏幕共享画面中继路线（from = 共享者，via 转给 to）
    video_relays: Mutex<Vec<RelayRoute>>,
    /// 上次上报链路质量的时间
    last_stats: Mutex<Instant>,
    /// 屏幕画面的匀速发送队列（语音不走这里，直接发）
    pace_tx: Mutex<Option<Sender<(Instant, SocketAddr, Vec<u8>)>>>,
    /// 伴奏 / 共享声音的外层序号（只给服务器去重用；端到端的计数器在密文里）
    music_seq: AtomicU16,
    share_seq: AtomicU16,
    /// 画面包在匀速队列里过期被丢掉的次数（网络 / 上行太挤的信号，码率自适应用）
    pace_dropped: AtomicU32,
    /// 路由器端口映射的结果（网络自检显示）
    portmap: Mutex<crate::portmap::PortMapStatus>,
    /// 本机局域网地址（不含端口映射得到的公网地址）、本地端口、建立时间（网络自检用）
    lan_addrs: Vec<SocketAddr>,
    local_port: u16,
    created: Instant,
    /// 服务器看到的我的公网地址（新版服务器在 PONG 里告诉我）
    public_addr: Mutex<Option<SocketAddr>>,
    /// 服务器的 NAT 探测端口，以及从那里看到的我的公网地址
    probe: Option<SocketAddr>,
    probe_addr: Mutex<Option<SocketAddr>>,
    /// 自测的 NAT 类型（测出来后告诉服务器，打洞时对方据此选策略）
    pub nat: Mutex<Option<NatInfo>>,
    /// 最近 30 次服务器 PING：(发送时间, 收到回复)，算丢包率
    srv_pings: Mutex<std::collections::VecDeque<(u64, bool)>>,
    /// 发出的字节数（UDP 含 IP/UDP 头，TCP 隧道只算语音和画面），诊断「上行是不是被占满」用
    sent_bytes: AtomicU64,
    /// 最近 30 秒每秒的上行码率（kbps）
    up_history: Mutex<std::collections::VecDeque<u32>>,
    /// 用户手动选的线路（没有的按自动）
    route_prefs: Mutex<HashMap<u32, RoutePref>>,
    /// 服务器最近一次发来的直连候选（改了线路后重新套用）
    last_peer_list: Mutex<Vec<PeerInfo>>,
    /// 最近约 1 秒发出的画面分片（观看者请求补发时从这里取）：(时间, 画质层, 帧号, 分片)
    video_hist: Mutex<std::collections::VecDeque<(Instant, u8, u32, Vec<Vec<u8>>)>>,
    /// 发出的画面分片数、被请求补发的分片数（估计丢包率，决定冗余校验包的比例）
    video_frags_sent: AtomicU32,
    video_frags_nacked: AtomicU32,
    /// 观看者回报的收画面情况：观看者 → (原始丢包率 千分之几, 往返 ms, 时间)
    video_reports: Mutex<HashMap<u32, (u16, u32, Instant)>>,
    /// 画面直连 + 经服务器两条路同时发的观看者（丢包很高时）
    dual_video: Mutex<std::collections::HashSet<u32>>,
    /// 语音直连 + 经服务器两条路同时发的人（直连有丢包但还没到「不稳」时）
    dual_voice: Mutex<std::collections::HashSet<u32>>,
    /// 观看者确认的长期参考帧 (观看者, 帧号)、有没有人请求恢复帧
    ltr_acks: Mutex<Vec<(u32, u32)>>,
    ltr_recover: AtomicBool,
}

/// 直连语音丢包 ≥ 这个百分比：直连和服务器两条路同时发
const DUAL_VOICE_LOSS: u8 = 5;

/// 补发用的画面历史保留多久
const VIDEO_HIST: Duration = Duration::from_millis(1000);

impl Link {
    fn send_raw(&self, buf: &[u8], to: SocketAddr) {
        if self.sock.send_to(buf, wire(to, self.dual)).is_ok() {
            self.sent_bytes.fetch_add(buf.len() as u64 + 28, Ordering::Relaxed);
        }
    }

    fn send_server(&self, kind: u8, data: &[u8], buf: &mut Vec<u8>) {
        self.sealer.lock().unwrap().seal(kind, data, buf);
        self.send_raw(buf, self.server);
    }

    fn tcp_send(&self, o: Outgoing) {
        if let Outgoing::Voice(v) | Outgoing::Video(v) = &o {
            self.sent_bytes.fetch_add(v.len() as u64 + 60, Ordering::Relaxed);
        }
        let _ = self.tcp.lock().unwrap().send(o);
    }

    /// 上行码率（kbps）：(最近 1 秒, 最近 30 秒平均, 最近 30 秒峰值)
    pub fn upload_kbps(&self) -> (u32, u32, u32) {
        let h = self.up_history.lock().unwrap();
        let now = h.back().copied().unwrap_or(0);
        let avg = if h.is_empty() { 0 } else { (h.iter().map(|&x| x as u64).sum::<u64>() / h.len() as u64) as u32 };
        (now, avg, h.iter().copied().max().unwrap_or(0))
    }

    fn send_voice(&self, frame: &[u8], buf: &mut Vec<u8>) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let others = self.others.lock().unwrap().clone();
        let mut sent: Vec<u32> = Vec::new();
        {
            let mut peers = self.peers.lock().unwrap();
            if self.p2p_active() {
                let payload = udp::server_voice(self.my_id, seq, frame);
                let chosen = self.alloc.lock().unwrap().0.clone();
                for id in others.iter().filter(|i| chosen.contains(i)) {
                    let Some(p) = peers.map.get_mut(id) else { continue };
                    if let (true, Some(path), Some(s)) = (p.confirmed, p.path, p.sealer.as_mut()) {
                        s.seal(udp::VOICE, &payload, buf);
                        self.send_raw(buf, path);
                        sent.push(*id);
                    }
                }
            }
        }
        // 经成员中继能送到的人：中继是我直连发到的人
        // （手动选了走服务器的人不算：服务器可能还没收到新选择，照样发一份给服务器）
        let prefs = self.route_prefs.lock().unwrap().clone();
        let relayed: Vec<u32> = self
            .relays
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.from == self.my_id && sent.contains(&r.via) && prefs.get(&r.to) != Some(&RoutePref::Server))
            .map(|r| r.to)
            .collect();
        let dual = self.dual_voice.lock().unwrap().clone();
        if others.iter().all(|o| (sent.contains(o) && !dual.contains(o)) || relayed.contains(o)) {
            return; // 频道里只有自己，或全部直连 / 经成员中继：服务器不经手
        }
        let payload = udp::client_voice(seq, frame);
        if self.udp_ok.load(Ordering::Relaxed) {
            self.send_server(udp::VOICE, &payload, buf);
            // UDP 好像断了但还没确认：TCP 同时发一份（对方按序号去重），切换期间不断音
            let last = self.last_pong_ms.load(Ordering::Relaxed);
            if now_millis().saturating_sub(last) > UDP_SUSPECT_MS {
                self.tcp_send(Outgoing::Voice(payload));
            }
        } else {
            self.tcp_send(Outgoing::Voice(payload));
        }
    }

    fn p2p_active(&self) -> bool {
        self.p2p_enabled.load(Ordering::Relaxed)
    }

    pub fn set_budget(&self, bps: u32) {
        self.budget_bps.store(bps.max(500_000), Ordering::Relaxed);
    }

    /// 开始 / 停止共享时由共享者设置画面码率
    pub fn set_video_bps(&self, bps: u32) {
        self.video_bps.store(bps, Ordering::Relaxed);
    }

    /// 当前分配：(语音直连名单, 画面直连名单)
    pub fn allocation(&self) -> (Vec<u32>, Vec<u32>) {
        self.alloc.lock().unwrap().clone()
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    /// 是否允许帮队友中转语音（占用自己的上行带宽）
    pub fn set_relay_enabled(&self, on: bool) {
        self.relay_enabled.store(on, Ordering::Relaxed);
    }

    /// 服务器下发的本频道中继路线
    pub fn set_relays(&self, routes: Vec<RelayRoute>) {
        *self.relays.lock().unwrap() = routes;
    }

    pub fn set_video_relays(&self, routes: Vec<RelayRoute>) {
        *self.video_relays.lock().unwrap() = routes;
    }

    /// 正在帮人转发画面的路数（界面 / 诊断）、经别人转发给我的共享者
    pub fn video_relay_info(&self) -> (Vec<RelayRoute>, Vec<RelayRoute>) {
        let r = self.video_relays.lock().unwrap();
        (r.iter().filter(|x| x.via == self.my_id).copied().collect(), r.iter().filter(|x| x.to == self.my_id).copied().collect())
    }

    /// 帮人转发画面：直连收到共享者的画面包，原样（密文）转给安排给我的观看者
    fn relay_video(&self, peer: u32, payload: &[u8], buf: &mut Vec<u8>) {
        let Some(from) = payload.get(..4).map(|b| u32::from_le_bytes(b.try_into().unwrap())) else { return };
        if from != peer {
            return; // 只转共享者自己直连发来的，不转转来的
        }
        let targets: Vec<u32> = self.video_relays.lock().unwrap().iter().filter(|r| r.via == self.my_id && r.from == from).map(|r| r.to).collect();
        if targets.is_empty() || !self.relay_enabled.load(Ordering::Relaxed) {
            return;
        }
        let mut peers = self.peers.lock().unwrap();
        for to in targets {
            let Some(p) = peers.map.get_mut(&to) else { continue };
            if let (true, Some(path), Some(s)) = (p.confirmed, p.path, p.sealer.as_mut()) {
                s.seal(udp::VIDEO, payload, buf);
                self.send_raw(buf, path);
            }
        }
    }

    pub fn relays(&self) -> Vec<RelayRoute> {
        self.relays.lock().unwrap().clone()
    }

    pub fn my_id(&self) -> u32 {
        self.my_id
    }

    /// 作为中继：把 peer 直连发来的、他自己的语音包原样转给路线上的听众（包已端到端加密）
    fn relay_forward(&self, peer: u32, payload: &[u8], buf: &mut Vec<u8>) {
        let Some((from, _, _)) = udp::read_server_voice(payload) else { return };
        if from != peer {
            return; // 只转发对方自己的语音，不转发转来的包
        }
        let targets: Vec<u32> =
            self.relays.lock().unwrap().iter().filter(|r| r.via == self.my_id && r.from == from).map(|r| r.to).collect();
        if targets.is_empty() || !self.relay_enabled.load(Ordering::Relaxed) {
            return;
        }
        let mut peers = self.peers.lock().unwrap();
        for to in targets {
            let Some(p) = peers.map.get_mut(&to) else { continue };
            if let (true, Some(path), Some(s)) = (p.confirmed, p.path, p.sealer.as_mut()) {
                s.seal(udp::VOICE, payload, buf);
                self.send_raw(buf, path);
            }
        }
    }

    pub fn set_others(&self, ids: Vec<u32>) {
        *self.others.lock().unwrap() = ids;
    }

    pub fn set_p2p(&self, on: bool) {
        if self.p2p_enabled.swap(on, Ordering::Relaxed) == on {
            return;
        }
        if on {
            let local_addrs = self.local_addrs.lock().unwrap().clone();
            let nat = *self.nat.lock().unwrap();
            self.tcp_send(Outgoing::Msg(ClientMsg::P2pEnable { local_addrs, nat }));
            self.send_p2p_hide();
        } else {
            *self.peers.lock().unwrap() = Peers::default();
            self.tcp_send(Outgoing::Msg(ClientMsg::P2pDisable));
        }
    }

    /// 服务器告知：现在谁在看我的屏幕
    pub fn set_watchers(&self, ids: Vec<u32>, low: Vec<u32>) {
        let mut w = self.watchers.lock().unwrap();
        let mut wl = self.watchers_low.lock().unwrap();
        // 有新观看者，或有人换了画质层：编码器会补一个关键帧
        let fresh = ids.iter().any(|i| !w.contains(i) || low.contains(i) != wl.contains(i));
        if fresh {
            self.watchers_ver.fetch_add(1, Ordering::Relaxed);
        }
        *w = ids;
        *wl = low;
    }

    /// 服务器转告：哪些观看者会自己画鼠标
    pub fn set_watchers_cursor(&self, ids: Vec<u32>) {
        *self.watchers_cursor.lock().unwrap() = ids;
    }

    /// 观看者都会自己画鼠标（有人看时）
    pub fn all_watchers_draw_cursor(&self) -> bool {
        let w = self.watchers.lock().unwrap().clone();
        let c = self.watchers_cursor.lock().unwrap();
        !w.is_empty() && w.iter().all(|i| c.contains(i))
    }

    /// 服务器转告：观看者自己选的画面线路
    pub fn set_watcher_routes(&self, direct: Vec<u32>, server: Vec<u32>) {
        *self.watcher_routes.lock().unwrap() = (direct, server);
    }

    /// 某一层的观看者：low = 低画质层，否则原画
    pub fn layer_watchers(&self, low: bool) -> Vec<u32> {
        // 和 set_watchers 同样的加锁顺序（先 watchers 后 watchers_low），避免互相等死
        let w = self.watchers.lock().unwrap();
        let wl = self.watchers_low.lock().unwrap();
        w.iter().copied().filter(|i| wl.contains(i) == low).collect()
    }

    pub fn watchers_version(&self) -> u32 {
        self.watchers_ver.load(Ordering::Relaxed)
    }

    pub fn has_watchers(&self) -> bool {
        !self.watchers.lock().unwrap().is_empty()
    }

    pub fn watcher_count(&self) -> usize {
        self.watchers.lock().unwrap().len()
    }

    /// 发送一帧视频的全部分片（low = 低画质层）：这一层能 P2P 直连的观看者直接发，其余交给服务器按层中转。
    /// 数据分片记下来，观看者请求补发时重发
    pub fn send_video(&self, frags: &[Vec<u8>], buf: &mut Vec<u8>, low: bool) {
        let watchers = self.layer_watchers(low);
        if watchers.is_empty() {
            return;
        }
        if let Some((h, _)) = frags.first().and_then(|f| udp::read_video_frag(f)) {
            let data: Vec<Vec<u8>> = frags.iter().filter(|f| udp::read_video_frag(f).is_some_and(|(x, _)| !x.is_parity())).cloned().collect();
            self.video_frags_sent.fetch_add(data.len() as u32, Ordering::Relaxed);
            let mut hist = self.video_hist.lock().unwrap();
            let now = Instant::now();
            while hist.front().is_some_and(|e| now.duration_since(e.0) > VIDEO_HIST) {
                hist.pop_front();
            }
            hist.push_back((now, h.flags & udp::VIDEO_LOW, h.frame, data));
        }
        let mut direct = 0;
        if self.p2p_active() {
            let chosen = self.alloc.lock().unwrap().1.clone();
            let mut peers = self.peers.lock().unwrap();
            for id in watchers.iter().filter(|i| chosen.contains(i)) {
                let Some(p) = peers.map.get_mut(id) else { continue };
                if let (true, Some(path), Some(s)) = (p.confirmed, p.path, p.sealer.as_mut()) {
                    for f in frags {
                        s.seal(udp::VIDEO, &udp::server_video(self.my_id, f), buf);
                        self.send_paced(path, buf);
                    }
                    direct += 1;
                }
            }
        }
        let dual = self.dual_video.lock().unwrap().iter().filter(|i| watchers.contains(i)).count();
        // 经观看者转发能送到的人（转发的人得是我这次直连发到的）
        let relayed = {
            let chosen = self.alloc.lock().unwrap().1.clone();
            self.video_relays
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.from == self.my_id && watchers.contains(&r.to) && chosen.contains(&r.via) && !chosen.contains(&r.to))
                .count()
        };
        if direct + relayed >= watchers.len() && dual == 0 {
            return;
        }
        let udp_ok = self.udp_ok.load(Ordering::Relaxed);
        for f in frags {
            if udp_ok {
                self.sealer.lock().unwrap().seal(udp::VIDEO, f, buf);
                self.send_paced(self.server, buf);
            } else {
                self.tcp_send(Outgoing::Video(f.clone()));
            }
        }
    }

    /// 发送一帧伴奏（给同频道所有人）/ 屏幕共享声音（给观看者）：能直连的直接发，其余交给服务器
    pub fn send_stream(&self, kind: u8, blob: &[u8], buf: &mut Vec<u8>) {
        let music = kind == udp::MUSIC;
        let seq = if music { &self.music_seq } else { &self.share_seq }.fetch_add(1, Ordering::Relaxed);
        let others = self.others.lock().unwrap().clone();
        let targets: Vec<u32> = if music {
            others
        } else {
            self.watchers.lock().unwrap().iter().copied().filter(|w| others.contains(w)).collect()
        };
        if targets.is_empty() {
            return;
        }
        let mut sent = 0;
        if self.p2p_active() {
            let chosen = if music { self.alloc.lock().unwrap().0.clone() } else { self.alloc.lock().unwrap().1.clone() };
            let payload = udp::server_voice(self.my_id, seq, blob);
            let mut peers = self.peers.lock().unwrap();
            for id in targets.iter().filter(|i| chosen.contains(i)) {
                let Some(p) = peers.map.get_mut(id) else { continue };
                if let (true, Some(path), Some(s)) = (p.confirmed, p.path, p.sealer.as_mut()) {
                    s.seal(kind, &payload, buf);
                    self.send_raw(buf, path);
                    sent += 1;
                }
            }
        }
        if sent < targets.len() && self.udp_ok.load(Ordering::Relaxed) {
            self.send_server(kind, &udp::client_voice(seq, blob), buf);
        }
    }

    /// 路由器端口映射的结果
    pub fn portmap_status(&self) -> crate::portmap::PortMapStatus {
        self.portmap.lock().unwrap().clone()
    }

    /// 网络自检要用的链路快照
    pub fn netcheck_snapshot(&self) -> crate::netcheck::Snapshot {
        let now = now_millis();
        let loss = {
            let p = self.srv_pings.lock().unwrap();
            // 发出 1.5 秒还没回的才算丢
            let done: Vec<bool> = p.iter().filter(|(t, _)| now.saturating_sub(*t) > 1500).map(|(_, g)| *g).collect();
            (done.len() >= 5).then(|| (done.iter().filter(|g| !**g).count() * 100 / done.len()) as u8)
        };
        let rtt = self.server_rtt.load(Ordering::Relaxed);
        let states = self.punch_states();
        let count = |f: &dyn Fn(&PunchState) -> bool| states.values().filter(|s| f(s)).count();
        crate::netcheck::Snapshot {
            age_secs: self.created.elapsed().as_secs(),
            udp_ok: self.udp_ok.load(Ordering::Relaxed),
            ever_pong: self.last_pong_ms.load(Ordering::Relaxed) != 0,
            server_rtt: (rtt > 0).then_some(rtt),
            server_loss: loss,
            public: *self.public_addr.lock().unwrap(),
            lan: self.lan_addrs.clone(),
            local_port: self.local_port,
            portmap: self.portmap_status(),
            p2p: self.p2p_on(),
            punched: count(&|s| matches!(s, PunchState::Connected(_))),
            punching: count(&|s| matches!(s, PunchState::Punching(_))),
            failed: count(&|s| matches!(s, PunchState::Failed(_))),
            nat: *self.nat.lock().unwrap(),
            probe: match (self.probe, *self.probe_addr.lock().unwrap()) {
                (None, _) => crate::netcheck::ProbeState::NotOffered,
                (Some(p), Some(_)) => crate::netcheck::ProbeState::Ok(p.port()),
                (Some(p), None) if self.created.elapsed() > Duration::from_secs(10) => crate::netcheck::ProbeState::NoReply(p.port()),
                (Some(_), None) => crate::netcheck::ProbeState::Waiting,
            },
        }
    }

    /// 取走匀速队列的丢包计数
    pub fn take_pace_drops(&self) -> u32 {
        self.pace_dropped.swap(0, Ordering::Relaxed)
    }

    /// 画面包交给匀速发送线程（队列没起来时直接发）
    fn send_paced(&self, to: SocketAddr, pkt: &[u8]) {
        let tx = self.pace_tx.lock().unwrap().clone();
        match tx {
            Some(tx) => {
                let _ = tx.send((Instant::now(), to, pkt.to_vec()));
            }
            None => self.send_raw(pkt, to),
        }
    }

    /// 观看者的长期参考帧确认 / 恢复请求
    pub fn on_video_ltr(&self, by: u32, ack: Option<u32>, recover: bool) {
        if let Some(f) = ack {
            let mut a = self.ltr_acks.lock().unwrap();
            if a.len() < 256 {
                a.push((by, f));
            }
        }
        if recover {
            self.ltr_recover.store(true, Ordering::Relaxed);
        }
    }

    /// 取走攒下的长期参考帧确认
    pub fn take_ltr_acks(&self) -> Vec<(u32, u32)> {
        std::mem::take(&mut *self.ltr_acks.lock().unwrap())
    }

    /// 有人请求恢复帧（取走标记）
    pub fn take_ltr_recover(&self) -> bool {
        self.ltr_recover.swap(false, Ordering::Relaxed)
    }

    /// 恢复帧这次没发成（限速），留到下一帧
    pub fn keep_ltr_recover(&self) {
        self.ltr_recover.store(true, Ordering::Relaxed);
    }

    /// 观看者回报收画面的情况
    pub fn on_video_report(&self, by: u32, loss: u16, rtt: u32) {
        self.video_reports.lock().unwrap().insert(by, (loss.min(1000), rtt, Instant::now()));
    }

    /// 某一层观看者最近 5 秒回报的最大原始丢包率（千分之几）
    pub fn layer_loss(&self, low: bool) -> u16 {
        let ids = self.layer_watchers(low);
        let r = self.video_reports.lock().unwrap();
        ids.iter().filter_map(|i| r.get(i)).filter(|x| x.2.elapsed() < Duration::from_secs(5)).map(|x| x.0).max().unwrap_or(0)
    }

    /// 重新挑选两条路同时发画面的观看者：直连发画面、最近 5 秒回报丢包 ≥ loss 的人。返回人数
    pub fn update_dual_video(&self, loss: u16) -> usize {
        let direct = self.alloc.lock().unwrap().1.clone();
        let set: std::collections::HashSet<u32> = {
            let r = self.video_reports.lock().unwrap();
            direct.into_iter().filter(|i| r.get(i).is_some_and(|x| x.0 >= loss && x.2.elapsed() < Duration::from_secs(5))).collect()
        };
        let n = set.len();
        *self.dual_video.lock().unwrap() = set;
        n
    }

    /// 我发出去的语音大概的丢包率（%）：到服务器的丢包和直连的人里最高的那个
    pub fn voice_path_loss(&self) -> u8 {
        let now = now_millis();
        let srv = {
            let p = self.srv_pings.lock().unwrap();
            let done: Vec<bool> = p.iter().filter(|(t, _)| now.saturating_sub(*t) > 1500).map(|(_, g)| *g).collect();
            if done.len() >= 5 {
                (done.iter().filter(|g| !**g).count() * 100 / done.len()) as u8
            } else {
                0
            }
        };
        let direct = self.alloc.lock().unwrap().0.clone();
        let peers = self.peers.lock().unwrap();
        let p2p = direct.iter().filter_map(|i| peers.map.get(i)).map(|p| p.quality.loss_pct).max().unwrap_or(0);
        srv.max(p2p)
    }

    /// 和某人直连的往返时间（ms），没直连返回 None
    pub fn peer_rtt(&self, id: u32) -> Option<u32> {
        self.peers.lock().unwrap().map.get(&id).filter(|p| p.confirmed && p.rtt_ms > 0).map(|p| p.rtt_ms)
    }

    /// 观看者请求补发：peer = 直连收到的（直接重发给他），None = 服务器转来的（重发给服务器，服务器照常转发）
    pub fn resend_video(&self, peer: Option<u32>, nack: &[u8]) {
        let Some((_, layer, frame, idxs)) = udp::read_nack(nack) else { return };
        let frags: Vec<Vec<u8>> = {
            let hist = self.video_hist.lock().unwrap();
            let Some(e) = hist.iter().rev().find(|e| e.1 == layer && e.2 == frame) else { return };
            if idxs.contains(&udp::NACK_WHOLE) {
                e.3.clone()
            } else {
                idxs.iter().filter_map(|i| e.3.get(*i as usize).cloned()).collect()
            }
        };
        self.video_frags_nacked.fetch_add(frags.len() as u32, Ordering::Relaxed);
        let mut buf = Vec::new();
        match peer {
            Some(id) => {
                let mut peers = self.peers.lock().unwrap();
                let Some(p) = peers.map.get_mut(&id) else { return };
                if let (true, Some(path), Some(s)) = (p.confirmed, p.path, p.sealer.as_mut()) {
                    for f in &frags {
                        s.seal(udp::VIDEO, &udp::server_video(self.my_id, f), &mut buf);
                        self.send_paced(path, &buf);
                    }
                }
            }
            None => {
                for f in &frags {
                    self.sealer.lock().unwrap().seal(udp::VIDEO, f, &mut buf);
                    self.send_paced(self.server, &buf);
                }
            }
        }
    }

    /// 取走 (发出的画面分片数, 被请求补发的分片数)
    pub fn take_video_loss(&self) -> (u32, u32) {
        (self.video_frags_sent.swap(0, Ordering::Relaxed), self.video_frags_nacked.swap(0, Ordering::Relaxed))
    }

    /// 向共享者请求补发：画面是直连收到的就直连发给他，否则发给服务器（服务器要支持）
    fn send_nack(&self, to: u32, direct: bool, server_rtx: bool, payload: &[u8]) {
        let mut buf = Vec::new();
        let mut sent = false;
        if direct {
            let mut peers = self.peers.lock().unwrap();
            if let Some(p) = peers.map.get_mut(&to) {
                if let (true, Some(path), Some(s)) = (p.confirmed, p.path, p.sealer.as_mut()) {
                    s.seal(udp::NACK, payload, &mut buf);
                    self.send_raw(&buf, path);
                    sent = true;
                }
            }
        }
        // 没直连共享者（画面是别的观看者转来的）：发给服务器
        if !sent && server_rtx && self.udp_ok.load(Ordering::Relaxed) {
            self.send_server(udp::NACK, payload, &mut buf);
        }
    }

    pub fn request_keyframe(&self, from: u32) {
        self.tcp_send(Outgoing::Msg(ClientMsg::KeyframeRequest { target: from }));
    }

    /// 新增本机候选地址（例如 UPnP 映射出的公网地址），并通知服务器
    fn add_local_candidate(&self, addr: String) {
        let list = {
            let mut l = self.local_addrs.lock().unwrap();
            if l.contains(&addr) {
                return;
            }
            l.insert(0, addr);
            l.clone()
        };
        if self.p2p_enabled.load(Ordering::Relaxed) {
            let nat = *self.nat.lock().unwrap();
            self.tcp_send(Outgoing::Msg(ClientMsg::P2pEnable { local_addrs: list, nat }));
        }
    }

    /// 两次探测都有结果了：判断 NAT 类型，变了就告诉服务器
    fn update_nat(&self) {
        let (Some(main), Some(probe)) = (*self.public_addr.lock().unwrap(), *self.probe_addr.lock().unwrap()) else { return };
        let n = classify_nat(main, probe, &self.lan_addrs);
        let changed = {
            let mut cur = self.nat.lock().unwrap();
            let changed = *cur != Some(n);
            *cur = Some(n);
            changed
        };
        if changed && self.p2p_enabled.load(Ordering::Relaxed) {
            let local_addrs = self.local_addrs.lock().unwrap().clone();
            self.tcp_send(Outgoing::Msg(ClientMsg::P2pEnable { local_addrs, nat: Some(n) }));
        }
    }

    /// 和这个人两边都是对称型 NAT，不打洞
    fn hopeless(&self, p: &Peer) -> bool {
        hopeless_pair(*self.nat.lock().unwrap(), p.nat)
    }

    /// 本机是否开启了 P2P
    pub fn p2p_on(&self) -> bool {
        self.p2p_active()
    }

    /// 每个可直连成员（对方也开了 P2P、UDP 可用）的打洞状态
    pub fn punch_states(&self) -> HashMap<u32, PunchState> {
        let peers = self.peers.lock().unwrap();
        peers
            .map
            .iter()
            .map(|(id, p)| {
                let st = if p.sealer.is_none() {
                    PunchState::WaitingKeys
                } else if p.confirmed {
                    PunchState::Connected(p.quality)
                } else {
                    let secs = p.punch_started.map_or(0, |t| t.elapsed().as_secs() as u32);
                    if secs < PUNCH_FAST_SECS && !self.hopeless(p) {
                        PunchState::Punching(secs)
                    } else {
                        PunchState::Failed(secs)
                    }
                };
                (*id, st)
            })
            .collect()
    }

    /// 已打通直连的成员及链路质量
    pub fn direct_peers(&self) -> HashMap<u32, PeerQuality> {
        let peers = self.peers.lock().unwrap();
        peers.map.iter().filter(|(_, p)| p.confirmed).map(|(id, p)| (*id, p.quality)).collect()
    }

    /// 服务器推送的同频道 P2P 成员地址（密钥来自端到端加密层的成对密钥）
    /// 设置手动线路（id → 选择；不在表里的按自动）
    pub fn set_route_prefs(&self, prefs: HashMap<u32, RoutePref>) {
        {
            let mut cur = self.route_prefs.lock().unwrap();
            if *cur == prefs {
                return;
            }
            *cur = prefs;
        }
        self.send_p2p_hide();
        // 「走服务器」的人从直连候选里去掉，改回来的重新打洞
        let list = self.last_peer_list.lock().unwrap().clone();
        self.on_peers(list);
    }

    /// 告诉服务器：选了走服务器的人，双方地址互不透露
    fn send_p2p_hide(&self) {
        if !self.p2p_enabled.load(Ordering::Relaxed) {
            return;
        }
        let mut ids: Vec<u32> = self.route_prefs.lock().unwrap().iter().filter(|(_, r)| **r == RoutePref::Server).map(|(id, _)| *id).collect();
        ids.sort();
        self.tcp_send(Outgoing::Msg(ClientMsg::P2pHide { ids }));
    }

    pub fn route_pref(&self, id: u32) -> RoutePref {
        self.route_prefs.lock().unwrap().get(&id).copied().unwrap_or_default()
    }

    pub fn on_peers(&self, list: Vec<PeerInfo>) {
        *self.last_peer_list.lock().unwrap() = list.clone();
        if !self.p2p_enabled.load(Ordering::Relaxed) {
            return;
        }
        // 手动选了走服务器的人不打洞
        let prefs = self.route_prefs.lock().unwrap().clone();
        let list: Vec<PeerInfo> = list.into_iter().filter(|p| prefs.get(&p.id) != Some(&RoutePref::Server)).collect();
        let mut peers = self.peers.lock().unwrap();
        peers.map.retain(|id, _| list.iter().any(|p| p.id == *id));
        for info in list {
            let cands: Vec<SocketAddr> = info.addrs.iter().filter_map(|a| a.parse().ok()).map(norm).collect();
            let p = peers.map.entry(info.id).or_insert_with(|| Peer::new(Vec::new()));
            p.nat = info.nat;
            if p.cands != cands {
                p.cands = cands;
                p.punch_started = None; // 有新地址：重新快速打洞
            }
        }
    }

    /// 每 100 ms 调一次：打洞、保活、判断断开、同步直连名单
    fn tick(&self, buf: &mut Vec<u8>) {
        let others = self.others.lock().unwrap().clone();
        let my_nat = *self.nat.lock().unwrap();
        let active = self.p2p_active();
        let mut peers = self.peers.lock().unwrap();
        let now = Instant::now();
        {
            // 端到端成对密钥就绪后装上（会话重建时也会换新）
            let e2e = self.e2e.lock().unwrap();
            for (id, p) in peers.map.iter_mut() {
                match e2e.pair_keys(*id) {
                    Some(k) if p.sealer.is_none() => {
                        p.sealer = Some(DatagramSealer::new(&k.send, &k.send_hp));
                        p.opener = Some(DatagramOpener::new(&k.recv, &k.recv_hp));
                    }
                    None if p.sealer.is_some() => {
                        p.sealer = None;
                        p.opener = None;
                        p.confirmed = false;
                        p.path = None;
                    }
                    _ => {}
                }
            }
        }
        for p in peers.map.values_mut() {
            let Some(s) = p.sealer.as_mut() else { continue };
            if p.confirmed && now.duration_since(p.last_rx) > PEER_DEAD {
                p.confirmed = false;
                p.path = None;
                // 直连断过：重新打通后也先走中转观察一段时间
                p.unstable_until = Some(now + UNSTABLE_HOLD);
                p.good_since = None;
                p.pings.clear();
            }
            if !p.confirmed {
                // 头 15 秒每 200 ms 打一次洞，之后每 2 秒试一次（对方换了网络等情况）
                let started = *p.punch_started.get_or_insert(now);
                let hopeless = hopeless_pair(my_nat, p.nat);
                let interval = if !hopeless && now.duration_since(started) < Duration::from_secs(PUNCH_FAST_SECS as u64) {
                    Duration::from_millis(200)
                } else {
                    Duration::from_secs(2)
                };
                if now.duration_since(p.last_punch) >= interval {
                    p.last_punch = now;
                    let mut targets = p.cands.clone();
                    if let Some(path) = p.path {
                        targets.push(path);
                    }
                    for t in targets {
                        s.seal(p2p::PUNCH, &now_millis().to_le_bytes(), buf);
                        self.send_raw(buf, t);
                    }
                    // 端口预测：很多对称型 NAT 按顺序分配端口，2 秒还没打通就顺带试对方公网端口附近的几个（每秒一轮）
                    let elapsed = now.duration_since(started);
                    let spray = elapsed > Duration::from_secs(2) && elapsed < Duration::from_secs(30) && p.punch_round % 5 == 0;
                    p.punch_round = p.punch_round.wrapping_add(1);
                    // 对方是可预测的对称型 NAT：按它的步长朝接下来的端口打（每轮都打）
                    if let (Some(n), Some(public)) = (p.nat, p.cands.first().copied()) {
                        for port in predicted_ports(n) {
                            s.seal(p2p::PUNCH, &now_millis().to_le_bytes(), buf);
                            self.send_raw(buf, SocketAddr::new(public.ip(), port));
                        }
                    }
                    if let (true, Some(public)) = (spray, p.cands.first().copied()) {
                        for d in 1..=PORT_SPRAY {
                            for port in [public.port().wrapping_add(d), public.port().wrapping_sub(d)] {
                                if port >= 1024 {
                                    s.seal(p2p::PUNCH, &now_millis().to_le_bytes(), buf);
                                    self.send_raw(buf, SocketAddr::new(public.ip(), port));
                                }
                            }
                        }
                    }
                }
            } else if now.duration_since(p.last_ping) >= PEER_PING_INTERVAL {
                p.last_ping = now;
                if let Some(path) = p.path {
                    let ping = p.next_ping(now);
                    let s = p.sealer.as_mut().unwrap();
                    s.seal(p2p::PING, &ping, buf);
                    self.send_raw(buf, path);
                }
            }
            if p.confirmed {
                p.update_quality(now);
            }
        }
        // 手动「强制直连」的人：打通了就直连，不看稳定性、绕远和预算
        let prefs = self.route_prefs.lock().unwrap().clone();
        let forced: Vec<u32> = others
            .iter()
            .copied()
            .filter(|id| prefs.get(id) == Some(&RoutePref::Direct) && peers.map.get(id).is_some_and(|p| p.confirmed && p.alive(now)))
            .collect();
        // 其余的人按上行预算分配：已打通的候选按延迟从低到高
        let srv = self.server_rtt.load(Ordering::Relaxed);
        let by_rtt = |ids: &[u32]| {
            let mut v: Vec<(u32, u32)> = ids
                .iter()
                .filter_map(|id| {
                    peers
                        .map
                        .get(id)
                        .filter(|p| p.usable() && p.alive(now))
                        // 直连明显比经服务器绕远：不用（对方到服务器的延迟按和我差不多估）
                        .filter(|p| srv == 0 || p.rtt_ms <= 2 * srv + DIRECT_DETOUR_MS)
                        .map(|p| (p.rtt_ms, *id))
                })
                .collect();
            v.sort();
            v.into_iter().map(|(_, id)| id).collect::<Vec<u32>>()
        };
        let watchers: Vec<u32> = self.watchers.lock().unwrap().iter().copied().filter(|w| others.contains(w)).collect();
        let auto_others: Vec<u32> = others.iter().copied().filter(|i| !forced.contains(i)).collect();
        // 画面：观看者自己选的线路优先，其次是我对这个人手动选的线路
        let (vd, vs) = self.watcher_routes.lock().unwrap().clone();
        let forced_video: Vec<u32> = watchers
            .iter()
            .copied()
            .filter(|i| !vs.contains(i))
            .filter(|i| forced.contains(i) || (vd.contains(i) && peers.map.get(i).is_some_and(|p| p.confirmed && p.alive(now))))
            .collect();
        let auto_watchers: Vec<u32> = watchers.iter().copied().filter(|i| !forced_video.contains(i) && !vs.contains(i)).collect();
        let (mut voice, mut video) = if active {
            let (mut v, mut d) = allocate_direct(
                self.budget_bps.load(Ordering::Relaxed),
                &by_rtt(&auto_others),
                auto_others.len(),
                self.video_bps.load(Ordering::Relaxed),
                &by_rtt(&auto_watchers),
                auto_watchers.len(),
            );
            v.extend(forced.iter().copied());
            d.extend(forced_video.iter().copied());
            (v, d)
        } else {
            (Vec::new(), Vec::new())
        };
        *self.alloc.lock().unwrap() = (voice.clone(), video.clone());
        // 每 2 秒上报链路质量（服务器据此安排成员中继）
        let udp_rtt = self.server_rtt.load(Ordering::Relaxed);
        let due = now.duration_since(*self.last_stats.lock().unwrap()) >= STATS_INTERVAL;
        if active && due && udp_rtt > 0 && self.in_channel.load(Ordering::Relaxed) && self.udp_ok.load(Ordering::Relaxed) {
            *self.last_stats.lock().unwrap() = now;
            let mut stats: Vec<PeerStat> = peers
                .map
                .iter()
                .filter(|(id, p)| others.contains(id) && p.usable() && p.alive(now) && evaluate_pings(&p.pings, now).is_some())
                .map(|(id, p)| PeerStat { id: *id, rtt: p.quality.rtt_ms.max(1), loss: p.quality.loss_pct, jitter: p.quality.jitter_ms })
                .collect();
            stats.sort_by_key(|p| p.id);
            let slots = if self.relay_enabled.load(Ordering::Relaxed) {
                relay_slots(
                    self.budget_bps.load(Ordering::Relaxed),
                    voice.len(),
                    others.len(),
                    self.video_bps.load(Ordering::Relaxed),
                    video.len(),
                    watchers.len(),
                )
            } else {
                0
            };
            let mut via_server: Vec<u32> = prefs.iter().filter(|(_, r)| **r == RoutePref::Server).map(|(id, _)| *id).collect();
            via_server.sort();
            // 还能拿出来帮别人转发画面的上行：预算 - 自己的语音 - 自己共享的画面（帮人转发语音的量很小，忽略）
            let video_relay_bps = if self.relay_enabled.load(Ordering::Relaxed) {
                let own_video = self.video_bps.load(Ordering::Relaxed) as u64 * (video.len() as u64 + u64::from(video.len() < watchers.len()));
                let own_voice = VOICE_COST_BPS * (voice.len() as u64 + 1);
                (self.budget_bps.load(Ordering::Relaxed) as u64).saturating_sub(own_video + own_voice).min(u32::MAX as u64) as u32
            } else {
                0
            };
            self.tcp_send(Outgoing::Msg(ClientMsg::LinkStats {
                server_rtt: udp_rtt,
                peers: stats,
                relay_slots: slots,
                via_server,
                video_relay_bps,
            }));
        }
        // 直连有丢包（≥ 5%）但还没到「不稳」的人：语音两条路同时发（服务器也照常转发给他，接收端按序号去重）
        let dual_voice: std::collections::HashSet<u32> =
            voice.iter().copied().filter(|i| peers.map.get(i).is_some_and(|p| p.quality.loss_pct >= DUAL_VOICE_LOSS)).collect();
        voice.retain(|i| !dual_voice.contains(i));
        *self.dual_voice.lock().unwrap() = dual_voice;
        let dual_video = self.dual_video.lock().unwrap().clone();
        video.retain(|i| !dual_video.contains(i));
        voice.sort();
        video.sort();
        if voice != peers.last_direct || video != peers.last_direct_video {
            peers.last_direct = voice.clone();
            peers.last_direct_video = video.clone();
            drop(peers);
            self.tcp_send(Outgoing::Msg(ClientMsg::DirectPeers { ids: voice, video }));
        }
    }

    /// 处理来自非服务器地址的包。若是语音 / 视频，返回 (kind, 内容)（与服务器转发的格式相同）。
    fn on_peer_packet(&self, from: SocketAddr, pkt: &[u8], buf: &mut Vec<u8>) -> Option<(u32, u8, Vec<u8>)> {
        let mut peers = self.peers.lock().unwrap();
        // 先试地址匹配的对端，再试其余（对端 NAT 端口可能和候选不同）
        let mut ids: Vec<u32> = peers.map.keys().copied().collect();
        ids.sort_by_key(|id| {
            let p = &peers.map[id];
            !(p.path == Some(from) || p.cands.contains(&from))
        });
        for id in ids {
            let p = peers.map.get_mut(&id).unwrap();
            let Some(op) = p.opener.as_mut() else { continue };
            let Some((kind, data)) = op.open(pkt) else { continue };
            p.last_rx = Instant::now();
            let s = p.sealer.as_mut().unwrap();
            match kind {
                p2p::PUNCH => {
                    s.seal(p2p::PUNCH_ACK, &data, buf);
                    self.send_raw(buf, from);
                    if p.path.is_none() {
                        p.path = Some(from);
                    }
                }
                p2p::PUNCH_ACK => {
                    p.path = Some(from);
                    p.confirmed = true;
                    p.punch_started = None;
                    if data.len() == 8 {
                        p.rtt_ms = now_millis().saturating_sub(u64::from_le_bytes(data[..8].try_into().unwrap())) as u32;
                    }
                }
                p2p::PING => {
                    s.seal(p2p::PONG, &data, buf);
                    self.send_raw(buf, from);
                }
                p2p::PONG => {
                    p.on_pong(&data);
                    p.path = Some(from);
                }
                udp::VOICE | udp::VIDEO | udp::MUSIC | udp::SHARE_AUDIO | udp::NACK | udp::CURSOR => return Some((id, kind, data)),
                _ => {}
            }
            return None;
        }
        None
    }
}

// ------------------------------------------------------------ 解码

struct UserDecoder {
    dec: crate::opusx::Decoder,
    last_seq: Option<u16>,
}

/// 收到一包，前面丢了 gap 帧：先用这个包里的 DRED 还原（能还原多少算多少，越近的越准），
/// 剩下的离得远的用 PLC 补，紧挨着的一帧还可以用带内 FEC。返回按时间顺序的 (序号, 声音)
fn conceal_gap(d: &mut UserDecoder, last: u16, gap: u16, data: &[u8], pcm: &mut [i16]) -> Vec<(u16, Vec<i16>)> {
    let mut frames = Vec::new();
    let dred = d.dec.dred_recover(data, gap as usize, FRAME_SAMPLES);
    for i in 0..gap {
        let seq = last.wrapping_add(1 + i);
        if let Some(Some(f)) = dred.get(i as usize) {
            frames.push((seq, f.clone()));
            continue;
        }
        let n = if i + 1 == gap {
            d.dec.decode(data, &mut pcm[..FRAME_SAMPLES], true)
        } else {
            d.dec.decode(&[], &mut pcm[..FRAME_SAMPLES], false)
        };
        if let Some(n) = n {
            frames.push((seq, pcm[..n].to_vec()));
        }
    }
    frames
}

/// 中间丢了多少帧以内还补（DRED 最多约 1 秒；太长的断档当作重新开始）
const MAX_CONCEAL: u16 = 50;

/// UI 可读取的语音状态
#[derive(Default)]
pub struct VoiceState {
    pub link: Mutex<Option<Arc<Link>>>,
    /// 用户最后一次收到语音包的时间
    pub last_heard: Mutex<HashMap<u32, Instant>>,
    pub ping_ms: AtomicU32,
    decoders: Mutex<HashMap<u32, UserDecoder>>,
    /// 当前会话的端到端加密状态
    pub e2e: Mutex<Option<Arc<Mutex<E2e>>>>,
    /// 正在观看的屏幕共享
    pub video: VideoRx,
    /// 每个在线成员能解码的视频格式（共享时协商编码格式用）
    pub caps: Mutex<HashMap<u32, Vec<String>>>,
    /// 伴奏 / 屏幕共享声音的解码器：(发送者, 流)
    stream_decoders: Mutex<HashMap<(u32, u8), UserDecoder>>,
    /// 最近收到某人画面的线路：(是不是 P2P 直连, 时间)
    video_path: Mutex<HashMap<u32, (bool, Instant)>>,
    /// 屏幕共享支持补发的人
    pub rtx_sharers: Mutex<std::collections::HashSet<u32>>,
    /// 语音编码器正在用 DRED（线路有丢包）
    pub dred_active: AtomicBool,
    /// 服务器支持转发补发请求
    pub server_rtx: AtomicBool,
}

impl VoiceState {
    /// 最近这段时间里收到过他的语音
    pub fn heard_within(&self, user: u32, d: Duration) -> bool {
        self.last_heard.lock().unwrap().get(&user).is_some_and(|t| t.elapsed() < d)
    }

    /// 最近 2 秒收到他的画面走的线路：Some(true) 直连，Some(false) 经服务器
    pub fn video_direct(&self, from: u32) -> Option<bool> {
        self.video_path.lock().unwrap().get(&from).filter(|(_, t)| t.elapsed() < Duration::from_secs(2)).map(|(d, _)| *d)
    }

    pub fn is_speaking(&self, user: u32) -> bool {
        self.last_heard.lock().unwrap().get(&user).is_some_and(|t| t.elapsed() < Duration::from_millis(250))
    }

    pub fn current_link(&self) -> Option<Arc<Link>> {
        self.link.lock().unwrap().clone()
    }

    /// None = 未连接；Some(true) = UDP 直连服务器；Some(false) = TCP 隧道
    pub fn transport_udp(&self) -> Option<bool> {
        self.current_link().map(|l| l.udp_ok.load(Ordering::Relaxed))
    }

    pub fn disconnect(&self) {
        if let Some(l) = self.link.lock().unwrap().take() {
            l.close();
        }
        self.ping_ms.store(0, Ordering::Relaxed);
        self.decoders.lock().unwrap().clear();
        self.stream_decoders.lock().unwrap().clear();
        self.video.clear();
        *self.e2e.lock().unwrap() = None;
    }
}

/// 处理一帧伴奏 / 屏幕共享声音：解密、去重、丢包补偿、解码，送进这个人对应的混音通道
pub fn on_stream_audio(kind: u8, payload: &[u8], state: &VoiceState, mixer: &Mutex<Mixer>, pcm: &mut Vec<i16>) {
    let Some((from, _, blob)) = udp::read_server_voice(payload) else { return };
    let stream = if kind == udp::MUSIC { voc_proto::e2e::STREAM_MUSIC } else { voc_proto::e2e::STREAM_SHARE };
    let Some(e2e) = state.e2e.lock().unwrap().clone() else { return };
    let Some((ctr, data)) = e2e.lock().unwrap().decrypt_stream(from, stream, blob) else { return };
    let seq = ctr as u16;
    let mut decs = state.stream_decoders.lock().unwrap();
    let d = match decs.entry((from, stream)) {
        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
        std::collections::hash_map::Entry::Vacant(v) => {
            let Some(dec) = crate::opusx::Decoder::new(SAMPLE_RATE) else { return };
            v.insert(UserDecoder { dec, last_seq: None })
        }
    };
    pcm.resize(FRAME_SAMPLES * 3, 0);
    let mut frames: Vec<(u16, Vec<i16>)> = Vec::new();
    if let Some(last) = d.last_seq {
        if !seq_newer(seq, last) {
            return;
        }
        let gap = seq.wrapping_sub(last).wrapping_sub(1);
        if (1..=MAX_CONCEAL).contains(&gap) {
            frames.extend(conceal_gap(d, last, gap, &data, pcm));
        }
    }
    let Some(n) = d.dec.decode(&data, &mut pcm[..], false) else { return };
    d.last_seq = Some(seq);
    frames.push((seq, pcm[..n].to_vec()));
    drop(decs);
    let id = crate::audio::stream_mixer_id(kind, from);
    let mut m = mixer.lock().unwrap();
    for (s, f) in frames {
        m.push(id, s, &f);
    }
}

/// 共享者单独发来的鼠标位置
fn on_cursor(payload: &[u8], state: &VoiceState) {
    let Some((from, _, blob)) = udp::read_server_voice(payload) else { return };
    let Some(e2e) = state.e2e.lock().unwrap().clone() else { return };
    let Some((_, data)) = e2e.lock().unwrap().decrypt_stream(from, voc_proto::e2e::STREAM_CURSOR, blob) else { return };
    if let Some((x, y, shape, vis)) = udp::read_cursor(&data) {
        state.video.set_cursor(from, x, y, shape, vis);
    }
}

/// 处理一个视频分片（服务器中转 / P2P 直连 / TCP 隧道共用）；需要时向共享者请求关键帧
/// direct = 这个包是 P2P 直连收到的（否则经服务器 UDP / TCP 隧道）
pub fn on_server_video(payload: &[u8], state: &VoiceState, direct: bool) {
    if let Some((from, _, _)) = udp::read_server_video(payload) {
        state.video_path.lock().unwrap().insert(from, (direct, Instant::now()));
    }
    let Some(e2e) = state.e2e.lock().unwrap().clone() else { return };
    let acts = state.video.on_packet(payload, |f| sender_info(state, f), |from, blob| e2e.lock().unwrap().decrypt_video(from, blob));
    apply_video_actions(state, acts);
}

/// 测试用：环境变量 VOC_TEST_DELAY_MS=毫秒，收到的每个 UDP 包都晚这么久才处理（模拟高延迟线路；两端都设 = 往返多两倍）
struct TestDelay {
    rx: std::sync::mpsc::Receiver<(Instant, SocketAddr, Vec<u8>)>,
    delay: Duration,
}

impl TestDelay {
    fn start(sock: &UdpSocket, link: Arc<Link>) -> Option<Self> {
        let ms: u64 = std::env::var("VOC_TEST_DELAY_MS").ok()?.parse().ok().filter(|m| *m > 0)?;
        let sock = sock.try_clone().ok()?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("test-delay".into())
            .spawn(move || {
                let mut b = [0u8; 2048];
                while !link.closed.load(Ordering::Relaxed) {
                    if let Ok((n, f)) = sock.recv_from(&mut b) {
                        if tx.send((Instant::now(), f, b[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                }
            })
            .ok()?;
        Some(Self { rx, delay: Duration::from_millis(ms) })
    }

    /// 取下一个到时间的包（按到达顺序，每个都晚 delay）
    fn next(&mut self, out: &mut Vec<u8>) -> Option<SocketAddr> {
        let (t, from, pkt) = self.rx.recv_timeout(Duration::from_millis(200)).ok()?;
        let due = t + self.delay;
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        *out = pkt;
        Some(from)
    }
}

/// 测试用：环境变量 VOC_TEST_LOSS=百分比，随机丢掉收到的画面包（模拟网络丢包）
fn test_drop_video() -> bool {
    static LOSS: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    static RNG: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
    let loss = *LOSS.get_or_init(|| std::env::var("VOC_TEST_LOSS").ok().and_then(|v| v.parse::<f32>().ok()).map(|p| (p * 100.0) as u32).unwrap_or(0));
    if loss == 0 {
        return false;
    }
    let mut x = RNG.load(Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    RNG.store(x, Ordering::Relaxed);
    (x % 10_000) < loss as u64
}

/// 定时检查正在看的画面（尾巴丢片、等补发超时）
pub fn video_tick(state: &VoiceState) {
    let Some(e2e) = state.e2e.lock().unwrap().clone() else { return };
    let acts = state.video.tick(|f| sender_info(state, f), |from, blob| e2e.lock().unwrap().decrypt_video(from, blob));
    apply_video_actions(state, acts);
}

/// 某个共享者：支不支持补发、线路往返时间参考（直连用直连的 ping；经服务器按我到服务器的 ping 的两倍估）
fn sender_info(state: &VoiceState, from: u32) -> crate::video::SenderInfo {
    let rtx = state.rtx_sharers.lock().unwrap().contains(&from);
    let rtt_hint = match (state.video_direct(from), state.current_link()) {
        (Some(true), Some(l)) => l.peer_rtt(from).unwrap_or(0),
        _ => state.ping_ms.load(Ordering::Relaxed) * 2,
    };
    crate::video::SenderInfo { rtx, rtt_hint }
}

fn apply_video_actions(state: &VoiceState, acts: crate::video::RxActions) {
    if acts == crate::video::RxActions::default() {
        return;
    }
    let Some(l) = state.current_link() else { return };
    for from in acts.keyframes {
        l.request_keyframe(from);
    }
    for (target, loss, rtt) in acts.reports {
        l.tcp_send(Outgoing::Msg(ClientMsg::VideoReport { target, loss, rtt }));
    }
    for (target, frame) in acts.ltr_acks {
        l.tcp_send(Outgoing::Msg(ClientMsg::VideoLtr { target, ack: Some(frame), recover: false }));
    }
    for target in acts.recovers {
        l.tcp_send(Outgoing::Msg(ClientMsg::VideoLtr { target, ack: None, recover: true }));
    }
    let server_rtx = state.server_rtx.load(Ordering::Relaxed);
    for (from, layer, frame, idxs) in acts.nacks {
        let direct = state.video_direct(from) == Some(true);
        l.send_nack(from, direct, server_rtx, &udp::nack_payload(from, layer, frame, &idxs));
    }
}

/// 处理一帧语音（服务器中转 / P2P 直连 / TCP 隧道共用）：去重、丢包补偿、解码、送混音器
pub fn on_server_voice(payload: &[u8], state: &VoiceState, mixer: &Mutex<Mixer>, pcm: &mut Vec<i16>) {
    let Some((from, _, blob)) = udp::read_server_voice(payload) else { return };
    // 端到端解密；以发送者加密的帧计数器作为序号（服务器改不了）
    let Some(e2e) = state.e2e.lock().unwrap().clone() else { return };
    let Some((ctr, data)) = e2e.lock().unwrap().decrypt_frame(from, blob) else { return };
    let seq = ctr as u16;
    let data = &data[..];
    let mut decs = state.decoders.lock().unwrap();
    let d = match decs.entry(from) {
        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
        std::collections::hash_map::Entry::Vacant(v) => {
            let Some(dec) = crate::opusx::Decoder::new(SAMPLE_RATE) else { return };
            v.insert(UserDecoder { dec, last_seq: None })
        }
    };
    pcm.resize(FRAME_SAMPLES * 3, 0);
    let mut frames: Vec<(u16, Vec<i16>)> = Vec::new();
    if let Some(last) = d.last_seq {
        if !seq_newer(seq, last) {
            return; // 重复（直连和中转都到了）或过期
        }
        let gap = seq.wrapping_sub(last).wrapping_sub(1);
        if (1..=MAX_CONCEAL).contains(&gap) {
            // 丢包：先用本包里的 DRED 深度冗余还原，不够的用 FEC / PLC
            frames.extend(conceal_gap(d, last, gap, data, pcm));
        }
    }
    let Some(n) = d.dec.decode(data, &mut pcm[..], false) else { return };
    d.last_seq = Some(seq);
    frames.push((seq, pcm[..n].to_vec()));
    drop(decs);
    let mut m = mixer.lock().unwrap();
    for (s, f) in frames {
        m.push(from, s, &f);
    }
    drop(m);
    state.last_heard.lock().unwrap().insert(from, Instant::now());
}

// ------------------------------------------------------------ 建立链路

/// 建立加密 UDP 链路（含 P2P）并启动接收与定时线程
pub fn connect(
    server: SocketAddr,
    probe_port: u16,
    keys: &SessionKeys,
    my_id: u32,
    p2p_on: bool,
    e2e: Arc<Mutex<E2e>>,
    tcp: Sender<Outgoing>,
    state: Arc<VoiceState>,
    mixer: Arc<Mutex<Mixer>>,
) -> std::io::Result<()> {
    let (sock, dual) = make_socket(server)?;
    sock.set_read_timeout(Some(Duration::from_millis(200)))?;
    let port = sock.local_addr()?.port();
    let recv_sock = sock.try_clone()?;
    let link = Arc::new(Link {
        sock,
        dual,
        server: norm(server),
        sealer: Mutex::new(DatagramSealer::new(&keys.udp_c2s, &keys.udp_c2s_hp)),
        tcp: Mutex::new(tcp),
        seq: AtomicU16::new(0),
        closed: AtomicBool::new(false),
        last_pong_ms: AtomicU64::new(0),
        udp_ok: AtomicBool::new(false),
        in_channel: AtomicBool::new(false),
        my_id,
        e2e: e2e.clone(),
        p2p_enabled: AtomicBool::new(false),
        local_addrs: Mutex::new(local_candidates(port, dual)),
        peers: Mutex::new(Peers::default()),
        others: Mutex::new(Vec::new()),
        watchers: Mutex::new(Vec::new()),
        watchers_ver: AtomicU32::new(0),
        watchers_low: Mutex::new(Vec::new()),
        watcher_routes: Mutex::new((Vec::new(), Vec::new())),
        watchers_cursor: Mutex::new(Vec::new()),
        budget_bps: AtomicU32::new(DEFAULT_BUDGET_BPS),
        video_bps: AtomicU32::new(0),
        alloc: Mutex::new((Vec::new(), Vec::new())),
        server_rtt: AtomicU32::new(0),
        relay_enabled: AtomicBool::new(true),
        relays: Mutex::new(Vec::new()),
        video_relays: Mutex::new(Vec::new()),
        last_stats: Mutex::new(Instant::now()),
        pace_tx: Mutex::new(None),
        music_seq: AtomicU16::new(0),
        share_seq: AtomicU16::new(0),
        pace_dropped: AtomicU32::new(0),
        portmap: Mutex::new(crate::portmap::PortMapStatus::Trying),
        lan_addrs: local_candidates(port, dual).iter().filter_map(|a| a.parse().ok()).collect(),
        local_port: port,
        created: Instant::now(),
        public_addr: Mutex::new(None),
        probe: (probe_port != 0).then(|| norm(SocketAddr::new(server.ip(), probe_port))),
        probe_addr: Mutex::new(None),
        nat: Mutex::new(None),
        srv_pings: Mutex::new(std::collections::VecDeque::new()),
        sent_bytes: AtomicU64::new(0),
        up_history: Mutex::new(std::collections::VecDeque::new()),
        route_prefs: Mutex::new(HashMap::new()),
        last_peer_list: Mutex::new(Vec::new()),
        video_hist: Mutex::new(std::collections::VecDeque::new()),
        video_frags_sent: AtomicU32::new(0),
        video_frags_nacked: AtomicU32::new(0),
        video_reports: Mutex::new(HashMap::new()),
        dual_video: Mutex::new(Default::default()),
        dual_voice: Mutex::new(Default::default()),
        ltr_acks: Mutex::new(Vec::new()),
        ltr_recover: AtomicBool::new(false),
    });
    spawn_pacer(link.clone());
    state.disconnect();
    *state.link.lock().unwrap() = Some(link.clone());
    *state.e2e.lock().unwrap() = Some(e2e);
    link.set_p2p(p2p_on);
    spawn_upnp(link.clone(), port);

    // 定时：服务器 ping（每秒）、打洞与直连保活（每 100 ms）
    let l = link.clone();
    let st = state.clone();
    std::thread::Builder::new().name("udp-tick".into()).spawn(move || {
        let mut buf = Vec::new();
        let mut n = 0u32;
        let mut last_sent = 0u64;
        while !l.closed.load(Ordering::Relaxed) {
            if n % 10 == 0 {
                // 每秒记一次上行码率
                let sent = l.sent_bytes.load(Ordering::Relaxed);
                if n > 0 {
                    let mut h = l.up_history.lock().unwrap();
                    h.push_back((sent.saturating_sub(last_sent) * 8 / 1000) as u32);
                    while h.len() > 30 {
                        h.pop_front();
                    }
                }
                last_sent = sent;
                let ts = now_millis();
                l.send_server(udp::PING, &udp::ping_payload(ts), &mut buf);
                if let Some(probe) = l.probe {
                    if n < 100 || n % 300 == 0 {
                        l.sealer.lock().unwrap().seal(udp::PING, &udp::ping_payload(ts), &mut buf);
                        l.send_raw(&buf, probe);
                    }
                }
                {
                    let mut p = l.srv_pings.lock().unwrap();
                    p.push_back((ts, false));
                    while p.len() > 30 {
                        p.pop_front();
                    }
                }
                let last = l.last_pong_ms.load(Ordering::Relaxed);
                let ok = last != 0 && now_millis().saturating_sub(last) < UDP_DEAD_MS;
                if ok != l.udp_ok.swap(ok, Ordering::Relaxed) {
                    l.tcp_send(Outgoing::Msg(ClientMsg::Transport { udp: ok }));
                    if !ok {
                        st.ping_ms.store(0, Ordering::Relaxed);
                    }
                }
            }
            l.tick(&mut buf);
            video_tick(&st);
            n = n.wrapping_add(1);
            std::thread::sleep(Duration::from_millis(100));
        }
    })?;

    // 接收
    let l = link;
    let mut opener = DatagramOpener::new(&keys.udp_s2c, &keys.udp_s2c_hp);
    std::thread::Builder::new().name("udp-recv".into()).spawn(move || {
        crate::audio::boost_audio_thread();
        let mut buf = [0u8; 2048];
        let mut out = Vec::new();
        let mut pcm = Vec::with_capacity(FRAME_SAMPLES * 3);
        let mut delayed = TestDelay::start(&recv_sock, l.clone());
        let mut held = Vec::new();
        while !l.closed.load(Ordering::Relaxed) {
            // 超时 / Windows 的 ConnectionReset（打洞时常见）都忽略
            let (n, from) = match delayed.as_mut() {
                Some(d) => match d.next(&mut held) {
                    Some(f) => {
                        buf[..held.len()].copy_from_slice(&held);
                        (held.len(), f)
                    }
                    None => continue,
                },
                None => {
                    let Ok(r) = recv_sock.recv_from(&mut buf) else { continue };
                    r
                }
            };
            let from = norm(from);
            let pkt = &buf[..n];
            if Some(from) == l.probe {
                if let Some((udp::PONG, data)) = opener.open(pkt) {
                    if let Some((_, Some(addr))) = udp::read_pong(&data) {
                        *l.probe_addr.lock().unwrap() = Some(addr);
                        l.update_nat();
                    }
                }
                continue;
            }
            if from == l.server {
                let Some((kind, data)) = opener.open(pkt) else { continue };
                match kind {
                    udp::PONG => {
                        let Some((sent, public)) = udp::read_pong(&data) else { continue };
                        let now = now_millis();
                        if let Some(p) = l.srv_pings.lock().unwrap().iter_mut().find(|p| p.0 == sent) {
                            p.1 = true;
                        }
                        if public.is_some() {
                            *l.public_addr.lock().unwrap() = public;
                            l.update_nat();
                        }
                        l.last_pong_ms.store(now, Ordering::Relaxed);
                        let rtt = (now.saturating_sub(sent) as u32).max(1);
                        state.ping_ms.store(rtt, Ordering::Relaxed);
                        l.server_rtt.store(rtt, Ordering::Relaxed);
                    }
                    udp::VOICE => on_server_voice(&data, &state, &mixer, &mut pcm),
                    udp::VIDEO if test_drop_video() => {}
                    udp::VIDEO => on_server_video(&data, &state, false),
                    udp::NACK => l.resend_video(None, &data),
                    udp::MUSIC | udp::SHARE_AUDIO => on_stream_audio(kind, &data, &state, &mixer, &mut pcm),
                    udp::CURSOR => on_cursor(&data, &state),
                    _ => {}
                }
            } else if let Some((peer, kind, v)) = l.on_peer_packet(from, pkt, &mut out) {
                match kind {
                    udp::VIDEO if test_drop_video() => {}
                    udp::VIDEO => {
                        l.relay_video(peer, &v, &mut out);
                        on_server_video(&v, &state, true);
                    }
                    udp::NACK => l.resend_video(Some(peer), &v),
                    udp::MUSIC | udp::SHARE_AUDIO => on_stream_audio(kind, &v, &state, &mixer, &mut pcm),
                    udp::CURSOR => on_cursor(&v, &state),
                    _ => {
                        l.relay_forward(peer, &v, &mut out);
                        on_server_voice(&v, &state, &mixer, &mut pcm);
                    }
                }
            }
        }
    })?;
    Ok(())
}

/// 屏幕画面匀速发送：令牌桶限速，过期的包丢掉。语音不经过这里，永远先发。
fn spawn_pacer(link: Arc<Link>) {
    let (tx, rx) = std::sync::mpsc::channel::<(Instant, SocketAddr, Vec<u8>)>();
    *link.pace_tx.lock().unwrap() = Some(tx);
    let _ = std::thread::Builder::new().name("video-pacer".into()).spawn(move || {
        let mut tokens = PACE_BURST as f64;
        let mut last = Instant::now();
        while !link.closed.load(Ordering::Relaxed) {
            let Ok((queued, to, pkt)) = rx.recv_timeout(Duration::from_millis(200)) else { continue };
            if queued.elapsed() > PACE_STALE {
                link.pace_dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let rate = (link.video_bps.load(Ordering::Relaxed) as u64 * PACE_FACTOR).max(PACE_MIN_BPS) as f64 / 8.0;
            let now = Instant::now();
            tokens = (tokens + now.duration_since(last).as_secs_f64() * rate).min(PACE_BURST as f64);
            last = now;
            if tokens < pkt.len() as f64 {
                let wait = (pkt.len() as f64 - tokens) / rate;
                std::thread::sleep(Duration::from_secs_f64(wait));
                let now = Instant::now();
                tokens += now.duration_since(last).as_secs_f64() * rate;
                last = now;
            }
            tokens -= pkt.len() as f64;
            link.send_raw(&pkt, to);
        }
    });
}

/// 让家用路由器做 UDP 端口映射：UPnP → PCP → NAT-PMP，哪个成功用哪个；成功后把公网地址加入 P2P 候选。
/// 结果记在链路上（网络自检显示）；路由器本身没有公网 IP（运营商级 NAT）时映射没用，跳过。
fn spawn_upnp(link: Arc<Link>, port: u16) {
    use crate::portmap::{self, PortMapStatus, LEASE_SECS};
    let _ = std::thread::Builder::new().name("portmap".into()).spawn(move || {
        let set = |s: PortMapStatus| *link.portmap.lock().unwrap() = s;
        let lan = link
            .local_addrs
            .lock()
            .unwrap()
            .iter()
            .filter_map(|a| a.parse::<SocketAddr>().ok())
            .find(|a| a.is_ipv4());
        let Some(SocketAddr::V4(lan)) = lan else {
            set(PortMapStatus::Unavailable);
            return;
        };
        // 1. UPnP
        enum Kind {
            Upnp(igd_next::Gateway, SocketAddr),
            Pcp(std::net::Ipv4Addr),
            NatPmp(std::net::Ipv4Addr),
        }
        let mut got: Option<(Kind, std::net::Ipv4Addr, u16, &'static str)> = None;
        {
            use igd_next::{PortMappingProtocol, SearchOptions};
            let mut opts = SearchOptions::default();
            opts.timeout = Some(Duration::from_secs(3));
            if let Ok(gw) = igd_next::search_gateway(opts) {
                if let Ok(IpAddr::V4(ext_ip)) = gw.get_external_ip() {
                    if portmap::is_carrier_or_private(ext_ip) {
                        set(PortMapStatus::CarrierNat { method: "UPnP", router_external: ext_ip });
                        return;
                    }
                    let local = SocketAddr::V4(lan);
                    let ext_port = match gw.add_port(PortMappingProtocol::UDP, port, local, LEASE_SECS, "VOC") {
                        Ok(()) => Some(port),
                        Err(_) => gw.add_any_port(PortMappingProtocol::UDP, local, LEASE_SECS, "VOC").ok(),
                    };
                    if let Some(p) = ext_port {
                        got = Some((Kind::Upnp(gw, local), ext_ip, p, "UPnP"));
                    }
                }
            }
        }
        // 2. PCP、3. NAT-PMP（都发给默认网关）
        if got.is_none() {
            if let Some(gw) = portmap::gateway_for(*lan.ip()) {
                if let Some((ip, p)) = portmap::pcp_map(gw, *lan.ip(), port, LEASE_SECS) {
                    got = Some((Kind::Pcp(gw), ip, p, "PCP"));
                } else if let Some((ip, p)) = portmap::natpmp_map(gw, port, LEASE_SECS) {
                    got = Some((Kind::NatPmp(gw), ip, p, "NAT-PMP"));
                }
            }
        }
        let Some((kind, ext_ip, ext_port, method)) = got else {
            set(PortMapStatus::Unavailable);
            return;
        };
        if portmap::is_carrier_or_private(ext_ip) {
            set(PortMapStatus::CarrierNat { method, router_external: ext_ip });
            return;
        }
        let external = SocketAddr::new(IpAddr::V4(ext_ip), ext_port);
        set(PortMapStatus::Mapped { method, external });
        link.add_local_candidate(external.to_string());
        // 到一半有效期就续；断开时删除映射
        let renew = || match &kind {
            Kind::Upnp(gw, local) => {
                let _ = gw.add_port(igd_next::PortMappingProtocol::UDP, ext_port, *local, LEASE_SECS, "VOC");
            }
            Kind::Pcp(gw) => {
                let _ = portmap::pcp_map(*gw, *lan.ip(), port, LEASE_SECS);
            }
            Kind::NatPmp(gw) => {
                let _ = portmap::natpmp_map(*gw, port, LEASE_SECS);
            }
        };
        let mut since = Instant::now();
        while !link.closed.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(500));
            if since.elapsed() > Duration::from_secs(LEASE_SECS as u64 / 2) {
                renew();
                since = Instant::now();
            }
        }
        match &kind {
            Kind::Upnp(gw, _) => {
                let _ = gw.remove_port(igd_next::PortMappingProtocol::UDP, ext_port);
            }
            Kind::Pcp(gw) => {
                let _ = portmap::pcp_map(*gw, *lan.ip(), port, 0);
            }
            Kind::NatPmp(gw) => {
                let _ = portmap::natpmp_map(*gw, port, 0);
            }
        }
    });
}

/// 常驻编码线程：麦克风 48 kHz 帧 → 增益 → 降噪 → 16 kHz → VAD → Opus → 发送
pub fn spawn_encoder(
    frames: Receiver<Vec<f32>>,
    settings: Arc<Settings>,
    state: Arc<VoiceState>,
    mixer: Arc<Mutex<Mixer>>,
    denoise: DenoiseSlot,
) {
    std::thread::Builder::new()
        .name("encoder".into())
        .spawn(move || {
            crate::audio::boost_audio_thread();
            // 采集和编码都是 48 kHz 时不需要重采样（省掉一次插值和滤波延迟）
            let mut down = (CAPTURE_RATE != SAMPLE_RATE).then(|| PushResampler::new(CAPTURE_RATE, SAMPLE_RATE));
            let mut acc: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES * 2);
            let mut tx = Transmitter::new(settings.clone(), state, mixer);
            let mut ec = crate::aec::EchoCanceller::new();
            loop {
                let mut frame = match frames.recv_timeout(Duration::from_millis(200)) {
                    Ok(f) => f,
                    Err(RecvTimeoutError::Timeout) => {
                        settings.input_level_db.set(-100.0);
                        settings.transmitting.store(false, Ordering::Relaxed);
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                let gain = settings.input_gain.get();
                for x in frame.iter_mut() {
                    *x = (*x * gain).clamp(-1.0, 1.0);
                }
                // 先消回声（要用原始麦克风信号），再降噪
                if settings.aec.load(Ordering::Relaxed) {
                    ec.process(&mut frame);
                } else {
                    ec.discard_render();
                }
                if let Some(d) = denoise.lock().unwrap().as_mut() {
                    d.process(&mut frame);
                }
                match down.as_mut() {
                    Some(d) => {
                        for &x in &frame {
                            d.push(x, |y| acc.push(y));
                        }
                    }
                    None => acc.extend_from_slice(&frame),
                }
                while acc.len() >= FRAME_SAMPLES {
                    tx.on_frame(&acc[..FRAME_SAMPLES]);
                    acc.drain(..FRAME_SAMPLES);
                }
            }
        })
        .expect("spawn encoder");
}

fn new_encoder() -> crate::opusx::Encoder {
    let mut e = crate::opusx::Encoder::new_voip(SAMPLE_RATE).expect("opus encoder");
    e.set_bitrate(OPUS_BITRATE);
    e.set_dred_duration(DRED_DURATION);
    apply_loss_mode(&mut e, 0);
    e
}

/// DRED 冗余时长（10 ms 为单位）：最多约 0.5 秒（实际长度受码率限制，24 kbps 时约 130 ms）
const DRED_DURATION: i32 = 50;
/// 线路丢包 ≥ 这个百分比时改用 DRED（带内 FEC 只能补一帧，而且开着它 DRED 几乎分不到码率）
const DRED_LOSS_PCT: u8 = 3;

/// 按线路丢包调整编码器：丢包少时用带内 FEC（不影响音质）；丢包多时关 FEC、按实测丢包率给 DRED 分码率
fn apply_loss_mode(e: &mut crate::opusx::Encoder, loss_pct: u8) {
    if loss_pct >= DRED_LOSS_PCT {
        e.set_inband_fec(false);
        e.set_packet_loss_perc(loss_pct.min(50) as i32);
    } else {
        e.set_inband_fec(true); // 带内前向纠错：丢一个包可由下一个包恢复
        e.set_packet_loss_perc(10);
    }
}

/// 16 kHz / 20 ms 帧的 VAD + 编码 + 发送
struct Transmitter {
    settings: Arc<Settings>,
    state: Arc<VoiceState>,
    mixer: Arc<Mutex<Mixer>>,
    enc: crate::opusx::Encoder,
    loop_dec: Option<opus::Decoder>,
    /// 当前按多少丢包率配置编码器（每秒按线路实测更新一次）
    loss_mode: u8,
    loss_checked: Instant,
    pcm: Vec<i16>,
    bytes: Vec<u8>,
    decoded: Vec<i16>,
    pkt: Vec<u8>,
    hang: u32,
    loop_seq: u16,
    /// 编码器当前是否按低带宽码率
    low: bool,
    /// 连续多少帧完全没有声音（全 0）
    zero_frames: u32,
}

impl Transmitter {
    fn new(settings: Arc<Settings>, state: Arc<VoiceState>, mixer: Arc<Mutex<Mixer>>) -> Self {
        Self {
            settings,
            state,
            mixer,
            enc: new_encoder(),
            loop_dec: opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono).ok(),
            pcm: vec![0; FRAME_SAMPLES],
            bytes: vec![0; MAX_OPUS_PACKET],
            decoded: vec![0; FRAME_SAMPLES],
            pkt: Vec::with_capacity(udp::MAX_PACKET),
            hang: 0,
            loop_seq: 0,
            low: false,
            zero_frames: 0,
            loss_mode: 0,
            loss_checked: Instant::now(),
        }
    }

    fn on_frame(&mut self, frame: &[f32]) {
        let s = &self.settings;
        let mut energy = 0.0f32;
        for (o, &v) in self.pcm.iter_mut().zip(frame) {
            energy += v * v;
            *o = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
        }
        let rms = (energy / FRAME_SAMPLES as f32).sqrt();
        let db = 20.0 * (rms + 1e-9).log10();
        s.input_level_db.set(db);
        // 麦克风连续 5 秒给的全是 0：多半是 Windows 隐私设置不让 VOC 用麦克风，或设备坏了（真实环境总有底噪）
        self.zero_frames = if energy < 1e-12 { self.zero_frames.saturating_add(1) } else { 0 };
        s.mic_silent.store(self.zero_frames >= MIC_SILENT_FRAMES, Ordering::Relaxed);

        let ptt = s.ptt_vk.load(Ordering::Relaxed);
        let speaking = if ptt != 0 { key_down(ptt) } else { db >= s.vad_threshold_db.get() };
        if speaking {
            self.hang = if ptt != 0 { PTT_HANGOVER } else { VAD_HANGOVER };
        } else {
            self.hang = self.hang.saturating_sub(1);
        }
        let muted = s.muted.load(Ordering::Relaxed) || s.deafened.load(Ordering::Relaxed);
        let gate = self.hang > 0 && !muted;
        s.transmitting.store(gate, Ordering::Relaxed);
        if !gate {
            return;
        }

        let low = crate::audio::LOW_BANDWIDTH.load(Ordering::Relaxed);
        if low != self.low {
            self.low = low;
            self.enc.set_bitrate(if low { LOW_OPUS_BITRATE } else { OPUS_BITRATE });
        }
        // 每秒看一次线路丢包，变了就调编码器（FEC / DRED）
        if self.loss_checked.elapsed() >= Duration::from_secs(1) {
            self.loss_checked = Instant::now();
            let loss = self.state.current_link().map(|l| l.voice_path_loss()).unwrap_or(0);
            let mode = if loss >= DRED_LOSS_PCT { loss } else { 0 };
            if mode.abs_diff(self.loss_mode) >= 2 || (mode == 0) != (self.loss_mode == 0) {
                self.loss_mode = mode;
                apply_loss_mode(&mut self.enc, mode);
                self.state.dred_active.store(mode > 0, Ordering::Relaxed);
            }
        }
        let Some(n) = self.enc.encode(&self.pcm, &mut self.bytes) else { return };
        let packet = &self.bytes[..n];
        if let Some(l) = self.state.current_link().filter(|l| l.in_channel.load(Ordering::Relaxed)) {
            // 端到端加密后再发：服务器中转时只看得到密文
            if let Some(e2e) = self.state.e2e.lock().unwrap().clone() {
                let enc = e2e.lock().unwrap().encrypt_frame(packet);
                l.send_voice(&enc, &mut self.pkt);
            }
        }
        if s.loopback.load(Ordering::Relaxed) {
            if let Some(d) = self.loop_dec.as_mut() {
                if let Ok(m) = d.decode(packet, &mut self.decoded, false) {
                    self.mixer.lock().unwrap().push(LOOPBACK_ID, self.loop_seq, &self.decoded[..m]);
                    self.loop_seq = self.loop_seq.wrapping_add(1);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn nat_classification_and_strategy() {
        let lan: Vec<SocketAddr> = vec!["192.168.1.5:40000".parse().unwrap()];
        let a = |s: &str| s.parse::<SocketAddr>().unwrap();
        let cone = classify_nat(a("1.2.3.4:40000"), a("1.2.3.4:40000"), &lan);
        assert_eq!(cone.kind, voc_proto::NAT_CONE);
        let open = classify_nat(a("192.168.1.5:40000"), a("192.168.1.5:40000"), &lan);
        assert_eq!(open.kind, voc_proto::NAT_OPEN);
        let pred = classify_nat(a("1.2.3.4:40000"), a("1.2.3.4:40002"), &lan);
        assert_eq!((pred.kind, pred.port, pred.step), (voc_proto::NAT_SYM_PREDICTABLE, 40002, 2));
        assert_eq!(&predicted_ports(pred)[..3], &[40004, 40006, 40008]);
        let rnd = classify_nat(a("1.2.3.4:40000"), a("1.2.3.4:61234"), &lan);
        assert_eq!(rnd.kind, voc_proto::NAT_SYM_RANDOM);
        assert!(predicted_ports(rnd).is_empty());
        // 两边都对称且有一边随机：不打洞；一边好打洞就照打
        assert!(hopeless_pair(Some(rnd), Some(pred)));
        assert!(!hopeless_pair(Some(pred), Some(pred)));
        assert!(!hopeless_pair(Some(rnd), Some(cone)));
        assert!(!hopeless_pair(None, Some(rnd)), "unknown: keep trying");
    }

    use super::*;
    use crate::e2e::tests::pair_up;
    use voc_proto::e2e::Identity;

    fn e2e_pair(a_id: u32, b_id: u32) -> (Arc<Mutex<E2e>>, Arc<Mutex<E2e>>) {
        let mut a = E2e::new(a_id, "a", Identity::generate(), HashMap::new());
        let mut b = E2e::new(b_id, "b", Identity::generate(), HashMap::new());
        pair_up(&mut a, &mut b);
        a.set_channel_others(vec![b_id]);
        b.set_channel_others(vec![a_id]);
        pair_up(&mut a, &mut b);
        (Arc::new(Mutex::new(a)), Arc::new(Mutex::new(b)))
    }

    #[test]
    fn unstable_peer_falls_back_and_recovers() {
        let mut p = Peer::new(Vec::new());
        p.confirmed = true;
        let t0 = Instant::now();
        // 模拟：每 500 ms 一个 ping；前 10 秒稳定（30 ms），之后 30% 丢包
        let mut t = t0;
        let mut simulate = |p: &mut Peer, secs: u64, lossy: bool, rtt: u32| {
            for i in 0..secs * 2 {
                let ping = p.next_ping(t);
                let lost = lossy && i % 10 < 3;
                if !lost {
                    let seq = u32::from_le_bytes(ping[..4].try_into().unwrap());
                    if let Some(l) = p.pings.iter_mut().find(|l| l.seq == seq) {
                        l.rtt = Some(rtt + (i as u32 % 3)); // 少量抖动
                    }
                }
                t += Duration::from_millis(500);
                p.update_quality(t);
            }
        };
        simulate(&mut p, 10, false, 30);
        assert!(p.usable(), "stable link is used directly");
        assert!(p.quality.loss_pct == 0 && p.quality.rtt_ms >= 30);
        simulate(&mut p, 5, true, 30);
        assert!(!p.usable() && p.quality.unstable, "30% loss -> relay");
        assert!(p.quality.loss_pct >= 15);
        // 恢复稳定后：不会马上切回（至少中转 30 秒 + 连续稳定 10 秒）
        simulate(&mut p, 12, false, 30);
        assert!(!p.usable(), "hold relay for a while (hysteresis)");
        simulate(&mut p, 30, false, 30);
        assert!(p.usable(), "back to direct after being stable");
        // 高延迟 / 大抖动也算不稳
        assert!(quality_is_bad(0, 400, 10));
        assert!(quality_is_bad(0, 50, 120));
        assert!(!quality_is_bad(5, 120, 30));
    }

    #[test]
    fn bandwidth_allocation() {
        let peers: Vec<u32> = (1..=19).collect();
        // 19 人语音 ≈ 1.14 Mbps，15 Mbps 预算全部直连，无需服务器副本
        let (v, d) = allocate_direct(15_000_000, &peers, 19, 0, &[], 0);
        assert_eq!((v.len(), d.len()), (19, 0));
        // 只有 10 人打通：10 份直连 + 1 份给服务器
        let (v, _) = allocate_direct(15_000_000, &peers[..10], 19, 0, &[], 0);
        assert_eq!(v, peers[..10].to_vec());
        // 预算很小（0.5 Mbps）：7 份直连 + 1 份服务器 = 8 × 60k = 480k
        let (v, _) = allocate_direct(500_000, &peers, 19, 0, &[], 0);
        assert_eq!(v.len(), 7);
        // 1080p60（5.5 Mbps×1.05）6 个观看者都已打通：15 Mbps 预算扣掉语音后只够直连 1 个 + 1 份服务器
        let w: Vec<u32> = (1..=6).collect();
        let (v, d) = allocate_direct(15_000_000, &peers, 19, 5_500_000, &w, 6);
        assert_eq!(v.len(), 19);
        assert_eq!(d, vec![1]);
        // 720p30（2 Mbps×1.05）：15 Mbps 扣掉语音还剩 13.86，6 个全部直连（12.6）
        let (_, d) = allocate_direct(15_000_000, &peers, 19, 2_000_000, &w, 6);
        assert_eq!(d.len(), 6);
        // 预算 12 Mbps：剩 10.86，只够 4 个直连 + 1 份服务器（10.5）
        let (_, d) = allocate_direct(12_000_000, &peers, 19, 2_000_000, &w, 6);
        assert_eq!(d.len(), 4);
        let (_, d) = allocate_direct(100_000_000, &peers, 19, 5_500_000, &w, 6);
        assert_eq!(d.len(), 6);
        // 优先给延迟低的（调用方已排序）：取前缀
        let (_, d) = allocate_direct(15_000_000, &peers, 19, 2_000_000, &[9, 3, 7], 3);
        assert_eq!(d, vec![9, 3, 7]);
        // 没人可直连
        assert_eq!(allocate_direct(15_000_000, &[], 5, 2_000_000, &[], 2), (vec![], vec![]));
    }

    #[test]
    fn addr_mapping_roundtrip() {
        let v4: SocketAddr = "1.2.3.4:5".parse().unwrap();
        assert_eq!(norm(wire(v4, true)), v4);
        assert_eq!(wire(v4, false), v4);
        let v6: SocketAddr = "[2001:db8::1]:7".parse().unwrap();
        assert_eq!(norm(wire(v6, true)), v6);
    }

    #[test]
    fn opus_e2e_roundtrip_with_fec_and_plc() {
        let (ea, eb) = e2e_pair(5, 6);
        let state = VoiceState::default();
        *state.e2e.lock().unwrap() = Some(eb);
        let mixer = Mutex::new(Mixer::default());
        let mut enc = new_encoder();
        let mut buf = vec![0u8; MAX_OPUS_PACKET];
        let mut pcm = Vec::new();
        let mut total = 0;
        for i in 0..50u16 {
            let frame: Vec<i16> =
                (0..FRAME_SAMPLES).map(|j| (((i as usize * FRAME_SAMPLES + j) as f32 * 0.12).sin() * 9000.0) as i16).collect();
            let n = enc.encode(&frame, &mut buf).unwrap();
            total += n;
            let sealed = ea.lock().unwrap().encrypt_frame(&buf[..n]);
            if i == 20 || i == 21 {
                continue; // 模拟丢两个包
            }
            // 服务器篡改外层序号也没用：解码器用的是端到端认证过的计数器
            on_server_voice(&udp::server_voice(5, 999, &sealed), &state, &mixer, &mut pcm);
            on_server_voice(&udp::server_voice(5, 999, &sealed), &state, &mixer, &mut pcm); // 重放被拒
        }
        assert!(total / 50 < 80, "opus frames too big: {}", total / 50);
        assert_eq!(state.decoders.lock().unwrap()[&5].last_seq, Some(49));
        assert!(state.is_speaking(5));
        // 没有密钥（比如服务器）解不开
        let other = VoiceState::default();
        let (_, stranger) = e2e_pair(7, 8);
        *other.e2e.lock().unwrap() = Some(stranger);
        let n = enc.encode(&vec![0; FRAME_SAMPLES], &mut buf).unwrap();
        let sealed = ea.lock().unwrap().encrypt_frame(&buf[..n]);
        on_server_voice(&udp::server_voice(5, 1, &sealed), &other, &mixer, &mut pcm);
        assert!(!other.is_speaking(5));
    }

    #[test]
    fn local_candidates_are_valid_addrs() {
        for a in local_candidates(4242, true) {
            let s: SocketAddr = a.parse().unwrap();
            assert_eq!(s.port(), 4242);
            assert!(!s.ip().is_loopback());
        }
    }

    /// 两个真实 Link 在本机互相打洞、确认直连、直连发端到端加密语音（信令由测试手动转交，不需要服务器）
    #[test]
    fn two_real_links_punch_and_talk() {
        use std::sync::mpsc;
        let keys = SessionKeys {
            tcp_c2s: [1; 32],
            tcp_s2c: [2; 32],
            udp_c2s: [3; 32],
            udp_s2c: [4; 32],
            udp_c2s_hp: [5; 32],
            udp_s2c_hp: [6; 32],
        };
        let (ea, eb) = e2e_pair(3, 7);
        let dummy_server: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let mk = |id: u32, e2e: Arc<Mutex<E2e>>| {
            let (tx, rx) = mpsc::channel::<Outgoing>();
            let st = Arc::new(VoiceState::default());
            let mx = Arc::new(Mutex::new(Mixer::default()));
            connect(dummy_server, 0, &keys, id, true, e2e, tx, st.clone(), mx).unwrap();
            let link = st.current_link().unwrap();
            (link, st, rx)
        };
        let (la, sa, rxa) = mk(3, ea.clone());
        let (lb, sb, _rxb) = mk(7, eb);
        let addr = |l: &Link| format!("127.0.0.1:{}", l.sock.local_addr().unwrap().port());
        la.set_others(vec![7]);
        lb.set_others(vec![3]);
        la.on_peers(vec![PeerInfo { id: 7, addrs: vec![addr(&lb)], nat: None }]);
        lb.on_peers(vec![PeerInfo { id: 3, addrs: vec![addr(&la)], nat: None }]);

        let t = Instant::now();
        while !(la.direct_peers().contains_key(&7) && lb.direct_peers().contains_key(&3)) {
            assert!(t.elapsed() < Duration::from_secs(5), "hole punching did not complete");
            std::thread::sleep(Duration::from_millis(50));
        }
        let t = Instant::now();
        loop {
            assert!(t.elapsed() < Duration::from_secs(2));
            if let Ok(Outgoing::Msg(ClientMsg::DirectPeers { ids, .. })) = rxa.recv_timeout(Duration::from_millis(500)) {
                if ids == vec![7] {
                    break;
                }
            }
        }

        let mut enc = new_encoder();
        let mut buf = vec![0u8; MAX_OPUS_PACKET];
        let pcm: Vec<i16> = (0..FRAME_SAMPLES).map(|i| ((i as f32 * 0.2).sin() * 8000.0) as i16).collect();
        let n = enc.encode(&pcm, &mut buf).unwrap();
        let sealed = ea.lock().unwrap().encrypt_frame(&buf[..n]);
        let mut scratch = Vec::new();
        la.send_voice(&sealed, &mut scratch);
        let t = Instant::now();
        while !sb.is_speaking(3) {
            assert!(t.elapsed() < Duration::from_secs(2), "direct voice not received");
            std::thread::sleep(Duration::from_millis(20));
        }
        while let Ok(o) = rxa.try_recv() {
            assert!(!matches!(o, Outgoing::Voice(_)), "voice must not go via server when all peers are direct");
        }
        sa.disconnect();
        sb.disconnect();
    }

    #[test]
    fn relay_capacity_from_budget() {
        // 15 Mbps，5 人全部直连语音：用掉 0.3 Mbps，剩下的足够 → 封顶 8 路
        assert_eq!(relay_slots(15_000_000, 5, 5, 0, 0, 0), 8);
        // 0.5 Mbps，5 人直连 = 0.3 Mbps，剩 0.2 → 3 路
        assert_eq!(relay_slots(500_000, 5, 5, 0, 0, 0), 3);
        // 有人没直连，还要算给服务器的一份：0.36 → 剩 0.14 → 2 路
        assert_eq!(relay_slots(500_000, 5, 9, 0, 0, 0), 2);
        // 正在共享 1080p60 给 2 个直连观看者：预算用光了
        assert_eq!(relay_slots(12_000_000, 5, 5, 5_500_000, 2, 2), 2); // 剩 0.15 Mbps
        assert_eq!(relay_slots(11_800_000, 5, 5, 5_500_000, 2, 2), 0);
    }

    /// 三个真实 Link：A(3)、R(5)、B(7)。A–R、R–B 打通直连，A 和 B 互相不知道地址（模拟打不通）。
    /// 服务器安排 A→B 经 R：B 要能听到 A，而且 A 不再把语音发给服务器。
    #[test]
    fn member_relay_delivers_voice() {
        use std::sync::mpsc;
        let keys = SessionKeys {
            tcp_c2s: [1; 32],
            tcp_s2c: [2; 32],
            udp_c2s: [3; 32],
            udp_s2c: [4; 32],
            udp_c2s_hp: [5; 32],
            udp_s2c_hp: [6; 32],
        };
        // 三方端到端密钥
        let ids = [3u32, 5, 7];
        let mut es: Vec<E2e> = ids.iter().map(|&i| E2e::new(i, &format!("user{i}"), Identity::generate(), HashMap::new())).collect();
        crate::e2e::tests::route_all(&mut es);
        for (k, e) in es.iter_mut().enumerate() {
            e.set_channel_others(ids.iter().copied().filter(|&x| x != ids[k]).collect());
        }
        crate::e2e::tests::route_all(&mut es);
        for e in &es {
            for &o in ids.iter().filter(|&&o| o != e.my_id) {
                assert!(e.pair_keys(o).is_some(), "{} has no pair key with {o}", e.my_id);
            }
        }
        let es: Vec<Arc<Mutex<E2e>>> = es.into_iter().map(|e| Arc::new(Mutex::new(e))).collect();

        let dummy_server: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let mk = |id: u32, e2e: Arc<Mutex<E2e>>| {
            let (tx, rx) = mpsc::channel::<Outgoing>();
            let st = Arc::new(VoiceState::default());
            let mx = Arc::new(Mutex::new(Mixer::default()));
            connect(dummy_server, 0, &keys, id, true, e2e, tx, st.clone(), mx).unwrap();
            (st.current_link().unwrap(), st, rx)
        };
        let (la, sa, rxa) = mk(3, es[0].clone());
        let (lr, sr, _rxr) = mk(5, es[1].clone());
        let (lb, sb, _rxb) = mk(7, es[2].clone());
        let addr = |l: &Link| format!("127.0.0.1:{}", l.sock.local_addr().unwrap().port());
        for l in [&la, &lr, &lb] {
            let me = l.my_id;
            l.set_others(ids.iter().copied().filter(|&x| x != me).collect());
        }
        // A 只认识 R；B 只认识 R；R 认识两人
        la.on_peers(vec![PeerInfo { id: 5, addrs: vec![addr(&lr)], nat: None }]);
        lb.on_peers(vec![PeerInfo { id: 5, addrs: vec![addr(&lr)], nat: None }]);
        lr.on_peers(vec![PeerInfo { id: 3, addrs: vec![addr(&la)], nat: None }, PeerInfo { id: 7, addrs: vec![addr(&lb)], nat: None }]);
        let t = Instant::now();
        while !(la.direct_peers().contains_key(&5) && lb.direct_peers().contains_key(&5) && lr.direct_peers().len() == 2) {
            assert!(t.elapsed() < Duration::from_secs(5), "hole punching did not complete");
            std::thread::sleep(Duration::from_millis(50));
        }
        // 等 A 的分配把 R 列入直连
        let t = Instant::now();
        while !la.allocation().0.contains(&5) {
            assert!(t.elapsed() < Duration::from_secs(2));
            std::thread::sleep(Duration::from_millis(20));
        }
        // 服务器下发的路线（双向都经 R）
        let routes = vec![
            RelayRoute { from: 3, to: 7, via: 5, ms: 20, server_ms: 120 },
            RelayRoute { from: 7, to: 3, via: 5, ms: 20, server_ms: 120 },
        ];
        for l in [&la, &lr, &lb] {
            l.set_relays(routes.clone());
        }
        while rxa.try_recv().is_ok() {}

        let mut enc = new_encoder();
        let mut buf = vec![0u8; MAX_OPUS_PACKET];
        let pcm: Vec<i16> = (0..FRAME_SAMPLES).map(|i| ((i as f32 * 0.2).sin() * 8000.0) as i16).collect();
        let n = enc.encode(&pcm, &mut buf).unwrap();
        let sealed = es[0].lock().unwrap().encrypt_frame(&buf[..n]);
        let mut scratch = Vec::new();
        la.send_voice(&sealed, &mut scratch);
        let t = Instant::now();
        while !sb.is_speaking(3) {
            assert!(t.elapsed() < Duration::from_secs(2), "B did not hear A through the relay");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(sr.is_speaking(3), "the relay also hears A (it is a channel member)");
        while let Ok(o) = rxa.try_recv() {
            assert!(!matches!(o, Outgoing::Voice(_)), "A must not send voice to the server when everyone is covered");
        }

        // 中继关掉后：R 不再转发
        lr.set_relay_enabled(false);
        std::thread::sleep(Duration::from_millis(300));
        let sealed = es[0].lock().unwrap().encrypt_frame(&buf[..n]);
        sb.last_heard.lock().unwrap().clear();
        la.send_voice(&sealed, &mut scratch);
        std::thread::sleep(Duration::from_millis(400));
        assert!(!sb.is_speaking(3), "relay turned off must not forward");

        // R 不转发「别人转来的」包：B 的语音经 R 到 A 是可以的，但 R 不会把 A 发来的、声称来自 B 的包转出去
        lr.set_relay_enabled(true);
        let forged = udp::server_voice(7, 1, &sealed); // A 冒充 B 的包
        let mut out = Vec::new();
        let before = sb.last_heard.lock().unwrap().len();
        lr.relay_forward(3, &forged, &mut out);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(sb.last_heard.lock().unwrap().len(), before);

        sa.disconnect();
        sr.disconnect();
        sb.disconnect();
    }

    fn test_keys() -> SessionKeys {
        SessionKeys { tcp_c2s: [1; 32], tcp_s2c: [2; 32], udp_c2s: [3; 32], udp_s2c: [4; 32], udp_c2s_hp: [5; 32], udp_s2c_hp: [6; 32] }
    }

    /// 直连对端突然没响应：1.2 秒左右就不再直连发给他（改走服务器），不等 4 秒判定断开
    #[test]
    fn silent_peer_fails_over_quickly() {
        use std::sync::mpsc;
        let keys = test_keys();
        let (ea, eb) = e2e_pair(3, 7);
        let dummy_server: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let mk = |id: u32, e2e: Arc<Mutex<E2e>>| {
            let (tx, rx) = mpsc::channel::<Outgoing>();
            let st = Arc::new(VoiceState::default());
            let mx = Arc::new(Mutex::new(Mixer::default()));
            connect(dummy_server, 0, &keys, id, true, e2e, tx, st.clone(), mx).unwrap();
            (st.current_link().unwrap(), st, rx)
        };
        let (la, sa, _rxa) = mk(3, ea);
        let (lb, sb, _rxb) = mk(7, eb);
        let addr = |l: &Link| format!("127.0.0.1:{}", l.sock.local_addr().unwrap().port());
        la.set_others(vec![7]);
        lb.set_others(vec![3]);
        la.on_peers(vec![PeerInfo { id: 7, addrs: vec![addr(&lb)], nat: None }]);
        lb.on_peers(vec![PeerInfo { id: 3, addrs: vec![addr(&la)], nat: None }]);
        let t = Instant::now();
        while !la.allocation().0.contains(&7) {
            assert!(t.elapsed() < Duration::from_secs(5), "never went direct");
            std::thread::sleep(Duration::from_millis(20));
        }
        // B 掉线（不再回 ping）
        sb.disconnect();
        let t = Instant::now();
        while la.allocation().0.contains(&7) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let took = t.elapsed();
        println!("{took:?} 后不再直连");
        assert!(took < Duration::from_millis(1800), "failover took {took:?}");
        sa.disconnect();
    }

    /// 屏幕画面匀速发出：一个大关键帧（100 个包）不会一下子全涌出去
    #[test]
    fn video_is_paced() {
        use std::sync::mpsc;
        let keys = test_keys();
        let (ea, _) = e2e_pair(3, 7);
        let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
        sink.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let (tx, _rx) = mpsc::channel::<Outgoing>();
        let st = Arc::new(VoiceState::default());
        connect(sink.local_addr().unwrap(), 0, &keys, 3, false, ea, tx, st.clone(), Arc::new(Mutex::new(Mixer::default()))).unwrap();
        let l = st.current_link().unwrap();
        l.udp_ok.store(true, Ordering::Relaxed);
        l.set_watchers(vec![7], vec![]);
        let frags: Vec<Vec<u8>> = (0..100).map(|_| vec![0u8; 1150]).collect();
        let mut buf = Vec::new();
        let t = Instant::now();
        l.send_video(&frags, &mut buf, false);
        let mut got = 0;
        let mut b = [0u8; 2048];
        while got < 100 {
            let (n, _) = sink.recv_from(&mut b).expect("video packet");
            if n > 1000 {
                got += 1;
            }
        }
        let took = t.elapsed();
        println!("100 个画面包用了 {took:?}");
        // 4 Mbps 下约 120 KB 要 ~230 ms；至少要明显摊开
        assert!(took > Duration::from_millis(150), "not paced: {took:?}");
        st.disconnect();
    }

    /// 两个真实 Link 直连：A 发伴奏帧，B 解密解码进「A 的伴奏」混音通道；语音的帧序号不受影响
    fn music_pair() -> (Arc<Link>, Arc<VoiceState>, Arc<Link>, Arc<VoiceState>, Arc<Mutex<Mixer>>, Arc<Mutex<E2e>>) {
        use std::sync::mpsc;
        let keys = test_keys();
        let (ea, eb) = e2e_pair(3, 7);
        let dummy_server: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let (tx, _rx) = mpsc::channel::<Outgoing>();
        let sa = Arc::new(VoiceState::default());
        connect(dummy_server, 0, &keys, 3, true, ea.clone(), tx, sa.clone(), Arc::new(Mutex::new(Mixer::default()))).unwrap();
        let (tx, _rx2) = mpsc::channel::<Outgoing>();
        let sb = Arc::new(VoiceState::default());
        let mb = Arc::new(Mutex::new(Mixer::default()));
        connect(dummy_server, 0, &keys, 7, true, eb, tx, sb.clone(), mb.clone()).unwrap();
        let (la, lb) = (sa.current_link().unwrap(), sb.current_link().unwrap());
        let addr = |l: &Link| format!("127.0.0.1:{}", l.sock.local_addr().unwrap().port());
        la.set_others(vec![7]);
        lb.set_others(vec![3]);
        la.on_peers(vec![PeerInfo { id: 7, addrs: vec![addr(&lb)], nat: None }]);
        lb.on_peers(vec![PeerInfo { id: 3, addrs: vec![addr(&la)], nat: None }]);
        let t = Instant::now();
        while !la.allocation().0.contains(&7) {
            assert!(t.elapsed() < Duration::from_secs(5), "never went direct");
            std::thread::sleep(Duration::from_millis(20));
        }
        la.in_channel.store(true, Ordering::Relaxed);
        (la, sa, lb, sb, mb, ea)
    }

    fn music_buffered(mb: &Mutex<Mixer>, from: u32) -> bool {
        mb.lock().unwrap().buffer_ms(crate::audio::stream_mixer_id(udp::MUSIC, from)).is_some()
    }

    /// 手动线路：走服务器 → 不再直连、不打洞；改回自动重新打通；强制直连时不稳也照样直连
    #[test]
    fn manual_route_prefs() {
        let wait = |what: &str, f: &dyn Fn() -> bool| {
            let t = Instant::now();
            while !f() {
                assert!(t.elapsed() < Duration::from_secs(5), "{what}");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let (la, sa, _lb, sb, _mb, _ea) = music_pair();
        la.set_route_prefs(HashMap::from([(7, RoutePref::Server)]));
        wait("server route drops direct", &|| !la.allocation().0.contains(&7));
        assert!(!la.punch_states().contains_key(&7), "no punching with a server-only peer");
        assert_eq!(la.route_pref(7), RoutePref::Server);
        la.set_route_prefs(HashMap::new());
        wait("auto punches again", &|| la.allocation().0.contains(&7));
        // 直连被判为不稳：自动时改走服务器，强制直连时照样直连
        la.peers.lock().unwrap().map.get_mut(&7).unwrap().unstable_until = Some(Instant::now() + Duration::from_secs(600));
        wait("unstable falls back", &|| !la.allocation().0.contains(&7));
        la.set_route_prefs(HashMap::from([(7, RoutePref::Direct)]));
        wait("forced direct ignores instability", &|| la.allocation().0.contains(&7));
        sa.disconnect();
        sb.disconnect();
    }

    #[test]
    fn music_stream_reaches_peer_mixer() {
        let (la, sa, _lb, sb, mb, ea) = music_pair();
        let mut enc = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Audio).unwrap();
        let mut out = vec![0u8; 1500];
        let mut pkt = Vec::new();
        for i in 0..10 {
            let pcm: Vec<i16> = (0..FRAME_SAMPLES).map(|j| (((i * FRAME_SAMPLES + j) as f32 * 0.05).sin() * 8000.0) as i16).collect();
            let n = enc.encode(&pcm, &mut out).unwrap();
            let blob = ea.lock().unwrap().encrypt_stream(voc_proto::e2e::STREAM_MUSIC, &out[..n]);
            la.send_stream(udp::MUSIC, &blob, &mut pkt);
        }
        let t = Instant::now();
        while !music_buffered(&mb, 3) {
            assert!(t.elapsed() < Duration::from_secs(2), "music did not reach B's mixer");
            std::thread::sleep(Duration::from_millis(20));
        }
        // 伴奏不算说话（不亮绿圈），语音解码器不受影响
        assert!(!sb.is_speaking(3));
        assert!(sb.decoders.lock().unwrap().get(&3).is_none());
        sa.disconnect();
        sb.disconnect();
    }

    /// 真机：PowerShell 播一段声音，A 选它当伴奏 → B 的混音里出现 A 的伴奏。
    /// cargo test -p voc-client music_from_real_program -- --ignored --nocapture
    #[test]
    #[ignore]
    fn music_from_real_program() {
        let (_la, sa, _lb, sb, mb, _ea) = music_pair();
        let player = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", "[console]::beep(440, 4000)"])
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(800));
        let gain = Arc::new(crate::audio::AtomicF32::new(1.0));
        let src = crate::streams::start(udp::MUSIC, crate::loopback::Target::Include(player.id()), sa.clone(), gain).expect("start music");
        let t = Instant::now();
        while !music_buffered(&mb, 3) {
            assert!(t.elapsed() < Duration::from_secs(4), "program audio did not reach B");
            std::thread::sleep(Duration::from_millis(50));
        }
        println!("B 收到 A 的伴奏，缓冲 {:?} ms", mb.lock().unwrap().buffer_ms(crate::audio::stream_mixer_id(udp::MUSIC, 3)));
        drop(src);
        sa.disconnect();
        sb.disconnect();
    }

    /// 真实屏幕采集 → 720p H.264 → 端到端加密 → 两个真实 Link P2P 直连 → 解码。
    /// 需要桌面环境，默认不跑：cargo test -p voc-client live_screen_share -- --ignored
    /// VOC_LIVE_LOW=1：同时有人看原画（#99，走服务器）和低画质层（#7，直连），验证同一帧编两层；
    /// 再加 VOC_LIVE_ONLY_LOW=1 只有低画质层；VOC_LIVE_99_CAPS=1 让 #99 也能解 AV1 / HEVC。
    /// 注意：这里没有服务器帮忙转发「要关键帧」，H.264 的大关键帧被匀速队列丢掉时要等 10 秒的兜底关键帧
    #[test]
    #[ignore]
    fn live_screen_share_over_p2p() {
        use crate::video::{ShareConfig, Sharer};
        use std::sync::mpsc;
        let keys = SessionKeys {
            tcp_c2s: [1; 32],
            tcp_s2c: [2; 32],
            udp_c2s: [3; 32],
            udp_s2c: [4; 32],
            udp_c2s_hp: [5; 32],
            udp_s2c_hp: [6; 32],
        };
        let (ea, eb) = e2e_pair(3, 7);
        let dummy_server: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let mk = |id: u32, e2e: Arc<Mutex<E2e>>| {
            let (tx, rx) = mpsc::channel::<Outgoing>();
            let st = Arc::new(VoiceState::default());
            let mx = Arc::new(Mutex::new(Mixer::default()));
            connect(dummy_server, 0, &keys, id, true, e2e, tx, st.clone(), mx).unwrap();
            let link = st.current_link().unwrap();
            (link, st, rx)
        };
        let (la, sa, _rxa) = mk(3, ea);
        let (lb, sb, _rxb) = mk(7, eb);
        let addr = |l: &Link| format!("127.0.0.1:{}", l.sock.local_addr().unwrap().port());
        la.set_others(vec![7]);
        lb.set_others(vec![3]);
        la.on_peers(vec![PeerInfo { id: 7, addrs: vec![addr(&lb)], nat: None }]);
        lb.on_peers(vec![PeerInfo { id: 3, addrs: vec![addr(&la)], nat: None }]);
        let t = Instant::now();
        while !(la.direct_peers().contains_key(&7) && lb.direct_peers().contains_key(&3)) {
            assert!(t.elapsed() < Duration::from_secs(5), "hole punching did not complete");
            std::thread::sleep(Duration::from_millis(50));
        }
        let view = sb.video.watch(3, true).unwrap();
        // 观看者上报的解码能力（真实客户端由服务器转来）
        sa.caps.lock().unwrap().insert(7, crate::video::local_decoders().iter().map(|c| c.key().to_string()).collect());
        if std::env::var_os("VOC_LIVE_99_CAPS").is_some() {
            sa.caps.lock().unwrap().insert(99, crate::video::local_decoders().iter().map(|c| c.key().to_string()).collect());
        }
        let low = std::env::var("VOC_LIVE_LOW").is_ok();
        if std::env::var_os("VOC_LIVE_ONLY_LOW").is_some() {
            la.set_watchers(vec![7], vec![7]);
        } else if low {
            la.set_watchers(vec![7, 99], vec![7]);
        } else {
            la.set_watchers(vec![7], vec![]);
        }
        let h: u32 = std::env::var("VOC_LIVE_HEIGHT").ok().and_then(|v| v.parse().ok()).unwrap_or(720);
        let secs: u64 = std::env::var("VOC_LIVE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        let sharer = Sharer::start(ShareConfig { height: h, fps: 60, encoder: std::env::var("VOC_LIVE_ENCODER").unwrap_or_else(|_| "auto".into()), ..Default::default() }, sa.clone()).unwrap();
        // 开始共享后画面才计入直连分配，第一个关键帧走了（假的）服务器；分到直连后像真实观看者那样要一次关键帧
        let t = Instant::now();
        while !la.allocation().1.contains(&7) {
            assert!(t.elapsed() < Duration::from_secs(5), "video never allocated to the direct path");
            std::thread::sleep(Duration::from_millis(20));
        }
        sharer.request_keyframe();
        let t = Instant::now();
        loop {
            if let Some(f) = view.frame.lock().unwrap().as_ref() {
                if f.seq >= 30 && t.elapsed() >= Duration::from_secs(secs) {
                    println!(
                        "[{} | {}] -> [{}] decoded {}x{} frames: {}, viewer fps: {}, capture fps: {}, sender fps: {}, kbps: {}, encode_ms: {}, skipped/s: {}, elapsed {:?}, err {:?}",
                        sharer.stats.encoder.lock().unwrap(),
                        sharer.stats.pipeline.lock().unwrap(),
                        view.decoder.lock().unwrap(),
                        f.width,
                        f.height,
                        f.seq,
                        view.fps.load(Ordering::Relaxed),
                        sharer.stats.capture_fps.load(Ordering::Relaxed),
                        sharer.stats.fps.load(Ordering::Relaxed),
                        sharer.stats.kbps.load(Ordering::Relaxed),
                        sharer.stats.encode_ms.load(Ordering::Relaxed),
                        sharer.stats.skipped.load(Ordering::Relaxed),
                        t.elapsed(),
                        sharer.stats.error.lock().unwrap()
                    );
                    let n = (f.rgba.len() / 4).max(1);
                    let avg = |c: usize| f.rgba.chunks(4).map(|p| p[c] as u64).sum::<u64>() / n as u64;
                    println!("平均颜色 R {} G {} B {}", avg(0), avg(1), avg(2));
                    assert_eq!(f.height as u32, if low { 360 } else { h });
                    break;
                }
            }
            assert!(
                t.elapsed() < Duration::from_secs(15 + secs),
                "no video decoded via P2P: capture fps {}, sender fps {}, kbps {}, encoder {:?}, pipeline {:?}, err {:?}, decoder {:?}",
                sharer.stats.capture_fps.load(Ordering::Relaxed),
                sharer.stats.fps.load(Ordering::Relaxed),
                sharer.stats.kbps.load(Ordering::Relaxed),
                sharer.stats.encoder.lock().unwrap(),
                sharer.stats.pipeline.lock().unwrap(),
                sharer.stats.error.lock().unwrap(),
                view.decoder.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(sharer);
        sa.disconnect();
        sb.disconnect();
    }
}
