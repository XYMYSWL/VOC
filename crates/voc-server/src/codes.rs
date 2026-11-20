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
        let mut s = Self { path, mtime: None, entries: Vec::new(), psks: Vec::new() };
        s.reload()?;
        Ok(s)
    }

    fn reload(&mut self) -> Result<(), String> {
        let entries = read_file(&self.path)?;
        self.psks = entries.iter().map(|e| from_hex32(&e.psk).unwrap_or([0; 32])).collect();
        self.entries = entries;
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
