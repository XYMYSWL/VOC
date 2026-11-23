//! 用密码加密 + 混淆导出（第 33 项：服务器导出 / 导入授权码，换服务器用）。
//!
//! 输出是一串 base64 字符：`盐(16) ‖ 随机数(12) ‖ 密文`，没有任何固定开头、字段名、版本号，
//! 别人拿到也看不出是什么。密钥 = PBKDF2-HMAC-SHA256(密码, 盐)，用 ChaCha20-Poly1305 加密；
//! 密码不对 / 内容被改过都打不开。
//!
//! 因为没有版本标记，密码派生参数不能写进输出里：解密时按 [`KDF_ITERATIONS`] 里的已知参数挨个试。
//! **以后换参数只能往表里加，不能删**，不然旧的导出文件就打不开了。

use crate::auth::random_bytes;
use base64::Engine;
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// 已知的 PBKDF2 迭代次数，第一个是导出用的（OWASP 2023 对 PBKDF2-HMAC-SHA256 的建议值）。只加不删
pub const KDF_ITERATIONS: &[u32] = &[600_000];
/// 密码最短长度
pub const MIN_PASSWORD_CHARS: usize = 8;
/// 导入时最多接受多大（防止把奇怪的大文件当成导出内容）
const MAX_TEXT: usize = 16 * 1024 * 1024;
const SALT: usize = 16;
const NONCE: usize = 12;
const TAG: usize = 16;

/// PBKDF2-HMAC-SHA256，输出 32 字节（只要一块，RFC 8018）
fn pbkdf2(password: &[u8], salt: &[u8], n: u32) -> [u8; 32] {
    let mac = |data: &[&[u8]]| {
        let mut m = <Hmac<Sha256>>::new_from_slice(password).expect("HMAC 接受任意长度的密钥");
        for d in data {
            m.update(d);
        }
        let out: [u8; 32] = m.finalize().into_bytes().into();
        out
    };
    let mut u = mac(&[salt, &1u32.to_be_bytes()]);
    let mut t = u;
    for _ in 1..n {
        u = mac(&[&u]);
        for (a, b) in t.iter_mut().zip(u.iter()) {
            *a ^= b;
        }
    }
    t
}

fn seal_with(plain: &[u8], password: &str, purpose: &[u8], iterations: u32) -> String {
    let salt: [u8; SALT] = random_bytes();
    let nonce: [u8; NONCE] = random_bytes();
    let key = pbkdf2(password.as_bytes(), &salt, iterations);
    let ct = ChaCha20Poly1305::new((&key).into()).encrypt(&Nonce::from(nonce), Payload { msg: plain, aad: purpose }).expect("encrypt");
    let mut out = Vec::with_capacity(SALT + NONCE + ct.len());
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(out)
}

/// 用密码加密成一串看不出格式的字符。`purpose` 区分用途（不出现在输出里，解密时要给一样的）
pub fn seal(plain: &[u8], password: &str, purpose: &[u8]) -> String {
    seal_with(plain, password, purpose, KDF_ITERATIONS[0])
}

/// 解开 [`seal`] 的输出。空白、换行会被忽略（方便复制粘贴时被折行）
pub fn open(text: &str, password: &str, purpose: &[u8]) -> Result<Vec<u8>, String> {
    open_with(text, password, purpose, KDF_ITERATIONS)
}

fn open_with(text: &str, password: &str, purpose: &[u8], iterations: &[u32]) -> Result<Vec<u8>, String> {
    if text.len() > MAX_TEXT {
        return Err("内容太大，不是导出的数据".into());
    }
    let clean: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let raw = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(clean.trim_end_matches('='))
        .map_err(|_| "不是导出的数据（内容不完整或复制错了）".to_string())?;
    if raw.len() < SALT + NONCE + TAG {
        return Err("不是导出的数据（内容不完整或复制错了）".into());
    }
    let (salt, rest) = raw.split_at(SALT);
    let (nonce, ct) = rest.split_at(NONCE);
    let nonce: [u8; NONCE] = nonce.try_into().expect("长度已检查");
    for &n in iterations {
        let key = pbkdf2(password.as_bytes(), salt, n);
        if let Ok(pt) = ChaCha20Poly1305::new((&key).into()).decrypt(&Nonce::from(nonce), Payload { msg: ct, aad: purpose }) {
            return Ok(pt);
        }
    }
    Err("密码不对，或者内容被改过 / 不完整".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7914 第 11 节的 PBKDF2-HMAC-SHA256 测试向量（取前 8 字节）
    #[test]
    fn pbkdf2_matches_known_vector() {
        let out = pbkdf2(b"passwd", b"salt", 1);
        assert_eq!(out[..8], [0x55, 0xac, 0x04, 0x6e, 0x56, 0xe3, 0x08, 0x9f]);
        let out = pbkdf2(b"Password", b"NaCl", 80000);
        assert_eq!(out[..8], [0x4d, 0xdc, 0xd8, 0xf6, 0x0b, 0x98, 0xbe, 0x21]);
    }

    #[test]
    fn seal_open_roundtrip_wrong_password_tamper_and_no_markers() {
        let plain = br#"[{"id":"ABCDEFGH","psk":"00"}]"#;
        let text = seal_with(plain, "correct horse", b"t", 2000);
        // 看不出格式：没有明文、字段名、VOC 字样，只有 base64 字符
        for w in ["ABCDEFGH", "psk", "VOC", "voc", "{", "\"", "="] {
            assert!(!text.contains(w), "{w} in {text}");
        }
        assert!(text.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/'));
        assert_eq!(open_with(&text, "correct horse", b"t", &[2000]).unwrap(), plain);
        // 复制时被折行 / 前后带空白也能打开
        let wrapped = format!("  {}\n{}\r\n", &text[..20], &text[20..]);
        assert_eq!(open_with(&wrapped, "correct horse", b"t", &[2000]).unwrap(), plain);
        // 已知参数表里挨个试：新参数在前、旧参数在后也能打开
        assert_eq!(open_with(&text, "correct horse", b"t", &[3000, 2000]).unwrap(), plain);
        assert!(open_with(&text, "wrong horse", b"t", &[2000]).is_err());
        assert!(open_with(&text, "correct horse", b"other purpose", &[2000]).is_err());
        let mut raw = base64::engine::general_purpose::STANDARD_NO_PAD.decode(&text).unwrap();
        *raw.last_mut().unwrap() ^= 1;
        let bad = base64::engine::general_purpose::STANDARD_NO_PAD.encode(raw);
        assert!(open_with(&bad, "correct horse", b"t", &[2000]).is_err());
        assert!(open_with("随便什么", "x", b"t", &[2000]).is_err());
        assert!(open_with("AAAA", "x", b"t", &[2000]).is_err());
        // 两次导出不一样（随机盐和随机数）
        assert_ne!(seal_with(plain, "correct horse", b"t", 2000), text);
    }
}
