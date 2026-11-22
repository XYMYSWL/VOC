//! 本次（从登录起）用了多少流量（第 38-4 项）：语音（含伴奏）、屏幕共享（画面 + 共享声音 + 鼠标）、图片，各自发 / 收。
//! 按量计费的校园网、手机流量用户心里有数。在收发的地方按包计数，每包加上包头估算（UDP/IP + 加密头约 67 字节），不额外发包。

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy)]
pub enum Cat {
    Voice,
    Share,
    Image,
}

/// 每个包大概的包头开销（IP/UDP 28 + 计数器 8 + 认证标签 16 + 类型长度 3 + 随机填充平均 12）
pub const PACKET_OVERHEAD: u64 = 67;

static TX: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static RX: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

fn idx(c: Cat) -> usize {
    match c {
        Cat::Voice => 0,
        Cat::Share => 1,
        Cat::Image => 2,
    }
}

/// 发出一个包（payload 字节数，不含包头）
pub fn sent(c: Cat, payload: usize) {
    TX[idx(c)].fetch_add(payload as u64 + PACKET_OVERHEAD, Ordering::Relaxed);
}

/// 收到一个包
pub fn received(c: Cat, payload: usize) {
    RX[idx(c)].fetch_add(payload as u64 + PACKET_OVERHEAD, Ordering::Relaxed);
}

/// 登录时清零
pub fn reset() {
    for a in TX.iter().chain(RX.iter()) {
        a.store(0, Ordering::Relaxed);
    }
}

/// (发, 收) 字节：语音、屏幕共享、图片
pub fn snapshot() -> [(u64, u64); 3] {
    [0, 1, 2].map(|i| (TX[i].load(Ordering::Relaxed), RX[i].load(Ordering::Relaxed)))
}

/// 人看的大小：1.2 MB / 345 KB
pub fn human(b: u64) -> String {
    match b {
        0..1024 => format!("{b} B"),
        1024..1_048_576 => format!("{:.0} KB", b as f64 / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MB", b as f64 / 1_048_576.0),
        _ => format!("{:.2} GB", b as f64 / 1_073_741_824.0),
    }
}

/// 一行说明：语音 ↑1.2 MB ↓3.4 MB · 屏幕共享 ↑… · 图片 ↑…
pub fn summary() -> String {
    let s = snapshot();
    let names = ["语音", "屏幕共享", "图片"];
    let total: u64 = s.iter().map(|(t, r)| t + r).sum();
    let parts: Vec<String> = names.iter().zip(s.iter()).map(|(n, (t, r))| format!("{n} ↑{} ↓{}", human(*t), human(*r))).collect();
    format!("{}（合计 {}）", parts.join(" · "), human(total))
}

#[cfg(test)]
mod tests {
    #[test]
    fn human_sizes() {
        assert_eq!(super::human(500), "500 B");
        assert_eq!(super::human(2048), "2 KB");
        assert_eq!(super::human(5 * 1_048_576 + 100_000), "5.1 MB");
    }
}
