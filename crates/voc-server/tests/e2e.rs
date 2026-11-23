//! 端到端测试：启动真实服务器进程，用授权码 + ML-KEM 握手登录，验证加密语音转发、
//! 防重放、TCP 回退、错误授权码/探测得不到任何响应、吊销踢人、同码顶号。

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use voc_proto::auth::AuthCode;
use voc_proto::crypto::*;
use std::net::SocketAddr;
use voc_proto::e2e::{self, b64, unb64, Identity, ReceiverKeys, SenderKey};
use voc_proto::p2p::{self, LocalKem, PairKeys};
use voc_proto::{udp, ClientMsg, E2eEntry, ServerMsg, UserInfo, PROTOCOL_VERSION};

const EXE: &str = env!("CARGO_BIN_EXE_voc-server");

struct Server {
    child: Child,
    port: u16,
    codes: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = std::fs::remove_file(&self.codes);
    }
}

/// 一个 TCP 和 UDP 都空闲的本机端口（服务器两种都要监听）
fn free_port() -> u16 {
    loop {
        let t = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = t.local_addr().unwrap().port();
        if std::net::UdpSocket::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

impl Server {
    /// 起一个测试服务器。端口每次现找（TCP 和 UDP 都空闲的），被别的程序抢先占用、服务器没起来就换一个重试
    fn start(offset: u16) -> Server {
        let codes = std::env::temp_dir().join(format!("voc-test-{}-{offset}.json", std::process::id()));
        let _ = std::fs::remove_file(&codes);
        for _ in 0..10 {
            let port = free_port();
            let mut child = Command::new(EXE)
                .args(["--bind", &format!("127.0.0.1:{port}"), "--codes"])
                .arg(&codes)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let t = Instant::now();
            loop {
                if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    // 连上的也可能是别的测试同时在这个端口起的服务器（我们的绑不上、马上退出）：确认自己的还活着
                    std::thread::sleep(Duration::from_millis(300));
                    if child.try_wait().ok().flatten().is_none() {
                        return Server { child, port, codes };
                    }
                    break;
                }
                if child.try_wait().ok().flatten().is_some() || t.elapsed() > Duration::from_secs(5) {
                    let _ = child.kill();
                    break; // 端口被占了：换一个
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        panic!("server did not start");
    }

    fn admin(&self, args: &[&str]) -> String {
        let out = Command::new(EXE).args(args).arg("--codes").arg(&self.codes).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn gen_codes(&self, n: usize) -> Vec<String> {
        self.admin(&["gen-code", "-n", &n.to_string(), "--note", "test"])
            .lines()
            .filter(|l| l.starts_with("KH-"))
            .map(String::from)
            .collect()
    }
}

struct Client {
    s: TcpStream,
    seal: FrameSealer,
    open: FrameOpener,
    id: u32,
    udp: UdpSocket,
    server_udp: SocketAddr,
    useal: DatagramSealer,
    uopen: DatagramOpener,
    probe_port: u16,
    resume: String,
}

#[derive(Debug)]
enum ConnErr {
    Silence,
    Rejected(String),
}

fn read_frame(s: &mut TcpStream, op: &mut FrameOpener) -> Option<(u8, Vec<u8>)> {
    let mut h = [0u8; FRAME_LEN_LEN];
    s.read_exact(&mut h).ok()?;
    let n = op.open_len(&h)?;
    let mut b = vec![0u8; n];
    s.read_exact(&mut b).ok()?;
    op.open_body(&b)
}

impl Client {
    fn connect(port: u16, code: &str, name: &str) -> Result<Client, ConnErr> {
        Self::connect_to("127.0.0.1", port, code, name)
    }

    fn connect_to(host: &str, port: u16, code: &str, name: &str) -> Result<Client, ConnErr> {
        Self::connect_with(host, port, code, name, false, None)
    }

    /// can_resume / resume：断线恢复（第 37-2 项）
    fn connect_with(host: &str, port: u16, code: &str, name: &str, can_resume: bool, resume: Option<(u32, String)>) -> Result<Client, ConnErr> {
        let code = AuthCode::parse(code).unwrap();
        let mut s = TcpStream::connect((host, port)).unwrap();
        // 全部测试一起跑时机器很忙，服务器握手可能回得慢：多等一会儿
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut hs = ClientHandshake::start(code.psk());
        s.write_all(hs.first_flight()).unwrap();
        let mut prefix = [0u8; SERVER_PREFIX_LEN];
        s.read_exact(&mut prefix).map_err(|_| ConnErr::Silence)?;
        let need = hs.read_server_prefix(&prefix).map_err(|_| ConnErr::Silence)?;
        let mut rest = vec![0u8; need];
        s.read_exact(&mut rest).unwrap();
        let keys = hs.finish(&rest).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

        let udp = UdpSocket::bind(if host == "127.0.0.1" { "127.0.0.1:0" } else { "0.0.0.0:0" }).unwrap();
        udp.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let server_udp: SocketAddr = format!("{host}:{port}").parse().unwrap();
        let mut c = Client {
            s,
            seal: FrameSealer::new(&keys.tcp_c2s),
            open: FrameOpener::new(&keys.tcp_s2c),
            id: 0,
            udp,
            server_udp,
            useal: DatagramSealer::new(&keys.udp_c2s, &keys.udp_c2s_hp),
            uopen: DatagramOpener::new(&keys.udp_s2c, &keys.udp_s2c_hp),
            probe_port: 0,
            resume: String::new(),
        };
        c.send(ClientMsg::Hello {
            name: name.into(),
            version: PROTOCOL_VERSION,
            can_resume,
            resume: resume.map(|(id, token)| voc_proto::ResumeToken { id, token }),
        });
        match c.recv_json() {
            ServerMsg::Welcome { user_id, probe_port, resume, .. } => {
                c.id = user_id;
                c.probe_port = probe_port;
                c.resume = resume;
                Ok(c)
            }
            ServerMsg::Resumed { user_id } => {
                c.id = user_id;
                c.resume = "resumed".into();
                Ok(c)
            }
            ServerMsg::Error { message } => Err(ConnErr::Rejected(message)),
            m => panic!("unexpected {m:?}"),
        }
    }

    fn send(&mut self, m: ClientMsg) {
        let f = self.seal.seal(KIND_JSON, serde_json::to_string(&m).unwrap().as_bytes());
        self.s.write_all(&f).unwrap();
    }

    fn send_tcp_voice(&mut self, seq: u16, frame: &[u8]) {
        let f = self.seal.seal(KIND_VOICE, &udp::client_voice(seq, frame));
        self.s.write_all(&f).unwrap();
    }

    fn send_raw_frame(&mut self, kind: u8, data: &[u8]) {
        let f = self.seal.seal(kind, data);
        self.s.write_all(&f).unwrap();
    }

    /// 等一个指定类型的帧（期间的其它帧丢掉），超时返回 None
    fn wait_frame(&mut self, kind: u8, ms: u64) -> Option<Vec<u8>> {
        self.s.set_read_timeout(Some(Duration::from_millis(ms))).unwrap();
        let deadline = Instant::now() + Duration::from_millis(ms);
        let r = loop {
            if Instant::now() > deadline {
                break None;
            }
            match self.recv_frame() {
                Some((k, d)) if k == kind => break Some(d),
                Some(_) => continue,
                None => break None,
            }
        };
        self.s.set_read_timeout(None).unwrap();
        r
    }

    fn recv_frame(&mut self) -> Option<(u8, Vec<u8>)> {
        read_frame(&mut self.s, &mut self.open)
    }

    fn recv_json(&mut self) -> ServerMsg {
        loop {
            let (k, d) = self.recv_frame().expect("connection closed");
            if k == KIND_JSON {
                return serde_json::from_slice(&d).unwrap();
            }
        }
    }

    /// 等到某条消息满足条件
    fn wait_for(&mut self, mut f: impl FnMut(&ServerMsg) -> bool) -> ServerMsg {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "timeout waiting for message");
            let m = self.recv_json();
            if f(&m) {
                return m;
            }
        }
    }

    fn wait_state(&mut self, f: impl Fn(&[UserInfo]) -> bool) {
        self.wait_for(|m| matches!(m, ServerMsg::State { users, .. } if f(users)));
    }

    fn udp_send(&mut self, kind: u8, data: &[u8]) -> Vec<u8> {
        let mut p = Vec::new();
        self.useal.seal(kind, data, &mut p);
        self.udp.send_to(&p, self.server_udp).unwrap();
        p
    }

    /// 收任意来源的 UDP 包
    fn udp_recv_raw(&self, ms: u64) -> Option<(SocketAddr, Vec<u8>)> {
        let mut buf = [0u8; 2048];
        let deadline = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < deadline {
            if let Ok((n, from)) = self.udp.recv_from(&mut buf) {
                return Some((from, buf[..n].to_vec()));
            }
        }
        None
    }

    fn udp_recv(&mut self) -> Option<(u8, Vec<u8>)> {
        let mut buf = [0u8; 2048];
        let deadline = Instant::now() + Duration::from_millis(600);
        while Instant::now() < deadline {
            let Ok((n, from)) = self.udp.recv_from(&mut buf) else { continue };
            if from != self.server_udp {
                continue;
            }
            if let Some(r) = self.uopen.open(&buf[..n]) {
                return Some(r);
            }
        }
        None
    }

    /// 注册 UDP 并告诉服务器走 UDP
    fn enable_udp(&mut self) {
        self.udp_send(udp::PING, &42u64.to_le_bytes());
        let (k, d) = self.udp_recv().expect("pong");
        assert_eq!((k, d), (udp::PONG, 42u64.to_le_bytes().to_vec()));
        self.send(ClientMsg::Transport { udp: true });
    }
}

/// 服务器不解码语音，内容随便
fn frame() -> Vec<u8> {
    (0..60u8).collect()
}

fn both_in(a: &mut Client, b: &mut Client, ch: u32) {
    a.send(ClientMsg::JoinChannel { channel_id: ch });
    b.send(ClientMsg::JoinChannel { channel_id: ch });
    let ids = [a.id, b.id];
    for c in [a, b] {
        c.wait_state(|u| ids.iter().all(|id| u.iter().any(|x| x.id == *id && x.channel_id == Some(ch))));
    }
}

#[test]
fn encrypted_udp_voice_and_replay() {
    let srv = Server::start(0);
    let codes = srv.gen_codes(2);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap();
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap();
    a.enable_udp();
    b.enable_udp();
    both_in(&mut a, &mut b, 1);

    let f = frame();
    let pkt = a.udp_send(udp::VOICE, &udp::client_voice(7, &f));
    let (k, d) = b.udp_recv().expect("bob should receive alice's voice");
    assert_eq!(k, udp::VOICE);
    let (from, seq, payload) = udp::read_server_voice(&d).unwrap();
    assert_eq!((from, seq, payload), (a.id, 7, &f[..]));
    assert!(a.udp_recv().is_none(), "sender must not hear itself");

    // 原样重放同一个包：服务器不应再转发
    a.udp.send_to(&pkt, a.server_udp).unwrap();
    assert!(b.udp_recv().is_none(), "replayed packet must be dropped");

    // 随机垃圾包：静默丢弃，也不影响后续
    a.udp.send_to(&[0xAB; 200], a.server_udp).unwrap();
    a.udp_send(udp::VOICE, &udp::client_voice(8, &f));
    assert!(b.udp_recv().is_some());

    // bob 换频道后收不到
    b.send(ClientMsg::JoinChannel { channel_id: 2 });
    let bid = b.id;
    b.wait_state(|u| u.iter().any(|x| x.id == bid && x.channel_id == Some(2)));
    a.udp_send(udp::VOICE, &udp::client_voice(9, &f));
    assert!(b.udp_recv().is_none());
}

#[test]
fn tcp_fallback_when_udp_blocked() {
    let srv = Server::start(1);
    let codes = srv.gen_codes(2);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap();
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap(); // bob 的 UDP "被封"，不启用
    a.enable_udp();
    both_in(&mut a, &mut b, 1);
    let f = frame();

    // A 走 UDP，B 应通过 TCP 隧道收到
    a.udp_send(udp::VOICE, &udp::client_voice(1, &f));
    // A 走 TCP，B 同样收到
    a.send_tcp_voice(2, &f);
    let mut seqs = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while seqs.len() < 2 && Instant::now() < deadline {
        let (k, d) = b.recv_frame().unwrap();
        if k == KIND_VOICE {
            let (from, seq, payload) = udp::read_server_voice(&d).unwrap();
            assert_eq!((from, payload), (a.id, &f[..]));
            seqs.push(seq);
        }
    }
    seqs.sort();
    assert_eq!(seqs, vec![1, 2]);
}

/// 等待：对方既不发数据，也不立即断开
fn expect_silence(s: &mut TcpStream, secs: u64) {
    s.set_read_timeout(Some(Duration::from_secs(secs))).unwrap();
    let mut buf = [0u8; 64];
    match s.read(&mut buf) {
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
        other => panic!("expected silence, got {other:?}"),
    }
}

#[test]
fn wrong_code_and_probes_get_no_response() {
    let srv = Server::start(2);
    srv.gen_codes(3);

    // 不存在的授权码：服务器一个字节都不回
    let stranger = AuthCode::generate().display();
    assert!(matches!(Client::connect(srv.port, &stranger, "x"), Err(ConnErr::Silence)));

    // 主动探测：随机数据 / HTTP 请求，都应石沉大海
    for probe in [vec![0x16, 0x03, 0x01, 0x00, 0xA5], b"GET / HTTP/1.1\r\nHost: x\r\n\r\n".to_vec(), vec![7u8; 1500]] {
        let mut s = TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
        s.write_all(&probe).unwrap();
        expect_silence(&mut s, 2);
    }

    // 重放一个合法的握手首包：也应沉默
    let codes = srv.gen_codes(1);
    let hs = ClientHandshake::start(AuthCode::parse(&codes[0]).unwrap().psk());
    let mut s1 = TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
    s1.write_all(hs.first_flight()).unwrap();
    let mut got = [0u8; SERVER_PREFIX_LEN];
    s1.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    s1.read_exact(&mut got).expect("first use answered");
    let mut s2 = TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
    s2.write_all(hs.first_flight()).unwrap();
    expect_silence(&mut s2, 2);
}

#[test]
fn revoke_kicks_and_same_code_takes_over() {
    let srv = Server::start(3);
    let codes = srv.gen_codes(2);
    let id = &codes[0][3..11];

    // 同一授权码二次登录：旧连接被顶掉
    let mut old = Client::connect(srv.port, &codes[0], "小明").unwrap();
    let mut new = Client::connect(srv.port, &codes[0], "小明").unwrap();
    let m = old.wait_for(|m| matches!(m, ServerMsg::Error { .. }));
    assert!(matches!(m, ServerMsg::Error { message } if message.contains("其他地方")));

    // 吊销：在线的人几秒内被踢，之后也登不上
    let out = srv.admin(&["revoke-code", id]);
    assert!(out.contains("已吊销"), "{out}");
    let m = new.wait_for(|m| matches!(m, ServerMsg::Error { .. }));
    assert!(matches!(m, ServerMsg::Error { message } if message.contains("吊销")));
    match Client::connect(srv.port, &codes[0], "小明") {
        Err(ConnErr::Rejected(m)) => assert!(m.contains("吊销"), "{m}"),
        other => panic!("expected rejection, got {:?}", other.err()),
    }
    // 另一个授权码不受影响；恢复后又能登录
    assert!(Client::connect(srv.port, &codes[1], "小红").is_ok());
    srv.admin(&["restore-code", id]);
    std::thread::sleep(Duration::from_millis(100));
    assert!(Client::connect(srv.port, &codes[0], "小明").is_ok());
}


/// 测试用的端到端身份
struct E2eId {
    id: Identity,
    kem: LocalKem,
}

impl E2eId {
    fn announce(c: &mut Client, name: &str) -> E2eId {
        let me = E2eId { id: Identity::generate(), kem: LocalKem::default() };
        let sig = me.id.sign_ek(name, &me.kem.ek);
        c.send(ClientMsg::E2eAnnounce {
            entry: E2eEntry { id: 0, name: name.into(), idk: b64(&me.id.public), ek: b64(&me.kem.ek), sig: b64(&sig) },
        });
        me
    }
}

/// 等到收到某人的身份公告（目录或新加入），并验签
fn entry_of(c: &mut Client, who: u32) -> E2eEntry {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "no directory entry for {who}");
        let entries = match c.recv_json() {
            ServerMsg::Directory { entries } => entries,
            ServerMsg::E2eJoin { entry } => vec![entry],
            _ => continue,
        };
        if let Some(e) = entries.into_iter().find(|e| e.id == who) {
            let (idk, ek, sig) = (unb64(&e.idk).unwrap(), unb64(&e.ek).unwrap(), unb64(&e.sig).unwrap());
            assert!(e2e::verify_ek(&idk, &e.name, &ek, &sig), "directory entry must verify");
            return e;
        }
    }
}

/// a（id 小）与 b 经服务器建立端到端成对密钥，返回 (a 的, b 的)
fn pair(a: &mut Client, ia: &E2eId, b: &mut Client, ib: &E2eId) -> (PairKeys, PairKeys) {
    assert!(a.id < b.id);
    let eb = entry_of(a, b.id);
    let ea = entry_of(b, a.id);
    let ek_b = unb64(&eb.ek).unwrap();
    let (ct, ss_a) = p2p::encapsulate(&ek_b).unwrap();
    let sig = ia.id.sign_pair(&ek_b, &ct);
    a.send(ClientMsg::PeerKey { to: b.id, ct: b64(&ct), sig: b64(&sig) });
    let (ct_b, sig_b) = match b.wait_for(|m| matches!(m, ServerMsg::PeerKey { .. })) {
        ServerMsg::PeerKey { from, ct, sig } => {
            assert_eq!(from, a.id);
            (unb64(&ct).unwrap(), unb64(&sig).unwrap())
        }
        _ => unreachable!(),
    };
    assert!(e2e::verify_pair(&unb64(&ea.idk).unwrap(), &ib.kem.ek, &ct_b, &sig_b));
    let ss_b = ib.kem.decapsulate(&ct_b).unwrap();
    (
        p2p::pair_keys(&ss_a, a.id, &ia.kem.ek, b.id, &ib.kem.ek),
        p2p::pair_keys(&ss_b, b.id, &ib.kem.ek, a.id, &ia.kem.ek),
    )
}

#[test]
fn server_only_relays_ciphertext() {
    let srv = Server::start(4);
    let codes = srv.gen_codes(2);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap();
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap();
    a.enable_udp();
    b.enable_udp();
    let ia = E2eId::announce(&mut a, "alice");
    let ib = E2eId::announce(&mut b, "bob");
    let (ka, kb) = pair(&mut a, &ia, &mut b, &ib);
    both_in(&mut a, &mut b, 1);

    // 聊天：端到端加密信封，服务器原样转交
    let secret = "今晚八点开黑".as_bytes();
    let blob = e2e::seal_blob(&ka.blob, e2e::BLOB_CHAT, secret);
    a.send(ClientMsg::E2eBlob { to: b.id, blob: b64(&blob) });
    let got = match b.wait_for(|m| matches!(m, ServerMsg::E2eBlob { .. })) {
        ServerMsg::E2eBlob { from, blob, .. } => {
            assert_eq!(from, a.id);
            unb64(&blob).unwrap()
        }
        _ => unreachable!(),
    };
    assert!(!got.windows(secret.len()).any(|w| w == secret), "server-visible blob must not contain plaintext");
    assert_eq!(e2e::open_blob(&kb.blob, &got), Some((e2e::BLOB_CHAT, secret.to_vec())));

    // 语音：先把发送者密钥（加密信封）交给 bob，再经服务器中转一帧
    let mut sk = SenderKey::new(1);
    a.send(ClientMsg::E2eBlob { to: b.id, blob: b64(&e2e::seal_blob(&ka.blob, e2e::BLOB_SENDER_KEY, &sk.export())) });
    let mut rk = ReceiverKeys::default();
    match b.wait_for(|m| matches!(m, ServerMsg::E2eBlob { .. })) {
        ServerMsg::E2eBlob { blob, .. } => {
            let (kind, data) = e2e::open_blob(&kb.blob, &unb64(&blob).unwrap()).unwrap();
            assert_eq!(kind, e2e::BLOB_SENDER_KEY);
            assert!(rk.install(&data));
        }
        _ => unreachable!(),
    }
    let opus = b"OPUS-FRAME-PLAINTEXT";
    let frame = sk.encrypt(a.id, opus);
    a.udp_send(udp::VOICE, &udp::client_voice(1, &frame));
    let (_, d) = b.udp_recv().expect("relayed voice");
    let (from, _, relayed) = udp::read_server_voice(&d).unwrap();
    // 服务器解开外层后看到的就是这个：不含明文
    assert!(!relayed.windows(opus.len()).any(|w| w == opus));
    assert_eq!(rk.decrypt(from, relayed).map(|x| x.1), Some(opus.to_vec()));
}

#[test]
fn p2p_signaling_punching_and_direct_voice() {
    let srv = Server::start(5);
    let codes = srv.gen_codes(3);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap();
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap();
    let mut c = Client::connect(srv.port, &codes[2], "carol").unwrap(); // 不开 P2P
    for x in [&mut a, &mut b, &mut c] {
        x.enable_udp();
    }
    let ia = E2eId::announce(&mut a, "alice");
    let ib = E2eId::announce(&mut b, "bob");
    let (k_a, k_b) = pair(&mut a, &ia, &mut b, &ib);
    a.send(ClientMsg::P2pEnable { local_addrs: vec!["192.168.1.5:1234".into(), "bogus".into()], nat: None });
    b.send(ClientMsg::P2pEnable { local_addrs: vec![], nat: None });
    for x in [&mut a, &mut b, &mut c] {
        x.send(ClientMsg::JoinChannel { channel_id: 1 });
    }

    let peers_of = |x: &mut Client| match x.wait_for(|m| matches!(m, ServerMsg::Peers { peers } if !peers.is_empty())) {
        ServerMsg::Peers { peers } => peers,
        _ => unreachable!(),
    };
    let pa = peers_of(&mut a);
    let pb = peers_of(&mut b);
    // 只看到对方；carol 没开 P2P，谁都看不到她的地址
    assert_eq!(pa.iter().map(|p| p.id).collect::<Vec<_>>(), vec![b.id]);
    assert_eq!(pb.iter().map(|p| p.id).collect::<Vec<_>>(), vec![a.id]);
    let b_addr: SocketAddr = pa[0].addrs[0].parse().unwrap();
    let a_addr: SocketAddr = pb[0].addrs[0].parse().unwrap();
    assert_eq!(b_addr.port(), b.udp.local_addr().unwrap().port());
    assert_eq!(pb[0].addrs, vec![a_addr.to_string(), "192.168.1.5:1234".to_string()]);

    let (mut a_tx, mut a_rx) = (DatagramSealer::new(&k_a.send, &k_a.send_hp), DatagramOpener::new(&k_a.recv, &k_a.recv_hp));
    let (mut b_tx, mut b_rx) = (DatagramSealer::new(&k_b.send, &k_b.send_hp), DatagramOpener::new(&k_b.recv, &k_b.recv_hp));

    // 打洞
    let mut p = Vec::new();
    a_tx.seal(p2p::PUNCH, b"t", &mut p);
    a.udp.send_to(&p, b_addr).unwrap();
    let (from, pkt) = b.udp_recv_raw(1000).expect("punch should arrive directly");
    assert_eq!(from.port(), a.udp.local_addr().unwrap().port());
    assert_eq!(b_rx.open(&pkt), Some((p2p::PUNCH, b"t".to_vec())));
    b_tx.seal(p2p::PUNCH_ACK, b"t", &mut p);
    b.udp.send_to(&p, from).unwrap();
    let (_, pkt) = a.udp_recv_raw(1000).expect("ack");
    assert_eq!(a_rx.open(&pkt), Some((p2p::PUNCH_ACK, b"t".to_vec())));

    // 已直连 bob：服务器只给 carol 转发
    a.send(ClientMsg::DirectPeers { ids: vec![b.id, c.id], video: vec![], share_audio: None }); // carol 不在 P2P 名单里，应被忽略
    a.send(ClientMsg::Ping);
    a.wait_for(|m| matches!(m, ServerMsg::Pong));
    let f = frame();
    a.udp_send(udp::VOICE, &udp::client_voice(1, &f));
    let (k, d) = c.udp_recv().expect("carol still gets relayed voice");
    assert_eq!((k, udp::read_server_voice(&d).unwrap().0), (udp::VOICE, a.id));
    assert!(b.udp_recv().is_none(), "server must not relay to a direct peer");

    // 直连语音
    a_tx.seal(udp::VOICE, &udp::server_voice(a.id, 1, &f), &mut p);
    a.udp.send_to(&p, b_addr).unwrap();
    let (_, pkt) = b.udp_recv_raw(1000).unwrap();
    let (k, d) = b_rx.open(&pkt).unwrap();
    assert_eq!((k, udp::read_server_voice(&d).unwrap()), (udp::VOICE, (a.id, 1, &f[..])));

    // bob 关掉 P2P：alice 的名单清空，服务器恢复给 bob 转发
    b.send(ClientMsg::P2pDisable);
    a.wait_for(|m| matches!(m, ServerMsg::Peers { peers } if peers.is_empty()));
    a.udp_send(udp::VOICE, &udp::client_voice(2, &f));
    assert!(b.udp_recv().is_some(), "relay resumes after peer leaves P2P");
}

#[test]
fn screen_share_watch_relay_and_keyframes() {
    let srv = Server::start(6);
    let codes = srv.gen_codes(3);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap(); // 共享者
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap(); // 观看者
    let mut c = Client::connect(srv.port, &codes[2], "carol").unwrap(); // 同频道但不看
    for x in [&mut a, &mut b, &mut c] {
        x.enable_udp();
        x.send(ClientMsg::JoinChannel { channel_id: 1 });
    }
    let aid = a.id;
    b.wait_state(|u| u.iter().filter(|x| x.channel_id == Some(1)).count() == 3);

    // 不在共享时不能观看
    b.send(ClientMsg::Watch { target: aid, on: true, low: false, route: 0, cursor: false });
    a.send(ClientMsg::ShareStart { desc: "720p30".into(), audio: false, simulcast: false, rtx: false, bps: 0, p2p_only: false });
    b.wait_state(|u| u.iter().any(|x| x.id == aid && x.sharing.as_deref() == Some("720p30")));
    // 之前那次 Watch 无效，alice 此时没有观看者；现在正式观看
    b.send(ClientMsg::Watch { target: aid, on: true, low: false, route: 0, cursor: false });
    let w = a.wait_for(|m| matches!(m, ServerMsg::Watchers { ids, .. } if !ids.is_empty()));
    assert!(matches!(w, ServerMsg::Watchers { ids, .. } if ids == vec![b.id]));

    // 视频分片经服务器只转给观看者
    let frame: Vec<u8> = (0..3000u32).map(|i| (i * 7) as u8).collect();
    let frags = udp::video_fragments(0, udp::VIDEO_KEY, &frame);
    for f in &frags {
        a.udp_send(udp::VIDEO, f);
    }
    let mut got = Vec::new();
    for _ in 0..frags.len() {
        let (k, d) = b.udp_recv().expect("watcher receives video fragment");
        assert_eq!(k, udp::VIDEO);
        let (from, h, data) = udp::read_server_video(&d).unwrap();
        assert_eq!((from, h.frame, h.count as usize), (aid, 0, frags.len()));
        got.push((h.idx, data.to_vec()));
    }
    got.sort_by_key(|x| x.0);
    assert_eq!(got.into_iter().flat_map(|x| x.1).collect::<Vec<_>>(), frame);
    assert!(c.udp_recv().is_none(), "non-watcher must not receive video");

    // 观看者丢包后请求关键帧 → 转给共享者
    b.send(ClientMsg::KeyframeRequest { target: aid });
    let k = a.wait_for(|m| matches!(m, ServerMsg::KeyframeRequest { .. }));
    assert!(matches!(k, ServerMsg::KeyframeRequest { by } if by == b.id));

    // 损坏的分片头被丢弃
    a.udp_send(udp::VIDEO, &[1, 2, 3]);
    assert!(b.udp_recv().is_none());

    // 停止共享：观看者名单清空，之后的视频不再转发
    a.send(ClientMsg::ShareStop);
    a.wait_for(|m| matches!(m, ServerMsg::Watchers { ids, .. } if ids.is_empty()));
    b.wait_state(|u| u.iter().any(|x| x.id == aid && x.sharing.is_none()));
    a.udp_send(udp::VIDEO, &frags[0]);
    assert!(b.udp_recv().is_none(), "no video after share stopped");
}

/// 观看端选画质：要低画质的人只收低画质层，其他人只收原画；共享者不支持时不分层
#[test]
fn simulcast_layers_routed_by_viewer_choice() {
    let srv = Server::start(13);
    let codes = srv.gen_codes(3);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap(); // 共享者
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap(); // 看原画
    let mut c = Client::connect(srv.port, &codes[2], "carol").unwrap(); // 看流畅
    for x in [&mut a, &mut b, &mut c] {
        x.enable_udp();
        x.send(ClientMsg::JoinChannel { channel_id: 1 });
    }
    let aid = a.id;
    b.wait_state(|u| u.iter().filter(|x| x.channel_id == Some(1)).count() == 3);
    a.send(ClientMsg::ShareStart { desc: "1080p60".into(), audio: false, simulcast: true, rtx: false, bps: 0, p2p_only: false });
    b.wait_state(|u| u.iter().any(|x| x.id == aid && x.simulcast));
    b.send(ClientMsg::Watch { target: aid, on: true, low: false, route: 0, cursor: false });
    c.send(ClientMsg::Watch { target: aid, on: true, low: true, route: 0, cursor: false });
    let (bid, cid) = (b.id, c.id);
    a.wait_for(|m| matches!(m, ServerMsg::Watchers { ids, low, .. } if ids.len() == 2 && low == &vec![cid]));

    let main = udp::video_fragments(0, udp::VIDEO_KEY, b"main");
    let low = udp::video_fragments(0, udp::VIDEO_KEY | udp::VIDEO_LOW, b"low");
    a.udp_send(udp::VIDEO, &main[0]);
    a.udp_send(udp::VIDEO, &low[0]);
    let (_, d) = b.udp_recv().expect("bob gets main layer");
    assert_eq!(udp::read_server_video(&d).unwrap().2, b"main");
    assert!(b.udp_recv().is_none(), "bob must not get the low layer");
    let (_, d) = c.udp_recv().expect("carol gets low layer");
    assert_eq!(udp::read_server_video(&d).unwrap().2, b"low");
    assert!(c.udp_recv().is_none(), "carol must not get the main layer");

    // carol 换回原画
    c.send(ClientMsg::Watch { target: aid, on: true, low: false, route: 0, cursor: false });
    a.wait_for(|m| matches!(m, ServerMsg::Watchers { ids, low, .. } if ids.len() == 2 && low.is_empty()));
    a.udp_send(udp::VIDEO, &main[0]);
    assert!(c.udp_recv().is_some() && b.udp_recv().is_some());

    // 共享者不支持分层（旧客户端）：要低画质也照收原画
    a.send(ClientMsg::ShareStop);
    a.send(ClientMsg::ShareStart { desc: "720p30".into(), audio: false, simulcast: false, rtx: false, bps: 0, p2p_only: false });
    b.wait_state(|u| u.iter().any(|x| x.id == aid && x.sharing.as_deref() == Some("720p30") && !x.simulcast));
    b.send(ClientMsg::Watch { target: aid, on: true, low: false, route: 0, cursor: false });
    c.send(ClientMsg::Watch { target: aid, on: true, low: true, route: 0, cursor: false });
    a.wait_for(|m| matches!(m, ServerMsg::Watchers { ids, low, .. } if ids == &vec![bid, cid] && low.is_empty()));
    a.udp_send(udp::VIDEO, &main[0]);
    assert!(c.udp_recv().is_some(), "old sharer: low-quality viewer still gets the only layer");
}

/// 丢包补发：服务器缓存里有的直接补发给请求者；没有的转给共享者；冗余校验包照常转发
#[test]
fn video_nack_served_from_cache_or_forwarded() {
    let srv = Server::start(15);
    let codes = srv.gen_codes(3);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap(); // 共享者
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap(); // 观看者
    let mut c = Client::connect(srv.port, &codes[2], "carol").unwrap(); // 没在看
    for x in [&mut a, &mut b, &mut c] {
        x.enable_udp();
        x.send(ClientMsg::JoinChannel { channel_id: 1 });
    }
    let (aid, bid) = (a.id, b.id);
    b.wait_state(|u| u.iter().filter(|x| x.channel_id == Some(1)).count() == 3);
    a.send(ClientMsg::ShareStart { desc: "1080p60".into(), audio: false, simulcast: false, rtx: true, bps: 0, p2p_only: false });
    b.wait_state(|u| u.iter().any(|x| x.id == aid && x.rtx));
    b.send(ClientMsg::Watch { target: aid, on: true, low: false, route: 0, cursor: false });
    a.wait_for(|m| matches!(m, ServerMsg::Watchers { ids, .. } if ids == &vec![bid]));

    let data: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
    let frags = udp::video_fragments(7, udp::VIDEO_KEY, &data);
    let par = udp::fec_parities(7, udp::VIDEO_KEY, &data, 10);
    for f in frags.iter().chain(par.iter()) {
        a.udp_send(udp::VIDEO, f);
    }
    for _ in 0..frags.len() + par.len() {
        let (k, _) = b.udp_recv().expect("bob gets fragments and parity");
        assert_eq!(k, udp::VIDEO);
    }

    // 缓存里有：直接补发给 bob，不打扰共享者
    b.udp_send(udp::NACK, &udp::nack_payload(aid, 0, 7, &[1]));
    let (k, d) = b.udp_recv().expect("resent from cache");
    let (from, h, body) = udp::read_server_video(&d).unwrap();
    assert_eq!((k, from, h.frame, h.idx), (udp::VIDEO, aid, 7, 1));
    assert_eq!(body, udp::read_video_frag(&frags[1]).unwrap().1);
    // 整帧
    b.udp_send(udp::NACK, &udp::nack_payload(aid, 0, 7, &[udp::NACK_WHOLE]));
    for i in 0..frags.len() {
        let (_, d) = b.udp_recv().expect("whole frame resent");
        assert_eq!(udp::read_server_video(&d).unwrap().1.idx as usize, i);
    }
    assert!(a.udp_recv().is_none(), "sharer not bothered when cache has it");

    // 缓存里没有（共享者→服务器这段丢了）：转给共享者，开头换成请求者编号
    b.udp_send(udp::NACK, &udp::nack_payload(aid, 0, 8, &[0, 2]));
    let (k, d) = a.udp_recv().expect("nack forwarded to sharer");
    assert_eq!((k, udp::read_nack(&d)), (udp::NACK, Some((bid, 0, 8, vec![0, 2]))));

    // 没在看的人发的补发请求不理
    c.udp_send(udp::NACK, &udp::nack_payload(aid, 0, 7, &[0]));
    c.udp_send(udp::NACK, &udp::nack_payload(aid, 0, 9, &[0]));
    assert!(c.udp_recv().is_none() && a.udp_recv().is_none());
}

/// 隐私：选了和某人走服务器（P2pHide），双方地址互不透露；其他人照常
#[test]
fn p2p_hide_keeps_addresses_private() {
    let srv = Server::start(17);
    let codes = srv.gen_codes(3);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap();
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap();
    let mut c = Client::connect(srv.port, &codes[2], "carol").unwrap();
    for x in [&mut a, &mut b, &mut c] {
        x.enable_udp();
        x.send(ClientMsg::P2pEnable { local_addrs: vec![], nat: None });
    }
    let (aid, bid, cid) = (a.id, b.id, c.id);
    a.send(ClientMsg::P2pHide { ids: vec![bid] });
    for x in [&mut a, &mut b, &mut c] {
        x.send(ClientMsg::JoinChannel { channel_id: 1 });
    }
    // 0.6 起不在同一频道也会下发（预先打洞），消息会来好几次：等到最终状态
    let settle = |x: &mut Client, want: Vec<u32>| {
        x.wait_for(|m| matches!(m, ServerMsg::Peers { peers } if peers.iter().map(|p| p.id).collect::<Vec<_>>() == want));
    };
    let (mut ac, mut ab) = (vec![aid, bid], vec![bid, cid]);
    ac.sort();
    ab.sort();
    settle(&mut a, vec![cid]);
    settle(&mut b, vec![cid]);
    settle(&mut c, ac);
    // 隐藏后，发给 alice 的名单里一直没有 bob
    a.send(ClientMsg::Ping);
    let mut saw_bob = false;
    loop {
        match a.recv_json() {
            ServerMsg::Peers { peers } => saw_bob |= peers.iter().any(|p| p.id == bid),
            ServerMsg::Pong => break,
            _ => {}
        }
    }
    assert!(!saw_bob, "alice must not get bob's address");
    // 改回自动：互相看得到
    a.send(ClientMsg::P2pHide { ids: vec![] });
    settle(&mut a, ab);
}

/// NAT 探测端口：已登录用户的 PING 回 PONG 并附上地址；乱发的包不理
#[test]
fn probe_port_answers_registered_users_only() {
    let srv = Server::start(16);
    let codes = srv.gen_codes(1);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap();
    if a.probe_port == 0 {
        eprintln!("probe port could not be bound (taken); skipping");
        return;
    }
    assert_eq!(a.probe_port, srv.port + 1);
    let probe: SocketAddr = format!("127.0.0.1:{}", a.probe_port).parse().unwrap();
    let mut p = Vec::new();
    a.useal.seal(udp::PING, &udp::ping_payload(77), &mut p);
    a.udp.send_to(&p, probe).unwrap();
    let (from, pkt) = a.udp_recv_raw(1000).expect("probe pong");
    assert_eq!(from, probe);
    let (k, d) = a.uopen.open(&pkt).unwrap();
    let (ts, addr) = udp::read_pong(&d).unwrap();
    assert_eq!((k, ts, addr), (udp::PONG, 77, Some(a.udp.local_addr().unwrap())));
    // 不是 PING、或者不是任何已登录用户加密的：不回
    a.useal.seal(udp::VOICE, &udp::client_voice(1, b"x"), &mut p);
    a.udp.send_to(&p, probe).unwrap();
    a.udp.send_to(b"garbage packet", probe).unwrap();
    assert!(a.udp_recv_raw(400).is_none());
}

/// 观看者选的画面线路转告给共享者
#[test]
fn viewer_route_choice_reaches_sharer() {
    let srv = Server::start(14);
    let codes = srv.gen_codes(3);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap();
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap();
    let mut c = Client::connect(srv.port, &codes[2], "carol").unwrap();
    for x in [&mut a, &mut b, &mut c] {
        x.send(ClientMsg::JoinChannel { channel_id: 1 });
    }
    let aid = a.id;
    b.wait_state(|u| u.iter().filter(|x| x.channel_id == Some(1)).count() == 3);
    a.send(ClientMsg::ShareStart { desc: "720p30".into(), audio: false, simulcast: true, rtx: false, bps: 0, p2p_only: false });
    b.wait_state(|u| u.iter().any(|x| x.id == aid && x.sharing.is_some()));
    let (bid, cid) = (b.id, c.id);
    b.send(ClientMsg::Watch { target: aid, on: true, low: false, route: 1, cursor: false });
    c.send(ClientMsg::Watch { target: aid, on: true, low: false, route: 2, cursor: false });
    a.wait_for(|m| matches!(m, ServerMsg::Watchers { ids, direct, server, .. } if ids.len() == 2 && direct == &vec![bid] && server == &vec![cid]));
    // 改回自动
    c.send(ClientMsg::Watch { target: aid, on: true, low: false, route: 0, cursor: false });
    a.wait_for(|m| matches!(m, ServerMsg::Watchers { ids, server, .. } if ids.len() == 2 && server.is_empty()));
}

/// 连真实服务器做冒烟测试：VOC_REMOTE=IP VOC_CODE=授权码 cargo test -p voc-server --test e2e remote_smoke -- --ignored --nocapture
#[test]
#[ignore]
fn remote_smoke() {
    let host = std::env::var("VOC_REMOTE").expect("VOC_REMOTE");
    let code = std::env::var("VOC_CODE").expect("VOC_CODE");
    let t = Instant::now();
    let mut c = Client::connect_to(&host, 7878, &code, "smoke-test").expect("login");
    println!("login ok as #{} in {:?}", c.id, t.elapsed());
    let mut udp_ok = false;
    for _ in 0..3 {
        c.udp_send(udp::PING, &1u64.to_le_bytes());
        let t = Instant::now();
        if let Some((k, _)) = c.udp_recv() {
            assert_eq!(k, udp::PONG);
            println!("UDP pong ok, rtt {:?}", t.elapsed());
            udp_ok = true;
            break;
        }
    }
    println!("UDP reachable: {udp_ok}");
    match c.wait_for(|m| matches!(m, ServerMsg::State { .. })) {
        ServerMsg::State { channels, users } => println!("channels: {}, users online: {}", channels.len(), users.len()),
        _ => unreachable!(),
    }
}

#[test]
fn channel_capacity_is_20() {
    let srv = Server::start(7);
    let codes = srv.gen_codes(21);
    // 21 个人连续登录；机器很忙时握手偶尔超时，重试一下
    let connect = |c: &str, name: String| {
        (0..3).find_map(|_| Client::connect(srv.port, c, &name).ok()).expect("login after 3 attempts")
    };
    let mut cs: Vec<Client> = codes.iter().enumerate().map(|(i, c)| connect(c, format!("u{i}"))).collect();
    for c in cs.iter_mut().take(20) {
        c.send(ClientMsg::JoinChannel { channel_id: 1 });
    }
    cs[0].wait_state(|u| u.iter().filter(|x| x.channel_id == Some(1)).count() == 20);
    // 第 21 个人进不去，只收到提示，不会被断开
    let last = cs.last_mut().unwrap();
    last.send(ClientMsg::JoinChannel { channel_id: 1 });
    let m = last.wait_for(|m| matches!(m, ServerMsg::System { text } if text.contains("已满")));
    assert!(matches!(m, ServerMsg::System { text } if text.contains("20")));
    // 其他频道照常可进
    last.send(ClientMsg::JoinChannel { channel_id: 2 });
    let id = last.id;
    last.wait_state(|u| u.iter().any(|x| x.id == id && x.channel_id == Some(2)));
    // 有人离开后又能进
    cs[0].send(ClientMsg::LeaveChannel);
    let last = cs.last_mut().unwrap();
    last.wait_state(|u| u.iter().filter(|x| x.channel_id == Some(1)).count() == 19);
    last.send(ClientMsg::JoinChannel { channel_id: 1 });
    last.wait_state(|u| u.iter().any(|x| x.id == id && x.channel_id == Some(1)));
}

/// 第 37-2 项：断线后 20 秒内带凭证回来，接回原来的编号；这期间别人看不到他离开、UDP 语音照常转发；
/// 凭证不对就拒绝；主动 Bye 的马上移除
#[test]
fn resume_after_disconnect() {
    let srv = Server::start(41);
    let codes = srv.gen_codes(2);
    let mut a = Client::connect_with("127.0.0.1", srv.port, &codes[0], "alice", true, None).unwrap();
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap();
    assert_eq!(a.resume.len(), 32, "新客户端拿到凭证");
    assert!(b.resume.is_empty(), "旧客户端没有凭证");
    for x in [&mut a, &mut b] {
        x.enable_udp();
        x.send(ClientMsg::JoinChannel { channel_id: 1 });
    }
    let aid = a.id;
    b.wait_state(|u| u.iter().filter(|x| x.channel_id == Some(1)).count() == 2);
    // a 的 TCP 断了（换网络）：服务器先保留
    a.s.shutdown(std::net::Shutdown::Both).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    a.udp_send(udp::VOICE, &udp::client_voice(1, b"opus"));
    assert_eq!(b.udp_recv().map(|x| x.0), Some(udp::VOICE), "断开期间 UDP 语音照常转发");
    // 凭证不对：拒绝
    match Client::connect_with("127.0.0.1", srv.port, &codes[0], "alice", true, Some((aid, "00".repeat(16)))) {
        Err(ConnErr::Rejected(m)) => assert_eq!(m, voc_proto::RESUME_FAILED),
        Err(ConnErr::Silence) => panic!("错的凭证应该被拒绝：服务器没回应"),
        Ok(c) => panic!("错的凭证应该被拒绝：却登录成功了（#{}，凭证 {:?}）", c.id, c.resume),
    }
    // 带对的凭证回来：同一个编号，还在频道里
    let token = a.resume.clone();
    let mut a2 = Client::connect_with("127.0.0.1", srv.port, &codes[0], "alice", true, Some((aid, token))).unwrap();
    assert_eq!((a2.id, a2.resume.as_str()), (aid, "resumed"));
    a2.wait_state(|u| u.iter().any(|x| x.id == aid && x.channel_id == Some(1)));
    // 主动退出：马上移除
    a2.send(ClientMsg::Bye);
    b.wait_state(|u| u.iter().all(|x| x.id != aid));
}

#[test]
fn voice_and_video_direct_lists_are_independent() {
    let srv = Server::start(8);
    let codes = srv.gen_codes(2);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap();
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap();
    for x in [&mut a, &mut b] {
        x.enable_udp();
        x.send(ClientMsg::P2pEnable { local_addrs: vec![], nat: None });
        x.send(ClientMsg::JoinChannel { channel_id: 1 });
    }
    let (aid, bid) = (a.id, b.id);
    a.wait_for(|m| matches!(m, ServerMsg::Peers { peers } if peers.iter().any(|p| p.id == bid)));
    a.send(ClientMsg::ShareStart { desc: "1080p60".into(), audio: false, simulcast: false, rtx: false, bps: 0, p2p_only: false });
    b.wait_state(|u| u.iter().any(|x| x.id == aid && x.sharing.is_some()));
    b.send(ClientMsg::Watch { target: aid, on: true, low: false, route: 0, cursor: false });
    a.wait_for(|m| matches!(m, ServerMsg::Watchers { ids, .. } if ids == &vec![bid]));
    let frag = udp::video_fragments(0, udp::VIDEO_KEY, b"frame").remove(0);
    let sync = |a: &mut Client| {
        a.send(ClientMsg::Ping);
        a.wait_for(|m| matches!(m, ServerMsg::Pong));
    };

    // 画面直连、语音中转
    a.send(ClientMsg::DirectPeers { ids: vec![], video: vec![bid], share_audio: None });
    sync(&mut a);
    a.udp_send(udp::VIDEO, &frag);
    assert!(b.udp_recv().is_none(), "video must not be relayed to a video-direct peer");
    a.udp_send(udp::VOICE, &udp::client_voice(1, b"opus"));
    assert_eq!(b.udp_recv().map(|x| x.0), Some(udp::VOICE), "voice still relayed");

    // 反过来：语音直连、画面中转
    a.send(ClientMsg::DirectPeers { ids: vec![bid], video: vec![], share_audio: None });
    sync(&mut a);
    a.udp_send(udp::VOICE, &udp::client_voice(2, b"opus"));
    assert!(b.udp_recv().is_none(), "voice must not be relayed to a voice-direct peer");
    a.udp_send(udp::VIDEO, &frag);
    assert_eq!(b.udp_recv().map(|x| x.0), Some(udp::VIDEO), "video still relayed");

    // 旧版客户端不带 video 字段：画面照常中转
    let raw = r#"{"type":"direct_peers","ids":[]}"#;
    let old: ClientMsg = serde_json::from_str(raw).unwrap();
    assert_eq!(old, ClientMsg::DirectPeers { ids: vec![], video: vec![], share_audio: None });
}

/// 第 54 项：共享声音单独的直连名单。没报（旧客户端）跟画面走；报了就按它，和画面无关
#[test]
fn share_audio_direct_list_is_separate() {
    let srv = Server::start(40);
    let codes = srv.gen_codes(2);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap();
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap();
    for x in [&mut a, &mut b] {
        x.enable_udp();
        x.send(ClientMsg::P2pEnable { local_addrs: vec![], nat: None });
        x.send(ClientMsg::JoinChannel { channel_id: 1 });
    }
    let (aid, bid) = (a.id, b.id);
    a.wait_for(|m| matches!(m, ServerMsg::Peers { peers } if peers.iter().any(|p| p.id == bid)));
    a.send(ClientMsg::ShareStart { desc: "1080p60".into(), audio: true, simulcast: false, rtx: false, bps: 0, p2p_only: false });
    b.wait_state(|u| u.iter().any(|x| x.id == aid && x.sharing.is_some()));
    b.send(ClientMsg::Watch { target: aid, on: true, low: false, route: 0, cursor: false });
    a.wait_for(|m| matches!(m, ServerMsg::Watchers { ids, .. } if ids == &vec![bid]));
    let sync = |a: &mut Client| {
        a.send(ClientMsg::Ping);
        a.wait_for(|m| matches!(m, ServerMsg::Pong));
    };
    let mut seq = 0u16;
    let mut audio = |a: &mut Client, b: &mut Client| {
        seq += 1;
        a.udp_send(udp::SHARE_AUDIO, &udp::client_voice(seq, b"opus"));
        b.udp_recv().map(|x| x.0)
    };
    // 旧客户端：画面直连 → 共享声音也不转
    a.send(ClientMsg::DirectPeers { ids: vec![], video: vec![bid], share_audio: None });
    sync(&mut a);
    assert_eq!(audio(&mut a, &mut b), None);
    // 新客户端：画面直连，但共享声音选了「经服务器」（名单为空）→ 照转
    a.send(ClientMsg::DirectPeers { ids: vec![], video: vec![bid], share_audio: Some(vec![]) });
    sync(&mut a);
    assert_eq!(audio(&mut a, &mut b), Some(udp::SHARE_AUDIO));
    // 画面经服务器，共享声音直连 → 不转
    a.send(ClientMsg::DirectPeers { ids: vec![], video: vec![], share_audio: Some(vec![bid]) });
    sync(&mut a);
    assert_eq!(audio(&mut a, &mut b), None);
}

/// 按听众保护语音（0.6）：bob 报告经服务器收语音丢包 20% → 服务器只给 bob 每包多发一份，carol 照常一份
#[test]
fn lossy_listener_gets_duplicate_voice() {
    let srv = Server::start(9);
    let codes = srv.gen_codes(3);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap();
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap();
    let mut c = Client::connect(srv.port, &codes[2], "carol").unwrap();
    for x in [&mut a, &mut b, &mut c] {
        x.enable_udp();
        x.send(ClientMsg::JoinChannel { channel_id: 1 });
    }
    let aid = a.id;
    b.wait_state(|u| u.iter().filter(|x| x.channel_id == Some(1)).count() == 3);
    b.send(ClientMsg::LinkStats {
        server_rtt: 50,
        peers: vec![],
        relay_slots: 0,
        via_server: vec![],
        video_relay_bps: 0,
        talking: false,
        heard: vec![],
        manual_via: vec![],
        voice_loss: 20,
    });
    b.send(ClientMsg::Ping);
    b.wait_for(|m| matches!(m, ServerMsg::Pong));
    a.udp_send(udp::VOICE, &udp::client_voice(1, &frame()));
    for _ in 0..2 {
        let (k, d) = b.udp_recv().expect("bob gets two copies");
        assert_eq!((k, udp::read_server_voice(&d).unwrap().0), (udp::VOICE, aid));
    }
    assert!(c.udp_recv().is_some());
    assert!(c.udp_recv().is_none(), "carol gets one copy");
}

/// 语音优先（0.6）：alice 说 bob 是直连，但 alice 在说话时 bob 连续两次报告没听到她
/// → 服务器让 alice 改经服务器发给 bob（VoiceFallback），并且照转 alice 的语音给 bob
#[test]
fn voice_fallback_when_listener_does_not_hear() {
    use voc_proto::PeerStat;
    let srv = Server::start(9);
    let codes = srv.gen_codes(2);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap();
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap();
    for x in [&mut a, &mut b] {
        x.enable_udp();
        x.send(ClientMsg::P2pEnable { local_addrs: vec![], nat: None });
        x.send(ClientMsg::JoinChannel { channel_id: 1 });
    }
    for x in [&mut a, &mut b] {
        x.wait_for(|m| matches!(m, ServerMsg::Peers { peers } if peers.len() == 1));
    }
    let (ia, ib) = (a.id, b.id);
    let stats = |peer: u32, talking: bool, heard: Vec<u32>| ClientMsg::LinkStats {
        server_rtt: 50,
        peers: vec![PeerStat { id: peer, rtt: 20, loss: 0, jitter: 2 }],
        relay_slots: 0,
        via_server: vec![],
        video_relay_bps: 0,
        talking,
        heard,
        manual_via: vec![],
        voice_loss: 0,
    };
    a.send(ClientMsg::DirectPeers { ids: vec![ib], video: vec![], share_audio: None });
    // 直连：服务器不转
    a.send(stats(ib, true, vec![]));
    a.send(ClientMsg::Ping);
    a.wait_for(|m| matches!(m, ServerMsg::Pong));
    a.udp_send(udp::VOICE, &udp::client_voice(1, &frame()));
    assert!(b.udp_recv().is_none(), "direct path: server does not forward");
    // bob 一次没听到：还不算（可能是刚开始说话）
    b.send(stats(ia, false, vec![]));
    b.send(ClientMsg::Ping);
    b.wait_for(|m| matches!(m, ServerMsg::Pong));
    // 第二次还是没听到 → 改经服务器
    a.send(stats(ib, true, vec![]));
    b.send(stats(ia, false, vec![]));
    a.wait_for(|m| matches!(m, ServerMsg::VoiceFallback { ids } if ids == &vec![ib]));
    a.udp_send(udp::VOICE, &udp::client_voice(2, &frame()));
    let (k, d) = b.udp_recv().expect("server forwards to the listener who couldn't hear");
    assert_eq!((k, udp::read_server_voice(&d).unwrap().0), (udp::VOICE, ia));
}

/// 成员中继：alice、bob 互相打不通，carol 和两人都直连且延迟低 → 服务器安排 carol 中转，
/// 并且不再替 alice 把语音转给 bob；carol 关掉中继后服务器恢复转发。
#[test]
fn member_relay_is_planned_and_server_stops_forwarding() {
    use voc_proto::{PeerStat, RelayRoute};
    let srv = Server::start(9);
    let codes = srv.gen_codes(3);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap();
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap();
    let mut r = Client::connect(srv.port, &codes[2], "carol").unwrap();
    for x in [&mut a, &mut b, &mut r] {
        x.enable_udp();
        x.send(ClientMsg::P2pEnable { local_addrs: vec![], nat: None });
        x.send(ClientMsg::JoinChannel { channel_id: 1 });
    }
    for x in [&mut a, &mut b, &mut r] {
        x.wait_for(|m| matches!(m, ServerMsg::Peers { peers } if peers.len() == 2));
    }
    let (ia, ib, ir) = (a.id, b.id, r.id);
    let stat = |id: u32| PeerStat { id, rtt: 20, loss: 0, jitter: 2 };
    a.send(ClientMsg::DirectPeers { ids: vec![ir], video: vec![], share_audio: None });
    b.send(ClientMsg::DirectPeers { ids: vec![ir], video: vec![], share_audio: None });
    r.send(ClientMsg::DirectPeers { ids: vec![ia, ib], video: vec![], share_audio: None });
    a.send(ClientMsg::LinkStats { server_rtt: 150, peers: vec![stat(ir)], relay_slots: 0, via_server: vec![], video_relay_bps: 0, talking: false, heard: vec![], manual_via: vec![], voice_loss: 0 });
    b.send(ClientMsg::LinkStats { server_rtt: 150, peers: vec![stat(ir)], relay_slots: 0, via_server: vec![], video_relay_bps: 0, talking: false, heard: vec![], manual_via: vec![], voice_loss: 0 });
    // carol 顺带报了一个不在频道里的 id，服务器应忽略
    r.send(ClientMsg::LinkStats { server_rtt: 150, peers: vec![stat(ia), stat(ib), stat(999)], relay_slots: 4, via_server: vec![], video_relay_bps: 0, talking: false, heard: vec![], manual_via: vec![], voice_loss: 0 });

    // 经 carol：20 + 20 + 4 = 44 ms；经服务器：150 + 150 = 300 ms
    let want = vec![
        RelayRoute { from: ia, to: ib, via: ir, ms: 44, server_ms: 300, manual: false },
        RelayRoute { from: ib, to: ia, via: ir, ms: 44, server_ms: 300, manual: false },
    ];
    let mut expected = want.clone();
    expected.sort();
    for x in [&mut a, &mut b, &mut r] {
        x.wait_for(|m| matches!(m, ServerMsg::Relays { routes } if *routes == expected));
    }

    // alice 发语音：bob 由 carol 中继，carol 是直连 → 服务器谁都不转
    a.udp_send(udp::VOICE, &udp::client_voice(1, &frame()));
    assert!(b.udp_recv().is_none(), "server must not forward a relayed path");
    assert!(r.udp_recv().is_none());

    // carol 关掉中继（名额 0）→ 路线撤销，服务器恢复转发给 bob
    r.send(ClientMsg::LinkStats { server_rtt: 150, peers: vec![stat(ia), stat(ib)], relay_slots: 0, via_server: vec![], video_relay_bps: 0, talking: false, heard: vec![], manual_via: vec![], voice_loss: 0 });
    for x in [&mut a, &mut b, &mut r] {
        x.wait_for(|m| matches!(m, ServerMsg::Relays { routes } if routes.is_empty()));
    }
    a.udp_send(udp::VOICE, &udp::client_voice(2, &frame()));
    let (k, d) = b.udp_recv().expect("server forwards again after the relay is withdrawn");
    assert_eq!(k, udp::VOICE);
    assert_eq!(udp::read_server_voice(&d).unwrap().0, ia);
}


/// 观看者互相转发画面：共享者 alice 直连发给 carol，bob 和 carol 直连稳定 → 服务器安排 carol 转给 bob，
/// 不再替 alice 转画面给 bob；carol 不看了 → 路线撤销，服务器恢复转发
#[test]
fn viewer_video_relay_is_planned() {
    use voc_proto::{PeerStat, RelayRoute};
    let srv = Server::start(18);
    let codes = srv.gen_codes(3);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap(); // 共享者
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap(); // 打不通 alice
    let mut r = Client::connect(srv.port, &codes[2], "carol").unwrap(); // 直连 alice、也直连 bob
    for x in [&mut a, &mut b, &mut r] {
        x.enable_udp();
        x.send(ClientMsg::P2pEnable { local_addrs: vec![], nat: None });
        x.send(ClientMsg::JoinChannel { channel_id: 1 });
    }
    for x in [&mut a, &mut b, &mut r] {
        x.wait_for(|m| matches!(m, ServerMsg::Peers { peers } if peers.len() == 2));
    }
    let (ia, ib, ir) = (a.id, b.id, r.id);
    a.send(ClientMsg::ShareStart { desc: "1080p60".into(), audio: false, simulcast: false, rtx: true, bps: 6_000_000, p2p_only: false });
    b.wait_state(|u| u.iter().any(|x| x.id == ia && x.sharing.is_some()));
    for x in [&mut b, &mut r] {
        x.send(ClientMsg::Watch { target: ia, on: true, low: false, route: 0, cursor: true });
    }
    a.wait_for(|m| matches!(m, ServerMsg::Watchers { ids, .. } if ids.len() == 2));
    let stat = |id: u32| PeerStat { id, rtt: 20, loss: 0, jitter: 2 };
    a.send(ClientMsg::DirectPeers { ids: vec![ir], video: vec![ir], share_audio: None });
    b.send(ClientMsg::LinkStats { server_rtt: 80, peers: vec![stat(ir)], relay_slots: 0, via_server: vec![], video_relay_bps: 0, talking: false, heard: vec![], manual_via: vec![], voice_loss: 0 });
    // carol 上行不够：不安排
    r.send(ClientMsg::LinkStats { server_rtt: 80, peers: vec![stat(ia), stat(ib)], relay_slots: 4, via_server: vec![], video_relay_bps: 3_000_000, talking: false, heard: vec![], manual_via: vec![], voice_loss: 0 });
    std::thread::sleep(Duration::from_millis(1500));
    // carol 上行够了
    r.send(ClientMsg::LinkStats { server_rtt: 80, peers: vec![stat(ia), stat(ib)], relay_slots: 4, via_server: vec![], video_relay_bps: 20_000_000, talking: false, heard: vec![], manual_via: vec![], voice_loss: 0 });
    let want = vec![RelayRoute { from: ia, to: ib, via: ir, ms: 0, server_ms: 0, manual: false }];
    for x in [&mut a, &mut b, &mut r] {
        x.wait_for(|m| matches!(m, ServerMsg::VideoRelays { routes } if *routes == want));
    }
    // 服务器不再替 alice 把画面转给 bob（carol 是直连，也不转）
    let frag = udp::video_fragments(0, udp::VIDEO_KEY, b"x");
    a.udp_send(udp::VIDEO, &frag[0]);
    assert!(b.udp_recv().is_none(), "relayed viewer gets the video from carol, not the server");

    // carol 不看了：撤销，服务器恢复转发
    r.send(ClientMsg::Watch { target: ia, on: false, low: false, route: 0, cursor: false });
    a.wait_for(|m| matches!(m, ServerMsg::VideoRelays { routes } if routes.is_empty()));
    a.udp_send(udp::VIDEO, &frag[0]);
    assert_eq!(b.udp_recv().map(|x| x.0), Some(udp::VIDEO));
}

/// 运行状态：服务器每 5 秒写状态文件，`voc-server status` 能读出在线人数、频道、探测端口
#[test]
fn status_command_shows_running_state() {
    let srv = Server::start(19);
    let codes = srv.gen_codes(1);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap();
    a.send(ClientMsg::JoinChannel { channel_id: 1 });
    std::thread::sleep(Duration::from_millis(6000));
    let out = Command::new(EXE).args(["status", "--codes"]).arg(&srv.codes).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("在线 1 人"), "{text}");
    assert!(text.contains("alice"), "{text}");
    assert!(text.contains("NAT 探测"), "{text}");
    assert!(text.contains("画面"), "{text}");
}

/// 图片分块：只转给同频道的其他人，带上发送者；别的频道收不到；同时最多传 2 张
#[test]
fn image_chunks_go_to_same_channel_only() {
    use voc_proto::crypto::KIND_FILE;
    use voc_proto::file;
    let srv = Server::start(10);
    let codes = srv.gen_codes(3);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap();
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap();
    let mut c = Client::connect(srv.port, &codes[2], "carol").unwrap();
    both_in(&mut a, &mut b, 1);
    c.send(ClientMsg::JoinChannel { channel_id: 2 });
    let cid = c.id;
    c.wait_state(|u| u.iter().any(|x| x.id == cid && x.channel_id == Some(2)));

    let key = [3u8; 32];
    let ct = file::encrypt_chunk(&key, 77, 0, b"png bytes");
    a.send_raw_frame(KIND_FILE, &file::client_chunk(77, 0, 1, &ct));
    let got = b.wait_frame(KIND_FILE, 3000).expect("bob gets the chunk");
    let (from, id, seq, total, cipher) = file::read_server_chunk(&got).unwrap();
    assert_eq!((from, id, seq, total), (a.id, 77, 0, 1));
    assert_eq!(file::decrypt_chunk(&key, id, seq, cipher).unwrap(), b"png bytes");
    assert!(c.wait_frame(KIND_FILE, 800).is_none(), "other channel must not get it");
    assert!(a.wait_frame(KIND_FILE, 300).is_none(), "sender does not get its own chunk back");

    // 同时最多 2 张：第 3 张未完成的图被丢弃
    for id in [100u64, 101, 102] {
        a.send_raw_frame(KIND_FILE, &file::client_chunk(id, 0, 2, &ct));
    }
    let mut ids = Vec::new();
    while let Some(f) = b.wait_frame(KIND_FILE, 800) {
        ids.push(file::read_server_chunk(&f).unwrap().1);
    }
    assert_eq!(ids, vec![100, 101]);
    // 格式不对的块被丢弃（块数超上限）
    a.send_raw_frame(KIND_FILE, &file::client_chunk(200, 0, file::MAX_CHUNKS + 1, &ct));
    assert!(b.wait_frame(KIND_FILE, 500).is_none());
}

/// 私聊发图：FileTarget 之后的那张图只转给对方，不管在不在同一频道
#[test]
fn dm_image_goes_to_target_only() {
    use voc_proto::crypto::KIND_FILE;
    use voc_proto::file;
    let srv = Server::start(12);
    let codes = srv.gen_codes(3);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap();
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap();
    let mut c = Client::connect(srv.port, &codes[2], "carol").unwrap();
    // alice 和 bob 同频道，carol 在大厅外
    both_in(&mut a, &mut b, 1);
    let ct = file::encrypt_chunk(&[5u8; 32], 9, 0, b"secret png");
    a.send(ClientMsg::FileTarget { id: 9, to: c.id });
    a.send_raw_frame(KIND_FILE, &file::client_chunk(9, 0, 2, &ct));
    a.send_raw_frame(KIND_FILE, &file::client_chunk(9, 1, 2, &ct));
    for seq in 0..2 {
        let got = c.wait_frame(KIND_FILE, 3000).expect("carol gets the dm chunk");
        let (from, id, s, total, _) = file::read_server_chunk(&got).unwrap();
        assert_eq!((from, id, s, total), (a.id, 9, seq, 2));
    }
    assert!(b.wait_frame(KIND_FILE, 800).is_none(), "same-channel member must not get a dm image");
    // 不是私聊的图照旧发给同频道
    a.send_raw_frame(KIND_FILE, &file::client_chunk(10, 0, 1, &ct));
    assert!(b.wait_frame(KIND_FILE, 3000).is_some());
    assert!(c.wait_frame(KIND_FILE, 800).is_none());
}

/// 共享者选了「只走 P2P」：服务器不转发画面和共享声音，状态里告诉观看者原因
#[test]
fn p2p_only_share_is_not_forwarded() {
    let srv = Server::start(11);
    let codes = srv.gen_codes(2);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap();
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap();
    for x in [&mut a, &mut b] {
        x.enable_udp();
        x.send(ClientMsg::JoinChannel { channel_id: 1 });
    }
    let aid = a.id;
    a.send(ClientMsg::ShareStart { desc: "1080p60".into(), audio: true, simulcast: false, rtx: true, bps: 0, p2p_only: true });
    b.wait_state(|u| u.iter().any(|x| x.id == aid && x.sharing.is_some() && x.share_p2p_only));
    b.send(ClientMsg::Watch { target: aid, on: true, low: false, route: 0, cursor: false });
    b.send(ClientMsg::Ping);
    b.wait_for(|m| matches!(m, ServerMsg::Pong));
    let frag = udp::video_fragments(1, udp::VIDEO_KEY, b"frame")[0].clone();
    a.udp_send(udp::VIDEO, &frag);
    a.udp_send(udp::SHARE_AUDIO, &udp::client_voice(1, &frame()));
    assert!(b.udp_recv().is_none(), "nothing of a p2p-only share goes through the server");
    // 关掉「只走 P2P」重新共享：照常转发
    a.send(ClientMsg::ShareStart { desc: "1080p60".into(), audio: true, simulcast: false, rtx: true, bps: 0, p2p_only: false });
    b.wait_state(|u| u.iter().any(|x| x.id == aid && x.sharing.is_some() && !x.share_p2p_only));
    a.udp_send(udp::VIDEO, &frag);
    assert_eq!(b.udp_recv().expect("normal share is forwarded").0, udp::VIDEO);
}

/// UDP 不通的人：伴奏经 TCP 发上来、也经 TCP 收到（0.6 起；以前这种人收不到伴奏和共享声音）
#[test]
fn music_over_tcp_when_udp_is_blocked() {
    let srv = Server::start(11);
    let codes = srv.gen_codes(2);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap();
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap();
    // 两人都没开 UDP
    a.send(ClientMsg::JoinChannel { channel_id: 1 });
    b.send(ClientMsg::JoinChannel { channel_id: 1 });
    let aid = a.id;
    a.send(ClientMsg::SetMusic { name: Some("音乐".into()) });
    b.wait_state(|u| u.iter().any(|x| x.id == aid && x.music.is_some() && x.channel_id == Some(1)) && u.iter().filter(|x| x.channel_id == Some(1)).count() == 2);
    a.send_raw_frame(KIND_STREAM, &stream_frame(udp::MUSIC, &udp::client_voice(5, &frame())));
    let got = b.wait_frame(KIND_STREAM, 3000).expect("bob gets the music over TCP");
    assert_eq!(got[0], udp::MUSIC);
    let (from, seq, payload) = udp::read_server_voice(&got[1..]).unwrap();
    assert_eq!((from, seq, payload), (aid, 5, &frame()[..]));
    // 不认识的类型不转
    a.send_raw_frame(KIND_STREAM, &stream_frame(udp::VOICE, &udp::client_voice(6, &frame())));
    assert!(b.wait_frame(KIND_STREAM, 800).is_none());
}

/// 伴奏：发给同频道所有没静音的人（发送者闭麦也照转）；屏幕共享声音只发给观看者
#[test]
fn music_and_share_audio_forwarding() {
    let srv = Server::start(11);
    let codes = srv.gen_codes(4);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap();
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap();
    let mut c = Client::connect(srv.port, &codes[2], "carol").unwrap();
    let mut d = Client::connect(srv.port, &codes[3], "dave").unwrap();
    for x in [&mut a, &mut b, &mut c, &mut d] {
        x.enable_udp();
    }
    for x in [&mut a, &mut b, &mut c] {
        x.send(ClientMsg::JoinChannel { channel_id: 1 });
    }
    d.send(ClientMsg::JoinChannel { channel_id: 2 });
    let aid = a.id;
    a.send(ClientMsg::SetMusic { name: Some("网易云音乐".into()) });
    a.send(ClientMsg::SetState { muted: true, deafened: false }); // 闭麦也能放伴奏
    c.send(ClientMsg::SetState { muted: true, deafened: true }); // carol 静音：收不到
    // 两件事的广播顺序不定：一次等齐
    let cid = c.id;
    b.wait_state(|u| {
        u.iter().any(|x| x.id == aid && x.music.as_deref() == Some("网易云音乐") && x.muted)
            && u.iter().any(|x| x.id == cid && x.deafened)
    });

    a.udp_send(udp::MUSIC, &udp::client_voice(1, &frame()));
    let (k, data) = b.udp_recv().expect("bob hears the music");
    assert_eq!(k, udp::MUSIC);
    assert_eq!(udp::read_server_voice(&data).unwrap().0, aid);
    assert!(c.udp_recv().is_none(), "deafened member gets nothing");
    assert!(d.udp_recv().is_none(), "other channel gets nothing");
    // 停止伴奏后再发的包不转
    a.send(ClientMsg::SetMusic { name: None });
    b.wait_state(|u| u.iter().any(|x| x.id == aid && x.music.is_none()));
    a.udp_send(udp::MUSIC, &udp::client_voice(2, &frame()));
    assert!(b.udp_recv().is_none(), "music after stop is dropped");

    // 屏幕共享带声音：只有观看者收到
    a.send(ClientMsg::ShareStart { desc: "1080p60".into(), audio: true, simulcast: false, rtx: false, bps: 0, p2p_only: false });
    b.wait_state(|u| u.iter().any(|x| x.id == aid && x.share_audio));
    b.send(ClientMsg::Watch { target: aid, on: true, low: false, route: 0, cursor: false });
    b.send(ClientMsg::Ping);
    b.wait_for(|m| matches!(m, ServerMsg::Pong));
    a.udp_send(udp::SHARE_AUDIO, &udp::client_voice(1, &frame()));
    let (k, _) = b.udp_recv().expect("viewer hears the shared audio");
    assert_eq!(k, udp::SHARE_AUDIO);
    c.send(ClientMsg::SetState { muted: false, deafened: false });
    c.send(ClientMsg::Ping);
    c.wait_for(|m| matches!(m, ServerMsg::Pong));
    a.udp_send(udp::SHARE_AUDIO, &udp::client_voice(2, &frame()));
    assert!(b.udp_recv().is_some());
    assert!(c.udp_recv().is_none(), "non-viewer does not get shared audio");
}
