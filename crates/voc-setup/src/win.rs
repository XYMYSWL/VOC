//! Windows 部分：证书库、提权、MSIX 部署、启动应用。

use std::path::Path;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Foundation::Uri;
use windows_future::AsyncStatus;
use windows::Management::Deployment::{DeploymentOptions, DeploymentProgress, PackageManager};
use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED};
use windows::Win32::Security::Cryptography::*;
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows::Win32::UI::Shell::{ShellExecuteExW, ShellExecuteW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNORMAL};
use windows_future::AsyncOperationProgressHandler;

pub const PKG_NAME: &str = "VOC.VoiceChat";
pub const PUBLISHER: &str = "CN=VOC";
const ENC: CERT_QUERY_ENCODING_TYPE = CERT_QUERY_ENCODING_TYPE(X509_ASN_ENCODING.0 | PKCS_7_ASN_ENCODING.0);

/// 打开本机的「受信任的人」证书库
fn open_store(readonly: bool) -> windows::core::Result<HCERTSTORE> {
    let mut flags = CERT_SYSTEM_STORE_LOCAL_MACHINE;
    if readonly {
        flags |= CERT_STORE_READONLY_FLAG.0;
    }
    unsafe {
        CertOpenStore(
            CERT_STORE_PROV_SYSTEM_W,
            CERT_QUERY_ENCODING_TYPE(0),
            None,
            CERT_OPEN_STORE_FLAGS(flags),
            Some(w!("TrustedPeople").as_ptr() as *const _),
        )
    }
}

/// VOC 证书是否已在本机「受信任的人」里
pub fn cert_trusted(cer: &[u8]) -> Result<bool, String> {
    unsafe {
        let store = open_store(true).map_err(|e| format!("打开证书库失败：{}", e.message()))?;
        let ctx = CertCreateCertificateContext(ENC, cer);
        if ctx.is_null() {
            let _ = CertCloseStore(Some(store), 0);
            return Err("内置证书格式不对".into());
        }
        let found = CertFindCertificateInStore(store, ENC, 0, CERT_FIND_EXISTING, Some(ctx as *const _), None);
        let ok = !found.is_null();
        if ok {
            let _ = CertFreeCertificateContext(Some(found));
        }
        let _ = CertFreeCertificateContext(Some(ctx));
        let _ = CertCloseStore(Some(store), 0);
        Ok(ok)
    }
}

/// 在管理员进程里调用：导入证书
pub fn import_cert(cer: &[u8]) -> windows::core::Result<()> {
    unsafe {
        let store = open_store(false)?;
        let r = CertAddEncodedCertificateToStore(Some(store), ENC, cer, CERT_STORE_ADD_REPLACE_EXISTING, None);
        let _ = CertCloseStore(Some(store), 0);
        r
    }
}

/// 弹 UAC，以管理员身份运行自己 --trust-cert，并等它结束
pub fn trust_cert_elevated() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = HSTRING::from(exe.as_os_str());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(exe.as_ptr()),
        lpParameters: w!("--trust-cert"),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe {
        if let Err(e) = ShellExecuteExW(&mut info) {
            return Err(if e.code() == ERROR_CANCELLED.to_hresult() {
                "你取消了管理员授权。第一次安装需要允许一次，用来信任 VOC 的签名证书。".into()
            } else {
                format!("请求管理员权限失败：{}", e.message())
            });
        }
        if info.hProcess.is_invalid() {
            return Err("请求管理员权限失败".into());
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        if code != 0 {
            return Err("导入证书失败".into());
        }
    }
    Ok(())
}

/// 安装 / 升级 MSIX（当前用户），progress 收到 0–100
pub fn add_package(path: &Path, progress: impl Fn(u32) + Send + 'static) -> Result<(), String> {
    let run = || -> windows::core::Result<()> {
        let url = format!("file:///{}", path.to_string_lossy().replace('\\', "/"));
        let uri = Uri::CreateUri(&HSTRING::from(url))?;
        let pm = PackageManager::new()?;
        let op = pm.AddPackageAsync(&uri, None, DeploymentOptions::ForceApplicationShutdown)?;
        op.SetProgress(&AsyncOperationProgressHandler::<_, DeploymentProgress>::new(move |_, p| {
            progress(p.percentage);
            Ok(())
        }))?;
        // join() 在部署失败时不一定返回 Err：要再看操作状态和结果里的扩展错误码
        let result = op.join();
        if op.Status()? == AsyncStatus::Error {
            let code = op.ErrorCode()?;
            let text = result.as_ref().ok().and_then(|r| r.ErrorText().ok()).map(|t| t.to_string()).unwrap_or_default();
            return Err(windows::core::Error::new(code, text));
        }
        let result = result?;
        let code = result.ExtendedErrorCode()?;
        if code.is_err() {
            return Err(windows::core::Error::new(code, result.ErrorText()?.to_string()));
        }
        Ok(())
    };
    run().map_err(|e| {
        let msg = e.message().trim().to_string();
        match e.code().0 as u32 {
            0x800B0109 | 0x800B010A => format!("签名证书不受信任（{msg}）"),
            0x80073D02 => "VOC 正在运行且无法关闭，请先退出 VOC 再重试。".into(),
            0x80073CFB => format!("已经装了同版本但内容不同的 VOC，请先在开始菜单卸载 VOC 再安装。（{msg}）"),
            _ => format!("{msg}（错误码 0x{:08X}）", e.code().0 as u32),
        }
    })
}

/// 打开已安装的 VOC
pub fn launch_voc() {
    let find = || -> windows::core::Result<Option<HSTRING>> {
        let pm = PackageManager::new()?;
        let pkgs = pm.FindPackagesByUserSecurityIdNamePublisher(&HSTRING::new(), &HSTRING::from(PKG_NAME), &HSTRING::from(PUBLISHER))?;
        let it = pkgs.First()?;
        if it.HasCurrent()? {
            return Ok(Some(it.Current()?.Id()?.FamilyName()?));
        }
        Ok(None)
    };
    if let Ok(Some(fam)) = find() {
        let target = HSTRING::from(format!("shell:AppsFolder\\{fam}!VOC"));
        unsafe {
            ShellExecuteW(None, w!("open"), w!("explorer.exe"), &target, PCWSTR::null(), SW_SHOWNORMAL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在「没信任过 VOC 证书」的机器上跑：证书检查应为 false，部署应被 Windows 以证书不受信任拒绝（不会改动系统）。
    /// cargo test -p voc-setup untrusted -- --ignored --nocapture
    #[test]
    #[ignore]
    fn untrusted_machine_rejects_package() {
        assert!(!crate::CER.is_empty() && !crate::MSIX.is_empty(), "先运行 build-msix.ps1");
        assert_eq!(cert_trusted(crate::CER), Ok(false));
        let path = std::env::temp_dir().join("voc-setup-test.msix");
        std::fs::write(&path, crate::MSIX).unwrap();
        let err = add_package(&path, |p| println!("progress {p}")).unwrap_err();
        let _ = std::fs::remove_file(&path);
        println!("{err}");
        assert!(err.contains("签名证书不受信任"), "{err}");
    }
}
