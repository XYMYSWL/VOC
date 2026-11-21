//! 音频设备、重采样、抖动缓冲与混音。

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use voc_proto::{udp::seq_newer, FRAME_SAMPLES, SAMPLE_RATE};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

/// 采集端输出 48 kHz 单声道、10 ms 一帧（RNNoise / Krisp 都用这个格式），
/// 编码和混音也是 48 kHz，不再重采样。
pub const CAPTURE_RATE: u32 = 48_000;
pub const CAPTURE_FRAME: usize = 480;

/// 本地回放（麦克风测试）在混音器里使用的 id
pub const LOOPBACK_ID: u32 = 0;
/// 提示音在混音器里使用的 id
pub const SFX_ID: u32 = u32::MAX;
/// 伴奏、屏幕共享声音在混音器里的 id：用户 id 加上标志位（每人各一路，音量单独调）
pub const MUSIC_FLAG: u32 = 0x4000_0000;
pub const SHARE_FLAG: u32 = 0x2000_0000;
const USER_MASK: u32 = 0x0FFF_FFFF;

/// kind = udp::MUSIC / udp::SHARE_AUDIO
pub fn stream_mixer_id(kind: u8, user: u32) -> u32 {
    if kind == voc_proto::udp::MUSIC {
        MUSIC_FLAG | user
    } else {
        SHARE_FLAG | user
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Chime {
    /// 有人进入频道：上行两音
    Join,
    /// 有人离开频道：下行两音
    Leave,
    /// 快捷键闭麦 / 静音：一声低音
    MuteOn,
    /// 快捷键开麦 / 取消静音：一声高音
    MuteOff,
}

/// 合成提示音（混音采样率，单声道）：两个带包络的柔和正弦音，叠一点二次谐波让声音更圆润
pub fn chime(kind: Chime) -> Vec<i16> {
    // 「噔、噔」两声：每声约 0.17 秒，中间停 0.06 秒；进入低→高，离开高→低
    let (a, b) = (659.25f32, 987.77f32); // E5、B5
    let notes: &[f32] = match kind {
        Chime::Join => &[a, b],
        Chime::Leave => &[b, a],
        Chime::MuteOn => &[440.0],
        Chime::MuteOff => &[a],
    };
    const NOTE: f32 = 0.17;
    const GAP: f32 = 0.06;
    let sr = SAMPLE_RATE as f32;
    let mut out = Vec::new();
    for (k, &freq) in notes.iter().enumerate() {
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
/// 低带宽模式：语音 / 伴奏降码率，屏幕共享最高 480P 30 帧，观看时默认低画质
pub static LOW_BANDWIDTH: AtomicBool = AtomicBool::new(false);

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
    /// 回声消除
    pub aec: AtomicBool,
    /// 按键说话的按键（Windows 虚拟键码）；0 = 语音激活
    pub ptt_vk: AtomicU32,
    /// 麦克风连续几秒完全没有声音（Windows 隐私设置关了麦克风等）
    pub mic_silent: AtomicBool,
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
            aec: AtomicBool::new(true),
            ptt_vk: AtomicU32::new(0),
            mic_silent: AtomicBool::new(false),
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

/// 每帧时长（ms）
const FRAME_MS: f32 = FRAME_SAMPLES as f32 * 1000.0 / SAMPLE_RATE as f32;
/// 抖动缓冲目标的上下限（ms）
const JB_MIN_MS: f32 = 20.0;
const JB_MAX_MS: f32 = 160.0;
/// 提示音 / 麦克风测试这种本地声音：固定 20 ms
const LOCAL_PREBUFFER: usize = FRAME_SAMPLES;
/// 缓冲比目标多出这么多（ms）就丢掉多余部分，把延迟拉回来
const JB_EXCESS_MS: f32 = 60.0;

/// 自适应抖动缓冲：按到达间隔的抖动决定先攒多少再播。
/// 目标 = 一帧 + 3 × 抖动估计 + 欠载补偿；欠载（播空）一次补偿加 20 ms，稳定时每秒减 1 ms。
#[derive(Clone, Debug)]
pub struct JitterEstimator {
    last_arrival: Option<std::time::Instant>,
    /// 到达间隔偏差的平滑估计（ms），算法同 RFC 3550
    jitter_ms: f32,
    /// 欠载后额外加的缓冲（ms）
    penalty_ms: f32,
    last_decay: std::time::Instant,
}

impl Default for JitterEstimator {
    fn default() -> Self {
        Self { last_arrival: None, jitter_ms: 0.0, penalty_ms: 0.0, last_decay: std::time::Instant::now() }
    }
}

impl JitterEstimator {
    /// 收到 `frames` 帧（丢包补出来的帧会一次到好几帧）
    pub fn on_arrival(&mut self, now: std::time::Instant, frames: usize) {
        if let Some(last) = self.last_arrival {
            let gap = now.duration_since(last).as_secs_f32() * 1000.0;
            // 说话停顿后的第一包不算抖动
            if gap < 400.0 {
                let expected = FRAME_MS * frames.max(1) as f32;
                let d = (gap - expected).abs();
                self.jitter_ms += (d - self.jitter_ms) / 16.0;
            }
        }
        self.last_arrival = Some(now);
        let secs = now.duration_since(self.last_decay).as_secs_f32();
        if secs >= 1.0 {
            self.penalty_ms = (self.penalty_ms - secs).max(0.0);
            self.last_decay = now;
        }
    }

    pub fn on_underrun(&mut self) {
        self.penalty_ms = (self.penalty_ms + 20.0).min(100.0);
    }

    /// 当前的缓冲目标（ms）
    pub fn target_ms(&self) -> f32 {
        (FRAME_MS + 3.0 * self.jitter_ms + self.penalty_ms).clamp(JB_MIN_MS, JB_MAX_MS)
    }
}

fn ms_to_samples(ms: f32) -> usize {
    (ms * SAMPLE_RATE as f32 / 1000.0) as usize
}

struct Stream {
    buf: VecDeque<f32>,
    playing: bool,
    last_seq: Option<u16>,
    jitter: JitterEstimator,
    /// 本地声音（提示音 / 麦克风测试），不做自适应
    local: bool,
}

impl Stream {
    fn prebuffer(&self) -> usize {
        if self.local {
            LOCAL_PREBUFFER
        } else {
            ms_to_samples(self.jitter.target_ms())
        }
    }
}

#[derive(Default)]
pub struct Mixer {
    streams: HashMap<u32, Stream>,
    volumes: HashMap<u32, f32>,
    /// 暂时不播的通道（例如正在看某人带声音的屏幕共享时，他的伴奏已经在共享声音里了）
    suppressed: std::collections::HashSet<u32>,
}

impl Mixer {
    pub fn push(&mut self, user: u32, seq: u16, pcm: &[i16]) {
        let s = self.streams.entry(user).or_insert_with(|| Stream {
            buf: VecDeque::with_capacity(FRAME_SAMPLES * 12),
            playing: false,
            last_seq: None,
            jitter: JitterEstimator::default(),
            local: user == LOOPBACK_ID || user == SFX_ID,
        });
        if let Some(last) = s.last_seq {
            // 乱序/重复包直接丢弃（距离太远则视为重新开始）
            if !seq_newer(seq, last) && last.wrapping_sub(seq) < 100 {
                return;
            }
        }
        s.last_seq = Some(seq);
        if !s.local {
            s.jitter.on_arrival(std::time::Instant::now(), pcm.len().div_ceil(FRAME_SAMPLES));
        }
        s.buf.extend(pcm.iter().map(|&x| x as f32 / 32768.0));
        // 攒得比目标多太多（网络恢复后一下子到了一堆）：丢掉旧的，把延迟拉回目标
        if !s.local {
            let target = s.prebuffer();
            if s.buf.len() > target + ms_to_samples(JB_EXCESS_MS) {
                let drop = s.buf.len() - target;
                s.buf.drain(..drop);
            }
        } else if s.buf.len() > FRAME_SAMPLES * 50 {
            let drop = s.buf.len() - FRAME_SAMPLES * 50;
            s.buf.drain(..drop);
        }
    }

    /// 某人当前的抖动缓冲目标（ms），显示延迟用
    pub fn buffer_ms(&self, user: u32) -> Option<u32> {
        self.streams.get(&user).map(|s| s.jitter.target_ms().round() as u32)
    }

    pub fn set_volume(&mut self, user: u32, v: f32) {
        self.volumes.insert(user, v);
    }
    pub fn volume(&self, user: u32) -> f32 {
        self.volumes.get(&user).copied().unwrap_or(1.0)
    }

    pub fn retain_users(&mut self, alive: impl Fn(u32) -> bool) {
        self.streams.retain(|id, _| *id == LOOPBACK_ID || *id == SFX_ID || alive(*id & USER_MASK));
    }

    pub fn set_suppressed(&mut self, ids: std::collections::HashSet<u32>) {
        self.suppressed = ids;
    }

    pub fn clear(&mut self) {
        self.streams.clear();
    }

    /// 取出一个混音样本
    fn next_sample(&mut self) -> f32 {
        let mut sum = 0.0;
        for (id, s) in self.streams.iter_mut() {
            if self.suppressed.contains(id) {
                s.buf.clear();
                s.playing = false;
                continue;
            }
            if !s.playing {
                if s.buf.len() >= s.prebuffer() {
                    s.playing = true;
                } else {
                    continue;
                }
            }
            match s.buf.pop_front() {
                Some(x) => sum += x * self.volumes.get(id).copied().unwrap_or(1.0),
                None => {
                    // 欠载：重新预缓冲。距上一包很近说明是网络抖动（不是说完话），加大缓冲
                    s.playing = false;
                    if !s.local && s.jitter.last_arrival.is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(200)) {
                        s.jitter.on_underrun();
                    }
                }
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

/// 声卡缓冲目标：10 ms（越小延迟越低；设备不支持时退回系统默认）
const DEVICE_BUFFER_MS: u32 = 10;

/// 按设备支持的范围选一个约 10 ms 的固定缓冲
fn low_latency_buffer(sup: &cpal::SupportedStreamConfig) -> cpal::BufferSize {
    let want = sup.sample_rate() * DEVICE_BUFFER_MS / 1000;
    match sup.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => cpal::BufferSize::Fixed(want.clamp(*min, *max)),
        cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Fixed(want),
    }
}

/// 把当前线程登记为 Windows「专业音频」多媒体线程（MMCSS），调度优先级更高，小缓冲下不容易断音
pub fn boost_audio_thread() {
    #[cfg(windows)]
    unsafe {
        use windows::core::w;
        use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
        let mut idx = 0u32;
        let _ = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut idx);
    }
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
    /// 缓冲欠载 / 溢出次数（小缓冲扛不住时退回默认缓冲）
    pub xruns: Arc<AtomicU32>,
    /// 用的是 10 ms 小缓冲
    pub small_buffer: bool,
}

/// 设备错误的去向：只有设备没了 / 失效才需要重新打开；欠载溢出只计数
#[derive(Clone)]
pub struct StreamHealth {
    failed: Arc<std::sync::atomic::AtomicBool>,
    xruns: Arc<AtomicU32>,
}

impl StreamHealth {
    fn report(&self, e: &cpal::Error) {
        use cpal::ErrorKind as K;
        match e.kind() {
            K::Xrun => {
                self.xruns.fetch_add(1, Ordering::Relaxed);
            }
            K::RealtimeDenied => {}
            K::DeviceNotAvailable | K::StreamInvalidated | K::DeviceChanged | K::BackendError | K::Other => {
                eprintln!("音频设备错误：{e}");
                crate::applog::log(format!("音频设备错误：{e}"));
                self.failed.store(true, Ordering::Relaxed);
            }
            _ => eprintln!("音频设备错误：{e}"),
        }
    }
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

#[cfg(test)]
pub fn start_streams(
    input_name: Option<&str>,
    output_name: Option<&str>,
    frames_tx: SyncSender<Vec<f32>>,
    mixer: Arc<Mutex<Mixer>>,
    settings: Arc<Settings>,
) -> (AudioStreams, Vec<String>) {
    start_streams_with(input_name, output_name, frames_tx, mixer, settings, true)
}

/// small = 先试 10 ms 小缓冲（扛不住时调用方传 false 重开）
pub fn start_streams_with(
    input_name: Option<&str>,
    output_name: Option<&str>,
    frames_tx: SyncSender<Vec<f32>>,
    mixer: Arc<Mutex<Mixer>>,
    settings: Arc<Settings>,
    small: bool,
) -> (AudioStreams, Vec<String>) {
    let mut errors = Vec::new();
    let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let xruns = Arc::new(AtomicU32::new(0));
    let health = StreamHealth { failed: failed.clone(), xruns: xruns.clone() };
    let (input, input_desc, in_name) = match start_input(input_name, frames_tx, health.clone(), small) {
        Ok((s, d, n)) => (Some(s), d, n),
        Err(e) => {
            errors.push(format!("麦克风启动失败：{e}"));
            (None, "无".into(), String::new())
        }
    };
    let (output, output_desc, out_name) = match start_output(output_name, mixer, settings, health, small) {
        Ok((s, d, n)) => (Some(s), d, n),
        Err(e) => {
            errors.push(format!("扬声器启动失败：{e}"));
            (None, "无".into(), String::new())
        }
    };
    (
        AudioStreams {
            _input: input,
            _output: output,
            input_desc,
            output_desc,
            input_name: in_name,
            output_name: out_name,
            failed,
            xruns,
            small_buffer: small,
        },
        errors,
    )
}

fn start_input(
    name: Option<&str>,
    frames_tx: SyncSender<Vec<f32>>,
    health: StreamHealth,
    small: bool,
) -> Result<(cpal::Stream, String, String), String> {
    let dev = find_device(true, name).ok_or("找不到输入设备")?;
    let dname = dev.description().map(|d| d.name().to_string()).unwrap_or_default();
    let sup = dev.default_input_config().map_err(|e| e.to_string())?;
    let mut cfg = sup.config();
    let build = |cfg: cpal::StreamConfig| match sup.sample_format() {
        SampleFormat::F32 => build_input::<f32>(&dev, cfg, frames_tx.clone(), health.clone()),
        SampleFormat::I16 => build_input::<i16>(&dev, cfg, frames_tx.clone(), health.clone()),
        SampleFormat::U16 => build_input::<u16>(&dev, cfg, frames_tx.clone(), health.clone()),
        SampleFormat::I32 => build_input::<i32>(&dev, cfg, frames_tx.clone(), health.clone()),
        SampleFormat::U8 => build_input::<u8>(&dev, cfg, frames_tx.clone(), health.clone()),
        f => Err(format!("不支持的采样格式 {f:?}")),
    };
    // 先试 10 ms 小缓冲，设备不接受就用默认
    let small_cfg = cpal::StreamConfig { buffer_size: low_latency_buffer(&sup), ..cfg.clone() };
    let (stream, buf_desc) = match small.then(|| build(small_cfg)).and_then(|r| r.ok()) {
        Some(s) => (s, format!("{DEVICE_BUFFER_MS} ms 缓冲")),
        None => {
            cfg.buffer_size = cpal::BufferSize::Default;
            (build(cfg.clone())?, "默认缓冲".to_string())
        }
    };
    let desc = format!("{dname}（{} Hz, {} 声道，{buf_desc}）", cfg.sample_rate, cfg.channels);
    stream.play().map_err(|e| e.to_string())?;
    Ok((stream, desc, dname))
}

fn build_input<T>(
    dev: &cpal::Device,
    cfg: cpal::StreamConfig,
    frames_tx: SyncSender<Vec<f32>>,
    health: StreamHealth,
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
        move |e| health.report(&e),
        None,
    )
    .map_err(|e| e.to_string())
}

fn start_output(
    name: Option<&str>,
    mixer: Arc<Mutex<Mixer>>,
    settings: Arc<Settings>,
    health: StreamHealth,
    small: bool,
) -> Result<(cpal::Stream, String, String), String> {
    let dev = find_device(false, name).ok_or("找不到输出设备")?;
    let dname = dev.description().map(|d| d.name().to_string()).unwrap_or_default();
    let sup = dev.default_output_config().map_err(|e| e.to_string())?;
    let mut cfg = sup.config();
    let build = |cfg: cpal::StreamConfig| match sup.sample_format() {
        SampleFormat::F32 => build_output::<f32>(&dev, cfg, mixer.clone(), settings.clone(), health.clone()),
        SampleFormat::I16 => build_output::<i16>(&dev, cfg, mixer.clone(), settings.clone(), health.clone()),
        SampleFormat::U16 => build_output::<u16>(&dev, cfg, mixer.clone(), settings.clone(), health.clone()),
        SampleFormat::I32 => build_output::<i32>(&dev, cfg, mixer.clone(), settings.clone(), health.clone()),
        SampleFormat::U8 => build_output::<u8>(&dev, cfg, mixer.clone(), settings.clone(), health.clone()),
        f => Err(format!("不支持的采样格式 {f:?}")),
    };
    let small_cfg = cpal::StreamConfig { buffer_size: low_latency_buffer(&sup), ..cfg.clone() };
    let (stream, buf_desc) = match small.then(|| build(small_cfg)).and_then(|r| r.ok()) {
        Some(s) => (s, format!("{DEVICE_BUFFER_MS} ms 缓冲")),
        None => {
            cfg.buffer_size = cpal::BufferSize::Default;
            (build(cfg.clone())?, "默认缓冲".to_string())
        }
    };
    let desc = format!("{dname}（{} Hz, {} 声道，{buf_desc}）", cfg.sample_rate, cfg.channels);
    stream.play().map_err(|e| e.to_string())?;
    Ok((stream, desc, dname))
}

fn build_output<T>(
    dev: &cpal::Device,
    cfg: cpal::StreamConfig,
    mixer: Arc<Mutex<Mixer>>,
    settings: Arc<Settings>,
    health: StreamHealth,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = cfg.channels.max(1) as usize;
    let mut rs = PullResampler::new(SAMPLE_RATE, cfg.sample_rate);
    let mut tap: Vec<f32> = Vec::with_capacity(crate::aec::AEC_FRAME);
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
                let v = soft_clip(
                    rs.next(|| {
                        let x = m.next_sample();
                        // 回声消除的远端参考：按播放音量，10 ms 一帧
                        tap.push(x * vol);
                        if tap.len() == crate::aec::AEC_FRAME {
                            crate::aec::push_render(std::mem::replace(&mut tap, Vec::with_capacity(crate::aec::AEC_FRAME)));
                        }
                        x
                    }) * vol,
                );
                let s = T::from_sample(v);
                for x in frame.iter_mut() {
                    *x = s;
                }
            }
        },
        move |e| health.report(&e),
        None,
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实设备：cargo test -p voc-client real_device_buffers -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_device_buffers() {
        let (tx, _rx) = std::sync::mpsc::sync_channel(64);
        let (st, errs) = start_streams(None, None, tx, Arc::new(Mutex::new(Mixer::default())), Arc::new(Settings::default()));
        println!("输入：{}
输出：{}
错误：{errs:?}", st.input_desc, st.output_desc);
        std::thread::sleep(std::time::Duration::from_secs(5));
        println!("5 秒内欠载 / 溢出：{} 次", st.xruns.load(Ordering::Relaxed));
        assert!(!st.failed.load(Ordering::Relaxed));
    }

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
        let half = vec![16384i16; FRAME_SAMPLES / 2];
        m.push(5, 1, &half);
        assert_eq!(m.next_sample(), 0.0); // 不到一帧（20 ms）：还在预缓冲
        m.push(5, 0, &f); // 旧包丢弃
        // 测试里两包是同时到的（真实网络约隔 20 ms），会算出一点抖动，所以攒到一帧多一点就开播
        m.push(5, 2, &f);
        assert!((m.next_sample() - 0.5).abs() < 1e-6);
        assert!(m.buffer_ms(5).unwrap() <= 25);
    }

    #[test]
    fn jitter_buffer_adapts() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        // 每 20 ms 准时到：目标保持最小 20 ms
        let mut j = JitterEstimator::default();
        for i in 0..100u64 {
            j.on_arrival(t0 + Duration::from_millis(20 * i), 1);
        }
        assert_eq!(j.target_ms().round() as u32, 20);
        // 到达时间忽早忽晚 ±15 ms：目标明显变大，但不超过上限
        let mut j = JitterEstimator::default();
        for i in 0..200u64 {
            let wobble = if i % 2 == 0 { 0 } else { 30 };
            j.on_arrival(t0 + Duration::from_millis(20 * i + wobble), 1);
        }
        let t = j.target_ms();
        assert!(t > 60.0 && t <= JB_MAX_MS, "{t}");
        // 欠载一次多攒 20 ms
        let before = j.target_ms();
        j.on_underrun();
        assert!(j.target_ms() >= (before + 19.0).min(JB_MAX_MS));
        // 说话停顿后的第一包不算抖动
        let mut j = JitterEstimator::default();
        j.on_arrival(t0, 1);
        j.on_arrival(t0 + Duration::from_secs(3), 1);
        assert_eq!(j.target_ms().round() as u32, 20);
    }

    #[test]
    fn jitter_buffer_drains_excess() {
        let mut m = Mixer::default();
        let f = vec![1000i16; FRAME_SAMPLES];
        // 网络卡了一下，一次到了 10 帧（200 ms）：只保留目标量，不把延迟一直拖着
        for seq in 0..10u16 {
            m.push(9, seq, &f);
        }
        let len = m.streams[&9].buf.len();
        assert!(len <= ms_to_samples(m.streams[&9].jitter.target_ms() + JB_EXCESS_MS), "{len}");
    }
}
