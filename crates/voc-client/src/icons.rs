//! 线条图标（第 59 项）：24×24 的 SVG 路径（和效果图同一套画法），用 kurbo 解析成折线后用 egui 画。
//! 不用表情符号当图标（浅色主题下发灰、各台电脑字体不一样），也不用下载图标字体。

use eframe::egui::{self, Color32, Pos2, Rect, Stroke};
use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Mic,
    MicOff,
    Headphones,
    HeadphonesOff,
    Screen,
    Music,
    Route,
    Settings,
    Leave,
    Hash,
    Speaker,
    Lock,
    Search,
    Plus,
    Keyboard,
    Wave,
    Wifi,
    Info,
    Back,
    Image,
    Smile,
    Warn,
    Palette,
    PopOut,
    Fullscreen,
    Chat,
    Grid,
    Logout,
    Close,
    User,
}

const ALL: [Icon; 30] = [
    Icon::Mic,
    Icon::MicOff,
    Icon::Headphones,
    Icon::HeadphonesOff,
    Icon::Screen,
    Icon::Music,
    Icon::Route,
    Icon::Settings,
    Icon::Leave,
    Icon::Hash,
    Icon::Speaker,
    Icon::Lock,
    Icon::Search,
    Icon::Plus,
    Icon::Keyboard,
    Icon::Wave,
    Icon::Wifi,
    Icon::Info,
    Icon::Back,
    Icon::Image,
    Icon::Smile,
    Icon::Warn,
    Icon::Palette,
    Icon::PopOut,
    Icon::Fullscreen,
    Icon::Chat,
    Icon::Grid,
    Icon::Logout,
    Icon::Close,
    Icon::User,
];

fn svg(i: Icon) -> &'static str {
    match i {
        Icon::Mic => "M9 6a3 3 0 0 1 6 0v5a3 3 0 0 1-6 0zM5 11a7 7 0 0 0 14 0M12 18v3",
        Icon::MicOff => "M9 9v3a3 3 0 0 0 5 2.2M15 9.3V6a3 3 0 0 0-5.7-1.3M19 11a7 7 0 0 1-1.2 3.9M5 11a7 7 0 0 0 11.5 5.4M12 18v3M3 3l18 18",
        Icon::Headphones => "M4 14v-2a8 8 0 0 1 16 0v2M3 14h4v6H3zM17 14h4v6h-4z",
        Icon::HeadphonesOff => "M4 14v-2a8 8 0 0 1 16 0v2M3 14h4v6H3zM17 14h4v6h-4zM3 3l18 18",
        Icon::Screen => "M3 5h18v12H3zM8 21h8M12 17v4",
        Icon::Music => "M9 18V5l11-2v13M9 18a3 3 0 1 1-6 0a3 3 0 0 1 6 0zM20 16a3 3 0 1 1-6 0a3 3 0 0 1 6 0z",
        Icon::Route => "M4 12h4l3-8 4 16 3-8h2",
        Icon::Settings => "M9 12a3 3 0 1 0 6 0a3 3 0 1 0-6 0M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z",
        Icon::Leave => "M4 15c4.5-4 11.5-4 16 0l-2 3-3-1v-3c-2-.7-4-.7-6 0v3l-3 1z",
        Icon::Hash => "M5 9h14M5 15h14M10 4 8 20M16 4l-2 16",
        Icon::Speaker => "M11 5 6 9H3v6h3l5 4zM15.5 8.5a5 5 0 0 1 0 7",
        Icon::Lock => "M5 11h14v10H5zM8 11V7a4 4 0 0 1 8 0v4",
        Icon::Search => "M4 11a7 7 0 1 0 14 0a7 7 0 1 0-14 0M20 20l-3.5-3.5",
        Icon::Plus => "M12 5v14M5 12h14",
        Icon::Keyboard => "M3 7h18v10H3zM7 11h.01M11 11h.01M15 11h.01M8 14h8",
        Icon::Wave => "M2 12h3l3-7 4 14 3-7h7",
        Icon::Wifi => "M5 12.5a10 10 0 0 1 14 0M8.5 16a5 5 0 0 1 7 0M12 19.5h.01M2 9a15 15 0 0 1 20 0",
        Icon::Info => "M3 12a9 9 0 1 0 18 0a9 9 0 1 0-18 0M12 8h.01M11 12h1v5h1",
        Icon::Back => "M15 18l-6-6 6-6",
        Icon::Image => "M3 6a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2zM7 10a2 2 0 1 0 4 0a2 2 0 1 0-4 0M21 16l-5-5L5 20",
        Icon::Smile => "M3 12a9 9 0 1 0 18 0a9 9 0 1 0-18 0M8.5 14a4 4 0 0 0 7 0M9 9.5h.01M15 9.5h.01",
        Icon::Warn => "M12 9v4M12 17h.01M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z",
        Icon::Palette => "M12 3a9 9 0 1 0 0 18c.8 0 1.5-.7 1.5-1.5 0-.4-.2-.8-.4-1.1-.3-.3-.4-.7-.4-1.1 0-.8.7-1.5 1.5-1.5H16a5 5 0 0 0 5-5c0-4.4-4-7.8-9-7.8zM7.5 10.5h.01M12 7.5h.01M16.5 10.5h.01",
        Icon::PopOut => "M15 3h6v6M10 14 21 3M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6",
        Icon::Fullscreen => "M8 3H5a2 2 0 0 0-2 2v3M21 8V5a2 2 0 0 0-2-2h-3M3 16v3a2 2 0 0 0 2 2h3M16 21h3a2 2 0 0 0 2-2v-3",
        Icon::Chat => "M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z",
        Icon::Grid => "M3 3h7v7H3zM14 3h7v7h-7zM14 14h7v7h-7zM3 14h7v7H3z",
        Icon::Logout => "M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4M16 17l5-5-5-5M21 12H9",
        Icon::Close => "M18 6 6 18M6 6l12 12",
        Icon::User => "M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2M8 7a4 4 0 1 0 8 0a4 4 0 1 0-8 0",
    }
}

/// 一个图标的折线（24×24 坐标），bool = 闭合
type Lines = Vec<(Vec<[f32; 2]>, bool)>;

fn parse(d: &str) -> Lines {
    let mut out: Lines = Vec::new();
    let Ok(path) = kurbo::BezPath::from_svg(d) else { return out };
    let mut cur: Vec<[f32; 2]> = Vec::new();
    kurbo::flatten(path, 0.05, |el| match el {
        kurbo::PathEl::MoveTo(p) => {
            if cur.len() > 1 {
                out.push((std::mem::take(&mut cur), false));
            }
            cur = vec![[p.x as f32, p.y as f32]];
        }
        kurbo::PathEl::LineTo(p) => cur.push([p.x as f32, p.y as f32]),
        kurbo::PathEl::ClosePath => {
            if cur.len() > 1 {
                out.push((std::mem::take(&mut cur), true));
            }
        }
        _ => {}
    });
    if cur.len() > 1 {
        out.push((cur, false));
    }
    out
}

fn lines(i: Icon) -> &'static Lines {
    static CACHE: OnceLock<Vec<Lines>> = OnceLock::new();
    let all = CACHE.get_or_init(|| ALL.iter().map(|i| parse(svg(*i))).collect());
    let idx = ALL.iter().position(|x| *x == i).unwrap_or(0);
    &all[idx]
}

/// 在 rect 里画图标（按 rect 的短边缩放，线宽跟着缩放，端点画成圆头）
pub fn paint(p: &egui::Painter, rect: Rect, icon: Icon, color: Color32) {
    let s = rect.width().min(rect.height()) / 24.0;
    let o = rect.center() - egui::vec2(12.0 * s, 12.0 * s);
    let w = (2.0 * s).max(1.2);
    let stroke = Stroke::new(w, color);
    let map = |q: &[f32; 2]| Pos2::new(o.x + q[0] * s, o.y + q[1] * s);
    for (pts, closed) in lines(icon) {
        let pts: Vec<Pos2> = pts.iter().map(map).collect();
        if *closed {
            p.add(egui::Shape::closed_line(pts, stroke));
        } else {
            if let (Some(a), Some(b)) = (pts.first(), pts.last()) {
                p.circle_filled(*a, w / 2.0, color);
                p.circle_filled(*b, w / 2.0, color);
            }
            p.add(egui::Shape::line(pts, stroke));
        }
    }
}

/// 占一块地方画图标
pub fn show(ui: &mut egui::Ui, icon: Icon, size: f32, color: Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    paint(ui.painter(), rect, icon, color);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_icons_parse() {
        for i in ALL {
            let l = parse(svg(i));
            assert!(!l.is_empty(), "{i:?}");
            for (pts, _) in &l {
                for q in pts {
                    assert!((-1.0..=25.0).contains(&q[0]) && (-1.0..=25.0).contains(&q[1]), "{i:?} out of box: {q:?}");
                }
            }
        }
    }
}
