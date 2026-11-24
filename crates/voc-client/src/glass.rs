//! 毛玻璃效果（第 59 项，设置 → 外观，默认关）：
//! ① 窗口背景用 Windows 11（22H2 及以后）的云母：窗口要在创建时就是透明的，所以开关存成一个标记文件，重开 VOC 后生效；
//! ② 弹出设置、共享这类窗口时，后面的界面变成模糊的磨砂玻璃：弹框出现前先截一帧，用 CPU 缩小 + 模糊一次，贴在弹框下面。
//!    只在弹框打开那一刻算一次，不持续占显卡；弹框开着时后面的内容不会更新（关掉弹框就恢复）。

use eframe::egui::{self, Color32, ColorImage};
use std::path::PathBuf;

/// Windows 11 22H2（内部版本 22621）及以后才有 DWMWA_SYSTEMBACKDROP_TYPE
pub fn mica_supported() -> bool {
    static S: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *S.get_or_init(|| build_number().is_some_and(|b| b >= 22621))
}

#[cfg(windows)]
fn build_number() -> Option<u32> {
    use windows::core::HSTRING;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let mut buf = [0u16; 32];
    let mut len = (buf.len() * 2) as u32;
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &HSTRING::from(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
            &HSTRING::from("CurrentBuildNumber"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut len),
        )
        .ok()
        .ok()?;
    }
    let n = (len as usize / 2).saturating_sub(1).min(buf.len());
    String::from_utf16_lossy(&buf[..n]).trim().parse().ok()
}

#[cfg(not(windows))]
fn build_number() -> Option<u32> {
    None
}

fn flag_path() -> Option<PathBuf> {
    crate::applog::data_dir().map(|d| d.join("glass.on"))
}

/// 这次启动要不要做透明窗口 + 云母（开关打开过、系统支持）。
/// 第 76 项：先停用。透明窗口用默认的显卡接口（Vulkan）画时，拖动 / 放大窗口后有一块原来窗口大小的亮灰色方块，
/// 重设云母、扩展边框都没用；换成 DX12 不出灰块，但 DX12 不支持透明，云母本来就透不出来。所以毛玻璃只留「弹框后面模糊」。
/// VOC_TEST_MICA=1 时照旧打开（以后找到修法时测试用）
pub fn startup_enabled() -> bool {
    std::env::var_os("VOC_TEST_MICA").is_some()
        && std::env::var_os("VOC_SHOT").is_none()
        && mica_supported()
        && flag_path().is_some_and(|p| p.exists())
}

/// 开关改了：记下来，下次启动用
pub fn remember(on: bool) {
    let Some(p) = flag_path() else { return };
    if on {
        let _ = std::fs::write(p, b"1");
    } else {
        let _ = std::fs::remove_file(p);
    }
}

/// 给窗口打开云母背景。dark：深色界面用深色云母（不设的话系统按浅色画，透过半透明的深色面板是一片发灰）。
/// 第 76 项：只设背景类型时，云母只铺在窗口创建时那么大的地方，拖动 / 放大后有一块亮灰色的方块；
/// 按微软的用法把边框扩展到整个窗口（-1 边距），并在窗口移动、改大小后重新设一次（见 ui.rs）
#[cfg(windows)]
pub fn apply_mica(hwnd: isize, dark: bool) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE};
    use windows::Win32::UI::Controls::MARGINS;
    let h = HWND(hwnd as *mut core::ffi::c_void);
    let kind: i32 = 2; // DWMSBT_MAINWINDOW = 云母
    let dark: i32 = dark as i32;
    let m = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
    unsafe {
        let _ = DwmSetWindowAttribute(h, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const i32 as *const _, 4);
        let _ = DwmExtendFrameIntoClientArea(h, &m);
        DwmSetWindowAttribute(h, DWMWA_SYSTEMBACKDROP_TYPE, &kind as *const i32 as *const _, 4).is_ok()
    }
}

#[cfg(not(windows))]
pub fn apply_mica(_hwnd: isize, _dark: bool) -> bool {
    false
}

/// 窗口句柄（eframe 的窗口）
pub fn hwnd_of(frame: &eframe::Frame) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match frame.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

/// 截图 → 磨砂玻璃背景：缩小到 1/4 后做三遍盒式模糊（近似高斯），贴图时拉大，自然就是柔和的模糊
pub fn blur(img: &ColorImage) -> ColorImage {
    const K: usize = 4;
    let [w, h] = img.size;
    let (sw, sh) = ((w / K).max(1), (h / K).max(1));
    let mut px: Vec<[f32; 3]> = vec![[0.0; 3]; sw * sh];
    for y in 0..sh {
        for x in 0..sw {
            let mut acc = [0.0f32; 3];
            let mut n = 0.0;
            for dy in 0..K {
                for dx in 0..K {
                    let (xx, yy) = (x * K + dx, y * K + dy);
                    if xx < w && yy < h {
                        let c = img.pixels[yy * w + xx];
                        acc[0] += c.r() as f32;
                        acc[1] += c.g() as f32;
                        acc[2] += c.b() as f32;
                        n += 1.0;
                    }
                }
            }
            px[y * sw + x] = [acc[0] / n, acc[1] / n, acc[2] / n];
        }
    }
    for _ in 0..3 {
        px = box_pass(&px, sw, sh, 5, true);
        px = box_pass(&px, sw, sh, 5, false);
    }
    ColorImage::new([sw, sh], px.iter().map(|c| Color32::from_rgb(c[0] as u8, c[1] as u8, c[2] as u8)).collect())
}

fn box_pass(src: &[[f32; 3]], w: usize, h: usize, r: usize, horiz: bool) -> Vec<[f32; 3]> {
    let mut out = vec![[0.0; 3]; src.len()];
    let (outer, inner) = if horiz { (h, w) } else { (w, h) };
    let idx = |o: usize, i: usize| if horiz { o * w + i } else { i * w + o };
    for o in 0..outer {
        for i in 0..inner {
            let lo = i.saturating_sub(r);
            let hi = (i + r).min(inner - 1);
            let mut acc = [0.0f32; 3];
            for j in lo..=hi {
                let c = src[idx(o, j)];
                acc[0] += c[0];
                acc[1] += c[1];
                acc[2] += c[2];
            }
            let n = (hi - lo + 1) as f32;
            out[idx(o, i)] = [acc[0] / n, acc[1] / n, acc[2] / n];
        }
    }
    out
}

/// 弹框后面的磨砂玻璃
pub struct Backdrop {
    pub tex: egui::TextureHandle,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blur_smooths_and_downscales() {
        let mut img = ColorImage::new([64, 32], vec![Color32::BLACK; 64 * 32]);
        for y in 0..32 {
            for x in 32..64 {
                img.pixels[y * 64 + x] = Color32::WHITE;
            }
        }
        let b = blur(&img);
        assert_eq!(b.size, [16, 8]);
        let mid = b.pixels[4 * 16 + 8].r();
        assert!(mid > 40 && mid < 215, "edge is blurred: {mid}");
        assert!(b.pixels[4 * 16].r() < 60 && b.pixels[4 * 16 + 15].r() > 195);
    }
}
