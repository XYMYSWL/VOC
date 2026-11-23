//! 全局按键（第 58 项改版）：按键说话、闭麦 / 静音、开始 / 停止共享、弹出观看窗口。
//! 后台线程每 15 ms 查一次按键状态（GetAsyncKeyState），不占用系统热键（不会和游戏、其他软件抢），VOC 不在前台也有效；
//! 每个键可以改成「只在 VOC 窗口在前台时」。
//! 录键（Capture）：实时显示正按着的组合，全部松开才确定；Esc 取消。录的时候所有全局按键暂停。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const VK_SHIFT: u32 = 0x10;
const VK_CONTROL: u32 = 0x11;
const VK_MENU: u32 = 0x12;
const VK_ESCAPE: u32 = 0x1B;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(default)]
pub struct Hotkey {
    /// 虚拟键码，0 = 不用。按键说话可以只是一个修饰键（0x10～0x12 = Shift / Ctrl / Alt）
    pub vk: u32,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    /// 只在 VOC 窗口在前台时有效（默认 false = 全局，游戏里也有效）
    pub focus_only: bool,
}

impl Hotkey {
    #[cfg(test)]
    pub const fn ctrl(vk: u32) -> Self {
        Self { vk, ctrl: true, shift: false, alt: false, focus_only: false }
    }

    pub const fn ctrl_shift(vk: u32) -> Self {
        Self { vk, ctrl: true, shift: true, alt: false, focus_only: false }
    }

    pub const fn key(vk: u32) -> Self {
        Self { vk, ctrl: false, shift: false, alt: false, focus_only: false }
    }

    pub fn is_set(&self) -> bool {
        self.vk != 0
    }

    /// 同一个组合（不管范围）
    pub fn same_combo(&self, o: &Hotkey) -> bool {
        self.is_set() && (self.vk, self.ctrl, self.shift, self.alt) == (o.vk, o.ctrl, o.shift, o.alt)
    }

    /// 显示用，例如 "Ctrl+Shift+M"、"F8"、"鼠标侧键 1"、"Ctrl"
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
        s + &key_name(self.vk)
    }

    /// 切换类快捷键：修饰键要完全一致（Ctrl+M 和 Ctrl+Shift+M 是两个键）
    fn down(&self, key_down: &impl Fn(u32) -> bool) -> bool {
        self.is_set()
            && key_down(self.vk)
            && key_down(VK_CONTROL) == self.ctrl
            && key_down(VK_SHIFT) == self.shift
            && key_down(VK_MENU) == self.alt
    }

    /// 按键说话：要求的修饰键按着就行，多按了别的不影响（游戏里常按着 Shift 跑步，照样能说话）
    fn held(&self, key_down: &impl Fn(u32) -> bool) -> bool {
        self.is_set()
            && key_down(self.vk)
            && (!self.ctrl || key_down(VK_CONTROL))
            && (!self.shift || key_down(VK_SHIFT))
            && (!self.alt || key_down(VK_MENU))
    }

    /// 按键说话现在是否按着（含「只在前台」的判断）
    pub fn held_now(&self) -> bool {
        self.held(&crate::voice::key_down) && (!self.focus_only || ours_foreground())
    }

    /// 打包成一个数，给音频线程用（0 = 没设）
    pub fn pack(&self) -> u32 {
        if !self.is_set() {
            return 0;
        }
        (self.vk & 0xFF) | (self.ctrl as u32) << 8 | (self.shift as u32) << 9 | (self.alt as u32) << 10 | (self.focus_only as u32) << 11
    }

    pub fn unpack(p: u32) -> Self {
        Self { vk: p & 0xFF, ctrl: p & 1 << 8 != 0, shift: p & 1 << 9 != 0, alt: p & 1 << 10 != 0, focus_only: p & 1 << 11 != 0 }
    }

    fn parts(&self) -> u32 {
        self.is_set() as u32 + self.ctrl as u32 + self.shift as u32 + self.alt as u32
    }
}

fn key_name(vk: u32) -> String {
    match vk {
        VK_SHIFT => "Shift".into(),
        VK_CONTROL => "Ctrl".into(),
        VK_MENU => "Alt".into(),
        _ => crate::app::vk_name(vk),
    }
}

/// 修饰键（单独按不算，按键说话除外）
fn is_modifier(vk: u32) -> bool {
    matches!(vk, 0x10..=0x12 | 0xA0..=0xA5 | VK_LWIN | VK_RWIN)
}

/// 现在前台的窗口是不是 VOC 自己的（主窗口、弹出的观看窗口都算）
pub fn ours_foreground() -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
        let h = GetForegroundWindow();
        if h.0.is_null() {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        return pid == std::process::id();
    }
    #[allow(unreachable_code)]
    false
}

// ---------------------------------------------------------------- 动作

/// 按键页上的动作（顺序就是表里的顺序；Watcher 里第 i 位 = 第 i 个动作，按键说话不走 Watcher）
pub const PTT: usize = 0;
pub const MUTE: usize = 1;
pub const DEAFEN: usize = 2;
pub const SHARE: usize = 3;
pub const POP: usize = 4;
pub const ACTIONS: [(&str, &str); 5] = [
    ("按键说话", "按住说话，松开停"),
    ("切换闭麦", "按一下闭麦，再按一下恢复（低音 = 闭麦，高音 = 恢复）"),
    ("切换静音", "按一下听不到别人也不发声，再按一下恢复"),
    ("开始 / 停止共享屏幕", "按共享窗口里上次的设置开始；正在共享就停止"),
    ("弹出 / 收回观看窗口", "正在看的共享画面弹出成独立窗口 / 收回主窗口"),
];

/// 默认按键：闭麦 / 静音改成 Ctrl+Shift+M / D（Ctrl+M、Ctrl+D 是很多软件的常用快捷键，容易误触）
pub fn default_key(action: usize) -> Hotkey {
    match action {
        MUTE => Hotkey::ctrl_shift(0x4D),
        DEAFEN => Hotkey::ctrl_shift(0x44),
        _ => Hotkey::default(),
    }
}

/// 设不了 / 要提醒的原因
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Problem {
    /// 带了 Win 键（系统快捷键）
    Win,
    /// 常用系统快捷键，例如 Ctrl+C
    System(&'static str),
    /// 切换类快捷键用了打字用的键（不带 Ctrl / Alt），打字时会触发
    Typing,
    /// 和第 i 个动作完全一样
    Dup(usize),
    /// 按这个键时会顺带触发第 i 个动作（按键说话只看要求的修饰键）
    Overlap(usize),
}

impl Problem {
    /// 不能用（只能重录）；Dup 可以换过来，Overlap 可以仍然使用
    pub fn blocking(&self) -> bool {
        matches!(self, Problem::Win | Problem::System(_) | Problem::Typing)
    }
}

/// 常用系统 / 软件快捷键，不让设
pub fn system_shortcut(k: &Hotkey) -> Option<&'static str> {
    let (c, s, a) = (k.ctrl, k.shift, k.alt);
    Some(match (k.vk, c, s, a) {
        (0x43, true, false, false) => "复制",
        (0x56, true, false, false) => "粘贴",
        (0x58, true, false, false) => "剪切",
        (0x5A, true, false, false) => "撤销",
        (0x59, true, false, false) => "重做",
        (0x41, true, false, false) => "全选",
        (0x53, true, false, false) => "保存",
        (0x09, _, _, true) => "切换窗口",
        (0x73, false, false, true) => "关闭窗口",
        (0x20, false, false, true) => "窗口菜单",
        (0x2C, _, _, _) => "截图",
        _ => return None,
    })
}

fn typing_key(vk: u32) -> bool {
    matches!(vk, 0x08 | 0x09 | 0x0D | 0x20 | 0x30..=0x39 | 0x41..=0x5A | 0x60..=0x6F | 0xBA..=0xC0 | 0xDB..=0xDF | 0xE2)
}

/// 按下 `t`（切换类）时会不会让按键说话 `p` 也算按着
fn ptt_overlaps(p: &Hotkey, t: &Hotkey) -> bool {
    if !p.is_set() || !t.is_set() {
        return false;
    }
    let held = |vk: u32| vk == t.vk || (vk == VK_CONTROL && t.ctrl) || (vk == VK_SHIFT && t.shift) || (vk == VK_MENU && t.alt);
    p.held(&held)
}

/// 给第 `action` 个动作设 `k` 有没有问题。`keys[i]` = 现在每个动作的按键；`win` = 录键时按过 Win
pub fn check(action: usize, k: &Hotkey, keys: &[Hotkey], win: bool) -> Option<Problem> {
    if win {
        return Some(Problem::Win);
    }
    if let Some(name) = system_shortcut(k) {
        return Some(Problem::System(name));
    }
    if action != PTT && !k.ctrl && !k.alt && typing_key(k.vk) {
        return Some(Problem::Typing);
    }
    for (i, o) in keys.iter().enumerate() {
        if i != action && o.same_combo(k) {
            return Some(Problem::Dup(i));
        }
    }
    for (i, o) in keys.iter().enumerate() {
        if i == action || !o.is_set() {
            continue;
        }
        let hit = if action == PTT { ptt_overlaps(k, o) } else if i == PTT { ptt_overlaps(o, k) } else { false };
        if hit {
            return Some(Problem::Overlap(i));
        }
    }
    None
}

/// 现在的设置里有哪些冲突（老配置留下的，页面上提示）：(动作, 问题)
pub fn current_problems(keys: &[Hotkey]) -> Vec<(usize, Problem)> {
    let mut out = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        if !k.is_set() {
            continue;
        }
        if let Some(p) = check(i, k, keys, false) {
            // 同一对只报一次
            let dup = match p {
                Problem::Dup(j) | Problem::Overlap(j) => out.iter().any(|(a, q)| *a == j && matches!(q, Problem::Dup(x) | Problem::Overlap(x) if *x == i)),
                _ => false,
            };
            if !dup {
                out.push((i, p));
            }
        }
    }
    out
}

/// 切换类快捷键是「Ctrl + 一个字母」、又是全局的：浏览器、编辑器里按也会触发，提醒一下
pub fn risky_global(k: &Hotkey) -> bool {
    k.is_set() && !k.focus_only && k.ctrl && !k.shift && !k.alt && (0x41..=0x5A).contains(&k.vk)
}

// ---------------------------------------------------------------- 录键

/// 录键的一步
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// 等开始前手里的键（例如点按钮时还按着的）先松开，或者还没按任何键
    Waiting,
    /// 正按着：显示用的组合，例如 "Ctrl + Shift + …"
    Live(String),
    /// 全部松开了：录到的键
    Done(Hotkey),
    /// Esc
    Cancel,
}

pub struct Capture {
    for_ptt: bool,
    armed: bool,
    best: Option<Hotkey>,
    /// 录的时候按过 Win 键（系统快捷键，不让设）
    pub win: bool,
}

impl Capture {
    pub fn new(for_ptt: bool) -> Self {
        Self { for_ptt, armed: false, best: None, win: false }
    }

    pub fn poll(&mut self) -> Step {
        self.poll_with(&crate::voice::key_down)
    }

    fn poll_with(&mut self, key_down: &impl Fn(u32) -> bool) -> Step {
        if key_down(VK_ESCAPE) {
            return Step::Cancel;
        }
        // 0x01 / 0x02 是鼠标左右键（点「取消」用），0x03 是 Ctrl+Break；左右区分的修饰键和通用的重复，只看通用的
        let pressed: Vec<u32> = (0x04u32..=0xFE).filter(|&vk| !(0xA0..=0xA5).contains(&vk) && vk != VK_ESCAPE && key_down(vk)).collect();
        if !self.armed {
            if pressed.is_empty() {
                self.armed = true;
            }
            return Step::Waiting;
        }
        if pressed.is_empty() {
            return match self.best.take() {
                Some(k) => Step::Done(k),
                None => Step::Waiting,
            };
        }
        let (ctrl, shift, alt) = (key_down(VK_CONTROL), key_down(VK_SHIFT), key_down(VK_MENU));
        let win = pressed.iter().any(|&vk| vk == VK_LWIN || vk == VK_RWIN);
        self.win |= win;
        let main = pressed.iter().copied().find(|&vk| !is_modifier(vk));
        let cand = match main {
            Some(vk) => Some(Hotkey { vk, ctrl, shift, alt, focus_only: false }),
            // 按键说话可以只用一个修饰键（例如左手按着 Ctrl 说话）
            None if self.for_ptt && !win && (ctrl as u8 + shift as u8 + alt as u8) == 1 => {
                Some(Hotkey::key(if ctrl { VK_CONTROL } else if shift { VK_SHIFT } else { VK_MENU }))
            }
            None => None,
        };
        // 记按过的最长组合：先松开 Ctrl 再松开 M 也算 Ctrl+M
        if let Some(c) = cand {
            if self.best.is_none_or(|b| c.parts() >= b.parts()) {
                self.best = Some(c);
            }
        }
        let mut parts: Vec<String> = Vec::new();
        for (on, name) in [(ctrl, "Ctrl"), (shift, "Shift"), (alt, "Alt"), (win, "Win")] {
            if on {
                parts.push(name.into());
            }
        }
        match main {
            Some(vk) => parts.push(key_name(vk)),
            None => parts.push("…".into()),
        }
        Step::Live(parts.join(" + "))
    }
}

// ---------------------------------------------------------------- 后台检测

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
                let fg = ks.iter().any(|k| k.is_set() && k.focus_only) && ours_foreground();
                let now: Vec<bool> = ks.iter().map(|k| k.down(&crate::voice::key_down) && (!k.focus_only || fg)).collect();
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
    use std::cell::RefCell;

    fn pressed(keys: &'static [u32]) -> impl Fn(u32) -> bool {
        move |vk: u32| keys.contains(&vk)
    }

    #[test]
    fn modifiers_must_match_exactly() {
        let ctrl_m = Hotkey::ctrl(0x4D);
        assert!(ctrl_m.down(&pressed(&[0x11, 0x4D])));
        assert!(!ctrl_m.down(&pressed(&[0x4D])), "M alone");
        assert!(!ctrl_m.down(&pressed(&[0x11, 0x10, 0x4D])), "Ctrl+Shift+M is a different key");
        let f8 = Hotkey::key(0x77);
        assert!(f8.down(&pressed(&[0x77])));
        assert!(!Hotkey::default().down(&pressed(&[0x77])));
    }

    #[test]
    fn ptt_allows_extra_modifiers() {
        let v = Hotkey::key(0x56);
        assert!(v.held(&pressed(&[0x56])));
        assert!(v.held(&pressed(&[0x10, 0x56])), "running with Shift held still talks");
        let ctrl_v = Hotkey::ctrl(0x05);
        assert!(!ctrl_v.held(&pressed(&[0x05])), "required Ctrl missing");
        assert!(ctrl_v.held(&pressed(&[0x11, 0x10, 0x05])));
        let ctrl_only = Hotkey::key(0x11);
        assert!(ctrl_only.held(&pressed(&[0x11, 0xA2])));
    }

    #[test]
    fn pack_roundtrip() {
        for k in [Hotkey::ctrl_shift(0x4D), Hotkey::key(0x05), Hotkey { vk: 0x11, focus_only: true, ..Default::default() }, Hotkey { vk: 0x70, alt: true, ..Default::default() }] {
            assert_eq!(Hotkey::unpack(k.pack()), k);
        }
        assert_eq!(Hotkey::default().pack(), 0);
    }

    /// 按时间顺序喂几组「正按着的键」
    fn run(for_ptt: bool, frames: &[&'static [u32]]) -> (Vec<Step>, bool) {
        let mut c = Capture::new(for_ptt);
        let cur: RefCell<&'static [u32]> = RefCell::new(&[]);
        let mut out = Vec::new();
        for f in frames {
            *cur.borrow_mut() = f;
            out.push(c.poll_with(&|vk| cur.borrow().contains(&vk)));
        }
        (out, c.win)
    }

    #[test]
    fn capture_waits_for_release_then_takes_longest_combo() {
        // 开始时还按着鼠标中键（点按钮那一下）：先等松开
        let (s, _) = run(false, &[&[0x04], &[], &[0x11, 0xA2], &[0x11, 0xA2, 0x10, 0xA0, 0x4D], &[0x4D], &[]]);
        assert_eq!(s[0], Step::Waiting);
        assert_eq!(s[1], Step::Waiting);
        assert_eq!(s[2], Step::Live("Ctrl + …".into()), "only Ctrl held: keep waiting, show live");
        assert!(matches!(&s[3], Step::Live(t) if t.starts_with("Ctrl + Shift + ")));
        assert_eq!(s[5], Step::Done(Hotkey::ctrl_shift(0x4D)), "released Ctrl first: still Ctrl+Shift+M");
    }

    #[test]
    fn capture_modifier_only_and_mouse() {
        let (s, _) = run(true, &[&[], &[0x11, 0xA3], &[]]);
        assert_eq!(s[2], Step::Done(Hotkey::key(0x11)), "PTT can be Ctrl alone (right Ctrl is not a separate key)");
        let (s, _) = run(false, &[&[], &[0x11, 0xA3], &[]]);
        assert_eq!(s[2], Step::Waiting, "toggle hotkeys need a real key");
        let (s, _) = run(true, &[&[], &[0x05], &[]]);
        assert_eq!(s[2], Step::Done(Hotkey::key(0x05)));
        let (s, _) = run(true, &[&[], &[0x01, 0x02], &[]]);
        assert_eq!(s[2], Step::Waiting, "left / right click are never captured");
        let (s, _) = run(false, &[&[], &[0x1B]]);
        assert_eq!(s[1], Step::Cancel);
    }

    #[test]
    fn capture_flags_win() {
        let (s, win) = run(false, &[&[], &[0x5B, 0x44], &[]]);
        assert_eq!(s[2], Step::Done(Hotkey::key(0x44)));
        assert!(win);
        let (s, _) = run(true, &[&[], &[0x5B], &[]]);
        assert_eq!(s[2], Step::Waiting, "Win alone is not a PTT key");
    }

    #[test]
    fn checks() {
        let mut keys = vec![Hotkey::key(0x4D), Hotkey::ctrl_shift(0x4D), Hotkey::ctrl_shift(0x44), Hotkey::default(), Hotkey::default()];
        assert_eq!(check(MUTE, &Hotkey::ctrl(0x43), &keys, false), Some(Problem::System("复制")));
        assert_eq!(check(MUTE, &Hotkey { vk: 0x09, alt: true, ..Default::default() }, &keys, false), Some(Problem::System("切换窗口")));
        assert_eq!(check(MUTE, &Hotkey::key(0x4B), &keys, false), Some(Problem::Typing));
        assert_eq!(check(MUTE, &Hotkey { vk: 0x4B, shift: true, ..Default::default() }, &keys, false), Some(Problem::Typing), "Shift+K types K");
        assert_eq!(check(PTT, &Hotkey::key(0x4B), &keys, false), None, "PTT on a letter is normal in games");
        assert_eq!(check(MUTE, &Hotkey::key(0x77), &keys, true), Some(Problem::Win));
        assert_eq!(check(SHARE, &Hotkey::ctrl_shift(0x44), &keys, false), Some(Problem::Dup(DEAFEN)));
        // 说话键 M、闭麦 Ctrl+Shift+M：按闭麦时也会说话
        assert_eq!(check(SHARE, &Hotkey::ctrl(0x4D), &keys, false), Some(Problem::Overlap(PTT)));
        assert_eq!(check(PTT, &Hotkey::key(0x44), &keys, false), Some(Problem::Overlap(DEAFEN)));
        assert_eq!(check(PTT, &Hotkey::key(0x10), &keys, false), Some(Problem::Overlap(MUTE)), "PTT = Shift, mute uses Shift");
        assert_eq!(check(PTT, &Hotkey::key(0x05), &keys, false), None);
        keys[PTT] = Hotkey::key(0x05);
        assert_eq!(check(MUTE, &Hotkey::ctrl_shift(0x4D), &keys, false), None, "own key is not a dup");
        assert!(current_problems(&keys).is_empty());
        keys[SHARE] = Hotkey::ctrl_shift(0x4D);
        assert_eq!(current_problems(&keys), vec![(MUTE, Problem::Dup(SHARE))], "a pair is reported once");
    }

    #[test]
    fn risky() {
        assert!(risky_global(&Hotkey::ctrl(0x44)));
        assert!(!risky_global(&Hotkey::ctrl_shift(0x44)));
        assert!(!risky_global(&Hotkey { focus_only: true, ..Hotkey::ctrl(0x44) }));
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
        assert_eq!(Hotkey::key(0x11).label(), "Ctrl");
    }
}
