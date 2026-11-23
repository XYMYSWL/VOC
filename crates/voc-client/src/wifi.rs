//! Wi-Fi 低延迟模式（第 37-1 项）：在语音频道里时打开无线网卡的「媒体流模式」
//! （WlanSetInterface / wlan_intf_opcode_media_streaming_mode），网卡不进省电、不在信道间扫描，
//! 减少 Wi-Fi 下的抖动和偶发几十毫秒的卡顿。
//!
//! 只对 VOC 打开的这个句柄生效：离开频道 / 退出 VOC（句柄关掉）时 Windows 自动恢复，不改系统设置。
//! 有线网络、没有无线网卡、网卡不支持时什么都不做。

use std::sync::atomic::{AtomicU8, Ordering};

/// 当前状态（网络自检 / 诊断显示）：0 没开（不在频道 / 没有已连接的 Wi-Fi），1 已打开，2 网卡不支持
pub static STATE: AtomicU8 = AtomicU8::new(0);

pub fn state_text() -> &'static str {
    match STATE.load(Ordering::Relaxed) {
        1 => "已打开（语音期间）",
        2 => "网卡不支持",
        _ => "未使用（不在语音频道，或没有连着的 Wi-Fi）",
    }
}

/// 持有期间保持低延迟模式；Drop 时关掉句柄（Windows 随即恢复）
pub struct LowLatency {
    #[cfg(windows)]
    handle: windows::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl LowLatency {
    /// 给所有已连接的无线网卡打开媒体流模式；一个都没开成返回 None
    pub fn enable() -> Option<Self> {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::NetworkManagement::WiFi::*;
        unsafe {
            let (mut ver, mut h) = (0u32, HANDLE::default());
            if WlanOpenHandle(2, None, &mut ver, &mut h) != 0 {
                return None; // 没有 WLAN 服务（台式机没无线网卡）
            }
            let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
            if WlanEnumInterfaces(h, None, &mut list) != 0 || list.is_null() {
                WlanCloseHandle(h, None);
                return None;
            }
            let n = (*list).dwNumberOfItems as usize;
            let items = std::slice::from_raw_parts((*list).InterfaceInfo.as_ptr(), n);
            let (mut connected, mut ok) = (0, 0);
            for it in items.iter().filter(|i| i.isState == wlan_interface_state_connected) {
                connected += 1;
                let on: u32 = 1;
                if WlanSetInterface(h, &it.InterfaceGuid, wlan_intf_opcode_media_streaming_mode, 4, &on as *const u32 as *const _, None) == 0 {
                    ok += 1;
                }
            }
            WlanFreeMemory(list as *const _);
            if ok == 0 {
                WlanCloseHandle(h, None);
                STATE.store(if connected > 0 { 2 } else { 0 }, Ordering::Relaxed);
                return None;
            }
            STATE.store(1, Ordering::Relaxed);
            crate::applog::log(format!("网络：Wi-Fi 低延迟模式已打开（{ok} 块无线网卡）"));
            Some(Self { handle: h })
        }
    }
}

#[cfg(windows)]
impl Drop for LowLatency {
    fn drop(&mut self) {
        unsafe {
            windows::Win32::NetworkManagement::WiFi::WlanCloseHandle(self.handle, None);
        }
        STATE.store(0, Ordering::Relaxed);
        crate::applog::log("网络：Wi-Fi 低延迟模式已关闭");
    }
}

#[cfg(not(windows))]
impl LowLatency {
    pub fn enable() -> Option<Self> {
        None
    }
}
