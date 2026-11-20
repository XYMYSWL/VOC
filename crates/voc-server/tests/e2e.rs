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

impl Server {
    fn start(offset: u16) -> Server {
        let port = 41000 + (std::process::id() % 9000) as u16 + offset * 3;
        let codes = std::env::temp_dir().join(format!("voc-test-{}-{offset}.json", std::process::id()));
        let _ = std::fs::remove_file(&codes);
        let child = Command::new(EXE)
            .args(["--bind", &format!("127.0.0.1:{port}"), "--codes"])
            .arg(&codes)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let t = Instant::now();
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(t.elapsed() < Duration::from_secs(5), "server did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
        Server { child, port, codes }
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
        let code = AuthCode::parse(code).unwrap();
        let mut s = TcpStream::connect((host, port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
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
        };
        c.send(ClientMsg::Hello { name: name.into(), version: PROTOCOL_VERSION });
        match c.recv_json() {
            ServerMsg::Welcome { user_id, .. } => {
                c.id = user_id;
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
    a.send(ClientMsg::P2pEnable { local_addrs: vec!["192.168.1.5:1234".into(), "bogus".into()] });
    b.send(ClientMsg::P2pEnable { local_addrs: vec![] });
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
    a.send(ClientMsg::DirectPeers { ids: vec![b.id, c.id], video: vec![] }); // carol 不在 P2P 名单里，应被忽略
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
    b.send(ClientMsg::Watch { target: aid, on: true });
    a.send(ClientMsg::ShareStart { desc: "720p30".into() });
    b.wait_state(|u| u.iter().any(|x| x.id == aid && x.sharing.as_deref() == Some("720p30")));
    // 之前那次 Watch 无效，alice 此时没有观看者；现在正式观看
    b.send(ClientMsg::Watch { target: aid, on: true });
    let w = a.wait_for(|m| matches!(m, ServerMsg::Watchers { ids } if !ids.is_empty()));
    assert!(matches!(w, ServerMsg::Watchers { ids } if ids == vec![b.id]));

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
    a.wait_for(|m| matches!(m, ServerMsg::Watchers { ids } if ids.is_empty()));
    b.wait_state(|u| u.iter().any(|x| x.id == aid && x.sharing.is_none()));
    a.udp_send(udp::VIDEO, &frags[0]);
    assert!(b.udp_recv().is_none(), "no video after share stopped");
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
    let mut cs: Vec<Client> = codes.iter().enumerate().map(|(i, c)| Client::connect(srv.port, c, &format!("u{i}")).unwrap()).collect();
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

#[test]
fn voice_and_video_direct_lists_are_independent() {
    let srv = Server::start(8);
    let codes = srv.gen_codes(2);
    let mut a = Client::connect(srv.port, &codes[0], "alice").unwrap();
    let mut b = Client::connect(srv.port, &codes[1], "bob").unwrap();
    for x in [&mut a, &mut b] {
        x.enable_udp();
        x.send(ClientMsg::P2pEnable { local_addrs: vec![] });
        x.send(ClientMsg::JoinChannel { channel_id: 1 });
    }
    let (aid, bid) = (a.id, b.id);
    a.wait_for(|m| matches!(m, ServerMsg::Peers { peers } if peers.iter().any(|p| p.id == bid)));
    a.send(ClientMsg::ShareStart { desc: "1080p60".into() });
    b.wait_state(|u| u.iter().any(|x| x.id == aid && x.sharing.is_some()));
    b.send(ClientMsg::Watch { target: aid, on: true });
    a.wait_for(|m| matches!(m, ServerMsg::Watchers { ids } if ids == &vec![bid]));
    let frag = udp::video_fragments(0, udp::VIDEO_KEY, b"frame").remove(0);
    let sync = |a: &mut Client| {
        a.send(ClientMsg::Ping);
        a.wait_for(|m| matches!(m, ServerMsg::Pong));
    };

    // 画面直连、语音中转
    a.send(ClientMsg::DirectPeers { ids: vec![], video: vec![bid] });
    sync(&mut a);
    a.udp_send(udp::VIDEO, &frag);
    assert!(b.udp_recv().is_none(), "video must not be relayed to a video-direct peer");
    a.udp_send(udp::VOICE, &udp::client_voice(1, b"opus"));
    assert_eq!(b.udp_recv().map(|x| x.0), Some(udp::VOICE), "voice still relayed");

    // 反过来：语音直连、画面中转
    a.send(ClientMsg::DirectPeers { ids: vec![bid], video: vec![] });
    sync(&mut a);
    a.udp_send(udp::VOICE, &udp::client_voice(2, b"opus"));
    assert!(b.udp_recv().is_none(), "voice must not be relayed to a voice-direct peer");
    a.udp_send(udp::VIDEO, &frag);
    assert_eq!(b.udp_recv().map(|x| x.0), Some(udp::VIDEO), "video still relayed");

    // 旧版客户端不带 video 字段：画面照常中转
    let raw = r#"{"type":"direct_peers","ids":[]}"#;
    let old: ClientMsg = serde_json::from_str(raw).unwrap();
    assert_eq!(old, ClientMsg::DirectPeers { ids: vec![], video: vec![] });
}
