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
/// 屏幕画面匀速发送（第 36-4 项）：队列里的量争取在一帧的时间内发完，速度在画面码率的 1.5～3 倍之间（至少 4 Mbps），最多连发 4 个包。
/// 普通帧摊到整帧时间里慢慢发（不再一下子冲出去），关键帧这种大帧最多按 3 倍发
const PACE_FACTOR: f64 = 3.0;
const PACE_FACTOR_LOW: f64 = 1.5;
const PACE_MIN_BPS: f64 = 4_000_000.0;
/// 正在共享的帧率（匀速发送按它算一帧的时间）
pub static VIDEO_FPS: AtomicU32 = AtomicU32::new(30);

/// 匀速发送的速度（bps）：video_bps = 一路画面码率，fec_pct = 冗余比例，dests = 正在发的目标数，queued = 队列里的字节数
pub fn pace_rate_bps(video_bps: u32, fec_pct: u32, dests: usize, queued: usize, fps: u32) -> f64 {
    // 冗余也要发：冗余 200% 时实际是码率的 3 倍（不算进去的话一加冗余队列就积压，又被当成拥塞把冗余压下去）
    let per = video_bps as f64 * (100 + fec_pct) as f64 / 100.0 * dests.max(1) as f64;
    let frame_secs = 1.0 / fps.clamp(5, 240) as f64;
    let want = queued as f64 * 8.0 / (frame_secs * 0.9);
    want.clamp(per * PACE_FACTOR_LOW, per * PACE_FACTOR).max(PACE_MIN_BPS)
}
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

/// 两帧的分片交替排（第 34-5 项）：a0 b0 a1 b1 …，多出来的接在后面
fn interleave_frags(a: &[Vec<u8>], b: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    for i in 0..a.len().max(b.len()) {
        if let Some(x) = a.get(i) {
            out.push(x.clone());
        }
        if let Some(x) = b.get(i) {
            out.push(x.clone());
        }
    }
    out
}

/// 探测线路能走多大的包（第 37-4 项）：明文大小，加密后再加约 40～60 字节。以太网一般能走 1472 字节的 UDP，VPN 常常小一些
const MTU_PROBE_SIZES: [usize; 7] = [1100, 1180, 1240, 1300, 1360, 1400, 1440];

/// 临时打开「不许分片」（DF），把 pkts 发出去，再关掉。调用方要保证这期间没有别的线程用这个套接字发包
fn send_dont_fragment(sock: &UdpSocket, dual: bool, pkts: &[(Vec<u8>, SocketAddr)]) {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows::Win32::Networking::WinSock::{setsockopt, IPPROTO_IP, IPPROTO_IPV6, IPV6_DONTFRAG, IP_DONTFRAGMENT, SOCKET};
        let s = SOCKET(sock.as_raw_socket() as usize);
        let set = |on: u32| unsafe {
            let v = on.to_ne_bytes();
            let _ = setsockopt(s, IPPROTO_IP.0, IP_DONTFRAGMENT, Some(&v));
            if dual {
                let _ = setsockopt(s, IPPROTO_IPV6.0, IPV6_DONTFRAG, Some(&v));
            }
        };
        set(1);
        for (p, to) in pkts {
            let _ = sock.send_to(p, wire(*to, dual));
        }
        set(0);
    }
    #[cfg(not(windows))]
    {
        let _ = (sock, dual, pkts);
    }
}

/// 低 TTL 先发（第 35-2 项）用的 TTL：只够走到自己的路由器 / 运营商 NAT，到不了对方。
/// 先在自己这边的 NAT 上开好口子，对方的包先到也不会被当成「不请自来」拉黑映射
const LOW_TTLS: [u32; 4] = [2, 3, 4, 5];

/// 临时把套接字的 TTL 调低，按每个 TTL 把 pkts 各发一遍，再改回原来的值。调用方要保证这期间没有别的线程用这个套接字发包
fn send_low_ttl(sock: &UdpSocket, dual: bool, pkts: &[(Vec<u8>, SocketAddr)], ttls: &[u32]) -> std::io::Result<()> {
    let sr = socket2::SockRef::from(sock);
    let old4 = sr.ttl_v4().ok();
    let old6 = if dual { sr.unicast_hops_v6().ok() } else { None };
    let set = |t: u32| -> std::io::Result<()> {
        sr.set_ttl_v4(t)?;
        if dual {
            sr.set_unicast_hops_v6(t)?;
        }
        Ok(())
    };
    let mut r = Ok(());
    for &t in ttls {
        if let Err(e) = set(t) {
            r = Err(e);
            break;
        }
        for (p, to) in pkts {
            let _ = sock.send_to(p, wire(*to, dual));
        }
    }
    if let Some(o) = old4 {
        let _ = sr.set_ttl_v4(o);
    }
    if let Some(o) = old6 {
        let _ = sr.set_unicast_hops_v6(o);
    }
    r
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

/// 0.6 起：对某个队友，语音和画面的线路分开选，另有「完全不透露 IP」
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PeerRoute {
    /// 语音线路（两个方向都按这个：我说给他、他说给我）
    pub voice: VoiceRoute,
    /// 画面线路（我共享给他看、我看他的共享）
    pub video: RoutePref,
    /// 完全不透露 IP：不和他打洞，语音、画面都不直连（可以经队友中转语音：中转的人看不到双方 IP 以外的东西，你们俩互相看不到 IP）
    pub hide_ip: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum VoiceRoute {
    #[default]
    Auto,
    /// 强制直连：打通了就一直直连；打不通时仍走服务器
    Direct,
    /// 走服务器：语音不直连、也不经队友中转（画面可以照常直连）
    Server,
    /// 经某个队友中转（按昵称记，两个方向都经他）；他不在 / 不能中转时退回服务器
    Via(String),
}

impl PeerRoute {
    /// 0.5.x 只有一个选择：「走服务器」= 不打洞、不透露 IP；「强制直连」= 语音和画面都强制直连
    pub fn from_old(r: RoutePref) -> Self {
        match r {
            RoutePref::Auto => Self::default(),
            RoutePref::Direct => Self { voice: VoiceRoute::Direct, video: RoutePref::Direct, hide_ip: false },
            RoutePref::Server => Self { hide_ip: true, ..Default::default() },
        }
    }

    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// 简短说明（成员名字旁、诊断里用）
    pub fn label(&self) -> String {
        if self.hide_ip {
            return "不透露 IP".into();
        }
        let v = match &self.voice {
            VoiceRoute::Auto => None,
            VoiceRoute::Direct => Some("语音强制直连".to_string()),
            VoiceRoute::Server => Some("语音走服务器".to_string()),
            VoiceRoute::Via(n) => Some(format!("语音经 {n}")),
        };
        let d = match self.video {
            RoutePref::Auto => None,
            RoutePref::Direct => Some("画面强制直连".to_string()),
            RoutePref::Server => Some("画面走服务器".to_string()),
        };
        [v, d].into_iter().flatten().collect::<Vec<_>>().join(" · ")
    }
}

/// 链路层用的线路选择（昵称已换成编号）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeerPref {
    pub voice_direct: bool,
    pub voice_server: bool,
    /// 经这个人中转语音（服务器安排；不行就走服务器）
    pub voice_via: Option<u32>,
    pub video_direct: bool,
    pub video_server: bool,
    pub hide_ip: bool,
}

impl PeerPref {
    pub fn from_route(r: &PeerRoute, id_of: impl Fn(&str) -> Option<u32>) -> Self {
        Self {
            voice_direct: r.voice == VoiceRoute::Direct,
            voice_server: r.voice == VoiceRoute::Server,
            voice_via: match &r.voice {
                VoiceRoute::Via(n) => id_of(n),
                _ => None,
            },
            video_direct: r.video == RoutePref::Direct,
            video_server: r.video == RoutePref::Server,
            hide_ip: r.hide_ip,
        }
    }

    /// 诊断用的简短说明（只写编号）
    pub fn label(&self) -> String {
        if self.hide_ip {
            return "不透露 IP".into();
        }
        let mut v = Vec::new();
        if self.voice_direct {
            v.push("语音强制直连".to_string());
        }
        if self.voice_server {
            v.push("语音走服务器".to_string());
        }
        if let Some(x) = self.voice_via {
            v.push(format!("语音经 #{x}"));
        }
        if self.video_direct {
            v.push("画面强制直连".to_string());
        }
        if self.video_server {
            v.push("画面走服务器".to_string());
        }
        if v.is_empty() {
            "自动".into()
        } else {
            v.join(" · ")
        }
    }

    /// 语音不能和他直连（走服务器 / 不透露 IP / 指定了中转）
    fn voice_not_direct(&self) -> bool {
        self.voice_server || self.hide_ip || self.voice_via.is_some()
    }

    /// 画面不能和他直连
    fn video_not_direct(&self) -> bool {
        self.video_server || self.hide_ip
    }
}

/// 直连保活 / 测量间隔
const PEER_PING_INTERVAL: Duration = Duration::from_millis(500);
/// 不在同一个频道、预先打好洞备用的人：保活间隔（够 NAT 映射不过期）和断线判定
const IDLE_PING_INTERVAL: Duration = Duration::from_secs(15);
const IDLE_PEER_DEAD: Duration = Duration::from_secs(50);
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
    /// 这一轮打洞（按开始时间）已经低 TTL 先发过了（第 35-2 项）
    low_ttl_for: Option<Instant>,
    punch_round: u32,
    rtt_ms: u32,
    ping_seq: u32,
    pings: VecDeque<PingLog>,
    unstable_until: Option<Instant>,
    good_since: Option<Instant>,
    quality: PeerQuality,
    /// 对方自测的 NAT 类型
    nat: Option<NatInfo>,
    /// 对方的身份指纹（记住上次打通的地址用）
    fp: String,
    /// 以前和他打通过的地址（加密存在本机，先试这些）
    remembered: Vec<SocketAddr>,
    /// 刚断掉的那条路（断线后先试它）和断的时间：几秒内恢复就直接用回，不进「先中转观察」
    prev_path: Option<SocketAddr>,
    dropped_at: Option<Instant>,
    /// ICE 式（第 39-1 项）：所有回过 PUNCH_ACK 的地址（验证过能通的路）→ (最近一次, 往返 ms)
    valid: HashMap<SocketAddr, (Instant, u32)>,
    /// 对方从候选以外的地址发来的打洞包（对方真实的出口，类似 ICE 的 peer-reflexive）：也往那里打
    reflexive: Vec<SocketAddr>,
    /// 上次在已连通时试更好的路
    last_upgrade: Instant,
    /// 生日攻击进行中（第 35-1 项）
    bday: Option<Birthday>,
    /// 这一轮打洞已经试过生日攻击（不重复试）
    bday_tried: bool,
}

/// 生日攻击（对方或自己是「端口随机的对称型 NAT」时）：
/// - 自己端口随机：临时多开一批本地端口，每个都朝对方发打洞包（在自己 NAT 上开出一批映射）；
/// - 对方端口随机：朝对方公网 IP 的随机端口发打洞包，撞上他的某个映射就通。
/// 按生日问题：256 个映射、发 1024 个包约 98% 撞上（约 10 秒）。
struct Birthday {
    started: Instant,
    /// 我这边多开的端口（我端口随机时）
    socks: Vec<Arc<UdpSocket>>,
    /// 还能开多少个（开不出来 = 系统 / 端口额度不够，就停在已开的数量）
    sock_quota: usize,
    /// 朝对方随机端口发过的（对方端口随机时）
    sent_ports: std::collections::HashSet<u16>,
    probe_quota: usize,
    /// 每 100 ms 开几个端口 / 发几个探测包（每秒约 100 个，不像扫描）
    per_tick: usize,
    last_resend: Instant,
}

/// 正常打洞多久还没通才开始生日攻击、最多持续多久
const BDAY_AFTER: Duration = Duration::from_secs(2);
const BDAY_FOR: Duration = Duration::from_secs(20);
/// 一边端口随机时：最多开 256 个端口 / 发 2048 个探测包；两边都随机：各一半
const BDAY_SOCKS: usize = 256;
const BDAY_PROBES: usize = 2048;
const BDAY_PER_TICK: usize = 10;

/// 挑 n 个没发过的随机端口（1024～65535）
fn bday_ports(rng: &AtomicU64, sent: &mut std::collections::HashSet<u16>, n: usize) -> Vec<u16> {
    let mut out = Vec::with_capacity(n);
    let mut x = rng.load(Ordering::Relaxed) | 1;
    let mut tries = 0;
    while out.len() < n && tries < n * 20 {
        tries += 1;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let port = 1024 + (x % (65536 - 1024)) as u16;
        if sent.insert(port) {
            out.push(port);
        }
    }
    rng.store(x, Ordering::Relaxed);
    out
}

/// 路的优先级（越小越好）：局域网 > IPv6 公网（没有 NAT）> IPv4 公网 > 其他（运营商大内网等）
fn addr_class(a: &SocketAddr) -> u8 {
    let ip = a.ip().to_canonical();
    if is_private_or_local(ip) {
        // 运营商大内网（100.64/10）不算局域网
        if let std::net::IpAddr::V4(v) = ip {
            if v.octets()[0] == 100 && (v.octets()[1] & 0xC0) == 64 {
                return 3;
            }
        }
        return 0;
    }
    if ip.is_ipv6() {
        1
    } else {
        2
    }
}

/// 收到一个地址的 PUNCH_ACK 时要不要把直连换到这条路：更高优先级的类型，或同类型但明显更快（快 30% 以上）
fn better_path(cur: Option<(SocketAddr, u32)>, new: SocketAddr, new_rtt: u32) -> bool {
    match cur {
        None => true,
        Some((c, _)) if c == new => false,
        Some((c, c_rtt)) => {
            let (cc, nc) = (addr_class(&c), addr_class(&new));
            nc < cc || (nc == cc && c_rtt > 0 && new_rtt > 0 && new_rtt * 10 < c_rtt * 7)
        }
    }
}

/// 日志里说路的类型（不写 IP）
fn addr_kind(a: &SocketAddr) -> &'static str {
    match addr_class(a) {
        0 => "局域网",
        1 => "IPv6",
        2 => "IPv4 公网",
        _ => "运营商内网",
    }
}

/// 已连通时多久试一次更好的路
const UPGRADE_CHECK: Duration = Duration::from_secs(5);

/// 打洞开头密集补打的窗口和间隔
const BURST_WINDOW: Duration = Duration::from_millis(150);
const BURST_GAP: Duration = Duration::from_millis(20);

/// 直连断了这么久以内又通了：当作短暂抖动，直接用回（不强制先走中转观察 30 秒）
const QUICK_RECOVER: Duration = Duration::from_secs(10);

/// 「端口保持」候选：很多 NAT 对外沿用本机端口。对方公网 IP + 对方局域网地址里的端口（和已有候选重复就不要）
fn port_preservation_target(cands: &[SocketAddr]) -> Option<SocketAddr> {
    let public = cands.iter().find(|a| !is_private_or_local(a.ip()))?;
    let local = cands.iter().find(|a| is_private_or_local(a.ip()) && a.is_ipv4() == public.is_ipv4())?;
    let t = SocketAddr::new(public.ip(), local.port());
    (!cands.contains(&t)).then_some(t)
}

fn is_private_or_local(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v) => v.is_private() || v.is_loopback() || v.is_link_local() || (v.octets()[0] == 100 && (v.octets()[1] & 0xC0) == 64),
        std::net::IpAddr::V6(v) => v.is_loopback() || (v.segments()[0] & 0xfe00) == 0xfc00 || (v.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// 打洞的目标：候选地址 + 当前 / 刚断的路 + 以前打通过的 + 端口保持候选（去重）
fn punch_targets(cands: &[SocketAddr], path: Option<SocketAddr>, prev: Option<SocketAddr>, remembered: &[SocketAddr]) -> Vec<SocketAddr> {
    let mut t = cands.to_vec();
    for a in path.into_iter().chain(prev).chain(remembered.iter().copied()).chain(port_preservation_target(cands)) {
        if !t.contains(&a) {
            t.push(a);
        }
    }
    t
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
            low_ttl_for: None,
            punch_round: 0,
            rtt_ms: 0,
            ping_seq: 0,
            pings: VecDeque::new(),
            unstable_until: None,
            good_since: None,
            quality: PeerQuality::default(),
            nat: None,
            fp: String::new(),
            remembered: Vec::new(),
            prev_path: None,
            dropped_at: None,
            valid: HashMap::new(),
            reflexive: Vec::new(),
            last_upgrade: past,
            bday: None,
            bday_tried: false,
        }
    }

    /// 备用地址：以前打通过的 + 对方真实出口 + 验证过能通的（断线、换路时一起试）
    fn spare(&self) -> Vec<SocketAddr> {
        let mut v = self.remembered.clone();
        v.extend(self.reflexive.iter().copied());
        v.extend(self.valid.keys().copied());
        v
    }
}

/// 有第二个 IP 的探测点时（第 39-2 项）：换了对方 IP 映射就变（「按地址分配」）的 NAT，对不同的队友来说也是对称型，
/// 只换端口看不出来。这时按主端口和第二个 IP 看到的地址判断
pub fn classify_nat3(main: SocketAddr, probe: SocketAddr, probe2: Option<SocketAddr>, lan: &[SocketAddr]) -> NatInfo {
    match probe2 {
        Some(p2) if p2 != main && lan.iter().all(|l| l.ip() != main.ip().to_canonical()) => classify_nat(main, SocketAddr::new(main.ip(), p2.port()), lan),
        _ => classify_nat(main, probe, lan),
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
    last_share_audio: Option<Vec<u32>>,
}

/// 我的屏幕共享选了「只走 P2P」：画面、补发、鼠标、共享声音都不发给服务器（伴奏、语音不受影响）
pub static SHARE_P2P_ONLY: AtomicBool = AtomicBool::new(false);
/// 语音 10 ms 一包（第 36-3 项）：用户打开了、并且同频道的人都认识（端到端 Hello 里的 voice10）
pub static VOICE_10MS: AtomicBool = AtomicBool::new(false);
/// 同频道的人都认识带冗余的语音包（第 34-3 项）
pub static VOICE_RED_OK: AtomicBool = AtomicBool::new(false);
/// 发出方向丢包 ≥ 15%：前一帧顺带再发一份；≥ 35%：前两帧
const RED_LOSS_1: u8 = 15;
const RED_LOSS_2: u8 = 35;

/// 共享声音的线路（第 54 项，共享者在设置里选）：0 自动（跟语音的线路走，直连有丢包时两条路同时发），
/// 1 总是经服务器，2 总是直连（打不通的人还是经服务器）
pub static SHARE_AUDIO_ROUTE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
pub const SHARE_AUDIO_AUTO: u8 = 0;
pub const SHARE_AUDIO_SERVER: u8 = 1;
pub const SHARE_AUDIO_DIRECT: u8 = 2;
/// 服务器支持共享声音单独的直连名单（FEATURE_SHARE_AUDIO_ROUTE）；旧服务器上共享声音只能跟画面的直连名单走
pub static SERVER_SHARE_AUDIO_ROUTE: AtomicBool = AtomicBool::new(false);
/// UDP 发送失败的次数（诊断 / 测试日志）
pub static SEND_ERRORS: AtomicU64 = AtomicU64::new(0);
/// 收包线程：处理过的包数、处理一个包最久用了多少微秒（测试日志读一次清零）
pub static RECV_PKTS: AtomicU64 = AtomicU64::new(0);
pub static SENT_PKTS: AtomicU64 = AtomicU64::new(0);
pub static RECV_MAX_US: AtomicU64 = AtomicU64::new(0);
struct RecvTimer(Instant);
impl Drop for RecvTimer {
    fn drop(&mut self) {
        RECV_MAX_US.fetch_max(self.0.elapsed().as_micros() as u64, Ordering::Relaxed);
    }
}

/// 每秒最多放行多少次（陌生地址来的包挨个试解密用）
#[derive(Default)]
struct RateLimit {
    win: Mutex<Option<(Instant, u32)>>,
}

/// 陌生地址的包每秒最多挨个试解这么多次（正常只在对端换了出口端口的头几个包用到）
const UNKNOWN_TRIALS_PER_SEC: u32 = 50;

impl RateLimit {
    fn allow(&self, now: Instant) -> bool {
        let mut w = self.win.lock().unwrap();
        match &mut *w {
            Some((start, n)) if now.duration_since(*start) < Duration::from_secs(1) => {
                *n += 1;
                *n <= UNKNOWN_TRIALS_PER_SEC
            }
            _ => {
                *w = Some((now, 1));
                true
            }
        }
    }
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
    /// 生日攻击（第 35-1 项）：对端地址 → 和他收发用的备用本地端口
    alt_socks: Mutex<HashMap<SocketAddr, Arc<UdpSocket>>>,
    alt_active: AtomicBool,
    /// 备用端口收到的包交给主接收循环
    alt_tx: Mutex<Sender<(SocketAddr, Vec<u8>)>>,
    alt_rx: Mutex<Option<std::sync::mpsc::Receiver<(SocketAddr, Vec<u8>)>>>,
    /// 生日攻击挑随机端口用
    bday_rng: AtomicU64,
    /// 陌生地址来的包挨个试解的限流
    unknown_trials: RateLimit,
    /// 因为限流没试解就丢掉的陌生地址包（测试日志 / 诊断）
    pub unknown_dropped: AtomicU32,
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
    /// 共享声音：(直连发给谁, 报给服务器「不用替我转」的人)。两者的差 = 两条路同时发的人
    share_alloc: Mutex<(Vec<u32>, Vec<u32>)>,
    /// 最近一次看到突发丢包（任何一条路 ≥ 3%）的时间（第 38-8 项）
    last_loss_burst: Mutex<Option<Instant>>,
    /// 探测线路能走多大的包（第 37-4 项）：发出去的探测 时间戳 → (对谁, 加密后大小, 发的时间)；各条线路的结果；实际发过的最大包
    mtu_probes: Mutex<HashMap<u64, (Option<u32>, u32, Instant)>>,
    path_mtu: Mutex<HashMap<Option<u32>, (u32, Instant)>>,
    max_sent: AtomicU32,
    mtu_seq: AtomicU64,
    /// 跨帧交错（第 34-5 项）：开着没有、先留着等下一帧的那一帧
    interleave: AtomicBool,
    held_video: Mutex<Option<(Vec<Vec<u8>>, Instant)>>,
    /// 上次看到的本机地址（换网络检测，第 37-2 项）
    last_local: Mutex<Vec<String>>,
    /// 低 TTL 先发时拿写锁，平时发包拿读锁（第 35-2 项）
    ttl_lock: std::sync::RwLock<()>,
    /// 网关会说 PCP 但不给 MAP：(网关, 我的内网 IP, 本地端口)；已经为哪些人试过 PEER（第 39-3 项）
    pcp_peer_gw: Mutex<Option<(std::net::Ipv4Addr, std::net::Ipv4Addr, u16)>>,
    pcp_peer_tried: Mutex<std::collections::HashSet<u32>>,
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
    pacer: Pacer,
    /// 我帮人转发过的画面包（短时间）：(时间, 共享者, 画质层, 帧号, 分片号, 包)
    relay_cache: Mutex<VecDeque<(Instant, u32, u8, u32, u16, Vec<u8>)>>,
    /// 伴奏 / 共享声音的外层序号（只给服务器去重用；端到端的计数器在密文里）
    music_seq: AtomicU16,
    share_seq: AtomicU16,
    /// 画面包在匀速队列里过期被丢掉的次数（网络 / 上行太挤的信号，码率自适应用）
    pace_dropped: AtomicU32,
    pace_dropped_sum: AtomicU32,
    /// 当前画面冗余比例（%）
    fec_pct: AtomicU32,
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
    /// 第二个 IP 上的探测点和它看到的我的地址；过滤测试的结果（Some(true) = 没联系过的地址也进得来）
    probe2: Option<SocketAddr>,
    probe2_addr: Mutex<Option<SocketAddr>>,
    pub filter_open: Mutex<Option<bool>>,
    /// 自测的 NAT 类型（测出来后告诉服务器，打洞时对方据此选策略）
    pub nat: Mutex<Option<NatInfo>>,
    /// 最近 30 次服务器 PING：(发送时间, 收到回复)，算丢包率
    srv_pings: Mutex<std::collections::VecDeque<(u64, bool)>>,
    /// 发出的字节数（UDP 含 IP/UDP 头，TCP 隧道只算语音和画面），诊断「上行是不是被占满」用
    sent_bytes: AtomicU64,
    /// 最近 30 秒每秒的上行码率（kbps）
    up_history: Mutex<std::collections::VecDeque<u32>>,
    /// 用户手动选的线路（没有的按自动）
    route_prefs: Mutex<HashMap<u32, PeerPref>>,
    /// 语音优先：服务器说这些人在我说话时没听到，先经服务器发给他们
    pub server_fallback: Mutex<std::collections::HashSet<u32>>,
    /// 最近实际听到（解出）谁的语音
    heard: Mutex<HashMap<u32, Instant>>,
    /// 最近一次发出自己的语音
    last_voice_sent: Mutex<Option<Instant>>,
    /// 经服务器收语音：发送者 → (上一包序号, 收到, 丢了)
    srv_voice_loss: Mutex<HashMap<u32, (u16, u32, u32)>>,
    /// 到服务器的往返时间记录（拥塞判断用）
    srv_rtt_hist: Mutex<VecDeque<(Instant, u32)>>,
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
/// 长距离不稳的线路（第 38-8 项）：两条路同时发的门槛降到 2%
const DUAL_VOICE_LOSS_LONG: u8 = 2;
/// 「长距离」：往返超过这么多（ms）
const LONG_HAUL_RTT_MS: u32 = 150;
/// 最近这么久内出现过突发丢包（≥ 3%）才算「不稳的长距离线路」，之后一直稳定就降回去
const LONG_HAUL_MEMORY: Duration = Duration::from_secs(180);

/// 第 38-8 项：长距离（往返 > 150 ms）且最近 3 分钟出现过突发丢包的线路，提前加强抗丢包（不等卡了才反应）
pub fn long_haul_unstable(rtt_ms: u32, last_burst: Option<Instant>, now: Instant) -> bool {
    rtt_ms > LONG_HAUL_RTT_MS && last_burst.is_some_and(|t| now.duration_since(t) < LONG_HAUL_MEMORY)
}

/// 补发用的画面历史保留多久
const VIDEO_HIST: Duration = Duration::from_millis(1000);

impl Link {
    /// 探测线路能走多大的包（第 37-4 项）：to = 服务器（seal_for = None）或某个直连的人。拿写锁打开 DF 发一组不同大小的探测包，
    /// 回来的最大那个就是这条线路能走的大小（加密后的字节数）
    fn probe_mtu(&self, to: SocketAddr, peer: Option<u32>) {
        // 编号用递增计数（同一毫秒里会同时探服务器和好几个人，用时间戳会撞号）
        let base = self.mtu_seq.fetch_add(MTU_PROBE_SIZES.len() as u64, Ordering::Relaxed);
        let mut pkts = Vec::new();
        {
            let mut sent = self.mtu_probes.lock().unwrap();
            sent.retain(|_, (_, _, t)| t.elapsed() < Duration::from_secs(10));
            let mut peers = self.peers.lock().unwrap();
            for (i, &size) in MTU_PROBE_SIZES.iter().enumerate() {
                let ts = base + i as u64;
                let plain = udp::mtu_probe_payload(ts, size);
                let mut buf = Vec::new();
                match peer {
                    None => self.sealer.lock().unwrap().seal(udp::PING, &plain, &mut buf),
                    Some(id) => {
                        let Some(s) = peers.map.get_mut(&id).and_then(|p| p.sealer.as_mut()) else { return };
                        s.seal(p2p::PING, &plain, &mut buf);
                    }
                }
                sent.insert(ts, (peer, buf.len() as u32, Instant::now()));
                pkts.push((buf, to));
            }
        }
        let _w = self.ttl_lock.write().unwrap();
        send_dont_fragment(&self.sock, self.dual, &pkts);
    }

    /// 探测包回来了：记下这条线路能走的最大包
    fn on_mtu_reply(&self, data: &[u8]) {
        let Some(ts) = data.get(..8).map(|b| u64::from_le_bytes(b.try_into().unwrap())) else { return };
        let Some((peer, len, _)) = self.mtu_probes.lock().unwrap().remove(&ts) else { return };
        let mut m = self.path_mtu.lock().unwrap();
        let e = m.entry(peer).or_insert((0, Instant::now()));
        // 新的一轮（超过 30 秒）重新算，线路变小了也能看出来
        if e.1.elapsed() > Duration::from_secs(30) {
            *e = (0, Instant::now());
        }
        e.0 = e.0.max(len);
    }

    /// 诊断：各条线路能走的最大包（字节，加密后）和 VOC 实际发过的最大包。None 的键 = 服务器
    pub fn path_mtu(&self) -> (Vec<(Option<u32>, u32)>, u32) {
        let mut v: Vec<(Option<u32>, u32)> = self.path_mtu.lock().unwrap().iter().map(|(k, (m, _))| (*k, *m)).collect();
        v.sort();
        (v, self.max_sent.load(Ordering::Relaxed))
    }

    /// 低 TTL 先发（第 35-2 项）：拿写锁，期间没有别的包从主套接字发出去（语音不会带着低 TTL 发到服务器）
    fn low_ttl_burst(&self, pkts: &[(Vec<u8>, SocketAddr)]) {
        if pkts.is_empty() {
            return;
        }
        let _w = self.ttl_lock.write().unwrap();
        if let Err(e) = send_low_ttl(&self.sock, self.dual, pkts, &LOW_TTLS) {
            crate::applog::log(format!("低 TTL 先发失败（{e}），跳过"));
        }
    }

    fn send_raw(&self, buf: &[u8], to: SocketAddr) {
        // 生日攻击打通的对端：和他改用撞上的那个本地端口收发（第 35-1 项）
        if self.alt_active.load(Ordering::Relaxed) {
            let alt = self.alt_socks.lock().unwrap().get(&to).cloned();
            if let Some(s) = alt {
                if s.send_to(buf, to).is_ok() {
                    self.sent_bytes.fetch_add(buf.len() as u64 + 28, Ordering::Relaxed);
                }
                return;
            }
        }
        let _r = self.ttl_lock.read().unwrap();
        self.max_sent.fetch_max(buf.len() as u32, Ordering::Relaxed);
        if self.sock.send_to(buf, wire(to, self.dual)).is_ok() {
            self.sent_bytes.fetch_add(buf.len() as u64 + 28, Ordering::Relaxed);
            SENT_PKTS.fetch_add(1, Ordering::Relaxed);
        } else {
            SEND_ERRORS.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// 备用端口收到一个包：记下「这个对端用这个端口」，交给主接收循环照常处理
    fn alt_received(&self, sock: &Arc<UdpSocket>, from: SocketAddr, pkt: &[u8]) {
        let from = norm(from);
        self.alt_socks.lock().unwrap().entry(from).or_insert_with(|| sock.clone());
        self.alt_active.store(true, Ordering::Relaxed);
        let _ = self.alt_tx.lock().unwrap().send((from, pkt.to_vec()));
    }

    fn send_server(&self, kind: u8, data: &[u8], buf: &mut Vec<u8>) {
        count_tx(kind, data.len());
        self.sealer.lock().unwrap().seal(kind, data, buf);
        self.send_raw(buf, self.server);
    }

    /// 直连发一个包给对端（已封好），顺带计流量
    fn send_peer(&self, kind: u8, buf: &[u8], to: SocketAddr) {
        count_tx(kind, buf.len());
        self.send_raw(buf, to);
    }

    /// 本机网络变了（第 37-2 项）：UDP 照常从新地址发（服务器按密钥认出是同一个人），
    /// 重新探测 NAT、告诉服务器新的本机地址，直连的人马上改走服务器并重新打洞（旧的直连地址已经不通了）
    fn on_network_change(&self, local: Vec<String>) {
        crate::applog::log(format!("本机网络变了（本机地址现在 {} 个）：语音先走服务器，重新探测 NAT、重新打洞", local.len()));
        *self.local_addrs.lock().unwrap() = local.clone();
        *self.public_addr.lock().unwrap() = None;
        *self.probe_addr.lock().unwrap() = None;
        *self.probe2_addr.lock().unwrap() = None;
        *self.filter_open.lock().unwrap() = None;
        *self.portmap.lock().unwrap() = crate::portmap::PortMapStatus::Trying;
        let now = Instant::now();
        for p in self.peers.lock().unwrap().map.values_mut() {
            if p.confirmed {
                p.confirmed = false;
                p.prev_path = p.path.take();
                p.dropped_at = Some(now);
            }
            p.punch_started = Some(now);
            p.low_ttl_for = None;
            p.bday_tried = false;
        }
        if self.p2p_enabled.load(Ordering::Relaxed) {
            self.tcp_send(Outgoing::Msg(ClientMsg::P2pEnable { local_addrs: local, nat: None }));
        }
    }

    /// 断线恢复后换成新的 TCP 连接（第 37-2 项）
    pub fn set_tcp(&self, tx: Sender<Outgoing>) {
        *self.tcp.lock().unwrap() = tx;
    }

    fn tcp_send(&self, o: Outgoing) {
        match &o {
            Outgoing::Voice(v) => crate::traffic::sent(crate::traffic::Cat::Voice, v.len()),
            Outgoing::Video(v) => crate::traffic::sent(crate::traffic::Cat::Share, v.len()),
            Outgoing::Stream(k, v) => count_tx(*k, v.len()),
            _ => {}
        }
        if let Outgoing::Voice(v) | Outgoing::Video(v) | Outgoing::Stream(_, v) = &o {
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
        *self.last_voice_sent.lock().unwrap() = Some(Instant::now());
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
                        self.send_peer(udp::VOICE, buf, path);
                        sent.push(*id);
                    }
                }
            }
        }
        // 经成员中继能送到的人：中继是我直连发到的人
        // （手动选了走服务器的人不算：服务器可能还没收到新选择，照样发一份给服务器）
        // 语音优先改经服务器的人也不算（服务器说他经中转听不到我）
        let prefs = self.route_prefs.lock().unwrap().clone();
        let fallback = self.server_fallback.lock().unwrap().clone();
        let relayed: Vec<u32> = self
            .relays
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.from == self.my_id && sent.contains(&r.via) && !fallback.contains(&r.to))
            .filter(|r| !prefs.get(&r.to).is_some_and(|p| p.voice_server || p.hide_ip))
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
        let mut cur = self.relays.lock().unwrap();
        if *cur != routes {
            let text = if routes.is_empty() {
                "无".to_string()
            } else {
                routes.iter().map(|r| format!("#{}→#{} 经 #{}", r.from, r.to, r.via)).collect::<Vec<_>>().join("，")
            };
            crate::applog::log(format!("线路：语音中转路线 {text}"));
        }
        *cur = routes;
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
        // 转过的包短时间留一份：我转给的观看者缺片时先找我要（就近补，第 42-1 项）
        if let Some((_, h, _)) = udp::read_server_video(payload) {
            let now = Instant::now();
            let mut c = self.relay_cache.lock().unwrap();
            while c.front().is_some_and(|e| now.duration_since(e.0) > RELAY_CACHE_TTL) || c.len() >= RELAY_CACHE_MAX {
                c.pop_front();
            }
            c.push_back((now, from, h.flags & udp::VIDEO_LOW, h.frame, h.idx, payload.to_vec()));
        }
        let mut peers = self.peers.lock().unwrap();
        for to in targets {
            let Some(p) = peers.map.get_mut(&to) else { continue };
            if let (true, Some(path), Some(s)) = (p.confirmed, p.path, p.sealer.as_mut()) {
                s.seal(udp::VIDEO, payload, buf);
                self.send_peer(udp::VIDEO, buf, path);
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
                self.send_peer(udp::VOICE, buf, path);
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
        // 跨帧交错（第 34-5 项）：丢包高时原画层两帧一组，这一帧先留着，等下一帧来了两帧的分片交替发，
        // 一串连丢分摊到两帧上，每帧丢得少、校验包更救得回来。代价是先留的那帧晚一帧发
        if !low && self.interleave.load(Ordering::Relaxed) {
            let prev = self.held_video.lock().unwrap().take();
            match prev {
                None => {
                    self.record_video(frags);
                    *self.held_video.lock().unwrap() = Some((frags.to_vec(), Instant::now()));
                    return;
                }
                Some((p, _)) => {
                    self.record_video(frags);
                    let mut merged = interleave_frags(&p, frags);
                    // 标上「交错」：接收端等这一对到齐再要补发（第 34-5 项）
                    for f in merged.iter_mut() {
                        if let Some(b) = f.get_mut(udp::VIDEO_FLAGS_AT) {
                            *b |= udp::VIDEO_INTERLEAVED;
                        }
                    }
                    self.dispatch_video(&merged, buf, low);
                    return;
                }
            }
        }
        if let Some((p, _)) = self.held_video.lock().unwrap().take() {
            self.dispatch_video(&p, buf, false);
        }
        self.record_video(frags);
        self.dispatch_video(frags, buf, low);
    }

    /// 留着等交错的那一帧太久没等到下一帧（画面不动了）：单独发出去
    pub fn flush_held_video(&self, max_age: Duration) {
        let due = self.held_video.lock().unwrap().as_ref().is_some_and(|(_, t)| t.elapsed() >= max_age);
        if due {
            if let Some((p, _)) = self.held_video.lock().unwrap().take() {
                self.dispatch_video(&p, &mut Vec::new(), false);
            }
        }
    }

    /// 正在看我的人（编号）
    pub fn watcher_ids(&self) -> Vec<u32> {
        self.watchers.lock().unwrap().clone()
    }

    /// 打开 / 关掉跨帧交错（共享者每秒按观看者回报的丢包决定）
    pub fn set_interleave(&self, on: bool) {
        self.interleave.store(on, Ordering::Relaxed);
    }

    /// 记进补发用的历史
    fn record_video(&self, frags: &[Vec<u8>]) {
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
    }

    fn dispatch_video(&self, frags: &[Vec<u8>], _buf: &mut Vec<u8>, low: bool) {
        let watchers = self.layer_watchers(low);
        if watchers.is_empty() {
            return;
        }
        let mut direct = 0;
        if self.p2p_active() {
            let chosen = self.alloc.lock().unwrap().1.clone();
            let targets: Vec<(u32, SocketAddr)> = {
                let peers = self.peers.lock().unwrap();
                watchers
                    .iter()
                    .filter(|i| chosen.contains(i))
                    .filter_map(|id| peers.map.get(id).filter(|p| p.confirmed && p.sealer.is_some()).and_then(|p| p.path).map(|path| (*id, path)))
                    .collect()
            };
            for (id, path) in &targets {
                for f in frags {
                    self.send_paced(*path, SealFor::Peer(*id), udp::VIDEO, udp::server_video(self.my_id, f));
                }
            }
            direct = targets.len();
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
        if direct + relayed >= watchers.len() && dual == 0 || SHARE_P2P_ONLY.load(Ordering::Relaxed) {
            return; // 都送到了；或者选了只走 P2P（打不通的人看不了，不经服务器）
        }
        let udp_ok = self.udp_ok.load(Ordering::Relaxed);
        for f in frags {
            if udp_ok {
                self.send_paced(self.server, SealFor::Server, udp::VIDEO, f.clone());
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
        // 共享声音有自己的线路（服务器支持时）：直连发给 chosen，服务器替我转给 covered 以外的人
        let own_route = kind == udp::SHARE_AUDIO && SERVER_SHARE_AUDIO_ROUTE.load(Ordering::Relaxed);
        let (chosen, covered) = if own_route {
            self.share_alloc.lock().unwrap().clone()
        } else {
            let c = if music { self.alloc.lock().unwrap().0.clone() } else { self.alloc.lock().unwrap().1.clone() };
            (c, Vec::new())
        };
        let mut sent = Vec::new();
        if self.p2p_active() {
            let payload = udp::server_voice(self.my_id, seq, blob);
            let mut peers = self.peers.lock().unwrap();
            for id in targets.iter().filter(|i| chosen.contains(i)) {
                let Some(p) = peers.map.get_mut(id) else { continue };
                if let (true, Some(path), Some(s)) = (p.confirmed, p.path, p.sealer.as_mut()) {
                    s.seal(kind, &payload, buf);
                    self.send_peer(kind, buf, path);
                    sent.push(*id);
                }
            }
        }
        if !music && SHARE_P2P_ONLY.load(Ordering::Relaxed) {
            return; // 共享声音 / 鼠标只走 P2P
        }
        let need_server = if own_route {
            targets.iter().any(|t| !(covered.contains(t) && sent.contains(t)))
        } else {
            sent.len() < targets.len()
        };
        if need_server {
            if self.udp_ok.load(Ordering::Relaxed) {
                self.send_server(kind, &udp::client_voice(seq, blob), buf);
            } else {
                // UDP 不通：和画面、语音一样改走 TCP（0.5.x 这里直接不发，没直连的人听不到）
                self.tcp_send(Outgoing::Stream(kind, udp::client_voice(seq, blob)));
            }
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
        let n = self.pace_dropped.swap(0, Ordering::Relaxed);
        self.pace_dropped_sum.fetch_add(n, Ordering::Relaxed);
        n
    }

    /// 匀速队列累计丢了多少包（测试日志）
    pub fn pace_dropped_total(&self) -> u32 {
        self.pace_dropped_sum.load(Ordering::Relaxed) + self.pace_dropped.load(Ordering::Relaxed)
    }

    /// 画面包交给匀速发送线程（队列没起来时直接发）。每个目标一个队列，总字节数有上限：
    /// 满了丢这个包（画面会靠补发 / 恢复帧接上），不让延迟越积越多（0.6：第 41-1 项）
    /// 调用时不能拿着 peers 锁（没开匀速发送时当场加密要用它）
    fn send_paced(&self, to: SocketAddr, seal: SealFor, kind: u8, data: Vec<u8>) {
        crate::traffic::sent(crate::traffic::Cat::Share, data.len());
        let pkt = Paced { seal, kind, data };
        if !self.pacer.running.load(Ordering::Relaxed) {
            self.send_sealed(to, &pkt, &mut Vec::new());
            return;
        }
        let rate = self.pace_rate();
        let mut q = self.pacer.q.lock().unwrap();
        let cap = ((rate / 8.0) * PACE_QUEUE_SECS) as usize;
        if q.bytes + pkt.data.len() > cap.max(PACE_QUEUE_MIN) {
            drop(q);
            self.pace_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        q.push(to, pkt, Instant::now());
        drop(q);
        self.pacer.wake.notify_one();
    }

    /// 排队的画面包轮到了：这时才加密（编号按发出顺序），发给服务器 / 对端当前的线路
    fn send_sealed(&self, to: SocketAddr, p: &Paced, buf: &mut Vec<u8>) {
        match p.seal {
            SealFor::Server => {
                self.sealer.lock().unwrap().seal(p.kind, &p.data, buf);
                self.send_raw(buf, to);
            }
            SealFor::Peer(id) => {
                let path = {
                    let mut peers = self.peers.lock().unwrap();
                    let Some(peer) = peers.map.get_mut(&id) else { return };
                    let Some(s) = peer.sealer.as_mut() else { return };
                    s.seal(p.kind, &p.data, buf);
                    peer.path.unwrap_or(to)
                };
                self.send_raw(buf, path);
            }
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
        if SHARE_P2P_ONLY.load(Ordering::Relaxed) {
            self.dual_video.lock().unwrap().clear(); // 只走 P2P：不经服务器多发一份
            return 0;
        }
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
    /// 第 38-8 项：现在是不是「不稳的长距离线路」（到服务器或任何一条直连往返 > 150 ms，且最近 3 分钟有过突发丢包）
    pub fn long_haul(&self) -> bool {
        let now = Instant::now();
        let burst = *self.last_loss_burst.lock().unwrap();
        let srv = self.server_rtt.load(Ordering::Relaxed);
        long_haul_unstable(srv, burst, now) || self.direct_peers().values().any(|q| long_haul_unstable(q.rtt_ms, burst, now))
    }

    /// 记一次突发丢包（语音发出方向、画面观看者回报）
    pub fn note_loss_burst(&self) {
        *self.last_loss_burst.lock().unwrap() = Some(Instant::now());
    }

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

    /// 我帮人转发画面时，被转发的观看者 c 找我补片：先从我转过的包里找；没有的把请求直连转给共享者
    /// （共享者补发给我，我照常转给 c）。只给路线上安排给我的观看者补
    fn relay_resend(&self, c: u32, sharer: u32, layer: u8, frame: u32, idxs: &[u16], nack: &[u8]) {
        if !self.video_relays.lock().unwrap().iter().any(|r| r.via == self.my_id && r.from == sharer && r.to == c) {
            return;
        }
        let whole = idxs.contains(&udp::NACK_WHOLE);
        let found: Vec<(u16, Vec<u8>)> = self
            .relay_cache
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.1 == sharer && e.2 == layer && e.3 == frame && (whole || idxs.contains(&e.4)))
            .map(|e| (e.4, e.5.clone()))
            .collect();
        let mut buf = Vec::new();
        let path = self.peers.lock().unwrap().map.get(&c).filter(|p| p.confirmed && p.sealer.is_some()).and_then(|p| p.path);
        if let Some(path) = path {
            for (_, pl) in &found {
                self.send_paced(path, SealFor::Peer(c), udp::VIDEO, pl.clone());
            }
        }
        // 我这儿没有的（或整帧都要）：直连向共享者要
        let missing: Vec<u16> = if whole { Vec::new() } else { idxs.iter().copied().filter(|i| !found.iter().any(|(j, _)| j == i)).collect() };
        if whole && found.is_empty() || !missing.is_empty() {
            let fwd = if whole { nack.to_vec() } else { udp::nack_payload(sharer, layer, frame, &missing) };
            let mut peers = self.peers.lock().unwrap();
            if let Some(p) = peers.map.get_mut(&sharer) {
                if let (true, Some(path), Some(s)) = (p.confirmed, p.path, p.sealer.as_mut()) {
                    s.seal(udp::NACK, &fwd, &mut buf);
                    self.send_peer(udp::NACK, &buf, path);
                }
            }
        }
    }

    /// 观看者请求补发：peer = 直连收到的（直接重发给他），None = 服务器转来的（重发给服务器，服务器照常转发）
    pub fn resend_video(&self, peer: Option<u32>, nack: &[u8]) {
        let Some((target, layer, frame, idxs)) = udp::read_nack(nack) else { return };
        if target != self.my_id {
            // 不是要我的画面：是我转发给他的那一路（就近补发）
            if let Some(c) = peer {
                self.relay_resend(c, target, layer, frame, &idxs, nack);
            }
            return;
        }
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
        match peer {
            Some(id) => {
                let path = self.peers.lock().unwrap().map.get(&id).filter(|p| p.confirmed && p.sealer.is_some()).and_then(|p| p.path);
                if let Some(path) = path {
                    for f in &frags {
                        self.send_paced(path, SealFor::Peer(id), udp::VIDEO, udp::server_video(self.my_id, f));
                    }
                }
            }
            None if SHARE_P2P_ONLY.load(Ordering::Relaxed) => {} // 只走 P2P：不经服务器补发
            None => {
                for f in frags {
                    self.send_paced(self.server, SealFor::Server, udp::VIDEO, f);
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
                    self.send_peer(udp::NACK, &buf, path);
                    sent = true;
                }
            }
        }
        // 画面是别的观看者转给我的：先向他要（他刚转过，手上有；没有的他会向共享者要），比绕服务器快
        if !sent {
            let via = self.video_relays.lock().unwrap().iter().find(|r| r.from == to && r.to == self.my_id).map(|r| r.via);
            if let Some(via) = via {
                let mut peers = self.peers.lock().unwrap();
                if let Some(p) = peers.map.get_mut(&via) {
                    if let (true, Some(path), Some(s)) = (p.confirmed, p.path, p.sealer.as_mut()) {
                        s.seal(udp::NACK, payload, &mut buf);
                        self.send_peer(udp::NACK, &buf, path);
                        sent = true;
                    }
                }
            }
        }
        // 没直连共享者、也没有转发的队友可问：发给服务器
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
        let n = classify_nat3(main, probe, *self.probe2_addr.lock().unwrap(), &self.lan_addrs);
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

    /// 第 39-3 项：为某个还没打通的人要一个 PCP PEER 映射（每人只试一次，在后台线程里）
    fn try_pcp_peer(self: &Arc<Self>, id: u32, cands: &[SocketAddr]) {
        let Some((gw, lan, port)) = *self.pcp_peer_gw.lock().unwrap() else { return };
        let Some(remote) = cands.iter().find_map(|a| match a {
            SocketAddr::V4(v4) if !crate::portmap::is_carrier_or_private(*v4.ip()) => Some(*v4),
            _ => None,
        }) else {
            return;
        };
        if !self.pcp_peer_tried.lock().unwrap().insert(id) {
            return;
        }
        let link = self.clone();
        let _ = std::thread::Builder::new().name("pcp-peer".into()).spawn(move || {
            match crate::portmap::pcp_peer(gw, lan, port, remote, 600) {
                Some((ip, p)) if !crate::portmap::is_carrier_or_private(ip) => {
                    crate::applog::log(format!("PCP PEER：给 #{id} 要到了映射"));
                    link.add_local_candidate(SocketAddr::new(IpAddr::V4(ip), p).to_string());
                }
                _ => crate::applog::log(format!("PCP PEER：给 #{id} 没要到映射")),
            }
        });
    }

    /// 第二个探测点的结果（第 39-2 项，诊断用）；服务器没有第二个 IP 时为 None
    pub fn nat_detail(&self) -> Option<String> {
        self.probe2?;
        let main = *self.public_addr.lock().unwrap();
        let p2 = *self.probe2_addr.lock().unwrap();
        let mapping = match (main, p2) {
            (Some(a), Some(b)) if a == b => "换了对方 IP 映射不变（好打洞）",
            (Some(_), Some(_)) => "换了对方 IP 映射就变（对不同队友相当于对称型）",
            _ => "还没测出来",
        };
        let filter = match *self.filter_open.lock().unwrap() {
            Some(true) => "没联系过的地址也进得来",
            Some(false) => "只放行联系过的地址",
            None => "还没测出来",
        };
        Some(format!("{mapping}；{filter}"))
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

    /// 设置手动线路（id → 选择；不在表里的按自动）。只影响选了的那个人：
    /// 改「不透露 IP」才会重新打洞 / 拆掉和他的直连，改语音或画面线路不动直连本身（0.6：调选项不动全局）
    pub fn set_route_prefs(&self, prefs: HashMap<u32, PeerPref>) {
        let hide_changed = {
            let mut cur = self.route_prefs.lock().unwrap();
            if *cur == prefs {
                return;
            }
            let hidden = |m: &HashMap<u32, PeerPref>| m.iter().filter(|(_, p)| p.hide_ip).map(|(id, _)| *id).collect::<std::collections::BTreeSet<u32>>();
            let changed = hidden(&cur) != hidden(&prefs);
            *cur = prefs;
            changed
        };
        if hide_changed {
            self.send_p2p_hide();
            // 不透露 IP 的人从直连候选里去掉，改回来的重新打洞（其他人不受影响）
            let list = self.last_peer_list.lock().unwrap().clone();
            self.on_peers(list);
        }
    }

    /// 告诉服务器：选了不透露 IP 的人，双方地址互不透露
    fn send_p2p_hide(&self) {
        if !self.p2p_enabled.load(Ordering::Relaxed) {
            return;
        }
        let mut ids: Vec<u32> = self.route_prefs.lock().unwrap().iter().filter(|(_, r)| r.hide_ip).map(|(id, _)| *id).collect();
        ids.sort();
        self.tcp_send(Outgoing::Msg(ClientMsg::P2pHide { ids }));
    }

    pub fn route_pref(&self, id: u32) -> PeerPref {
        self.route_prefs.lock().unwrap().get(&id).copied().unwrap_or_default()
    }

    /// 语音优先：服务器说这些人在我说话时没听到我（直连 / 中转那条路实际不通），给他们的语音先改经服务器
    pub fn set_server_fallback(&self, ids: Vec<u32>) {
        let new: std::collections::HashSet<u32> = ids.into_iter().collect();
        let mut cur = self.server_fallback.lock().unwrap();
        if *cur != new {
            let list = |s: &std::collections::HashSet<u32>| {
                let mut v: Vec<String> = s.iter().map(|i| format!("#{i}")).collect();
                v.sort();
                if v.is_empty() { "无".into() } else { v.join(" ") }
            };
            crate::applog::log(format!("线路：语音优先，给这些人的语音改经服务器（他们没听到我）：{}", list(&new)));
            *cur = new;
        }
    }

    /// 打洞开头的密集补打：刚开始打洞（或刚断线重打）的头 150 ms 里，在 20 / 50 / 100 ms 左右各补一轮
    /// （正常节奏是 200 ms 一轮）。网络好时几十毫秒就通。返回还有没有人在这个窗口里
    fn punch_burst(&self, buf: &mut Vec<u8>) -> bool {
        let now = Instant::now();
        let mut any = false;
        let mut peers = self.peers.lock().unwrap();
        for p in peers.map.values_mut() {
            let Some(started) = p.punch_started.filter(|_| !p.confirmed) else { continue };
            let age = now.duration_since(started);
            if age >= BURST_WINDOW {
                continue;
            }
            any = true;
            if now.duration_since(p.last_punch) < BURST_GAP {
                continue;
            }
            let targets = punch_targets(&p.cands, None, p.prev_path, &p.spare());
            let Some(s) = p.sealer.as_mut() else { continue };
            p.last_punch = now;
            for t in targets {
                s.seal(p2p::PUNCH, &now_millis().to_le_bytes(), buf);
                self.send_raw(buf, t);
            }
        }
        any
    }

    /// 语音优先：给他的语音现在是不是临时改经服务器
    pub fn in_server_fallback(&self, peer: u32) -> bool {
        self.server_fallback.lock().unwrap().contains(&peer)
    }

    /// 经服务器收到一包语音：按每个人的序号算丢了多少（按听众保护语音用，第 42-5 项）
    fn note_server_voice(&self, payload: &[u8]) {
        let Some((from, seq, _)) = udp::read_server_voice(payload) else { return };
        let mut m = self.srv_voice_loss.lock().unwrap();
        let e = m.entry(from).or_insert((seq.wrapping_sub(1), 0, 0));
        let gap = seq.wrapping_sub(e.0).wrapping_sub(1);
        if seq_newer(seq, e.0) {
            // 小的断档算丢包；大的（对方停了一阵、换了路）不算
            if (1..=25).contains(&gap) {
                e.2 += gap as u32;
            }
            e.0 = seq;
            e.1 += 1;
        }
    }

    /// 取走这一段经服务器收语音的丢包率（%）；样本太少返回 0
    fn take_server_voice_loss(&self) -> u8 {
        let mut m = self.srv_voice_loss.lock().unwrap();
        let (got, lost) = m.values().fold((0u32, 0u32), |a, e| (a.0 + e.1, a.1 + e.2));
        for e in m.values_mut() {
            e.1 = 0;
            e.2 = 0;
        }
        if got + lost < 50 {
            return 0;
        }
        (lost * 100 / (got + lost)).min(100) as u8
    }

    /// 收到并解出了某人的语音（语音优先上报用）
    pub fn note_heard(&self, from: u32) {
        self.heard.lock().unwrap().insert(from, Instant::now());
    }

    /// 我在某个中转路线上没能经指定的人中转：返回经谁（界面提示「暂时走服务器」用）
    pub fn manual_via_unmet(&self, peer: u32) -> Option<u32> {
        let via = self.route_pref(peer).voice_via?;
        let ok = self.relays.lock().unwrap().iter().any(|r| r.via == via && ((r.from == self.my_id && r.to == peer) || (r.from == peer && r.to == self.my_id)));
        (!ok).then_some(via)
    }

    pub fn on_peers(&self, list: Vec<PeerInfo>) {
        *self.last_peer_list.lock().unwrap() = list.clone();
        if !self.p2p_enabled.load(Ordering::Relaxed) {
            return;
        }
        // 选了不透露 IP 的人不打洞
        let prefs = self.route_prefs.lock().unwrap().clone();
        let list: Vec<PeerInfo> = list.into_iter().filter(|p| !prefs.get(&p.id).is_some_and(|r| r.hide_ip)).collect();
        let mut peers = self.peers.lock().unwrap();
        peers.map.retain(|id, _| list.iter().any(|p| p.id == *id));
        for info in list {
            let cands: Vec<SocketAddr> = info.addrs.iter().filter_map(|a| a.parse().ok()).map(norm).collect();
            let p = peers.map.entry(info.id).or_insert_with(|| Peer::new(Vec::new()));
            p.nat = info.nat;
            if p.cands != cands {
                p.cands = cands;
                p.punch_started = None; // 有新地址：重新快速打洞
                p.bday_tried = false;
            }
        }
    }

    /// 生日攻击的一步（每 100 ms，对还没打通的一个人）：该开始就开始、开端口 / 发探测、收备用端口上的包、到时间就收摊。
    /// 返回 true = 正在进行
    fn birthday_step(&self, id: u32, p: &mut Peer, my_nat: Option<NatInfo>, now: Instant, buf: &mut Vec<u8>) -> bool {
        let random = |n: Option<NatInfo>| n.is_some_and(|n| n.kind == voc_proto::NAT_SYM_RANDOM);
        let (me_hard, peer_hard) = (random(my_nat), random(p.nat));
        let Some(started) = p.punch_started else { return false };
        if p.confirmed || !(me_hard || peer_hard) {
            return false;
        }
        // 对方的公网地址（IPv4；生日攻击只对 IPv4 NAT 有意义）
        let Some(public) = p.cands.iter().copied().find(|a| a.is_ipv4() && !is_private_or_local(a.ip())) else { return false };
        if p.bday.is_none() {
            if p.bday_tried || now.duration_since(started) < BDAY_AFTER {
                return false;
            }
            p.bday_tried = true;
            let half = me_hard && peer_hard; // 两边都随机：撞上的机会小，量减半
            let scale = |n: usize| if half { n / 2 } else { n };
            p.bday = Some(Birthday {
                started: now,
                socks: Vec::new(),
                sock_quota: if me_hard { scale(BDAY_SOCKS) } else { 0 },
                sent_ports: Default::default(),
                probe_quota: if peer_hard { scale(BDAY_PROBES) } else { 0 },
                per_tick: if half { BDAY_PER_TICK / 2 } else { BDAY_PER_TICK },
                last_resend: now,
            });
            let who = match (me_hard, peer_hard) {
                (true, true) => "两边都是端口随机的对称型 NAT",
                (true, false) => "我这边是端口随机的对称型 NAT",
                _ => "对方是端口随机的对称型 NAT",
            };
            crate::applog::log(format!("线路：和 #{id} 正常打洞没通，{who}，开始生日攻击（最多 {} 秒）", BDAY_FOR.as_secs()));
        }
        let Some(s) = p.sealer.as_mut() else { return false };
        let b = p.bday.as_mut().unwrap();
        if now.duration_since(b.started) > BDAY_FOR {
            crate::applog::log(format!("线路：和 #{id} 生日攻击 {} 秒没撞上，停止（开了 {} 个端口、发了 {} 个探测）", BDAY_FOR.as_secs(), b.socks.len(), b.sent_ports.len()));
            p.bday = None;
            return false;
        }
        // 我端口随机：再开几个端口，每个朝对方发一个打洞包（在我的 NAT 上开出映射）
        let mut fresh = Vec::new();
        for _ in 0..b.per_tick {
            if b.socks.len() >= b.sock_quota {
                break;
            }
            match UdpSocket::bind("0.0.0.0:0").and_then(|u| u.set_nonblocking(true).map(|_| u)) {
                Ok(u) => {
                    let u = Arc::new(u);
                    fresh.push(u.clone());
                    b.socks.push(u);
                }
                Err(_) => {
                    b.sock_quota = b.socks.len(); // 开不出来了：就用这么多
                    break;
                }
            }
        }
        let resend = now.duration_since(b.last_resend) >= Duration::from_secs(2);
        if resend {
            b.last_resend = now;
        }
        for u in if resend { &b.socks } else { &fresh } {
            s.seal(p2p::PUNCH, &now_millis().to_le_bytes(), buf);
            let _ = u.send_to(buf, public);
        }
        // 对方端口随机：朝他公网 IP 的随机端口发
        let left = b.probe_quota.saturating_sub(b.sent_ports.len()).min(b.per_tick);
        for port in bday_ports(&self.bday_rng, &mut b.sent_ports, left) {
            s.seal(p2p::PUNCH, &now_millis().to_le_bytes(), buf);
            self.send_raw(buf, SocketAddr::new(public.ip(), port));
        }
        // 备用端口上有没有对方的包（撞上了）：交给主接收循环（回 PUNCH_ACK 会自动从这个端口发出去）
        let mut rb = [0u8; 2048];
        for u in &b.socks {
            while let Ok((n, from)) = u.recv_from(&mut rb) {
                self.alt_received(u, from, &rb[..n]);
            }
        }
        true
    }

    /// 生日攻击打通了：留下撞上的那个端口（专门开一个线程收它的包），其余的关掉
    fn birthday_finish(self: &Arc<Self>, id: u32, p: &mut Peer) {
        let Some(b) = p.bday.take() else { return };
        let Some(path) = p.path else { return };
        let winner = self.alt_socks.lock().unwrap().get(&path).cloned();
        crate::applog::log(format!(
            "线路：和 #{id} 生日攻击打通（用时 {} ms，开了 {} 个端口、发了 {} 个探测）{}",
            b.started.elapsed().as_millis(),
            b.socks.len(),
            b.sent_ports.len(),
            if winner.is_some() { "，改用撞上的那个本地端口收发" } else { "" }
        ));
        if let Some(w) = winner {
            self.spawn_alt_reader(w);
        }
        // 其余端口随 b 一起关掉
    }

    /// 给一个备用端口开收包线程：收到的交给主接收循环；这个端口不再被用到（或链路关了）就退出
    fn spawn_alt_reader(self: &Arc<Self>, w: Arc<UdpSocket>) {
        let _ = w.set_nonblocking(false);
        let _ = w.set_read_timeout(Some(Duration::from_millis(500)));
        let l = self.clone();
        let _ = std::thread::Builder::new().name("udp-alt-recv".into()).spawn(move || {
            let mut rb = [0u8; 2048];
            while !l.closed.load(Ordering::Relaxed) && l.alt_socks.lock().unwrap().values().any(|s| Arc::ptr_eq(s, &w)) {
                if let Ok((n, from)) = w.recv_from(&mut rb) {
                    l.alt_received(&w, from, &rb[..n]);
                }
            }
        });
    }

    /// 每 100 ms 调一次：打洞、保活、判断断开、同步直连名单
    fn tick(self: &Arc<Self>, buf: &mut Vec<u8>) {
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
                        // 身份确认了：取出以前和他打通过的地址，打洞时先试
                        p.fp = e2e.members.get(id).map(|m| voc_proto::e2e::fingerprint(&m.idk)).unwrap_or_default();
                        p.remembered = crate::pathcache::recall(&p.fp, now_millis());
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
        for (id, p) in peers.map.iter_mut() {
            // 生日攻击：打通了就收摊（留下撞上的端口）；没打通、该做就做一步
            if p.confirmed && p.bday.is_some() {
                self.birthday_finish(*id, p);
            }
            if p.sealer.is_some() {
                self.birthday_step(*id, p, my_nat, now, buf);
                // 打了 2 秒还没通、网关只给 PEER：给这个人单独要映射，拿到的外部地址加进候选（第 39-3 项）
                if p.punch_started.is_some_and(|t| now.duration_since(t) > Duration::from_secs(2)) {
                    self.try_pcp_peer(*id, &p.cands);
                }
            }
            let spare = p.spare();
            let Some(s) = p.sealer.as_mut() else { continue };
            // 不在同一个频道的人（预先打好洞备用）：保活慢一点，断线判定也宽一点
            let idle = !others.contains(id);
            let dead_after = if idle { IDLE_PEER_DEAD } else { PEER_DEAD };
            if p.confirmed && now.duration_since(p.last_rx) > dead_after {
                crate::applog::log(format!("线路：和 #{id} 的直连断了（{} 秒没收到包），先走服务器 / 中转", dead_after.as_secs()));
                p.confirmed = false;
                p.prev_path = p.path.take();
                p.dropped_at = Some(now);
                p.punch_started = Some(now);
                p.bday_tried = false; // 断了要重新打：需要的话还可以再试生日攻击
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
                    let targets = punch_targets(&p.cands, p.path, p.prev_path, &spare);
                    // 这一轮打洞开始时先低 TTL 发一遍（只发 IPv4 公网地址，第 35-2 项）
                    if p.low_ttl_for != Some(started) {
                        p.low_ttl_for = Some(started);
                        let pkts: Vec<(Vec<u8>, SocketAddr)> = targets
                            .iter()
                            .filter(|t| matches!(t, SocketAddr::V4(v4) if !crate::portmap::is_carrier_or_private(*v4.ip())))
                            .map(|t| {
                                s.seal(p2p::PUNCH, &now_millis().to_le_bytes(), buf);
                                (buf.clone(), *t)
                            })
                            .collect();
                        self.low_ttl_burst(&pkts);
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
            } else if !idle && now.duration_since(p.last_upgrade) >= UPGRADE_CHECK && p.path.is_some_and(|cur| {
                punch_targets(&p.cands, None, None, &spare).iter().any(|t| addr_class(t) < addr_class(&cur))
            }) {
                // ICE 式：已经连通，但还有优先级更高的路没试通（例如走的是 IPv4 公网，对方还有局域网 / IPv6 地址）：
                // 每 5 秒往那些地址打一下，回了 PUNCH_ACK 就换过去（better_path）
                p.last_upgrade = now;
                let cur = p.path.unwrap();
                for t in punch_targets(&p.cands, None, None, &spare).into_iter().filter(|t| addr_class(t) < addr_class(&cur)) {
                    s.seal(p2p::PUNCH, &now_millis().to_le_bytes(), buf);
                    self.send_raw(buf, t);
                }
            } else if now.duration_since(p.last_ping) >= if idle { IDLE_PING_INTERVAL } else { PEER_PING_INTERVAL } {
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
        // 手动「强制直连」的人：打通了就直连，不看稳定性、绕远和预算（语音、画面分开）
        let prefs = self.route_prefs.lock().unwrap().clone();
        let pref = |id: &u32| prefs.get(id).copied().unwrap_or_default();
        let up = |id: &u32| peers.map.get(id).is_some_and(|p| p.confirmed && p.alive(now));
        // 对方（或我）手动指定了经谁中转、服务器排了这条线：我这个方向也不直连，照指定的走（两个方向一致）
        let manual_relay: Vec<u32> = self.relays.lock().unwrap().iter().filter(|r| r.manual && r.from == self.my_id).map(|r| r.to).collect();
        let forced: Vec<u32> = others.iter().copied().filter(|id| pref(id).voice_direct && up(id) && !manual_relay.contains(id)).collect();
        // 语音不能直连的人（走服务器 / 不透露 IP / 指定了经谁中转）、服务器让我们改回服务器转发的人
        let fallback = self.server_fallback.lock().unwrap().clone();
        let no_voice: Vec<u32> = others.iter().copied().filter(|id| pref(id).voice_not_direct() || fallback.contains(id) || manual_relay.contains(id)).collect();
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
        let auto_others: Vec<u32> = others.iter().copied().filter(|i| !forced.contains(i) && !no_voice.contains(i)).collect();
        // 画面：观看者自己选的线路和我对这个人选的画面线路，冲突时按更保守的（走服务器）
        let (vd, mut vs) = self.watcher_routes.lock().unwrap().clone();
        vs.extend(watchers.iter().copied().filter(|i| pref(i).video_not_direct()));
        // 只走 P2P：能打通的观看者全部直连发，不受上行预算限制（服务器那条路不用）
        let p2p_only = SHARE_P2P_ONLY.load(Ordering::Relaxed);
        let forced_video: Vec<u32> = watchers
            .iter()
            .copied()
            .filter(|i| !vs.contains(i))
            .filter(|i| (pref(i).video_direct || vd.contains(i) || p2p_only) && up(i))
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
            // 语音选了走服务器 / 不透露 IP 的人：不安排中转（指定了经谁中转的不算）
            let mut via_server: Vec<u32> = prefs.iter().filter(|(_, r)| r.voice_server || r.hide_ip).map(|(id, _)| *id).collect();
            via_server.sort();
            let mut manual_via: Vec<[u32; 2]> = prefs.iter().filter_map(|(id, r)| r.voice_via.map(|v| [*id, v])).collect();
            manual_via.sort();
            // 语音优先：我在不在说话、最近听到了谁
            let talking = self.last_voice_sent.lock().unwrap().is_some_and(|t| now.duration_since(t) < Duration::from_secs(2));
            let heard: Vec<u32> = {
                let mut h = self.heard.lock().unwrap();
                h.retain(|_, t| now.duration_since(*t) < Duration::from_secs(30));
                let mut v: Vec<u32> = h.iter().filter(|(_, t)| now.duration_since(**t) < Duration::from_secs(3)).map(|(id, _)| *id).collect();
                v.sort();
                v
            };
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
                talking,
                heard,
                manual_via,
                voice_loss: self.take_server_voice_loss(),
            }));
        }
        // 记下突发丢包（第 38-8 项）：任何一条直连 ≥ 3%
        if voice.iter().any(|i| peers.map.get(i).is_some_and(|p| p.quality.loss_pct >= 3)) {
            *self.last_loss_burst.lock().unwrap() = Some(now);
        }
        let burst = *self.last_loss_burst.lock().unwrap();
        // 直连有丢包（≥ 5%）但还没到「不稳」的人：语音两条路同时发（服务器也照常转发给他，接收端按序号去重）。
        // 长距离、最近有过突发丢包的直连：门槛降到 2%（第 38-8 项，只对直连；服务器那条路本来就照常转）
        let dual_voice: std::collections::HashSet<u32> = voice
            .iter()
            .copied()
            .filter(|i| {
                peers.map.get(i).is_some_and(|p| {
                    let limit = if long_haul_unstable(p.quality.rtt_ms, burst, now) { DUAL_VOICE_LOSS_LONG } else { DUAL_VOICE_LOSS };
                    p.quality.loss_pct >= limit
                })
            })
            .collect();
        // 共享声音的线路（第 54 项）
        let share = {
            let route = SHARE_AUDIO_ROUTE.load(Ordering::Relaxed);
            let direct: Vec<u32> = if !active {
                Vec::new()
            } else if SHARE_P2P_ONLY.load(Ordering::Relaxed) {
                video.clone() // 画面只走 P2P：共享声音也不经服务器（服务器不转），能直连的都直连
            } else {
                match route {
                    SHARE_AUDIO_SERVER => Vec::new(),
                    SHARE_AUDIO_DIRECT => watchers.iter().copied().filter(|i| up(i) && !pref(i).hide_ip && !manual_relay.contains(i)).collect(),
                    _ => voice.iter().copied().filter(|i| watchers.contains(i)).collect(),
                }
            };
            // 自动：直连有丢包（和语音一样 ≥ 5%）的人服务器也照常转一份
            let covered: Vec<u32> = if route == SHARE_AUDIO_AUTO {
                direct.iter().copied().filter(|i| !dual_voice.contains(i)).collect()
            } else {
                direct.clone()
            };
            (direct, covered)
        };
        let share_report = SERVER_SHARE_AUDIO_ROUTE.load(Ordering::Relaxed).then(|| {
            let mut c = share.1.clone();
            c.sort();
            c
        });
        *self.share_alloc.lock().unwrap() = share;
        voice.retain(|i| !dual_voice.contains(i));
        *self.dual_voice.lock().unwrap() = dual_voice;
        let dual_video = self.dual_video.lock().unwrap().clone();
        video.retain(|i| !dual_video.contains(i));
        voice.sort();
        video.sort();
        if voice != peers.last_direct || video != peers.last_direct_video || share_report != peers.last_share_audio {
            let ids = |v: &[u32]| if v.is_empty() { "无".to_string() } else { v.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join(" ") };
            crate::applog::log(format!(
                "线路：语音直连 {}（两条路同时发 {}）· 画面直连 {}（两条路 {}）· 其余经服务器 / 中转",
                ids(&voice),
                ids(&self.dual_voice.lock().unwrap().iter().copied().collect::<Vec<_>>()),
                ids(&video),
                ids(&dual_video.iter().copied().collect::<Vec<_>>())
            ));
            if let Some(c) = &share_report {
                crate::applog::log(format!("线路：共享声音直连（服务器不用转）{}", ids(c)));
            }
            peers.last_direct = voice.clone();
            peers.last_direct_video = video.clone();
            peers.last_share_audio = share_report.clone();
            drop(peers);
            self.tcp_send(Outgoing::Msg(ClientMsg::DirectPeers { ids: voice, video, share_audio: share_report }));
        }
    }

    /// 处理来自非服务器地址的包。若是语音 / 视频，返回 (kind, 内容)（与服务器转发的格式相同）。
    fn on_peer_packet(&self, from: SocketAddr, pkt: &[u8], buf: &mut Vec<u8>) -> Option<(u32, u8, Vec<u8>)> {
        let mut peers = self.peers.lock().unwrap();
        // 先试地址匹配的对端；都不匹配（对端 NAT 端口和候选不同）才挨个试其余的。
        // 挨个试要算哈希和解密，限次数：有人往这个端口灌包时不至于把 CPU 吃满
        // 「认识的地址」= 当前路、候选、验证过的路、学到的对方真实出口（对方多网卡 / IPv6 临时地址时，包的来源可能不在候选里）
        let known = |p: &Peer| p.path == Some(from) || p.cands.contains(&from) || p.valid.contains_key(&from) || p.reflexive.contains(&from);
        let (mut ids, others): (Vec<u32>, Vec<u32>) = peers.map.iter().map(|(id, _)| *id).partition(|id| known(&peers.map[id]));
        let matched = !ids.is_empty();
        if !others.is_empty() {
            if self.unknown_trials.allow(Instant::now()) {
                ids.extend(others);
            } else if !matched {
                self.unknown_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        for id in ids {
            let p = peers.map.get_mut(&id).unwrap();
            let Some(op) = p.opener.as_mut() else { continue };
            let Some((kind, data)) = op.open(pkt) else { continue };
            p.last_rx = Instant::now();
            // 陌生地址试解成功：记下来，之后这个地址的包直接认（不再占限流名额）
            let new_addr = !matched || !(p.path == Some(from) || p.cands.contains(&from) || p.valid.contains_key(&from) || p.reflexive.contains(&from));
            if new_addr {
                if p.reflexive.len() >= 8 {
                    p.reflexive.remove(0);
                }
                p.reflexive.push(from);
            }
            let s = p.sealer.as_mut().unwrap();
            match kind {
                p2p::PUNCH => {
                    s.seal(p2p::PUNCH_ACK, &data, buf);
                    self.send_raw(buf, from);
                    if p.path.is_none() {
                        p.path = Some(from);
                    }
                    // 对方从一个我不知道的地址打过来（他 NAT 真实的出口，上面已记下）：也往那里打（ICE 的反向检查）
                    if new_addr {
                        s.seal(p2p::PUNCH, &now_millis().to_le_bytes(), buf);
                        self.send_raw(buf, from);
                    }
                }
                p2p::PUNCH_ACK => {
                    let rtt = if data.len() == 8 { now_millis().saturating_sub(u64::from_le_bytes(data[..8].try_into().unwrap())) as u32 } else { 0 };
                    p.valid.insert(from, (Instant::now(), rtt));
                    if p.valid.len() > 8 {
                        // 只留最近验证过的 8 条
                        if let Some(old) = p.valid.iter().min_by_key(|(_, v)| v.0).map(|(a, _)| *a) {
                            p.valid.remove(&old);
                        }
                    }
                    // 已连通时：只有更好的路（优先级更高，或同类但快很多）才换过去，不再「谁最后回就用谁」
                    let cur = p.path.filter(|_| p.confirmed).map(|c| (c, p.valid.get(&c).map(|v| v.1).unwrap_or(p.rtt_ms)));
                    if !p.confirmed || better_path(cur, from, rtt) {
                        if let (true, Some((c, _))) = (p.confirmed, cur) {
                            crate::applog::log(format!("线路：和 #{id} 的直连换到更好的路（{} → {}，往返 {rtt} ms）", addr_kind(&c), addr_kind(&from)));
                        }
                        p.path = Some(from);
                    }
                    if !p.confirmed {
                        let took = p.punch_started.map(|t| t.elapsed().as_millis()).unwrap_or(0);
                        crate::applog::log(format!("线路：和 #{id} 打通直连（用时 {took} ms）"));
                        // 断了没多久又通了（Wi-Fi 抖一下）：直接用回，不先走中转观察；真不稳的话质量统计会再判
                        if p.dropped_at.is_some_and(|t| t.elapsed() < QUICK_RECOVER) {
                            p.unstable_until = None;
                        }
                        p.dropped_at = None;
                        p.prev_path = None;
                        crate::pathcache::remember(&p.fp, from, now_millis());
                    }
                    p.confirmed = true;
                    p.punch_started = None;
                    if data.len() == 8 {
                        p.rtt_ms = now_millis().saturating_sub(u64::from_le_bytes(data[..8].try_into().unwrap())) as u32;
                    }
                }
                p2p::PING => {
                    s.seal(p2p::PONG, &data, buf);
                    if udp::is_mtu_probe(&data) {
                        // 回显探测包：不算进「VOC 实际发过的最大包」
                        let _r = self.ttl_lock.read().unwrap();
                        let _ = self.sock.send_to(buf, wire(from, self.dual));
                    } else {
                        self.send_raw(buf, from);
                    }
                }
                p2p::PONG if udp::is_mtu_probe(&data) => {
                    drop(peers);
                    self.on_mtu_reply(&data);
                    return None;
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
    /// 当前密钥编号（序号只在同一把密钥里比较：对方换密钥后计数器从 0 重新开始）
    epoch: Option<u16>,
    /// 最近用过的旧密钥编号（换密钥那一刻还在路上的旧包认得出来，直接丢）
    old_epochs: Vec<u16>,
    /// 对方一包多少样本（10 ms = 480，20 ms = 960）：补丢包时补这么长
    frame_len: usize,
}

impl UserDecoder {
    fn new(dec: crate::opusx::Decoder) -> Self {
        Self { dec, last_seq: None, epoch: None, old_epochs: Vec::new(), frame_len: FRAME_SAMPLES }
    }

    /// 这一包（已解密验证过）用的是哪把密钥：
    /// 当前的 → 照常；以前用过的 → 换密钥前在路上的旧包，丢掉；没见过的 → 对方换了新密钥，序号从头算。
    /// 返回 (要不要这一包, 是不是刚换了密钥)
    fn on_epoch(&mut self, epoch: u16) -> (bool, bool) {
        match self.epoch {
            Some(e) if e == epoch => (true, false),
            _ if self.old_epochs.contains(&epoch) => (false, false),
            cur => {
                if let Some(e) = cur {
                    self.old_epochs.push(e);
                    if self.old_epochs.len() > 4 {
                        self.old_epochs.remove(0);
                    }
                }
                self.epoch = Some(epoch);
                self.last_seq = None;
                (true, cur.is_some())
            }
        }
    }

    /// 序号检查：重复 / 稍旧的丢掉；差得太远（落后几百帧以上）当作对方重新开始，不再一直丢
    fn is_stale(&mut self, seq: u16) -> bool {
        match self.last_seq {
            Some(last) if !seq_newer(seq, last) => {
                if last.wrapping_sub(seq) >= RESTART_GAP {
                    self.last_seq = None;
                    false
                } else {
                    true
                }
            }
            _ => false,
        }
    }
}

/// 序号落后这么多帧（20 ms 一帧，约 10 秒）就当作对方重新开始
const RESTART_GAP: u16 = 500;

/// 去重、丢包补偿、解码一包：返回按时间顺序的 (序号, 声音)；重复 / 过期 / 解不了返回 None
fn decode_with_concealment(d: &mut UserDecoder, seq: u16, data: &[u8], pcm: &mut Vec<i16>) -> Option<Vec<(u16, Vec<i16>)>> {
    if d.is_stale(seq) {
        return None; // 重复（直连和中转都到了）或过期
    }
    // 带冗余的包（第 34-3 项）：拆出前一两帧的原包
    let (older, data) = match udp::read_voice_red(data) {
        Some((o, p)) => (o, p),
        None => (Vec::new(), data),
    };
    pcm.resize(FRAME_SAMPLES * 3, 0);
    let mut frames: Vec<(u16, Vec<i16>)> = Vec::new();
    if let Some(last) = d.last_seq {
        let gap = seq.wrapping_sub(last).wrapping_sub(1);
        if (1..=MAX_CONCEAL).contains(&gap) {
            // 丢包：先用这包里带的旧包原样还原，再用 DRED 深度冗余，不够的用 FEC / PLC
            frames.extend(conceal_gap(d, last, gap, seq, &older, data, pcm));
        }
    }
    let n = d.dec.decode(data, &mut pcm[..], false)?;
    d.frame_len = n.clamp(FRAME_SAMPLES / 4, FRAME_SAMPLES * 3);
    d.last_seq = Some(seq);
    frames.push((seq, pcm[..n].to_vec()));
    Some(frames)
}

/// 密文开头 2 字节是密钥编号（明文，解密成功后才用）
fn blob_epoch(blob: &[u8]) -> u16 {
    u16::from_le_bytes([blob[0], blob[1]])
}

/// 收到一包，前面丢了 gap 帧：先用这个包里的 DRED 还原（能还原多少算多少，越近的越准），
/// 剩下的离得远的用 PLC 补，紧挨着的一帧还可以用带内 FEC。返回按时间顺序的 (序号, 声音)
fn conceal_gap(d: &mut UserDecoder, last: u16, gap: u16, cur: u16, older: &[&[u8]], data: &[u8], pcm: &mut [i16]) -> Vec<(u16, Vec<i16>)> {
    let mut frames = Vec::new();
    let fl = d.frame_len;
    let dred = d.dec.dred_recover(data, gap as usize, fl);
    for i in 0..gap {
        let seq = last.wrapping_add(1 + i);
        // 这包里顺带的旧包：older[k-1] 是 cur-1，older[0] 是 cur-k
        let back = cur.wrapping_sub(seq) as usize;
        if (1..=older.len()).contains(&back) {
            if let Some(n) = d.dec.decode(older[older.len() - back], &mut pcm[..], false) {
                frames.push((seq, pcm[..n].to_vec()));
                continue;
            }
        }
        if let Some(Some(f)) = dred.get(i as usize) {
            frames.push((seq, f.clone()));
            continue;
        }
        let n = if i + 1 == gap {
            d.dec.decode(data, &mut pcm[..fl], true)
        } else {
            d.dec.decode(&[], &mut pcm[..fl], false)
        };
        if let Some(n) = n {
            frames.push((seq, pcm[..n].to_vec()));
        }
    }
    frames
}

/// 伴奏 / 共享声音（立体声解码，没有 DRED）：丢了帧用下一包的带内 FEC（紧挨着的一帧）和 PLC 补。
/// 返回按时间顺序的 (序号, 左右交错的声音)
fn decode_stream_stereo(d: &mut UserDecoder, seq: u16, data: &[u8], pcm: &mut Vec<i16>) -> Option<Vec<(u16, Vec<i16>)>> {
    if d.is_stale(seq) {
        return None;
    }
    pcm.resize(FRAME_SAMPLES * 3 * 2, 0);
    let mut frames: Vec<(u16, Vec<i16>)> = Vec::new();
    if let Some(last) = d.last_seq {
        let gap = seq.wrapping_sub(last).wrapping_sub(1);
        // 音乐断太久（> 0.2 秒）就不补了：PLC 补长了是怪声，不如淡出（播放端会淡出 / 淡入）
        if (1..=10).contains(&gap) {
            for i in 0..gap {
                let n = if i + 1 == gap {
                    d.dec.decode_ch(data, &mut pcm[..FRAME_SAMPLES * 2], true, 2)
                } else {
                    d.dec.decode_ch(&[], &mut pcm[..FRAME_SAMPLES * 2], false, 2)
                };
                if let Some(n) = n {
                    frames.push((last.wrapping_add(1 + i), pcm[..n * 2].to_vec()));
                }
            }
        }
    }
    let n = d.dec.decode_ch(data, &mut pcm[..], false, 2)?;
    d.last_seq = Some(seq);
    frames.push((seq, pcm[..n * 2].to_vec()));
    Some(frames)
}

/// 中间丢了多少帧以内还补（DRED 最多约 1 秒；太长的断档当作重新开始）
const MAX_CONCEAL: u16 = 50;

/// UI 可读取的语音状态
#[derive(Default)]
pub struct VoiceState {
    pub link: Mutex<Option<Arc<Link>>>,
    /// 用户最后一次收到语音包的时间
    pub last_heard: Mutex<HashMap<u32, Instant>>,
    /// 测试日志：每个人的语音（放出的帧数, 其中补出来的帧数），每秒取走清零
    pub voice_rx: Mutex<HashMap<u32, (u32, u32)>>,
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
            // 立体声解码：对方发的是单声道时左右一样（第 55-b 项）
            let Some(dec) = crate::opusx::Decoder::with_channels(SAMPLE_RATE, 2) else { return };
            v.insert(UserDecoder::new(dec))
        }
    };
    let (keep, rekeyed) = d.on_epoch(blob_epoch(blob));
    if !keep {
        return;
    }
    let Some(frames) = decode_stream_stereo(d, seq, &data, pcm) else { return };
    drop(decs);
    let id = crate::audio::stream_mixer_id(kind, from);
    let mut m = mixer.lock().unwrap();
    if rekeyed {
        m.restart_seq(id);
    }
    let concealed = frames.len().saturating_sub(1) as u32;
    for (s, f) in frames {
        m.push_ch(id, s, &f, 2);
    }
    if concealed > 0 {
        m.note_concealed(id, concealed);
    }
}

/// 共享者单独发来的鼠标位置
fn on_cursor(payload: &[u8], state: &VoiceState) {
    let Some((from, _, blob)) = udp::read_server_voice(payload) else { return };
    let Some(e2e) = state.e2e.lock().unwrap().clone() else { return };
    let Some((_, data)) = e2e.lock().unwrap().decrypt_stream(from, voc_proto::e2e::STREAM_CURSOR, blob) else { return };
    if let Some((x, y, shape, vis)) = udp::read_cursor(&data) {
        state.video.set_cursor(from, x, y, shape, vis, udp::read_share_info(&data));
    }
}

/// 流量统计：按 UDP 包类型归到语音（含伴奏）/ 屏幕共享
fn count_rx(kind: u8, len: usize) {
    match kind {
        udp::VOICE | udp::MUSIC => crate::traffic::received(crate::traffic::Cat::Voice, len),
        udp::VIDEO | udp::SHARE_AUDIO | udp::CURSOR | udp::NACK => crate::traffic::received(crate::traffic::Cat::Share, len),
        _ => {}
    }
}

fn count_tx(kind: u8, len: usize) {
    match kind {
        udp::VOICE | udp::MUSIC => crate::traffic::sent(crate::traffic::Cat::Voice, len),
        udp::VIDEO | udp::SHARE_AUDIO | udp::CURSOR | udp::NACK => crate::traffic::sent(crate::traffic::Cat::Share, len),
        _ => {}
    }
}

/// UDP 不通时经 TCP 隧道收到的伴奏 / 共享声音 / 鼠标（crypto::KIND_STREAM：类型 1 字节 + 和 UDP 一样的内容）
pub fn on_tcp_stream(data: &[u8], state: &VoiceState, mixer: &Mutex<Mixer>, pcm: &mut Vec<i16>) {
    let Some((&kind, payload)) = data.split_first() else { return };
    match kind {
        udp::MUSIC | udp::SHARE_AUDIO => on_stream_audio(kind, payload, state, mixer, pcm),
        udp::CURSOR => on_cursor(payload, state),
        _ => {}
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

/// 测试用的弱网模拟（第 40-3 项，只在设了环境变量时打开，作用在收到的所有 UDP 包上）：
/// - VOC_TEST_DELAY_MS=毫秒：每个包晚这么久（两端都设 = 往返多两倍）
/// - VOC_TEST_JITTER_MS=毫秒：再随机多晚 0～这么多（抖动）
/// - VOC_TEST_REORDER=百分比：这么多的包额外晚 5～30 ms（乱序）
/// - VOC_TEST_NET_LOSS=百分比 + VOC_TEST_BURST=平均连丢几个：所有包按突发丢包模型丢（Gilbert-Elliott；不设 BURST = 随机丢）
/// - VOC_TEST_BW_KBPS=千比特每秒：下行带宽上限，排队超过 300 ms 的丢掉（模拟拥塞）
/// （原有的 VOC_TEST_LOSS 只丢画面包、VOC_TEST_VOICE_LOSS 只丢语音包，见 test_drop_video / test_drop_voice）
struct TestNet {
    rx: std::sync::mpsc::Receiver<(Instant, SocketAddr, Vec<u8>)>,
    cfg: TestNetCfg,
    /// 按「该送达的时间」排的待送包
    queue: std::collections::BinaryHeap<std::cmp::Reverse<(Instant, u64, SocketAddr, Vec<u8>)>>,
    seq: u64,
    /// 按对端的条件要查链路（谁是谁）
    link: Option<Arc<Link>>,
    /// 带宽上限：上一个包送达的时间
    link_free: Instant,
    /// 突发丢包模型：现在在「坏」状态
    bad: bool,
    rng: u64,
}

#[derive(Default, Clone, Copy)]
struct TestNetCfg {
    delay: Duration,
    jitter_ms: u32,
    reorder_pct: u32,
    loss_pct: f32,
    burst: f32,
    bw_kbps: u32,
}

impl TestNetCfg {
    fn from_env() -> Option<Self> {
        let num = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<f32>().ok()).filter(|v| *v > 0.0);
        let c = Self {
            delay: Duration::from_millis(num("VOC_TEST_DELAY_MS").unwrap_or(0.0) as u64),
            jitter_ms: num("VOC_TEST_JITTER_MS").unwrap_or(0.0) as u32,
            reorder_pct: num("VOC_TEST_REORDER").unwrap_or(0.0) as u32,
            loss_pct: num("VOC_TEST_NET_LOSS").unwrap_or(0.0),
            burst: num("VOC_TEST_BURST").unwrap_or(1.0).max(1.0),
            bw_kbps: num("VOC_TEST_BW_KBPS").unwrap_or(0.0) as u32,
        };
        (c.delay > Duration::ZERO || c.jitter_ms > 0 || c.reorder_pct > 0 || c.loss_pct > 0.0 || c.bw_kbps > 0 || !test_peer_rules().is_empty()).then_some(c)
    }
}

impl TestNet {
    fn start(sock: &UdpSocket, link: Arc<Link>) -> Option<Self> {
        let cfg = TestNetCfg::from_env()?;
        let sock = sock.try_clone().ok()?;
        let (tx, rx) = std::sync::mpsc::channel();
        let link2 = link.clone();
        std::thread::Builder::new()
            .name("test-net".into())
            .spawn(move || {
                let mut b = [0u8; 2048];
                while !link2.closed.load(Ordering::Relaxed) {
                    if let Ok((n, f)) = sock.recv_from(&mut b) {
                        if tx.send((Instant::now(), f, b[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                }
            })
            .ok()?;
        let mut n = Self::new(rx, cfg);
        n.link = Some(link);
        Some(n)
    }

    fn new(rx: std::sync::mpsc::Receiver<(Instant, SocketAddr, Vec<u8>)>, cfg: TestNetCfg) -> Self {
        Self { rx, cfg, queue: Default::default(), seq: 0, link: None, link_free: Instant::now(), bad: false, rng: 0x2545_F491_4F6C_DD1D }
    }

    fn rand(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng % 1_000_000) as f32 / 1_000_000.0
    }

    /// 一个包到了：按模型决定丢不丢、什么时候送达
    fn admit(&mut self, t: Instant, from: SocketAddr, pkt: Vec<u8>) {
        // 这个对端单独设了条件（三人测试）：按他的来
        if let Some(r) = self.link.as_ref().and_then(|l| l.test_rule_for_addr(norm(from))) {
            if self.rand() * 100.0 < r.loss {
                return;
            }
            let due = t + r.delay + Duration::from_millis((self.rand() * r.jitter_ms as f32) as u64);
            self.seq += 1;
            self.queue.push(std::cmp::Reverse((due, self.seq, from, pkt)));
            return;
        }
        let c = self.cfg;
        if c.loss_pct > 0.0 {
            // Gilbert-Elliott：坏状态里全丢；平均连丢 burst 个，长期丢包率 loss_pct
            let p = (c.loss_pct / 100.0).min(0.95);
            let to_bad = p / (c.burst * (1.0 - p));
            let to_good = 1.0 / c.burst;
            let r = self.rand();
            self.bad = if self.bad { r >= to_good } else { r < to_bad };
            if self.bad {
                return;
            }
        }
        let mut due = t + c.delay;
        if c.jitter_ms > 0 {
            due += Duration::from_millis((self.rand() * c.jitter_ms as f32) as u64);
        }
        if c.reorder_pct > 0 && self.rand() * 100.0 < c.reorder_pct as f32 {
            due += Duration::from_millis(5 + (self.rand() * 25.0) as u64);
        }
        if c.bw_kbps > 0 {
            let tx_time = Duration::from_secs_f64(pkt.len() as f64 * 8.0 / (c.bw_kbps as f64 * 1000.0));
            let start = self.link_free.max(t);
            if start.duration_since(t) > Duration::from_millis(300) {
                return; // 排队太久：路由器丢包
            }
            self.link_free = start + tx_time;
            due = due.max(self.link_free + c.delay);
        }
        self.seq += 1;
        self.queue.push(std::cmp::Reverse((due, self.seq, from, pkt)));
    }

    /// 取下一个到时间的包
    fn next(&mut self, out: &mut Vec<u8>) -> Option<SocketAddr> {
        loop {
            while let Ok((t, f, p)) = self.rx.try_recv() {
                self.admit(t, f, p);
            }
            let now = Instant::now();
            match self.queue.peek() {
                Some(std::cmp::Reverse((due, ..))) if *due <= now => {
                    let std::cmp::Reverse((_, _, from, pkt)) = self.queue.pop().unwrap();
                    *out = pkt;
                    return Some(from);
                }
                Some(std::cmp::Reverse((due, ..))) => {
                    let wait = (*due - now).min(Duration::from_millis(5));
                    if let Ok((t, f, p)) = self.rx.recv_timeout(wait) {
                        self.admit(t, f, p);
                    }
                }
                None => match self.rx.recv_timeout(Duration::from_millis(200)) {
                    Ok((t, f, p)) => self.admit(t, f, p),
                    Err(_) => return None,
                },
            }
        }
    }
}

/// 测试用：按对端设网络条件（三人测试）。VOC_TEST_PEER_NET="昵称=单程延迟ms,丢包%,抖动ms;昵称2=…"：
/// 从这个人直连发来的所有包（含打洞、ping）按这个条件；VOC_TEST_PEER_NET_SERVER=1 时，他经服务器转来的语音 / 画面也按这个条件
#[derive(Clone, Copy, Debug)]
struct PeerRule {
    delay: Duration,
    loss: f32,
    jitter_ms: u32,
}

fn test_peer_rules() -> &'static HashMap<String, PeerRule> {
    static R: std::sync::OnceLock<HashMap<String, PeerRule>> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        let Ok(v) = std::env::var("VOC_TEST_PEER_NET") else { return HashMap::new() };
        v.split(';')
            .filter_map(|item| {
                let (name, vals) = item.split_once('=')?;
                let n: Vec<f32> = vals.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                Some((name.trim().to_string(), PeerRule { delay: Duration::from_millis(*n.first()? as u64), loss: *n.get(1).unwrap_or(&0.0), jitter_ms: *n.get(2).unwrap_or(&0.0) as u32 }))
            })
            .collect()
    })
}

fn test_server_path_rules() -> bool {
    std::env::var("VOC_TEST_PEER_NET_SERVER").as_deref() == Ok("1")
}

impl Link {
    fn test_rule_for_id(&self, id: u32) -> Option<PeerRule> {
        let rules = test_peer_rules();
        if rules.is_empty() {
            return None;
        }
        let name = self.e2e.lock().unwrap().members.get(&id)?.name.clone();
        rules.get(&name).copied()
    }

    fn test_rule_for_addr(&self, a: SocketAddr) -> Option<PeerRule> {
        if test_peer_rules().is_empty() || a == self.server {
            return None;
        }
        let id = {
            let peers = self.peers.lock().unwrap();
            peers
                .map
                .iter()
                .find(|(_, p)| p.path == Some(a) || p.cands.contains(&a) || p.valid.contains_key(&a) || p.reflexive.contains(&a) || p.prev_path == Some(a))
                .map(|(id, _)| *id)?
        };
        self.test_rule_for_id(id)
    }
}

/// 测试用：环境变量 VOC_TEST_VOICE_LOSS=百分比，随机丢掉收到的语音包（只丢语音）
fn test_drop_voice() -> bool {
    static LOSS: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    static RNG: AtomicU64 = AtomicU64::new(0x1234_5678_9ABC_DEF1);
    let loss = *LOSS.get_or_init(|| std::env::var("VOC_TEST_VOICE_LOSS").ok().and_then(|v| v.parse::<f32>().ok()).map(|p| (p * 100.0) as u32).unwrap_or(0));
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
            v.insert(UserDecoder::new(dec))
        }
    };
    // 对方换了密钥（有人进出频道时每个人都会换）：计数器从 0 重新开始，序号检查跟着重来，
    // 否则新包会被当成旧包丢掉，最长要等对方再说同样久才恢复（0.5.1 的「两人互相听不到」）
    let (keep, rekeyed) = d.on_epoch(blob_epoch(blob));
    if !keep {
        return;
    }
    let Some(frames) = decode_with_concealment(d, seq, data, pcm) else { return };
    drop(decs);
    {
        // 测试日志用：这个人的语音一共放了多少帧、其中多少是补出来的（DRED / FEC / PLC）
        let mut r = state.voice_rx.lock().unwrap();
        let e = r.entry(from).or_default();
        e.0 += frames.len() as u32;
        e.1 += frames.len().saturating_sub(1) as u32;
    }
    let mut m = mixer.lock().unwrap();
    if rekeyed {
        m.restart_seq(from);
    }
    for (s, f) in frames {
        m.push(from, s, &f);
    }
    drop(m);
    state.last_heard.lock().unwrap().insert(from, Instant::now());
    if let Some(l) = state.current_link() {
        l.note_heard(from); // 语音优先：告诉服务器「我确实听到他了」
    }
}

// ------------------------------------------------------------ 建立链路

/// 建立加密 UDP 链路（含 P2P）并启动接收与定时线程
pub fn connect(
    server: SocketAddr,
    probe_port: u16,
    probe2: Option<SocketAddr>,
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
    let (alt_tx, alt_rx) = std::sync::mpsc::channel();
    let link = Arc::new(Link {
        alt_socks: Mutex::new(HashMap::new()),
        alt_active: AtomicBool::new(false),
        alt_tx: Mutex::new(alt_tx),
        alt_rx: Mutex::new(Some(alt_rx)),
        bday_rng: AtomicU64::new(now_millis() ^ 0x9E37_79B9_7F4A_7C15 ^ ((my_id as u64) << 32)),
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
        unknown_trials: RateLimit::default(),
        unknown_dropped: AtomicU32::new(0),
        others: Mutex::new(Vec::new()),
        watchers: Mutex::new(Vec::new()),
        watchers_ver: AtomicU32::new(0),
        watchers_low: Mutex::new(Vec::new()),
        watcher_routes: Mutex::new((Vec::new(), Vec::new())),
        watchers_cursor: Mutex::new(Vec::new()),
        budget_bps: AtomicU32::new(DEFAULT_BUDGET_BPS),
        video_bps: AtomicU32::new(0),
        alloc: Mutex::new((Vec::new(), Vec::new())),
        share_alloc: Mutex::new((Vec::new(), Vec::new())),
        last_loss_burst: Mutex::new(None),
        ttl_lock: std::sync::RwLock::new(()),
        last_local: Mutex::new(local_candidates(port, dual)),
        interleave: AtomicBool::new(false),
        mtu_probes: Mutex::new(HashMap::new()),
        path_mtu: Mutex::new(HashMap::new()),
        max_sent: AtomicU32::new(0),
        mtu_seq: AtomicU64::new(1),
        held_video: Mutex::new(None),
        pcp_peer_gw: Mutex::new(None),
        pcp_peer_tried: Mutex::new(Default::default()),
        server_rtt: AtomicU32::new(0),
        relay_enabled: AtomicBool::new(true),
        relays: Mutex::new(Vec::new()),
        video_relays: Mutex::new(Vec::new()),
        last_stats: Mutex::new(Instant::now()),
        pacer: Pacer::default(),
        relay_cache: Mutex::new(VecDeque::new()),
        music_seq: AtomicU16::new(0),
        share_seq: AtomicU16::new(0),
        pace_dropped: AtomicU32::new(0),
        pace_dropped_sum: AtomicU32::new(0),
        fec_pct: AtomicU32::new(0),
        portmap: Mutex::new(crate::portmap::PortMapStatus::Trying),
        lan_addrs: local_candidates(port, dual).iter().filter_map(|a| a.parse().ok()).collect(),
        local_port: port,
        created: Instant::now(),
        public_addr: Mutex::new(None),
        probe: (probe_port != 0).then(|| norm(SocketAddr::new(server.ip(), probe_port))),
        probe_addr: Mutex::new(None),
        probe2,
        probe2_addr: Mutex::new(None),
        filter_open: Mutex::new(None),
        nat: Mutex::new(None),
        srv_pings: Mutex::new(std::collections::VecDeque::new()),
        sent_bytes: AtomicU64::new(0),
        up_history: Mutex::new(std::collections::VecDeque::new()),
        route_prefs: Mutex::new(HashMap::new()),
        server_fallback: Mutex::new(Default::default()),
        heard: Mutex::new(HashMap::new()),
        last_voice_sent: Mutex::new(None),
        srv_voice_loss: Mutex::new(HashMap::new()),
        srv_rtt_hist: Mutex::new(VecDeque::new()),
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
        let mut last_full = Instant::now() - Duration::from_secs(1);
        while !l.closed.load(Ordering::Relaxed) {
            // 刚开始打洞的头 150 ms 每 20 ms 补打一轮（第 49 项：网络好时几十毫秒就通，不用等 200 ms 一轮）
            let burst = l.punch_burst(&mut buf);
            let since = last_full.elapsed();
            if since < Duration::from_millis(100) {
                let rest = Duration::from_millis(100) - since;
                std::thread::sleep(if burst { rest.min(Duration::from_millis(20)) } else { rest });
                continue;
            }
            last_full = Instant::now();
            // 每分钟探一次各条线路能走多大的包（第 37-4 项）：服务器那条，和每个直连的人
            if n % 600 == 50 {
                l.probe_mtu(l.server, None);
                let direct: Vec<(u32, SocketAddr)> = l.peers.lock().unwrap().map.iter().filter(|(_, p)| p.confirmed).filter_map(|(id, p)| p.path.map(|a| (*id, a))).collect();
                for (id, a) in direct {
                    l.probe_mtu(a, Some(id));
                }
            }
            // 每 3 秒看一次本机地址：换了网络（Wi-Fi ↔ 热点 / 有线）就马上重新探测、重新打洞（第 37-2 项）
            if n % 30 == 15 {
                let now_local = local_candidates(l.local_port, l.dual);
                let changed = {
                    let mut last = l.last_local.lock().unwrap();
                    let changed = !now_local.is_empty() && *last != now_local;
                    if changed {
                        *last = now_local.clone();
                    }
                    changed
                };
                if changed {
                    l.on_network_change(now_local);
                    spawn_upnp(l.clone(), l.local_port);
                    n = 0; // 重新快速探测 NAT
                }
            }
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
                        // 第二个探测点：看换了对方 IP 映射变不变；再请探测端口从第二个 IP 回，看没联系过的地址能不能进来（第 39-2 项）
                        if let Some(p2) = l.probe2 {
                            l.sealer.lock().unwrap().seal(udp::PING, &udp::ping_payload(ts), &mut buf);
                            l.send_raw(&buf, p2);
                            if l.filter_open.lock().unwrap().is_none() {
                                l.sealer.lock().unwrap().seal(udp::PING, &udp::filter_test_payload(ts), &mut buf);
                                l.send_raw(&buf, probe);
                                if l.created.elapsed() > Duration::from_secs(8) {
                                    *l.filter_open.lock().unwrap() = Some(false);
                                }
                            }
                        }
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
        let mut delayed = TestNet::start(&recv_sock, l.clone());
        let mut held = Vec::new();
        let alt_rx = l.alt_rx.lock().unwrap().take();
        let mut alt_mode = false;
        // 三人测试：经服务器转来、要晚一点处理的包
        let mut srv_q: std::collections::BinaryHeap<std::cmp::Reverse<(Instant, u64, u8, Vec<u8>)>> = Default::default();
        let (mut srv_seq, mut srv_rng) = (0u64, 0x1357_9BDF_2468_ACE1u64);
        while !l.closed.load(Ordering::Relaxed) {
            while srv_q.peek().is_some_and(|r| r.0 .0 <= Instant::now()) {
                let std::cmp::Reverse((_, _, kind, data)) = srv_q.pop().unwrap();
                match kind {
                    udp::VOICE => {
                        l.note_server_voice(&data);
                        on_server_voice(&data, &state, &mixer, &mut pcm)
                    }
                    udp::VIDEO => on_server_video(&data, &state, false),
                    udp::MUSIC | udp::SHARE_AUDIO => on_stream_audio(kind, &data, &state, &mixer, &mut pcm),
                    udp::CURSOR => on_cursor(&data, &state),
                    _ => {}
                }
            }
            // 有备用端口在用（生日攻击打通的对端）：主端口等包的时间缩短，备用端口转来的包不用干等
            let active = l.alt_active.load(Ordering::Relaxed);
            if active != alt_mode {
                alt_mode = active;
                let _ = recv_sock.set_read_timeout(Some(Duration::from_millis(if active { 5 } else { 200 })));
            }
            let alt = alt_rx.as_ref().and_then(|r| r.try_recv().ok());
            // 超时 / Windows 的 ConnectionReset（打洞时常见）都忽略
            let (n, from) = match (alt, delayed.as_mut()) {
                (Some((f, p)), _) if p.len() <= buf.len() => {
                    buf[..p.len()].copy_from_slice(&p);
                    (p.len(), f)
                }
                (Some(_), _) => continue,
                (None, Some(d)) => match d.next(&mut held) {
                    Some(f) => {
                        buf[..held.len()].copy_from_slice(&held);
                        (held.len(), f)
                    }
                    None => continue,
                },
                (None, None) => {
                    let Ok(r) = recv_sock.recv_from(&mut buf) else { continue };
                    r
                }
            };
            let from = norm(from);
            let pkt = &buf[..n];
            RECV_PKTS.fetch_add(1, Ordering::Relaxed);
            let _timer = RecvTimer(Instant::now());
            if Some(from) == l.probe {
                if let Some((udp::PONG, data)) = opener.open(pkt) {
                    if let Some((_, Some(addr))) = udp::read_pong(&data) {
                        *l.probe_addr.lock().unwrap() = Some(addr);
                        l.update_nat();
                    }
                }
                continue;
            }
            if l.probe2.is_some_and(|p| p.ip() == from.ip()) {
                if let Some((udp::PONG, data)) = opener.open(pkt) {
                    if data.get(8) == Some(&udp::FILTER_TEST) {
                        // 没往第二个 IP 发过过滤测试却收到了它的回包：我的 NAT 对没联系过的地址也放行
                        *l.filter_open.lock().unwrap() = Some(true);
                    } else if let Some((_, Some(addr))) = udp::read_pong(&data) {
                        *l.probe2_addr.lock().unwrap() = Some(addr);
                        l.update_nat();
                    }
                }
                continue;
            }
            if from == l.server {
                let Some((kind, data)) = opener.open(pkt) else { continue };
                count_rx(kind, data.len());
                // 三人测试：经服务器转来的某人的语音 / 画面也按他的网络条件丢 / 晚到
                if test_server_path_rules() && matches!(kind, udp::VOICE | udp::VIDEO | udp::MUSIC | udp::SHARE_AUDIO | udp::CURSOR) {
                    let sender = data.get(..4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
                    if let Some(r) = sender.and_then(|id| l.test_rule_for_id(id)) {
                        srv_rng ^= srv_rng << 13;
                        srv_rng ^= srv_rng >> 7;
                        srv_rng ^= srv_rng << 17;
                        let rnd = (srv_rng % 10_000) as f32 / 10_000.0;
                        if rnd * 100.0 >= r.loss {
                            let jitter = Duration::from_millis(((srv_rng >> 20) % (r.jitter_ms as u64 + 1)) as u64);
                            srv_seq += 1;
                            srv_q.push(std::cmp::Reverse((Instant::now() + r.delay + jitter, srv_seq, kind, data)));
                        }
                        continue;
                    }
                }
                match kind {
                    udp::PONG if udp::is_mtu_probe(&data) => l.on_mtu_reply(&data),
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
                        {
                            let mut h = l.srv_rtt_hist.lock().unwrap();
                            h.push_back((Instant::now(), rtt));
                            while h.len() > 30 {
                                h.pop_front();
                            }
                        }
                    }
                    udp::VOICE if test_drop_voice() => {}
                    udp::VOICE => {
                        l.note_server_voice(&data);
                        on_server_voice(&data, &state, &mixer, &mut pcm)
                    }
                    udp::VIDEO if test_drop_video() => {}
                    udp::VIDEO => on_server_video(&data, &state, false),
                    udp::NACK => l.resend_video(None, &data),
                    udp::MUSIC | udp::SHARE_AUDIO => on_stream_audio(kind, &data, &state, &mixer, &mut pcm),
                    udp::CURSOR => on_cursor(&data, &state),
                    _ => {}
                }
            } else if let Some((peer, kind, v)) = l.on_peer_packet(from, pkt, &mut out) {
                count_rx(kind, v.len());
                match kind {
                    udp::VIDEO if test_drop_video() => {}
                    udp::VIDEO => {
                        l.relay_video(peer, &v, &mut out);
                        on_server_video(&v, &state, true);
                    }
                    udp::NACK => l.resend_video(Some(peer), &v),
                    udp::MUSIC | udp::SHARE_AUDIO => on_stream_audio(kind, &v, &state, &mixer, &mut pcm),
                    udp::CURSOR => on_cursor(&v, &state),
                    udp::VOICE if test_drop_voice() => {}
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

/// 匀速发送队列（0.6）：每个目标（观看者 / 服务器）一个队列，按字节轮流发（亏额轮询 DRR，第 42-2 项）：
/// 某个观看者一下子要很多补发时，别人的实时画面不用排在整批补发后面
/// 排队的画面包：存明文，真正发出时才加密（0.6.1）。
/// 以前是先加密再排队：关键帧一下子排进几百个包时，后加密的语音 / ping / 补发请求不排队、先到，
/// 接收端（服务器 / 对端）的防重放窗口只有 64 个编号，排在后面的画面包就被当成重放悄悄丢掉
/// （语音也走服务器时，每个关键帧都丢几十上百个包）。现在加密编号按真正发出的顺序递增
struct Paced {
    seal: SealFor,
    kind: u8,
    data: Vec<u8>,
}

/// 用谁的密钥加密：服务器，或者直连的某个队友（发的时候按他当前的线路发）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SealFor {
    Server,
    Peer(u32),
}

#[derive(Default)]
struct PaceQueue {
    queues: HashMap<SocketAddr, VecDeque<(Instant, Paced)>>,
    /// 轮到谁（有包的目标按到达顺序轮）
    order: VecDeque<SocketAddr>,
    deficit: HashMap<SocketAddr, usize>,
    /// 队列里一共多少字节
    bytes: usize,
}

impl PaceQueue {
    fn push(&mut self, to: SocketAddr, pkt: Paced, now: Instant) {
        self.bytes += pkt.data.len();
        let q = self.queues.entry(to).or_default();
        if q.is_empty() && !self.order.contains(&to) {
            self.order.push_back(to);
        }
        q.push_back((now, pkt));
    }

    /// 按 DRR 取下一个要发的包；过期的（排太久的）丢掉并计数
    fn pop(&mut self, now: Instant, dropped: &mut u32) -> Option<(SocketAddr, Paced)> {
        let mut spins = 0;
        while let Some(to) = self.order.front().copied() {
            let q = self.queues.entry(to).or_default();
            while q.front().is_some_and(|(t, _)| now.duration_since(*t) > PACE_STALE) {
                let (_, p) = q.pop_front().unwrap();
                self.bytes -= p.data.len();
                *dropped += 1;
            }
            let Some(len) = q.front().map(|(_, p)| p.data.len()) else {
                self.order.pop_front();
                self.deficit.remove(&to);
                continue;
            };
            let d = self.deficit.entry(to).or_insert(0);
            if *d < len {
                // 这一轮的额度不够：加一份额度，轮到下一个
                *d += PACE_QUANTUM;
                self.order.rotate_left(1);
                spins += 1;
                if spins > self.order.len() * 4 + 4 {
                    *d = len; // 保险：一个包比额度大很多时也能发出去
                }
                continue;
            }
            *d -= len;
            let (_, p) = q.pop_front().unwrap();
            self.bytes -= p.data.len();
            return Some((to, p));
        }
        None
    }

    /// 有包在等的目标数
    fn active(&self) -> usize {
        self.order.len()
    }
}

#[derive(Default)]
struct Pacer {
    q: Mutex<PaceQueue>,
    wake: std::sync::Condvar,
    running: AtomicBool,
}

/// 帮人转发画面时留多久、最多多少包（就近补发用）
const RELAY_CACHE_TTL: Duration = Duration::from_millis(1500);
const RELAY_CACHE_MAX: usize = 4000;

/// DRR 每轮给每个目标的额度（约一个包）
const PACE_QUANTUM: usize = 1200;
/// 匀速队列最多攒多少秒的量（按当前限速算），再多就丢
const PACE_QUEUE_SECS: f64 = 0.3;
const PACE_QUEUE_MIN: usize = 256 * 1024;

/// 往返时间在涨：最近 2 秒的平均比最近 10 秒的最小值高出 max(30 ms, 一半)。排队延迟变大是拥塞的早期信号（还没丢包）
fn rtt_rising(samples: impl Iterator<Item = (Instant, u32)>, now: Instant) -> bool {
    let (mut base, mut sum, mut n) = (u32::MAX, 0u32, 0u32);
    for (t, rtt) in samples {
        let age = now.duration_since(t);
        if age <= Duration::from_secs(10) {
            base = base.min(rtt);
        }
        if age <= Duration::from_secs(2) {
            sum += rtt;
            n += 1;
        }
    }
    n >= 2 && base != u32::MAX && sum / n > base + (base / 2).max(30)
}

impl Link {
    /// 拥塞判断（第 40-2 项）：画面走的路往返时间在涨，或者匀速发送队列积了很多。
    /// 拥塞时要降码率 / 控速，不能再加冗余（冗余也占带宽，越加越堵）；延迟稳但有丢包才是该加冗余的突发丢包
    pub fn congested(&self) -> bool {
        let now = Instant::now();
        // 发送队列里最老的包排了多久：关键帧一下子进来很多是正常的（几十毫秒就发完），排超过 150 ms 才是真的发不动
        let oldest = {
            let q = self.pacer.q.lock().unwrap();
            q.queues.values().filter_map(|v| v.front().map(|x| x.0)).min()
        };
        if oldest.is_some_and(|t| now.duration_since(t) > Duration::from_millis(150)) {
            return true;
        }
        let video = self.alloc.lock().unwrap().1.clone();
        let peers = self.peers.lock().unwrap();
        if video.iter().filter_map(|i| peers.map.get(i)).any(|p| rtt_rising(p.pings.iter().filter_map(|x| x.rtt.map(|r| (x.sent, r))), now)) {
            return true;
        }
        drop(peers);
        // 有观看者经服务器看：看到服务器的往返
        let all_direct = self.watchers.lock().unwrap().iter().all(|w| video.contains(w));
        !all_direct && rtt_rising(self.srv_rtt_hist.lock().unwrap().iter().copied(), now)
    }

    /// 匀速发送的总速度：一路画面码率 × 倍数 × 正在发的目标数（0.5.x 不管几个观看者都按一路算，人一多就排队过期，F-07）
    fn pace_rate(&self) -> f64 {
        let (dests, queued) = {
            let q = self.pacer.q.lock().unwrap();
            (q.active(), q.bytes)
        };
        pace_rate_bps(self.video_bps.load(Ordering::Relaxed), self.fec_pct.load(Ordering::Relaxed), dests, queued, VIDEO_FPS.load(Ordering::Relaxed))
    }

    /// 共享者每秒告诉链路当前的冗余比例（%），匀速发送按「码率 ×（1 + 冗余）」算
    pub fn set_fec_pct(&self, pct: u32) {
        self.fec_pct.store(pct.min(400), Ordering::Relaxed);
    }
}

/// 屏幕画面匀速发送：令牌桶限速 + 按目标轮流，过期的包丢掉。语音不经过这里，永远先发。
fn spawn_pacer(link: Arc<Link>) {
    link.pacer.running.store(true, Ordering::Relaxed);
    let _ = std::thread::Builder::new().name("video-pacer".into()).spawn(move || {
        let mut tokens = PACE_BURST as f64;
        let mut last = Instant::now();
        let mut buf = Vec::with_capacity(1500);
        while !link.closed.load(Ordering::Relaxed) {
            let next = {
                let mut q = link.pacer.q.lock().unwrap();
                let mut dropped = 0;
                let mut got = q.pop(Instant::now(), &mut dropped);
                if got.is_none() {
                    // 有交错留着的帧时醒得勤一点，画面不动了也能及时发出去
                    let wait = if link.held_video.lock().unwrap().is_some() { 20 } else { 200 };
                    q = link.pacer.wake.wait_timeout(q, Duration::from_millis(wait)).unwrap().0;
                    got = q.pop(Instant::now(), &mut dropped);
                }
                if dropped > 0 {
                    link.pace_dropped.fetch_add(dropped, Ordering::Relaxed);
                }
                got
            };
            let Some((to, pkt)) = next else {
                link.flush_held_video(Duration::from_millis(60));
                continue;
            };
            // 加密后的大小（明文 + 编号 8 + 类型长度 3 + 随机填充平均 12 + 认证标签 16）
            let len = (pkt.data.len() + 39) as f64;
            let rate = link.pace_rate() / 8.0;
            let now = Instant::now();
            tokens = (tokens + now.duration_since(last).as_secs_f64() * rate).min(PACE_BURST as f64);
            last = now;
            if tokens < len {
                let wait = (len - tokens) / rate;
                std::thread::sleep(Duration::from_secs_f64(wait));
                let now = Instant::now();
                tokens += now.duration_since(last).as_secs_f64() * rate;
                last = now;
            }
            tokens -= len;
            link.send_sealed(to, &pkt, &mut buf);
        }
        link.pacer.running.store(false, Ordering::Relaxed);
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
            // 第 39-3 项：网关会说 PCP、只是不给 MAP → 记下来，打洞打不通时给那个人单独要 PEER 映射
            if let Some(gw) = portmap::gateway_for(*lan.ip()) {
                if let Some(code) = portmap::pcp_answers(gw, *lan.ip(), port) {
                    crate::applog::log(format!("端口映射：网关支持 PCP 但不给普通映射（结果码 {code}），打不通时改试 PCP PEER"));
                    *link.pcp_peer_gw.lock().unwrap() = Some((gw, *lan.ip(), port));
                }
            }
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
            // 测试：VOC_TEST_TONE=1 → 不用麦克风，每 20 ms 送一帧有起伏的声音（一直在说话），测语音线路用
            if std::env::var_os("VOC_TEST_TONE").is_some() {
                let mut next = Instant::now();
                let mut t = 0u64;
                loop {
                    let f: Vec<f32> = (0..FRAME_SAMPLES)
                        .map(|i| {
                            let n = (t * FRAME_SAMPLES as u64 + i as u64) as f32 / SAMPLE_RATE as f32;
                            0.2 * (2.0 * std::f32::consts::PI * 220.0 * n).sin() * (0.6 + 0.4 * (2.0 * std::f32::consts::PI * 3.0 * n).sin())
                        })
                        .collect();
                    let flen = if VOICE_10MS.load(Ordering::Relaxed) { FRAME_SAMPLES / 2 } else { FRAME_SAMPLES };
                    for c in f.chunks(flen) {
                        tx.on_frame(c);
                    }
                    t += 1;
                    next += Duration::from_millis(20);
                    let now = Instant::now();
                    if next > now {
                        std::thread::sleep(next - now);
                    }
                }
            }
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
                // 10 ms 一包（第 36-3 项）还是 20 ms 一包
                let flen = if VOICE_10MS.load(Ordering::Relaxed) { FRAME_SAMPLES / 2 } else { FRAME_SAMPLES };
                while acc.len() >= flen {
                    tx.on_frame(&acc[..flen]);
                    acc.drain(..flen);
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
    /// 连续多久（ms）完全没有声音（全 0）
    zero_ms: u32,
    /// 最近发出的几包（带冗余时顺带塞进下一包），停止说话就清空
    recent: std::collections::VecDeque<Vec<u8>>,
    /// 现在每包顺带几份旧包（0 / 1 / 2）
    red_copies: usize,
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
            zero_ms: 0,
            recent: std::collections::VecDeque::new(),
            red_copies: 0,
            loss_mode: 0,
            loss_checked: Instant::now(),
        }
    }

    /// frame：10 ms（480）或 20 ms（960）
    fn on_frame(&mut self, frame: &[f32]) {
        let s = &self.settings;
        let frame_ms = (frame.len() * 1000 / SAMPLE_RATE as usize) as u32;
        self.pcm.resize(frame.len(), 0);
        let mut energy = 0.0f32;
        for (o, &v) in self.pcm.iter_mut().zip(frame) {
            energy += v * v;
            *o = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
        }
        let rms = (energy / frame.len().max(1) as f32).sqrt();
        let db = 20.0 * (rms + 1e-9).log10();
        s.input_level_db.set(db);
        // 麦克风连续 5 秒给的全是 0：多半是 Windows 隐私设置不让 VOC 用麦克风，或设备坏了（真实环境总有底噪）
        self.zero_ms = if energy < 1e-12 { self.zero_ms.saturating_add(frame_ms) } else { 0 };
        s.mic_silent.store(self.zero_ms >= MIC_SILENT_FRAMES * 20, Ordering::Relaxed);

        let ptt = s.ptt_vk.load(Ordering::Relaxed);
        let speaking = if ptt != 0 { key_down(ptt) } else { db >= s.vad_threshold_db.get() };
        if speaking {
            // 拖尾按毫秒算（10 ms / 20 ms 一包都一样长）
            self.hang = if ptt != 0 { PTT_HANGOVER } else { VAD_HANGOVER } * 20;
        } else {
            self.hang = self.hang.saturating_sub(frame_ms);
        }
        let muted = s.muted.load(Ordering::Relaxed) || s.deafened.load(Ordering::Relaxed);
        let gate = self.hang > 0 && !muted;
        s.transmitting.store(gate, Ordering::Relaxed);
        if !gate {
            self.recent.clear();
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
            let link = self.state.current_link();
            let loss = link.as_ref().map(|l| l.voice_path_loss()).unwrap_or(0);
            if loss >= 3 {
                if let Some(l) = &link {
                    l.note_loss_burst();
                }
            }
            // 发出方向丢包高、大家都认识：前一两帧顺带再发（第 34-3 项；和 DRED 一起用，DRED 是压缩过的近似，这个是原包）
            self.red_copies = if !VOICE_RED_OK.load(Ordering::Relaxed) {
                0
            } else if loss >= RED_LOSS_2 {
                2
            } else if loss >= RED_LOSS_1 {
                1
            } else {
                0
            };
            // 长距离、最近有过突发丢包：丢包还没起来就先开 DRED（第 38-8 项）
            let mode = if loss >= DRED_LOSS_PCT {
                loss
            } else if link.is_some_and(|l| l.long_haul()) {
                DRED_LOSS_PCT
            } else {
                0
            };
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
                let k = self.red_copies.min(self.recent.len());
                let enc = if k > 0 {
                    let older: Vec<&[u8]> = self.recent.iter().skip(self.recent.len() - k).map(|v| v.as_slice()).collect();
                    e2e.lock().unwrap().encrypt_frame(&udp::voice_red(packet, &older))
                } else {
                    e2e.lock().unwrap().encrypt_frame(packet)
                };
                l.send_voice(&enc, &mut self.pkt);
            }
        }
        self.recent.push_back(packet.to_vec());
        if self.recent.len() > 2 {
            self.recent.pop_front();
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
mod congestion_tests {
    use super::rtt_rising;
    use std::time::{Duration, Instant};

    #[test]
    fn rising_rtt_means_queueing() {
        let now = Instant::now();
        let at = |secs_ago: f32, rtt: u32| (now - Duration::from_secs_f32(secs_ago), rtt);
        // 稳定 40 ms：不算
        assert!(!rtt_rising((0..20).map(|i| at(i as f32 * 0.5, 40)), now));
        // 抖一下（一个 70）：不算
        assert!(!rtt_rising([at(9.0, 40), at(5.0, 40), at(1.5, 40), at(0.5, 70)].into_iter(), now));
        // 最近 2 秒涨到 90+（比最低多 50 以上）：排队了
        assert!(rtt_rising([at(9.0, 40), at(5.0, 42), at(1.5, 95), at(0.5, 110)].into_iter(), now));
        // 高延迟线路（基线 200）：要涨一半以上才算
        assert!(!rtt_rising([at(8.0, 200), at(1.0, 260), at(0.2, 270)].into_iter(), now));
        assert!(rtt_rising([at(8.0, 200), at(1.0, 320), at(0.2, 330)].into_iter(), now));
    }
}

#[cfg(test)]
mod testnet_tests {
    use super::{TestNet, TestNetCfg};
    use std::net::SocketAddr;
    use std::time::{Duration, Instant};

    #[test]
    fn burst_loss_rate_and_bandwidth_cap() {
        let a: SocketAddr = "1.1.1.1:1".parse().unwrap();
        // 突发丢包：长期 30%，平均连丢 4 个
        let (_tx, rx) = std::sync::mpsc::channel();
        let mut n = TestNet::new(rx, TestNetCfg { loss_pct: 30.0, burst: 4.0, ..Default::default() });
        let t = Instant::now();
        let (mut kept, mut runs, mut prev_lost) = (0, 0, false);
        for _ in 0..20_000 {
            let before = n.queue.len();
            n.admit(t, a, vec![0; 100]);
            let lost = n.queue.len() == before;
            if lost && !prev_lost {
                runs += 1;
            }
            prev_lost = lost;
            kept += usize::from(!lost);
        }
        let loss = 1.0 - kept as f32 / 20_000.0;
        assert!((0.25..0.35).contains(&loss), "loss {loss}");
        let avg_burst = (20_000 - kept) as f32 / runs as f32;
        assert!((3.0..5.0).contains(&avg_burst), "burst {avg_burst}");
        // 1 Mbps 下行：瞬间来 1 MB，只能排 300 ms 的量（约 37 KB），其余丢掉
        let (_tx, rx) = std::sync::mpsc::channel();
        let mut n = TestNet::new(rx, TestNetCfg { bw_kbps: 1000, ..Default::default() });
        for _ in 0..1000 {
            n.admit(t, a, vec![0; 1000]);
        }
        assert!((30..45).contains(&n.queue.len()), "queued {}", n.queue.len());
        let last = n.queue.iter().map(|r| r.0 .0).max().unwrap();
        assert!(last.duration_since(t) > Duration::from_millis(250));
    }
}

#[cfg(test)]
mod pace_tests {
    use super::{PaceQueue, Paced, SealFor, PACE_STALE};
    use std::net::SocketAddr;
    use std::time::{Duration, Instant};

    /// 一个观看者一下子要了 100 个补发包，另一个观看者的实时画面不用等它们发完
    #[test]
    fn drr_interleaves_destinations() {
        let (a, b): (SocketAddr, SocketAddr) = ("1.1.1.1:1".parse().unwrap(), "2.2.2.2:2".parse().unwrap());
        let mut q = PaceQueue::default();
        let t = Instant::now();
        let pk = |x: u8| Paced { seal: SealFor::Server, kind: 0, data: vec![x; 1100] };
        for _ in 0..100 {
            q.push(a, pk(0), t);
        }
        for _ in 0..3 {
            q.push(b, pk(1), t);
        }
        let mut dropped = 0;
        let first: Vec<SocketAddr> = (0..8).map(|_| q.pop(t, &mut dropped).unwrap().0).collect();
        assert!(first.iter().filter(|x| **x == b).count() >= 3, "b's packets go out early: {first:?}");
        assert_eq!(q.bytes, (100 + 3 - 8) * 1100);
        // 排太久的丢掉、计数
        let late = t + PACE_STALE + Duration::from_millis(1);
        while q.pop(late, &mut dropped).is_some() {}
        assert_eq!(q.bytes, 0);
        assert_eq!(dropped, 95);
    }
}

#[cfg(test)]
mod punch_target_tests {
    use super::{addr_class, better_path, port_preservation_target, punch_targets};
    use std::net::SocketAddr;

    /// ICE 式选路：局域网 > IPv6 > IPv4 公网 > 运营商内网；同类只在明显更快时换，不来回跳
    #[test]
    fn path_priority() {
        let a = |s: &str| s.parse::<SocketAddr>().unwrap();
        assert!(addr_class(&a("192.168.1.5:1")) < addr_class(&a("[2408::1]:1")));
        assert!(addr_class(&a("[2408::1]:1")) < addr_class(&a("1.2.3.4:1")));
        assert!(addr_class(&a("1.2.3.4:1")) < addr_class(&a("100.70.1.2:1")));
        let v4 = a("1.2.3.4:5000");
        assert!(better_path(None, v4, 50), "first path");
        assert!(better_path(Some((v4, 50)), a("192.168.1.5:5000"), 60), "LAN wins even if a bit slower");
        assert!(better_path(Some((v4, 50)), a("[2408::1]:5000"), 55), "IPv6 beats IPv4");
        assert!(!better_path(Some((a("[2408::1]:5000"), 40)), v4, 30), "IPv4 does not replace IPv6");
        assert!(!better_path(Some((v4, 50)), a("5.6.7.8:5000"), 45), "same class, only slightly faster: keep");
        assert!(better_path(Some((v4, 100)), a("5.6.7.8:5000"), 60), "same class, much faster: switch");
        assert!(!better_path(Some((v4, 50)), v4, 10), "same address");
    }

    #[test]
    fn port_preservation_and_remembered_paths() {
        let a = |s: &str| s.parse::<SocketAddr>().unwrap();
        // 公网 1.2.3.4:50001（NAT 换了端口），局域网 192.168.1.5:40000 → 多试 1.2.3.4:40000
        let cands = vec![a("1.2.3.4:50001"), a("192.168.1.5:40000")];
        assert_eq!(port_preservation_target(&cands), Some(a("1.2.3.4:40000")));
        // NAT 本来就保持端口：和已有候选重复，不再加
        assert_eq!(port_preservation_target(&[a("1.2.3.4:40000"), a("192.168.1.5:40000")]), None);
        // 没有局域网地址：没法猜
        assert_eq!(port_preservation_target(&[a("1.2.3.4:50001")]), None);
        // 运营商大内网地址（100.64/10）算内网
        assert_eq!(port_preservation_target(&[a("1.2.3.4:50001"), a("100.70.1.2:41000")]), Some(a("1.2.3.4:41000")));
        // 目标：候选 + 刚断的路 + 记住的 + 端口保持，去重
        let t = punch_targets(&cands, None, Some(a("5.6.7.8:9000")), &[a("1.2.3.4:50001"), a("9.9.9.9:1000")]);
        assert_eq!(t, vec![a("1.2.3.4:50001"), a("192.168.1.5:40000"), a("5.6.7.8:9000"), a("9.9.9.9:1000"), a("1.2.3.4:40000")]);
    }
}

#[cfg(test)]
mod rate_tests {
    use super::{RateLimit, UNKNOWN_TRIALS_PER_SEC};
    use std::time::{Duration, Instant};

    #[test]
    fn unknown_source_trials_are_limited() {
        let r = RateLimit::default();
        let t = Instant::now();
        let ok = (0..200).filter(|_| r.allow(t)).count() as u32;
        assert_eq!(ok, UNKNOWN_TRIALS_PER_SEC);
        // 下一秒重新放行
        assert!(r.allow(t + Duration::from_millis(1001)));
    }
}

#[cfg(test)]
mod epoch_tests {
    use super::{decode_with_concealment, UserDecoder};
    use voc_proto::{FRAME_SAMPLES, SAMPLE_RATE};

    fn packets(n: usize) -> Vec<Vec<u8>> {
        let mut enc = crate::opusx::Encoder::new_voip(SAMPLE_RATE).unwrap();
        let pcm = vec![0i16; FRAME_SAMPLES];
        (0..n)
            .map(|_| {
                let mut out = vec![0u8; 1500];
                let len = enc.encode(&pcm, &mut out).unwrap();
                out.truncate(len);
                out
            })
            .collect()
    }

    /// 第 36-3 / 34-3 项：10 ms 一包，丢了第 3 包，第 4 包里顺带了它 → 原样还原（和没丢时解出来的一样），补出来的长度也是 10 ms
    #[test]
    fn red_recovers_lost_10ms_packet() {
        let mut enc = crate::opusx::Encoder::new_voip(SAMPLE_RATE).unwrap();
        let pk: Vec<Vec<u8>> = (0..6)
            .map(|i| {
                let pcm: Vec<i16> = (0..FRAME_SAMPLES / 2).map(|j| (((i * 480 + j) as f32 * 0.07).sin() * 9000.0) as i16).collect();
                let mut out = vec![0u8; 1500];
                let n = enc.encode(&pcm, &mut out).unwrap();
                out.truncate(n);
                out
            })
            .collect();
        // 参考：全部收到
        let mut d0 = UserDecoder::new(crate::opusx::Decoder::new(SAMPLE_RATE).unwrap());
        let mut pcm = Vec::new();
        let full: Vec<Vec<i16>> = (0..6).map(|i| decode_with_concealment(&mut d0, i as u16, &pk[i], &mut pcm).unwrap().pop().unwrap().1).collect();
        assert!(full.iter().all(|f| f.len() == FRAME_SAMPLES / 2));
        // 丢了第 3 包（序号 2），第 4 包带着第 2、3 包
        let mut d = UserDecoder::new(crate::opusx::Decoder::new(SAMPLE_RATE).unwrap());
        for i in 0..2 {
            decode_with_concealment(&mut d, i as u16, &pk[i], &mut pcm).unwrap();
        }
        let red = voc_proto::udp::voice_red(&pk[3], &[&pk[1], &pk[2]]);
        let got = decode_with_concealment(&mut d, 3, &red, &mut pcm).unwrap();
        assert_eq!(got.iter().map(|f| f.0).collect::<Vec<_>>(), vec![2, 3]);
        assert_eq!(got[0].1, full[2], "冗余里的原包还原出来和没丢时一样");
        assert_eq!(got[1].1, full[3]);
        // 没有冗余时：补出来的也是 10 ms（按对方的包长），不是 20 ms
        let mut d = UserDecoder::new(crate::opusx::Decoder::new(SAMPLE_RATE).unwrap());
        for i in 0..2 {
            decode_with_concealment(&mut d, i as u16, &pk[i], &mut pcm).unwrap();
        }
        let got = decode_with_concealment(&mut d, 3, &pk[3], &mut pcm).unwrap();
        assert_eq!(got[0].1.len(), FRAME_SAMPLES / 2);
    }

    /// 0.5.1 的「两人互相听不到」：A 说了一阵（序号到 3000），有人进频道 → A 换密钥、序号从 0 开始，
    /// B 这边不能把新包当成旧包丢掉
    #[test]
    fn rekey_restarts_sequence() {
        let pk = packets(4);
        let mut d = UserDecoder::new(crate::opusx::Decoder::new(SAMPLE_RATE).unwrap());
        let mut pcm = Vec::new();
        assert_eq!(d.on_epoch(100), (true, false));
        assert!(decode_with_concealment(&mut d, 2999, &pk[0], &mut pcm).is_some());
        assert!(decode_with_concealment(&mut d, 3000, &pk[1], &mut pcm).is_some());
        // 新密钥，计数器归零
        assert_eq!(d.on_epoch(101), (true, true));
        assert!(decode_with_concealment(&mut d, 0, &pk[2], &mut pcm).is_some(), "first packet of the new key must play");
        assert!(decode_with_concealment(&mut d, 1, &pk[3], &mut pcm).is_some());
        // 换密钥前还在路上的旧包：丢掉，不打乱新序号
        assert_eq!(d.on_epoch(100), (false, false));
        assert_eq!(d.on_epoch(101), (true, false));
        // 重复包仍然丢
        assert!(decode_with_concealment(&mut d, 1, &pk[3], &mut pcm).is_none());
    }

    /// 对方重连（新的随机密钥编号，可能「看起来更旧」）也照常播
    #[test]
    fn unseen_epoch_is_always_new() {
        let pk = packets(2);
        let mut d = UserDecoder::new(crate::opusx::Decoder::new(SAMPLE_RATE).unwrap());
        let mut pcm = Vec::new();
        d.on_epoch(40000);
        assert!(decode_with_concealment(&mut d, 500, &pk[0], &mut pcm).is_some());
        assert_eq!(d.on_epoch(7), (true, true));
        assert!(decode_with_concealment(&mut d, 0, &pk[1], &mut pcm).is_some());
    }

    /// 同一把密钥里序号倒退很多（不正常，但别一直静音）：当作重新开始
    #[test]
    fn far_backwards_sequence_restarts() {
        let pk = packets(3);
        let mut d = UserDecoder::new(crate::opusx::Decoder::new(SAMPLE_RATE).unwrap());
        let mut pcm = Vec::new();
        d.on_epoch(1);
        assert!(decode_with_concealment(&mut d, 5000, &pk[0], &mut pcm).is_some());
        assert!(decode_with_concealment(&mut d, 4990, &pk[1], &mut pcm).is_none(), "slightly old: dropped");
        assert!(decode_with_concealment(&mut d, 3, &pk[2], &mut pcm).is_some(), "far behind: restart");
    }
}

#[cfg(test)]
mod tests {
    /// 第 37-4 项：打开 DF 发完照常能发（本机回环 MTU 很大，几个大小都收得到）
    #[test]
    fn dont_fragment_probe_sends() {
        let (sock, dual) = super::make_socket("127.0.0.1:1".parse().unwrap()).unwrap();
        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(std::time::Duration::from_millis(500))).unwrap();
        let to = rx.local_addr().unwrap();
        let pkts: Vec<(Vec<u8>, std::net::SocketAddr)> = [1100usize, 1400, 1472].iter().map(|&n| (vec![7u8; n], to)).collect();
        super::send_dont_fragment(&sock, dual, &pkts);
        sock.send_to(b"after", super::wire(to, dual)).unwrap();
        let mut b = [0u8; 2000];
        let mut sizes = Vec::new();
        while let Ok((n, _)) = rx.recv_from(&mut b) {
            sizes.push(n);
        }
        assert_eq!(sizes, vec![1100, 1400, 1472, 5]);
        assert!(voc_proto::udp::is_mtu_probe(&voc_proto::udp::mtu_probe_payload(5, 1200)));
        assert!(!voc_proto::udp::is_mtu_probe(&voc_proto::udp::ping_payload(5)));
    }

    #[test]
    fn interleave_two_frames() {
        let f = |t: u8, n: usize| (0..n).map(|i| vec![t, i as u8]).collect::<Vec<_>>();
        let m = super::interleave_frags(&f(1, 3), &f(2, 2));
        assert_eq!(m, vec![vec![1, 0], vec![2, 0], vec![1, 1], vec![2, 1], vec![1, 2]]);
    }

    /// 第 35-2 项：低 TTL 发完 TTL 改回原样；本机回环上 TTL 2 的包照样收得到
    #[test]
    fn low_ttl_restores() {
        use std::net::UdpSocket;
        let (sock, dual) = super::make_socket("127.0.0.1:1".parse().unwrap()).unwrap();
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(std::time::Duration::from_millis(500))).unwrap();
        let sr = socket2::SockRef::from(&sock);
        let before = sr.ttl_v4().unwrap();
        super::send_low_ttl(&sock, dual, &[(b"hi".to_vec(), rx.local_addr().unwrap())], &[2, 3]).unwrap();
        assert_eq!(sr.ttl_v4().unwrap(), before, "TTL 改回去了");
        let mut b = [0u8; 8];
        let mut got = 0;
        while rx.recv_from(&mut b).is_ok() {
            got += 1;
        }
        assert_eq!(got, 2, "每个 TTL 发一遍");
    }

    /// 第 39-2 项：只换端口映射不变、换了 IP 才变的 NAT，有第二个探测点时判成对称型
    #[test]
    fn second_probe_finds_address_dependent_mapping() {
        use super::classify_nat3;
        let a = |s: &str| s.parse::<std::net::SocketAddr>().unwrap();
        let lan = [a("192.168.1.5:5000")];
        let main = a("1.2.3.4:40000");
        assert_eq!(classify_nat3(main, main, None, &lan).kind, voc_proto::NAT_CONE);
        assert_eq!(classify_nat3(main, main, Some(main), &lan).kind, voc_proto::NAT_CONE);
        let n = classify_nat3(main, main, Some(a("1.2.3.4:40002")), &lan);
        assert_eq!((n.kind, n.step), (voc_proto::NAT_SYM_PREDICTABLE, 2));
        assert_eq!(classify_nat3(main, main, Some(a("1.2.3.4:51234")), &lan).kind, voc_proto::NAT_SYM_RANDOM);
    }

    #[test]
    fn long_haul_detection() {
        use super::long_haul_unstable;
        use std::time::{Duration, Instant};
        let now = Instant::now();
        assert!(!long_haul_unstable(200, None, now), "没丢过包：不算");
        assert!(long_haul_unstable(200, Some(now - Duration::from_secs(60)), now));
        assert!(!long_haul_unstable(80, Some(now), now), "近距离：不算");
        assert!(!long_haul_unstable(200, Some(now - Duration::from_secs(400)), now), "很久没丢包：降回去");
    }

    /// 第 36-4 项：普通帧摊到一帧时间里（不低于 1.5 倍码率），大帧最多 3 倍，低码率时至少 4 Mbps
    #[test]
    fn pace_rate_spreads_frames() {
        use super::pace_rate_bps;
        // 20 Mbps、60 帧：一帧平均 41.7 KB。队列里正好一帧 → 1.5 倍（30 Mbps）就能在一帧时间内发完
        let r = pace_rate_bps(20_000_000, 0, 1, 41_700, 60);
        assert!((r - 30_000_000.0).abs() < 1.0, "{r}");
        // 关键帧（10 倍大）：封顶 3 倍
        assert_eq!(pace_rate_bps(20_000_000, 0, 1, 417_000, 60), 60_000_000.0);
        // 两帧的量：在 1.5～3 倍之间，一帧时间内发完
        let r = pace_rate_bps(20_000_000, 0, 1, 83_400, 60);
        assert!(r > 40_000_000.0 && r < 60_000_000.0, "{r}");
        // 冗余 100%、两个目标：按 4 倍的量算
        assert_eq!(pace_rate_bps(20_000_000, 100, 2, 10_000_000, 60), 240_000_000.0);
        // 低码率：至少 4 Mbps
        assert_eq!(pace_rate_bps(500_000, 0, 1, 0, 30), 4_000_000.0);
    }

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
            connect(dummy_server, 0, None, &keys, id, true, e2e, tx, st.clone(), mx).unwrap();
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
            connect(dummy_server, 0, None, &keys, id, true, e2e, tx, st.clone(), mx).unwrap();
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
            RelayRoute { from: 3, to: 7, via: 5, ms: 20, server_ms: 120, manual: false },
            RelayRoute { from: 7, to: 3, via: 5, ms: 20, server_ms: 120, manual: false },
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
            connect(dummy_server, 0, None, &keys, id, true, e2e, tx, st.clone(), mx).unwrap();
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
        connect(sink.local_addr().unwrap(), 0, None, &keys, 3, false, ea, tx, st.clone(), Arc::new(Mutex::new(Mixer::default()))).unwrap();
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
        connect(dummy_server, 0, None, &keys, 3, true, ea.clone(), tx, sa.clone(), Arc::new(Mutex::new(Mixer::default()))).unwrap();
        let (tx, _rx2) = mpsc::channel::<Outgoing>();
        let sb = Arc::new(VoiceState::default());
        let mb = Arc::new(Mutex::new(Mixer::default()));
        connect(dummy_server, 0, None, &keys, 7, true, eb, tx, sb.clone(), mb.clone()).unwrap();
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

    /// 生日攻击打通后的收发管道：和对方改用备用端口（撞上的那个）发；对方发到这个端口的包也能照常处理
    #[test]
    fn birthday_alt_socket_plumbing() {
        let wait = |what: &str, f: &dyn Fn() -> bool| {
            let t = Instant::now();
            while !f() {
                assert!(t.elapsed() < Duration::from_secs(5), "{what}");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let (la, sa, lb, sb, mb, ea) = music_pair();
        let b_addr = la.peers.lock().unwrap().map[&7].path.unwrap();
        // A 这边：和 B 改用一个新端口（模拟撞上的那个）
        let alt = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        let alt_addr = alt.local_addr().unwrap();
        la.alt_socks.lock().unwrap().insert(b_addr, alt.clone());
        la.alt_active.store(true, Ordering::Relaxed);
        la.spawn_alt_reader(alt);
        // A 发伴奏：从备用端口出去，B 照样收到（陌生地址试解成功后记住它）
        let mut enc = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Audio).unwrap();
        let mut out = vec![0u8; 1500];
        let n = enc.encode(&vec![0i16; FRAME_SAMPLES], &mut out).unwrap();
        let blob = ea.lock().unwrap().encrypt_stream(voc_proto::e2e::STREAM_MUSIC, &out[..n]);
        let mut pkt = Vec::new();
        la.send_stream(udp::MUSIC, &blob, &mut pkt);
        wait("music via the alt socket reaches B", &|| music_buffered(&mb, 3));
        assert!(lb.peers.lock().unwrap().map[&3].reflexive.contains(&alt_addr), "B learned A's new port");
        // B 往 A 的备用端口发 PING：A 的备用端口收到、主循环处理、从备用端口回 PONG，B 收到（算出往返）
        let mut buf = Vec::new();
        {
            let mut peers = lb.peers.lock().unwrap();
            let p = peers.map.get_mut(&3).unwrap();
            p.pings.clear();
            let ping = p.next_ping(Instant::now());
            p.sealer.as_mut().unwrap().seal(p2p::PING, &ping, &mut buf);
        }
        lb.send_raw(&buf, alt_addr);
        wait("pong back through the alt socket", &|| lb.peers.lock().unwrap().map[&3].pings.iter().any(|x| x.rtt.is_some()));
        sa.disconnect();
        sb.disconnect();
    }

    #[test]
    fn birthday_ports_are_distinct_and_in_range() {
        let rng = AtomicU64::new(12345);
        let mut sent = std::collections::HashSet::new();
        let mut all = Vec::new();
        for _ in 0..30 {
            all.extend(bday_ports(&rng, &mut sent, 100));
        }
        assert_eq!(all.len(), 3000);
        assert!(all.iter().all(|p| *p >= 1024));
        let uniq: std::collections::HashSet<u16> = all.iter().copied().collect();
        assert_eq!(uniq.len(), 3000, "no port probed twice");
        // 撞上的概率（按生日问题）：256 个映射、发 1024 个探测 ≈ 98%
        let p_hit = 1.0 - (1.0 - 256.0 / 64512.0f64).powi(1024);
        assert!(p_hit > 0.98, "{p_hit}");
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
        let hide = PeerPref { hide_ip: true, ..Default::default() };
        la.set_route_prefs(HashMap::from([(7, hide)]));
        wait("hide-ip drops direct", &|| !la.allocation().0.contains(&7));
        assert!(!la.punch_states().contains_key(&7), "no punching with a hidden peer");
        assert_eq!(la.route_pref(7), hide);
        la.set_route_prefs(HashMap::new());
        wait("auto punches again", &|| la.allocation().0.contains(&7));
        // 0.6：只把语音改成走服务器：语音不直连，但和他的直连本身不拆（画面等照常能用）
        la.set_route_prefs(HashMap::from([(7, PeerPref { voice_server: true, ..Default::default() })]));
        wait("voice via server", &|| !la.allocation().0.contains(&7));
        assert!(la.peers.lock().unwrap().map.get(&7).is_some_and(|p| p.confirmed), "the P2P link itself stays up");
        la.set_route_prefs(HashMap::new());
        wait("voice direct again", &|| la.allocation().0.contains(&7));
        // 语音优先：服务器说他没听到我 → 给他的语音改经服务器
        la.set_server_fallback(vec![7]);
        wait("fallback goes via server", &|| !la.allocation().0.contains(&7));
        la.set_server_fallback(vec![]);
        wait("fallback lifted", &|| la.allocation().0.contains(&7));
        // 直连被判为不稳：自动时改走服务器，强制直连时照样直连
        la.peers.lock().unwrap().map.get_mut(&7).unwrap().unstable_until = Some(Instant::now() + Duration::from_secs(600));
        wait("unstable falls back", &|| !la.allocation().0.contains(&7));
        la.set_route_prefs(HashMap::from([(7, PeerPref { voice_direct: true, video_direct: true, ..Default::default() })]));
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
        let src = crate::streams::start(udp::MUSIC, crate::loopback::Target::Include(player.id()), sa.clone(), gain, false).expect("start music");
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
            connect(dummy_server, 0, None, &keys, id, true, e2e, tx, st.clone(), mx).unwrap();
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
