//! 音频设备、重采样、抖动缓冲与混音。

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use voc_proto::{udp::seq_newer, FRAME_SAMPLES, SAMPLE_RATE};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

/// 采集端输出 48 kHz 单声道、10 ms 一帧（RNNoise / Krisp 都用这个格式），
/// 编码线程降噪后再降到 16 kHz。
pub const CAPTURE_RATE: u32 = 48_000;
pub const CAPTURE_FRAME: usize = 480;

/// 本地回放（麦克风测试）在混音器里使用的 id
pub const LOOPBACK_ID: u32 = 0;
/// 提示音在混音器里使用的 id
pub const SFX_ID: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Chime {
    /// 有人进入频道：上行两音
    Join,
    /// 有人离开频道：下行两音
    Leave,
}

/// 合成提示音（16 kHz 单声道）：两个带包络的柔和正弦音，叠一点二次谐波让声音更圆润
pub fn chime(kind: Chime) -> Vec<i16> {
    // 「噔、噔」两声：每声约 0.17 秒，中间停 0.06 秒；进入低→高，离开高→低
    let (a, b) = (659.25f32, 987.77f32); // E5、B5
    let notes = match kind {
        Chime::Join => [a, b],
        Chime::Leave => [b, a],
    };
    const NOTE: f32 = 0.17;
    const GAP: f32 = 0.06;
    let sr = SAMPLE_RATE as f32;
    let mut out = Vec::new();
    for (k, freq) in notes.into_iter().enumerate() {
        if k > 0 {
            out.extend(std::iter::repeat_n(0i16, (GAP * sr) as usize));
        }
        let n = (NOTE * sr) as usize;
        for i in 0..n {
            let t = i as f32 / sr;
            let attack = (t / 0.008).min(1.0);
            // 衰减放慢，尾音更圆；最后 15 ms 淡出到 0，避免咔哒声
            let release = (-(t / NOTE) * 2.6).exp();
            let tail = ((NOTE - t) / 0.015).clamp(0.0, 1.0);
            let env = attack * release * tail;
            let w = std::f32::consts::TAU * freq * t;
            let v = (w.sin() + 0.3 * (2.0 * w).sin() + 0.08 * (3.0 * w).sin()) * env * 0.2;
            out.push((v * 32767.0) as i16);
        }
    }
    // 补齐到整帧，最后加一点静音
    out.resize(out.len().div_ceil(FRAME_SAMPLES) * FRAME_SAMPLES + FRAME_SAMPLES, 0);
    out
}

#[derive(Default)]
pub struct AtomicF32(AtomicU32);
impl AtomicF32 {
    pub fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed)
    }
}

/// UI 与音频线程共享的设置/状态
pub struct Settings {
    pub muted: AtomicBool,
    pub deafened: AtomicBool,
    pub loopback: AtomicBool,
    /// 语音激活阈值（dBFS）
    pub vad_threshold_db: AtomicF32,
    pub input_gain: AtomicF32,
    pub output_volume: AtomicF32,
    /// 当前麦克风电平（dBFS），供 UI 显示
    pub input_level_db: AtomicF32,
    /// 当前是否正在发送语音
    pub transmitting: AtomicBool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            loopback: AtomicBool::new(false),
            vad_threshold_db: AtomicF32::new(-45.0),
            input_gain: AtomicF32::new(1.0),
            output_volume: AtomicF32::new(1.0),
            input_level_db: AtomicF32::new(-100.0),
            transmitting: AtomicBool::new(false),
        }
    }
}

// ---------------------------------------------------------------- 重采样

/// RBJ 双二阶低通，用于降采样前抗混叠
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn lowpass(sample_rate: f32, cutoff: f32) -> Self {
        let w0 = std::f32::consts::TAU * cutoff / sample_rate;
        let (s, c) = w0.sin_cos();
        let alpha = s / (2.0 * std::f32::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        Self {
            b0: (1.0 - c) / 2.0 / a0,
            b1: (1.0 - c) / a0,
            b2: (1.0 - c) / 2.0 / a0,
            a1: -2.0 * c / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }
    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// 推式线性重采样（采集端：设备采样率 -> 16 kHz）
pub struct PushResampler {
    step: f64,
    pos: f64,
    last: f32,
    lp: Option<(Biquad, Biquad)>,
}

impl PushResampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        let lp = (in_rate > out_rate).then(|| {
            let c = out_rate as f32 * 0.45;
            (Biquad::lowpass(in_rate as f32, c), Biquad::lowpass(in_rate as f32, c))
        });
        Self { step: in_rate as f64 / out_rate as f64, pos: 0.0, last: 0.0, lp }
    }

    /// 处理一个输入样本，可能产生 0 个或多个输出样本
    pub fn push(&mut self, mut x: f32, mut out: impl FnMut(f32)) {
        if let Some((a, b)) = &mut self.lp {
            x = b.process(a.process(x));
        }
        // 在 [last, x] 之间插值，pos ∈ [0,1) 表示相对 last 的位置
        while self.pos < 1.0 {
            out(self.last + (x - self.last) * self.pos as f32);
            self.pos += self.step;
        }
        self.pos -= 1.0;
        self.last = x;
    }
}

/// 拉式线性重采样（播放端：16 kHz -> 设备采样率）
pub struct PullResampler {
    step: f64,
    pos: f64,
    a: f32,
    b: f32,
}

impl PullResampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self { step: in_rate as f64 / out_rate as f64, pos: 1.0, a: 0.0, b: 0.0 }
    }
    pub fn next(&mut self, mut src: impl FnMut() -> f32) -> f32 {
        while self.pos >= 1.0 {
            self.a = self.b;
            self.b = src();
            self.pos -= 1.0;
        }
        let v = self.a + (self.b - self.a) * self.pos as f32;
        self.pos += self.step;
        v
    }
}

// ---------------------------------------------------------------- 混音

/// 起播前预缓冲 60 ms
const PREBUFFER: usize = FRAME_SAMPLES * 3;
/// 缓冲超过 200 ms 时丢弃旧数据，控制延迟
const MAX_BUFFER: usize = FRAME_SAMPLES * 10;

struct Stream {
    buf: VecDeque<f32>,
    playing: bool,
    last_seq: Option<u16>,
}

#[derive(Default)]
pub struct Mixer {
    streams: HashMap<u32, Stream>,
    volumes: HashMap<u32, f32>,
}

impl Mixer {
    pub fn push(&mut self, user: u32, seq: u16, pcm: &[i16]) {
        let s = self.streams.entry(user).or_insert_with(|| Stream {
            buf: VecDeque::with_capacity(MAX_BUFFER + FRAME_SAMPLES),
            playing: false,
            last_seq: None,
        });
        if let Some(last) = s.last_seq {
            // 乱序/重复包直接丢弃（距离太远则视为重新开始）
            if !seq_newer(seq, last) && last.wrapping_sub(seq) < 100 {
                return;
            }
        }
        s.last_seq = Some(seq);
        s.buf.extend(pcm.iter().map(|&x| x as f32 / 32768.0));
        if s.buf.len() > MAX_BUFFER {
            let drop = s.buf.len() - PREBUFFER;
            s.buf.drain(..drop);
        }
    }

    pub fn set_volume(&mut self, user: u32, v: f32) {
        self.volumes.insert(user, v);
    }
    pub fn volume(&self, user: u32) -> f32 {
        self.volumes.get(&user).copied().unwrap_or(1.0)
    }

    pub fn retain_users(&mut self, alive: impl Fn(u32) -> bool) {
        self.streams.retain(|id, _| *id == LOOPBACK_ID || *id == SFX_ID || alive(*id));
    }

    pub fn clear(&mut self) {
        self.streams.clear();
    }

    /// 取出一个 16 kHz 混音样本
    fn next_sample(&mut self) -> f32 {
        let mut sum = 0.0;
        for (id, s) in self.streams.iter_mut() {
            if !s.playing {
                if s.buf.len() >= PREBUFFER {
                    s.playing = true;
                } else {
                    continue;
                }
            }
            match s.buf.pop_front() {
                Some(x) => sum += x * self.volumes.get(id).copied().unwrap_or(1.0),
                None => s.playing = false, // 欠载，重新预缓冲
            }
        }
        sum
    }
}

/// 软削波，避免多人同时说话时爆音
fn soft_clip(x: f32) -> f32 {
    if x.abs() <= 0.8 {
        x
    } else {
        x.signum() * (0.8 + 0.2 * ((x.abs() - 0.8) / 0.2).tanh())
    }
}

// ---------------------------------------------------------------- 设备

pub fn input_device_names() -> Vec<String> {
    let host = cpal::default_host();
    host.input_devices()
        .map(|it| it.filter_map(|d| d.description().ok().map(|n| n.name().to_string())).collect())
        .unwrap_or_default()
}

pub fn output_device_names() -> Vec<String> {
    let host = cpal::default_host();
    host.output_devices()
        .map(|it| it.filter_map(|d| d.description().ok().map(|n| n.name().to_string())).collect())
        .unwrap_or_default()
}

fn find_device(input: bool, name: Option<&str>) -> Option<cpal::Device> {
    let host = cpal::default_host();
    if let Some(name) = name {
        let found = if input { host.input_devices().ok() } else { host.output_devices().ok() }
            .and_then(|mut it| {
                it.find(|d| d.description().map(|n| n.name() == name).unwrap_or(false))
            });
        if found.is_some() {
            return found;
        }
    }
    if input {
        host.default_input_device()
    } else {
        host.default_output_device()
    }
}

/// 持有音频流；drop 即停止
pub struct AudioStreams {
    _input: Option<cpal::Stream>,
    _output: Option<cpal::Stream>,
    pub input_desc: String,
    pub output_desc: String,
    /// 实际打开的设备名（没打开为空）
    pub input_name: String,
    pub output_name: String,
    /// 设备出错（被拔掉等），需要重新打开
    pub failed: Arc<std::sync::atomic::AtomicBool>,
}

/// 设备快照（后台线程每 2 秒刷新），用来发现拔插和默认设备变化
#[derive(Clone, Default, PartialEq, Eq)]
pub struct DeviceSnapshot {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
}

/// 启动设备监视线程
pub fn watch_devices(ctx: eframe::egui::Context) -> Arc<Mutex<Option<DeviceSnapshot>>> {
    let snap = Arc::new(Mutex::new(None));
    let out = snap.clone();
    let _ = std::thread::Builder::new().name("device-watch".into()).spawn(move || loop {
        let host = cpal::default_host();
        let name = |d: cpal::Device| d.description().ok().map(|n| n.name().to_string());
        let s = DeviceSnapshot {
            inputs: input_device_names(),
            outputs: output_device_names(),
            default_input: host.default_input_device().and_then(name),
            default_output: host.default_output_device().and_then(name),
        };
        let changed = {
            let mut g = out.lock().unwrap();
            let c = g.as_ref() != Some(&s);
            *g = Some(s);
            c
        };
        if changed {
            ctx.request_repaint();
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    });
    snap
}

/// 按用户的选择，现在应该用哪个设备：选的设备还在就用它，否则用系统默认
pub fn wanted_device(choice: Option<&str>, available: &[String], default: Option<&String>) -> Option<String> {
    match choice {
        Some(c) if available.iter().any(|a| a == c) => Some(c.to_string()),
        _ => default.cloned(),
    }
}

pub fn start_streams(
    input_name: Option<&str>,
    output_name: Option<&str>,
    frames_tx: SyncSender<Vec<f32>>,
    mixer: Arc<Mutex<Mixer>>,
    settings: Arc<Settings>,
) -> (AudioStreams, Vec<String>) {
    let mut errors = Vec::new();
    let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (input, input_desc, in_name) = match start_input(input_name, frames_tx, failed.clone()) {
        Ok((s, d, n)) => (Some(s), d, n),
        Err(e) => {
            errors.push(format!("麦克风启动失败：{e}"));
            (None, "无".into(), String::new())
        }
    };
    let (output, output_desc, out_name) = match start_output(output_name, mixer, settings, failed.clone()) {
        Ok((s, d, n)) => (Some(s), d, n),
        Err(e) => {
            errors.push(format!("扬声器启动失败：{e}"));
            (None, "无".into(), String::new())
        }
    };
    (
        AudioStreams { _input: input, _output: output, input_desc, output_desc, input_name: in_name, output_name: out_name, failed },
        errors,
    )
}

fn start_input(
    name: Option<&str>,
    frames_tx: SyncSender<Vec<f32>>,
    failed: Arc<std::sync::atomic::AtomicBool>,
) -> Result<(cpal::Stream, String, String), String> {
    let dev = find_device(true, name).ok_or("找不到输入设备")?;
    let dname = dev.description().map(|d| d.name().to_string()).unwrap_or_default();
    let sup = dev.default_input_config().map_err(|e| e.to_string())?;
    let cfg = sup.config();
    let desc = format!("{dname}（{} Hz, {} 声道）", cfg.sample_rate, cfg.channels);
    let stream = match sup.sample_format() {
        SampleFormat::F32 => build_input::<f32>(&dev, cfg, frames_tx, failed),
        SampleFormat::I16 => build_input::<i16>(&dev, cfg, frames_tx, failed),
        SampleFormat::U16 => build_input::<u16>(&dev, cfg, frames_tx, failed),
        SampleFormat::I32 => build_input::<i32>(&dev, cfg, frames_tx, failed),
        SampleFormat::U8 => build_input::<u8>(&dev, cfg, frames_tx, failed),
        f => return Err(format!("不支持的采样格式 {f:?}")),
    }?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((stream, desc, dname))
}

fn build_input<T>(
    dev: &cpal::Device,
    cfg: cpal::StreamConfig,
    frames_tx: SyncSender<Vec<f32>>,
    failed: Arc<std::sync::atomic::AtomicBool>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let ch = cfg.channels.max(1) as usize;
    let mut rs = PushResampler::new(cfg.sample_rate, CAPTURE_RATE);
    let mut frame = Vec::with_capacity(CAPTURE_FRAME);
    dev.build_input_stream::<T, _, _>(
        cfg,
        move |data: &[T], _| {
            for chunk in data.chunks(ch) {
                let mono = chunk.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / ch as f32;
                rs.push(mono, |y| {
                    frame.push(y);
                    if frame.len() == CAPTURE_FRAME {
                        let full = std::mem::replace(&mut frame, Vec::with_capacity(CAPTURE_FRAME));
                        let _ = frames_tx.try_send(full);
                    }
                });
            }
        },
        move |e| {
            eprintln!("输入流错误：{e}");
            failed.store(true, Ordering::Relaxed);
        },
        None,
    )
    .map_err(|e| e.to_string())
}

fn start_output(
    name: Option<&str>,
    mixer: Arc<Mutex<Mixer>>,
    settings: Arc<Settings>,
    failed: Arc<std::sync::atomic::AtomicBool>,
) -> Result<(cpal::Stream, String, String), String> {
    let dev = find_device(false, name).ok_or("找不到输出设备")?;
    let dname = dev.description().map(|d| d.name().to_string()).unwrap_or_default();
    let sup = dev.default_output_config().map_err(|e| e.to_string())?;
    let cfg = sup.config();
    let desc = format!("{dname}（{} Hz, {} 声道）", cfg.sample_rate, cfg.channels);
    let stream = match sup.sample_format() {
        SampleFormat::F32 => build_output::<f32>(&dev, cfg, mixer, settings, failed),
        SampleFormat::I16 => build_output::<i16>(&dev, cfg, mixer, settings, failed),
        SampleFormat::U16 => build_output::<u16>(&dev, cfg, mixer, settings, failed),
        SampleFormat::I32 => build_output::<i32>(&dev, cfg, mixer, settings, failed),
        SampleFormat::U8 => build_output::<u8>(&dev, cfg, mixer, settings, failed),
        f => return Err(format!("不支持的采样格式 {f:?}")),
    }?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((stream, desc, dname))
}

fn build_output<T>(
    dev: &cpal::Device,
    cfg: cpal::StreamConfig,
    mixer: Arc<Mutex<Mixer>>,
    settings: Arc<Settings>,
    failed: Arc<std::sync::atomic::AtomicBool>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = cfg.channels.max(1) as usize;
    let mut rs = PullResampler::new(SAMPLE_RATE, cfg.sample_rate);
    dev.build_output_stream::<T, _, _>(
        cfg,
        move |data: &mut [T], _| {
            let deaf = settings.deafened.load(Ordering::Relaxed);
            let vol = settings.output_volume.get();
            let Ok(mut m) = mixer.lock() else { return };
            if deaf {
                m.clear();
                for s in data.iter_mut() {
                    *s = T::from_sample(0.0f32);
                }
                return;
            }
            for frame in data.chunks_mut(ch) {
                let v = soft_clip(rs.next(|| m.next_sample()) * vol);
                let s = T::from_sample(v);
                for x in frame.iter_mut() {
                    *x = s;
                }
            }
        },
        move |e| {
            eprintln!("输出流错误：{e}");
            failed.store(true, Ordering::Relaxed);
        },
        None,
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_choice_follows_plug_and_default() {
        let avail = vec!["耳机".to_string(), "扬声器".to_string()];
        let def = Some("扬声器".to_string());
        // 用户选了耳机且耳机在：用耳机
        assert_eq!(wanted_device(Some("耳机"), &avail, def.as_ref()), Some("耳机".into()));
        // 耳机拔了：退回系统默认
        assert_eq!(wanted_device(Some("耳机"), &avail[1..], def.as_ref()), Some("扬声器".into()));
        // 选「系统默认」：跟着系统默认变
        assert_eq!(wanted_device(None, &avail, Some(&"耳机".to_string())), Some("耳机".into()));
        assert_eq!(wanted_device(None, &[], None), None);
    }

    #[test]
    fn chimes() {
        let j = chime(Chime::Join);
        let l = chime(Chime::Leave);
        assert_eq!(j.len() % FRAME_SAMPLES, 0);
        assert!(j.iter().map(|x| x.unsigned_abs()).max().unwrap() > 3000, "audible");
        assert!(j.iter().map(|x| x.unsigned_abs()).max().unwrap() < 12000, "not too loud");
        assert_ne!(j, l, "join and leave sound different");
        // 两声之间有停顿，总长约 0.4 秒
        let secs = j.len() as f32 / SAMPLE_RATE as f32;
        assert!((0.38..0.5).contains(&secs), "{secs}");
        let mid = (0.17 * SAMPLE_RATE as f32) as usize + 200;
        assert!(j[mid..mid + 400].iter().all(|&x| x == 0), "gap between the two notes");
        // 导出试听：VOC_CHIME_WAV=目录 cargo test -p voc-client chimes
        if let Ok(dir) = std::env::var("VOC_CHIME_WAV") {
            for (name, pcm) in [("join.wav", &j), ("leave.wav", &l)] {
                let mut w = Vec::new();
                let data_len = (pcm.len() * 2) as u32;
                w.extend_from_slice(b"RIFF");
                w.extend_from_slice(&(36 + data_len).to_le_bytes());
                w.extend_from_slice(b"WAVEfmt ");
                w.extend_from_slice(&16u32.to_le_bytes());
                w.extend_from_slice(&1u16.to_le_bytes());
                w.extend_from_slice(&1u16.to_le_bytes());
                w.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
                w.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
                w.extend_from_slice(&2u16.to_le_bytes());
                w.extend_from_slice(&16u16.to_le_bytes());
                w.extend_from_slice(b"data");
                w.extend_from_slice(&data_len.to_le_bytes());
                for s in pcm.iter() {
                    w.extend_from_slice(&s.to_le_bytes());
                }
                std::fs::write(std::path::Path::new(&dir).join(name), w).unwrap();
            }
        }
    }

    #[test]
    fn resample_ratio() {
        let mut rs = PushResampler::new(48000, 16000);
        let mut n = 0;
        for _ in 0..48000 {
            rs.push(0.5, |_| n += 1);
        }
        assert!((15990..=16010).contains(&n), "{n}");

        let mut pr = PullResampler::new(16000, 48000);
        let mut pulled = 0;
        for _ in 0..48000 {
            pr.next(|| {
                pulled += 1;
                0.0
            });
        }
        assert!((15990..=16010).contains(&pulled), "{pulled}");
    }

    #[test]
    fn mixer_prebuffer_and_order() {
        let mut m = Mixer::default();
        let f = vec![16384i16; FRAME_SAMPLES];
        m.push(5, 1, &f);
        assert_eq!(m.next_sample(), 0.0); // 预缓冲未满
        m.push(5, 0, &f); // 旧包丢弃
        m.push(5, 2, &f);
        m.push(5, 3, &f);
        assert!((m.next_sample() - 0.5).abs() < 1e-6);
    }
}
