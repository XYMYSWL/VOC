//! VOC 传输层：ML-KEM-768 抗量子握手 + 授权码双向认证 + 全随机外观（抗 DPI）。
//!
//! # 设计目标
//! - **机密 / 完整**：ChaCha20-Poly1305，256-bit 密钥。
//! - **抗量子**：每次连接用临时 ML-KEM-768（FIPS 203）密钥交换，提供前向安全。
//! - **双向认证**：授权码派生的 psk 参与所有密钥派生；没有正确授权码的人（包括中间人）
//!   解不开、伪造不了，也认不出这是 VOC 流量。授权码秘密部分从不上网。
//! - **抗 DPI / 抗探测**：线路上**没有任何明文字段**——没有魔数、版本号、用户 ID、长度。
//!   ML-KEM 公钥/密文本身有统计特征，所以也整体加密。每个握手包、TCP 帧、UDP 包都带随机填充，
//!   长度不固定。服务器对无法认证的连接**不回任何字节**，读一段随机时间后再关闭，主动探测拿不到响应；
//!   握手带时间戳 + 盐值重放缓存，录下来的握手重放无效。
//!
//! # 握手（TCP 首包）
//! ```text
//! C -> S:  salt_c(32) | AEAD_kc(n=0, ts u64 | pad u16) | AEAD_kc(n=1, ek 1184) | 随机填充(pad)
//!          kc = HKDF(psk, salt=salt_c, "voc hs c")      服务器用每个授权码的 psk 逐个试解第一段
//! S -> C:  salt_s(32) | AEAD_ks(n=0, pad u16)          | AEAD_ks(n=1, ct 1088) | 随机填充(pad)
//!          ks = HKDF(psk, salt=salt_s|salt_c, "voc hs s")
//! 会话密钥 = HKDF(salt = SHA-256(双方首包全部字节), ikm = ML-KEM 共享秘密 | psk)
//! ```
//! # 之后
//! - TCP 帧：`AEAD(长度 u16)`（18 字节）| `AEAD(kind u8 | 数据长度 u32 | 数据 | 随机填充)`，各方向独立计数器做 nonce。
//! - UDP 包：`掩码后的计数器(8)` | `AEAD(kind u8 | 数据长度 u16 | 数据 | 随机填充)`。
//!   计数器用「头部保护」（类似 QUIC）：掩码 = SHA-256(hp_key | 认证标签前 12 字节) 的前 8 字节，
//!   看起来同样是随机数。接收端 64 包滑动窗口防重放。

use crate::auth::random_bytes;
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use hkdf::Hkdf;
use ml_kem::kem::{Decapsulate, Encapsulate, Kem, KeyExport};
use ml_kem::{ml_kem_768, MlKem768};
use sha2::{Digest, Sha256};

pub const SALT_LEN: usize = 32;
pub const TAG_LEN: usize = 16;
pub const EK_LEN: usize = 1184;
pub const CT_LEN: usize = 1088;
/// 握手随机填充上限
pub const MAX_HS_PAD: usize = 900;
/// 允许的客户端时钟偏差（秒）
pub const TS_WINDOW_SECS: u64 = 600;

/// 客户端首包前缀（服务器先读这么多来识别授权码）
pub const CLIENT_PREFIX_LEN: usize = SALT_LEN + 10 + TAG_LEN;
pub const CLIENT_BODY_LEN: usize = EK_LEN + TAG_LEN;
pub const SERVER_PREFIX_LEN: usize = SALT_LEN + 2 + TAG_LEN;
pub const SERVER_BODY_LEN: usize = CT_LEN + TAG_LEN;
/// TCP 帧长度头
pub const FRAME_LEN_LEN: usize = 2 + TAG_LEN;
pub const MAX_FRAME_DATA: usize = 60_000;

/// TCP 帧类型
pub const KIND_JSON: u8 = 0;
pub const KIND_VOICE: u8 = 1;
pub const KIND_VIDEO: u8 = 2;

#[derive(Clone)]
pub struct SessionKeys {
    pub tcp_c2s: [u8; 32],
    pub tcp_s2c: [u8; 32],
    pub udp_c2s: [u8; 32],
    pub udp_s2c: [u8; 32],
    pub udp_c2s_hp: [u8; 32],
    pub udp_s2c_hp: [u8; 32],
}

fn nonce(ctr: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&ctr.to_le_bytes());
    Nonce::from(n)
}

fn kdf(ikm: &[u8], salt: &[u8], info: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(Some(salt), ikm).expand(info, &mut out).expect("hkdf");
    out
}

fn aead(key: &[u8; 32]) -> ChaCha20Poly1305 {
    ChaCha20Poly1305::new(key.into())
}

fn rand_pad(max: usize) -> usize {
    let r = u32::from_le_bytes(random_bytes::<4>());
    (r as usize) % (max + 1)
}

fn random_vec(n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    getrandom::fill(&mut v).expect("rng");
    v
}

pub fn unix_secs() -> u64 {
    crate::now_millis() / 1000
}

fn session_keys(ss: &[u8], psk: &[u8; 32], client_flight: &[u8], server_flight: &[u8]) -> SessionKeys {
    let mut t = Sha256::new();
    t.update(b"voc-v3 transcript");
    t.update((client_flight.len() as u32).to_be_bytes());
    t.update(client_flight);
    t.update((server_flight.len() as u32).to_be_bytes());
    t.update(server_flight);
    let transcript = t.finalize();
    let mut ikm = Vec::with_capacity(ss.len() + 32);
    ikm.extend_from_slice(ss);
    ikm.extend_from_slice(psk);
    let hk = Hkdf::<Sha256>::new(Some(transcript.as_slice()), &ikm);
    let k = |label: &[u8]| {
        let mut out = [0u8; 32];
        hk.expand(label, &mut out).expect("hkdf");
        out
    };
    let keys = SessionKeys {
        tcp_c2s: k(b"voc tcp c2s"),
        tcp_s2c: k(b"voc tcp s2c"),
        udp_c2s: k(b"voc udp c2s"),
        udp_s2c: k(b"voc udp s2c"),
        udp_c2s_hp: k(b"voc udp c2s hp"),
        udp_s2c_hp: k(b"voc udp s2c hp"),
    };
    ikm.fill(0);
    keys
}

#[derive(Debug, PartialEq, Eq)]
pub enum HandshakeError {
    /// 解不开：授权码错误或对端不是 VOC 服务器
    BadAuth,
    /// 时间戳超出窗口（可能是重放，或本机时间不准）
    Stale,
    Malformed,
}

// ------------------------------------------------------------ 客户端

pub struct ClientHandshake {
    dk: ml_kem_768::DecapsulationKey,
    psk: [u8; 32],
    salt_c: [u8; SALT_LEN],
    flight: Vec<u8>,
    server_prefix: Vec<u8>,
    server_pad: usize,
}

impl ClientHandshake {
    pub fn start(psk: [u8; 32]) -> Self {
        Self::start_at(psk, unix_secs())
    }

    pub fn start_at(psk: [u8; 32], ts: u64) -> Self {
        let (dk, ek) = MlKem768::generate_keypair();
        let salt_c = random_bytes::<SALT_LEN>();
        let key = aead(&kdf(&psk, &salt_c, b"voc hs c"));
        let pad = rand_pad(MAX_HS_PAD);
        let mut hdr = [0u8; 10];
        hdr[..8].copy_from_slice(&ts.to_le_bytes());
        hdr[8..].copy_from_slice(&(pad as u16).to_le_bytes());
        let mut flight = Vec::with_capacity(CLIENT_PREFIX_LEN + CLIENT_BODY_LEN + pad);
        flight.extend_from_slice(&salt_c);
        flight.extend_from_slice(&key.encrypt(&nonce(0), &hdr[..]).expect("enc"));
        flight.extend_from_slice(&key.encrypt(&nonce(1), ek.to_bytes().as_slice()).expect("enc"));
        flight.extend_from_slice(&random_vec(pad));
        Self { dk, psk, salt_c, flight, server_prefix: Vec::new(), server_pad: 0 }
    }

    pub fn first_flight(&self) -> &[u8] {
        &self.flight
    }

    /// 读到服务器回复的前 SERVER_PREFIX_LEN 字节后调用；返回还要再读多少字节
    pub fn read_server_prefix(&mut self, prefix: &[u8]) -> Result<usize, HandshakeError> {
        if prefix.len() != SERVER_PREFIX_LEN {
            return Err(HandshakeError::Malformed);
        }
        let salt_s = &prefix[..SALT_LEN];
        let mut salt = salt_s.to_vec();
        salt.extend_from_slice(&self.salt_c);
        let key = aead(&kdf(&self.psk, &salt, b"voc hs s"));
        let hdr = key.decrypt(&nonce(0), &prefix[SALT_LEN..]).map_err(|_| HandshakeError::BadAuth)?;
        let pad = u16::from_le_bytes([hdr[0], hdr[1]]) as usize;
        if pad > MAX_HS_PAD {
            return Err(HandshakeError::Malformed);
        }
        self.server_prefix = prefix.to_vec();
        self.server_pad = pad;
        Ok(SERVER_BODY_LEN + pad)
    }

    pub fn finish(self, rest: &[u8]) -> Result<SessionKeys, HandshakeError> {
        if rest.len() != SERVER_BODY_LEN + self.server_pad {
            return Err(HandshakeError::Malformed);
        }
        let mut salt = self.server_prefix[..SALT_LEN].to_vec();
        salt.extend_from_slice(&self.salt_c);
        let key = aead(&kdf(&self.psk, &salt, b"voc hs s"));
        let ct = key.decrypt(&nonce(1), &rest[..SERVER_BODY_LEN]).map_err(|_| HandshakeError::BadAuth)?;
        let ss = self.dk.decapsulate_slice(&ct).map_err(|_| HandshakeError::Malformed)?;
        let mut server_flight = self.server_prefix.clone();
        server_flight.extend_from_slice(rest);
        Ok(session_keys(ss.as_slice(), &self.psk, &self.flight, &server_flight))
    }
}

// ------------------------------------------------------------ 服务器

pub struct ClientPrefix {
    /// psks 中匹配的下标
    pub index: usize,
    pub salt: [u8; SALT_LEN],
    pub pad: usize,
    key: [u8; 32],
    raw: Vec<u8>,
}

/// 用所有候选 psk 试解客户端前缀。成功返回匹配的授权码下标。
pub fn server_identify(prefix: &[u8], psks: &[[u8; 32]], now: u64) -> Result<ClientPrefix, HandshakeError> {
    if prefix.len() != CLIENT_PREFIX_LEN {
        return Err(HandshakeError::Malformed);
    }
    let salt: [u8; SALT_LEN] = prefix[..SALT_LEN].try_into().unwrap();
    for (index, psk) in psks.iter().enumerate() {
        let key = kdf(psk, &salt, b"voc hs c");
        if let Ok(hdr) = aead(&key).decrypt(&nonce(0), &prefix[SALT_LEN..]) {
            let ts = u64::from_le_bytes(hdr[..8].try_into().unwrap());
            if ts.abs_diff(now) > TS_WINDOW_SECS {
                return Err(HandshakeError::Stale);
            }
            let pad = u16::from_le_bytes([hdr[8], hdr[9]]) as usize;
            if pad > MAX_HS_PAD {
                return Err(HandshakeError::Malformed);
            }
            return Ok(ClientPrefix { index, salt, pad, key, raw: prefix.to_vec() });
        }
    }
    Err(HandshakeError::BadAuth)
}

impl ClientPrefix {
    /// 前缀之后还要读的字节数
    pub fn remaining(&self) -> usize {
        CLIENT_BODY_LEN + self.pad
    }

    /// 读完剩余部分后生成服务器回复与会话密钥
    pub fn respond(self, rest: &[u8], psk: &[u8; 32]) -> Result<(Vec<u8>, SessionKeys), HandshakeError> {
        if rest.len() != self.remaining() {
            return Err(HandshakeError::Malformed);
        }
        let ek_bytes = aead(&self.key).decrypt(&nonce(1), &rest[..CLIENT_BODY_LEN]).map_err(|_| HandshakeError::BadAuth)?;
        let ek_arr = ml_kem::kem::Key::<ml_kem_768::EncapsulationKey>::try_from(ek_bytes.as_slice())
            .map_err(|_| HandshakeError::Malformed)?;
        let ek = ml_kem_768::EncapsulationKey::new(&ek_arr).map_err(|_| HandshakeError::Malformed)?;
        let (ct, ss) = ek.encapsulate();

        let salt_s = random_bytes::<SALT_LEN>();
        let mut salt = salt_s.to_vec();
        salt.extend_from_slice(&self.salt);
        let key = aead(&kdf(psk, &salt, b"voc hs s"));
        let pad = rand_pad(MAX_HS_PAD);
        let mut reply = Vec::with_capacity(SERVER_PREFIX_LEN + SERVER_BODY_LEN + pad);
        reply.extend_from_slice(&salt_s);
        reply.extend_from_slice(&key.encrypt(&nonce(0), &(pad as u16).to_le_bytes()[..]).expect("enc"));
        reply.extend_from_slice(&key.encrypt(&nonce(1), ct.as_slice()).expect("enc"));
        reply.extend_from_slice(&random_vec(pad));

        let mut client_flight = self.raw;
        client_flight.extend_from_slice(rest);
        let keys = session_keys(ss.as_slice(), psk, &client_flight, &reply);
        Ok((reply, keys))
    }
}

/// 握手盐值重放缓存（服务器用）
#[derive(Default)]
pub struct ReplayCache {
    seen: std::collections::HashMap<[u8; SALT_LEN], u64>,
}

impl ReplayCache {
    /// 第一次见到返回 true
    pub fn insert(&mut self, salt: [u8; SALT_LEN], now: u64) -> bool {
        if self.seen.len() > 10_000 {
            self.seen.retain(|_, t| now.saturating_sub(*t) <= TS_WINDOW_SECS * 2);
        }
        self.seen.insert(salt, now).is_none()
    }
}

// ------------------------------------------------------------ TCP 帧

pub struct FrameSealer {
    aead: ChaCha20Poly1305,
    ctr: u64,
}

impl FrameSealer {
    pub fn new(key: &[u8; 32]) -> Self {
        Self { aead: aead(key), ctr: 0 }
    }

    pub fn seal(&mut self, kind: u8, data: &[u8]) -> Vec<u8> {
        assert!(data.len() <= MAX_FRAME_DATA, "frame too large");
        let pad = rand_pad(if data.len() < 256 { 64 } else { 16 });
        let mut pt = Vec::with_capacity(5 + data.len() + pad);
        pt.push(kind);
        pt.extend_from_slice(&(data.len() as u32).to_le_bytes());
        pt.extend_from_slice(data);
        pt.extend_from_slice(&random_vec(pad));
        let body = self.aead.encrypt(&nonce(self.ctr + 1), pt.as_slice()).expect("enc");
        let len = self.aead.encrypt(&nonce(self.ctr), &(body.len() as u16).to_le_bytes()[..]).expect("enc");
        self.ctr += 2;
        let mut out = len;
        out.extend_from_slice(&body);
        out
    }
}

pub struct FrameOpener {
    aead: ChaCha20Poly1305,
    ctr: u64,
}

impl FrameOpener {
    pub fn new(key: &[u8; 32]) -> Self {
        Self { aead: aead(key), ctr: 0 }
    }

    /// 解开 18 字节长度头，返回帧体长度
    pub fn open_len(&mut self, hdr: &[u8]) -> Option<usize> {
        let pt = self.aead.decrypt(&nonce(self.ctr), hdr).ok()?;
        let n = u16::from_le_bytes([pt[0], pt[1]]) as usize;
        (n >= TAG_LEN + 5).then_some(n)
    }

    pub fn open_body(&mut self, body: &[u8]) -> Option<(u8, Vec<u8>)> {
        let pt = self.aead.decrypt(&nonce(self.ctr + 1), body).ok()?;
        self.ctr += 2;
        let kind = pt[0];
        let n = u32::from_le_bytes(pt[1..5].try_into().ok()?) as usize;
        let data = pt.get(5..5 + n)?.to_vec();
        Some((kind, data))
    }
}

// ------------------------------------------------------------ UDP 包

fn hp_mask(hp: &[u8; 32], sample: &[u8]) -> u64 {
    let mut h = Sha256::new();
    h.update(hp);
    h.update(sample);
    u64::from_le_bytes(h.finalize()[..8].try_into().unwrap())
}

pub struct DatagramSealer {
    aead: ChaCha20Poly1305,
    hp: [u8; 32],
    ctr: u64,
}

impl DatagramSealer {
    pub fn new(key: &[u8; 32], hp: &[u8; 32]) -> Self {
        Self { aead: aead(key), hp: *hp, ctr: 0 }
    }

    pub fn seal(&mut self, kind: u8, data: &[u8], out: &mut Vec<u8>) {
        let ctr = self.ctr;
        self.ctr += 1;
        let pad = rand_pad(24);
        let mut pt = Vec::with_capacity(3 + data.len() + pad);
        pt.push(kind);
        pt.extend_from_slice(&(data.len() as u16).to_le_bytes());
        pt.extend_from_slice(data);
        pt.extend_from_slice(&random_vec(pad));
        let ct = self.aead.encrypt(&nonce(ctr), pt.as_slice()).expect("enc");
        let tag = &ct[ct.len() - TAG_LEN..];
        let masked = ctr ^ hp_mask(&self.hp, &tag[..12]);
        out.clear();
        out.extend_from_slice(&masked.to_le_bytes());
        out.extend_from_slice(&ct);
    }
}

pub struct DatagramOpener {
    aead: ChaCha20Poly1305,
    hp: [u8; 32],
    max: Option<u64>,
    window: u64,
}

impl DatagramOpener {
    pub fn new(key: &[u8; 32], hp: &[u8; 32]) -> Self {
        Self { aead: aead(key), hp: *hp, max: None, window: 0 }
    }

    fn fresh(&self, ctr: u64) -> bool {
        match self.max {
            None => true,
            Some(m) if ctr > m => true,
            Some(m) => {
                let back = m - ctr;
                back < 64 && self.window & (1u64 << back) == 0
            }
        }
    }

    fn mark(&mut self, ctr: u64) {
        match self.max {
            None => {
                self.max = Some(ctr);
                self.window = 1;
            }
            Some(m) if ctr > m => {
                let shift = ctr - m;
                self.window = if shift >= 64 { 0 } else { self.window << shift } | 1;
                self.max = Some(ctr);
            }
            Some(m) => self.window |= 1u64 << (m - ctr),
        }
    }

    /// 成功返回 (kind, data)
    pub fn open(&mut self, pkt: &[u8]) -> Option<(u8, Vec<u8>)> {
        if pkt.len() < 8 + 3 + TAG_LEN {
            return None;
        }
        let tag = &pkt[pkt.len() - TAG_LEN..];
        let ctr = u64::from_le_bytes(pkt[..8].try_into().ok()?) ^ hp_mask(&self.hp, &tag[..12]);
        if !self.fresh(ctr) {
            return None;
        }
        let pt = self.aead.decrypt(&nonce(ctr), Payload { msg: &pkt[8..], aad: &[] }).ok()?;
        self.mark(ctr);
        let n = u16::from_le_bytes([pt[1], pt[2]]) as usize;
        Some((pt[0], pt.get(3..3 + n)?.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthCode;

    fn handshake(client_psk: [u8; 32], server_psks: &[[u8; 32]]) -> Result<(SessionKeys, SessionKeys), HandshakeError> {
        let mut c = ClientHandshake::start(client_psk);
        let flight = c.first_flight().to_vec();
        let p = server_identify(&flight[..CLIENT_PREFIX_LEN], server_psks, unix_secs())?;
        let idx = p.index;
        assert_eq!(flight.len(), CLIENT_PREFIX_LEN + p.remaining());
        let (reply, sk) = p.respond(&flight[CLIENT_PREFIX_LEN..], &server_psks[idx])?;
        let need = c.read_server_prefix(&reply[..SERVER_PREFIX_LEN])?;
        assert_eq!(reply.len(), SERVER_PREFIX_LEN + need);
        let ck = c.finish(&reply[SERVER_PREFIX_LEN..])?;
        Ok((ck, sk))
    }

    #[test]
    fn handshake_multi_user() {
        let codes: Vec<_> = (0..20).map(|_| AuthCode::generate().psk()).collect();
        let (ck, sk) = handshake(codes[13], &codes).unwrap();
        assert_eq!(ck.tcp_c2s, sk.tcp_c2s);
        assert_eq!(ck.udp_s2c_hp, sk.udp_s2c_hp);
        assert_ne!(ck.tcp_c2s, ck.tcp_s2c);
    }

    #[test]
    fn wrong_code_not_identified() {
        let codes: Vec<_> = (0..5).map(|_| AuthCode::generate().psk()).collect();
        let stranger = AuthCode::generate().psk();
        assert_eq!(handshake(stranger, &codes).err(), Some(HandshakeError::BadAuth));
    }

    #[test]
    fn stale_timestamp_rejected() {
        let psk = AuthCode::generate().psk();
        let c = ClientHandshake::start_at(psk, unix_secs() - 3600);
        let r = server_identify(&c.first_flight()[..CLIENT_PREFIX_LEN], &[psk], unix_secs());
        assert_eq!(r.err(), Some(HandshakeError::Stale));
    }

    #[test]
    fn flights_have_variable_length_and_no_structure() {
        let psk = AuthCode::generate().psk();
        let lens: std::collections::HashSet<usize> =
            (0..10).map(|_| ClientHandshake::start(psk).first_flight().len()).collect();
        assert!(lens.len() > 3, "padding should vary length");
        // 两次握手首包任何位置都不应有固定字节（无魔数/版本号）
        let a = ClientHandshake::start(psk);
        let b = ClientHandshake::start(psk);
        let same = a.first_flight().iter().zip(b.first_flight()).take(64).filter(|(x, y)| x == y).count();
        assert!(same < 6, "too many identical leading bytes: {same}");
    }

    #[test]
    fn frames_roundtrip() {
        let k = [3u8; 32];
        let mut s = FrameSealer::new(&k);
        let mut o = FrameOpener::new(&k);
        for (kind, msg) in [(KIND_JSON, &b"{\"a\":1}"[..]), (KIND_VOICE, &[9u8; 170][..])] {
            let f = s.seal(kind, msg);
            let n = o.open_len(&f[..FRAME_LEN_LEN]).unwrap();
            assert_eq!(f.len(), FRAME_LEN_LEN + n);
            assert_eq!(o.open_body(&f[FRAME_LEN_LEN..]).unwrap(), (kind, msg.to_vec()));
        }
    }

    #[test]
    fn datagram_replay_tamper_and_masking() {
        let (k, hp) = ([7u8; 32], [8u8; 32]);
        let mut tx = DatagramSealer::new(&k, &hp);
        let mut rx = DatagramOpener::new(&k, &hp);
        let (mut p1, mut p2) = (Vec::new(), Vec::new());
        tx.seal(1, b"one", &mut p1);
        tx.seal(1, b"two", &mut p2);
        // 计数器 0、1 被掩码后不应以明文出现
        assert_ne!(&p1[..8], &0u64.to_le_bytes());
        assert_ne!(&p2[..8], &1u64.to_le_bytes());
        assert_eq!(rx.open(&p2).unwrap(), (1, b"two".to_vec()));
        assert_eq!(rx.open(&p1).unwrap(), (1, b"one".to_vec())); // 乱序可接受
        assert!(rx.open(&p1).is_none()); // 重放
        let mut bad = p2.clone();
        bad[9] ^= 1;
        assert!(rx.open(&bad).is_none()); // 篡改
        // 错误密钥打不开（服务器靠这个在多个用户间试解）
        assert!(DatagramOpener::new(&[1; 32], &hp).open(&p2).is_none());
    }
}
