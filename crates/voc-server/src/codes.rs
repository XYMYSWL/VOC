//! 授权码存储（JSON 文件）。只保存 PSK（授权码的哈希派生值），不保存授权码明文。
//! 服务器运行时会自动发现文件变化，新生成/吊销的授权码无需重启即可生效。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use voc_proto::auth::{from_hex32, to_hex, AuthCode};
use voc_proto::now_millis;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CodeEntry {
    pub id: String,
    /// hex(psk)
    pub psk: String,
    #[serde(default)]
    pub note: String,
    pub created: u64,
    #[serde(default)]
    pub expires: Option<u64>,
    #[serde(default)]
    pub revoked: bool,
}

impl CodeEntry {
    /// 当前是否可用；不可用时返回原因
    pub fn check(&self, now_ms: u64) -> Result<(), String> {
        if self.revoked {
            return Err("授权码已被管理员吊销".into());
        }
        if self.expires.is_some_and(|e| now_ms > e) {
            return Err("授权码已过期".into());
        }
        Ok(())
    }
}

pub struct CodeStore {
    path: PathBuf,
    mtime: Option<SystemTime>,
    pub entries: Vec<CodeEntry>,
    /// 与 entries 一一对应（握手试解用）
    pub psks: Vec<[u8; 32]>,
    /// 密钥损坏、已停用的授权码 ID
    pub broken: Vec<String>,
}

/// 分出密钥完好的授权码（和 PSK 一一对应）与损坏的（只返回 ID）
fn split_valid(all: Vec<CodeEntry>) -> (Vec<CodeEntry>, Vec<[u8; 32]>, Vec<String>) {
    let (mut entries, mut psks, mut broken) = (Vec::new(), Vec::new(), Vec::new());
    for e in all {
        match from_hex32(&e.psk) {
            Some(k) => {
                psks.push(k);
                entries.push(e);
            }
            None => broken.push(e.id),
        }
    }
    (entries, psks, broken)
}

pub fn read_file(path: &Path) -> Result<Vec<CodeEntry>, String> {
    match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(Vec::new()),
        Ok(s) => serde_json::from_str(&s).map_err(|e| format!("{} 格式错误：{e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("读取 {} 失败：{e}", path.display())),
    }
}

pub fn write_file(path: &Path, entries: &[CodeEntry]) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_string_pretty(entries).unwrap();
    std::fs::write(&tmp, data).map_err(|e| format!("写入失败：{e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("写入失败：{e}"))
}

impl CodeStore {
    pub fn open(path: PathBuf) -> Result<Self, String> {
        let mut s = Self { path, mtime: None, entries: Vec::new(), psks: Vec::new(), broken: Vec::new() };
        s.reload()?;
        Ok(s)
    }

    fn reload(&mut self) -> Result<(), String> {
        let (entries, psks, broken) = split_valid(read_file(&self.path)?);
        // PSK 损坏（文件被改坏 / 迁移出错）的授权码直接停用：绝不能拿固定值（例如全 0）当密钥，那样谁都能冒充
        for id in &broken {
            eprintln!("[VOC] 授权码 {id} 在 {} 里的密钥损坏，已停用（重新生成一个给这个人）", self.path.display());
        }
        self.psks = psks;
        self.entries = entries;
        self.broken = broken;
        self.mtime = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        Ok(())
    }

    /// 文件有变化就重新加载
    pub fn refresh(&mut self) {
        let m = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        if m != self.mtime {
            if let Err(e) = self.reload() {
                eprintln!("[VOC] {e}（继续使用旧的授权码列表）");
                self.mtime = m;
            }
        }
    }

    pub fn find(&self, id: &str) -> Option<&CodeEntry> {
        self.entries.iter().find(|e| e.id == id)
    }
}

// ------------------------------------------------------------ 命令行管理

pub fn generate(path: &Path, count: usize, note: &str, days: Option<u64>) -> Result<Vec<AuthCode>, String> {
    let mut entries = read_file(path)?;
    let now = now_millis();
    let mut out = Vec::new();
    for _ in 0..count {
        let code = AuthCode::generate();
        entries.push(CodeEntry {
            id: code.id.clone(),
            psk: to_hex(&code.psk()),
            note: note.to_string(),
            created: now,
            expires: days.map(|d| now + d * 86_400_000),
            revoked: false,
        });
        out.push(code);
    }
    write_file(path, &entries)?;
    Ok(out)
}

pub fn set_revoked(path: &Path, id: &str, revoked: bool) -> Result<(), String> {
    let mut entries = read_file(path)?;
    let id = id.trim().trim_start_matches("KH-").to_ascii_uppercase();
    let e = entries.iter_mut().find(|e| e.id == id).ok_or_else(|| format!("找不到授权码 ID {id}"))?;
    e.revoked = revoked;
    write_file(path, &entries)
}

/// 导出 / 导入授权码时加密用的用途标记（不出现在导出内容里）
const EXPORT_PURPOSE: &[u8] = b"voc-server-users-v1";

/// 把全部授权码（含备注、到期、吊销）用密码加密 + 混淆成一串字符（第 33 项：换服务器用）
pub fn export_users(path: &Path, password: &str) -> Result<(String, usize), String> {
    let entries = read_file(path)?;
    let json = serde_json::to_vec(&entries).map_err(|e| e.to_string())?;
    Ok((voc_proto::backup::seal(&json, password, EXPORT_PURPOSE), entries.len()))
}

/// 导入结果
#[derive(Debug, Default, PartialEq)]
pub struct ImportReport {
    /// 新加进来的
    pub added: usize,
    /// 已经有了（ID 和密钥都一样），没动
    pub same: usize,
    /// 同一个 ID、密钥不一样：跳过（不覆盖本服务器已有的）
    pub conflict: Vec<String>,
    /// 导出内容里密钥损坏的：跳过
    pub broken: Vec<String>,
}

/// 把 [`export_users`] 导出的内容合并进本服务器的授权码文件。已有的不动；运行中的服务器几秒内自动生效
pub fn import_users(path: &Path, text: &str, password: &str) -> Result<ImportReport, String> {
    let json = voc_proto::backup::open(text, password, EXPORT_PURPOSE)?;
    let incoming: Vec<CodeEntry> = serde_json::from_slice(&json).map_err(|_| "导出内容的格式不认识（可能是更新版本的 VOC 导出的）".to_string())?;
    let mut entries = read_file(path)?;
    let mut r = ImportReport::default();
    for e in incoming {
        if from_hex32(&e.psk).is_none() {
            r.broken.push(e.id);
        } else if let Some(old) = entries.iter().find(|o| o.id == e.id) {
            if old.psk.eq_ignore_ascii_case(&e.psk) {
                r.same += 1;
            } else {
                r.conflict.push(e.id);
            }
        } else {
            entries.push(e);
            r.added += 1;
        }
    }
    if r.added > 0 {
        write_file(path, &entries)?;
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, psk: &str) -> CodeEntry {
        CodeEntry { id: id.into(), psk: psk.into(), note: String::new(), created: 0, expires: None, revoked: false }
    }

    #[test]
    fn export_import_users_merges_without_overwriting() {
        let dir = std::env::temp_dir().join(format!("voc-users-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.json"), dir.join("b.json"));
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
        generate(&a, 3, "朋友", Some(30)).unwrap();
        set_revoked(&a, &read_file(&a).unwrap()[2].id, true).unwrap();
        let (text, n) = export_users(&a, "correct horse").unwrap();
        assert_eq!(n, 3);
        let src = read_file(&a).unwrap();
        assert!(!text.contains(&src[0].id) && !text.contains(&src[0].psk) && !text.contains("朋友"));
        assert!(import_users(&b, &text, "wrong horse").is_err());
        assert!(!b.exists(), "wrong password writes nothing");

        // 新服务器上已经有一个自己的码，另有一个 ID 撞了但密钥不同
        generate(&b, 1, "", None).unwrap();
        let mut bl = read_file(&b).unwrap();
        bl.push(CodeEntry { id: src[1].id.clone(), ..bl[0].clone() });
        write_file(&b, &bl).unwrap();

        let r = import_users(&b, &text, "correct horse").unwrap();
        assert_eq!(r, ImportReport { added: 2, same: 0, conflict: vec![src[1].id.clone()], broken: vec![] });
        let after = read_file(&b).unwrap();
        assert_eq!(after.len(), 4);
        let got = after.iter().find(|e| e.id == src[2].id).unwrap();
        assert!(got.revoked && got.note == "朋友" && got.expires == src[2].expires && got.psk == src[2].psk);
        assert_eq!(after.iter().find(|e| e.id == src[1].id).unwrap().psk, bl[0].psk, "conflict not overwritten");
        // 再导一次：都已经有了
        let r = import_users(&b, &text, "correct horse").unwrap();
        assert_eq!((r.added, r.same, r.conflict.len()), (0, 2, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_psk_is_disabled_not_zeroed() {
        let good = to_hex(&AuthCode::generate().psk());
        let (entries, psks, broken) = split_valid(vec![
            entry("AAAAAAAA", &good),
            entry("BBBBBBBB", "zz"),
            entry("CCCCCCCC", ""),
            entry("DDDDDDDD", &good[..62]),
        ]);
        assert_eq!(entries.len(), 1);
        assert_eq!(psks.len(), 1);
        assert_eq!(entries[0].id, "AAAAAAAA");
        assert_eq!(broken, vec!["BBBBBBBB", "CCCCCCCC", "DDDDDDDD"]);
        assert!(!psks.contains(&[0u8; 32]), "never fall back to a fixed key");
    }
}