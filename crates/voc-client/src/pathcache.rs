//! 记住和每个队友上次打通直连用的地址（第 49 项）：下次上线 / 断线重连时先试这些地址，打洞更快。
//!
//! - 按队友的**身份指纹**记（不是昵称：改昵称也认得；冒充的人指纹不同，拿不到）
//! - 文件用 Windows 凭据保护（DPAPI，只有当前 Windows 用户能解开）加密，别人拷走文件也看不到队友的 IP
//! - 每人最多记 4 个地址，30 天没用过的自动删；设置里可以一键清除

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Mutex;

const MAX_PER_PEER: usize = 4;
const KEEP_MS: u64 = 30 * 24 * 3600 * 1000;

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Cache {
    /// 身份指纹 → [(地址, 上次成功的时间 ms)]
    peers: HashMap<String, Vec<(String, u64)>>,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

fn file() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    Some(base.join("VOC").join("peer-paths.bin"))
}

#[cfg(windows)]
pub(crate) fn protect(data: &[u8], encrypt: bool) -> Option<Vec<u8>> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN};
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        let r = if encrypt {
            CryptProtectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        } else {
            CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        };
        r.ok()?;
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData as _)));
        Some(v)
    }
}

#[cfg(not(windows))]
pub(crate) fn protect(_data: &[u8], _encrypt: bool) -> Option<Vec<u8>> {
    None
}

fn with<R>(f: impl FnOnce(&mut Cache) -> R) -> R {
    let mut g = CACHE.lock().unwrap();
    let c = g.get_or_insert_with(|| {
        file()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| protect(&b, false))
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    });
    f(c)
}

fn save(c: &Cache) {
    let Some(p) = file() else { return };
    let Ok(json) = serde_json::to_vec(c) else { return };
    let Some(enc) = protect(&json, true) else { return }; // 加密不了就不存（绝不明文落盘）
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, enc).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

/// 打通了：记下这个地址
pub fn remember(fingerprint: &str, addr: SocketAddr, now_ms: u64) {
    if fingerprint.is_empty() {
        return;
    }
    with(|c| {
        let list = c.peers.entry(fingerprint.to_string()).or_default();
        let a = addr.to_string();
        if list.first().is_some_and(|(x, t)| *x == a && now_ms.saturating_sub(*t) < 3600 * 1000) {
            return; // 一小时内记过同一个，不重复写盘
        }
        list.retain(|(x, _)| *x != a);
        list.insert(0, (a, now_ms));
        list.truncate(MAX_PER_PEER);
        c.peers.retain(|_, l| {
            l.retain(|(_, t)| now_ms.saturating_sub(*t) < KEEP_MS);
            !l.is_empty()
        });
        save(c);
    });
}

/// 上次和他打通用过的地址（最近的在前）
pub fn recall(fingerprint: &str, now_ms: u64) -> Vec<SocketAddr> {
    if fingerprint.is_empty() {
        return Vec::new();
    }
    with(|c| {
        c.peers
            .get(fingerprint)
            .map(|l| l.iter().filter(|(_, t)| now_ms.saturating_sub(*t) < KEEP_MS).filter_map(|(a, _)| a.parse().ok()).collect())
            .unwrap_or_default()
    })
}

/// 一键清除（设置里）
pub fn clear() {
    with(|c| c.peers.clear());
    if let Some(p) = file() {
        let _ = std::fs::remove_file(p);
    }
}

/// 记了多少个队友（设置里显示）
pub fn count() -> usize {
    with(|c| c.peers.len())
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    #[test]
    fn dpapi_roundtrip() {
        let enc = super::protect(b"hello", true).unwrap();
        assert_ne!(enc, b"hello");
        assert_eq!(super::protect(&enc, false).unwrap(), b"hello");
        assert!(super::protect(b"not encrypted", false).is_none());
    }
}
