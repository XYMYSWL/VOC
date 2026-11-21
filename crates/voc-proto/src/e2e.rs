//! 端到端加密（服务器只转发，看不到语音和聊天内容）。
//!
//! # 身份
//! 每个客户端有一个长期的 **ML-DSA-65**（FIPS 204，抗量子签名）身份密钥，保存在本机。
//! 登录后公布：身份公钥 idk、本次会话的 ML-KEM-768 公钥 ek、签名 Sign(idk, 昵称 | ek)。
//! 客户端首次见到某人时记住其身份（TOFU），以后身份变了会警告并暂停加密通信；
//! 双方也可以对照「安全码」（身份公钥的指纹）手动核验，排除服务器做中间人的可能。
//!
//! # 成对密钥
//! 每两个在线用户之间：id 小的一方对另一方的 ek 封装，并对 (对方 ek | 密文) 签名，经服务器转交；
//! 对方验签后解封装。双方用共享秘密派生成对密钥（[`crate::p2p::pair_keys`]）：
//! 用于 P2P 直连包，以及经服务器转交的加密信封（[`seal_blob`]）。
//!
//! # 语音：发送者密钥
//! 每人有一把自己的语音密钥（带编号 epoch），通过加密信封发给同频道每个人。
//! 每帧 Opus 用它加密一次，所有接收者共用同一份密文，所以服务器中转时也只是转发密文。
//! 频道成员一变就换新密钥：离开的人听不到以后的语音，新来的人听不到以前的语音。
//!
//! # 聊天
//! 每条消息对每个接收者单独用成对密钥加密（聊天量小，这样最简单也最安全）。

use base64::Engine;
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use ml_dsa::signature::{Keypair, SignatureEncoding, Signer, Verifier};
use ml_dsa::{KeyExport, MlDsa65, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub const IDK_LEN: usize = 1952;
pub const SIG_LEN: usize = 3309;
pub const EK_LEN: usize = 1184;
pub const CT_LEN: usize = 1088;

/// 加密信封里的内容类型
pub const BLOB_SENDER_KEY: u8 = 1;
pub const BLOB_CHAT: u8 = 2;
/// 图片密钥和信息（file::ImageMeta 的 JSON）
pub const BLOB_IMAGE: u8 = 3;

const CTX_EK: &[u8] = b"voc e2e ek v1";
const CTX_PAIR: &[u8] = b"voc e2e pair v1";

pub fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(b)
}

pub fn unb64(s: &str) -> Option<Vec<u8>> {
    if s.len() > 64 * 1024 {
        return None;
    }
    base64::engine::general_purpose::STANDARD_NO_PAD.decode(s).ok()
}

// ------------------------------------------------------------ 身份

pub struct Identity {
    sk: SigningKey<MlDsa65>,
    pub public: Vec<u8>,
}

impl Identity {
    pub fn generate() -> Self {
        Self::from_seed(&crate::auth::random_bytes::<32>())
    }

    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let sk = SigningKey::<MlDsa65>::from_seed(&(*seed).into());
        let public = sk.verifying_key().encode().to_vec();
        Self { sk, public }
    }

    /// 32 字节种子（保存在本机配置里）
    pub fn seed(&self) -> [u8; 32] {
        let s = self.sk.to_bytes();
        s.as_slice().try_into().expect("seed is 32 bytes")
    }

    fn sign(&self, ctx: &[u8], msg: &[u8]) -> Vec<u8> {
        let mut m = ctx.to_vec();
        m.push(0);
        m.extend_from_slice(msg);
        self.sk.sign(&m).to_bytes().to_vec()
    }

    /// 对「昵称 | 会话公钥」签名
    pub fn sign_ek(&self, name: &str, ek: &[u8]) -> Vec<u8> {
        self.sign(CTX_EK, &name_ek_msg(name, ek))
    }

    /// 对「对方会话公钥 | 封装密文」签名
    pub fn sign_pair(&self, peer_ek: &[u8], ct: &[u8]) -> Vec<u8> {
        let mut m = peer_ek.to_vec();
        m.extend_from_slice(ct);
        self.sign(CTX_PAIR, &m)
    }
}

fn name_ek_msg(name: &str, ek: &[u8]) -> Vec<u8> {
    let mut m = (name.len() as u32).to_le_bytes().to_vec();
    m.extend_from_slice(name.as_bytes());
    m.extend_from_slice(ek);
    m
}

fn verify(idk: &[u8], ctx: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    let Ok(arr) = ml_dsa::EncodedVerifyingKey::<MlDsa65>::try_from(idk) else { return false };
    let vk = VerifyingKey::<MlDsa65>::decode(&arr);
    let Ok(sig) = ml_dsa::Signature::<MlDsa65>::try_from(sig) else { return false };
    let mut m = ctx.to_vec();
    m.push(0);
    m.extend_from_slice(msg);
    vk.verify(&m, &sig).is_ok()
}

pub fn verify_ek(idk: &[u8], name: &str, ek: &[u8], sig: &[u8]) -> bool {
    ek.len() == EK_LEN && verify(idk, CTX_EK, &name_ek_msg(name, ek), sig)
}

pub fn verify_pair(idk: &[u8], my_ek: &[u8], ct: &[u8], sig: &[u8]) -> bool {
    let mut m = my_ek.to_vec();
    m.extend_from_slice(ct);
    ct.len() == CT_LEN && verify(idk, CTX_PAIR, &m, sig)
}

/// 身份指纹（用于记住身份）
pub fn fingerprint(idk: &[u8]) -> String {
    let h = Sha256::digest([b"voc idk fp\0".as_slice(), idk].concat());
    crate::auth::to_hex(&h[..16])
}

/// 安全码：30 位数字，分 6 组。双方当面/电话对一下，一致就说明没有中间人。
pub fn safety_code(idk: &[u8]) -> String {
    let h = Sha256::digest([b"voc safety code\0".as_slice(), idk].concat());
    (0..6)
        .map(|i| {
            let v = u32::from_le_bytes(h[i * 4..i * 4 + 4].try_into().unwrap()) % 100_000;
            format!("{v:05}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ------------------------------------------------------------ 成对信封

/// 用成对密钥加密一个信封（随机 nonce）
pub fn seal_blob(key: &[u8; 32], kind: u8, data: &[u8]) -> Vec<u8> {
    let nonce = crate::auth::random_bytes::<12>();
    let mut pt = vec![kind];
    pt.extend_from_slice(data);
    let ct = ChaCha20Poly1305::new(key.into()).encrypt(&Nonce::from(nonce), pt.as_slice()).expect("enc");
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    out
}

pub fn open_blob(key: &[u8; 32], blob: &[u8]) -> Option<(u8, Vec<u8>)> {
    if blob.len() < 12 + 16 + 1 {
        return None;
    }
    let nonce: [u8; 12] = blob[..12].try_into().ok()?;
    let pt = ChaCha20Poly1305::new(key.into()).decrypt(&Nonce::from(nonce), &blob[12..]).ok()?;
    Some((pt[0], pt[1..].to_vec()))
}

// ------------------------------------------------------------ 语音发送者密钥

fn frame_nonce(epoch: u16, ctr: u32) -> Nonce {
    let mut n = [0u8; 12];
    n[..2].copy_from_slice(&epoch.to_le_bytes());
    n[2..6].copy_from_slice(&ctr.to_le_bytes());
    Nonce::from(n)
}

/// 视频用的密钥由语音密钥派生（独立的密钥和计数器，和语音互不影响）
fn video_key(key: &[u8; 32]) -> ChaCha20Poly1305 {
    let mut out = [0u8; 32];
    hkdf::Hkdf::<Sha256>::new(None, key).expand(b"voc video v1", &mut out).expect("hkdf");
    ChaCha20Poly1305::new((&out).into())
}

fn video_aad(sender_id: u32) -> [u8; 5] {
    let mut a = [b'V'; 5];
    a[..4].copy_from_slice(&sender_id.to_le_bytes());
    a
}

/// 自己的语音 / 视频密钥
pub struct SenderKey {
    pub epoch: u16,
    pub key: [u8; 32],
    ctr: u32,
    aead: ChaCha20Poly1305,
    video_ctr: u32,
    video_aead: ChaCha20Poly1305,
}

impl SenderKey {
    pub fn new(epoch: u16) -> Self {
        let key = crate::auth::random_bytes::<32>();
        Self {
            epoch,
            key,
            ctr: 0,
            aead: ChaCha20Poly1305::new((&key).into()),
            video_ctr: 0,
            video_aead: video_key(&key),
        }
    }

    /// 加密一整帧视频（加密后再分片）：epoch(2) | ctr(4) | AEAD
    pub fn encrypt_video(&mut self, sender_id: u32, frame: &[u8]) -> Vec<u8> {
        let ctr = self.video_ctr;
        self.video_ctr = self.video_ctr.wrapping_add(1);
        let ct = self
            .video_aead
            .encrypt(&frame_nonce(self.epoch, ctr), Payload { msg: frame, aad: &video_aad(sender_id) })
            .expect("enc");
        let mut out = self.epoch.to_le_bytes().to_vec();
        out.extend_from_slice(&ctr.to_le_bytes());
        out.extend_from_slice(&ct);
        out
    }

    /// 发给别人的密钥内容：epoch | key
    pub fn export(&self) -> Vec<u8> {
        let mut v = self.epoch.to_le_bytes().to_vec();
        v.extend_from_slice(&self.key);
        v
    }

    /// 加密一帧：epoch(2) | ctr(4) | AEAD(帧, aad = 发送者 id)
    pub fn encrypt(&mut self, sender_id: u32, frame: &[u8]) -> Vec<u8> {
        let ctr = self.ctr;
        self.ctr = self.ctr.wrapping_add(1);
        let ct = self
            .aead
            .encrypt(&frame_nonce(self.epoch, ctr), Payload { msg: frame, aad: &sender_id.to_le_bytes() })
            .expect("enc");
        let mut out = self.epoch.to_le_bytes().to_vec();
        out.extend_from_slice(&ctr.to_le_bytes());
        out.extend_from_slice(&ct);
        out
    }

    /// 计数器快用完时应换密钥（20 ms 一帧，2^31 帧约 1.4 年，实际不会触发）
    pub fn exhausted(&self) -> bool {
        self.ctr > u32::MAX / 2
    }
}

struct EpochKey {
    aead: ChaCha20Poly1305,
    max: Option<u32>,
    window: u64,
    video_aead: ChaCha20Poly1305,
    video_max: Option<u32>,
}

/// 某个发送者的语音密钥（保留最近几个 epoch，换密钥时在途的包也能解）
#[derive(Default)]
pub struct ReceiverKeys {
    epochs: Vec<(u16, EpochKey)>,
}

impl ReceiverKeys {
    pub fn install(&mut self, exported: &[u8]) -> bool {
        if exported.len() != 34 {
            return false;
        }
        let epoch = u16::from_le_bytes([exported[0], exported[1]]);
        let key: [u8; 32] = exported[2..].try_into().unwrap();
        self.epochs.retain(|(e, _)| *e != epoch);
        self.epochs.push((
            epoch,
            EpochKey {
                aead: ChaCha20Poly1305::new((&key).into()),
                max: None,
                window: 0,
                video_aead: video_key(&key),
                video_max: None,
            },
        ));
        if self.epochs.len() > 3 {
            self.epochs.remove(0);
        }
        true
    }

    pub fn has_any(&self) -> bool {
        !self.epochs.is_empty()
    }

    /// 解密一帧，返回 (帧计数器, Opus 数据)。重放/篡改/未知密钥返回 None。
    pub fn decrypt(&mut self, sender_id: u32, blob: &[u8]) -> Option<(u32, Vec<u8>)> {
        if blob.len() < 6 + 16 {
            return None;
        }
        let epoch = u16::from_le_bytes([blob[0], blob[1]]);
        let ctr = u32::from_le_bytes(blob[2..6].try_into().ok()?);
        let (_, k) = self.epochs.iter_mut().find(|(e, _)| *e == epoch)?;
        // 防重放（恶意服务器可能重放旧包）
        if let Some(m) = k.max {
            if ctr <= m && (m - ctr >= 64 || k.window & (1u64 << (m - ctr)) != 0) {
                return None;
            }
        }
        let pt = k
            .aead
            .decrypt(&frame_nonce(epoch, ctr), Payload { msg: &blob[6..], aad: &sender_id.to_le_bytes() })
            .ok()?;
        match k.max {
            Some(m) if ctr <= m => k.window |= 1u64 << (m - ctr),
            Some(m) => {
                let shift = ctr - m;
                k.window = if shift >= 64 { 0 } else { k.window << shift } | 1;
                k.max = Some(ctr);
            }
            None => {
                k.max = Some(ctr);
                k.window = 1;
            }
        }
        Some((ctr, pt))
    }
}

impl ReceiverKeys {
    /// 解密一整帧视频。帧计数器必须递增（旧帧 / 重放直接丢弃——视频只要最新的）。
    pub fn decrypt_video(&mut self, sender_id: u32, blob: &[u8]) -> Option<Vec<u8>> {
        if blob.len() < 6 + 16 {
            return None;
        }
        let epoch = u16::from_le_bytes([blob[0], blob[1]]);
        let ctr = u32::from_le_bytes(blob[2..6].try_into().ok()?);
        let (_, k) = self.epochs.iter_mut().find(|(e, _)| *e == epoch)?;
        if k.video_max.is_some_and(|m| ctr <= m) {
            return None;
        }
        let pt = k
            .video_aead
            .decrypt(&frame_nonce(epoch, ctr), Payload { msg: &blob[6..], aad: &video_aad(sender_id) })
            .ok()?;
        k.video_max = Some(ctr);
        Some(pt)
    }
}

/// 所有发送者的语音密钥
#[derive(Default)]
pub struct ReceiverKeyring {
    pub by_sender: HashMap<u32, ReceiverKeys>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_signatures() {
        let id = Identity::generate();
        assert_eq!(id.public.len(), IDK_LEN);
        let again = Identity::from_seed(&id.seed());
        assert_eq!(again.public, id.public);

        let ek = vec![7u8; EK_LEN];
        let sig = id.sign_ek("小明", &ek);
        assert_eq!(sig.len(), SIG_LEN);
        assert!(verify_ek(&id.public, "小明", &ek, &sig));
        // 服务器改名、换公钥、换身份都会验签失败
        assert!(!verify_ek(&id.public, "小红", &ek, &sig));
        assert!(!verify_ek(&id.public, "小明", &vec![8u8; EK_LEN], &sig));
        assert!(!verify_ek(&Identity::generate().public, "小明", &ek, &sig));

        let ct = vec![1u8; CT_LEN];
        let ps = id.sign_pair(&ek, &ct);
        assert!(verify_pair(&id.public, &ek, &ct, &ps));
        assert!(!verify_pair(&id.public, &vec![0u8; EK_LEN], &ct, &ps));
    }

    #[test]
    fn safety_codes() {
        let a = Identity::generate();
        let c = safety_code(&a.public);
        assert_eq!(c.len(), 35);
        assert_eq!(c, safety_code(&a.public));
        assert_ne!(c, safety_code(&Identity::generate().public));
    }

    #[test]
    fn blobs() {
        let k = [9u8; 32];
        let b = seal_blob(&k, BLOB_CHAT, "你好".as_bytes());
        assert_eq!(open_blob(&k, &b), Some((BLOB_CHAT, "你好".as_bytes().to_vec())));
        assert!(open_blob(&[8u8; 32], &b).is_none());
    }

    #[test]
    fn sender_key_frames() {
        let mut sk = SenderKey::new(3);
        let mut rk = ReceiverKeys::default();
        assert!(rk.install(&sk.export()));
        let f0 = sk.encrypt(42, b"opus-0");
        let f1 = sk.encrypt(42, b"opus-1");
        assert!(!f0.windows(6).any(|w| w == b"opus-0"), "frame must be encrypted");
        assert_eq!(rk.decrypt(42, &f1), Some((1, b"opus-1".to_vec())));
        assert_eq!(rk.decrypt(42, &f0), Some((0, b"opus-0".to_vec()))); // 乱序
        assert!(rk.decrypt(42, &f0).is_none()); // 重放
        assert!(rk.decrypt(43, &sk.encrypt(42, b"x")).is_none()); // 冒充别的发送者
        // 换密钥后旧 epoch 仍可解在途包，新 epoch 也能解
        let mut sk2 = SenderKey::new(4);
        rk.install(&sk2.export());
        assert!(rk.decrypt(42, &sk.encrypt(42, b"late")).is_some());
        assert!(rk.decrypt(42, &sk2.encrypt(42, b"new")).is_some());
        // 没有密钥的人解不开
        assert!(ReceiverKeys::default().decrypt(42, &sk2.encrypt(42, b"y")).is_none());
    }

    #[test]
    fn video_frames() {
        let mut sk = SenderKey::new(1);
        let mut rk = ReceiverKeys::default();
        rk.install(&sk.export());
        let v0 = sk.encrypt_video(5, b"h264-key-frame");
        let v1 = sk.encrypt_video(5, b"h264-p-frame");
        // 视频和语音的计数器互不影响
        let a0 = sk.encrypt(5, b"audio");
        assert_eq!(rk.decrypt(5, &a0).map(|x| x.1), Some(b"audio".to_vec()));
        assert_eq!(rk.decrypt_video(5, &v0), Some(b"h264-key-frame".to_vec()));
        assert_eq!(rk.decrypt_video(5, &v1), Some(b"h264-p-frame".to_vec()));
        assert!(rk.decrypt_video(5, &v1).is_none(), "replay");
        assert!(rk.decrypt_video(6, &sk.encrypt_video(5, b"x")).is_none(), "wrong sender");
        // 视频密文不能当语音解
        assert!(rk.decrypt(5, &sk.encrypt_video(5, b"y")).is_none());
    }
}
