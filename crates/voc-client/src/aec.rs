//! 回声消除：不戴耳机时，扬声器里队友的声音会被麦克风收进去再发回去（队友听到自己的回声）。
//! 用 sonora（WebRTC AEC3 的纯 Rust 实现）：
//! - 播放端（声卡输出回调）把即将播放的 48 kHz 混音切成 10 ms 一帧，放进一个队列（`push_render`）；
//! - 编码线程处理每一帧麦克风之前，先把这些「远端参考」喂给回声消除器，再从麦克风信号里减掉回声。
//! 在 RNNoise 降噪之前处理。

use sonora::config::EchoCanceller as EcConfig;
use sonora::{AudioProcessing, Config, StreamConfig};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Mutex;

/// 回声消除处理的帧长：10 ms @ 48 kHz
pub const AEC_FRAME: usize = 480;
const AEC_RATE: usize = 48_000;
/// 播放到采集的大致延迟（两边声卡缓冲），AEC3 会在此基础上自己估计
const DELAY_HINT_MS: i32 = 30;

/// 播放端写入远端参考的通道（当前的回声消除器创建时换成它的）
static RENDER_TX: Mutex<Option<SyncSender<Vec<f32>>>> = Mutex::new(None);

/// 播放端调用：即将播放的一帧（10 ms，48 kHz 单声道）。在声卡回调里调用，不阻塞。
pub fn push_render(frame: Vec<f32>) {
    if let Ok(g) = RENDER_TX.try_lock() {
        if let Some(tx) = g.as_ref() {
            match tx.try_send(frame) {
                Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
            }
        }
    }
}

pub struct EchoCanceller {
    apm: AudioProcessing,
    render_rx: Receiver<Vec<f32>>,
    out: Vec<f32>,
}

impl EchoCanceller {
    /// 创建并接管播放端的参考通道
    pub fn new() -> Self {
        let (tx, rx) = sync_channel(64);
        *RENDER_TX.lock().unwrap() = Some(tx);
        Self::with_receiver(rx)
    }

    fn with_receiver(render_rx: Receiver<Vec<f32>>) -> Self {
        let config = Config { echo_canceller: Some(EcConfig::default()), ..Default::default() };
        let mut apm = AudioProcessing::builder()
            .config(config)
            .capture_config(StreamConfig::new(AEC_RATE as u32, 1))
            .render_config(StreamConfig::new(AEC_RATE as u32, 1))
            .build();
        let _ = apm.set_stream_delay_ms(DELAY_HINT_MS);
        Self { apm, render_rx, out: vec![0.0; AEC_FRAME] }
    }

    /// 把积压的远端参考都喂进去
    fn feed_render(&mut self) {
        while let Ok(r) = self.render_rx.try_recv() {
            self.render(&r);
        }
    }

    fn render(&mut self, frame: &[f32]) {
        if frame.len() != AEC_FRAME {
            return;
        }
        let mut sink = [0.0f32; AEC_FRAME];
        let _ = self.apm.process_render_f32(&[frame], &mut [&mut sink[..]]);
    }

    /// 处理一帧麦克风（480 个样本），原地去掉回声
    pub fn process(&mut self, mic: &mut [f32]) {
        self.feed_render();
        if mic.len() != AEC_FRAME {
            return;
        }
        if self.apm.process_capture_f32(&[&*mic], &mut [&mut self.out[..]]).is_ok() {
            mic.copy_from_slice(&self.out);
        }
    }

    /// 关闭回声消除时也要把参考队列清掉，免得重新打开时用到旧数据
    pub fn discard_render(&mut self) {
        while self.render_rx.try_recv().is_ok() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn energy_db(x: &[f32]) -> f32 {
        let e: f32 = x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32;
        10.0 * (e + 1e-12).log10()
    }

    /// 模拟外放：麦克风收到的是扬声器声音延迟 30 ms、衰减一半、再加一点房间混响。
    /// 收敛后，回声应被压低 15 dB 以上；近端（自己说话）不应被消掉。
    #[test]
    fn cancels_simulated_speaker_echo() {
        let (_tx, rx) = sync_channel(1);
        let mut ec = EchoCanceller::with_receiver(rx);
        let delay = 30 * AEC_RATE / 1000;
        // 远端：类似语音的调幅噪声
        let mut seed = 12345u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        };
        let secs = 8;
        let n = secs * AEC_RATE;
        let far: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / AEC_RATE as f32;
                let env = 0.5 + 0.5 * (std::f32::consts::TAU * 3.0 * t).sin();
                noise() * 0.3 * env
            })
            .collect();
        let echo = |i: usize| -> f32 {
            let a = if i >= delay { far[i - delay] * 0.5 } else { 0.0 };
            let b = if i >= delay + 240 { far[i - delay - 240] * 0.15 } else { 0.0 };
            a + b
        };
        let mut before = Vec::new();
        let mut after = Vec::new();
        for f in 0..n / AEC_FRAME {
            let r = &far[f * AEC_FRAME..(f + 1) * AEC_FRAME];
            ec.render(r);
            let mut mic: Vec<f32> = (0..AEC_FRAME).map(|k| echo(f * AEC_FRAME + k)).collect();
            if f * AEC_FRAME > (secs - 2) * AEC_RATE {
                before.extend_from_slice(&mic);
            }
            ec.process(&mut mic);
            if f * AEC_FRAME > (secs - 2) * AEC_RATE {
                after.extend_from_slice(&mic);
            }
        }
        let erle = energy_db(&before) - energy_db(&after);
        println!("回声被压低 {erle:.1} dB");
        assert!(erle > 15.0, "echo only reduced by {erle:.1} dB");

        // 没有远端声音时，自己说话照常通过
        let mut near: Vec<f32> = (0..AEC_FRAME * 100).map(|i| (i as f32 * 0.05).sin() * 0.3).collect();
        let e_in = energy_db(&near[AEC_FRAME * 50..]);
        for c in near.chunks_mut(AEC_FRAME) {
            ec.render(&[0.0; AEC_FRAME]);
            ec.process(c);
        }
        let e_out = energy_db(&near[AEC_FRAME * 50..]);
        assert!(e_in - e_out < 6.0, "near-end speech attenuated by {:.1} dB", e_in - e_out);
    }
}
