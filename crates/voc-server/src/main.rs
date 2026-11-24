//! VOC 服务器：授权码登录、ML-KEM-768 加密、抗 DPI 传输；语音 SFU 转发（不混音）。

mod codes;
mod relay;

use codes::CodeStore;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, Notify};
use voc_proto::auth::random_bytes;
use voc_proto::crypto::{self, *};
use voc_proto::e2e;
use voc_proto::{udp, *};

enum Out {
    Json(ServerMsg),
    Voice(Vec<u8>),
    Video(Vec<u8>),
    /// 伴奏 / 共享声音 / 鼠标（UDP 包类型, 内容）
    Stream(u8, Vec<u8>),
    /// 图片分块（端到端加密的密文），只转发不保存
    File(Vec<u8>),
}

/// 一个接收者还没发出去的图片数据超过这么多，就不再给他转这张图（防止慢的人把服务器内存撑爆）
const MAX_PENDING_FILE: usize = 8 * 1024 * 1024;
/// 每个人最多同时传几张图
const MAX_UPLOADS: usize = 2;
/// 传输多久没动静就作废
const UPLOAD_TTL: Duration = Duration::from_secs(120);

struct User {
    name: String,
    code_id: String,
    channel: Option<u32>,
    muted: bool,
    deafened: bool,
    tx: mpsc::UnboundedSender<Out>,
    kick: Arc<Notify>,
    udp: Option<SocketAddr>,
    udp_open: DatagramOpener,
    udp_seal: DatagramSealer,
    /// 该用户的语音走 UDP（false 则走 TCP 隧道）
    voice_udp: bool,
    /// 开启 P2P 时：自报的本地地址
    p2p: Option<Vec<String>>,
    /// 自测的 NAT 类型
    nat: Option<NatInfo>,
    /// 选了和这些人走服务器：双方的地址互不透露
    p2p_hide: HashSet<u32>,
    /// 端到端加密身份公告
    e2e: Option<E2eEntry>,
    /// 已与之直连的成员：不再替该用户把语音转发给他们
    direct: HashSet<u32>,
    /// 屏幕共享画面已直连的观看者：不再替该用户转发视频给他们
    direct_video: HashSet<u32>,
    /// 共享声音已直连的观看者（客户端单独报了才有；None = 跟 direct_video 一样）
    direct_share_audio: Option<HashSet<u32>>,
    /// 断线恢复（第 37-2 项）：凭证（空 = 旧客户端，不保留）、当前是第几个连接、断开的时间
    resume_token: String,
    conn_gen: u64,
    detached: Option<Instant>,
    /// 上次发给该用户的 Peers 列表（没变化就不重发）
    last_peers: Vec<PeerInfo>,
    /// 正在屏幕共享（描述，例如 "1080p60"）
    sharing: Option<String>,
    /// 我正在观看谁的屏幕
    watching: HashSet<u32>,
    /// 其中要低画质层的
    watch_low: HashSet<u32>,
    /// 观看时选的画面线路：共享者 → 1 P2P / 2 服务器（自动的不记）
    watch_route: HashMap<u32, u8>,
    /// 我会自己画鼠标的共享者（我在看他）
    watch_cursor: HashSet<u32>,
    /// 我的屏幕共享有低画质层
    simulcast: bool,
    /// 我的屏幕共享支持丢包补发（收到 NACK 会重发）和冗余校验包
    rtx: bool,
    /// 我的屏幕共享原画码率（bps，估算画面中继的上行用）
    share_bps: u32,
    /// 上次发给我的画面中继路线
    last_video_relays: Vec<RelayRoute>,
    /// 我最近发出的画面分片（观看者补发请求先从这里找，找不到再转给我）
    video_cache: VideoCache,
    /// 上次发给我的观看者名单（我是共享者时）：全部、要低画质的、选了 P2P 的、选了走服务器的、自己画鼠标的
    last_watchers: (Vec<u32>, Vec<u32>, Vec<u32>, Vec<u32>, Vec<u32>),
    /// 能解码的视频格式
    decoders: Vec<String>,
    /// 正在放伴奏的程序名
    music: Option<String>,
    /// 屏幕共享带声音
    share_audio: bool,
    /// 屏幕共享只走 P2P：服务器不转发也不缓存这个人的画面 / 共享声音 / 鼠标
    share_p2p_only: bool,
    /// 最近一次链路质量上报（成员中继用）
    stats: Option<LinkReport>,
    /// 上次发给我的中继路线
    last_relays: Vec<RelayRoute>,
    /// 还在发送队列里的图片字节数
    pending_file: Arc<std::sync::atomic::AtomicUsize>,
    /// 还在 TCP 发送队列里的语音 / 画面 / 伴奏字节数
    pending_media: Arc<std::sync::atomic::AtomicUsize>,
    /// 因为队列满丢掉的媒体包数（状态里显示）
    media_dropped: Arc<std::sync::atomic::AtomicU64>,
    /// 我正在上传的图片：id → (块数, 已收块数, 最后一次收到的时间, 私聊对象)
    uploads: HashMap<u64, (u32, u32, Instant, Option<u32>)>,
    /// 还没开始上传的私聊图片：id → 接收者
    upload_to: HashMap<u64, u32>,
}

/// 共享者最近的画面分片：(画质层, 帧号) → 分片号 → 分片（含分片头）
#[derive(Default)]
struct VideoCache {
    frames: HashMap<(u8, u32), HashMap<u16, Vec<u8>>>,
    order: std::collections::VecDeque<(u8, u32)>,
    bytes: usize,
}

/// 每个共享者最多缓存多少字节的画面（约 1 秒的 25 Mbps）
const VIDEO_CACHE_BYTES: usize = 4 * 1024 * 1024;
const VIDEO_CACHE_FRAMES: usize = 240;

impl VideoCache {
    fn insert(&mut self, layer: u8, frame: u32, idx: u16, frag: &[u8]) {
        let key = (layer, frame);
        let e = self.frames.entry(key).or_insert_with(|| {
            self.order.push_back(key);
            HashMap::new()
        });
        if e.insert(idx, frag.to_vec()).is_none() {
            self.bytes += frag.len();
        }
        while self.bytes > VIDEO_CACHE_BYTES || self.order.len() > VIDEO_CACHE_FRAMES {
            let Some(old) = self.order.pop_front() else { break };
            if let Some(m) = self.frames.remove(&old) {
                self.bytes -= m.values().map(|v| v.len()).sum::<usize>();
            }
        }
    }

    /// 找补发请求要的分片：(找到的, 没找到的分片号)。整帧请求时只要缓存里有这一帧的全部数据片就算找到
    fn lookup(&self, layer: u8, frame: u32, idxs: &[u16]) -> (Vec<Vec<u8>>, Vec<u16>) {
        let Some(m) = self.frames.get(&(layer, frame)) else { return (Vec::new(), idxs.to_vec()) };
        let (mut found, mut missing) = (Vec::new(), Vec::new());
        for &i in idxs {
            if i == udp::NACK_WHOLE {
                let count = m.values().next().and_then(|f| udp::read_video_frag(f)).map(|(h, _)| h.count).unwrap_or(0);
                if count > 0 && (0..count).all(|j| m.contains_key(&j)) {
                    found.extend((0..count).map(|j| m[&j].clone()));
                } else {
                    missing.push(i);
                }
            } else {
                match m.get(&i) {
                    Some(f) => found.push(f.clone()),
                    None => missing.push(i),
                }
            }
        }
        (found, missing)
    }
}

/// 客户端上报的链路质量
#[derive(Clone)]
struct LinkReport {
    at: Instant,
    server_rtt: u32,
    peers: Vec<PeerStat>,
    slots: u16,
    /// 手动选了走服务器的人
    via_server: HashSet<u32>,
    /// 还能帮别人转发画面的上行（bps）
    video_relay_bps: u32,
    /// 最近 2 秒在说话
    talking: bool,
    /// 最近 3 秒实际听到了谁
    heard: HashSet<u32>,
    /// 手动指定的语音中转：对方 → 经谁
    manual_via: HashMap<u32, u32>,
    /// 他经服务器收语音的丢包率（%）
    voice_loss: u8,
}

/// 听众经服务器收语音丢包到这个程度（%），服务器给他的语音每包多发一份（只给他，第 42-5 项）
const VOICE_DUP_LOSS: u8 = 10;

/// 语音优先：连续这么多次上报「他在说、我没听到」就改经服务器转发
const FALLBACK_MISSES: u8 = 2;
/// 改经服务器转发后保持这么久，再试回直连 / 中转（还不行会再改回来）
const FALLBACK_HOLD: Duration = Duration::from_secs(60);

/// 链路上报超过这么久没更新就不再采信
const STATS_TTL: Duration = Duration::from_secs(6);

impl User {
    fn send(&self, m: ServerMsg) {
        let _ = self.tx.send(Out::Json(m));
    }

    /// 经 TCP 发语音 / 画面 / 伴奏给这个人（他的 UDP 不通）。对方收得慢时队列有上限：
    /// 排队超过 MAX_PENDING_VIDEO 先丢画面，超过 MAX_PENDING_MEDIA 连声音也丢——宁可丢包也不让服务器内存一直涨（0.5.x 不限）
    fn send_media(&self, o: Out) {
        let len = match &o {
            Out::Voice(v) | Out::Video(v) | Out::Stream(_, v) => v.len(),
            _ => 0,
        };
        let cap = if matches!(o, Out::Video(_)) { MAX_PENDING_VIDEO } else { MAX_PENDING_MEDIA };
        let pending = self.pending_media.load(std::sync::atomic::Ordering::Relaxed);
        if pending + len > cap {
            self.media_dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        self.pending_media.fetch_add(len, std::sync::atomic::Ordering::Relaxed);
        if self.tx.send(o).is_err() {
            self.pending_media.fetch_sub(len, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// 经 TCP 排队等发给一个人的画面最多这么多字节（约 1 秒 8 Mbps）
const MAX_PENDING_VIDEO: usize = 1024 * 1024;
/// 语音 / 伴奏可以多排一点（画面已经在丢了，声音尽量保住）
const MAX_PENDING_MEDIA: usize = MAX_PENDING_VIDEO + 256 * 1024;

struct State {
    server_name: String,
    udp_port: u16,
    /// NAT 探测端口，0 = 没开
    probe_port: u16,
    users: HashMap<u32, User>,
    addr_map: HashMap<SocketAddr, u32>,
    channels: Vec<ChannelInfo>,
    next_user: u32,
    next_channel: u32,
    codes: CodeStore,
    replay: ReplayCache,
    /// 成员中继路线：(说话的人, 听的人) → 中继成员
    relays: HashMap<(u32, u32), u32>,
    /// 屏幕共享画面中继：(共享者, 观看者) → 帮忙转发的观看者
    video_relays: HashMap<(u32, u32), u32>,
    /// 语音优先：(说话的人, 听的人) 连续几次「在说却没听到」
    voice_misses: HashMap<(u32, u32), u8>,
    /// 语音优先：(说话的人, 听的人) 改经服务器转发到什么时候
    voice_fallback: HashMap<(u32, u32), Instant>,
    /// 这一秒转发的语音 / 画面字节数；上一秒的（状态文件和带宽上限用）
    sec_bytes: (u64, u64),
    last_sec_bytes: (u64, u64),
    /// 启动以来每 5 秒数一次「同频道、都开了 P2P 的两人」：(总对数, 语音直连的对数)，按时间加权算直连率（第 47 项）
    p2p_samples: (u64, u64),
    /// 按天累计的直连率（第 47-2 项）：(日期 UTC「YYYY-MM-DD」, 总对数, 直连对数)，只留最近 7 天，存在授权码文件旁边
    p2p_days: Vec<(String, u64, u64)>,
    /// 按天、按双方 NAT 类型的打洞结果（第 62-1 项）：只留最近 7 天，存在授权码文件旁边；只有次数，没有昵称和 IP
    punch_days: Vec<PunchDay>,
    /// 已经记过的一对（编号小的, 编号大的, 双方的连接计数）：每对每次登录只记第一次
    punch_seen: HashSet<(u32, u32, u64, u64)>,
    /// 连接计数（断线恢复用，第 37-2 项）
    next_conn_gen: u64,
    /// 画面转发总带宽上限（bps）
    max_video_bps: Option<u64>,
    /// 因为服务器带宽不够被临时改看「流畅」的 (观看者, 共享者)；带宽宽松了多少秒
    forced_low: HashSet<(u32, u32)>,
    cap_calm_secs: u32,
    /// 认证失败太多次的 IP
    bans: BanList,
    /// NAT 探测端口（状态文件用）
    probe_listening: bool,
    /// 第二个探测点「公网IP:端口」，没有为空
    probe2: String,
}

/// 认证失败的 IP：(1 分钟内失败次数, 这一分钟从什么时候开始, 封到什么时候)
#[derive(Default)]
struct BanList(HashMap<std::net::IpAddr, (u32, Instant, Option<Instant>)>);

impl BanList {
    fn banned(&mut self, ip: std::net::IpAddr, now: Instant) -> bool {
        self.0.retain(|_, (_, start, until)| now.duration_since(*start) < Duration::from_secs(60) || until.is_some_and(|u| u > now));
        self.0.get(&ip).is_some_and(|b| b.2.is_some_and(|u| u > now))
    }

    /// 记一次认证失败。返回 (要不要打日志：这一分钟的第一次, 这次刚被封)
    fn failed(&mut self, ip: std::net::IpAddr, now: Instant) -> (bool, bool) {
        let e = self.0.entry(ip).or_insert((0, now, None));
        if now.duration_since(e.1) >= Duration::from_secs(60) {
            *e = (0, now, e.2);
        }
        e.0 += 1;
        if e.0 == BAN_FAILS {
            e.2 = Some(now + BAN_FOR);
        }
        (e.0 == 1, e.0 == BAN_FAILS)
    }

    fn count(&self, now: Instant) -> usize {
        self.0.values().filter(|b| b.2.is_some_and(|u| u > now)).count()
    }
}

/// 同一个 IP 1 分钟内认证失败这么多次就封
const BAN_FAILS: u32 = 10;
const BAN_FOR: Duration = Duration::from_secs(300);

type Shared = Arc<Mutex<State>>;

impl State {
    fn snapshot(&self) -> ServerMsg {
        let mut users: Vec<UserInfo> = self
            .users
            .iter()
            .map(|(id, u)| UserInfo {
                id: *id,
                name: u.name.clone(),
                channel_id: u.channel,
                muted: u.muted,
                deafened: u.deafened,
                sharing: u.sharing.clone(),
                decoders: u.decoders.clone(),
                p2p: u.p2p.is_some(),
                udp: u.voice_udp,
                music: u.music.clone(),
                share_audio: u.sharing.is_some() && u.share_audio,
                simulcast: u.sharing.is_some() && u.simulcast,
                rtx: u.sharing.is_some() && u.rtx,
                share_p2p_only: u.sharing.is_some() && u.share_p2p_only,
            })
            .collect();
        users.sort_by_key(|u| u.id);
        ServerMsg::State { channels: self.channels.clone(), users }
    }

    fn broadcast(&self, msg: &ServerMsg) {
        for u in self.users.values() {
            u.send(msg.clone());
        }
    }

    fn broadcast_state(&self) {
        self.broadcast(&self.snapshot());
    }

    fn system(&self, text: String) {
        self.broadcast(&ServerMsg::System { text });
    }

    fn channel_name(&self, id: u32) -> String {
        self.channels.iter().find(|c| c.id == id).map(|c| c.name.clone()).unwrap_or_default()
    }

    /// 给每个开启 P2P 的用户推送「同频道、也开启了 P2P、UDP 地址已知」的成员列表（有变化才发）。
    /// 隐私：任何一方选了和对方走服务器，双方地址互不透露；局域网地址只给同一个局域网（公网 IP 相同）的人
    fn push_peers(&mut self) {
        struct Cand {
            id: u32,
            public: SocketAddr,
            local: Vec<String>,
            nat: Option<NatInfo>,
            hide: HashSet<u32>,
        }
        let cands: Vec<Cand> = self
            .users
            .iter()
            .filter_map(|(id, u)| {
                let local = u.p2p.as_ref()?;
                Some(Cand { id: *id, public: u.udp?, local: local.iter().take(8).cloned().collect(), nat: u.nat, hide: u.p2p_hide.clone() })
            })
            .collect();
        for (id, u) in self.users.iter_mut() {
            let my_public = u.udp.map(|a| a.ip().to_canonical());
            // 0.6：不只同频道——服务器上所有开了 P2P 的人都先打好洞（进频道那一刻就是直连）；
            // 选了不透露 IP 的照旧互相不给
            let mut peers: Vec<PeerInfo> = match &u.p2p {
                Some(_) => cands
                    .iter()
                    .filter(|c| c.id != *id && !c.hide.contains(id) && !u.p2p_hide.contains(&c.id))
                    .map(|c| {
                        let same_lan = my_public == Some(c.public.ip().to_canonical());
                        let mut addrs = vec![c.public.to_string()];
                        addrs.extend(c.local.iter().filter(|a| same_lan || a.parse::<SocketAddr>().is_ok_and(|a| is_public(a.ip()))).cloned());
                        PeerInfo { id: c.id, addrs, nat: c.nat }
                    })
                    .collect(),
                None => Vec::new(),
            };
            peers.sort_by_key(|p| p.id);
            // 新出现、或者地址变了的人：同一时刻通知双方开打（第 62-2 项；在同一轮里给所有人发，双方收到的时间只差各自的网络延迟）
            let fresh: Vec<u32> = peers.iter().filter(|p| !u.last_peers.contains(p)).map(|p| p.id).collect();
            if peers != u.last_peers {
                u.direct.retain(|d| peers.iter().any(|p| p.id == *d));
                u.direct_video.retain(|d| peers.iter().any(|p| p.id == *d));
                if let Some(a) = &mut u.direct_share_audio {
                    a.retain(|d| peers.iter().any(|p| p.id == *d));
                }
                u.last_peers = peers.clone();
                u.send(ServerMsg::Peers { peers });
                for peer in fresh {
                    u.send(ServerMsg::PunchAt { peer, in_ms: PUNCH_SYNC_MS });
                }
            }
        }
    }

    fn remove_user(&mut self, id: u32) -> Option<User> {
        self.punch_seen.retain(|k| k.0 != id && k.1 != id);
        let u = self.users.remove(&id)?;
        for o in self.users.values_mut() {
            o.watching.remove(&id);
        }
        if let Some(a) = u.udp {
            self.addr_map.remove(&a);
        }
        Some(u)
    }

    /// 告诉每个共享者现在有谁在看（有变化才发）
    fn push_watchers(&mut self) {
        type Pair = (u32, Option<u32>, HashSet<u32>, HashSet<u32>, HashMap<u32, u8>, HashSet<u32>);
        let forced = self.forced_low.clone();
        let pairs: Vec<Pair> = self
            .users
            .iter()
            .map(|(id, u)| {
                let mut low = u.watch_low.clone();
                low.extend(forced.iter().filter(|(v, _)| v == id).map(|(_, s)| *s));
                (*id, u.channel, u.watching.clone(), low, u.watch_route.clone(), u.watch_cursor.clone())
            })
            .collect();
        for (id, u) in self.users.iter_mut() {
            // (观看者, 要低画质, 线路, 自己画鼠标)
            let mut w: Vec<(u32, bool, u8, bool)> = match (&u.sharing, u.channel) {
                (Some(_), Some(ch)) => pairs
                    .iter()
                    .filter(|(wid, wch, set, _, _, _)| wid != id && *wch == Some(ch) && set.contains(id))
                    .map(|(wid, _, _, low, route, cur)| {
                        (*wid, u.simulcast && low.contains(id), route.get(id).copied().unwrap_or(0), cur.contains(id))
                    })
                    .collect(),
                _ => Vec::new(),
            };
            w.sort();
            let ids: Vec<u32> = w.iter().map(|x| x.0).collect();
            let low: Vec<u32> = w.iter().filter(|x| x.1).map(|x| x.0).collect();
            let direct: Vec<u32> = w.iter().filter(|x| x.2 == 1).map(|x| x.0).collect();
            let server: Vec<u32> = w.iter().filter(|x| x.2 == 2).map(|x| x.0).collect();
            let cursor: Vec<u32> = w.iter().filter(|x| x.3).map(|x| x.0).collect();
            let now = (ids, low, direct, server, cursor);
            if now != u.last_watchers {
                u.last_watchers = now.clone();
                let (ids, low, direct, server, cursor) = now;
                u.send(ServerMsg::Watchers { ids, low, direct, server, cursor });
            }
        }
    }

    /// 转发伴奏（给同频道所有没静音的人，发送者闭麦也照转）/ 屏幕共享声音（给观看者）。已直连的不转。
    fn forward_stream(&mut self, from: u32, kind: u8, seq: u16, frame: &[u8]) -> Vec<(SocketAddr, Vec<u8>)> {
        let Some(u) = self.users.get(&from) else { return Vec::new() };
        let Some(ch) = u.channel else { return Vec::new() };
        let music = kind == udp::MUSIC;
        // 伴奏要在放；共享声音要在共享且带声音；鼠标只要在共享
        if music && u.music.is_none() || !music && (u.sharing.is_none() || (kind == udp::SHARE_AUDIO && !u.share_audio) || u.share_p2p_only) {
            return Vec::new();
        }
        let direct = match (music, kind == udp::SHARE_AUDIO, &u.direct_share_audio) {
            (true, _, _) => u.direct.clone(),
            (false, true, Some(a)) => a.clone(),
            _ => u.direct_video.clone(),
        };
        let payload = udp::server_voice(from, seq, frame);
        let mut out = Vec::new();
        for (id, other) in self.users.iter_mut() {
            if *id == from || other.channel != Some(ch) || other.deafened || direct.contains(id) {
                continue;
            }
            if !music && !other.watching.contains(&from) {
                continue;
            }
            match (other.voice_udp, other.udp) {
                (true, Some(addr)) => {
                    let mut pkt = Vec::with_capacity(payload.len() + 64);
                    other.udp_seal.seal(kind, &payload, &mut pkt);
                    out.push((addr, pkt));
                }
                // UDP 不通的人走 TCP（0.6 起有 KIND_STREAM；以前这种人收不到伴奏和共享声音）
                _ => other.send_media(Out::Stream(kind, payload.clone())),
            }
        }
        out
    }

    /// 转发一块图片给同频道的其他人，私聊图片只转给对方（只转发不保存）
    fn forward_file(&mut self, from: u32, chunk: &[u8]) {
        let Some((id, _seq, total, _)) = voc_proto::file::read_client_chunk(chunk) else { return };
        let Some(u) = self.users.get_mut(&from) else { return };
        let channel = u.channel;
        let now = Instant::now();
        u.uploads.retain(|_, (t, got, at, _)| *got < *t && now.duration_since(*at) < UPLOAD_TTL);
        let target = match u.uploads.get_mut(&id) {
            Some((t, got, at, to)) => {
                if *t != total {
                    return;
                }
                *got += 1;
                *at = now;
                *to
            }
            None => {
                if u.uploads.len() >= MAX_UPLOADS {
                    return;
                }
                let to = u.upload_to.remove(&id);
                u.uploads.insert(id, (total, 1, now, to));
                to
            }
        };
        if target.is_none() && channel.is_none() {
            return;
        }
        let payload = voc_proto::file::server_chunk(from, chunk);
        for (id, other) in self.users.iter() {
            let wanted = match target {
                Some(t) => *id == t,
                None => other.channel == channel,
            };
            if *id == from || !wanted {
                continue;
            }
            let pending = other.pending_file.load(std::sync::atomic::Ordering::Relaxed);
            if pending + payload.len() > MAX_PENDING_FILE {
                continue; // 他收得太慢：这张图对他作废（客户端会超时提示）
            }
            other.pending_file.fetch_add(payload.len(), std::sync::atomic::Ordering::Relaxed);
            let _ = other.tx.send(Out::File(payload.clone()));
        }
    }

    /// 语音优先（0.6）：listener 刚上报了「最近听到了谁」。同频道里正在说话、名单上说是直连或经队友中转给他的人，
    /// 他却连续几次都没听到 → 那条路实际不通，让说话的人先改经服务器发给他（不依赖链路判断正确）
    fn check_voice_heard(&mut self, listener: u32) {
        let now = Instant::now();
        let Some(l) = self.users.get(&listener) else { return };
        let (Some(ch), false) = (l.channel, l.deafened) else { return };
        let Some(heard) = l.stats.as_ref().filter(|r| now.duration_since(r.at) < STATS_TTL).map(|r| r.heard.clone()) else { return };
        let mut changed: Vec<u32> = Vec::new();
        for (tid, t) in &self.users {
            if *tid == listener || t.channel != Some(ch) || t.muted {
                continue;
            }
            let talking = t.stats.as_ref().is_some_and(|r| r.talking && now.duration_since(r.at) < STATS_TTL);
            let bypass = t.direct.contains(&listener) || self.relays.contains_key(&(*tid, listener));
            let key = (*tid, listener);
            if heard.contains(tid) || !talking || !bypass {
                if heard.contains(tid) {
                    self.voice_misses.remove(&key);
                }
                continue;
            }
            let n = self.voice_misses.entry(key).or_default();
            *n = n.saturating_add(1);
            if *n >= FALLBACK_MISSES && !self.voice_fallback.contains_key(&key) {
                self.voice_fallback.insert(key, now + FALLBACK_HOLD);
                self.voice_misses.remove(&key);
                println!("[VOC] 语音优先：#{tid} 在说话但 #{listener} 没听到（直连 / 中转不通），改经服务器转发 {} 秒", FALLBACK_HOLD.as_secs());
                changed.push(*tid);
            }
        }
        for t in changed {
            self.push_voice_fallback(t);
        }
    }

    /// 把「哪些人要改经服务器发」告诉说话的人
    fn push_voice_fallback(&self, talker: u32) {
        let mut ids: Vec<u32> = self.voice_fallback.keys().filter(|(t, _)| *t == talker).map(|(_, l)| *l).collect();
        ids.sort();
        if let Some(u) = self.users.get(&talker) {
            u.send(ServerMsg::VoiceFallback { ids });
        }
    }

    /// 改经服务器转发到期的：撤销，通知说话的人（之后再试直连 / 中转，还不行会再改回来）
    fn expire_voice_fallback(&mut self) {
        let now = Instant::now();
        let gone: Vec<(u32, u32)> =
            self.voice_fallback.iter().filter(|(k, until)| now >= **until || !self.users.contains_key(&k.0) || !self.users.contains_key(&k.1)).map(|(k, _)| *k).collect();
        let mut talkers: Vec<u32> = gone.iter().map(|k| k.0).collect();
        talkers.sort();
        talkers.dedup();
        for k in gone {
            self.voice_fallback.remove(&k);
        }
        self.voice_misses.retain(|k, _| self.users.contains_key(&k.0) && self.users.contains_key(&k.1));
        for t in talkers {
            self.push_voice_fallback(t);
        }
    }

    /// 重新规划各频道的成员中继路线，有变化就通知频道成员
    fn push_relays(&mut self) {
        let now = Instant::now();
        let mut by_channel: HashMap<u32, Vec<relay::Member>> = HashMap::new();
        for (id, u) in &self.users {
            let Some(ch) = u.channel else { continue };
            let mut m = relay::Member {
                id: *id,
                muted: u.muted,
                deafened: u.deafened,
                direct: u.direct.clone(),
                ..Default::default()
            };
            if let Some(r) = u.stats.as_ref().filter(|r| now.duration_since(r.at) < STATS_TTL && u.p2p.is_some()) {
                m.server_rtt = Some(r.server_rtt.max(1));
                m.links = r.peers.iter().map(|p| (p.id, (p.rtt, p.loss, p.jitter))).collect();
                m.slots = r.slots;
                m.server_only = r.via_server.clone();
                m.manual_via = r.manual_via.clone();
            }
            // 语音优先改经服务器的人：这段时间不安排中转
            m.no_relay_to = self.voice_fallback.keys().filter(|(t, _)| t == id).map(|(_, l)| *l).collect();
            by_channel.entry(ch).or_default().push(m);
        }
        let mut all: HashMap<(u32, u32), u32> = HashMap::new();
        let mut per_channel: HashMap<u32, Vec<RelayRoute>> = HashMap::new();
        for (ch, members) in &by_channel {
            let ids: HashSet<u32> = members.iter().map(|m| m.id).collect();
            let current: HashMap<(u32, u32), u32> =
                self.relays.iter().filter(|((f, _), _)| ids.contains(f)).map(|(k, v)| (*k, *v)).collect();
            let routes = relay::plan(members, &current);
            for r in &routes {
                all.insert((r.from, r.to), r.via);
            }
            per_channel.insert(*ch, routes);
        }
        self.relays = all;
        for u in self.users.values_mut() {
            let routes = u.channel.and_then(|ch| per_channel.get(&ch).cloned()).unwrap_or_default();
            if routes != u.last_relays {
                u.last_relays = routes.clone();
                u.send(ServerMsg::Relays { routes });
            }
        }
    }

    /// 安排观看者互相转发画面：共享者直连发给 R 的画面，由 R 原样（密文）转给和 R 直连稳定、又没直连共享者的观看者 T。
    /// 条件：R、T 同一画质层；R→T、T→R 都是稳定直连（丢包 ≤ 3%、抖动 ≤ 30 ms）；R 剩余上行够一路画面（按共享码率 × 1.1）；
    /// 两人之间没选「走服务器」。已有路线仍有效就保留；没有合适的就由服务器照常转发
    fn push_video_relays(&mut self) {
        let now = Instant::now();
        fn fresh(u: &User, now: Instant) -> Option<&LinkReport> {
            u.stats.as_ref().filter(|r| now.duration_since(r.at) < STATS_TTL && u.p2p.is_some())
        }
        let fresh = |u: &User| fresh(u, now).cloned();
        let good = |a: &User, b: u32| {
            fresh(a).and_then(|r| r.peers.iter().find(|p| p.id == b).copied()).filter(|p| p.loss <= relay::MAX_LOSS && p.jitter <= relay::MAX_JITTER).map(|p| p.rtt)
        };
        let mut plan: HashMap<(u32, u32), u32> = HashMap::new();
        let mut used: HashMap<u32, u64> = HashMap::new();
        let mut sharers: Vec<u32> = self.users.iter().filter(|(_, u)| u.sharing.is_some() && u.channel.is_some()).map(|(i, _)| *i).collect();
        sharers.sort();
        for s in sharers {
            let su = &self.users[&s];
            let cost = (su.share_bps.max(1_000_000) as u64) * 11 / 10;
            let mut watchers: Vec<u32> =
                self.users.iter().filter(|(i, u)| **i != s && u.channel == su.channel && u.watching.contains(&s)).map(|(i, _)| *i).collect();
            watchers.sort();
            let low = |id: u32| su.simulcast && self.users[&id].watch_low.contains(&s);
            for &t in watchers.iter().filter(|t| !su.direct_video.contains(t)) {
                let tu = &self.users[&t];
                let mut cands: Vec<(u32, u32)> = watchers
                    .iter()
                    .copied()
                    .filter(|r| *r != t && su.direct_video.contains(r) && low(*r) == low(t))
                    .filter_map(|r| {
                        let ru = &self.users[&r];
                        let rep = fresh(ru)?;
                        let hidden = ru.p2p_hide.contains(&t) || tu.p2p_hide.contains(&r) || rep.via_server.contains(&t)
                            || fresh(tu).is_some_and(|x| x.via_server.contains(&r));
                        let spare = (rep.video_relay_bps as u64).saturating_sub(used.get(&r).copied().unwrap_or(0));
                        let rtt = good(ru, t)?;
                        good(tu, r)?;
                        (!hidden && spare >= cost).then_some((rtt, r))
                    })
                    .collect();
                cands.sort();
                let keep = self.video_relays.get(&(s, t)).copied().filter(|v| cands.iter().any(|(_, r)| r == v));
                if let Some(via) = keep.or(cands.first().map(|c| c.1)) {
                    plan.insert((s, t), via);
                    *used.entry(via).or_default() += cost;
                }
            }
        }
        self.video_relays = plan;
        let routes: Vec<RelayRoute> = self
            .video_relays
            .iter()
            .map(|((s, t), v)| RelayRoute { from: *s, to: *t, via: *v, ms: 0, server_ms: 0, manual: false })
            .collect();
        for (id, u) in self.users.iter_mut() {
            let mut mine: Vec<RelayRoute> = routes.iter().filter(|r| r.from == *id || r.via == *id || r.to == *id).copied().collect();
            mine.sort();
            if mine != u.last_video_relays {
                u.last_video_relays = mine.clone();
                u.send(ServerMsg::VideoRelays { routes: mine });
            }
        }
    }

    /// 转发一个视频分片给正在观看的人（已 P2P 直连的除外）
    fn forward_video(&mut self, from: u32, frag: &[u8]) -> Vec<(SocketAddr, Vec<u8>)> {
        let Some(u) = self.users.get(&from) else { return Vec::new() };
        let (Some(ch), Some(_)) = (u.channel, &u.sharing) else { return Vec::new() };
        if u.share_p2p_only {
            return Vec::new(); // 共享者选了只走 P2P：服务器不转发也不缓存（正常客户端这时也不会发上来）
        }
        let Some((f, _)) = udp::read_video_frag(frag) else { return Vec::new() };
        // 有低画质层时按层转发：要低画质的只收低画质层，其他人只收原画
        let layer_low = f.flags & udp::VIDEO_LOW != 0;
        let simulcast = u.simulcast;
        let direct = u.direct_video.clone();
        let payload = udp::server_video(from, frag);
        if let Some(u) = self.users.get_mut(&from) {
            u.video_cache.insert(f.flags & udp::VIDEO_LOW, f.frame, f.idx, frag);
        }
        let mut out = Vec::new();
        let relayed: HashSet<u32> = self.video_relays.keys().filter(|(s, _)| *s == from).map(|(_, t)| *t).collect();
        let forced: HashSet<u32> = self.forced_low.iter().filter(|(_, s)| *s == from).map(|(v, _)| *v).collect();
        for (id, other) in self.users.iter_mut() {
            if other.channel != Some(ch) || !other.watching.contains(&from) || direct.contains(id) || relayed.contains(id) {
                continue;
            }
            if layer_low != (simulcast && (other.watch_low.contains(&from) || forced.contains(id))) {
                continue;
            }
            match (other.voice_udp, other.udp) {
                (true, Some(addr)) => {
                    let mut pkt = Vec::with_capacity(payload.len() + 64);
                    other.udp_seal.seal(udp::VIDEO, &payload, &mut pkt);
                    out.push((addr, pkt));
                }
                _ => other.send_media(Out::Video(payload.clone())),
            }
            self.sec_bytes.1 += payload.len() as u64 + 60;
        }
        out
    }

    fn banned(&mut self, ip: std::net::IpAddr) -> bool {
        self.bans.banned(ip, Instant::now())
    }

    fn auth_failed(&mut self, ip: std::net::IpAddr) -> (bool, bool) {
        self.bans.failed(ip, Instant::now())
    }

    /// 每秒：统计转发带宽；超过画面上限时让一个经服务器看原画的人改看「流畅」，宽松 10 秒后放回一个
    fn balance_video_cap(&mut self) {
        self.last_sec_bytes = std::mem::take(&mut self.sec_bytes);
        let Some(cap) = self.max_video_bps else {
            if !self.forced_low.is_empty() {
                self.forced_low.clear();
                self.push_watchers();
            }
            return;
        };
        let rate = self.last_sec_bytes.1 * 8;
        if rate > cap {
            self.cap_calm_secs = 0;
            let mut cands: Vec<(u32, u32)> = Vec::new();
            for (vid, v) in &self.users {
                for s in &v.watching {
                    let Some(su) = self.users.get(s) else { continue };
                    let via_server = !su.direct_video.contains(vid) && !self.video_relays.contains_key(&(*s, *vid));
                    if su.simulcast && via_server && !v.watch_low.contains(s) && !self.forced_low.contains(&(*vid, *s)) {
                        cands.push((*vid, *s));
                    }
                }
            }
            cands.sort();
            if let Some(c) = cands.first() {
                self.forced_low.insert(*c);
                self.push_watchers();
            }
        } else if rate < cap * 6 / 10 && !self.forced_low.is_empty() {
            self.cap_calm_secs += 1;
            if self.cap_calm_secs >= 10 {
                self.cap_calm_secs = 0;
                let mut v: Vec<(u32, u32)> = self.forced_low.iter().copied().collect();
                v.sort();
                self.forced_low.remove(&v[v.len() - 1]);
                self.push_watchers();
            }
        } else {
            self.cap_calm_secs = 0;
        }
        // 已经不看了的去掉
        let users = &self.users;
        self.forced_low.retain(|(v, s)| users.get(v).is_some_and(|u| u.watching.contains(s)));
    }

    /// 同频道、两人都开了 P2P（且没有对对方隐藏 IP）的对数、其中语音直连的对数、没直连的按 NAT 类型组合分
    fn p2p_pairs(&self) -> (u32, u32, HashMap<String, u32>) {
        fn nat_name(n: Option<&NatInfo>) -> &'static str {
            match n.map(|n| n.kind) {
                Some(voc_proto::NAT_OPEN) => "公网",
                Some(voc_proto::NAT_CONE) => "锥形",
                Some(voc_proto::NAT_SYM_PREDICTABLE) => "对称可预测",
                Some(voc_proto::NAT_SYM_RANDOM) => "对称随机",
                _ => "未知",
            }
        }
        let mut ids: Vec<&u32> = self.users.keys().collect();
        ids.sort();
        let (mut pairs, mut direct, mut failed) = (0u32, 0u32, HashMap::new());
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                let (ua, ub) = (&self.users[*a], &self.users[*b]);
                if ua.channel.is_none() || ua.channel != ub.channel || ua.p2p.is_none() || ub.p2p.is_none() {
                    continue;
                }
                if ua.p2p_hide.contains(*b) || ub.p2p_hide.contains(*a) {
                    continue;
                }
                pairs += 1;
                if ua.direct.contains(*b) || ub.direct.contains(*a) {
                    direct += 1;
                } else {
                    let mut k = [nat_name(ua.nat.as_ref()), nat_name(ub.nat.as_ref())];
                    k.sort();
                    *failed.entry(format!("{} × {}", k[0], k[1])).or_insert(0) += 1;
                }
            }
        }
        (pairs, direct, failed)
    }

    /// 状态文件内容
    fn status_text(&self, port: u16) -> String {
        use std::fmt::Write;
        let mut t = String::new();
        let _ = writeln!(t, "VOC 服务器 {} · 「{}」", env!("CARGO_PKG_VERSION"), self.server_name);
        let probe = if self.probe_listening { format!("NAT 探测 UDP {}（监听中）", self.probe_port) } else { "NAT 探测端口没开".into() };
        let _ = writeln!(t, "端口：TCP/UDP {port} · {probe}");
        let mbps = |b: u64| b as f64 * 8.0 / 1e6;
        let cap = self.max_video_bps.map(|c| format!("上限 {} Mbps", fmt_mbps(c))).unwrap_or_else(|| "不限".into());
        let _ = writeln!(
            t,
            "最近 1 秒服务器转发：语音 {:.2} Mbps · 画面 {:.2} Mbps（画面{cap}）",
            mbps(self.last_sec_bytes.0),
            mbps(self.last_sec_bytes.1)
        );
        let _ = writeln!(t, "在线 {} 人", self.users.len());
        let mut chans: Vec<&ChannelInfo> = self.channels.iter().collect();
        chans.sort_by_key(|c| c.id);
        for c in chans {
            let mut names: Vec<String> = self
                .users
                .values()
                .filter(|u| u.channel == Some(c.id))
                .map(|u| {
                    let mut n = u.name.clone();
                    if u.deafened {
                        n += "（静音）";
                    } else if u.muted {
                        n += "（闭麦）";
                    }
                    if let Some(d) = &u.sharing {
                        n += &format!("（共享 {d}）");
                    }
                    n
                })
                .collect();
            if names.is_empty() {
                continue;
            }
            names.sort();
            let _ = writeln!(t, "  「{}」：{}", c.name, names.join("、"));
        }
        for (id, u) in &self.users {
            if u.sharing.is_none() {
                continue;
            }
            let watchers: Vec<u32> = self.users.iter().filter(|(_, v)| v.watching.contains(id)).map(|(i, _)| *i).collect();
            let direct = watchers.iter().filter(|w| u.direct_video.contains(w)).count();
            let relayed = watchers.iter().filter(|w| self.video_relays.contains_key(&(*id, **w))).count();
            let _ = writeln!(
                t,
                "  {} 的屏幕：{} 人在看（直连 {direct}，观看者转发 {relayed}，经服务器 {}）",
                u.name,
                watchers.len(),
                watchers.len() - direct - relayed
            );
        }
        if !self.forced_low.is_empty() {
            let _ = writeln!(t, "因为画面带宽上限临时改看「流畅」的：{} 人", self.forced_low.len());
        }
        // P2P 直连情况（第 47 项）：只数次数和 NAT 类型组合，不写昵称、不写 IP
        let (pairs, direct, failed) = self.p2p_pairs();
        if pairs > 0 {
            let mut f: Vec<(String, u32)> = failed.into_iter().collect();
            f.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            let fails = if f.is_empty() { String::new() } else { format!("；没直连的：{}", f.iter().map(|(k, n)| format!("{k} {n} 对")).collect::<Vec<_>>().join("、")) };
            let _ = writeln!(t, "P2P：现在语音直连 {direct} / {pairs} 对（{}%）{fails}", direct * 100 / pairs);
        }
        if self.p2p_samples.0 > 0 {
            let _ = writeln!(t, "P2P：启动以来语音直连率 {}%（按时间算）", self.p2p_samples.1 * 100 / self.p2p_samples.0);
        }
        if let Some(line) = punch_days_text(&self.punch_days) {
            let _ = writeln!(t, "{line}");
        }
        if let Some(line) = p2p_days_text(&self.p2p_days) {
            let _ = writeln!(t, "P2P：最近 {line}");
        }
        let banned = self.bans.count(Instant::now());
        if banned > 0 {
            let _ = writeln!(t, "封禁中的 IP：{banned} 个（认证失败太多次）");
        }
        t
    }

    /// 观看者的补发请求：缓存里有的直接重发给他，没有的转给共享者（共享者重发后照常转发）
    fn on_nack(&mut self, uid: u32, data: &[u8]) -> Vec<(SocketAddr, Vec<u8>)> {
        let mut out = Vec::new();
        let Some((target, layer, frame, idxs)) = udp::read_nack(data) else { return out };
        let (Some(me), Some(t)) = (self.users.get(&uid), self.users.get(&target)) else { return out };
        if !me.watching.contains(&target) || t.sharing.is_none() {
            return out;
        }
        let (found, missing) = t.video_cache.lookup(layer, frame, &idxs);
        let sharer = (t.rtx, t.voice_udp, t.udp);
        let me = self.users.get_mut(&uid).unwrap();
        if let (true, Some(addr)) = (me.voice_udp, me.udp) {
            for f in found {
                let mut pkt = Vec::new();
                me.udp_seal.seal(udp::VIDEO, &udp::server_video(target, &f), &mut pkt);
                out.push((addr, pkt));
            }
        }
        if let (false, (true, true, Some(addr))) = (missing.is_empty(), sharer) {
            let t = self.users.get_mut(&target).unwrap();
            let mut pkt = Vec::new();
            t.udp_seal.seal(udp::NACK, &udp::nack_payload(uid, layer, frame, &missing), &mut pkt);
            out.push((addr, pkt));
        }
        out
    }

    /// 把一帧语音转发给同频道成员。TCP 隧道用户直接塞进其发送队列；返回需要发出的 UDP 包。
    fn forward_voice(&mut self, from: u32, seq: u16, frame: &[u8]) -> Vec<(SocketAddr, Vec<u8>)> {
        let Some(u) = self.users.get(&from) else { return Vec::new() };
        let (Some(ch), false) = (u.channel, u.muted) else { return Vec::new() };
        let direct = u.direct.clone();
        let relayed: HashSet<u32> = self.relays.keys().filter(|(f, _)| *f == from).map(|(_, t)| *t).collect();
        let payload = udp::server_voice(from, seq, frame);
        let mut out = Vec::new();
        for (id, other) in self.users.iter_mut() {
            // 语音优先改经服务器的：不管名单上怎么说都转（说话的人那边也会改发给服务器）
            let fallback = self.voice_fallback.contains_key(&(from, *id));
            if *id == from || other.channel != Some(ch) || other.deafened || (direct.contains(id) && !fallback) {
                continue;
            }
            if relayed.contains(id) && !fallback {
                continue; // 由成员中继转发
            }
            self.sec_bytes.0 += payload.len() as u64 + 60;
            match (other.voice_udp, other.udp) {
                (true, Some(addr)) => {
                    let mut pkt = Vec::with_capacity(payload.len() + 64);
                    other.udp_seal.seal(udp::VOICE, &payload, &mut pkt);
                    out.push((addr, pkt));
                    // 这个听众经服务器收语音丢包高：只给他多发一份（另封一次，接收端按序号去重）
                    if other.stats.as_ref().is_some_and(|r| r.voice_loss >= VOICE_DUP_LOSS && r.at.elapsed() < STATS_TTL) {
                        let mut dup = Vec::with_capacity(payload.len() + 64);
                        other.udp_seal.seal(udp::VOICE, &payload, &mut dup);
                        out.push((addr, dup));
                        self.sec_bytes.0 += payload.len() as u64 + 60;
                    }
                }
                _ => other.send_media(Out::Voice(payload.clone())),
            }
        }
        out
    }
}

// ------------------------------------------------------------ 命令行

struct Config {
    bind: SocketAddr,
    /// 服务器转发屏幕共享画面的总带宽上限（bps），None = 不限
    max_video_bps: Option<u64>,
    /// NAT 探测端口，None = 主端口 + 1，Some(0) = 不开
    probe_port: Option<u16>,
    /// 第二个探测点：(绑定的 IP, 告诉客户端的公网 IP)
    probe2: Option<(std::net::IpAddr, std::net::IpAddr)>,
    name: String,
    channels: Vec<String>,
    codes: PathBuf,
}

fn die(msg: &str) -> ! {
    eprintln!("错误：{msg}");
    std::process::exit(2);
}

const HELP: &str = "\
VOC 服务器

运行服务器：
  voc-server [-b IP:端口] [-n 名称] [-c 频道1,频道2] [--codes 文件]
    -b, --bind       监听地址（TCP+UDP 同端口），默认 0.0.0.0:7878
    -n, --name       服务器名称
    -c, --channels   初始语音频道
        --codes      授权码文件，默认 voc-codes.json
        --probe-port NAT 探测用的 UDP 端口，默认主端口 + 1（7879），0 = 不开
        --probe2 IP[/公网IP] 服务器有第二个公网 IP 时：在它上面也开探测端口，客户端能更准地判断 NAT（换了对方 IP 映射变不变、
                   没联系过的地址能不能进来）。云服务器上绑的是内网 IP 时用「内网IP/公网IP」
        --max-video-mbps  服务器转发屏幕共享画面的总带宽上限（Mbps），超出时让部分观看者自动看「流畅」；默认不限

查看运行状态（在线的人、谁在共享、转发带宽、NAT 探测端口）：
  voc-server status [--codes 文件]

管理授权码（服务器运行中也可以执行，立即生效）：
  voc-server gen-code [-n 数量] [--note 备注] [--days 有效天数] [--codes 文件]
  voc-server list-codes [--codes 文件]
  voc-server revoke-code <ID>   [--codes 文件]   吊销（在线的人会被立即踢下线）
  voc-server restore-code <ID>  [--codes 文件]   取消吊销

换服务器时搬授权码（大家原来的授权码在新服务器上照样能用）：
  voc-server export-users [--codes 文件] > 文件   导出全部授权码，用你设的密码加密（输出是一串看不出格式的字符）
  voc-server import-users [文件] [--codes 文件]    在新服务器上导入（不带文件 = 从标准输入读），已有的授权码不动
  密码在终端里输入（不显示）；脚本里用可以设环境变量 VOC_USERS_PASSWORD

  voc-server --version   显示版本";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut codes_path = PathBuf::from("voc-codes.json");
    let mut rest = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == "--codes" {
            codes_path = it.next().unwrap_or_else(|| die("--codes 需要一个参数")).into();
        } else if a == "-h" || a == "--help" {
            println!("{HELP}");
            return;
        } else if a == "-V" || a == "--version" {
            println!("voc-server {}", env!("CARGO_PKG_VERSION"));
            return;
        } else {
            rest.push(a);
        }
    }

    match rest.first().map(String::as_str) {
        Some("gen-code") => cmd_gen(&codes_path, &rest[1..]),
        Some("status") => cmd_status(&codes_path),
        Some("list-codes") => cmd_list(&codes_path),
        Some("export-users") => cmd_export_users(&codes_path),
        Some("import-users") => cmd_import_users(&codes_path, rest.get(1).map(String::as_str)),
        Some("revoke-code") | Some("restore-code") => {
            let id = rest.get(1).unwrap_or_else(|| die("需要授权码 ID"));
            let revoke = rest[0] == "revoke-code";
            match codes::set_revoked(&codes_path, id, revoke) {
                Ok(()) => println!("已{}授权码 {id}", if revoke { "吊销" } else { "恢复" }),
                Err(e) => die(&e),
            }
        }
        _ => {
            let cfg = parse_run(&rest, codes_path);
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio")
                .block_on(run(cfg))
                .unwrap_or_else(|e| die(&e.to_string()));
        }
    }
}

fn parse_run(args: &[String], codes: PathBuf) -> Config {
    let mut cfg = Config {
        bind: SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT)),
        probe_port: None,
        probe2: None,
        max_video_bps: None,
        name: "VOC 服务器".into(),
        channels: vec!["大厅".into(), "开黑一号".into(), "开黑二号".into(), "挂机".into()],
        codes,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().unwrap_or_else(|| die(&format!("{a} 需要一个参数")));
        match a.as_str() {
            "--bind" | "-b" => cfg.bind = val().parse().unwrap_or_else(|_| die("--bind 格式应为 IP:端口")),
            "--name" | "-n" => cfg.name = val(),
            "--probe-port" => cfg.probe_port = Some(val().parse().unwrap_or_else(|_| die("--probe-port 必须是端口号"))),
            "--probe2" => {
                let v = val();
                let (b, p) = v.split_once('/').unwrap_or((v.as_str(), v.as_str()));
                let ip = |s: &str| s.trim().parse::<std::net::IpAddr>().unwrap_or_else(|_| die("--probe2 要写 IP 或「内网IP/公网IP」"));
                cfg.probe2 = Some((ip(b), ip(p)));
            }
            "--max-video-mbps" => {
                let m: f64 = val().parse().unwrap_or_else(|_| die("--max-video-mbps 必须是数字"));
                cfg.max_video_bps = (m > 0.0).then_some((m * 1_000_000.0) as u64);
            }
            "--channels" | "-c" => {
                cfg.channels = val().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
            }
            other => die(&format!("未知参数：{other}（用 --help 查看）")),
        }
    }
    if cfg.channels.is_empty() {
        cfg.channels.push("大厅".into());
    }
    cfg
}

fn cmd_gen(path: &std::path::Path, args: &[String]) {
    let (mut n, mut note, mut days) = (1usize, String::new(), None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().unwrap_or_else(|| die(&format!("{a} 需要一个参数")));
        match a.as_str() {
            "-n" | "--count" => n = val().parse().unwrap_or_else(|_| die("数量必须是数字")),
            "--note" => note = val(),
            "--days" => days = Some(val().parse().unwrap_or_else(|_| die("天数必须是数字"))),
            other => die(&format!("未知参数：{other}")),
        }
    }
    let codes = codes::generate(path, n.clamp(1, 1000), &note, days).unwrap_or_else(|e| die(&e));
    println!("已生成 {} 个授权码（只显示这一次，请妥善发给对应的人）：", codes.len());
    for c in codes {
        println!("{}", c.display());
    }
}

/// 读密码：环境变量 VOC_USERS_PASSWORD（脚本用）→ 终端（不显示输入）→ 标准输入的一行
fn read_password(prompt: &str) -> String {
    if let Ok(p) = std::env::var("VOC_USERS_PASSWORD") {
        return p;
    }
    use std::io::{BufRead, Write};
    #[cfg(unix)]
    if let Ok(tty) = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty") {
        let stty = |arg: &str| {
            let _ = std::process::Command::new("stty").arg(arg).stdin(tty.try_clone().expect("tty")).status();
        };
        let mut w = &tty;
        let _ = write!(w, "{prompt}");
        let _ = w.flush();
        stty("-echo");
        let mut line = String::new();
        let r = std::io::BufReader::new(&tty).read_line(&mut line);
        stty("echo");
        let _ = writeln!(w);
        if r.is_ok() {
            return line.trim_end_matches(['\r', '\n']).to_string();
        }
    }
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).unwrap_or_else(|e| die(&format!("读不到密码：{e}")));
    line.trim_end_matches(['\r', '\n']).to_string()
}

fn cmd_export_users(path: &std::path::Path) {
    let min = voc_proto::backup::MIN_PASSWORD_CHARS;
    let pw = read_password(&format!("设一个导出密码（至少 {min} 个字，导入时要用）："));
    if pw.chars().count() < min {
        die(&format!("密码至少 {min} 个字"));
    }
    if std::env::var_os("VOC_USERS_PASSWORD").is_none() && read_password("再输一次：") != pw {
        die("两次密码不一样");
    }
    let (text, n) = codes::export_users(path, &pw).unwrap_or_else(|e| die(&e));
    println!("{text}");
    eprintln!("已导出 {n} 个授权码（含备注、到期、吊销状态）。");
    eprintln!("在新服务器上：sudo voc import-users 这个文件，输同一个密码。文件和密码请分开保管：拿到两样的人能冒充任何一个授权码登录。");
}

/// 文本文件转字符串：Windows PowerShell 5 的 `>` 存成 UTF-16（带 BOM），记事本可能带 UTF-8 BOM
fn decode_text(b: &[u8]) -> String {
    if let Some(rest) = b.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&units);
    }
    let b = b.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(b);
    String::from_utf8_lossy(b).into_owned()
}

fn cmd_import_users(path: &std::path::Path, file: Option<&str>) {
    use std::io::Read;
    let bytes = match file {
        Some(f) if f != "-" => std::fs::read(f).unwrap_or_else(|e| die(&format!("读不了 {f}：{e}"))),
        _ => {
            let mut t = Vec::new();
            std::io::stdin().read_to_end(&mut t).unwrap_or_else(|e| die(&format!("读不到导出内容：{e}")));
            t
        }
    };
    let text = decode_text(&bytes);
    if text.trim().is_empty() {
        die("导出内容是空的。用法：voc-server import-users 文件（或从标准输入传进来）");
    }
    let pw = read_password("导出时设的密码：");
    let r = codes::import_users(path, &text, &pw).unwrap_or_else(|e| die(&e));
    println!("导入完成：新加 {} 个授权码，{} 个本来就有（没动）。", r.added, r.same);
    if !r.conflict.is_empty() {
        println!("跳过 {} 个：本服务器已有同一个 ID 但密钥不同（没覆盖）：{}", r.conflict.len(), r.conflict.join(", "));
    }
    if !r.broken.is_empty() {
        println!("跳过 {} 个：导出内容里的密钥损坏：{}", r.broken.len(), r.broken.join(", "));
    }
    if r.added > 0 {
        println!("服务器在运行的话几秒内自动生效，不用重启。用 `list-codes` 查看。");
    }
}

/// 运行中的服务器每 5 秒写一次的状态文件（在授权码文件旁边）
fn status_path(codes: &std::path::Path) -> PathBuf {
    let stem = codes.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "voc".into());
    codes.with_file_name(format!("{stem}.status.txt"))
}

fn cmd_status(codes: &std::path::Path) {
    let p = status_path(codes);
    match std::fs::read_to_string(&p) {
        Ok(t) => {
            let age = std::fs::metadata(&p).and_then(|m| m.modified()).ok().and_then(|m| m.elapsed().ok()).map(|d| d.as_secs()).unwrap_or(0);
            print!("{t}");
            if age > 15 {
                println!("（这是 {age} 秒前的状态：服务器可能没在运行）");
            }
        }
        Err(_) => println!("没有状态文件 {}：服务器没在运行，或者版本太旧（0.5.1 起才有）", p.display()),
    }
}

fn cmd_list(path: &std::path::Path) {
    let entries = codes::read_file(path).unwrap_or_else(|e| die(&e));
    if entries.is_empty() {
        println!("还没有授权码。用 `voc-server gen-code` 生成。");
        return;
    }
    let now = now_millis();
    println!("{:<10} {:<8} {:<12} 备注", "ID", "状态", "到期");
    for e in entries {
        let status = match e.check(now) {
            Ok(()) => "可用",
            Err(_) if e.revoked => "已吊销",
            Err(_) => "已过期",
        };
        let exp = e.expires.map(|x| format!("{} 天后", (x.saturating_sub(now)) / 86_400_000)).unwrap_or("永久".into());
        println!("{:<10} {:<8} {:<12} {}", e.id, status, exp, e.note);
    }
}

// ------------------------------------------------------------ 运行

async fn run(cfg: Config) -> std::io::Result<()> {
    let codes = CodeStore::open(cfg.codes.clone()).map_err(std::io::Error::other)?;
    let tcp = TcpListener::bind(cfg.bind).await?;
    let udp_sock = Arc::new(bind_udp(cfg.bind)?);
    let port = tcp.local_addr()?.port();
    // NAT 探测端口：开不了（被占用等）不影响主服务
    let probe_port = cfg.probe_port.unwrap_or(port.wrapping_add(1));
    let probe_sock = if probe_port == 0 {
        None
    } else {
        match bind_udp(SocketAddr::new(cfg.bind.ip(), probe_port)) {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                eprintln!("[VOC] NAT 探测端口 UDP {probe_port} 开不了（{e}），客户端将无法准确判断 NAT 类型");
                None
            }
        }
    };

    // 第二个公网 IP 上的探测点（第 39-2 项）
    let probe2_sock = match (cfg.probe2, &probe_sock) {
        (Some((bind, _)), Some(p1)) => match bind_udp(SocketAddr::new(bind, p1.local_addr()?.port())) {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                eprintln!("[VOC] 第二个探测点 {bind} 开不了（{e}）");
                None
            }
        },
        _ => None,
    };
    let probe2_text = match (&probe2_sock, cfg.probe2) {
        (Some(s), Some((_, public))) => SocketAddr::new(public, s.local_addr()?.port()).to_string(),
        _ => String::new(),
    };

    let channels: Vec<ChannelInfo> =
        cfg.channels.iter().enumerate().map(|(i, n)| ChannelInfo { id: i as u32 + 1, name: n.clone() }).collect();
    let n_codes = codes.entries.iter().filter(|e| e.check(now_millis()).is_ok()).count();
    let state: Shared = Arc::new(Mutex::new(State {
        server_name: cfg.name.clone(),
        udp_port: udp_sock.local_addr()?.port(),
        probe_port: match &probe_sock {
            Some(s) => s.local_addr()?.port(),
            None => 0,
        },
        next_user: 1,
        next_channel: channels.len() as u32 + 1,
        channels,
        users: HashMap::new(),
        addr_map: HashMap::new(),
        codes,
        replay: ReplayCache::default(),
        relays: HashMap::new(),
        video_relays: HashMap::new(),
        voice_misses: HashMap::new(),
        voice_fallback: HashMap::new(),
        sec_bytes: (0, 0),
        last_sec_bytes: (0, 0),
        p2p_samples: (0, 0),
        p2p_days: Vec::new(),
        punch_days: Vec::new(),
        punch_seen: HashSet::new(),
        next_conn_gen: 0,
        max_video_bps: cfg.max_video_bps,
        forced_low: HashSet::new(),
        cap_calm_secs: 0,
        bans: BanList::default(),
        probe_listening: false,
        probe2: probe2_text,
    }));

    println!("[VOC] 服务器「{}」已启动：TCP/UDP {}", cfg.name, cfg.bind);
    println!("[VOC] 频道：{}", cfg.channels.join(" / "));
    println!("[VOC] 授权码文件：{}（可用 {n_codes} 个）", cfg.codes.display());
    if n_codes == 0 {
        println!("[VOC] 还没有可用的授权码，另开一个窗口运行：voc-server gen-code --note 名字");
    }
    println!("[VOC] 加密：ML-KEM-768 + ChaCha20-Poly1305，全流量随机化；放行 TCP 与 UDP 端口 {port}");

    tokio::spawn(udp_loop(state.clone(), udp_sock.clone()));
    state.lock().unwrap().probe_listening = probe_sock.is_some();
    tokio::spawn(status_loop(state.clone(), status_path(&cfg.codes), port));
    if let Some(p) = probe_sock {
        println!("[VOC] NAT 探测：UDP {}（云服务器的安全组也要放行这个 UDP 端口）", p.local_addr()?.port());
        if let Some(p2) = &probe2_sock {
            println!("[VOC] 第二个探测点：{}", p2.local_addr()?);
            tokio::spawn(probe_loop(state.clone(), p2.clone(), Some(p.clone())));
        }
        tokio::spawn(probe_loop(state.clone(), p, probe2_sock));
    }
    tokio::spawn(revocation_loop(state.clone()));
    tokio::spawn(relay_loop(state.clone()));
    let pre_auth = PreAuth::default();

    loop {
        tokio::select! {
            r = tcp.accept() => {
                let Ok((stream, addr)) = r else { continue };
                // 未认证（还在握手 / 被吞数据）的连接总数和每个 IP 的数量有上限：
                // 大量并发握手不会把服务器拖垮；正常用户握手很快就认证完、不占名额，同一公网 IP 下多人也不会被误伤
                let Some(guard) = pre_auth.enter(addr.ip()) else {
                    drop(stream);
                    continue;
                };
                // 被封的 IP：不做任何处理，吞掉数据后关闭（和探测得到的反应一样）
                if state.lock().unwrap().banned(addr.ip()) {
                    tokio::spawn(async move {
                        let (rd, wr) = stream.into_split();
                        sink(rd).await;
                        drop(wr);
                        drop(guard);
                    });
                    continue;
                }
                let _ = stream.set_nodelay(true);
                let (st, us) = (state.clone(), udp_sock.clone());
                tokio::spawn(async move {
                    if let Err(e) = handle_client(st, us, stream, addr, guard).await {
                        eprintln!("[{addr}] {e}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                println!("[VOC] 正在关闭");
                notify_restarting(&state).await;
                return Ok(());
            }
            _ = terminate() => {
                println!("[VOC] 收到停止信号（systemctl stop / restart / 升级），正在关闭");
                notify_restarting(&state).await;
                return Ok(());
            }
        }
    }
}

/// 服务器要停了：告诉在线的人「服务器正在重启」，等一小会儿让消息发出去（第 53-5 项）。
/// 客户端看到后断线时显示这句、立刻重连，不会以为是自己网络的问题
async fn notify_restarting(state: &Shared) {
    let n = {
        let st = state.lock().unwrap();
        st.broadcast(&ServerMsg::Restarting { text: "服务器正在重启（可能是在升级），几秒后会自动重新连上".into() });
        st.users.len()
    };
    if n > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
}

/// systemd 停止服务时发的 SIGTERM（Windows 上没有，只等 Ctrl+C）
#[cfg(unix)]
async fn terminate() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(_) => std::future::pending::<()>().await,
    }
}
#[cfg(not(unix))]
async fn terminate() {
    std::future::pending::<()>().await
}

/// 断线后保留多久等客户端恢复（第 37-2 项）
const RESUME_GRACE: Duration = Duration::from_secs(20);

/// 客户端要求恢复、但没接上（凭证不对 / 已经过期）
fn resume_is_failed(resumed: &Option<u32>, hello: &[u8]) -> bool {
    resumed.is_none() && matches!(serde_json::from_slice::<ClientMsg>(hello), Ok(ClientMsg::Hello { resume: Some(_), .. }))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 未认证连接最多同时这么多个（全服务器）
const MAX_PRE_AUTH: usize = 256;
/// 同一个 IP 最多同时这么多个未认证连接（同一公网 IP 下的正常用户握手很快，不会同时占这么多）
const MAX_PRE_AUTH_PER_IP: usize = 16;

/// 未认证连接计数
#[derive(Default, Clone)]
struct PreAuth(Arc<Mutex<(usize, HashMap<std::net::IpAddr, usize>)>>);

/// 占着一个未认证连接名额，drop 时归还
struct PreAuthGuard {
    owner: PreAuth,
    ip: std::net::IpAddr,
}

impl PreAuth {
    fn enter(&self, ip: std::net::IpAddr) -> Option<PreAuthGuard> {
        let ip = match ip {
            std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map(std::net::IpAddr::V4).unwrap_or(std::net::IpAddr::V6(v6)),
            v4 => v4,
        };
        let mut g = self.0.lock().unwrap();
        let per_ip = g.1.get(&ip).copied().unwrap_or(0);
        if g.0 >= MAX_PRE_AUTH || per_ip >= MAX_PRE_AUTH_PER_IP {
            return None;
        }
        g.0 += 1;
        *g.1.entry(ip).or_default() += 1;
        Some(PreAuthGuard { owner: self.clone(), ip })
    }
}

impl Drop for PreAuthGuard {
    fn drop(&mut self) {
        let mut g = self.owner.0.lock().unwrap();
        g.0 = g.0.saturating_sub(1);
        if let Some(n) = g.1.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                g.1.remove(&self.ip);
            }
        }
    }
}

/// UDP 套接字：加大收发缓冲（屏幕共享的关键帧是几十个包一起到）
fn bind_udp(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    let _ = s.set_recv_buffer_size(4 << 20);
    let _ = s.set_send_buffer_size(4 << 20);
    s.bind(&addr.into())?;
    s.set_nonblocking(true)?;
    UdpSocket::from_std(s.into())
}

/// 无法认证的连接：不回任何字节，吞掉对方数据一段随机时间再关闭，让探测者看不出这是什么服务
async fn sink(mut rd: OwnedReadHalf) {
    let secs = 20 + (u16::from_le_bytes(random_bytes::<2>()) % 40) as u64;
    let _ = tokio::time::timeout(Duration::from_secs(secs), async {
        let mut buf = [0u8; 4096];
        while let Ok(n) = rd.read(&mut buf).await {
            if n == 0 {
                break;
            }
        }
    })
    .await;
}

/// 发出最后一帧后优雅关闭：先关写端，再把对方已发来的数据读完。
/// 直接 drop 的话，接收缓冲里有未读数据时 Windows 会发 RST，对方可能收不到这最后一帧。
async fn close_with(mut rd: OwnedReadHalf, mut wr: tokio::net::tcp::OwnedWriteHalf, last: &[u8]) {
    let _ = wr.write_all(last).await;
    let _ = wr.shutdown().await;
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        let mut buf = [0u8; 4096];
        while let Ok(n) = rd.read(&mut buf).await {
            if n == 0 {
                break;
            }
        }
    })
    .await;
}

async fn read_frame(rd: &mut OwnedReadHalf, op: &mut FrameOpener) -> Option<(u8, Vec<u8>)> {
    let mut h = [0u8; FRAME_LEN_LEN];
    rd.read_exact(&mut h).await.ok()?;
    let n = op.open_len(&h)?;
    let mut b = vec![0u8; n];
    rd.read_exact(&mut b).await.ok()?;
    op.open_body(&b)
}

async fn handle_client(state: Shared, udp_sock: Arc<UdpSocket>, stream: TcpStream, addr: SocketAddr, pre_auth: PreAuthGuard) -> Result<(), String> {
    let (mut rd, mut wr) = stream.into_split();
    // 握手完成前一直占着「未认证连接」的名额（无法认证的连接在 sink 里耗到超时才放）
    let pre_auth = Some(pre_auth);

    // ---- 握手 ----
    let mut prefix = [0u8; CLIENT_PREFIX_LEN];
    if !matches!(tokio::time::timeout(Duration::from_secs(15), rd.read_exact(&mut prefix)).await, Ok(Ok(_))) {
        return Ok(()); // 没发够数据：静默关闭
    }
    let now = crypto::unix_secs();
    let ident = {
        let mut st = state.lock().unwrap();
        st.codes.refresh();
        match server_identify(&prefix, &st.codes.psks, now) {
            Ok(p) if st.replay.insert(p.salt, now) => {
                let psk = st.codes.psks[p.index];
                let entry = st.codes.entries[p.index].clone();
                Ok((p, psk, entry))
            }
            Ok(_) => Err("重放的握手"),
            Err(HandshakeError::Stale) => Err("时间戳超出窗口（重放或客户端时间不准）"),
            Err(_) => Err("无法认证（错误授权码或探测）"),
        }
    };
    let (p, psk, entry) = match ident {
        Ok(x) => x,
        Err(why) => {
            // 同一个 IP 1 分钟内只记第一次，免得扫描刷屏；失败太多次就封几分钟
            let (log, banned) = state.lock().unwrap().auth_failed(addr.ip());
            if log {
                println!("[{addr}] 拒绝：{why}");
            }
            if banned {
                println!("[VOC] {} 1 分钟内认证失败 {BAN_FAILS} 次，封 {} 分钟", addr.ip(), BAN_FOR.as_secs() / 60);
            }
            sink(rd).await;
            return Ok(());
        }
    };
    let mut rest = vec![0u8; p.remaining()];
    if !matches!(tokio::time::timeout(Duration::from_secs(15), rd.read_exact(&mut rest)).await, Ok(Ok(_))) {
        return Ok(());
    }
    let Ok((reply, keys)) = p.respond(&rest, &psk) else {
        sink(rd).await;
        return Ok(());
    };
    wr.write_all(&reply).await.map_err(|e| e.to_string())?;
    drop(pre_auth); // 已认证：不再占未认证连接的名额

    let mut sealer = FrameSealer::new(&keys.tcp_s2c);
    let mut opener = FrameOpener::new(&keys.tcp_c2s);
    let reject = |sealer: &mut FrameSealer, msg: String| {
        sealer.seal(KIND_JSON, serde_json::to_string(&ServerMsg::Error { message: msg }).unwrap().as_bytes())
    };

    if let Err(e) = entry.check(now_millis()) {
        close_with(rd, wr, &reject(&mut sealer, e.clone())).await;
        return Err(format!("授权码 {}：{e}", entry.id));
    }

    // ---- 登录 ----
    let hello = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut rd, &mut opener))
        .await
        .ok()
        .flatten()
        .ok_or("登录超时")?;
    let (name, version, can_resume, resume) = match (hello.0, serde_json::from_slice::<ClientMsg>(&hello.1)) {
        (KIND_JSON, Ok(ClientMsg::Hello { name, version, can_resume, resume })) => (name.trim().to_string(), version, can_resume, resume),
        _ => return Err("第一条消息必须是 hello".into()),
    };
    let err = {
        let st = state.lock().unwrap();
        if version != PROTOCOL_VERSION {
            Some(voc_proto::version_mismatch_text(PROTOCOL_VERSION, version))
        } else if name.is_empty() || name.chars().count() > MAX_NAME_LEN {
            Some(format!("昵称长度需在 1~{MAX_NAME_LEN} 个字符之间"))
        } else if st.users.values().any(|u| u.name == name && u.code_id != entry.id) {
            Some("该昵称已被使用".into())
        } else {
            None
        }
    };
    if let Some(e) = err {
        close_with(rd, wr, &reject(&mut sealer, e.clone())).await;
        return Err(e);
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
    let kick = Arc::new(Notify::new());
    let pending_file = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let pending_media = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let conn_gen = {
        let mut st = state.lock().unwrap();
        st.next_conn_gen += 1;
        st.next_conn_gen
    };
    let resume_token = if can_resume { hex(&voc_proto::auth::random_bytes::<16>()) } else { String::new() };
    // 断线恢复（第 37-2 项）：凭证对、授权码一样 → 接回原来的身份（编号、频道、UDP 密钥不变）。旧连接可能还没发现断了：踢掉它
    let resumed = resume.and_then(|r| {
        let mut st = state.lock().unwrap();
        let u = st.users.get_mut(&r.id).filter(|u| !u.resume_token.is_empty() && u.resume_token == r.token && u.code_id == entry.id)?;
        let old_kick = std::mem::replace(&mut u.kick, kick.clone());
        u.tx = tx.clone();
        u.conn_gen = conn_gen;
        u.detached = None;
        u.pending_file = pending_file.clone();
        u.pending_media = pending_media.clone();
        u.last_peers.clear();
        u.last_watchers = Default::default();
        u.last_relays.clear();
        old_kick.notify_one();
        let _ = tx.send(Out::Json(ServerMsg::Resumed { user_id: r.id }));
        let snap = st.snapshot();
        let _ = tx.send(Out::Json(snap));
        st.push_peers();
        st.push_watchers();
        st.push_relays();
        Some(r.id)
    });
    if resume_is_failed(&resumed, &hello.1) {
        close_with(rd, wr, &reject(&mut sealer, voc_proto::RESUME_FAILED.into())).await;
        return Ok(());
    }
    let user_id = if let Some(id) = resumed {
        println!("[{addr}] {name} (#{id}) 断线后恢复了连接");
        id
    } else {
        let mut st = state.lock().unwrap();
        // 同一授权码只允许一个在线：踢掉旧的
        let old: Vec<u32> = st.users.iter().filter(|(_, u)| u.code_id == entry.id).map(|(id, _)| *id).collect();
        for id in old {
            if let Some(u) = st.remove_user(id) {
                u.send(ServerMsg::Error { message: "你的授权码在其他地方登录了".into() });
                u.kick.notify_one();
            }
        }
        let id = st.next_user;
        st.next_user += 1;
        let _ = tx.send(Out::Json(ServerMsg::Welcome {
            user_id: id,
            udp_port: st.udp_port,
            server_name: st.server_name.clone(),
            features: vec![
                voc_proto::FEATURE_DM_FILE.into(),
                voc_proto::FEATURE_PONG_ADDR.into(),
                voc_proto::FEATURE_VIDEO_RTX.into(),
                voc_proto::FEATURE_SHARE_AUDIO_ROUTE.into(),
                voc_proto::FEATURE_PUNCH.into(),
            ],
            probe_port: st.probe_port,
            probe2: st.probe2.clone(),
            resume: resume_token.clone(),
        }));
        st.users.insert(
            id,
            User {
                name: name.clone(),
                code_id: entry.id.clone(),
                channel: None,
                muted: false,
                deafened: false,
                tx,
                kick: kick.clone(),
                udp: None,
                udp_open: DatagramOpener::new(&keys.udp_c2s, &keys.udp_c2s_hp),
                udp_seal: DatagramSealer::new(&keys.udp_s2c, &keys.udp_s2c_hp),
                voice_udp: false, // 等客户端确认 UDP 通了再切过去
                p2p: None,
                nat: None,
                p2p_hide: HashSet::new(),
                e2e: None,
                direct: HashSet::new(),
                direct_video: HashSet::new(),
                direct_share_audio: None,
                resume_token: resume_token.clone(),
                conn_gen,
                detached: None,
                last_peers: Vec::new(),
                sharing: None,
                watching: HashSet::new(),
                watch_low: HashSet::new(),
                watch_route: HashMap::new(),
                watch_cursor: HashSet::new(),
                simulcast: false,
                rtx: false,
                share_bps: 0,
                last_video_relays: Vec::new(),
                video_cache: VideoCache::default(),
                last_watchers: Default::default(),
                decoders: Vec::new(),
                music: None,
                share_audio: false,
                share_p2p_only: false,
                stats: None,
                last_relays: Vec::new(),
                pending_file: pending_file.clone(),
                pending_media: pending_media.clone(),
                media_dropped: Default::default(),
                uploads: HashMap::new(),
                upload_to: HashMap::new(),
            },
        );
        st.broadcast_state();
        st.system(format!("{name} 加入了服务器"));
        st.push_peers();
        println!("[{addr}] {name} (#{id}, 授权码 {} {}) 已连接", entry.id, entry.note);
        id
    };

    let pending_writer = pending_file.clone();
    let media_writer = pending_media.clone();
    let writer = tokio::spawn(async move {
        while let Some(o) = rx.recv().await {
            let (mut file_len, mut media_len) = (0, 0);
            let f = match o {
                Out::Json(m) => sealer.seal(KIND_JSON, serde_json::to_string(&m).unwrap().as_bytes()),
                Out::Voice(v) => {
                    media_len = v.len();
                    sealer.seal(KIND_VOICE, &v)
                }
                Out::Video(v) => {
                    media_len = v.len();
                    sealer.seal(KIND_VIDEO, &v)
                }
                Out::Stream(k, v) => {
                    media_len = v.len();
                    sealer.seal(KIND_STREAM, &stream_frame(k, &v))
                }
                Out::File(v) => {
                    file_len = v.len();
                    sealer.seal(KIND_FILE, &v)
                }
            };
            let ok = wr.write_all(&f).await.is_ok();
            if file_len > 0 {
                pending_writer.fetch_sub(file_len, std::sync::atomic::Ordering::Relaxed);
            }
            if media_len > 0 {
                media_writer.fetch_sub(media_len, std::sync::atomic::Ordering::Relaxed);
            }
            if !ok {
                break;
            }
        }
    });

    let mut bye = false;
    loop {
        let frame = tokio::select! {
            f = read_frame(&mut rd, &mut opener) => f,
            _ = kick.notified() => None,
        };
        let Some((kind, data)) = frame else { break };
        match kind {
            KIND_JSON => {
                match serde_json::from_slice::<ClientMsg>(&data) {
                    Ok(ClientMsg::Bye) => {
                        bye = true;
                        break;
                    }
                    Ok(msg) => handle_msg(&state, user_id, msg),
                    Err(_) => {}
                }
            }
            KIND_VOICE => {
                let Some((seq, f)) = udp::read_client_voice(&data) else { continue };
                let pkts = state.lock().unwrap().forward_voice(user_id, seq, f);
                for (a, p) in pkts {
                    let _ = udp_sock.send_to(&p, a).await;
                }
            }
            KIND_VIDEO => {
                let pkts = state.lock().unwrap().forward_video(user_id, &data);
                for (a, p) in pkts {
                    let _ = udp_sock.send_to(&p, a).await;
                }
            }
            KIND_FILE => state.lock().unwrap().forward_file(user_id, &data),
            // UDP 不通的人经 TCP 发来的伴奏 / 共享声音 / 鼠标
            KIND_STREAM => {
                let Some((&k, rest)) = data.split_first() else { continue };
                if !matches!(k, udp::MUSIC | udp::SHARE_AUDIO | udp::CURSOR) {
                    continue;
                }
                let Some((seq, f)) = udp::read_client_voice(rest) else { continue };
                let pkts = state.lock().unwrap().forward_stream(user_id, k, seq, f);
                for (a, p) in pkts {
                    let _ = udp_sock.send_to(&p, a).await;
                }
            }
            _ => {}
        }
    }

    // ---- 断开 ----
    {
        let mut st = state.lock().unwrap();
        // 已经被新连接接走了（断线恢复）：什么都不做
        let mine = st.users.get(&user_id).is_some_and(|u| u.conn_gen == conn_gen);
        let keep = mine && !bye && st.users.get(&user_id).is_some_and(|u| !u.resume_token.is_empty());
        if keep {
            // 第 37-2 项：先保留 20 秒（UDP 语音照常转发），等它带着凭证回来；没回来再移除
            if let Some(u) = st.users.get_mut(&user_id) {
                u.detached = Some(Instant::now());
            }
            let (state2, name2) = (state.clone(), name.clone());
            tokio::spawn(async move {
                tokio::time::sleep(RESUME_GRACE).await;
                let mut st = state2.lock().unwrap();
                if st.users.get(&user_id).is_some_and(|u| u.conn_gen == conn_gen && u.detached.is_some()) && st.remove_user(user_id).is_some() {
                    st.broadcast_state();
                    st.system(format!("{name2} 离开了服务器"));
                    st.push_peers();
                    st.push_watchers();
                    st.push_relays();
                }
            });
        } else if mine && st.remove_user(user_id).is_some() {
            st.broadcast_state();
            st.system(format!("{name} 离开了服务器"));
            st.push_peers();
            st.push_watchers();
            st.push_relays();
        }
    }
    // 发送队列的 Sender 已随 User 释放，写任务会把剩余消息（例如踢下线原因）发完再退出
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
    println!("[{addr}] {name} (#{user_id}) 已断开");
    Ok(())
}

fn handle_msg(state: &Shared, uid: u32, msg: ClientMsg) {
    let mut st = state.lock().unwrap();
    handle_msg_locked(&mut st, uid, msg);
    st.push_peers();
    st.push_watchers();
    st.push_relays();
}

fn handle_msg_locked(st: &mut State, uid: u32, msg: ClientMsg) {
    match msg {
        ClientMsg::Hello { .. } | ClientMsg::Bye => {}
        ClientMsg::Transport { udp } => {
            if let Some(u) = st.users.get_mut(&uid) {
                if u.voice_udp != udp {
                    u.voice_udp = udp;
                    st.broadcast_state();
                }
            }
        }
        ClientMsg::JoinChannel { channel_id } => {
            if !st.channels.iter().any(|c| c.id == channel_id) {
                return;
            }
            let cname = st.channel_name(channel_id);
            if st.users.get(&uid).is_some_and(|u| u.channel == Some(channel_id)) {
                return;
            }
            let count = st.users.values().filter(|u| u.channel == Some(channel_id)).count();
            if count >= MAX_CHANNEL_USERS {
                if let Some(u) = st.users.get(&uid) {
                    u.send(ServerMsg::System { text: format!("语音频道「{cname}」已满（{MAX_CHANNEL_USERS} 人）") });
                }
                return;
            }
            let Some(u) = st.users.get_mut(&uid) else { return };
            u.channel = Some(channel_id);
            u.stats = None;
            u.music = None;
            u.direct.clear();
            u.direct_video.clear();
            u.direct_share_audio = None;
            // 换频道：停止共享和观看
            u.sharing = None;
            u.watching.clear();
            let uname = u.name.clone();
            st.broadcast_state();
            st.system(format!("{uname} 进入了语音频道「{cname}」"));
        }
        ClientMsg::LeaveChannel => {
            if let Some(u) = st.users.get_mut(&uid) {
                if u.channel.take().is_some() {
                    u.stats = None;
                    u.music = None;
                    u.direct.clear();
                    u.direct_video.clear();
                    u.direct_share_audio = None;
                    u.sharing = None;
                    u.watching.clear();
                    st.broadcast_state();
                }
            }
        }
        ClientMsg::SetState { muted, deafened } => {
            if let Some(u) = st.users.get_mut(&uid) {
                u.muted = muted || deafened;
                u.deafened = deafened;
                st.broadcast_state();
            }
        }
        ClientMsg::CreateChannel { name } => {
            let name = name.trim().to_string();
            if name.is_empty() || name.chars().count() > MAX_NAME_LEN || st.channels.len() >= 64 {
                return;
            }
            if st.channels.iter().any(|c| c.name == name) {
                return;
            }
            let id = st.next_channel;
            st.next_channel += 1;
            st.channels.push(ChannelInfo { id, name: name.clone() });
            let uname = st.users.get(&uid).map(|u| u.name.clone()).unwrap_or_default();
            st.broadcast_state();
            st.system(format!("{uname} 创建了语音频道「{name}」"));
        }
        ClientMsg::Ping => {
            if let Some(u) = st.users.get(&uid) {
                u.send(ServerMsg::Pong);
            }
        }
        ClientMsg::P2pEnable { local_addrs, nat } => {
            let local: Vec<String> = local_addrs
                .into_iter()
                .filter(|a| a.parse::<SocketAddr>().is_ok())
                .take(8)
                .collect();
            if let Some(u) = st.users.get_mut(&uid) {
                let was = u.p2p.is_some();
                u.p2p = Some(local);
                if nat.is_some() {
                    u.nat = nat;
                }
                if !was {
                    st.broadcast_state();
                }
                st.push_peers();
            }
        }
        ClientMsg::P2pHide { ids } => {
            if let Some(u) = st.users.get_mut(&uid) {
                let ids: HashSet<u32> = ids.into_iter().take(64).collect();
                if u.p2p_hide != ids {
                    u.p2p_hide = ids;
                    st.push_peers();
                }
            }
        }
        ClientMsg::P2pDisable => {
            if let Some(u) = st.users.get_mut(&uid) {
                let was = u.p2p.take().is_some();
                u.direct.clear();
                u.direct_video.clear();
                u.direct_share_audio = None;
                if was {
                    st.broadcast_state();
                }
            }
        }
        ClientMsg::E2eAnnounce { mut entry } => {
            // 只检查长度；签名由各客户端自己验证（服务器不可信）
            let ok = [(&entry.idk, e2e::IDK_LEN), (&entry.ek, e2e::EK_LEN), (&entry.sig, e2e::SIG_LEN)]
                .iter()
                .all(|(s, n)| e2e::unb64(s).is_some_and(|b| b.len() == *n));
            let Some(u) = st.users.get_mut(&uid) else { return };
            if !ok {
                return;
            }
            entry.id = uid;
            entry.name = u.name.clone();
            u.e2e = Some(entry.clone());
            let others: Vec<E2eEntry> =
                st.users.iter().filter(|(id, _)| **id != uid).filter_map(|(_, o)| o.e2e.clone()).collect();
            for (id, o) in st.users.iter() {
                if *id == uid {
                    o.send(ServerMsg::Directory { entries: others.clone() });
                } else {
                    o.send(ServerMsg::E2eJoin { entry: entry.clone() });
                }
            }
        }
        ClientMsg::PeerKey { to, ct, sig } => {
            // ML-KEM-768 密文 1088 字节 + ML-DSA-65 签名，服务器只转发
            if ct.len() > 2000 || sig.len() > 5000 {
                return;
            }
            let (Some(me), Some(peer)) = (st.users.get(&uid), st.users.get(&to)) else { return };
            if me.e2e.is_some() && peer.e2e.is_some() {
                peer.send(ServerMsg::PeerKey { from: uid, ct, sig });
            }
        }
        ClientMsg::FileTarget { id, to } => {
            if to == uid || !st.users.contains_key(&to) {
                return;
            }
            if let Some(u) = st.users.get_mut(&uid) {
                if u.upload_to.len() >= 8 {
                    u.upload_to.clear();
                }
                u.upload_to.insert(id, to);
            }
        }
        ClientMsg::E2eBlob { to, blob } => {
            // 端到端加密信封（语音密钥 / 聊天），服务器看不到内容，只转发
            if blob.len() > 16 * 1024 {
                return;
            }
            if let (Some(_), Some(peer)) = (st.users.get(&uid), st.users.get(&to)) {
                peer.send(ServerMsg::E2eBlob { from: uid, blob, ts: now_millis() });
            }
        }
        ClientMsg::SetMusic { name } => {
            let name = name.map(|n| n.trim().chars().take(40).collect::<String>()).filter(|n| !n.is_empty());
            if let Some(u) = st.users.get_mut(&uid) {
                let name = if u.channel.is_some() { name } else { None };
                if u.music != name {
                    u.music = name;
                    st.broadcast_state();
                }
            }
        }
        ClientMsg::ShareStart { desc, audio, simulcast, rtx, bps, p2p_only } => {
            let desc: String = desc.trim().chars().take(16).collect();
            if let Some(u) = st.users.get_mut(&uid) {
                if u.channel.is_some() {
                    u.share_p2p_only = p2p_only;
                    u.share_audio = audio;
                    u.simulcast = simulcast;
                    u.rtx = rtx;
                    u.share_bps = bps.min(100_000_000);
                    u.video_cache = VideoCache::default();
                    u.sharing = Some(if desc.is_empty() { "屏幕".into() } else { desc });
                    let name = u.name.clone();
                    st.broadcast_state();
                    st.system(format!("{name} 开始了屏幕共享"));
                }
            }
        }
        ClientMsg::ShareStop => {
            if let Some(u) = st.users.get_mut(&uid) {
                if u.sharing.take().is_some() {
                    st.broadcast_state();
                }
            }
        }
        ClientMsg::Watch { target, on, low, route, cursor } => {
            let ok = match (st.users.get(&uid), st.users.get(&target)) {
                (Some(me), Some(t)) => t.sharing.is_some() && me.channel.is_some() && me.channel == t.channel,
                _ => false,
            };
            if let Some(u) = st.users.get_mut(&uid) {
                if on && ok {
                    u.watching.insert(target);
                    if low {
                        u.watch_low.insert(target);
                    } else {
                        u.watch_low.remove(&target);
                    }
                    if matches!(route, 1 | 2) {
                        u.watch_route.insert(target, route);
                    } else {
                        u.watch_route.remove(&target);
                    }
                    if cursor {
                        u.watch_cursor.insert(target);
                    } else {
                        u.watch_cursor.remove(&target);
                    }
                } else {
                    u.watching.remove(&target);
                    u.watch_low.remove(&target);
                    u.watch_route.remove(&target);
                    u.watch_cursor.remove(&target);
                }
            }
        }
        ClientMsg::VideoLtr { target, ack, recover } => {
            let watching = st.users.get(&uid).is_some_and(|u| u.watching.contains(&target));
            if let (true, Some(t)) = (watching, st.users.get(&target)) {
                t.send(ServerMsg::VideoLtr { by: uid, ack, recover });
            }
        }
        ClientMsg::VideoReport { target, loss, rtt } => {
            let watching = st.users.get(&uid).is_some_and(|u| u.watching.contains(&target));
            if let (true, Some(t)) = (watching, st.users.get(&target)) {
                t.send(ServerMsg::VideoReport { by: uid, loss: loss.min(1000), rtt: rtt.min(10_000) });
            }
        }
        ClientMsg::KeyframeRequest { target } => {
            let watching = st.users.get(&uid).is_some_and(|u| u.watching.contains(&target));
            if let (true, Some(t)) = (watching, st.users.get(&target)) {
                t.send(ServerMsg::KeyframeRequest { by: uid });
            }
        }
        ClientMsg::Caps { decoders } => {
            let list: Vec<String> = decoders.into_iter().filter(|d| ["av1", "hevc", "h264"].contains(&d.as_str())).take(8).collect();
            if let Some(u) = st.users.get_mut(&uid) {
                if u.decoders != list {
                    u.decoders = list;
                    st.broadcast_state();
                }
            }
        }
        ClientMsg::LinkStats { server_rtt, peers, relay_slots, via_server, video_relay_bps, talking, heard, manual_via, voice_loss } => {
            let Some(u) = st.users.get_mut(&uid) else { return };
            let allowed: HashSet<u32> = u.last_peers.iter().map(|p| p.id).collect();
            let peers: Vec<PeerStat> = peers.into_iter().filter(|p| allowed.contains(&p.id)).take(32).collect();
            let via_server: HashSet<u32> = via_server.into_iter().take(32).collect();
            u.stats = Some(LinkReport {
                at: Instant::now(),
                server_rtt: server_rtt.min(10_000),
                peers,
                slots: relay_slots.min(16),
                via_server,
                video_relay_bps,
                talking,
                heard: heard.into_iter().take(64).collect(),
                manual_via: manual_via.into_iter().take(32).map(|[p, v]| (p, v)).collect(),
                voice_loss: voice_loss.min(100),
            });
            st.check_voice_heard(uid);
        }
        ClientMsg::PunchResult { peer, ok, ms } => {
            let (Some(a), Some(b)) = (st.users.get(&uid), st.users.get(&peer)) else { return };
            if !a.last_peers.iter().any(|p| p.id == peer) {
                return;
            }
            let (lo, hi) = if uid < peer { ((uid, a), (peer, b)) } else { ((peer, b), (uid, a)) };
            let key = (lo.0, hi.0, lo.1.conn_gen, hi.1.conn_gen);
            let kinds = nat_pair(a.nat.map(|n| n.kind), b.nat.map(|n| n.kind));
            if st.punch_seen.insert(key) {
                let today = utc_day(voc_proto::now_millis() / 1000);
                add_punch_day(&mut st.punch_days, &today, kinds, ok, ms.min(60_000));
            }
        }
        ClientMsg::DirectPeers { ids, video, share_audio } => {
            let Some(u) = st.users.get(&uid) else { return };
            let allowed: HashSet<u32> = u.last_peers.iter().map(|p| p.id).collect();
            let u = st.users.get_mut(&uid).unwrap();
            u.direct = ids.into_iter().filter(|i| allowed.contains(i)).collect();
            u.direct_video = video.into_iter().filter(|i| allowed.contains(i)).collect();
            u.direct_share_audio = share_audio.map(|a| a.into_iter().filter(|i| allowed.contains(i)).collect());
        }
    }
}

/// 每 5 秒检查授权码文件：被吊销/过期的在线用户立即踢下线
async fn revocation_loop(state: Shared) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let mut st = state.lock().unwrap();
        st.codes.refresh();
        let now = now_millis();
        let bad: Vec<(u32, String)> = st
            .users
            .iter()
            .filter_map(|(id, u)| {
                let r = match st.codes.find(&u.code_id) {
                    Some(e) => e.check(now),
                    None => Err("授权码已被删除".into()),
                };
                r.err().map(|e| (*id, e))
            })
            .collect();
        for (id, why) in bad {
            if let Some(u) = st.remove_user(id) {
                println!("[VOC] 踢出 {}：{why}", u.name);
                u.send(ServerMsg::Error { message: why });
                u.kick.notify_one();
                let name = u.name.clone();
                st.broadcast_state();
                st.system(format!("{name} 离开了服务器"));
                st.push_peers();
                st.push_watchers();
                st.push_relays();
            }
        }
    }
}

/// 每 5 秒把运行状态写进状态文件（`voc-server status` / `voc status` 读）；按天的直连率每分钟存一次（重启不丢）
async fn status_loop(state: Shared, path: PathBuf, port: u16) {
    let days_path = path.with_file_name(path.file_name().map(|n| n.to_string_lossy().replace(".status.txt", ".p2p-days.txt")).unwrap_or_else(|| "voc.p2p-days.txt".into()));
    let punch_path = path.with_file_name(path.file_name().map(|n| n.to_string_lossy().replace(".status.txt", ".punch-days.txt")).unwrap_or_else(|| "voc.punch-days.txt".into()));
    state.lock().unwrap().p2p_days = std::fs::read_to_string(&days_path).map(|t| parse_p2p_days(&t)).unwrap_or_default();
    state.lock().unwrap().punch_days = std::fs::read_to_string(&punch_path).map(|t| parse_punch_days(&t)).unwrap_or_default();
    let mut ticks = 0u32;
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        ticks += 1;
        let (text, days) = {
            let mut st = state.lock().unwrap();
            let (pairs, direct, _) = st.p2p_pairs();
            st.p2p_samples.0 += pairs as u64;
            st.p2p_samples.1 += direct as u64;
            if pairs > 0 {
                let today = utc_day(voc_proto::now_millis() / 1000);
                add_p2p_day(&mut st.p2p_days, &today, pairs as u64, direct as u64);
            }
            (st.status_text(port), (ticks % 12 == 0).then(|| (format_p2p_days(&st.p2p_days), format_punch_days(&st.punch_days))))
        };
        let _ = std::fs::write(&path, text);
        if let Some((d, p)) = days {
            let _ = std::fs::write(&days_path, d);
            let _ = std::fs::write(&punch_path, p);
        }
    }
}

/// Unix 秒 → UTC 日期「YYYY-MM-DD」
fn utc_day(secs: u64) -> String {
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

/// 只留最近这么多天
const P2P_DAYS_KEEP: usize = 7;

fn add_p2p_day(days: &mut Vec<(String, u64, u64)>, day: &str, pairs: u64, direct: u64) {
    match days.iter_mut().find(|d| d.0 == day) {
        Some(d) => {
            d.1 += pairs;
            d.2 += direct;
        }
        None => days.push((day.to_string(), pairs, direct)),
    }
    days.sort_by(|a, b| b.0.cmp(&a.0));
    days.truncate(P2P_DAYS_KEEP);
}

/// 文件格式：每行「日期 总对数 直连对数」（只有次数，没有昵称和 IP）
fn format_p2p_days(days: &[(String, u64, u64)]) -> String {
    days.iter().map(|(d, p, k)| format!("{d} {p} {k}\n")).collect()
}

fn parse_p2p_days(t: &str) -> Vec<(String, u64, u64)> {
    let mut v: Vec<(String, u64, u64)> = t
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let (d, p, k) = (it.next()?, it.next()?.parse().ok()?, it.next()?.parse::<u64>().ok()?);
            (d.len() == 10 && k <= p).then(|| (d.to_string(), p, k))
        })
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0));
    v.truncate(P2P_DAYS_KEEP);
    v
}

/// 通知双方「过这么久同时开打」（第 62-2 项）：要比两边到服务器的单程延迟都长，才能都等到同一刻
const PUNCH_SYNC_MS: u32 = 400;

/// 一天、一种 NAT 组合的打洞结果
#[derive(Clone, Debug, PartialEq)]
struct PunchDay {
    day: String,
    /// 双方的 NAT 类型（小的在前；0 = 没测出来）
    kinds: (u8, u8),
    ok: u64,
    fail: u64,
    /// 打通的那些一共用了多少毫秒（算平均用）
    ok_ms: u64,
}

fn nat_pair(a: Option<u8>, b: Option<u8>) -> (u8, u8) {
    let (a, b) = (a.unwrap_or(0).min(9), b.unwrap_or(0).min(9));
    (a.min(b), a.max(b))
}

fn nat_short(k: u8) -> &'static str {
    match k {
        voc_proto::NAT_OPEN => "公网",
        voc_proto::NAT_CONE => "锥形",
        voc_proto::NAT_SYM_PREDICTABLE => "对称可预测",
        voc_proto::NAT_SYM_RANDOM => "对称随机",
        _ => "未知",
    }
}

fn add_punch_day(days: &mut Vec<PunchDay>, day: &str, kinds: (u8, u8), ok: bool, ms: u32) {
    let e = match days.iter_mut().find(|d| d.day == day && d.kinds == kinds) {
        Some(e) => e,
        None => {
            days.push(PunchDay { day: day.to_string(), kinds, ok: 0, fail: 0, ok_ms: 0 });
            days.last_mut().unwrap()
        }
    };
    if ok {
        e.ok += 1;
        e.ok_ms += ms as u64;
    } else {
        e.fail += 1;
    }
    let mut keep: Vec<String> = days.iter().map(|d| d.day.clone()).collect();
    keep.sort_by(|a, b| b.cmp(a));
    keep.dedup();
    keep.truncate(P2P_DAYS_KEEP);
    days.retain(|d| keep.contains(&d.day));
}

/// 文件格式：每行「日期 类型 类型 打通 失败 打通总毫秒」
fn format_punch_days(days: &[PunchDay]) -> String {
    days.iter().map(|d| format!("{} {} {} {} {} {}\n", d.day, d.kinds.0, d.kinds.1, d.ok, d.fail, d.ok_ms)).collect()
}

fn parse_punch_days(t: &str) -> Vec<PunchDay> {
    let mut v: Vec<PunchDay> = t
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() != 6 || f[0].len() != 10 {
                return None;
            }
            let n = |i: usize| f[i].parse::<u64>().ok();
            let (a, b) = (n(1)?.min(9) as u8, n(2)?.min(9) as u8);
            Some(PunchDay { day: f[0].to_string(), kinds: (a.min(b), a.max(b)), ok: n(3)?, fail: n(4)?, ok_ms: n(5)? })
        })
        .collect();
    let mut keep: Vec<String> = v.iter().map(|d| d.day.clone()).collect();
    keep.sort_by(|a, b| b.cmp(a));
    keep.dedup();
    keep.truncate(P2P_DAYS_KEEP);
    v.retain(|d| keep.contains(&d.day));
    v
}

/// 「7 天打洞（按 NAT 类型）：锥形×锥形 12/12 平均 1.1 秒 · 锥形×对称随机 3/7 平均 8.0 秒」
fn punch_days_text(days: &[PunchDay]) -> Option<String> {
    let mut by: Vec<((u8, u8), u64, u64, u64)> = Vec::new();
    for d in days {
        match by.iter_mut().find(|x| x.0 == d.kinds) {
            Some(x) => {
                x.1 += d.ok;
                x.2 += d.fail;
                x.3 += d.ok_ms;
            }
            None => by.push((d.kinds, d.ok, d.fail, d.ok_ms)),
        }
    }
    by.retain(|x| x.1 + x.2 > 0);
    if by.is_empty() {
        return None;
    }
    by.sort_by_key(|x| std::cmp::Reverse(x.1 + x.2));
    let parts: Vec<String> = by
        .iter()
        .map(|(k, ok, fail, ms)| {
            let avg = if *ok > 0 { format!(" 平均 {:.1} 秒", *ms as f64 / *ok as f64 / 1000.0) } else { String::new() };
            format!("{}×{} {}/{}{}", nat_short(k.0), nat_short(k.1), ok, ok + fail, avg)
        })
        .collect();
    let days_n = {
        let mut d: Vec<&str> = days.iter().map(|d| d.day.as_str()).collect();
        d.sort();
        d.dedup();
        d.len()
    };
    Some(format!("{days_n} 天的打洞结果（按双方 NAT 类型，打通 / 总次数）：{}", parts.join(" · ")))
}

/// 「7 天按天的直连率：11-23 80% · 11-22 75%」（UTC 日期）
fn p2p_days_text(days: &[(String, u64, u64)]) -> Option<String> {
    let parts: Vec<String> = days.iter().filter(|d| d.1 > 0).map(|(d, p, k)| format!("{} {}%", &d[5..], k * 100 / p)).collect();
    (!parts.is_empty()).then(|| format!("{} 天按天的语音直连率（UTC 日期）：{}", parts.len(), parts.join(" · ")))
}

/// 每秒重新规划一次成员中继（上报过期、链路变化都会体现）
async fn relay_loop(state: Shared) {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let mut st = state.lock().unwrap();
        st.expire_voice_fallback();
        st.push_relays();
        st.push_video_relays();
        st.balance_video_cap();
    }
}

/// 公网地址（局域网、运营商级 NAT、IPv6 本地 / 链路本地地址都不算）
fn is_public(ip: std::net::IpAddr) -> bool {
    match ip.to_canonical() {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || (o[0] == 100 && (o[1] & 0xC0) == 64))
        }
        std::net::IpAddr::V6(v6) => {
            let s = v6.segments()[0];
            !(v6.is_loopback() || v6.is_unspecified() || (s & 0xfe00) == 0xfc00 || (s & 0xffc0) == 0xfe80)
        }
    }
}

/// NAT 探测端口：只回答 PING（告诉客户端从这个端口看到的它的地址），其他包一律不理。
/// 同样要求是已登录用户、用自己的密钥加密，不是公开的 STUN
/// other = 另一个 IP 上的探测点：客户端做过滤测试（udp::FILTER_TEST）时从它回（第 39-2 项）
async fn probe_loop(state: Shared, sock: Arc<UdpSocket>, other: Option<Arc<UdpSocket>>) {
    let mut buf = [0u8; 256];
    loop {
        let Ok((n, addr)) = sock.recv_from(&mut buf).await else { continue };
        let reply = {
            let mut st = state.lock().unwrap();
            let mut reply = None;
            for u in st.users.values_mut().take(MAX_TRIAL) {
                if let Some((kind, data)) = u.udp_open.open(&buf[..n]) {
                    if kind == udp::PING {
                        let mut p = Vec::new();
                        u.udp_seal.seal(udp::PONG, &udp::pong_payload(&data, addr), &mut p);
                        let filter = data.get(8) == Some(&udp::FILTER_TEST);
                        reply = Some((p, filter));
                    }
                    break;
                }
            }
            reply
        };
        match (reply, &other) {
            (Some((p, true)), Some(o)) => {
                let _ = o.send_to(&p, addr).await;
            }
            (Some((p, false)), _) => {
                let _ = sock.send_to(&p, addr).await;
            }
            _ => {}
        }
    }
}

/// 带宽显示：1.5 → 「1.5」，2.0 → 「2」（第 52 项：以前四舍五入成整数，1.5 显示成 2）
fn fmt_mbps(bps: u64) -> String {
    let m = (bps as f64 / 1e5).round() / 10.0;
    if m.fract() == 0.0 {
        format!("{m:.0}")
    } else {
        format!("{m:.1}")
    }
}

/// 未知地址最多试解多少个用户（防止被随机包刷 CPU）
const MAX_TRIAL: usize = 512;

async fn udp_loop(state: Shared, sock: Arc<UdpSocket>) {
    let mut buf = [0u8; 2048];
    let mut out: Vec<(SocketAddr, Vec<u8>)> = Vec::new();
    // 调试：VOC_DEBUG_UDP=1 每秒打印收到的画面包、转发出去的包、等锁最久多少毫秒、收包出错次数
    let debug = std::env::var_os("VOC_DEBUG_UDP").is_some();
    let (mut d_at, mut d_vin, mut d_out, mut d_wait, mut d_err, mut d_send_err) = (Instant::now(), 0u32, 0u32, 0u128, 0u32, 0u32);
    let (mut d_last, mut d_gap) = (Instant::now(), 0u128);
    loop {
        if debug && d_at.elapsed() >= Duration::from_secs(1) {
            println!("[udp] {} 画面收 {d_vin} 发出 {d_out} 等锁最久 {d_wait} ms 收包间隔最长 {d_gap} ms 收错 {d_err} 发错 {d_send_err}", now_millis());
            (d_at, d_vin, d_out, d_wait, d_err, d_send_err, d_gap) = (Instant::now(), 0, 0, 0, 0, 0, 0);
        }
        // Windows 上对端不可达时 recv_from 会返回 ConnectionReset，忽略即可
        let Ok((n, addr)) = sock.recv_from(&mut buf).await else {
            d_err += 1;
            continue;
        };
        d_gap = d_gap.max(d_last.elapsed().as_millis());
        d_last = Instant::now();
        let pkt = &buf[..n];
        out.clear();
        {
            let t0 = Instant::now();
            let mut st = state.lock().unwrap();
            d_wait = d_wait.max(t0.elapsed().as_millis());
            // 先试这个地址对应的用户；不认识的地址（新连接 / NAT 换了端口）再逐个试解
            let mut found = None;
            if let Some(&uid) = st.addr_map.get(&addr) {
                if let Some(r) = st.users.get_mut(&uid).and_then(|u| u.udp_open.open(pkt)) {
                    found = Some((uid, r));
                }
            }
            if found.is_none() {
                for (id, u) in st.users.iter_mut().take(MAX_TRIAL) {
                    if let Some(r) = u.udp_open.open(pkt) {
                        found = Some((*id, r));
                        break;
                    }
                }
            }
            let Some((uid, (kind, data))) = found else { continue }; // 无法认证：静默丢弃
            let old = st.users.get(&uid).and_then(|u| u.udp);
            if old != Some(addr) {
                if let Some(o) = old {
                    st.addr_map.remove(&o);
                }
                st.addr_map.insert(addr, uid);
                st.users.get_mut(&uid).unwrap().udp = Some(addr);
                st.push_peers(); // 公网地址变了，告诉直连的对端
            }
            match kind {
                udp::PING => {
                    let u = st.users.get_mut(&uid).unwrap();
                    let mut p = Vec::new();
                    u.udp_seal.seal(udp::PONG, &udp::pong_payload(&data, addr), &mut p);
                    out.push((addr, p));
                }
                udp::VOICE => {
                    if let Some((seq, f)) = udp::read_client_voice(&data) {
                        out = st.forward_voice(uid, seq, f);
                    }
                }
                udp::VIDEO => {
                    d_vin += 1;
                    out = st.forward_video(uid, &data)
                }
                udp::NACK => out = st.on_nack(uid, &data),
                udp::MUSIC | udp::SHARE_AUDIO | udp::CURSOR => {
                    if let Some((seq, f)) = udp::read_client_voice(&data) {
                        out = st.forward_stream(uid, kind, seq, f);
                    }
                }
                _ => {}
            }
        }
        for (a, p) in &out {
            d_out += 1;
            if sock.send_to(p, a).await.is_err() {
                d_send_err += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_public;

    #[test]
    fn punch_days_roundtrip_and_text() {
        use super::*;
        let mut d = Vec::new();
        add_punch_day(&mut d, "2026-11-24", nat_pair(Some(2), Some(4)), true, 3000);
        add_punch_day(&mut d, "2026-11-24", nat_pair(Some(4), Some(2)), false, 30000);
        add_punch_day(&mut d, "2026-11-24", nat_pair(Some(2), Some(2)), true, 1000);
        add_punch_day(&mut d, "2026-11-23", nat_pair(None, Some(2)), true, 500);
        assert_eq!(d.iter().find(|x| x.kinds == (2, 4)).map(|x| (x.ok, x.fail, x.ok_ms)), Some((1, 1, 3000)), "order of kinds does not matter");
        let back = parse_punch_days(&format_punch_days(&d));
        assert_eq!(back, d);
        let t = punch_days_text(&d).unwrap();
        assert!(t.starts_with("2 天"), "{t}");
        assert!(t.contains("锥形×对称随机 1/2 平均 3.0 秒"), "{t}");
        assert!(t.contains("未知×锥形 1/1"), "{t}");
        for i in 0..10 {
            add_punch_day(&mut d, &format!("2026-12-{:02}", i + 1), (2, 2), true, 1);
        }
        let mut days: Vec<&String> = d.iter().map(|x| &x.day).collect();
        days.dedup();
        assert_eq!(days.len(), P2P_DAYS_KEEP);
        assert!(parse_punch_days("坏行\n2026-11-01 2 2 x 1 1\n").is_empty());
    }

    #[test]
    fn p2p_days_keep_seven() {
        use super::*;
        assert_eq!(utc_day(0), "1970-01-01");
        assert_eq!(utc_day(1_795_278_600), "2026-11-21");
        assert_eq!(utc_day(951_782_400), "2000-02-29");
        let mut d = Vec::new();
        for i in 0..9u64 {
            let day = utc_day(1_795_278_600 + i * 86_400);
            add_p2p_day(&mut d, &day, 10, i.min(10));
            add_p2p_day(&mut d, &day, 10, 0);
        }
        assert_eq!(d.len(), 7);
        assert_eq!(d[0], ("2026-11-29".to_string(), 20, 8));
        let back = parse_p2p_days(&format_p2p_days(&d));
        assert_eq!(back, d);
        let t = p2p_days_text(&d).unwrap();
        assert!(t.starts_with("7 天") && t.contains("11-29 40%"), "{t}");
        assert!(parse_p2p_days("坏行\n2026-11-01 5 9\n").is_empty(), "直连数不会比总数多");
    }

    #[test]
    fn video_cap_shows_decimals() {
        assert_eq!(super::fmt_mbps(1_500_000), "1.5");
        assert_eq!(super::fmt_mbps(2_000_000), "2");
        assert_eq!(super::fmt_mbps(10_000_000), "10");
        assert_eq!(super::fmt_mbps(800_000), "0.8");
    }

    #[test]
    fn pre_auth_connections_are_limited() {
        use super::{PreAuth, MAX_PRE_AUTH, MAX_PRE_AUTH_PER_IP};
        let p = PreAuth::default();
        let a: std::net::IpAddr = "1.2.3.4".parse().unwrap();
        let mapped: std::net::IpAddr = "::ffff:1.2.3.4".parse().unwrap();
        let mut held: Vec<_> = (0..MAX_PRE_AUTH_PER_IP).map(|_| p.enter(a).unwrap()).collect();
        assert!(p.enter(a).is_none(), "per-IP limit");
        assert!(p.enter(mapped).is_none(), "IPv4-mapped IPv6 counts as the same IP");
        assert!(p.enter("5.6.7.8".parse().unwrap()).is_some(), "other IPs are not affected");
        held.pop(); // 一个认证完了
        assert!(p.enter(a).is_some());
        // 全局上限
        let p = PreAuth::default();
        let all: Vec<_> = (0..MAX_PRE_AUTH).map(|i| p.enter(std::net::IpAddr::from([10, 0, (i / 250) as u8, (i % 250) as u8])).unwrap()).collect();
        assert!(p.enter("9.9.9.9".parse().unwrap()).is_none(), "global limit");
        drop(all);
        assert!(p.enter("9.9.9.9".parse().unwrap()).is_some(), "released on drop");
    }

    #[test]
    fn repeated_auth_failures_ban_the_ip_for_a_while() {
        use super::{BanList, BAN_FAILS, BAN_FOR};
        use std::time::{Duration, Instant};
        let mut b = BanList::default();
        let (ip, other): (std::net::IpAddr, std::net::IpAddr) = ("1.2.3.4".parse().unwrap(), "5.6.7.8".parse().unwrap());
        let t = Instant::now();
        assert_eq!(b.failed(ip, t), (true, false), "first failure is logged");
        for i in 2..BAN_FAILS {
            assert_eq!(b.failed(ip, t), (false, false), "failure {i} is not logged again");
        }
        assert!(!b.banned(ip, t));
        assert_eq!(b.failed(ip, t), (false, true));
        assert!(b.banned(ip, t + Duration::from_secs(1)));
        assert!(!b.banned(other, t), "other IPs are not affected");
        assert_eq!(b.count(t), 1);
        assert!(!b.banned(ip, t + BAN_FOR + Duration::from_secs(1)), "ban expires");
        // 慢慢试（每分钟不到 10 次）不会被封
        let mut b = BanList::default();
        for m in 0..30u64 {
            for _ in 0..(BAN_FAILS - 1) {
                b.failed(other, t + Duration::from_secs(61 * m));
            }
        }
        assert!(!b.banned(other, t + Duration::from_secs(61 * 30)));
    }

    #[test]
    fn public_address_filter() {
        for a in ["8.8.8.8", "2408:8000::1", "1.2.3.4"] {
            assert!(is_public(a.parse().unwrap()), "{a}");
        }
        for a in ["192.168.1.5", "10.0.0.2", "172.20.1.1", "100.70.1.2", "169.254.3.4", "127.0.0.1", "fd00::1", "fe80::1", "::ffff:192.168.0.1"] {
            assert!(!is_public(a.parse().unwrap()), "{a}");
        }
    }
}
