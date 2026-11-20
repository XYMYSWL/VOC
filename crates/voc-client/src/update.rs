//! 检查更新：读 GitHub Releases 的最新版本号，比当前版本新就提示下载。
//! 只发一个匿名的 HTTPS GET（User-Agent 是固定的 "VOC-update-check"），不上传任何本机信息。

use std::sync::{Arc, Mutex};
use std::time::Duration;

/// GitHub 仓库（所有者/仓库名）。编译时可用环境变量 VOC_REPO 覆盖。
pub const REPO: &str = match option_env!("VOC_REPO") {
    Some(r) => r,
    None => "XYMYSWL/VOC",
};
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available { version: String, url: String },
    Failed(String),
}

#[derive(Clone)]
pub struct Updater {
    pub state: Arc<Mutex<UpdateState>>,
}

impl Default for Updater {
    fn default() -> Self {
        Self { state: Arc::new(Mutex::new(UpdateState::Idle)) }
    }
}

impl Updater {
    pub fn get(&self) -> UpdateState {
        self.state.lock().unwrap().clone()
    }

    /// 后台检查（不阻塞界面）
    pub fn check(&self, ctx: eframe::egui::Context) {
        if *self.state.lock().unwrap() == UpdateState::Checking {
            return;
        }
        *self.state.lock().unwrap() = UpdateState::Checking;
        let st = self.state.clone();
        std::thread::spawn(move || {
            let r = fetch_latest().map(|(tag, url)| {
                if is_newer(&tag, VERSION) {
                    UpdateState::Available { version: tag.trim_start_matches('v').to_string(), url }
                } else {
                    UpdateState::UpToDate
                }
            });
            *st.lock().unwrap() = r.unwrap_or_else(UpdateState::Failed);
            ctx.request_repaint();
        });
    }

    pub fn releases_page() -> String {
        format!("https://github.com/{REPO}/releases")
    }
}

fn fetch_latest() -> Result<(String, String), String> {
    use ureq::tls::{TlsConfig, TlsProvider};
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(TlsConfig::builder().provider(TlsProvider::NativeTls).build())
        .timeout_global(Some(Duration::from_secs(8)))
        .build()
        .into();
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = agent
        .get(&url)
        .header("User-Agent", "VOC-update-check")
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("检查更新失败：{e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let tag = v["tag_name"].as_str().ok_or("没有找到版本号")?.to_string();
    let page = v["html_url"].as_str().map(String::from).unwrap_or_else(Updater::releases_page);
    Ok((tag, page))
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
}
