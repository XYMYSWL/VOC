//! 系统托盘：关窗口后缩到托盘继续语音。左键点图标打开窗口，右键菜单可闭麦 / 静音 / 退出。

use eframe::egui;
use std::sync::mpsc::{self, Receiver};
use std::sync::Mutex;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// 托盘里的操作
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    Show,
    ToggleMute,
    ToggleDeaf,
    Quit,
}

pub struct Tray {
    icon: TrayIcon,
    mute: CheckMenuItem,
    deaf: CheckMenuItem,
    rx: Mutex<Receiver<TrayAction>>,
    last_tip: Mutex<String>,
}

impl Tray {
    /// 创建托盘图标（必须在主线程，也就是窗口所在的线程上调用）
    pub fn new(ctx: &egui::Context, icon_rgba: Vec<u8>, w: u32, h: u32) -> Option<Self> {
        let show = MenuItem::new("打开 VOC", true, None);
        let mute = CheckMenuItem::new("闭麦", true, false, None);
        let deaf = CheckMenuItem::new("静音（不听也不说）", true, false, None);
        let quit = MenuItem::new("退出 VOC", true, None);
        let menu = Menu::new();
        menu.append_items(&[&show, &PredefinedMenuItem::separator(), &mute, &deaf, &PredefinedMenuItem::separator(), &quit]).ok()?;

        let icon = tray_icon::Icon::from_rgba(icon_rgba, w, h).ok()?;
        let tray = TrayIconBuilder::new()
            .with_icon(icon)
            .with_tooltip("VOC")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()
            .ok()?;

        let (tx, rx) = mpsc::channel();
        let ids = (show.id().clone(), mute.id().clone(), deaf.id().clone(), quit.id().clone());
        let (tx1, ctx1) = (tx.clone(), ctx.clone());
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let a = if e.id == ids.0 {
                TrayAction::Show
            } else if e.id == ids.1 {
                TrayAction::ToggleMute
            } else if e.id == ids.2 {
                TrayAction::ToggleDeaf
            } else if e.id == ids.3 {
                TrayAction::Quit
            } else {
                return;
            };
            let _ = tx1.send(a);
            ctx1.request_repaint();
        }));
        let ctx2 = ctx.clone();
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            let show = matches!(
                e,
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                    | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }
            );
            if show {
                let _ = tx.send(TrayAction::Show);
                ctx2.request_repaint();
            }
        }));
        Some(Self { icon: tray, mute, deaf, rx: Mutex::new(rx), last_tip: Mutex::new(String::new()) })
    }

    /// 取出用户在托盘上的操作
    pub fn poll(&self) -> Vec<TrayAction> {
        self.rx.lock().unwrap().try_iter().collect()
    }

    /// 同步闭麦 / 静音勾选和提示文字（有变化才更新）
    pub fn set_state(&self, muted: bool, deafened: bool, status: &str) {
        self.mute.set_checked(muted);
        self.deaf.set_checked(deafened);
        let tip = if status.is_empty() { "VOC".to_string() } else { format!("VOC · {status}") };
        let mut last = self.last_tip.lock().unwrap();
        if *last != tip {
            let _ = self.icon.set_tooltip(Some(&tip));
            *last = tip;
        }
    }
}
