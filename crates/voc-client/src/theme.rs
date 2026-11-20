//! VOC 的界面主题：深色配色、字号、间距、圆角，以及几个通用小组件。

use crate::voice::PeerQuality;
use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Margin, RichText, Sense, Stroke, TextStyle, Vec2};

// ---------------------------------------------------------------- 配色

pub const BG_BASE: Color32 = Color32::from_rgb(0x10, 0x12, 0x17);
pub const BG_SIDEBAR: Color32 = Color32::from_rgb(0x15, 0x17, 0x1d);
pub const BG_PANEL: Color32 = Color32::from_rgb(0x1b, 0x1e, 0x25);
pub const BG_ELEVATED: Color32 = Color32::from_rgb(0x23, 0x27, 0x30);
pub const BG_HOVER: Color32 = Color32::from_rgb(0x2a, 0x2f, 0x3a);
pub const BG_ACTIVE: Color32 = Color32::from_rgb(0x31, 0x36, 0x44);
pub const BORDER: Color32 = Color32::from_rgb(0x2c, 0x31, 0x3c);

pub const TEXT: Color32 = Color32::from_rgb(0xe6, 0xe8, 0xee);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0xa0, 0xa8, 0xb8);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x6b, 0x73, 0x85);

pub const ACCENT: Color32 = Color32::from_rgb(0x7c, 0x6c, 0xf6);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0x90, 0x82, 0xff);
pub const ACCENT_SOFT: Color32 = Color32::from_rgb(0x2e, 0x2a, 0x4f);
pub const GREEN: Color32 = Color32::from_rgb(0x3e, 0xcf, 0x8e);
pub const GREEN_SOFT: Color32 = Color32::from_rgb(0x1a, 0x3a, 0x2e);
pub const YELLOW: Color32 = Color32::from_rgb(0xf2, 0xc1, 0x4e);
pub const YELLOW_SOFT: Color32 = Color32::from_rgb(0x3d, 0x33, 0x1a);
pub const RED: Color32 = Color32::from_rgb(0xf0, 0x61, 0x6d);
pub const RED_SOFT: Color32 = Color32::from_rgb(0x42, 0x20, 0x25);

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
    let mut v = egui::Visuals::dark();
    v.override_text_color = Some(TEXT);
    v.panel_fill = BG_PANEL;
    v.window_fill = BG_PANEL;
    v.extreme_bg_color = BG_BASE;
    v.faint_bg_color = BG_ELEVATED;
    v.code_bg_color = BG_ELEVATED;
    v.hyperlink_color = ACCENT_HOVER;
    v.window_corner_radius = CornerRadius::same(14);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_shadow = egui::epaint::Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(140) };
    v.popup_shadow = egui::epaint::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(120) };
    v.selection.bg_fill = ACCENT;
    v.slider_trailing_fill = true; // 滑条左侧用强调色填充，更容易看清
    v.selection.stroke = Stroke::new(1.0, Color32::WHITE);

    let r = CornerRadius::same(8);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = BG_PANEL;
    w.noninteractive.weak_bg_fill = BG_PANEL;
    w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_DIM);
    w.noninteractive.corner_radius = r;

    w.inactive.bg_fill = BG_ELEVATED;
    w.inactive.weak_bg_fill = BG_ELEVATED;
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    w.inactive.corner_radius = r;

    w.hovered.bg_fill = BG_HOVER;
    w.hovered.weak_bg_fill = BG_HOVER;
    w.hovered.bg_stroke = Stroke::new(1.0, BORDER);
    w.hovered.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;

    w.active.bg_fill = BG_ACTIVE;
    w.active.weak_bg_fill = BG_ACTIVE;
    w.active.bg_stroke = Stroke::new(1.0, ACCENT);
    w.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;

    w.open.bg_fill = BG_ACTIVE;
    w.open.weak_bg_fill = BG_ACTIVE;
    w.open.corner_radius = r;
    v
}

// ---------------------------------------------------------------- 小组件

/// 圆角小标签
pub fn pill(ui: &mut egui::Ui, text: impl Into<String>, fg: Color32, bg: Color32) -> egui::Response {
    egui::Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| ui.label(RichText::new(text.into()).size(12.0).color(fg)))
        .response
}

/// 分区标题（小号、偏灰）
pub fn section(ui: &mut egui::Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(text).size(11.5).strong().color(TEXT_FAINT));
    ui.add_space(2.0);
}

/// 卡片容器
pub fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(BG_ELEVATED)
        .corner_radius(CornerRadius::same(14))
        .stroke(Stroke::new(1.0, BORDER))
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
    Accent,
}

/// 方形图标按钮
pub fn icon_button(ui: &mut egui::Ui, icon: &str, tooltip: &str, tone: Tone) -> egui::Response {
    let size = Vec2::splat(34.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let (bg, fg) = match (tone, resp.hovered()) {
        (Tone::Danger, _) => (RED_SOFT, RED),
        (Tone::Accent, _) => (ACCENT_SOFT, ACCENT_HOVER),
        (Tone::Normal, true) => (BG_HOVER, Color32::WHITE),
        (Tone::Normal, false) => (Color32::TRANSPARENT, TEXT_DIM),
    };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(8), bg);
    p.text(rect.center(), egui::Align2::CENTER_CENTER, icon, FontId::proportional(16.0), fg);
    resp.on_hover_text(tooltip)
}

/// 主按钮（强调色，可指定宽度）
pub fn primary_button(ui: &mut egui::Ui, text: &str, width: f32, enabled: bool) -> egui::Response {
    let btn = egui::Button::new(RichText::new(text).strong().color(Color32::WHITE))
        .fill(if enabled { ACCENT } else { BG_ACTIVE })
        .corner_radius(CornerRadius::same(10))
        .min_size(Vec2::new(width, 38.0));
    ui.add_enabled(enabled, btn)
}

/// 名字 → 稳定的头像颜色
pub fn name_color(name: &str) -> Color32 {
    let h = name.bytes().fold(5381u32, |h, b| h.wrapping_mul(33) ^ b as u32);
    const P: [Color32; 8] = [
        Color32::from_rgb(0xe8, 0x6a, 0x92),
        Color32::from_rgb(0x4e, 0xc9, 0x9b),
        Color32::from_rgb(0xe9, 0xb7, 0x4a),
        Color32::from_rgb(0xef, 0x76, 0x5a),
        Color32::from_rgb(0x4e, 0xa1, 0xff),
        Color32::from_rgb(0x9b, 0x7b, 0xf7),
        Color32::from_rgb(0x3f, 0xc1, 0xc9),
        Color32::from_rgb(0xc7, 0x8b, 0xe8),
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
        p.circle_filled(c, r, GREEN_SOFT);
        p.circle_stroke(c, r - 1.0, Stroke::new(2.0, GREEN));
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
        pill(ui, "⇄ 不稳·中转", YELLOW, YELLOW_SOFT).on_hover_text(format!(
            "P2P 直连不稳定（{detail}），已自动改走服务器中转；连续稳定后会自动切回直连"
        ));
    } else if voice_direct {
        let (fg, bg) = if q.rtt_ms < 80 {
            (GREEN, GREEN_SOFT)
        } else if q.rtt_ms < 200 {
            (YELLOW, YELLOW_SOFT)
        } else {
            (RED, RED_SOFT)
        };
        let tip = if video_direct { "P2P 直连：语音和你共享的画面都不经过服务器" } else { "P2P 直连：语音不经过服务器" };
        pill(ui, format!("⇄ {} ms", q.rtt_ms), fg, bg).on_hover_text(format!("{tip}\n{detail}"));
    } else {
        pill(ui, format!("⇄ {} ms·中转", q.rtt_ms), TEXT_FAINT, BG_ELEVATED)
            .on_hover_text(format!("已打通 P2P，但超出你设置的上行带宽预算，你的语音经服务器中转给他\n{detail}"));
    }
}

/// 按往返延迟选颜色（和直连徽章一致）
fn latency_colors(ms: u32) -> (Color32, Color32) {
    if ms < 80 {
        (GREEN, GREEN_SOFT)
    } else if ms < 200 {
        (YELLOW, YELLOW_SOFT)
    } else {
        (RED, RED_SOFT)
    }
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
            TEXT_FAINT,
            BG_ELEVATED,
        ),
        LinkState::Punch(Punching(s)) => (
            format!("⇄ 打洞中 {s}s"),
            format!("正在和他建立 P2P 直连（最多快速尝试 {fast} 秒）。打通前语音走服务器。"),
            YELLOW,
            YELLOW_SOFT,
        ),
        LinkState::Punch(Failed(s)) => (
            "⇄ 打不通·走服务器".to_string(),
            format!(
                "已经尝试 {s} 秒还没打通（通常是两边的路由器都比较严格），语音走服务器；有合适的队友时会改由队友中继。\n仍会每 2 秒重试一次。"
            ),
            TEXT_FAINT,
            BG_ELEVATED,
        ),
        LinkState::Punch(Connected(_)) => ("⇄ 已打通".to_string(), String::new(), GREEN, GREEN_SOFT),
        LinkState::MeOff => (
            "⇄ 你未开直连".to_string(),
            "你关闭了 P2P 直连，语音走服务器。可以在 设置 → 网络 打开。".to_string(),
            TEXT_FAINT,
            BG_ELEVATED,
        ),
        LinkState::ThemOff => ("⇄ 对方未开直连".to_string(), "对方关闭了 P2P 直连，语音走服务器。".to_string(), TEXT_FAINT, BG_ELEVATED),
        LinkState::ThemTcp => (
            "⇄ 对方 UDP 不通".to_string(),
            "对方的网络封锁了 UDP，只能经服务器的加密 TCP 通道，无法直连，延迟会高一些。".to_string(),
            TEXT_FAINT,
            BG_ELEVATED,
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
    egui::Frame::new().fill(BG_BASE).corner_radius(CornerRadius::same(10)).inner_margin(Margin::same(3)).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for (v, label) in options {
                let sel = *value == *v;
                let btn = egui::Button::new(RichText::new(*label).color(if sel { Color32::WHITE } else { TEXT_DIM }))
                    .fill(if sel { ACCENT } else { Color32::TRANSPARENT })
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

/// Unix 毫秒 → 本地「时:分」（按北京时间）
pub fn fmt_time(ts_ms: u64) -> String {
    let secs = ts_ms / 1000 + 8 * 3600;
    format!("{:02}:{:02}", (secs / 3600) % 24, (secs / 60) % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

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
