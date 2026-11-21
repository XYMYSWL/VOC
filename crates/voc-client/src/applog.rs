//! 客户端运行日志：连接、断线、屏幕共享、更新、出错、崩溃这类事件写进本机文件，
//! 「导出诊断信息」时带上最后一段，查问题不用再让用户复现。
//!
//! 不记聊天内容、授权码、密钥、IP 地址。文件超过 1 MB 就换一个（只留一个旧文件）。
//! 位置：%LOCALAPPDATA%\VOC\voc.log（安装版由系统重定向到应用自己的目录）。

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

const MAX_BYTES: u64 = 1024 * 1024;

static PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

fn dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).or_else(|| std::env::var_os("TEMP").map(PathBuf::from))?;
    Some(base.join("VOC"))
}

/// 启动时调用：定好日志位置，装上崩溃记录
pub fn init() {
    if std::env::var_os("VOC_NO_LOG").is_some() {
        return;
    }
    let Some(d) = dir() else { return };
    if std::fs::create_dir_all(&d).is_err() {
        return;
    }
    *PATH.lock().unwrap() = Some(d.join("voc.log"));
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        log(format!("崩溃：{info}\n{bt}"));
        prev(info);
    }));
    log(format!("启动 VOC {}（{}）", env!("CARGO_PKG_VERSION"), if crate::update::is_packaged() { "安装版" } else { "绿色版" }));
}

/// 记一行（带时间）
pub fn log(msg: impl AsRef<str>) {
    let Some(path) = PATH.lock().unwrap().clone() else { return };
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = std::fs::rename(&path, path.with_extension("old.log"));
    }
    let ms = voc_proto::now_millis();
    let secs = ms / 1000 + 8 * 3600;
    let line = format!("[{} {:02}:{:02}:{:02}.{:03}] {}\n", day(ms), (secs / 3600) % 24, (secs / 60) % 60, secs % 60, ms % 1000, msg.as_ref());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// 日期（北京时间，和界面上的时间一致）
fn day(ms: u64) -> String {
    let days = (ms / 1000 + 8 * 3600) / 86_400;
    // 1970-01-01 起的天数 → 年月日（公历）
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", if m <= 2 { y + 1 } else { y }, m, d)
}

/// 最后 n 行（导出诊断用）
pub fn tail(n: usize) -> String {
    let Some(path) = PATH.lock().unwrap().clone() else { return String::new() };
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(day(0), "1970-01-01");
        // 2026-11-22 00:30 北京时间 = 2026-11-21 16:30 UTC
        assert_eq!(day(1_795_278_600_000), "2026-11-22");
        assert_eq!(day(951_782_400_000), "2000-02-29"); // 闰日（UTC 00:00 = 北京 08:00）
    }
}
