//! 图片传输：端到端加密，只上传一份，服务器边收边转给同频道的人，不保存。
//!
//! - 发送方给每张图生成随机 256 位密钥，按块用 ChaCha20-Poly1305 加密
//!   （nonce = 块序号，附加数据 = 图片 id，块不能被调换、截断或拼到别的图里）。
//! - 密钥和图片信息（大小、格式、块数）用成对密钥加密后单独发给每个接收者（E2E 信封 BLOB_IMAGE）。
//! - 密文块走 TCP 的 KIND_FILE 帧：客户端 → 服务器 [id u64][seq u32][total u32][密文]，
//!   服务器 → 客户端前面再加 [发送者 u32]。

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde::{Deserialize, Serialize};

/// 单张图片最大 20 MB
pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
/// 每块明文大小（加密后再加 16 字节认证标签，整帧不超过 MAX_FRAME_DATA）
pub const CHUNK_PLAIN: usize = 48 * 1024;
const TAG: usize = 16;
/// 一张图最多多少块
pub const MAX_CHUNKS: u32 = (MAX_IMAGE_BYTES / CHUNK_PLAIN + 1) as u32;
const CLIENT_HDR: usize = 8 + 4 + 4;

/// 随图片密钥一起发给接收者的信息（JSON，放在 E2E 信封里）
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ImageMeta {
    pub id: u64,
    /// 图片密钥（base64）
    pub key: String,
    /// 明文字节数
    pub size: u32,
    /// 块数
    pub chunks: u32,
    /// 例如 "image/png"
    pub mime: String,
    pub width: u32,
    pub height: u32,
}

pub fn chunk_count(size: usize) -> u32 {
    size.div_ceil(CHUNK_PLAIN).max(1) as u32
}

fn nonce(seq: u32) -> Nonce {
    let mut n = [0u8; 12];
    n[..4].copy_from_slice(&seq.to_le_bytes());
    Nonce::from(n)
}

/// 加密第 seq 块
pub fn encrypt_chunk(key: &[u8; 32], id: u64, seq: u32, plain: &[u8]) -> Vec<u8> {
    let c = ChaCha20Poly1305::new(&Key::from(*key));
    c.encrypt(&nonce(seq), Payload { msg: plain, aad: &id.to_le_bytes() }).expect("encrypt")
}

/// 解密第 seq 块；被篡改 / 拼接返回 None
pub fn decrypt_chunk(key: &[u8; 32], id: u64, seq: u32, cipher: &[u8]) -> Option<Vec<u8>> {
    let c = ChaCha20Poly1305::new(&Key::from(*key));
    c.decrypt(&nonce(seq), Payload { msg: cipher, aad: &id.to_le_bytes() }).ok()
}

/// 客户端发出的一块
pub fn client_chunk(id: u64, seq: u32, total: u32, cipher: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(CLIENT_HDR + cipher.len());
    v.extend_from_slice(&id.to_le_bytes());
    v.extend_from_slice(&seq.to_le_bytes());
    v.extend_from_slice(&total.to_le_bytes());
    v.extend_from_slice(cipher);
    v
}

/// 服务器解析客户端发来的一块：(id, seq, total, 密文)
pub fn read_client_chunk(b: &[u8]) -> Option<(u64, u32, u32, &[u8])> {
    if b.len() <= CLIENT_HDR || b.len() > CLIENT_HDR + CHUNK_PLAIN + TAG {
        return None;
    }
    let id = u64::from_le_bytes(b[..8].try_into().ok()?);
    let seq = u32::from_le_bytes(b[8..12].try_into().ok()?);
    let total = u32::from_le_bytes(b[12..16].try_into().ok()?);
    (total >= 1 && total <= MAX_CHUNKS && seq < total).then_some((id, seq, total, &b[CLIENT_HDR..]))
}

/// 服务器转发时在前面加上发送者
pub fn server_chunk(from: u32, client_chunk: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + client_chunk.len());
    v.extend_from_slice(&from.to_le_bytes());
    v.extend_from_slice(client_chunk);
    v
}

/// 客户端解析服务器转来的一块：(发送者, id, seq, total, 密文)
pub fn read_server_chunk(b: &[u8]) -> Option<(u32, u64, u32, u32, &[u8])> {
    if b.len() < 4 {
        return None;
    }
    let from = u32::from_le_bytes(b[..4].try_into().ok()?);
    let (id, seq, total, c) = read_client_chunk(&b[4..])?;
    Some((from, id, seq, total, c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_roundtrip_and_tamper() {
        let key = [7u8; 32];
        let plain: Vec<u8> = (0..CHUNK_PLAIN * 2 + 100).map(|i| (i % 251) as u8).collect();
        let n = chunk_count(plain.len());
        assert_eq!(n, 3);
        let mut out = Vec::new();
        for (seq, p) in plain.chunks(CHUNK_PLAIN).enumerate() {
            let c = encrypt_chunk(&key, 42, seq as u32, p);
            let wire = server_chunk(9, &client_chunk(42, seq as u32, n, &c));
            let (from, id, s, total, ct) = read_server_chunk(&wire).unwrap();
            assert_eq!((from, id, s, total), (9, 42, seq as u32, n));
            out.extend(decrypt_chunk(&key, id, s, ct).unwrap());
        }
        assert_eq!(out, plain);
        let c = encrypt_chunk(&key, 42, 0, b"hello");
        assert!(decrypt_chunk(&key, 42, 1, &c).is_none(), "chunk moved to another position");
        assert!(decrypt_chunk(&key, 43, 0, &c).is_none(), "chunk spliced into another image");
        assert!(decrypt_chunk(&[8u8; 32], 42, 0, &c).is_none(), "wrong key");
        let mut bad = c.clone();
        bad[0] ^= 1;
        assert!(decrypt_chunk(&key, 42, 0, &bad).is_none(), "tampered");
    }

    #[test]
    fn header_limits() {
        assert!(read_client_chunk(&client_chunk(1, 0, MAX_CHUNKS + 1, b"x")).is_none(), "too many chunks");
        assert!(read_client_chunk(&client_chunk(1, 5, 5, b"x")).is_none(), "seq out of range");
        assert!(read_client_chunk(&client_chunk(1, 0, 1, &[])).is_none(), "empty");
        assert!(read_client_chunk(&client_chunk(1, 0, 1, &vec![0; CHUNK_PLAIN + TAG + 1])).is_none(), "oversized");
        assert!(MAX_CHUNKS as usize * CHUNK_PLAIN >= MAX_IMAGE_BYTES);
        assert!(CLIENT_HDR + 4 + CHUNK_PLAIN + TAG <= crate::crypto::MAX_FRAME_DATA);
    }
}
