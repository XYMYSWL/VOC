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
use voc_proto::{now_millis, udp, ClientMsg, PeerInfo, PeerStat, RelayRoute, FRAME_SAMPLES, SAMPLE_RATE};

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
        }
    }
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
    /// 上次上报链路质量的时间
    last_stats: Mutex<Instant>,
    /// 屏幕画面的匀速发送队列（语音不走这里，直接发）
    pace_tx: Mutex<Option<Sender<(Instant, SocketAddr, Vec<u8>)>>>,
}

impl Link {
    fn send_raw(&self, buf: &[u8], to: SocketAddr) {
        let _ = self.sock.send_to(buf, wire(to, self.dual));
    }

    fn send_server(&self, kind: u8, data: &[u8], buf: &mut Vec<u8>) {
        self.sealer.lock().unwrap().seal(kind, data, buf);
        self.send_raw(buf, self.server);
    }

    fn tcp_send(&self, o: Outgoing) {
        let _ = self.tcp.lock().unwrap().send(o);
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
        let relayed: Vec<u32> =
            self.relays.lock().unwrap().iter().filter(|r| r.from == self.my_id && sent.contains(&r.via)).map(|r| r.to).collect();
        if others.iter().all(|o| sent.contains(o) || relayed.contains(o)) {
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
            self.tcp_send(Outgoing::Msg(ClientMsg::P2pEnable { local_addrs }));
        } else {
            *self.peers.lock().unwrap() = Peers::default();
            self.tcp_send(Outgoing::Msg(ClientMsg::P2pDisable));
        }
    }

    /// 服务器告知：现在谁在看我的屏幕
    pub fn set_watchers(&self, ids: Vec<u32>) {
        let mut w = self.watchers.lock().unwrap();
        if ids.iter().any(|i| !w.contains(i)) {
            self.watchers_ver.fetch_add(1, Ordering::Relaxed); // 有新观看者：编码器会补一个关键帧
        }
        *w = ids;
    }

    pub fn watchers_version(&self) -> u32 {
        self.watchers_ver.load(Ordering::Relaxed)
    }

    pub fn has_watchers(&self) -> bool {
        !self.watchers.lock().unwrap().is_empty()
    }

    pub fn watcher_ids(&self) -> Vec<u32> {
        self.watchers.lock().unwrap().clone()
    }

    pub fn watcher_count(&self) -> usize {
        self.watchers.lock().unwrap().len()
    }

    /// 发送一帧视频的全部分片：能 P2P 直连的观看者直接发，其余交给服务器中转
    pub fn send_video(&self, frags: &[Vec<u8>], buf: &mut Vec<u8>) {
        let watchers = self.watchers.lock().unwrap().clone();
        if watchers.is_empty() {
            return;
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
        if direct == watchers.len() {
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
            self.tcp_send(Outgoing::Msg(ClientMsg::P2pEnable { local_addrs: list }));
        }
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
                    if secs < PUNCH_FAST_SECS {
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
    pub fn on_peers(&self, list: Vec<PeerInfo>) {
        if !self.p2p_enabled.load(Ordering::Relaxed) {
            return;
        }
        let mut peers = self.peers.lock().unwrap();
        peers.map.retain(|id, _| list.iter().any(|p| p.id == *id));
        for info in list {
            let cands: Vec<SocketAddr> = info.addrs.iter().filter_map(|a| a.parse().ok()).map(norm).collect();
            let p = peers.map.entry(info.id).or_insert_with(|| Peer::new(Vec::new()));
            if p.cands != cands {
                p.cands = cands;
                p.punch_started = None; // 有新地址：重新快速打洞
            }
        }
    }

    /// 每 100 ms 调一次：打洞、保活、判断断开、同步直连名单
    fn tick(&self, buf: &mut Vec<u8>) {
        let others = self.others.lock().unwrap().clone();
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
                let interval = if now.duration_since(started) < Duration::from_secs(PUNCH_FAST_SECS as u64) {
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
        // 按上行预算分配：已打通的候选按延迟从低到高
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
        let (mut voice, mut video) = if active {
            allocate_direct(
                self.budget_bps.load(Ordering::Relaxed),
                &by_rtt(&others),
                others.len(),
                self.video_bps.load(Ordering::Relaxed),
                &by_rtt(&watchers),
                watchers.len(),
            )
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
            self.tcp_send(Outgoing::Msg(ClientMsg::LinkStats { server_rtt: udp_rtt, peers: stats, relay_slots: slots }));
        }
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
                udp::VOICE | udp::VIDEO => return Some((id, kind, data)),
                _ => {}
            }
            return None;
        }
        None
    }
}

// ------------------------------------------------------------ 解码

struct UserDecoder {
    dec: opus::Decoder,
    last_seq: Option<u16>,
}

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
}

impl VoiceState {
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
        self.video.clear();
        *self.e2e.lock().unwrap() = None;
    }
}

/// 处理一个视频分片（服务器中转 / P2P 直连 / TCP 隧道共用）；需要时向共享者请求关键帧
pub fn on_server_video(payload: &[u8], state: &VoiceState) {
    let Some(e2e) = state.e2e.lock().unwrap().clone() else { return };
    let req = state.video.on_packet(payload, |from, blob| e2e.lock().unwrap().decrypt_video(from, blob));
    if let (Some(from), Some(l)) = (req, state.current_link()) {
        l.request_keyframe(from);
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
            let Ok(dec) = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono) else { return };
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
        if (1..=5).contains(&gap) {
            // 丢包：前面的用 PLC 补，紧挨着的一帧用本包携带的 FEC 恢复
            for i in 0..gap {
                let s = last.wrapping_add(1 + i);
                let n = if i + 1 == gap {
                    d.dec.decode(data, &mut pcm[..FRAME_SAMPLES], true)
                } else {
                    d.dec.decode(&[], &mut pcm[..FRAME_SAMPLES], false)
                };
                if let Ok(n) = n {
                    frames.push((s, pcm[..n].to_vec()));
                }
            }
        }
    }
    let Ok(n) = d.dec.decode(data, &mut pcm[..], false) else { return };
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
        budget_bps: AtomicU32::new(DEFAULT_BUDGET_BPS),
        video_bps: AtomicU32::new(0),
        alloc: Mutex::new((Vec::new(), Vec::new())),
        server_rtt: AtomicU32::new(0),
        relay_enabled: AtomicBool::new(true),
        relays: Mutex::new(Vec::new()),
        last_stats: Mutex::new(Instant::now()),
        pace_tx: Mutex::new(None),
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
        while !l.closed.load(Ordering::Relaxed) {
            if n % 10 == 0 {
                l.send_server(udp::PING, &now_millis().to_le_bytes(), &mut buf);
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
        while !l.closed.load(Ordering::Relaxed) {
            // 超时 / Windows 的 ConnectionReset（打洞时常见）都忽略
            let Ok((n, from)) = recv_sock.recv_from(&mut buf) else { continue };
            let from = norm(from);
            let pkt = &buf[..n];
            if from == l.server {
                let Some((kind, data)) = opener.open(pkt) else { continue };
                match kind {
                    udp::PONG if data.len() == 8 => {
                        let sent = u64::from_le_bytes(data[..8].try_into().unwrap());
                        let now = now_millis();
                        l.last_pong_ms.store(now, Ordering::Relaxed);
                        let rtt = (now.saturating_sub(sent) as u32).max(1);
                        state.ping_ms.store(rtt, Ordering::Relaxed);
                        l.server_rtt.store(rtt, Ordering::Relaxed);
                    }
                    udp::VOICE => on_server_voice(&data, &state, &mixer, &mut pcm),
                    udp::VIDEO => on_server_video(&data, &state),
                    _ => {}
                }
            } else if let Some((peer, kind, v)) = l.on_peer_packet(from, pkt, &mut out) {
                match kind {
                    udp::VIDEO => on_server_video(&v, &state),
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

/// 请求家用路由器做 UDP 端口映射（UPnP IGD），成功后把映射出的公网地址加入 P2P 候选。
/// 很多家用路由器默认开启 UPnP；拿到映射后，对方可以直接打进来，打洞成功率大幅提高。
/// 路由器本身没有公网 IP（运营商级 NAT）时映射没有意义，会跳过。
fn spawn_upnp(link: Arc<Link>, port: u16) {
    let _ = std::thread::Builder::new().name("upnp".into()).spawn(move || {
        use igd_next::{PortMappingProtocol, SearchOptions};
        let lan = link
            .local_addrs
            .lock()
            .unwrap()
            .iter()
            .filter_map(|a| a.parse::<SocketAddr>().ok())
            .find(|a| a.is_ipv4());
        let Some(SocketAddr::V4(lan)) = lan else { return };
        let mut opts = SearchOptions::default();
        opts.timeout = Some(Duration::from_secs(3));
        let Ok(gw) = igd_next::search_gateway(opts) else { return };
        let Ok(IpAddr::V4(ext_ip)) = gw.get_external_ip() else { return };
        let o = ext_ip.octets();
        let cgnat = o[0] == 100 && (64..128).contains(&o[1]);
        if ext_ip.is_private() || ext_ip.is_loopback() || ext_ip.is_unspecified() || cgnat {
            return;
        }
        let local = SocketAddr::V4(lan);
        let lease = 3600;
        let ext_port = match gw.add_port(PortMappingProtocol::UDP, port, local, lease, "VOC") {
            Ok(()) => port,
            Err(_) => match gw.add_any_port(PortMappingProtocol::UDP, local, lease, "VOC") {
                Ok(p) => p,
                Err(_) => return,
            },
        };
        link.add_local_candidate(SocketAddr::new(IpAddr::V4(ext_ip), ext_port).to_string());
        // 续期；断开时删除映射
        let mut since = Instant::now();
        while !link.closed.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(500));
            if since.elapsed() > Duration::from_secs(lease as u64 / 2) {
                let _ = gw.add_port(PortMappingProtocol::UDP, ext_port, local, lease, "VOC");
                since = Instant::now();
            }
        }
        let _ = gw.remove_port(PortMappingProtocol::UDP, ext_port);
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

fn new_encoder() -> opus::Encoder {
    let mut e = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip).expect("opus encoder");
    let _ = e.set_bitrate(opus::Bitrate::Bits(OPUS_BITRATE));
    let _ = e.set_inband_fec(true); // 带内前向纠错：丢一个包可由下一个包恢复
    let _ = e.set_packet_loss_perc(10);
    e
}

/// 16 kHz / 20 ms 帧的 VAD + 编码 + 发送
struct Transmitter {
    settings: Arc<Settings>,
    state: Arc<VoiceState>,
    mixer: Arc<Mutex<Mixer>>,
    enc: opus::Encoder,
    loop_dec: Option<opus::Decoder>,
    pcm: Vec<i16>,
    bytes: Vec<u8>,
    decoded: Vec<i16>,
    pkt: Vec<u8>,
    hang: u32,
    loop_seq: u16,
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

        let Ok(n) = self.enc.encode(&self.pcm, &mut self.bytes) else { return };
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
            connect(dummy_server, &keys, id, true, e2e, tx, st.clone(), mx).unwrap();
            let link = st.current_link().unwrap();
            (link, st, rx)
        };
        let (la, sa, rxa) = mk(3, ea.clone());
        let (lb, sb, _rxb) = mk(7, eb);
        let addr = |l: &Link| format!("127.0.0.1:{}", l.sock.local_addr().unwrap().port());
        la.set_others(vec![7]);
        lb.set_others(vec![3]);
        la.on_peers(vec![PeerInfo { id: 7, addrs: vec![addr(&lb)] }]);
        lb.on_peers(vec![PeerInfo { id: 3, addrs: vec![addr(&la)] }]);

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
            connect(dummy_server, &keys, id, true, e2e, tx, st.clone(), mx).unwrap();
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
        la.on_peers(vec![PeerInfo { id: 5, addrs: vec![addr(&lr)] }]);
        lb.on_peers(vec![PeerInfo { id: 5, addrs: vec![addr(&lr)] }]);
        lr.on_peers(vec![PeerInfo { id: 3, addrs: vec![addr(&la)] }, PeerInfo { id: 7, addrs: vec![addr(&lb)] }]);
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
            connect(dummy_server, &keys, id, true, e2e, tx, st.clone(), mx).unwrap();
            (st.current_link().unwrap(), st, rx)
        };
        let (la, sa, _rxa) = mk(3, ea);
        let (lb, sb, _rxb) = mk(7, eb);
        let addr = |l: &Link| format!("127.0.0.1:{}", l.sock.local_addr().unwrap().port());
        la.set_others(vec![7]);
        lb.set_others(vec![3]);
        la.on_peers(vec![PeerInfo { id: 7, addrs: vec![addr(&lb)] }]);
        lb.on_peers(vec![PeerInfo { id: 3, addrs: vec![addr(&la)] }]);
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
        connect(sink.local_addr().unwrap(), &keys, 3, false, ea, tx, st.clone(), Arc::new(Mutex::new(Mixer::default()))).unwrap();
        let l = st.current_link().unwrap();
        l.udp_ok.store(true, Ordering::Relaxed);
        l.set_watchers(vec![7]);
        let frags: Vec<Vec<u8>> = (0..100).map(|_| vec![0u8; 1150]).collect();
        let mut buf = Vec::new();
        let t = Instant::now();
        l.send_video(&frags, &mut buf);
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

    /// 真实屏幕采集 → 720p H.264 → 端到端加密 → 两个真实 Link P2P 直连 → 解码。
    /// 需要桌面环境，默认不跑：cargo test -p voc-client live_screen_share -- --ignored
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
            connect(dummy_server, &keys, id, true, e2e, tx, st.clone(), mx).unwrap();
            let link = st.current_link().unwrap();
            (link, st, rx)
        };
        let (la, sa, _rxa) = mk(3, ea);
        let (lb, sb, _rxb) = mk(7, eb);
        let addr = |l: &Link| format!("127.0.0.1:{}", l.sock.local_addr().unwrap().port());
        la.set_others(vec![7]);
        lb.set_others(vec![3]);
        la.on_peers(vec![PeerInfo { id: 7, addrs: vec![addr(&lb)] }]);
        lb.on_peers(vec![PeerInfo { id: 3, addrs: vec![addr(&la)] }]);
        let t = Instant::now();
        while !(la.direct_peers().contains_key(&7) && lb.direct_peers().contains_key(&3)) {
            assert!(t.elapsed() < Duration::from_secs(5), "hole punching did not complete");
            std::thread::sleep(Duration::from_millis(50));
        }
        let view = sb.video.watch(3, true).unwrap();
        // 观看者上报的解码能力（真实客户端由服务器转来）
        sa.caps.lock().unwrap().insert(7, crate::video::local_decoders().iter().map(|c| c.key().to_string()).collect());
        la.set_watchers(vec![7]);
        let h: u32 = std::env::var("VOC_LIVE_HEIGHT").ok().and_then(|v| v.parse().ok()).unwrap_or(720);
        let secs: u64 = std::env::var("VOC_LIVE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        let sharer = Sharer::start(ShareConfig { height: h, fps: 60, encoder: std::env::var("VOC_LIVE_ENCODER").unwrap_or_else(|_| "auto".into()), ..Default::default() }, sa.clone()).unwrap();
        let t = Instant::now();
        loop {
            if let Some(f) = view.frame.lock().unwrap().as_ref() {
                if f.seq >= 30 && t.elapsed() >= Duration::from_secs(secs) {
                    println!(
                        "[{}] -> [{}] decoded {}x{} frames: {}, viewer fps: {}, capture fps: {}, sender fps: {}, kbps: {}, encode_ms: {}, skipped/s: {}, elapsed {:?}",
                        sharer.stats.encoder.lock().unwrap(),
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
                        t.elapsed()
                    );
                    assert_eq!(f.height as u32, h);
                    break;
                }
            }
            assert!(t.elapsed() < Duration::from_secs(15 + secs), "no video decoded via P2P");
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(sharer);
        sa.disconnect();
        sb.disconnect();
    }
}
