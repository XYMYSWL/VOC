//! 授权码：服务器生成，用户登录时填写。
//!
//! 格式：`KH-IIIIIIII-SSSS-SSSS-SSSS-SSSS-SSSS-SS`
//! - `I`（8 字符，40 bit）：公开的授权码 ID，握手时明文发送，服务器据此查找密钥
//! - `S`（26 字符，128 bit）：秘密部分，**永不在网络上传输**，只用于派生预共享密钥（PSK）
//!
//! 字符集为 Crockford Base32（不含 I L O U，输入时自动纠正 O→0、I/L→1，忽略大小写、空格和横线）。
//! 服务器只保存 PSK = SHA-256(域分隔 || ID || 秘密)，不保存授权码明文。

use sha2::{Digest, Sha256};

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
pub const ID_LEN: usize = 8;
pub const SECRET_LEN: usize = 26;

#[derive(Clone, PartialEq, Eq)]
pub struct AuthCode {
    pub id: String,
    secret: String,
}

impl std::fmt::Debug for AuthCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AuthCode({}-****)", self.id)
    }
}

fn base32(bytes: &[u8], chars: usize) -> String {
    let mut out = String::with_capacity(chars);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in bytes {
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 5 && out.len() < chars {
            out.push(ALPHABET[((acc >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 && out.len() < chars {
        out.push(ALPHABET[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn normalize_char(c: char) -> Option<char> {
    let c = c.to_ascii_uppercase();
    let c = match c {
        'O' => '0',
        'I' | 'L' => '1',
        c => c,
    };
    ALPHABET.contains(&(c as u8)).then_some(c)
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("系统随机数不可用");
    b
}

impl AuthCode {
    pub fn generate() -> Self {
        Self { id: base32(&random_bytes::<5>(), ID_LEN), secret: base32(&random_bytes::<16>(), SECRET_LEN) }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        // 连同「授权码：」之类的前缀、结尾的引号句号一起粘贴进来也能认：去掉首尾不是字母数字的部分（含中文、全角标点）
        let s = s.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        let mut cleaned: String = s.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
        // 按字符边界取前两个（不能按字节切：开头可能是多字节字符）
        if cleaned.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("KH")) {
            cleaned.drain(..2);
        }
        let mut chars = Vec::with_capacity(ID_LEN + SECRET_LEN);
        for c in cleaned.chars() {
            chars.push(normalize_char(c).ok_or_else(|| format!("授权码包含无效字符「{c}」"))?);
        }
        if chars.len() != ID_LEN + SECRET_LEN {
            return Err("授权码长度不对，请完整复制服务器给你的授权码".into());
        }
        let id: String = chars[..ID_LEN].iter().collect();
        let secret: String = chars[ID_LEN..].iter().collect();
        Ok(Self { id, secret })
    }

    /// 给用户看的格式
    pub fn display(&self) -> String {
        let groups: Vec<String> =
            self.secret.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect();
        format!("KH-{}-{}", self.id, groups.join("-"))
    }

    /// 预共享密钥：服务器保存它，客户端由授权码现算
    pub fn psk(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(b"voc-psk-v1\0");
        h.update(self.id.as_bytes());
        h.update(b"\0");
        h.update(self.secret.as_bytes());
        h.finalize().into()
    }
}

pub fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn from_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_typos() {
        let c = AuthCode::generate();
        let shown = c.display();
        assert!(shown.starts_with("KH-"));
        assert_eq!(AuthCode::parse(&shown).unwrap(), c);
        // 小写、空格、O/0 混淆都能识别
        let messy = shown.to_lowercase().replace('-', " ").replace('0', "o");
        assert_eq!(AuthCode::parse(&messy).unwrap().psk(), c.psk());
        assert!(AuthCode::parse("KH-123").is_err());
        assert!(AuthCode::parse(&shown.replace('A', "U")).is_err() || !shown.contains('A'));
    }

    #[test]
    fn non_ascii_input_does_not_panic() {
        let c = AuthCode::generate();
        let shown = c.display();
        // 带中文前缀、全角冒号：能认出来
        assert_eq!(AuthCode::parse(&format!("授权码：{shown}")).unwrap(), c);
        assert_eq!(AuthCode::parse(&format!("「{shown}」")).unwrap(), c);
        // 各种乱输入：报错，不崩溃
        for s in ["中", "中文", "Ｋ", "ＫＨ-ABC", "K中", "é", "😀😀", "", "  ", "-", "KH", "k"] {
            let _ = AuthCode::parse(s);
        }
        assert!(AuthCode::parse("中文").is_err());
    }

    #[test]
    fn hex() {
        let k = AuthCode::generate().psk();
        assert_eq!(from_hex32(&to_hex(&k)), Some(k));
    }
}
