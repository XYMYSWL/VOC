#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod aec;
mod app;
mod audio;
mod autostart;
mod denoise;
mod images;
mod loopback;
mod streams;
mod e2e;
mod mf;
mod net;
mod theme;
mod tray;
mod update;
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
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("VOC")
        .with_icon(window_icon())
        .with_inner_size([1180.0, 760.0])
        .with_min_inner_size([760.0, 480.0]);
    if std::env::var("VOC_SHOT").is_ok() {
        // 截图模式：窗口放到屏幕外、不抢焦点
        viewport = viewport.with_position([-4000.0, -4000.0]).with_active(false);
    } else if autostart::launched_at_startup() {
        // 开机自启：直接缩到托盘
        viewport = viewport.with_visible(false);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("voc", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
