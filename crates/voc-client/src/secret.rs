//! 本机保存的机密（第 41-6 项）：端到端加密身份种子、记住的授权码。
//!
//! 写进配置文件时用 Windows 凭据保护（DPAPI，只有当前 Windows 用户在这台电脑上能解开）加密成
//! `dpapi:<base64>`；读出来时解开。别人拷走配置文件也拿不到身份和授权码。
//! 旧版本留下的明文照样认，下次保存时自动改成加密的。DPAPI 用不了时（极少见）按原样保存。
//! 换电脑 / 换 Windows 用户后解不开：会生成新身份（队友那边提示「身份变了」），授权码重新输入。

const PREFIX: &str = "dpapi:";

/// 加密成可以写进配置文件的文本；空的不加密
pub fn seal(plain: &str) -> String {
    if plain.is_empty() || plain.starts_with(PREFIX) {
        return plain.to_string();
    }
    match crate::pathcache::protect(plain.as_bytes(), true) {
        Some(ct) => format!("{PREFIX}{}", voc_proto::e2e::b64(&ct)),
        None => plain.to_string(),
    }
}

/// 解开配置文件里的文本。旧版本的明文原样返回；是加密的但解不开（换了电脑 / 用户）返回 None
pub fn open(stored: &str) -> Option<String> {
    let Some(b) = stored.strip_prefix(PREFIX) else { return Some(stored.to_string()) };
    let ct = voc_proto::e2e::unb64(b)?;
    let pt = crate::pathcache::protect(&ct, false)?;
    String::from_utf8(pt).ok()
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn seal_open_roundtrip_and_plaintext_passthrough() {
        let s = super::seal("KH-ABCD-1234");
        assert!(s.starts_with("dpapi:") && !s.contains("ABCD"), "{s}");
        assert_eq!(super::open(&s).as_deref(), Some("KH-ABCD-1234"));
        assert_eq!(super::open("old-plain").as_deref(), Some("old-plain"));
        assert_eq!(super::seal(""), "");
        assert_eq!(super::seal(&s), s, "already sealed stays as is");
        assert_eq!(super::open("dpapi:AAAA"), None, "corrupt / other user's blob");
    }
}
