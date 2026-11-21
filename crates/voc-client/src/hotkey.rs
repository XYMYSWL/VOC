//! 全局快捷键（闭麦 / 静音）：后台线程每 15 ms 查一次按键状态，VOC 不在前台、在游戏里也有效。
//! 和按键说话一样用 GetAsyncKeyState，不占用系统热键（不会和游戏、其他软件抢）。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const VK_SHIFT: u32 = 0x10;
const VK_CONTROL: u32 = 0x11;
const VK_MENU: u32 = 0x12;
const VK_ESCAPE: u32 = 0x1B;

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(default)]
pub struct Hotkey {
    /// 虚拟键码，0 = 不用快捷键
    pub vk: u32,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

impl Hotkey {
    pub const fn ctrl(vk: u32) -> Self {
        Self { vk, ctrl: true, shift: false, alt: false }
    }

    pub fn is_set(&self) -> bool {
        self.vk != 0
    }

    /// 显示用，例如 "Ctrl+M"、"F8"、"鼠标侧键 1"
    pub fn label(&self) -> String {
        if !self.is_set() {
            return "未设置".into();
        }
        let mut s = String::new();
        for (on, name) in [(self.ctrl, "Ctrl+"), (self.shift, "Shift+"), (self.alt, "Alt+")] {
            if on {
                s += name;
            }
        }
        s + &crate::app::vk_name(self.vk)
    }

    /// 现在是不是正按着（修饰键要完全一致：Ctrl+M 和 Ctrl+Shift+M 是两个键）
    fn down(&self, key_down: &impl Fn(u32) -> bool) -> bool {
        self.is_set()
            && key_down(self.vk)
            && key_down(VK_CONTROL) == self.ctrl
            && key_down(VK_SHIFT) == self.shift
            && key_down(VK_MENU) == self.alt
    }
}

/// 修饰键（单独按不算）
fn is_modifier(vk: u32) -> bool {
    matches!(vk, 0x10..=0x12 | 0xA0..=0xA5 | 0x5B | 0x5C)
}

/// 录键：等用户按下一个键（可以带 Ctrl / Shift / Alt；键盘键、鼠标中键或侧键）。Esc 取消，返回 Some(None)
pub fn capture() -> Option<Option<Hotkey>> {
    capture_with(&crate::voice::key_down)
}

fn capture_with(key_down: &impl Fn(u32) -> bool) -> Option<Option<Hotkey>> {
    if key_down(VK_ESCAPE) {
        return Some(None);
    }
    let vk = (0x04u32..=0xFE).filter(|&vk| vk != VK_ESCAPE && !is_modifier(vk)).find(|&vk| key_down(vk))?;
    Some(Some(Hotkey { vk, ctrl: key_down(VK_CONTROL), shift: key_down(VK_SHIFT), alt: key_down(VK_MENU) }))
}

/// 后台检测：按下（从没按到按下的那一刻）时把对应位写进 fired，并让界面刷新一次
pub struct Watcher {
    pub keys: Arc<Mutex<Vec<Hotkey>>>,
    pub fired: Arc<AtomicU32>,
    /// 正在录键时暂停，免得录的时候触发
    pub paused: Arc<AtomicBool>,
}

impl Watcher {
    pub fn spawn(keys: Vec<Hotkey>, ctx: eframe::egui::Context) -> Self {
        let w = Self { keys: Arc::new(Mutex::new(keys)), fired: Arc::new(AtomicU32::new(0)), paused: Arc::new(AtomicBool::new(false)) };
        let (keys, fired, paused) = (w.keys.clone(), w.fired.clone(), w.paused.clone());
        let _ = std::thread::Builder::new().name("hotkeys".into()).spawn(move || {
            let mut was = Vec::<bool>::new();
            loop {
                std::thread::sleep(Duration::from_millis(15));
                let ks = keys.lock().unwrap().clone();
                let now: Vec<bool> = ks.iter().map(|k| k.down(&crate::voice::key_down)).collect();
                if !paused.load(Ordering::Relaxed) {
                    let edges = edges(&was, &now);
                    if edges != 0 {
                        fired.fetch_or(edges, Ordering::Relaxed);
                        ctx.request_repaint();
                    }
                }
                was = now;
            }
        });
        w
    }

    pub fn set_keys(&self, keys: Vec<Hotkey>) {
        *self.keys.lock().unwrap() = keys;
    }

    /// 取走按下过的键（第 i 位 = 第 i 个快捷键）
    pub fn take(&self) -> u32 {
        self.fired.swap(0, Ordering::Relaxed)
    }
}

/// 刚按下的键（上一次没按、这一次按着）
fn edges(was: &[bool], now: &[bool]) -> u32 {
    now.iter().enumerate().filter(|(i, d)| **d && !was.get(*i).copied().unwrap_or(false)).fold(0, |m, (i, _)| m | (1 << i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_must_match_exactly() {
        let ctrl_m = Hotkey::ctrl(0x4D);
        let pressed = |keys: &'static [u32]| move |vk: u32| keys.contains(&vk);
        assert!(ctrl_m.down(&pressed(&[0x11, 0x4D])));
        assert!(!ctrl_m.down(&pressed(&[0x4D])), "M alone");
        assert!(!ctrl_m.down(&pressed(&[0x11, 0x10, 0x4D])), "Ctrl+Shift+M is a different key");
        let f8 = Hotkey { vk: 0x77, ..Default::default() };
        assert!(f8.down(&pressed(&[0x77])));
        assert!(!Hotkey::default().down(&pressed(&[0x77])));
    }

    #[test]
    fn capture_records_modifiers_and_ignores_them_alone() {
        let pressed = |keys: &'static [u32]| move |vk: u32| keys.contains(&vk);
        assert_eq!(capture_with(&pressed(&[0x11, 0xA2])), None, "only Ctrl held: keep waiting");
        assert_eq!(capture_with(&pressed(&[0x11, 0xA2, 0x4D])), Some(Some(Hotkey::ctrl(0x4D))));
        assert_eq!(capture_with(&pressed(&[0x05])), Some(Some(Hotkey { vk: 0x05, ..Default::default() })));
        assert_eq!(capture_with(&pressed(&[0x1B])), Some(None));
    }

    #[test]
    fn only_new_presses_fire() {
        assert_eq!(edges(&[], &[true, false]), 1);
        assert_eq!(edges(&[true, false], &[true, true]), 2);
        assert_eq!(edges(&[true, true], &[true, true]), 0);
        assert_eq!(edges(&[true, true], &[false, false]), 0);
    }

    #[test]
    fn labels() {
        assert_eq!(Hotkey::default().label(), "未设置");
        assert!(Hotkey::ctrl(0x4D).label().starts_with("Ctrl+"));
        assert_eq!(Hotkey { vk: 0x05, shift: true, ..Default::default() }.label(), "Shift+鼠标侧键 1");
    }
}
