#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod aec;
mod app;
mod applog;
mod audio;
mod autostart;
mod denoise;
mod glass;
mod speedtest;
mod hotkey;
mod icons;
mod images;
mod loopback;
mod streams;
mod e2e;
mod mf;
mod net;
mod netcheck;
mod notify;
mod nv12view;
mod opusx;
mod pathcache;
mod portmap;
mod secret;
mod theme;
mod traffic;
mod tray;
mod update;
mod vrecv;
mod wasapi_ll;
mod wifi;
mod video;
mod voice;

use eframe::egui;
use std::sync::Arc;

/// egui 自带字体不含中文，这里从系统字体里加载一个作为后备。
pub fn setup_fonts(ctx: &egui::Context) {
    let candidates = [
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\msyh.ttf",
        "C:\\Windows\\Fonts\\simhei.ttf",
        "C:\\Windows\\Fonts\\simsun.ttc",
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    ];
    let Some(bytes) = candidates.iter().find_map(|p| std::fs::read(p).ok()) else {
        eprintln!("未找到中文字体，中文可能显示为方块");
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("cjk".into(), Arc::new(egui::FontData::from_owned(bytes)));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("cjk".into());
    }
    ctx.set_fonts(fonts);
}

/// 窗口 / 任务栏 / 托盘图标（与 MSIX 包里的图标相同）
pub fn window_icon() -> egui::IconData {
    let bytes: &[u8] = include_bytes!("../../../deploy/windows/Assets/icon-256.png");
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let Ok(mut reader) = dec.read_info() else { return egui::IconData::default() };
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(256 * 256 * 4)];
    let Ok(info) = reader.next_frame(&mut buf) else { return egui::IconData::default() };
    buf.truncate(info.buffer_size());
    egui::IconData { rgba: buf, width: info.width, height: info.height }
}

fn main() -> eframe::Result {
    applog::init();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("VOC")
        .with_icon(window_icon())
        .with_inner_size([1180.0, 760.0])
        .with_min_inner_size([760.0, 480.0]);
    // VOC_TEST_OFFSCREEN=1：测试客户端放到屏幕外、不抢焦点（不打扰正在用电脑的人）
    if std::env::var("VOC_SHOT").is_ok() || std::env::var_os("VOC_TEST_OFFSCREEN").is_some() {
        // 截图模式：窗口放到屏幕外、不抢焦点
        viewport = viewport.with_position([-4000.0, -4000.0]).with_active(false);
    } else if autostart::launched_at_startup() {
        // 开机自启：直接缩到托盘
        viewport = viewport.with_visible(false);
    }
    // 毛玻璃（第 59 项）：窗口要创建时就透明，云母背景才透得出来
    // VOC_SHOT_SIZE=宽x高：截图时的窗口大小（测试窄 / 矮窗口的排版）
    if let Some((w, h)) = std::env::var("VOC_SHOT_SIZE").ok().and_then(|v| {
        let (w, h) = v.split_once('x')?;
        Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?))
    }) {
        viewport = viewport.with_inner_size([w, h]);
    }
    if glass::startup_enabled() {
        viewport = viewport.with_transparent(true);
    }
    // 截图指定了窗口大小时不用上次记住的窗口大小
    let persist_window = std::env::var_os("VOC_SHOT_SIZE").is_none();
    let options = eframe::NativeOptions { viewport, persist_window, ..Default::default() };
    eframe::run_native("voc", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
