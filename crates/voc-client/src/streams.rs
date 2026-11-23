//! 伴奏和屏幕共享声音的发送端：按程序录音 → Opus（音乐模式）→ 端到端加密 → 发送。
//! 伴奏发给整个频道（不受闭麦影响），屏幕共享声音只发给观看者。程序没声音时不发，省流量。
//! 录音线程只管录（「Pro Audio」优先级），压缩、加密、发送在另一个线程做：发送卡一下也不会让录音丢数据（第 54-c 项）。

use crate::audio::AtomicF32;
use crate::loopback::{self, Target};
use crate::voice::VoiceState;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use voc_proto::e2e::{STREAM_MUSIC, STREAM_SHARE};
use voc_proto::udp;

/// 码率（第 55-b 项）：单声道 96 kbps、立体声 128 kbps；低带宽模式 32 / 48 kbps
const BITRATE_MONO: i32 = 96_000;
const BITRATE_STEREO: i32 = 128_000;
const LOW_BITRATE_MONO: i32 = 32_000;
const LOW_BITRATE_STEREO: i32 = 48_000;
/// 连续这么多帧（20 ms × 25 = 0.5 秒）几乎没声音就不发了
const QUIET_FRAMES: u32 = 25;
/// 「几乎没声音」：-60 dB（原来 -80 dB，电脑上一点点底噪也会一直发出去，第 55-c 项）
const QUIET_RMS: f32 = 1e-3;
/// 恢复发送的第一帧淡入（样本数，每声道）
const FADE_IN: usize = 240;

/// 正在发送的一路声音；drop 即停止
pub struct Source {
    _cap: loopback::Capture,
    pub stereo: bool,
}

pub fn bitrate(stereo: bool, low: bool) -> i32 {
    match (stereo, low) {
        (true, true) => LOW_BITRATE_STEREO,
        (true, false) => BITRATE_STEREO,
        (false, true) => LOW_BITRATE_MONO,
        (false, false) => BITRATE_MONO,
    }
}

/// 一帧立体声（左右交错）→ 要发的 PCM（单声道时混成一路），乘上增益；返回 RMS
fn prepare(frame: &[f32], stereo: bool, g: f32, out: &mut Vec<i16>) -> f32 {
    out.clear();
    let mut energy = 0.0f32;
    let mut push = |v: f32, out: &mut Vec<i16>| {
        let v = (v * g).clamp(-1.0, 1.0);
        energy += v * v;
        // 四舍五入（原来直接截掉小数）
        out.push((v * 32767.0).round() as i16);
    };
    if stereo {
        for &x in frame {
            push(x, out);
        }
    } else {
        for lr in frame.chunks(2) {
            push((lr[0] + lr.get(1).copied().unwrap_or(lr[0])) * 0.5, out);
        }
    }
    (energy / out.len().max(1) as f32).sqrt()
}

/// kind = udp::MUSIC（伴奏）或 udp::SHARE_AUDIO（屏幕共享声音）。gain：发送前的增益（本地监听补偿用）。
/// stereo：立体声发送（旧版本客户端收到会自动混成单声道）
pub fn start(kind: u8, target: Target, state: Arc<VoiceState>, gain: Arc<AtomicF32>, stereo: bool) -> Result<Source, String> {
    let ch = if stereo { opus::Channels::Stereo } else { opus::Channels::Mono };
    let mut enc = opus::Encoder::new(voc_proto::SAMPLE_RATE, ch, opus::Application::Audio).map_err(|e| e.to_string())?;
    let mut low = crate::audio::LOW_BANDWIDTH.load(Ordering::Relaxed);
    let _ = enc.set_bitrate(opus::Bitrate::Bits(bitrate(stereo, low)));
    let _ = enc.set_inband_fec(true);
    let _ = enc.set_packet_loss_perc(5);
    let stream = if kind == udp::MUSIC { STREAM_MUSIC } else { STREAM_SHARE };
    // 录音线程 → 发送线程：最多排 25 帧（0.5 秒），发送线程卡住时丢最新的，不让录音线程等
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(25);
    std::thread::Builder::new()
        .name(if kind == udp::MUSIC { "music-send".into() } else { "share-audio-send".into() })
        .spawn(move || {
            let mut out = vec![0u8; 1500];
            let mut pcm: Vec<i16> = Vec::with_capacity(loopback::FRAME * 2);
            let mut quiet = 0u32;
            let mut pkt = Vec::with_capacity(udp::MAX_PACKET);
            while let Ok(frame) = rx.recv() {
                let rms = prepare(&frame, stereo, gain.get(), &mut pcm);
                let was_quiet = quiet > QUIET_FRAMES;
                quiet = if rms < QUIET_RMS { quiet + 1 } else { 0 };
                if quiet > QUIET_FRAMES {
                    continue;
                }
                if was_quiet {
                    // 停了一阵又有声音：第一帧淡入，接收端不会「啪」一声
                    let chs = if stereo { 2 } else { 1 };
                    for (i, s) in pcm.iter_mut().take(FADE_IN * chs).enumerate() {
                        *s = (*s as f32 * (i / chs) as f32 / FADE_IN as f32) as i16;
                    }
                }
                let Some(link) = state.current_link() else { continue };
                if !link.in_channel.load(Ordering::Relaxed) || (kind == udp::SHARE_AUDIO && !link.has_watchers()) {
                    continue;
                }
                let l = crate::audio::LOW_BANDWIDTH.load(Ordering::Relaxed);
                if l != low {
                    low = l;
                    let _ = enc.set_bitrate(opus::Bitrate::Bits(bitrate(stereo, low)));
                }
                let Ok(n) = enc.encode(&pcm, &mut out) else { continue };
                let Some(e2e) = state.e2e.lock().unwrap().clone() else { continue };
                let blob = e2e.lock().unwrap().encrypt_stream(stream, &out[..n]);
                link.send_stream(kind, &blob, &mut pkt);
            }
        })
        .map_err(|e| e.to_string())?;
    let cap = loopback::start(target, move |frame| {
        let _ = tx.try_send(frame.to_vec());
    })?;
    Ok(Source { _cap: cap, stereo })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_mono_and_stereo() {
        let frame: Vec<f32> = (0..loopback::FRAME).flat_map(|_| [0.5f32, -0.5]).collect();
        let mut pcm = Vec::new();
        // 左右相反的声音混成单声道 = 0（单声道是两边平均）
        let rms = prepare(&frame, false, 1.0, &mut pcm);
        assert_eq!(pcm.len(), loopback::FRAME);
        assert!(rms < 1e-6 && pcm.iter().all(|&x| x == 0));
        let rms = prepare(&frame, true, 1.0, &mut pcm);
        assert_eq!(pcm.len(), loopback::FRAME * 2);
        assert_eq!((pcm[0], pcm[1]), (16384, -16384));
        assert!((rms - 0.5).abs() < 1e-3);
        assert!(bitrate(true, false) > bitrate(false, false) && bitrate(false, true) < bitrate(false, false));
    }
}
