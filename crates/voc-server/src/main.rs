//! VOC 服务器：授权码登录、ML-KEM-768 加密、抗 DPI 传输；语音 SFU 转发（不混音）。

mod codes;

use codes::CodeStore;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
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
}

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
    /// 端到端加密身份公告
    e2e: Option<E2eEntry>,
    /// 已与之直连的成员：不再替该用户把语音转发给他们
    direct: HashSet<u32>,
    /// 屏幕共享画面已直连的观看者：不再替该用户转发视频给他们
    direct_video: HashSet<u32>,
    /// 上次发给该用户的 Peers 列表（没变化就不重发）
    last_peers: Vec<PeerInfo>,
    /// 正在屏幕共享（描述，例如 "1080p60"）
    sharing: Option<String>,
    /// 我正在观看谁的屏幕
    watching: HashSet<u32>,
    /// 上次发给我的观看者名单（我是共享者时）
    last_watchers: Vec<u32>,
    /// 能解码的视频格式
    decoders: Vec<String>,
}

impl User {
    fn send(&self, m: ServerMsg) {
        let _ = self.tx.send(Out::Json(m));
    }
}

struct State {
    server_name: String,
    udp_port: u16,
    users: HashMap<u32, User>,
    addr_map: HashMap<SocketAddr, u32>,
    channels: Vec<ChannelInfo>,
    next_user: u32,
    next_channel: u32,
    codes: CodeStore,
    replay: ReplayCache,
}

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

    /// 给每个开启 P2P 的用户推送「同频道、也开启了 P2P、UDP 地址已知」的成员列表（有变化才发）
    fn push_peers(&mut self) {
        let infos: Vec<(u32, Option<u32>, PeerInfo)> = self
            .users
            .iter()
            .filter_map(|(id, u)| {
                let local = u.p2p.as_ref()?;
                let public = u.udp?;
                let mut addrs = vec![public.to_string()];
                addrs.extend(local.iter().take(8).cloned());
                Some((*id, u.channel, PeerInfo { id: *id, addrs }))
            })
            .collect();
        for (id, u) in self.users.iter_mut() {
            let mut peers: Vec<PeerInfo> = match (&u.p2p, u.channel) {
                (Some(_), Some(ch)) => infos
                    .iter()
                    .filter(|(pid, pch, _)| pid != id && *pch == Some(ch))
                    .map(|(_, _, p)| p.clone())
                    .collect(),
                _ => Vec::new(),
            };
            peers.sort_by_key(|p| p.id);
            if peers != u.last_peers {
                u.direct.retain(|d| peers.iter().any(|p| p.id == *d));
                u.direct_video.retain(|d| peers.iter().any(|p| p.id == *d));
                u.last_peers = peers.clone();
                u.send(ServerMsg::Peers { peers });
            }
        }
    }

    fn remove_user(&mut self, id: u32) -> Option<User> {
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
        let pairs: Vec<(u32, Option<u32>, HashSet<u32>)> =
            self.users.iter().map(|(id, u)| (*id, u.channel, u.watching.clone())).collect();
        for (id, u) in self.users.iter_mut() {
            let mut w: Vec<u32> = match (&u.sharing, u.channel) {
                (Some(_), Some(ch)) => pairs
                    .iter()
                    .filter(|(wid, wch, set)| wid != id && *wch == Some(ch) && set.contains(id))
                    .map(|(wid, _, _)| *wid)
                    .collect(),
                _ => Vec::new(),
            };
            w.sort();
            if w != u.last_watchers {
                u.last_watchers = w.clone();
                u.send(ServerMsg::Watchers { ids: w });
            }
        }
    }

    /// 转发一个视频分片给正在观看的人（已 P2P 直连的除外）
    fn forward_video(&mut self, from: u32, frag: &[u8]) -> Vec<(SocketAddr, Vec<u8>)> {
        let Some(u) = self.users.get(&from) else { return Vec::new() };
        let (Some(ch), Some(_)) = (u.channel, &u.sharing) else { return Vec::new() };
        if udp::read_video_frag(frag).is_none() {
            return Vec::new();
        }
        let direct = u.direct_video.clone();
        let payload = udp::server_video(from, frag);
        let mut out = Vec::new();
        for (id, other) in self.users.iter_mut() {
            if other.channel != Some(ch) || !other.watching.contains(&from) || direct.contains(id) {
                continue;
            }
            match (other.voice_udp, other.udp) {
                (true, Some(addr)) => {
                    let mut pkt = Vec::with_capacity(payload.len() + 64);
                    other.udp_seal.seal(udp::VIDEO, &payload, &mut pkt);
                    out.push((addr, pkt));
                }
                _ => {
                    let _ = other.tx.send(Out::Video(payload.clone()));
                }
            }
        }
        out
    }

    /// 把一帧语音转发给同频道成员。TCP 隧道用户直接塞进其发送队列；返回需要发出的 UDP 包。
    fn forward_voice(&mut self, from: u32, seq: u16, frame: &[u8]) -> Vec<(SocketAddr, Vec<u8>)> {
        let Some(u) = self.users.get(&from) else { return Vec::new() };
        let (Some(ch), false) = (u.channel, u.muted) else { return Vec::new() };
        let direct = u.direct.clone();
        let payload = udp::server_voice(from, seq, frame);
        let mut out = Vec::new();
        for (id, other) in self.users.iter_mut() {
            if *id == from || other.channel != Some(ch) || other.deafened || direct.contains(id) {
                continue;
            }
            match (other.voice_udp, other.udp) {
                (true, Some(addr)) => {
                    let mut pkt = Vec::with_capacity(payload.len() + 64);
                    other.udp_seal.seal(udp::VOICE, &payload, &mut pkt);
                    out.push((addr, pkt));
                }
                _ => {
                    let _ = other.tx.send(Out::Voice(payload.clone()));
                }
            }
        }
        out
    }
}

// ------------------------------------------------------------ 命令行

struct Config {
    bind: SocketAddr,
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

管理授权码（服务器运行中也可以执行，立即生效）：
  voc-server gen-code [-n 数量] [--note 备注] [--days 有效天数] [--codes 文件]
  voc-server list-codes [--codes 文件]
  voc-server revoke-code <ID>   [--codes 文件]   吊销（在线的人会被立即踢下线）
  voc-server restore-code <ID>  [--codes 文件]   取消吊销";

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
        } else {
            rest.push(a);
        }
    }

    match rest.first().map(String::as_str) {
        Some("gen-code") => cmd_gen(&codes_path, &rest[1..]),
        Some("list-codes") => cmd_list(&codes_path),
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

    let channels: Vec<ChannelInfo> =
        cfg.channels.iter().enumerate().map(|(i, n)| ChannelInfo { id: i as u32 + 1, name: n.clone() }).collect();
    let n_codes = codes.entries.iter().filter(|e| e.check(now_millis()).is_ok()).count();
    let state: Shared = Arc::new(Mutex::new(State {
        server_name: cfg.name.clone(),
        udp_port: udp_sock.local_addr()?.port(),
        next_user: 1,
        next_channel: channels.len() as u32 + 1,
        channels,
        users: HashMap::new(),
        addr_map: HashMap::new(),
        codes,
        replay: ReplayCache::default(),
    }));

    println!("[VOC] 服务器「{}」已启动：TCP/UDP {}", cfg.name, cfg.bind);
    println!("[VOC] 频道：{}", cfg.channels.join(" / "));
    println!("[VOC] 授权码文件：{}（可用 {n_codes} 个）", cfg.codes.display());
    if n_codes == 0 {
        println!("[VOC] 还没有可用的授权码，另开一个窗口运行：voc-server gen-code --note 名字");
    }
    println!("[VOC] 加密：ML-KEM-768 + ChaCha20-Poly1305，全流量随机化；放行 TCP 与 UDP 端口 {port}");

    tokio::spawn(udp_loop(state.clone(), udp_sock.clone()));
    tokio::spawn(revocation_loop(state.clone()));

    loop {
        tokio::select! {
            r = tcp.accept() => {
                let Ok((stream, addr)) = r else { continue };
                let _ = stream.set_nodelay(true);
                let (st, us) = (state.clone(), udp_sock.clone());
                tokio::spawn(async move {
                    if let Err(e) = handle_client(st, us, stream, addr).await {
                        eprintln!("[{addr}] {e}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                println!("[VOC] 正在关闭");
                return Ok(());
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

async fn handle_client(state: Shared, udp_sock: Arc<UdpSocket>, stream: TcpStream, addr: SocketAddr) -> Result<(), String> {
    let (mut rd, mut wr) = stream.into_split();

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
            println!("[{addr}] 拒绝：{why}");
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
    let (name, version) = match (hello.0, serde_json::from_slice::<ClientMsg>(&hello.1)) {
        (KIND_JSON, Ok(ClientMsg::Hello { name, version })) => (name.trim().to_string(), version),
        _ => return Err("第一条消息必须是 hello".into()),
    };
    let err = {
        let st = state.lock().unwrap();
        if version != PROTOCOL_VERSION {
            Some(format!("协议版本不匹配（服务器 {PROTOCOL_VERSION}，客户端 {version}），请更新客户端"))
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
    let user_id = {
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
        let _ = tx.send(Out::Json(ServerMsg::Welcome { user_id: id, udp_port: st.udp_port, server_name: st.server_name.clone() }));
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
                e2e: None,
                direct: HashSet::new(),
                direct_video: HashSet::new(),
                last_peers: Vec::new(),
                sharing: None,
                watching: HashSet::new(),
                last_watchers: Vec::new(),
                decoders: Vec::new(),
            },
        );
        st.broadcast_state();
        st.system(format!("{name} 加入了服务器"));
        st.push_peers();
        id
    };
    println!("[{addr}] {name} (#{user_id}, 授权码 {} {}) 已连接", entry.id, entry.note);

    let writer = tokio::spawn(async move {
        while let Some(o) = rx.recv().await {
            let f = match o {
                Out::Json(m) => sealer.seal(KIND_JSON, serde_json::to_string(&m).unwrap().as_bytes()),
                Out::Voice(v) => sealer.seal(KIND_VOICE, &v),
                Out::Video(v) => sealer.seal(KIND_VIDEO, &v),
            };
            if wr.write_all(&f).await.is_err() {
                break;
            }
        }
    });

    loop {
        let frame = tokio::select! {
            f = read_frame(&mut rd, &mut opener) => f,
            _ = kick.notified() => None,
        };
        let Some((kind, data)) = frame else { break };
        match kind {
            KIND_JSON => {
                if let Ok(msg) = serde_json::from_slice::<ClientMsg>(&data) {
                    handle_msg(&state, user_id, msg);
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
            _ => {}
        }
    }

    // ---- 断开 ----
    {
        let mut st = state.lock().unwrap();
        if st.remove_user(user_id).is_some() {
            st.broadcast_state();
            st.system(format!("{name} 离开了服务器"));
            st.push_peers();
            st.push_watchers();
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
}

fn handle_msg_locked(st: &mut State, uid: u32, msg: ClientMsg) {
    match msg {
        ClientMsg::Hello { .. } => {}
        ClientMsg::Transport { udp } => {
            if let Some(u) = st.users.get_mut(&uid) {
                u.voice_udp = udp;
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
            u.direct.clear();
            u.direct_video.clear();
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
                    u.direct.clear();
                    u.direct_video.clear();
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
        ClientMsg::P2pEnable { local_addrs } => {
            let local: Vec<String> = local_addrs
                .into_iter()
                .filter(|a| a.parse::<SocketAddr>().is_ok())
                .take(8)
                .collect();
            if let Some(u) = st.users.get_mut(&uid) {
                u.p2p = Some(local);
            }
        }
        ClientMsg::P2pDisable => {
            if let Some(u) = st.users.get_mut(&uid) {
                u.p2p = None;
                u.direct.clear();
                u.direct_video.clear();
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
        ClientMsg::E2eBlob { to, blob } => {
            // 端到端加密信封（语音密钥 / 聊天），服务器看不到内容，只转发
            if blob.len() > 16 * 1024 {
                return;
            }
            if let (Some(_), Some(peer)) = (st.users.get(&uid), st.users.get(&to)) {
                peer.send(ServerMsg::E2eBlob { from: uid, blob, ts: now_millis() });
            }
        }
        ClientMsg::ShareStart { desc } => {
            let desc: String = desc.trim().chars().take(16).collect();
            if let Some(u) = st.users.get_mut(&uid) {
                if u.channel.is_some() {
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
        ClientMsg::Watch { target, on } => {
            let ok = match (st.users.get(&uid), st.users.get(&target)) {
                (Some(me), Some(t)) => t.sharing.is_some() && me.channel.is_some() && me.channel == t.channel,
                _ => false,
            };
            if let Some(u) = st.users.get_mut(&uid) {
                if on && ok {
                    u.watching.insert(target);
                } else {
                    u.watching.remove(&target);
                }
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
        ClientMsg::DirectPeers { ids, video } => {
            let Some(u) = st.users.get(&uid) else { return };
            let allowed: HashSet<u32> = u.last_peers.iter().map(|p| p.id).collect();
            let u = st.users.get_mut(&uid).unwrap();
            u.direct = ids.into_iter().filter(|i| allowed.contains(i)).collect();
            u.direct_video = video.into_iter().filter(|i| allowed.contains(i)).collect();
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
            }
        }
    }
}

/// 未知地址最多试解多少个用户（防止被随机包刷 CPU）
const MAX_TRIAL: usize = 512;

async fn udp_loop(state: Shared, sock: Arc<UdpSocket>) {
    let mut buf = [0u8; 2048];
    let mut out: Vec<(SocketAddr, Vec<u8>)> = Vec::new();
    loop {
        // Windows 上对端不可达时 recv_from 会返回 ConnectionReset，忽略即可
        let Ok((n, addr)) = sock.recv_from(&mut buf).await else { continue };
        let pkt = &buf[..n];
        out.clear();
        {
            let mut st = state.lock().unwrap();
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
                    u.udp_seal.seal(udp::PONG, &data, &mut p);
                    out.push((addr, p));
                }
                udp::VOICE => {
                    if let Some((seq, f)) = udp::read_client_voice(&data) {
                        out = st.forward_voice(uid, seq, f);
                    }
                }
                udp::VIDEO => out = st.forward_video(uid, &data),
                _ => {}
            }
        }
        for (a, p) in &out {
            let _ = sock.send_to(p, a).await;
        }
    }
}
