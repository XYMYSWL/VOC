//! 伴奏和屏幕共享声音的发送端：按程序录音 → Opus（音乐模式，单声道 64 kbps）→ 端到端加密 → 发送。
//! 伴奏发给整个频道（不受闭麦影响），屏幕共享声音只发给观看者。程序没声音时不发，省流量。

use crate::audio::AtomicF32;
use crate::loopback::{self, Target};
use crate::voice::VoiceState;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use voc_proto::e2e::{STREAM_MUSIC, STREAM_SHARE};
use voc_proto::udp;

const BITRATE: i32 = 64_000;
/// 低带宽模式
const LOW_BITRATE: i32 = 32_000;
/// 连续这么多帧（20 ms × 25 = 0.5 秒）几乎没声音就不发了
const QUIET_FRAMES: u32 = 25;
const QUIET_RMS: f32 = 1e-4;

/// 正在发送的一路声音；drop 即停止
pub struct Source {
    _cap: loopback::Capture,
}

/// kind = udp::MUSIC（伴奏）或 udp::SHARE_AUDIO（屏幕共享声音）。gain：发送前的增益（本地监听补偿用）
pub fn start(kind: u8, target: Target, state: Arc<VoiceState>, gain: Arc<AtomicF32>) -> Result<Source, String> {
    let mut enc = opus::Encoder::new(voc_proto::SAMPLE_RATE, opus::Channels::Mono, opus::Application::Audio).map_err(|e| e.to_string())?;
    let _ = enc.set_bitrate(opus::Bitrate::Bits(BITRATE));
    let _ = enc.set_inband_fec(true);
    let _ = enc.set_packet_loss_perc(5);
    let stream = if kind == udp::MUSIC { STREAM_MUSIC } else { STREAM_SHARE };
    let mut out = vec![0u8; 1500];
    let mut pcm = vec![0i16; loopback::FRAME];
    let mut quiet = 0u32;
    let mut low = false;
    let mut pkt = Vec::with_capacity(udp::MAX_PACKET);
    let cap = loopback::start(target, move |frame| {
        let g = gain.get();
        let mut energy = 0.0f32;
        for (o, &x) in pcm.iter_mut().zip(frame) {
            let v = (x * g).clamp(-1.0, 1.0);
            energy += v * v;
            *o = (v * 32767.0) as i16;
        }
        let rms = (energy / frame.len() as f32).sqrt();
        quiet = if rms < QUIET_RMS { quiet + 1 } else { 0 };
        if quiet > QUIET_FRAMES {
            return;
        }
        let Some(link) = state.current_link() else { return };
        if !link.in_channel.load(Ordering::Relaxed) || (kind == udp::SHARE_AUDIO && !link.has_watchers()) {
            return;
        }
        let l = crate::audio::LOW_BANDWIDTH.load(Ordering::Relaxed);
        if l != low {
            low = l;
            let _ = enc.set_bitrate(opus::Bitrate::Bits(if l { LOW_BITRATE } else { BITRATE }));
        }
        let Ok(n) = enc.encode(&pcm, &mut out) else { return };
        let Some(e2e) = state.e2e.lock().unwrap().clone() else { return };
        let blob = e2e.lock().unwrap().encrypt_stream(stream, &out[..n]);
        link.send_stream(kind, &blob, &mut pkt);
    })?;
    Ok(Source { _cap: cap })
}
