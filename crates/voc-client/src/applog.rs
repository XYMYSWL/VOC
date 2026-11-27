//! 客户端运行日志：连接、断线、屏幕共享、更新、出错、崩溃这类事件写进本机文件，
//! 「导出诊断信息」时带上最后一段，查问题不用再让用户复现。
//!
//! 不记聊天内容、授权码、密钥、IP 地址、昵称（成员用编号 #12 表示）。文件超过 1 MB 就换一个（只留一个旧文件）。
//! 位置（0.6 起）：
//! - 绿色版：程序所在文件夹下的 `logs\voc.log`（那里写不进去时，例如放在 Program Files 里，改用 %LOCALAPPDATA%\VOC）
//! - 安装版：应用私有目录 %LOCALAPPDATA%\Packages\<包名>\LocalCache\Local\VOC\voc.log，所有安装版路径一样
//!   （安装目录 WindowsApps 是系统只读的，写不进去；以前靠系统重定向，用过绿色版的电脑上位置会不一样）
//! 设置 → 关于里能看到实际路径、打开文件夹、复制路径。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_BYTES: u64 = 1024 * 1024;

static PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

fn local_appdata() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).or_else(|| std::env::var_os("TEMP").map(PathBuf::from))
}

/// 0.5.x 的位置（安装版被系统重定向，绿色版就在这里）
fn old_dir() -> Option<PathBuf> {
    Some(local_appdata()?.join("VOC"))
}

/// 安装版：包的家族名（例如 VOC.VoiceChat_v6je519s3qkf6），没有包身份返回 None
#[cfg(windows)]
fn package_family_name() -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFamilyName;
    let mut len = 0u32;
    unsafe {
        let _ = GetCurrentPackageFamilyName(&mut len, None);
        if len == 0 || len > 1024 {
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        if GetCurrentPackageFamilyName(&mut len, Some(PWSTR(buf.as_mut_ptr()))).0 != 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..(len as usize).saturating_sub(1)]))
    }
}

#[cfg(not(windows))]
fn package_family_name() -> Option<String> {
    None
}

/// 能不能在这个文件夹里写文件
fn writable(d: &Path) -> bool {
    if std::fs::create_dir_all(d).is_err() {
        return false;
    }
    let probe = d.join(".voc-write-test");
    let ok = std::fs::write(&probe, b"x").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

fn dir() -> Option<PathBuf> {
    if let Some(fam) = package_family_name() {
        let d = local_appdata()?.join("Packages").join(fam).join("LocalCache").join("Local").join("VOC");
        if writable(&d) {
            return Some(d);
        }
    } else if let Some(exe_dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        let d = exe_dir.join("logs");
        if writable(&d) {
            return Some(d);
        }
    }
    let d = old_dir()?;
    writable(&d).then_some(d)
}

/// VOC 自己的数据文件夹（日志所在的地方；毛玻璃开关的标记文件也放这里）
pub fn data_dir() -> Option<PathBuf> {
    dir()
}

/// 日志文件的实际位置（界面上显示、打开文件夹用）
pub fn path() -> Option<PathBuf> {
    PATH.lock().unwrap().clone()
}

/// 启动时调用：定好日志位置，装上崩溃记录
pub fn init() {
    if std::env::var_os("VOC_NO_LOG").is_some() {
        return;
    }
    let Some(d) = dir() else { return };
    let file = d.join("voc.log");
    // 第一次换到新位置：把旧位置的日志接过来（只做一次）
    if !file.exists() {
        if let Some(old) = old_dir().map(|o| o.join("voc.log")).filter(|o| *o != file && o.exists()) {
            let _ = std::fs::copy(&old, &file);
        }
    }
    *PATH.lock().unwrap() = Some(file);
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        log(format!("崩溃：{info}\n{bt}"));
        prev(info);
    }));
    log(format!("启动 VOC {}（{}）", env!("CARGO_PKG_VERSION"), if crate::update::is_packaged() { "安装版" } else { "绿色版" }));
}

/// 启动进行到哪一步（第 89 项：启动卡住时知道卡在哪）；界面画出第一帧以后不再记
static STEP: Mutex<&str> = Mutex::new("开始");
pub static FIRST_FRAME: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 启动的一步（写一行日志，并记下来给「启动卡住」用）
pub fn step(s: &'static str) {
    *STEP.lock().unwrap() = s;
    log(format!("启动：{s}"));
}

/// 启动 10 秒还没画出窗口：写一行卡在哪一步（第 89 项）
pub fn start_watchdog() {
    let _ = std::thread::Builder::new().name("start-watchdog".into()).spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(10));
        if !FIRST_FRAME.load(std::sync::atomic::Ordering::Relaxed) {
            log(format!("启动 10 秒还没显示窗口，卡在：{}", *STEP.lock().unwrap()));
        }
    });
}

/// 第 88 项：界面线程崩溃（例如打游戏时显卡驱动重置）后自动重开 VOC，回到原来的服务器和频道。
/// 只在主线程崩溃时重开（别的线程出错程序还在跑，不能再开一个）；连续 3 次都是开了不到 10 分钟就崩，就不再重开，免得一直闪
pub fn install_crash_restart() {
    let started = std::time::Instant::now();
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        prev(info);
        if std::thread::current().name() != Some("main") || std::env::var_os("VOC_NO_CRASH_RESTART").is_some() {
            return;
        }
        let n = crash_count();
        let n = if started.elapsed() < std::time::Duration::from_secs(600) { n + 1 } else { 1 };
        if n > 3 {
            log("崩溃后不再自动重开（连续 3 次开了不到 10 分钟就崩）");
            return;
        }
        let Ok(exe) = std::env::current_exe() else { return };
        match std::process::Command::new(exe).arg(format!("--after-crash={n}")).spawn() {
            Ok(_) => log(format!("崩溃后自动重开 VOC（第 {n} 次）")),
            Err(e) => log(format!("崩溃后重开 VOC 失败：{e}")),
        }
    }));
}

/// 这次是崩溃后第几次自动重开（命令行 --after-crash=N；0 = 不是）
pub fn crash_count() -> u32 {
    std::env::args().find_map(|a| a.strip_prefix("--after-crash=").and_then(|n| n.parse().ok())).unwrap_or(0)
}

/// 记一行（带时间）
pub fn log(msg: impl AsRef<str>) {
    let Some(path) = PATH.lock().unwrap().clone() else { return };
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = std::fs::rename(&path, path.with_extension("old.log"));
    }
    let ms = voc_proto::now_millis();
    let t = local_time(ms);
    let line = format!("[{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}] {}\n", t.0, t.1, t.2, t.3, t.4, t.5, ms % 1000, msg.as_ref());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Unix 毫秒 → 电脑本地时区的（年, 月, 日, 时, 分, 秒）；拿不到时区时按 UTC
pub fn local_time(ms: u64) -> (i64, u32, u32, u32, u32, u32) {
    #[cfg(windows)]
    if let Some(t) = win_local(ms) {
        return t;
    }
    utc_time(ms)
}

/// 电脑现在的时区，例如「UTC+9」「UTC+5:30」（诊断信息用）
pub fn tz_label() -> String {
    let ms = voc_proto::now_millis();
    let (y, mo, d, h, mi, _) = local_time(ms);
    let local = days_from_civil(y, mo, d) * 86_400 + h as i64 * 3600 + mi as i64 * 60;
    let utc = (ms / 1000) as i64 / 60 * 60;
    let off = (local - utc) / 60;
    let (sign, off) = if off < 0 { ('-', -off) } else { ('+', off) };
    if off % 60 == 0 {
        format!("UTC{sign}{}", off / 60)
    } else {
        format!("UTC{sign}{}:{:02}", off / 60, off % 60)
    }
}

#[cfg(windows)]
fn win_local(ms: u64) -> Option<(i64, u32, u32, u32, u32, u32)> {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    let ticks = ms * 10_000 + 116_444_736_000_000_000;
    let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    let mut utc = SYSTEMTIME::default();
    let mut loc = SYSTEMTIME::default();
    unsafe {
        FileTimeToSystemTime(&ft, &mut utc).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut loc).ok()?;
    }
    Some((loc.wYear as i64, loc.wMonth as u32, loc.wDay as u32, loc.wHour as u32, loc.wMinute as u32, loc.wSecond as u32))
}

fn utc_time(ms: u64) -> (i64, u32, u32, u32, u32, u32) {
    let secs = ms / 1000;
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    (y, m, d, ((secs / 3600) % 24) as u32, ((secs / 60) % 60) as u32, (secs % 60) as u32)
}

/// 1970-01-01 起的天数 → 年月日（公历）
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// 年月日 → 1970-01-01 起的天数
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
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
        assert_eq!(utc_time(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(utc_time(1_795_278_600_000), (2026, 11, 21, 16, 30, 0));
        assert_eq!(utc_time(951_782_400_000), (2000, 2, 29, 0, 0, 0)); // 闰日
        for days in [-1000i64, 0, 11_016, 20_413, 60_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        println!("本机时区 {}，现在 {:?}", tz_label(), local_time(voc_proto::now_millis()));
    }
}
