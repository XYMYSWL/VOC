//! 检查更新：读 GitHub Releases 的最新版本号，比当前版本新就提示。
//! 安装版（MSIX）还可以「一键更新」：下载 Release 里的 VOC-Setup.exe，核对 GitHub 给出的 SHA-256，运行它并退出客户端。
//! 只发匿名的 HTTPS GET（User-Agent 是固定的 "VOC-update-check"），不上传任何本机信息。
//!
//! 中国大陆加速：打开后安装程序改从 GitHub 加速镜像（[`MIRRORS`]，把 GitHub 地址拼在镜像地址后面）下载，
//! 一个不行换下一个，最后才直连 GitHub。版本号和 SHA-256 仍然从 GitHub 官方接口取，
//! 镜像只负责传文件，文件对不上 SHA-256 就丢掉——所以没有 SHA-256 的版本不走镜像。
//! 官方接口连不上时才经镜像问版本号，这时只提示有新版本、给下载链接，不一键安装（版本信息本身没法核对）。

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// GitHub 仓库（所有者/仓库名）。编译时可用环境变量 VOC_REPO 覆盖。
pub const REPO: &str = match option_env!("VOC_REPO") {
    Some(r) => r,
    None => "XYMYSWL/VOC",
};
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Release 里安装程序的文件名
pub const SETUP_ASSET: &str = "VOC-Setup.exe";
/// 绿色版压缩包
pub const PORTABLE_ASSET: &str = "VOC-portable.zip";
/// 国内的 GitHub 加速镜像（地址前缀，后面直接拼 https://github.com/... ），按顺序试
pub const MIRRORS: [&str; 3] = ["https://ghfast.top/", "https://gh-proxy.com/", "https://gh-proxy.org/"];

/// 经第一个加速镜像下载最新版某个文件的链接（给手动下载用）
pub fn accel_link(asset: &str) -> String {
    format!("{}https://github.com/{REPO}/releases/latest/download/{asset}", MIRRORS[0])
}

/// 下载安装程序要依次尝试的地址：加速时先各个镜像（要有 SHA-256 才能用镜像），最后直连 GitHub
fn download_urls(asset: &SetupAsset, accel: bool) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    if accel && asset.sha256.is_some() {
        v.extend(MIRRORS.iter().map(|m| format!("{m}{}", asset.url)));
    }
    v.push(asset.url.clone());
    v
}

/// Release 里的安装程序
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupAsset {
    pub url: String,
    /// GitHub 给出的 SHA-256（十六进制小写），老的 Release 可能没有
    pub sha256: Option<String>,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available { version: String, url: String, setup: Option<SetupAsset> },
    /// 一键更新：正在下载（0–100）
    Downloading { version: String, percent: u8 },
    /// 安装程序已启动，客户端应当退出
    Launched,
    Failed(String),
}

#[derive(Clone)]
pub struct Updater {
    pub state: Arc<Mutex<UpdateState>>,
    /// 中国大陆加速（设置里的开关）
    pub accel: Arc<AtomicBool>,
}

impl Default for Updater {
    fn default() -> Self {
        Self { state: Arc::new(Mutex::new(UpdateState::Idle)), accel: Arc::new(AtomicBool::new(false)) }
    }
}

fn agent(global_timeout: Option<Duration>) -> ureq::Agent {
    use ureq::tls::{TlsConfig, TlsProvider};
    ureq::Agent::config_builder()
        .tls_config(TlsConfig::builder().provider(TlsProvider::NativeTls).build())
        .timeout_global(global_timeout)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_body(Some(Duration::from_secs(30)))
        .build()
        .into()
}

impl Updater {
    pub fn get(&self) -> UpdateState {
        self.state.lock().unwrap().clone()
    }

    /// 后台检查（不阻塞界面）
    pub fn check(&self, ctx: eframe::egui::Context) {
        {
            let mut st = self.state.lock().unwrap();
            if matches!(*st, UpdateState::Checking | UpdateState::Downloading { .. } | UpdateState::Launched) {
                return;
            }
            *st = UpdateState::Checking;
        }
        let st = self.state.clone();
        let accel = self.accel.load(Ordering::Relaxed);
        std::thread::spawn(move || {
            let r = fetch_latest(accel).map(|rel| {
                if is_newer(&rel.tag, VERSION) {
                    UpdateState::Available { version: rel.tag.trim_start_matches('v').to_string(), url: rel.page, setup: rel.setup }
                } else {
                    UpdateState::UpToDate
                }
            });
            *st.lock().unwrap() = r.unwrap_or_else(UpdateState::Failed);
            ctx.request_repaint();
        });
    }

    /// 当前状态能否一键更新（只有 MSIX 安装版、且新版带了安装程序）
    pub fn can_auto_install(&self) -> bool {
        matches!(self.get(), UpdateState::Available { setup: Some(_), .. }) && is_packaged()
    }

    /// 一键更新：下载安装程序 → 校验 → 运行。成功后状态变为 Launched，界面负责关闭窗口。
    pub fn install(&self, ctx: eframe::egui::Context) {
        let UpdateState::Available { version, setup: Some(asset), .. } = self.get() else { return };
        *self.state.lock().unwrap() = UpdateState::Downloading { version: version.clone(), percent: 0 };
        let st = self.state.clone();
        let urls = download_urls(&asset, self.accel.load(Ordering::Relaxed));
        std::thread::spawn(move || {
            let progress = |p: u8| {
                *st.lock().unwrap() = UpdateState::Downloading { version: version.clone(), percent: p };
                ctx.request_repaint();
            };
            // 一个地址不行（连不上、下载中断、校验不对）换下一个
            let mut r = Err("没有可用的下载地址".to_string());
            for url in &urls {
                r = download_and_run(url, &asset, &version, &progress);
                if r.is_ok() {
                    break;
                }
            }
            *st.lock().unwrap() = match r {
                Ok(()) => UpdateState::Launched,
                Err(e) => UpdateState::Failed(e),
            };
            ctx.request_repaint();
        });
    }

    pub fn releases_page() -> String {
        format!("https://github.com/{REPO}/releases")
    }
}

struct Release {
    tag: String,
    page: String,
    setup: Option<SetupAsset>,
}

fn get_text(url: &str) -> Result<String, String> {
    agent(Some(Duration::from_secs(8)))
        .get(url)
        .header("User-Agent", "VOC-update-check")
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| e.to_string())?
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())
}

fn fetch_latest(accel: bool) -> Result<Release, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let err = match get_text(&url).and_then(|b| parse_release(&b)) {
        Ok(r) => return Ok(r),
        Err(e) => e,
    };
    if accel {
        // 官方接口连不上：经镜像问版本号。版本信息没法核对，只提示、不给一键安装
        for m in MIRRORS {
            if let Ok(mut r) = get_text(&format!("{m}{url}")).and_then(|b| parse_release(&b)) {
                r.setup = None;
                return Ok(r);
            }
        }
    }
    Err(format!("检查更新失败：{err}"))
}

fn parse_release(body: &str) -> Result<Release, String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
    let tag = v["tag_name"].as_str().ok_or("没有找到版本号")?.to_string();
    let page = v["html_url"].as_str().map(String::from).unwrap_or_else(Updater::releases_page);
    let setup = v["assets"].as_array().and_then(|a| {
        a.iter().find(|x| x["name"].as_str() == Some(SETUP_ASSET)).and_then(|x| {
            let url = x["browser_download_url"].as_str()?;
            // 只接受 GitHub 自己的下载地址
            if !url.starts_with(&format!("https://github.com/{REPO}/releases/download/")) {
                return None;
            }
            Some(SetupAsset {
                url: url.to_string(),
                sha256: x["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).map(|d| d.to_ascii_lowercase()),
                size: x["size"].as_u64().unwrap_or(0),
            })
        })
    });
    Ok(Release { tag, page, setup })
}

fn download_and_run(url: &str, asset: &SetupAsset, version: &str, progress: &impl Fn(u8)) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let mut resp = agent(None)
        .get(url)
        .header("User-Agent", "VOC-update-check")
        .call()
        .map_err(|e| format!("下载失败：{e}"))?;
    let total = resp.body().content_length().unwrap_or(asset.size).max(1);
    let mut reader = resp.body_mut().with_config().limit(512 << 20).reader();

    let path = std::env::temp_dir().join(format!("VOC-Setup-{version}.exe"));
    let mut file = std::fs::File::create(&path).map_err(|e| format!("无法写入临时文件：{e}"))?;
    let mut hasher = Sha256::new();
    let (mut buf, mut done, mut last) = (vec![0u8; 64 * 1024], 0u64, 0u8);
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("下载中断：{e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(|e| format!("无法写入临时文件：{e}"))?;
        done += n as u64;
        let p = (done * 100 / total).min(100) as u8;
        if p != last {
            last = p;
            progress(p);
        }
    }
    drop(file);
    let got = hex(&hasher.finalize());
    if let Some(want) = &asset.sha256 {
        if &got != want {
            let _ = std::fs::remove_file(&path);
            return Err("下载的安装程序校验失败（SHA-256 不一致），已删除。请稍后重试或到 GitHub 手动下载。".into());
        }
    }
    std::process::Command::new(&path).arg("--update").spawn().map_err(|e| format!("无法启动安装程序：{e}"))?;
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 是否以 MSIX 安装版运行（绿色版返回 false）
#[cfg(windows)]
pub fn is_packaged() -> bool {
    use windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
    let mut len = 0u32;
    // 没有包身份时返回 APPMODEL_ERROR_NO_PACKAGE (15700)；有身份时因为缓冲区为空返回 ERROR_INSUFFICIENT_BUFFER (122)
    unsafe { GetCurrentPackageFullName(&mut len, None).0 != 15700 }
}

#[cfg(not(windows))]
pub fn is_packaged() -> bool {
    false
}

fn parse(v: &str) -> Vec<u64> {
    v.trim().trim_start_matches('v').split(['.', '-']).map(|p| p.parse().unwrap_or(0)).take(3).collect()
}

/// remote 是否比 local 新（只比较数字部分 主.次.补丁）
pub fn is_newer(remote: &str, local: &str) -> bool {
    let (mut r, mut l) = (parse(remote), parse(local));
    r.resize(3, 0);
    l.resize(3, 0);
    r > l
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(is_newer("v0.3.0", "0.2.0"));
        assert!(is_newer("v0.2.10", "0.2.9"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("v0.2.0", "0.2.0"));
        assert!(!is_newer("v0.1.9", "0.2.0"));
    }

    #[test]
    fn release_parsing() {
        let ok = format!(
            r#"{{"tag_name":"v9.0.0","html_url":"https://github.com/{REPO}/releases/tag/v9.0.0","assets":[
                {{"name":"VOC.cer","browser_download_url":"https://github.com/{REPO}/releases/download/v9.0.0/VOC.cer","size":1}},
                {{"name":"VOC-Setup.exe","browser_download_url":"https://github.com/{REPO}/releases/download/v9.0.0/VOC-Setup.exe","size":123,"digest":"sha256:ABCD"}}]}}"#
        );
        let r = parse_release(&ok).unwrap();
        assert_eq!(r.tag, "v9.0.0");
        let s = r.setup.unwrap();
        assert_eq!(s.size, 123);
        assert_eq!(s.sha256.as_deref(), Some("abcd"));

        // 其它域名的下载地址不接受
        let bad = r#"{"tag_name":"v9.0.0","assets":[{"name":"VOC-Setup.exe","browser_download_url":"https://evil.example/VOC-Setup.exe"}]}"#;
        assert!(parse_release(bad).unwrap().setup.is_none());
        // 没有安装程序的老 Release
        assert!(parse_release(r#"{"tag_name":"v0.2.0","assets":[]}"#).unwrap().setup.is_none());
    }

    #[test]
    fn accelerated_download_order() {
        let url = format!("https://github.com/{REPO}/releases/download/v9.0.0/VOC-Setup.exe");
        let with_hash = SetupAsset { url: url.clone(), sha256: Some("ab".into()), size: 1 };
        let v = download_urls(&with_hash, true);
        assert_eq!(v.len(), MIRRORS.len() + 1);
        assert_eq!(v[0], format!("{}{url}", MIRRORS[0]));
        assert_eq!(v.last().unwrap(), &url, "GitHub itself is the last resort");
        // 不加速、或没有 SHA-256（镜像给的文件没法核对）：只直连
        assert_eq!(download_urls(&with_hash, false), vec![url.clone()]);
        let no_hash = SetupAsset { sha256: None, ..with_hash };
        assert_eq!(download_urls(&no_hash, true), vec![url]);
        assert!(accel_link(PORTABLE_ASSET).ends_with("/releases/latest/download/VOC-portable.zip"));
    }

    #[test]
    fn not_packaged_in_tests() {
        assert!(!is_packaged());
    }

    /// 真实访问 GitHub：cargo test -p voc-client live_fetch -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_fetch() {
        let r = fetch_latest(false).unwrap();
        println!("latest = {} {} setup = {:?}", r.tag, r.page, r.setup);
        assert!(r.tag.starts_with('v'));
    }
}
