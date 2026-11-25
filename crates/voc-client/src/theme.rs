//! VOC 的界面主题：深色配色、字号、间距、圆角，以及几个通用小组件。

use crate::voice::PeerQuality;
use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Margin, RichText, Sense, Stroke, TextStyle, Vec2};

// ---------------------------------------------------------------- 配色

/// 一套配色（第 59 项：浅色 / 深色两套，颜色取自效果图——靛蓝强调色、绿色说话、红色闭麦 / 离开）
pub struct Palette {
    pub bg_base: Color32,
    pub rail: Color32,
    pub bg_sidebar: Color32,
    pub bg_panel: Color32,
    pub bg_elevated: Color32,
    pub bg_hover: Color32,
    pub bg_active: Color32,
    pub border: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub text_faint: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub accent_soft: Color32,
    pub green: Color32,
    pub green_soft: Color32,
    pub yellow: Color32,
    pub yellow_soft: Color32,
    pub red: Color32,
    pub red_soft: Color32,
    /// 选中的底色（选中按钮、选中文字）：字是正文颜色，要看得清
    pub selection: Color32,
}

const fn c(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub const DARK: Palette = Palette {
    bg_base: c(0x0F1115),
    rail: c(0x0A0C0F),
    bg_sidebar: c(0x14171C),
    bg_panel: c(0x1A1D23),
    bg_elevated: c(0x23272F),
    bg_hover: c(0x2B3039),
    bg_active: c(0x343A46),
    border: c(0x2A2E36),
    text: c(0xE8EAED),
    text_dim: c(0xB4BAC4),
    text_faint: c(0x8C94A1),
    accent: c(0x6D72F0),
    accent_hover: c(0xA5ABFF),
    accent_soft: c(0x25284A),
    green: c(0x4ADE80),
    green_soft: c(0x123222),
    yellow: c(0xFBBF24),
    yellow_soft: c(0x3A2A10),
    red: c(0xF87171),
    red_soft: c(0x3A1717),
    selection: c(0x6D72F0),
};

pub const LIGHT: Palette = Palette {
    bg_base: c(0xF4F5F7),
    rail: c(0xE9EBEF),
    bg_sidebar: c(0xFAFAFB),
    bg_panel: c(0xFFFFFF),
    bg_elevated: c(0xF1F2F5),
    bg_hover: c(0xE8EAEF),
    bg_active: c(0xDFE2EA),
    border: c(0xE3E5E9),
    text: c(0x1F2328),
    text_dim: c(0x4B5260),
    text_faint: c(0x646B78),
    accent: c(0x4F46E5),
    accent_hover: c(0x3730A3),
    accent_soft: c(0xEEF0FF),
    green: c(0x15803D),
    green_soft: c(0xE8F7EE),
    yellow: c(0xB45309),
    yellow_soft: c(0xFEF3E2),
    red: c(0xDC2626),
    red_soft: c(0xFDECEC),
    selection: c(0xC7CBFF),
};

pub const GRAPHITE_LIGHT: Palette = Palette {
    bg_base: c(0xF6F6F7),
    rail: c(0xEDEDEF),
    bg_sidebar: c(0xFAFAFA),
    bg_panel: c(0xFFFFFF),
    bg_elevated: c(0xF2F2F4),
    bg_hover: c(0xEBEBEE),
    bg_active: c(0xE2E2E7),
    border: c(0xE6E6E9),
    text: c(0x18181B),
    text_dim: c(0x52525B),
    text_faint: c(0x71717A),
    accent: c(0x5B5BD6),
    accent_hover: c(0x4646B4),
    accent_soft: c(0xEFEFFC),
    green: c(0x1F8F55),
    green_soft: c(0xE9F6EF),
    yellow: c(0xA16207),
    yellow_soft: c(0xFBF4E3),
    red: c(0xC93B3B),
    red_soft: c(0xFBEDED),
    selection: c(0xD9D9F7),
};
pub const GRAPHITE_DARK: Palette = Palette {
    bg_base: c(0x111113),
    rail: c(0x0C0C0E),
    bg_sidebar: c(0x161618),
    bg_panel: c(0x1B1B1E),
    bg_elevated: c(0x232326),
    bg_hover: c(0x2B2B2F),
    bg_active: c(0x333338),
    border: c(0x2A2A2E),
    text: c(0xECECEE),
    text_dim: c(0xB0B0B8),
    text_faint: c(0x85858F),
    accent: c(0x7171E6),
    accent_hover: c(0xAEAEF8),
    accent_soft: c(0x26263F),
    green: c(0x46C987),
    green_soft: c(0x15291F),
    yellow: c(0xE5B454),
    yellow_soft: c(0x2E2614),
    red: c(0xEE7272),
    red_soft: c(0x351A1C),
    selection: c(0x3A3A6E),
};
pub const STONE_LIGHT: Palette = Palette {
    bg_base: c(0xF5F4F2),
    rail: c(0xECEAE6),
    bg_sidebar: c(0xFAF9F7),
    bg_panel: c(0xFFFFFF),
    bg_elevated: c(0xF1EFEC),
    bg_hover: c(0xE9E6E1),
    bg_active: c(0xDFDBD5),
    border: c(0xE5E2DD),
    text: c(0x1C1917),
    text_dim: c(0x57534E),
    text_faint: c(0x78716C),
    accent: c(0x0F766E),
    accent_hover: c(0x115E59),
    accent_soft: c(0xE6F2F0),
    green: c(0x2F855A),
    green_soft: c(0xEAF4EC),
    yellow: c(0xA16207),
    yellow_soft: c(0xF8F1E0),
    red: c(0xB83A3A),
    red_soft: c(0xF8EBEA),
    selection: c(0xCBE6E1),
};
pub const STONE_DARK: Palette = Palette {
    bg_base: c(0x141312),
    rail: c(0x0E0D0C),
    bg_sidebar: c(0x191817),
    bg_panel: c(0x1E1C1B),
    bg_elevated: c(0x272523),
    bg_hover: c(0x302D2B),
    bg_active: c(0x3A3633),
    border: c(0x2E2B28),
    text: c(0xEEEBE7),
    text_dim: c(0xB9B3AC),
    text_faint: c(0x8E8780),
    accent: c(0x1E9C8E),
    accent_hover: c(0x7FD8CC),
    accent_soft: c(0x16302D),
    green: c(0x5CC98A),
    green_soft: c(0x17291E),
    yellow: c(0xE2B45A),
    yellow_soft: c(0x2F2716),
    red: c(0xE77B73),
    red_soft: c(0x361D1B),
    selection: c(0x1E4A45),
};
pub const SEA_LIGHT: Palette = Palette {
    bg_base: c(0xF4F6F9),
    rail: c(0xE8ECF2),
    bg_sidebar: c(0xF9FAFC),
    bg_panel: c(0xFFFFFF),
    bg_elevated: c(0xEFF2F6),
    bg_hover: c(0xE6EBF1),
    bg_active: c(0xDCE3EC),
    border: c(0xE1E6ED),
    text: c(0x0F172A),
    text_dim: c(0x475569),
    text_faint: c(0x64748B),
    accent: c(0x2563EB),
    accent_hover: c(0x1D4ED8),
    accent_soft: c(0xEAF1FE),
    green: c(0x15803D),
    green_soft: c(0xE8F5EC),
    yellow: c(0xA16207),
    yellow_soft: c(0xFBF3E1),
    red: c(0xC53030),
    red_soft: c(0xFBECEC),
    selection: c(0xCFDEFC),
};
pub const SEA_DARK: Palette = Palette {
    bg_base: c(0x0B0F17),
    rail: c(0x070A10),
    bg_sidebar: c(0x0F1420),
    bg_panel: c(0x131A27),
    bg_elevated: c(0x1A2231),
    bg_hover: c(0x222B3C),
    bg_active: c(0x2A3447),
    border: c(0x1F293A),
    text: c(0xE2E8F0),
    text_dim: c(0xA9B4C4),
    text_faint: c(0x7C889A),
    accent: c(0x3B7BEF),
    accent_hover: c(0x93B8FB),
    accent_soft: c(0x172A4A),
    green: c(0x4ACB8A),
    green_soft: c(0x112A1F),
    yellow: c(0xE6B450),
    yellow_soft: c(0x2B2412),
    red: c(0xEF7373),
    red_soft: c(0x341A1E),
    selection: c(0x1F3E73),
};

/// 配色风格（第 63 项，设置 → 外观）：(名字, 浅色, 深色)
pub const STYLES: [(&str, &Palette, &Palette); 4] = [
    ("石墨", &GRAPHITE_LIGHT, &GRAPHITE_DARK),
    ("暖石", &STONE_LIGHT, &STONE_DARK),
    ("深海", &SEA_LIGHT, &SEA_DARK),
    ("靛蓝（0.6.3）", &LIGHT, &DARK),
];
static STYLE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

static LIGHT_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 窗口背景是云母（第 59 项）：大块底色改半透明，透出系统背景
static TRANSLUCENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn tr(c: Color32) -> Color32 {
    if TRANSLUCENT.load(std::sync::atomic::Ordering::Relaxed) {
        Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), if is_light() { 200 } else { 190 })
    } else {
        c
    }
}

/// 现在用的是不是浅色
pub fn is_light() -> bool {
    LIGHT_ON.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn p() -> &'static Palette {
    let (_, l, d) = STYLES[(STYLE.load(std::sync::atomic::Ordering::Relaxed) as usize).min(STYLES.len() - 1)];
    if is_light() {
        l
    } else {
        d
    }
}

macro_rules! colors {
    ($($name:ident => $field:ident),* $(,)?) => {
        $(
            #[allow(non_snake_case)]
            pub fn $name() -> Color32 {
                p().$field
            }
        )*
    };
}

colors! {
    BG_ELEVATED => bg_elevated, BG_HOVER => bg_hover, BG_ACTIVE => bg_active, BORDER => border, TEXT => text, TEXT_DIM => text_dim,
    TEXT_FAINT => text_faint, ACCENT => accent, ACCENT_HOVER => accent_hover, ACCENT_SOFT => accent_soft,
    GREEN => green, GREEN_SOFT => green_soft, YELLOW => yellow, YELLOW_SOFT => yellow_soft, RED => red, RED_SOFT => red_soft,
}

// 大块底色：云母窗口时半透明
#[allow(non_snake_case)]
pub fn BG_BASE() -> Color32 {
    tr(p().bg_base)
}
#[allow(non_snake_case)]
pub fn RAIL() -> Color32 {
    tr(p().rail)
}
#[allow(non_snake_case)]
pub fn BG_SIDEBAR() -> Color32 {
    tr(p().bg_sidebar)
}
#[allow(non_snake_case)]
pub fn BG_PANEL() -> Color32 {
    tr(p().bg_panel)
}

/// 外观：0 跟随系统，1 浅色，2 深色；style = 配色风格（STYLES 的下标）。每帧调一次，变了才重设样式（第 59 / 63 项）。translucent = 云母窗口
pub fn sync(ctx: &egui::Context, mode: u8, style: u8, translucent: bool) {
    TRANSLUCENT.store(translucent, std::sync::atomic::Ordering::Relaxed);
    let style = style.min(STYLES.len() as u8 - 1);
    let style_changed = STYLE.swap(style, std::sync::atomic::Ordering::Relaxed) != style;
    let light = match mode {
        1 => true,
        2 => false,
        _ => ctx.system_theme().is_some_and(|t| t == egui::Theme::Light),
    };
    static APPLIED: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
    let want = if light { 1 } else { 2 };
    if APPLIED.swap(want, std::sync::atomic::Ordering::Relaxed) != want || style_changed {
        LIGHT_ON.store(light, std::sync::atomic::Ordering::Relaxed);
        apply(ctx);
        // 系统标题栏也跟着换颜色
        ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(if light { egui::SystemTheme::Light } else { egui::SystemTheme::Dark }));
    }
}

/// 应用全局样式
pub fn apply(ctx: &egui::Context) {
    ctx.set_visuals(visuals());
    ctx.all_styles_mut(|s| {
        s.visuals = visuals();
        s.text_styles = [
            (TextStyle::Heading, FontId::new(19.0, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(14.5, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
            (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(13.5, FontFamily::Monospace)),
        ]
        .into();
        s.spacing.item_spacing = Vec2::new(8.0, 6.0);
        s.spacing.button_padding = Vec2::new(12.0, 6.0);
        s.spacing.interact_size.y = 30.0;
        s.spacing.window_margin = Margin::same(16);
        s.spacing.menu_margin = Margin::same(8);
        s.spacing.slider_width = 200.0;
        s.spacing.combo_width = 220.0;
    });
}

fn visuals() -> egui::Visuals {
    let mut v = if is_light() { egui::Visuals::light() } else { egui::Visuals::dark() };
    v.override_text_color = Some(TEXT());
    v.panel_fill = BG_PANEL();
    v.window_fill = p().bg_panel;
    v.extreme_bg_color = BG_BASE();
    v.faint_bg_color = BG_ELEVATED();
    v.code_bg_color = BG_ELEVATED();
    v.hyperlink_color = ACCENT_HOVER();
    v.window_corner_radius = CornerRadius::same(14);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_stroke = Stroke::new(1.0, BORDER());
    let sh = if is_light() { 40 } else { 140 };
    v.window_shadow = egui::epaint::Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(sh) };
    v.popup_shadow = egui::epaint::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(sh * 6 / 7) };
    // 浅色：选中的底色用浅靛蓝（字是深色的，深靛蓝底上看不清）
    v.selection.bg_fill = p().selection;
    v.slider_trailing_fill = true; // 滑条左侧用强调色填充，更容易看清
    v.selection.stroke = Stroke::new(1.0, TEXT());

    let r = CornerRadius::same(8);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = BG_PANEL();
    w.noninteractive.weak_bg_fill = BG_PANEL();
    w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER());
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_DIM());
    w.noninteractive.corner_radius = r;

    w.inactive.bg_fill = BG_ELEVATED();
    w.inactive.weak_bg_fill = BG_ELEVATED();
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT());
    w.inactive.corner_radius = r;

    w.hovered.bg_fill = BG_HOVER();
    w.hovered.weak_bg_fill = BG_HOVER();
    w.hovered.bg_stroke = Stroke::new(1.0, BORDER());
    w.hovered.fg_stroke = Stroke::new(1.0, TEXT());
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;

    w.active.bg_fill = BG_ACTIVE();
    w.active.weak_bg_fill = BG_ACTIVE();
    w.active.bg_stroke = Stroke::new(1.0, ACCENT());
    w.active.fg_stroke = Stroke::new(1.0, TEXT());
    w.active.corner_radius = r;
    w.active.expansion = 0.0;

    w.open.bg_fill = BG_ACTIVE();
    w.open.weak_bg_fill = BG_ACTIVE();
    w.open.corner_radius = r;
    v
}

// ---------------------------------------------------------------- 小组件

/// 圆角小标签
/// 音量滑块按人耳感觉走（第 80 项）：滑块正中 = 100%，右端 = 200%（+6 dB），左半段按平方往下（25% 处约 −12 dB）。
/// 线性滑块的毛病：0～50% 那一小段声音变化很大，100～200% 那一大段听起来差不多
pub fn gain_to_pos(g: f32) -> f32 {
    if g <= 1.0 {
        0.5 * g.max(0.0).sqrt()
    } else {
        0.5 + 0.5 * g.log2().min(1.0)
    }
}

pub fn pos_to_gain(p: f32) -> f32 {
    let p = p.clamp(0.0, 1.0);
    if p <= 0.5 {
        (2.0 * p) * (2.0 * p)
    } else {
        2f32.powf(2.0 * p - 1.0)
    }
}

/// 音量滑块（0～200%，gain 是倍数 0～2）：按人耳感觉走，数值框里写百分比（也能直接输入）；靠近 100% 时吸到 100%。
/// 只是一个控件（不包子区域），放在会换行的一行里也能整个换行
pub fn vol_slider(ui: &mut egui::Ui, gain: &mut f32, text: &str, width: f32) -> egui::Response {
    vol_slider_ex(ui, gain, text, width, true)
}

pub fn vol_slider_ex(ui: &mut egui::Ui, gain: &mut f32, text: &str, width: f32, enabled: bool) -> egui::Response {
    let mut p = gain_to_pos(*gain) as f64;
    let old_w = ui.spacing().slider_width;
    ui.spacing_mut().slider_width = width;
    let mut slider = egui::Slider::new(&mut p, 0.0..=1.0)
        .custom_formatter(|p, _| format!("{:.0}%", pos_to_gain(p as f32) * 100.0))
        .custom_parser(|t| t.trim().trim_end_matches('%').trim().parse::<f64>().ok().map(|g| gain_to_pos((g / 100.0).clamp(0.0, 2.0) as f32) as f64));
    if !text.is_empty() {
        slider = slider.text(text);
    }
    let r = ui.add_enabled(enabled, slider);
    ui.spacing_mut().slider_width = old_w;
    if r.changed() {
        let g = pos_to_gain(p as f32);
        *gain = if (g - 1.0).abs() < 0.04 { 1.0 } else { (g * 100.0).round() / 100.0 };
    }
    let db = if *gain > 0.0 { 20.0 * gain.log10() } else { f32::NEG_INFINITY };
    r.on_hover_text(if db.is_finite() { format!("{:.0}%（{:+.1} dB）", *gain * 100.0, db) } else { "0%（静音）".into() })
}

/// 先量好整块的大小再占位（第 73 项）：一行放不下时整个换到下一行，不会被挤成一个字一行
pub fn pill(ui: &mut egui::Ui, text: impl Into<String>, fg: Color32, bg: Color32) -> egui::Response {
    let galley = egui::WidgetText::from(RichText::new(text.into()).size(12.0).color(fg)).into_galley(
        ui,
        Some(egui::TextWrapMode::Extend),
        f32::INFINITY,
        egui::TextStyle::Body,
    );
    let pad = egui::vec2(8.0, 2.0);
    let size = galley.size() + pad * 2.0 + egui::vec2(2.0, 2.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        p.rect(
            rect,
            CornerRadius::same(6),
            bg,
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(fg.r(), fg.g(), fg.b(), 38)),
            egui::StrokeKind::Inside,
        );
        p.galley(rect.min + pad + egui::vec2(1.0, 1.0), galley, fg);
    }
    resp
}

/// 分区标题（小号、偏灰）
pub fn section(ui: &mut egui::Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(text).size(11.5).strong().color(TEXT_FAINT()));
    ui.add_space(2.0);
}

/// 卡片容器
pub fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(BG_ELEVATED())
        .corner_radius(CornerRadius::same(14))
        .stroke(Stroke::new(1.0, BORDER()))
        .inner_margin(Margin::same(20))
        .show(ui, add)
        .inner
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Normal,
    /// 开着某个「关闭」状态（例如闭麦）：红色
    Danger,
    /// 强调（例如正在共享）
    #[allow(dead_code)]
    Accent,
}

/// 方形图标按钮（线条图标，第 59 项）
pub fn icon_button(ui: &mut egui::Ui, icon: crate::icons::Icon, tooltip: &str, tone: Tone) -> egui::Response {
    let size = Vec2::splat(34.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let hover = resp.hovered();
    let (bg, fg) = match (tone, hover) {
        (Tone::Danger, _) => (RED_SOFT(), RED()),
        (Tone::Accent, _) => (ACCENT_SOFT(), ACCENT_HOVER()),
        (Tone::Normal, true) => (BG_HOVER(), TEXT()),
        (Tone::Normal, false) => (Color32::TRANSPARENT, TEXT_DIM()),
    };
    // 悬停时底色淡入（轻动画）
    let t = ui.ctx().animate_bool_with_time(resp.id, hover || tone != Tone::Normal, 0.12);
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(8), bg.gamma_multiply(t.max(if tone == Tone::Normal { 0.0 } else { 1.0 })));
    crate::icons::paint(p, rect.shrink(8.0), icon, fg);
    resp.on_hover_text(tooltip)
}

/// 控制栏按钮：图标在上、字在下（语音房间底部，第 59 项）
pub fn tool_button(ui: &mut egui::Ui, icon: crate::icons::Icon, label: &str, on: bool, danger: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(if danger { 84.0 } else { 72.0 }, 54.0), Sense::click());
    let hover = resp.hovered();
    let (bg, fg) = if danger {
        (if hover { RED().gamma_multiply(0.85) } else { RED() }, Color32::WHITE)
    } else if on {
        (ACCENT_SOFT(), ACCENT_HOVER())
    } else {
        (if hover { BG_HOVER() } else { BG_ELEVATED() }, TEXT_DIM())
    };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(12), bg);
    let ic = egui::Rect::from_center_size(egui::pos2(rect.center().x, rect.top() + 19.0), Vec2::splat(20.0));
    crate::icons::paint(p, ic, icon, fg);
    p.text(egui::pos2(rect.center().x, rect.bottom() - 13.0), egui::Align2::CENTER_CENTER, label, FontId::proportional(12.0), fg);
    resp
}

/// 标签页按钮：图标 + 字，选中时强调色底（第 59 项）
pub fn tab(ui: &mut egui::Ui, icon: crate::icons::Icon, text: &str, selected: bool, badge: u32) -> egui::Response {
    let text = if badge > 0 { format!("{text} ({badge})") } else { text.to_string() };
    let galley = ui.painter().layout_no_wrap(text, FontId::proportional(13.5), Color32::PLACEHOLDER);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(galley.size().x + 40.0, 30.0), Sense::click());
    let fg = if selected {
        ACCENT_HOVER()
    } else if badge > 0 {
        ACCENT_HOVER()
    } else if resp.hovered() {
        TEXT()
    } else {
        TEXT_DIM()
    };
    let p = ui.painter();
    if selected || resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(8), if selected { ACCENT_SOFT() } else { BG_HOVER() });
    }
    crate::icons::paint(p, egui::Rect::from_center_size(egui::pos2(rect.left() + 18.0, rect.center().y), Vec2::splat(16.0)), icon, fg);
    p.galley(egui::pos2(rect.left() + 30.0, rect.center().y - galley.size().y / 2.0), galley, fg);
    resp
}

/// 文字当图标的方形按钮（少数没有对应线条图标的地方）
pub fn glyph_button(ui: &mut egui::Ui, icon: &str, tooltip: &str, tone: Tone) -> egui::Response {
    let size = Vec2::splat(34.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let (bg, fg) = match (tone, resp.hovered()) {
        (Tone::Danger, _) => (RED_SOFT(), RED()),
        (Tone::Accent, _) => (ACCENT_SOFT(), ACCENT_HOVER()),
        (Tone::Normal, true) => (BG_HOVER(), TEXT()),
        (Tone::Normal, false) => (Color32::TRANSPARENT, TEXT_DIM()),
    };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(8), bg);
    p.text(rect.center(), egui::Align2::CENTER_CENTER, icon, FontId::proportional(16.0), fg);
    resp.on_hover_text(tooltip)
}

/// 主按钮（强调色，可指定宽度）
pub fn primary_button(ui: &mut egui::Ui, text: &str, width: f32, enabled: bool) -> egui::Response {
    let btn = egui::Button::new(RichText::new(text).strong().color(Color32::WHITE))
        .fill(if enabled { ACCENT() } else { BG_ACTIVE() })
        .corner_radius(CornerRadius::same(10))
        .min_size(Vec2::new(width, 38.0));
    ui.add_enabled(enabled, btn)
}

/// 名字 → 稳定的头像颜色
pub fn name_color(name: &str) -> Color32 {
    let h = name.bytes().fold(5381u32, |h, b| h.wrapping_mul(33) ^ b as u32);
    // 降饱和、明度统一的一组颜色（第 63 项：比纯色更沉稳）
    const P: [Color32; 8] = [
        c(0xC0727F),
        c(0x5A9A84),
        c(0xC09A55),
        c(0xC27660),
        c(0x5F86BF),
        c(0x8A7CC0),
        c(0x4C9AA0),
        c(0xA67AAE),
    ];
    P[h as usize % P.len()]
}

/// 圆形头像；说话时外圈绿色光环
pub fn avatar(ui: &mut egui::Ui, name: &str, size: f32, speaking: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let p = ui.painter();
    let c = rect.center();
    let r = size / 2.0;
    if speaking {
        p.circle_filled(c, r, GREEN_SOFT());
        p.circle_stroke(c, r - 1.0, Stroke::new(2.0, GREEN()));
    }
    p.circle_filled(c, r - 3.0, name_color(name));
    let ch: String = name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
    p.text(c, egui::Align2::CENTER_CENTER, ch, FontId::proportional(size * 0.42), Color32::WHITE);
    resp
}

/// 直连状态小标签。`voice_direct`：语音是否正在直连（否则是超出预算走中转）。
pub fn quality_chip(ui: &mut egui::Ui, q: &PeerQuality, voice_direct: bool, video_direct: bool) {
    let detail = format!("延迟 {} ms · 丢包 {}% · 抖动 {} ms", q.rtt_ms, q.loss_pct, q.jitter_ms);
    if q.unstable {
        pill(ui, "⇄ 不稳·中转", YELLOW(), YELLOW_SOFT()).on_hover_text(format!(
            "P2P 直连不稳定（{detail}），已自动改走服务器中转；连续稳定后会自动切回直连"
        ));
    } else if voice_direct {
        let (fg, bg) = if q.rtt_ms < 80 {
            (GREEN(), GREEN_SOFT())
        } else if q.rtt_ms < 200 {
            (YELLOW(), YELLOW_SOFT())
        } else {
            (RED(), RED_SOFT())
        };
        let tip = if video_direct { "P2P 直连：语音和你共享的画面都不经过服务器" } else { "P2P 直连：语音不经过服务器" };
        pill(ui, format!("⇄ {} ms", q.rtt_ms), fg, bg).on_hover_text(format!("{tip}\n{detail}"));
    } else {
        pill(ui, format!("⇄ {} ms·中转", q.rtt_ms), TEXT_FAINT(), BG_ELEVATED())
            .on_hover_text(format!("已打通 P2P，但超出你设置的上行带宽预算，你的语音经服务器中转给他\n{detail}"));
    }
}

/// 按往返延迟选颜色（和直连徽章一致）
fn latency_colors(ms: u32) -> (Color32, Color32) {
    if ms < 80 {
        (GREEN(), GREEN_SOFT())
    } else if ms < 200 {
        (YELLOW(), YELLOW_SOFT())
    } else {
        (RED(), RED_SOFT())
    }
}

/// 观看端这一秒收画面的情况（掉帧原因用）
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ViewRates {
    /// 收到的完整帧 / 显示的帧（每秒）
    pub rx_fps: u32,
    pub shown_fps: u32,
    /// 原始丢包（冗余和补发之前，%）
    pub loss_pct: f32,
    /// 每秒：等不到放弃的帧、靠冗余救回的片、补发收回的片
    pub gaps: u32,
    pub fec: u32,
    pub rtx: u32,
}

/// 掉帧时最主要的原因（第 53-1 项）。帧数比共享者选的低不到 4 帧不算掉帧；info = None：共享者是旧版本，只能看本机这边
pub fn drop_reason(info: Option<&voc_proto::udp::ShareInfo>, r: &ViewRates) -> Option<String> {
    const SLACK: u32 = 4;
    let low = |a: u32, b: u32| a + SLACK <= b;
    if let Some(i) = info {
        let (target, source, cap, sent) = (i.target_fps as u32, i.source_fps as u32, i.capture_fps as u32, i.sent_fps as u32);
        if !low(r.shown_fps, target) {
            return None;
        }
        if source > 0 && low(source, target) {
            return Some(format!("共享者的屏幕 / 游戏本身只有 {source} 帧（选了 {target} 帧）"));
        }
        if low(cap, source.max(1).min(target)) {
            return Some(format!("共享者那边采集只有 {cap} 帧（电脑或显卡太忙）"));
        }
        if low(sent, cap) {
            let static_skip = i.static_skipped as u32;
            let rate_skip = i.rate_skipped as u32;
            return Some(if static_skip >= rate_skip && static_skip > 0 {
                format!("画面变化小，共享者每秒少发 {static_skip} 帧（正常，省流量；一动起来就恢复）")
            } else if rate_skip > 0 || i.congested {
                format!("共享者上行带宽不够，在降码率 / 跳帧（码率降到 {}%，每秒跳过 {rate_skip} 帧）", i.rate_pct)
            } else {
                format!("共享者编码跟不上：只编出 {sent} 帧（每帧 {} ms）", i.encode_ms)
            });
        }
        if low(r.rx_fps, sent) {
            return Some(network_reason(r, sent));
        }
    } else if r.gaps > 0 || r.loss_pct >= 2.0 {
        return Some(network_reason(r, r.rx_fps + r.gaps));
    }
    if low(r.shown_fps, r.rx_fps) {
        return Some(format!("本机解码 / 显示跟不上：收到 {} 帧，只显示了 {} 帧", r.rx_fps, r.shown_fps));
    }
    None
}

fn network_reason(r: &ViewRates, sent: u32) -> String {
    if r.gaps > 0 {
        format!("网络丢包 {:.0}%：每秒有 {} 帧没等到、放弃了（共享者发了 {sent} 帧）", r.loss_pct, r.gaps)
    } else {
        format!("网络丢包 {:.0}% / 抖动：共享者发了 {sent} 帧，收到 {} 帧", r.loss_pct, r.rx_fps)
    }
}

/// 画面线路的说明（第 53-6 项）：为什么画面直连 / 经服务器，语音为什么不一样。
/// route = 我在观看窗口选的画面线路（0 自动 / 1 P2P / 2 服务器）
pub fn video_route_explain(video_direct: Option<bool>, q: Option<&PeerQuality>, route: u8, sharer_p2p_only: bool) -> String {
    let mut t = String::new();
    match q {
        Some(q) => {
            t.push_str(&format!("和他直连：延迟 {} ms · 丢包 {}% · 抖动 {} ms", q.rtt_ms, q.loss_pct, q.jitter_ms));
            if q.unstable {
                t.push_str("\n这条直连被判成「不稳」（丢包 ≥ 15%、延迟 > 350 ms 或抖动 > 80 ms 之一），语音已改走服务器 / 中转，稳定 10 秒后自动切回。");
            }
        }
        None => t.push_str("和他还没打通直连。"),
    }
    t.push('\n');
    match video_direct {
        Some(true) => {
            let why = if route == 1 {
                "你选了画面「P2P」：打通了就直连，不看稳不稳"
            } else if sharer_p2p_only {
                "共享者选了「只走 P2P」：画面不经服务器"
            } else if q.is_some_and(|q| q.unstable) {
                "共享者那边对你设了画面直连（右键名字 → 线路），不看稳不稳"
            } else {
                "自动：直连稳定、对方上行预算够"
            };
            t.push_str(&format!("画面现在走直连：{why}。"));
            if q.is_some_and(|q| q.unstable) {
                t.push_str("\n画面有冗余校验包和补发，比语音扛丢包；直连卡顿、花屏时可以改选「服务器」。");
            }
        }
        Some(false) => {
            let why = if route == 2 {
                "你选了画面「服务器」"
            } else if q.is_none() {
                "和他打不通直连"
            } else if q.is_some_and(|q| q.unstable) {
                "直连不稳"
            } else {
                "共享者的上行预算不够直连发给每个人，或直连比经服务器绕远"
            };
            t.push_str(&format!("画面现在经服务器（或经别的观看者转发）：{why}。"));
        }
        None => t.push_str("最近 2 秒没收到画面。"),
    }
    t
}

/// 我和某人之间三条线路的对比（往返延迟）：经队友中继 / 经服务器 / 直连
pub fn relay_compare(via_name: &str, ms: u32, server_ms: u32, direct: Option<&PeerQuality>) -> String {
    let saved = server_ms.saturating_sub(ms);
    let direct_line = match direct {
        Some(q) if q.unstable => {
            format!("直连：不稳定（{} ms · 丢包 {}% · 抖动 {} ms）", q.rtt_ms, q.loss_pct, q.jitter_ms)
        }
        Some(q) => format!("直连：已打通 {} ms，但超出你的上行预算", q.rtt_ms),
        None => "直连：打不通".to_string(),
    };
    [
        "线路对比（往返延迟，按实测链路估算）".to_string(),
        format!("✔ 经 {via_name} 中转：{ms} ms（当前）"),
        format!("    经服务器中转：{server_ms} ms（慢 {saved} ms）"),
        format!("    {direct_line}"),
        format!("语音仍是端到端加密的，{via_name} 只是原样转交。"),
    ]
    .join("
")
}

/// 「↪ 经 某人 62 ms」徽章，悬停显示线路对比
pub fn relay_chip(ui: &mut egui::Ui, via_name: &str, ms: u32, server_ms: u32, direct: Option<&PeerQuality>) -> egui::Response {
    let (fg, bg) = latency_colors(ms);
    pill(ui, format!("↪ 经 {via_name} {ms} ms"), fg, bg).on_hover_text(relay_compare(via_name, ms, server_ms, direct))
}

/// 和某个成员之间的直连状态（成员名字下面显示）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkState {
    Punch(crate::voice::PunchState),
    /// 我没开 P2P
    MeOff,
    /// 对方没开 P2P
    ThemOff,
    /// 对方 UDP 不通（走 TCP 隧道）
    ThemTcp,
    /// 双方都开了 P2P，等服务器交换地址
    Preparing,
}

/// 打洞状态徽章（已打通的用 quality_chip，不走这里）
pub fn link_state_chip(ui: &mut egui::Ui, st: LinkState) -> egui::Response {
    use crate::voice::PunchState::*;
    let fast = crate::voice::PUNCH_FAST_SECS;
    let (text, tip, fg, bg) = match st {
        LinkState::Punch(WaitingKeys) | LinkState::Preparing => (
            "⇄ 准备直连".to_string(),
            "正在交换端到端密钥和地址，马上开始打洞。现在语音走服务器。".to_string(),
            TEXT_FAINT(),
            BG_ELEVATED(),
        ),
        LinkState::Punch(Punching(s)) => (
            format!("⇄ 打洞中 {s}s"),
            format!("正在和他建立 P2P 直连（最多快速尝试 {fast} 秒）。打通前语音走服务器。"),
            YELLOW(),
            YELLOW_SOFT(),
        ),
        LinkState::Punch(Failed(s)) => (
            "⇄ 打不通·走服务器".to_string(),
            format!(
                "已经尝试 {s} 秒还没打通（通常是两边的路由器都比较严格），语音走服务器；有合适的队友时会改由队友中继。\n仍会每 2 秒重试一次。"
            ),
            TEXT_FAINT(),
            BG_ELEVATED(),
        ),
        LinkState::Punch(Connected(_)) => ("⇄ 已打通".to_string(), String::new(), GREEN(), GREEN_SOFT()),
        LinkState::MeOff => (
            "⇄ 你未开直连".to_string(),
            "你关闭了 P2P 直连，语音走服务器。可以在 设置 → 网络 打开。".to_string(),
            TEXT_FAINT(),
            BG_ELEVATED(),
        ),
        LinkState::ThemOff => ("⇄ 对方未开直连".to_string(), "对方关闭了 P2P 直连，语音走服务器。".to_string(), TEXT_FAINT(), BG_ELEVATED()),
        LinkState::ThemTcp => (
            "⇄ 对方 UDP 不通".to_string(),
            "对方的网络封锁了 UDP，只能经服务器的加密 TCP 通道，无法直连，延迟会高一些。".to_string(),
            TEXT_FAINT(),
            BG_ELEVATED(),
        ),
    };
    let r = pill(ui, text, fg, bg);
    if tip.is_empty() {
        r
    } else {
        r.on_hover_text(tip)
    }
}

/// 分段选择（像一排胶囊按钮）
pub fn segmented<T: PartialEq + Copy>(ui: &mut egui::Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let mut changed = false;
    egui::Frame::new().fill(BG_BASE()).corner_radius(CornerRadius::same(10)).inner_margin(Margin::same(3)).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for (v, label) in options {
                let sel = *value == *v;
                let btn = egui::Button::new(RichText::new(*label).color(if sel { Color32::WHITE } else { TEXT_DIM() }))
                    .fill(if sel { ACCENT() } else { Color32::TRANSPARENT })
                    .corner_radius(CornerRadius::same(8))
                    .min_size(Vec2::new(64.0, 28.0));
                if ui.add(btn).clicked() && !sel {
                    *value = *v;
                    changed = true;
                }
            }
        });
    });
    changed
}

/// Unix 毫秒 → 电脑本地时区的「时:分」
pub fn fmt_time(ts_ms: u64) -> String {
    let t = crate::applog::local_time(ts_ms);
    format!("{:02}:{:02}", t.3, t.4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_reasons() {
        use voc_proto::udp::ShareInfo;
        let ok = ShareInfo { target_fps: 60, source_fps: 60, capture_fps: 60, sent_fps: 60, kbps: 8000, rate_pct: 100, ..Default::default() };
        let r = |rx, shown, loss, gaps| ViewRates { rx_fps: rx, shown_fps: shown, loss_pct: loss, gaps, fec: 0, rtx: 0 };
        assert_eq!(drop_reason(Some(&ok), &r(60, 59, 0.0, 0)), None);
        let game = ShareInfo { source_fps: 48, capture_fps: 48, sent_fps: 48, ..ok };
        assert!(drop_reason(Some(&game), &r(48, 48, 0.0, 0)).unwrap().contains("本身只有 48 帧"));
        let busy = ShareInfo { capture_fps: 45, sent_fps: 45, ..ok };
        assert!(drop_reason(Some(&busy), &r(45, 45, 0.0, 0)).unwrap().contains("采集只有 45 帧"));
        let still = ShareInfo { sent_fps: 30, static_skipped: 30, ..ok };
        assert!(drop_reason(Some(&still), &r(30, 30, 0.0, 0)).unwrap().contains("画面变化小"));
        let cong = ShareInfo { sent_fps: 40, rate_skipped: 20, congested: true, rate_pct: 60, ..ok };
        assert!(drop_reason(Some(&cong), &r(40, 40, 0.0, 0)).unwrap().contains("上行带宽不够"));
        let slow = ShareInfo { sent_fps: 40, encode_ms: 25, ..ok };
        assert!(drop_reason(Some(&slow), &r(40, 40, 0.0, 0)).unwrap().contains("编码跟不上"));
        assert!(drop_reason(Some(&ok), &r(50, 50, 6.0, 10)).unwrap().contains("网络丢包 6%"));
        assert!(drop_reason(Some(&ok), &r(60, 40, 0.0, 0)).unwrap().contains("本机解码"));
        assert!(drop_reason(None, &r(40, 40, 8.0, 5)).unwrap().contains("网络丢包"));
        assert_eq!(drop_reason(None, &r(40, 40, 0.0, 0)), None);
        let q = PeerQuality { rtt_ms: 120, loss_pct: 18, jitter_ms: 30, unstable: true };
        let t = video_route_explain(Some(true), Some(&q), 1, false);
        println!("{t}");
        assert!(t.contains("不稳") && t.contains("你选了画面「P2P」"));
        assert!(video_route_explain(Some(false), None, 0, false).contains("打不通"));
    }

    #[test]
    fn relay_comparison_text() {
        let t = relay_compare("阿豪", 62, 148, None);
        println!("{t}");
        assert!(t.contains("经 阿豪 中转：62 ms（当前）"));
        assert!(t.contains("经服务器中转：148 ms（慢 86 ms）"));
        assert!(t.contains("直连：打不通"));
        let q = PeerQuality { rtt_ms: 161, loss_pct: 22, jitter_ms: 95, unstable: true };
        assert!(relay_compare("阿豪", 62, 148, Some(&q)).contains("直连：不稳定（161 ms · 丢包 22% · 抖动 95 ms）"));
        let q = PeerQuality { rtt_ms: 40, loss_pct: 0, jitter_ms: 2, unstable: false };
        assert!(relay_compare("阿豪", 62, 148, Some(&q)).contains("直连：已打通 40 ms，但超出你的上行预算"));
    }
}

#[cfg(test)]
mod vol_tests {
    use super::*;
    #[test]
    fn vol_curve_roundtrip() {
        assert_eq!(pos_to_gain(0.5), 1.0);
        assert_eq!(pos_to_gain(1.0), 2.0);
        assert_eq!(pos_to_gain(0.0), 0.0);
        assert!((pos_to_gain(0.25) - 0.25).abs() < 1e-6);
        for g in [0.0f32, 0.1, 0.5, 1.0, 1.4, 2.0] {
            assert!((pos_to_gain(gain_to_pos(g)) - g).abs() < 1e-4, "{g}");
        }
    }
}
