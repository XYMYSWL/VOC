//! P2P 直连语音。
//!
//! # 流程
//! 1. 每两个在线用户之间先建立成对密钥（见 [`crate::e2e`]：ML-KEM-768 封装 + ML-DSA-65 签名，服务器算不出）。
//! 2. 开启 P2P 的客户端把本机局域网 / IPv6 地址交给服务器（`ClientMsg::P2pEnable`）。服务器把**同频道、同样开启了 P2P**
//!    的成员地址发给每个人（`ServerMsg::Peers`）：服务器观察到的公网 UDP 地址 + 对方自报的本地地址。
//!    不开 P2P 的人的地址不会给任何人。
//! 3. 双方同时向对方所有候选地址发 PUNCH（UDP 打洞），收到 PUNCH_ACK 即确认双向连通，
//!    之后语音直接互发，并告诉服务器（`DirectPeers`）不要再给对方转发自己的语音。
//! 4. 每秒 P2P_PING 保活；4 秒没收到对方任何包就判定断开，自动回到服务器中转。
//! 5. 直连要给每个人各发一份，所以客户端按「上行带宽预算」决定哪些人直连：语音优先，剩下的给屏幕共享，
//!    超出预算的人走服务器中转（只需多传一份给服务器）。
//!
//! 直连包的外层格式与客户端↔服务器的 UDP 包相同（[`crate::crypto::DatagramSealer`]），线路上同样是全随机字节。

use ml_kem::kem::{Decapsulate, Encapsulate, Kem, KeyExport};
use ml_kem::{ml_kem_768, MlKem768};
use sha2::{Digest, Sha256};

pub const PUNCH: u8 = 10;
pub const PUNCH_ACK: u8 = 11;
pub const PING: u8 = 12;
pub const PONG: u8 = 13;

pub struct PairKeys {
    pub send: [u8; 32],
    pub send_hp: [u8; 32],
    pub recv: [u8; 32],
    pub recv_hp: [u8; 32],
    /// 经服务器转交的加密信封（双向共用，信封带随机 nonce）
    pub blob: [u8; 32],
}

pub struct LocalKem {
    dk: ml_kem_768::DecapsulationKey,
    pub ek: Vec<u8>,
}

impl Default for LocalKem {
    fn default() -> Self {
        let (dk, ek) = MlKem768::generate_keypair();
        Self { dk, ek: ek.to_bytes().to_vec() }
    }
}

impl LocalKem {
    pub fn decapsulate(&self, ct: &[u8]) -> Option<Vec<u8>> {
        self.dk.decapsulate_slice(ct).ok().map(|k| k.to_vec())
    }
}

/// 对对方的公钥封装，返回 (密文, 共享秘密)
pub fn encapsulate(peer_ek: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let arr = ml_kem::kem::Key::<ml_kem_768::EncapsulationKey>::try_from(peer_ek).ok()?;
    let ek = ml_kem_768::EncapsulationKey::new(&arr).ok()?;
    let (ct, ss) = ek.encapsulate();
    Some((ct.to_vec(), ss.to_vec()))
}

/// 双方得到相同的成对密钥（方向相反）
pub fn pair_keys(ss: &[u8], my_id: u32, my_ek: &[u8], peer_id: u32, peer_ek: &[u8]) -> PairKeys {
    let (lo, hi, ek_lo, ek_hi) =
        if my_id < peer_id { (my_id, peer_id, my_ek, peer_ek) } else { (peer_id, my_id, peer_ek, my_ek) };
    let mut s = Sha256::new();
    s.update(b"voc p2p v1");
    s.update(lo.to_le_bytes());
    s.update(hi.to_le_bytes());
    s.update(ek_lo);
    s.update(ek_hi);
    let salt = s.finalize();
    let hk = hkdf::Hkdf::<Sha256>::new(Some(salt.as_slice()), ss);
    let k = |label: &[u8]| {
        let mut out = [0u8; 32];
        hk.expand(label, &mut out).expect("hkdf");
        out
    };
    let (l2h, l2h_hp, h2l, h2l_hp) = (k(b"lo->hi"), k(b"lo->hi hp"), k(b"hi->lo"), k(b"hi->lo hp"));
    let blob = k(b"blob");
    if my_id < peer_id {
        PairKeys { send: l2h, send_hp: l2h_hp, recv: h2l, recv_hp: h2l_hp, blob }
    } else {
        PairKeys { send: h2l, send_hp: h2l_hp, recv: l2h, recv_hp: l2h_hp, blob }
    }
}

pub fn hex(b: &[u8]) -> String {
    crate::auth::to_hex(b)
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || s.len() > 8192 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{DatagramOpener, DatagramSealer};

    #[test]
    fn peers_agree_on_pair_keys() {
        let (a, b) = (LocalKem::default(), LocalKem::default());
        // id 小的一方（3）封装给 id 大的一方（9）
        let (ct, ss_a) = encapsulate(&b.ek).unwrap();
        let ss_b = b.decapsulate(&unhex(&hex(&ct)).unwrap()).unwrap();
        assert_eq!(ss_a, ss_b);
        let ka = pair_keys(&ss_a, 3, &a.ek, 9, &b.ek);
        let kb = pair_keys(&ss_b, 9, &b.ek, 3, &a.ek);
        assert_eq!(ka.send, kb.recv);
        assert_eq!(ka.recv_hp, kb.send_hp);
        assert_ne!(ka.send, ka.recv);

        let mut tx = DatagramSealer::new(&ka.send, &ka.send_hp);
        let mut rx = DatagramOpener::new(&kb.recv, &kb.recv_hp);
        let mut p = Vec::new();
        tx.seal(PUNCH, b"", &mut p);
        assert_eq!(rx.open(&p), Some((PUNCH, vec![])));
    }

    #[test]
    fn bad_input_rejected() {
        assert!(encapsulate(&[1, 2, 3]).is_none());
        assert!(unhex("zz").is_none());
        assert!(unhex("abc").is_none());
    }
}
