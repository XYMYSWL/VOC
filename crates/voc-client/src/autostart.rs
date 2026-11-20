//! 开机自启。
//! - 安装版（MSIX）：用清单里声明的启动任务（任务管理器「启动应用」里也能看到、能关掉）。
//! - 绿色版：写当前用户的「Run」注册表项，带 `--tray` 参数。
//! 开机启动时直接缩到托盘。

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutoStart {
    On,
    Off,
    /// 用户在任务管理器里禁用了，程序不能替他打开
    BlockedByUser,
    Unsupported(String),
}

/// 清单里启动任务的 TaskId（deploy/windows/AppxManifest.xml）
#[cfg(windows)]
const TASK_ID: &str = "VOCStartup";
#[cfg(windows)]
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
#[cfg(windows)]
const RUN_VALUE: &str = "VOC";

#[cfg(windows)]
fn task() -> Result<windows::ApplicationModel::StartupTask, String> {
    windows::ApplicationModel::StartupTask::GetAsync(&windows::core::HSTRING::from(TASK_ID))
        .and_then(|op| op.join())
        .map_err(|e| e.message().to_string())
}

#[cfg(windows)]
fn from_task_state(s: windows::ApplicationModel::StartupTaskState) -> AutoStart {
    use windows::ApplicationModel::StartupTaskState as S;
    match s {
        S::Enabled | S::EnabledByPolicy => AutoStart::On,
        S::DisabledByUser | S::DisabledByPolicy => AutoStart::BlockedByUser,
        _ => AutoStart::Off,
    }
}

#[cfg(windows)]
fn run_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\" --tray", exe.display()))
}

#[cfg(windows)]
fn read_run_value() -> Option<String> {
    use windows::core::HSTRING;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    let mut buf = vec![0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(RUN_KEY),
            &HSTRING::from(RUN_VALUE),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut len),
        )
    };
    if r.is_err() {
        return None;
    }
    let n = (len as usize / 2).saturating_sub(1);
    Some(String::from_utf16_lossy(&buf[..n]))
}

/// 当前是否开机自启
#[cfg(windows)]
pub fn state() -> AutoStart {
    if crate::update::is_packaged() {
        match task().and_then(|t| t.State().map_err(|e| e.message().to_string())) {
            Ok(s) => from_task_state(s),
            Err(e) => AutoStart::Unsupported(e),
        }
    } else if read_run_value().is_some_and(|v| Some(v) == run_command()) {
        AutoStart::On
    } else {
        AutoStart::Off
    }
}

/// 打开 / 关闭开机自启，返回新状态
#[cfg(windows)]
pub fn set(on: bool) -> AutoStart {
    use windows::core::HSTRING;
    use windows::Win32::System::Registry::{RegDeleteKeyValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};
    if crate::update::is_packaged() {
        let r = task().and_then(|t| {
            if on {
                t.RequestEnableAsync().and_then(|op| op.join()).map(from_task_state)
            } else {
                t.Disable().map(|_| AutoStart::Off)
            }
            .map_err(|e| e.message().to_string())
        });
        return r.unwrap_or_else(AutoStart::Unsupported);
    }
    unsafe {
        if on {
            let Some(cmd) = run_command() else { return AutoStart::Unsupported("找不到程序路径".into()) };
            let data: Vec<u16> = cmd.encode_utf16().chain(Some(0)).collect();
            let r = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(RUN_KEY),
                &HSTRING::from(RUN_VALUE),
                REG_SZ.0,
                Some(data.as_ptr() as *const _),
                (data.len() * 2) as u32,
            );
            if r.is_err() {
                return AutoStart::Unsupported(format!("写注册表失败（{}）", r.0));
            }
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, &HSTRING::from(RUN_KEY), &HSTRING::from(RUN_VALUE));
        }
    }
    state()
}

/// 这次是不是开机自动启动的（是的话直接缩到托盘）
#[cfg(windows)]
pub fn launched_at_startup() -> bool {
    if std::env::args().any(|a| a == "--tray") {
        return true;
    }
    if crate::update::is_packaged() {
        use windows::ApplicationModel::Activation::ActivationKind;
        if let Ok(args) = windows::ApplicationModel::AppInstance::GetActivatedEventArgs() {
            return args.Kind().is_ok_and(|k| k == ActivationKind::StartupTask);
        }
    }
    false
}

#[cfg(not(windows))]
pub fn state() -> AutoStart {
    AutoStart::Unsupported("只支持 Windows".into())
}
#[cfg(not(windows))]
pub fn set(_: bool) -> AutoStart {
    state()
}
#[cfg(not(windows))]
pub fn launched_at_startup() -> bool {
    false
}
