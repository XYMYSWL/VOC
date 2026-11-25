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
/// 语音码率（bps，设置 → 音频「语音音质」，第 68 项）
pub static VOICE_BITRATE: AtomicU32 = AtomicU32::new(96_000);

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
    /// 按键说话的按键（hotkey::Hotkey::pack）；0 = 语音激活
    pub ptt_key: AtomicU32,
    /// 松开说话键后再多发多久（毫秒）
    pub ptt_tail_ms: AtomicU32,
    /// 正在录键：按键说话也暂停（第 58 项）
    pub keys_paused: AtomicBool,
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
            ptt_key: AtomicU32::new(0),
            ptt_tail_ms: AtomicU32::new(200),
            keys_paused: AtomicBool::new(false),
            mic_silent: AtomicBool::new(false),
        }
    }
}

// ---------------------------------------------------------------- 重采样

/// 带限插值（加 Kaiser 窗的 sinc）的系数表：SINC_PHASES 个相位 × SINC_TAPS 个点。
/// 截止频率取两边采样率中较低一边的 45%：降采样时滤掉会折回来的高频（原来的直线插值没滤，音乐里听得出「沙沙」底噪，第 55-a 项），
/// 升采样时滤掉镜像
const SINC_TAPS: usize = 32;
const SINC_PHASES: usize = 256;

struct SincTable {
    /// [相位][点]，相位 p 对应小数位置 p / SINC_PHASES
    coef: Vec<[f32; SINC_TAPS]>,
}

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    let q = x * x / 4.0;
    for k in 1..50 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < 1e-12 * sum {
            break;
        }
    }
    sum
}

impl SincTable {
    /// cutoff：截止频率占输入采样率的比例（< 0.5）
    fn new(cutoff: f64) -> Self {
        const BETA: f64 = 8.0;
        let half = (SINC_TAPS / 2) as f64;
        let i0b = bessel_i0(BETA);
        let coef = (0..=SINC_PHASES)
            .map(|p| {
                let frac = p as f64 / SINC_PHASES as f64;
                let mut row = [0f32; SINC_TAPS];
                let mut sum = 0.0;
                let mut vals = [0f64; SINC_TAPS];
                for (k, v) in vals.iter_mut().enumerate() {
                    // 点 k 对应输入样本 n - half + 1 + k，离插值位置 n + frac 的距离
                    let t = k as f64 - (half - 1.0) - frac;
                    let x = 2.0 * cutoff * t;
                    let sinc = if x.abs() < 1e-9 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
                    let r = t / half;
                    let w = if r.abs() >= 1.0 { 0.0 } else { bessel_i0(BETA * (1.0 - r * r).sqrt()) / i0b };
                    *v = sinc * w;
                    sum += *v;
                }
                // 每个相位单独归一化：直流增益正好 1，不会随相位起伏（否则会调制出噪声）
                for (o, v) in row.iter_mut().zip(vals) {
                    *o = (v / sum) as f32;
                }
                row
            })
            .collect();
        Self { coef }
    }

    /// hist：最近 SINC_TAPS 个输入（最旧的在前），在倒数第 SINC_TAPS/2 个样本之后 frac 处插值
    fn interp(&self, hist: &[f32; SINC_TAPS], frac: f64) -> f32 {
        let p = frac * SINC_PHASES as f64;
        let i = (p as usize).min(SINC_PHASES - 1);
        let t = (p - i as f64) as f32;
        let (a, b) = (&self.coef[i], &self.coef[i + 1]);
        let mut s = 0.0f32;
        for k in 0..SINC_TAPS {
            s += hist[k] * (a[k] + (b[k] - a[k]) * t);
        }
        s
    }
}

fn cutoff_for(in_rate: u32, out_rate: u32) -> f64 {
    0.45 * in_rate.min(out_rate) as f64 / in_rate as f64
}

fn push_hist(hist: &mut [f32; SINC_TAPS], x: f32) {
    hist.copy_within(1.., 0);
    hist[SINC_TAPS - 1] = x;
}

/// 推式重采样（采集端：设备采样率 -> 48 kHz）。两边采样率一样时原样输出
pub struct PushResampler {
    step: f64,
    pos: f64,
    hist: [f32; SINC_TAPS],
    table: Option<SincTable>,
}

impl PushResampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        let table = (in_rate != out_rate).then(|| SincTable::new(cutoff_for(in_rate, out_rate)));
        Self { step: in_rate as f64 / out_rate as f64, pos: 0.0, hist: [0.0; SINC_TAPS], table }
    }

    /// 处理一个输入样本，可能产生 0 个或多个输出样本
    pub fn push(&mut self, x: f32, mut out: impl FnMut(f32)) {
        let Some(t) = &self.table else {
            out(x);
            return;
        };
        push_hist(&mut self.hist, x);
        // pos ∈ [0,1)：相对 hist 里插值基准样本的位置
        while self.pos < 1.0 {
            out(t.interp(&self.hist, self.pos));
            self.pos += self.step;
        }
        self.pos -= 1.0;
    }
}

/// 拉式重采样（播放端：48 kHz -> 设备采样率，左右两路）。两边采样率一样时原样输出
pub struct PullResampler {
    step: f64,
    pos: f64,
    hist: [[f32; SINC_TAPS]; 2],
    table: Option<SincTable>,
}

impl PullResampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        let table = (in_rate != out_rate).then(|| SincTable::new(cutoff_for(in_rate, out_rate)));
        Self { step: in_rate as f64 / out_rate as f64, pos: 1.0, hist: [[0.0; SINC_TAPS]; 2], table }
    }
    /// 单声道
    #[cfg(test)]
    pub fn next(&mut self, mut src: impl FnMut() -> f32) -> f32 {
        self.next2(|| {
            let v = src();
            [v, v]
        })[0]
    }
    pub fn next2(&mut self, mut src: impl FnMut() -> [f32; 2]) -> [f32; 2] {
        let Some(t) = &self.table else { return src() };
        while self.pos >= 1.0 {
            let x = src();
            push_hist(&mut self.hist[0], x[0]);
            push_hist(&mut self.hist[1], x[1]);
            self.pos -= 1.0;
        }
        let v = [t.interp(&self.hist[0], self.pos), t.interp(&self.hist[1], self.pos)];
        self.pos += self.step;
        v
    }
}

// ---------------------------------------------------------------- 混音

/// 每帧时长（ms）
const FRAME_MS: f32 = FRAME_SAMPLES as f32 * 1000.0 / SAMPLE_RATE as f32;
/// 抖动缓冲目标的上下限（ms）：语音
const JB_MIN_MS: f32 = 20.0;
const JB_MAX_MS: f32 = 160.0;
/// 伴奏 / 共享声音：连续的声音，多缓冲几十毫秒换不断音（第 54-b 项）
const STREAM_JB_MIN_MS: f32 = 70.0;
const STREAM_JB_MAX_MS: f32 = 250.0;
/// 提示音 / 麦克风测试这种本地声音：固定 20 ms
const LOCAL_PREBUFFER: usize = FRAME_SAMPLES;
/// 缓冲比目标多出这么多（ms）才直接丢掉多余部分（带淡入淡出）；少于这个靠稍微加快播放慢慢吃掉
const JB_EXCESS_MS: f32 = 200.0;
const STREAM_EXCESS_MS: f32 = 400.0;
/// 变速播放（第 36-2 / 54-b 项）：按「缓冲 - 目标」微调播放速度。语音最多快 / 慢 3%（几乎听不出），
/// 连续的音乐最多 0.5%（主要吃掉两台电脑声卡时钟的差别，音高变化听不出）
const VOICE_MAX_RATE: f64 = 0.03;
const STREAM_MAX_RATE: f64 = 0.005;
/// 播空时淡出、恢复时淡入的时长（样本）：约 4 ms，不再「啪」一声
const FADE_SAMPLES: f32 = 192.0;

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
    min_ms: f32,
    max_ms: f32,
}

impl Default for JitterEstimator {
    fn default() -> Self {
        Self::with_range(JB_MIN_MS, JB_MAX_MS)
    }
}

impl JitterEstimator {
    fn with_range(min_ms: f32, max_ms: f32) -> Self {
        Self { last_arrival: None, jitter_ms: 0.0, penalty_ms: 0.0, last_decay: std::time::Instant::now(), min_ms, max_ms }
    }

    /// 收到 `samples` 个样本的声音（10 / 20 ms 一包；丢包补出来的会一次到好几包）
    pub fn on_arrival(&mut self, now: std::time::Instant, samples: usize) {
        if let Some(last) = self.last_arrival {
            let gap = now.duration_since(last).as_secs_f32() * 1000.0;
            // 说话停顿后的第一包不算抖动
            if gap < 400.0 {
                let expected = samples.max(1) as f32 * 1000.0 / SAMPLE_RATE as f32;
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
        (FRAME_MS + 3.0 * self.jitter_ms + self.penalty_ms).clamp(self.min_ms, self.max_ms)
    }
}

fn ms_to_samples(ms: f32) -> usize {
    (ms * SAMPLE_RATE as f32 / 1000.0) as usize
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// 本地声音（提示音 / 麦克风测试），不做自适应
    Local,
    Voice,
    /// 伴奏 / 共享声音
    Stream,
}

/// 一路声音的播放统计（第 54-d 项）
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StreamStats {
    /// 网络原因播空的次数（说完话停下不算）
    pub underruns: u32,
    /// 丢包后靠补音（DRED / FEC / PLC）补出来的帧
    pub concealed: u32,
    /// 积压太多直接丢掉的次数
    pub drops: u32,
    /// 当前缓冲（ms）和目标（ms）
    pub buffered_ms: u32,
    pub target_ms: u32,
    /// 当前播放速度偏差（千分之几，正 = 加快）
    pub rate_permille: i32,
    /// 音量开得太大、峰值被限幅器压低的次数（压低超过 3 dB 算一次，第 57 项）和最近一次的时间
    pub limited: u32,
    #[doc(hidden)]
    pub limited_recently: bool,
}

/// 一个立体声样本（左, 右）；单声道的声音两边一样
type Frame2 = [f32; 2];

struct Stream {
    buf: VecDeque<Frame2>,
    playing: bool,
    last_seq: Option<u16>,
    jitter: JitterEstimator,
    kind: Kind,
    /// 变速播放：读位置的小数部分、上一个已读样本（插值要用）、当前速度
    frac: f64,
    prev: Frame2,
    rate: f64,
    /// 淡入增益（0 → 1）、播空后淡出的尾巴
    gain: f32,
    tail: Frame2,
    last_out: Frame2,
    stats: StreamStats,
    /// 放大后的限幅器（第 57 项）：音量 > 100% 时不再把峰值削平
    limiter: Limiter,
    /// 额外多缓冲多久（ms）：观看屏幕共享开了缓冲时，共享声音跟着画面晚这么多（第 78 项，音画同步）
    extra_ms: f32,
}

impl Stream {
    fn new(kind: Kind) -> Self {
        let jitter = match kind {
            Kind::Stream => JitterEstimator::with_range(STREAM_JB_MIN_MS, STREAM_JB_MAX_MS),
            _ => JitterEstimator::default(),
        };
        Self {
            buf: VecDeque::with_capacity(FRAME_SAMPLES * 16),
            playing: false,
            last_seq: None,
            jitter,
            kind,
            frac: 0.0,
            prev: [0.0; 2],
            rate: 1.0,
            gain: 0.0,
            tail: [0.0; 2],
            last_out: [0.0; 2],
            stats: StreamStats::default(),
            limiter: Limiter::new(LIMIT_THRESHOLD),
            extra_ms: 0.0,
        }
    }

    fn prebuffer(&self) -> usize {
        match self.kind {
            Kind::Local => LOCAL_PREBUFFER,
            _ => ms_to_samples(self.jitter.target_ms() + self.extra_ms),
        }
    }

    /// 改额外缓冲（第 78 项）。差得不多靠稍微变速慢慢跟上；一下子加了很多：淡出、静音一段攒够再淡入（和画面开缓冲那一下同时）；
    /// 一下子减了很多：丢掉多出来的那段（旧的淡出、新的淡入），不让声音落在画面后面
    fn set_extra(&mut self, ms: f32) {
        let old = self.extra_ms;
        self.extra_ms = ms;
        if ms > old + 40.0 && self.playing {
            self.playing = false;
            self.tail = self.last_out;
        } else if ms < old - 40.0 {
            let drop = ms_to_samples(old - ms).min(self.buf.len().saturating_sub(3));
            if drop > 0 {
                self.buf.drain(..drop);
                self.tail = self.last_out;
                self.gain = 0.0;
            }
        }
    }

    /// 按缓冲多少调播放速度（每收到一包调一次）
    fn update_rate(&mut self) {
        if self.kind == Kind::Local {
            self.rate = 1.0;
            return;
        }
        let target = self.prebuffer().max(1) as f64;
        let err = (self.buf.len() as f64 - target) / target;
        let max = if self.kind == Kind::Stream { STREAM_MAX_RATE } else { VOICE_MAX_RATE };
        // 多了 50% 以上时用满最大加速；少了就放慢（最多一半的幅度，避免越放越空）
        let want = (err * max * 2.0).clamp(-max / 2.0, max);
        self.rate += (1.0 + want - self.rate) * 0.2;
        self.stats.rate_permille = ((self.rate - 1.0) * 1000.0).round() as i32;
    }

    /// 读一个样本（带变速插值、淡入淡出）。None = 这一路现在没声音
    fn next(&mut self) -> Option<Frame2> {
        if !self.playing {
            if self.buf.len() >= self.prebuffer().max(4) {
                self.playing = true;
                self.gain = 0.0;
            } else {
                return self.fade_tail();
            }
        }
        // 4 点 Hermite 插值要 prev、buf[0..3]
        if self.buf.len() < 3 {
            // 播空：重新预缓冲。距上一包很近说明是网络抖动（不是说完话），加大缓冲
            self.playing = false;
            if self.kind != Kind::Local && self.jitter.last_arrival.is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(200)) {
                self.jitter.on_underrun();
                self.stats.underruns += 1;
            }
            self.tail = self.last_out;
            return self.fade_tail();
        }
        let t = self.frac as f32;
        let mut out = [0.0f32; 2];
        for (c, o) in out.iter_mut().enumerate() {
            let (xm1, x0, x1, x2) = (self.prev[c], self.buf[0][c], self.buf[1][c], self.buf[2][c]);
            let c1 = 0.5 * (x1 - xm1);
            let c2 = xm1 - 2.5 * x0 + 2.0 * x1 - 0.5 * x2;
            let c3 = 0.5 * (x2 - xm1) + 1.5 * (x0 - x1);
            *o = ((c3 * t + c2) * t + c1) * t + x0;
        }
        self.frac += self.rate;
        while self.frac >= 1.0 {
            self.prev = self.buf.pop_front().unwrap_or([0.0; 2]);
            self.frac -= 1.0;
        }
        if self.gain < 1.0 {
            self.gain = (self.gain + 1.0 / FADE_SAMPLES).min(1.0);
        }
        let out = [out[0] * self.gain, out[1] * self.gain];
        self.last_out = out;
        // 恢复播放时上一段的尾巴还没淡完：叠在一起（交叉淡化）
        let tail = self.fade_tail().unwrap_or([0.0; 2]);
        Some([out[0] + tail[0], out[1] + tail[1]])
    }

    /// 播空 / 被截断后上一段声音的尾巴：几毫秒内平滑降到 0，不硬切成 0
    fn fade_tail(&mut self) -> Option<Frame2> {
        if self.tail[0].abs() < 1e-5 && self.tail[1].abs() < 1e-5 {
            self.tail = [0.0; 2];
            return None;
        }
        let k = 1.0 - 4.0 / FADE_SAMPLES;
        self.tail = [self.tail[0] * k, self.tail[1] * k];
        Some(self.tail)
    }
}

#[derive(Default)]
pub struct Mixer {
    streams: HashMap<u32, Stream>,
    volumes: HashMap<u32, f32>,
    /// 暂时不播的通道（例如正在看某人带声音的屏幕共享时，他的伴奏已经在共享声音里了）
    suppressed: std::collections::HashSet<u32>,
    /// 各路声音额外多缓冲多久（ms，第 78 项）：这一路还没开始时先记着
    extra: HashMap<u32, f32>,
}

impl Mixer {
    /// 单声道的一帧
    pub fn push(&mut self, user: u32, seq: u16, pcm: &[i16]) {
        self.push_ch(user, seq, pcm, 1);
    }

    /// channels = 1（单声道）或 2（左右交错）
    pub fn push_ch(&mut self, user: u32, seq: u16, pcm: &[i16], channels: usize) {
        let kind = if user == LOOPBACK_ID || user == SFX_ID {
            Kind::Local
        } else if user & (MUSIC_FLAG | SHARE_FLAG) != 0 {
            Kind::Stream
        } else {
            Kind::Voice
        };
        let extra = self.extra.get(&user).copied().unwrap_or(0.0);
        let s = self.streams.entry(user).or_insert_with(|| {
            let mut s = Stream::new(kind);
            s.extra_ms = extra;
            s
        });
        if let Some(last) = s.last_seq {
            // 乱序/重复包直接丢弃（距离太远则视为重新开始）
            if !seq_newer(seq, last) && last.wrapping_sub(seq) < 100 {
                return;
            }
        }
        s.last_seq = Some(seq);
        let samples = pcm.len() / channels.max(1);
        if s.kind != Kind::Local {
            s.jitter.on_arrival(std::time::Instant::now(), samples);
        }
        if channels >= 2 {
            s.buf.extend(pcm.chunks_exact(channels).map(|c| [c[0] as f32 / 32768.0, c[1] as f32 / 32768.0]));
        } else {
            s.buf.extend(pcm.iter().map(|&x| {
                let v = x as f32 / 32768.0;
                [v, v]
            }));
        }
        match s.kind {
            Kind::Local => {
                if s.buf.len() > FRAME_SAMPLES * 50 {
                    let drop = s.buf.len() - FRAME_SAMPLES * 50;
                    s.buf.drain(..drop);
                }
            }
            _ => {
                // 积压一点点：稍微加快播放慢慢吃掉。积压特别多（网络卡了很久一下子到了一堆）才丢旧的，
                // 丢的时候旧的淡出、新的淡入，不会「咔」一声
                let target = s.prebuffer();
                let excess = if s.kind == Kind::Stream { STREAM_EXCESS_MS } else { JB_EXCESS_MS };
                if s.buf.len() > target + ms_to_samples(excess) {
                    let drop = s.buf.len() - target;
                    s.buf.drain(..drop);
                    s.tail = s.last_out;
                    s.gain = 0.0;
                    s.stats.drops += 1;
                }
                s.update_rate();
            }
        }
    }

    /// 丢包后补出来的帧数（统计用）
    pub fn note_concealed(&mut self, user: u32, frames: u32) {
        if let Some(s) = self.streams.get_mut(&user) {
            s.stats.concealed += frames;
        }
    }

    /// 某一路的播放统计（第 54-d 项）
    pub fn stream_stats(&self, user: u32) -> Option<StreamStats> {
        self.streams.get(&user).map(|s| StreamStats {
            buffered_ms: (s.buf.len() as f32 * 1000.0 / SAMPLE_RATE as f32).round() as u32,
            target_ms: (s.jitter.target_ms() + s.extra_ms).round() as u32,
            limited_recently: s.limiter.limited_within(std::time::Duration::from_secs(2)),
            ..s.stats
        })
    }

    /// 对方换了密钥、序号从头开始：忘掉上一包的序号（缓冲里已有的声音照常播完）
    pub fn restart_seq(&mut self, user: u32) {
        if let Some(s) = self.streams.get_mut(&user) {
            s.last_seq = None;
        }
    }

    /// 某人当前的抖动缓冲目标（ms），显示延迟用
    pub fn buffer_ms(&self, user: u32) -> Option<u32> {
        self.streams.get(&user).map(|s| s.jitter.target_ms().round() as u32)
    }

    /// 某一路声音额外多缓冲多久（ms，0 = 不额外缓冲；第 78 项：共享声音跟着画面的缓冲，音画同步）
    pub fn set_extra_delay(&mut self, user: u32, ms: f32) {
        if ms <= 0.0 {
            self.extra.remove(&user);
        } else {
            self.extra.insert(user, ms);
        }
        if let Some(s) = self.streams.get_mut(&user) {
            s.set_extra(ms.max(0.0));
        }
    }

    pub fn extra_delay(&self, user: u32) -> f32 {
        self.extra.get(&user).copied().unwrap_or(0.0)
    }

    /// 某一路自己的抖动缓冲目标（ms，不含额外缓冲）
    pub fn base_buffer_ms(&self, user: u32) -> Option<f32> {
        self.streams.get(&user).map(|s| s.jitter.target_ms())
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

    /// 取出一个混音样本（左, 右）
    fn next_frame(&mut self) -> Frame2 {
        let mut sum = [0.0f32; 2];
        for (id, s) in self.streams.iter_mut() {
            if self.suppressed.contains(id) {
                s.buf.clear();
                s.playing = false;
                s.tail = [0.0; 2];
                continue;
            }
            let v = self.volumes.get(id).copied().unwrap_or(1.0);
            let x = s.next();
            // 每一路先乘音量、再过限幅器（有 3 ms 预读：峰值到之前就平滑压下去，不削波形）。
            // 这一路停了也要把限幅器里预读的那 3 ms 放完
            if s.kind != Kind::Local && (x.is_some() || s.limiter.pending()) {
                let x = x.unwrap_or([0.0; 2]);
                let y = s.limiter.process([x[0] * v, x[1] * v]);
                if s.limiter.take_event() {
                    s.stats.limited += 1;
                }
                sum[0] += y[0];
                sum[1] += y[1];
            } else if let Some(x) = x {
                sum[0] += x[0] * v;
                sum[1] += x[1] * v;
            }
        }
        sum
    }
}

/// 限幅器的门槛：每一路压到这以下；混音后总的再过一个，留一点余量给软削波兜底
const LIMIT_THRESHOLD: f32 = 0.89;
const MASTER_THRESHOLD: f32 = 0.79;
/// 预读 3 ms、恢复约 120 ms
const LIMIT_LOOKAHEAD: usize = 144;
const LIMIT_RELEASE_S: f32 = 0.12;

/// 带预读的限幅器（第 57 项）：信号先延迟 3 ms，同时看「接下来 3 ms 里最大的峰值」算要压多少，
/// 增益在峰值到来之前平滑降下去，过后慢慢回升。和削波不同，波形形状不变，只是这一小段整体小一点，听不出破音。左右两路一起压
pub struct Limiter {
    thr: f32,
    delay: VecDeque<Frame2>,
    /// 接下来 LIMIT_LOOKAHEAD 个样本里需要的增益的滑动最小值（单调队列：(样本序号, 需要的增益)）
    mins: VecDeque<(u64, f32)>,
    n: u64,
    gain: f32,
    attack: f32,
    release: f32,
    /// 正在压（低于 -3 dB）：用来数「压了几次」
    active: bool,
    event: bool,
    last_event: Option<std::time::Instant>,
}

impl Limiter {
    pub fn new(thr: f32) -> Self {
        let sr = SAMPLE_RATE as f32;
        Self {
            thr,
            delay: VecDeque::with_capacity(LIMIT_LOOKAHEAD + 1),
            mins: VecDeque::new(),
            n: 0,
            gain: 1.0,
            // 预读窗口的四分之一内基本到位（3 ms 内一定压到）
            attack: 1.0 - (-4.0 / LIMIT_LOOKAHEAD as f32).exp(),
            release: 1.0 - (-1.0 / (LIMIT_RELEASE_S * sr)).exp(),
            active: false,
            event: false,
            last_event: None,
        }
    }

    /// 预读缓冲里还有没放完的声音
    pub fn pending(&self) -> bool {
        self.delay.iter().any(|x| x[0] != 0.0 || x[1] != 0.0) || self.gain < 0.999
    }

    pub fn process(&mut self, x: Frame2) -> Frame2 {
        let peak = x[0].abs().max(x[1].abs());
        let need = if peak > self.thr { self.thr / peak } else { 1.0 };
        while self.mins.back().is_some_and(|&(_, g)| g >= need) {
            self.mins.pop_back();
        }
        self.mins.push_back((self.n, need));
        while self.mins.front().is_some_and(|&(i, _)| i + LIMIT_LOOKAHEAD as u64 <= self.n) {
            self.mins.pop_front();
        }
        self.n += 1;
        let target = self.mins.front().map_or(1.0, |m| m.1);
        self.gain += (target - self.gain) * if target < self.gain { self.attack } else { self.release };
        self.delay.push_back(x);
        let out = if self.delay.len() > LIMIT_LOOKAHEAD { self.delay.pop_front().unwrap() } else { [0.0; 2] };
        // 平滑追不上的极少数尖峰：硬性兜底（正常情况下不会触发）
        let g = self.gain;
        let y = [(out[0] * g).clamp(-1.0, 1.0), (out[1] * g).clamp(-1.0, 1.0)];
        let now_active = self.gain < 0.708;
        if now_active && !self.active {
            self.event = true;
            self.last_event = Some(std::time::Instant::now());
        } else if now_active {
            self.last_event = Some(std::time::Instant::now());
        }
        self.active = now_active;
        y
    }

    /// 刚开始了一次「压低超过 3 dB」
    pub fn take_event(&mut self) -> bool {
        std::mem::take(&mut self.event)
    }

    pub fn limited_within(&self, d: std::time::Duration) -> bool {
        self.last_event.is_some_and(|t| t.elapsed() < d)
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
/// 打开着的声卡流：cpal 的，或者自己用 IAudioClient3 打开的低延迟流（第 36 项语音第 1 条）。只是拿着，drop 时关掉
#[allow(dead_code)]
pub enum Held {
    Cpal(cpal::Stream),
    Ll(crate::wasapi_ll::LlStream),
}

/// 设置「声卡低延迟模式」：驱动支持时用 IAudioClient3 的最小周期（约 3 ms）打开，不支持自动退回
pub static LOW_LATENCY_DEVICE: AtomicBool = AtomicBool::new(false);

pub struct AudioStreams {
    _input: Option<Held>,
    _output: Option<Held>,
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
/// 第 81 项：告诉 Windows「VOC 的声音不算通话、不要因此压低别的程序」。
/// Windows 把用默认通信设备的程序当成在打电话，默认把游戏等其他声音压低 80%（声音设置 → 通信），用户听起来就是游戏突然变小变闷。
/// 对每个可用的声卡（输入、输出）把本程序的声音会话设成不参与自动压低（IAudioSessionControl2::SetDuckingPreference，微软给通话软件的做法）。
/// 在打开声卡之前调；单独开一个线程做（COM 用多线程模式，不影响界面线程）
#[cfg(windows)]
pub fn opt_out_ducking() {
    let _ = std::thread::Builder::new()
        .name("ducking".into())
        .spawn(|| unsafe {
            use windows::core::Interface;
            use windows::Win32::Media::Audio::{eAll, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE};
            use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let Ok(en) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else { return };
            let Ok(list) = en.EnumAudioEndpoints(eAll, DEVICE_STATE_ACTIVE) else { return };
            let mut done = 0;
            for i in 0..list.GetCount().unwrap_or(0) {
                let Ok(dev) = list.Item(i) else { continue };
                let Ok(mgr) = dev.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { continue };
                let Ok(c) = mgr.GetAudioSessionControl(None, 0) else { continue };
                if let Ok(c2) = c.cast::<IAudioSessionControl2>() {
                    if c2.SetDuckingPreference(true).is_ok() {
                        done += 1;
                    }
                }
            }
            crate::applog::log(format!("声音：{done} 个声卡上设了「不触发通话时压低其他声音」"));
        })
        .map(|h| h.join());
}

#[cfg(not(windows))]
pub fn opt_out_ducking() {}

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
    opt_out_ducking();
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
) -> Result<(Held, String, String), String> {
    if small && LOW_LATENCY_DEVICE.load(Ordering::Relaxed) {
        match start_input_ll(name, frames_tx.clone(), health.clone()) {
            Ok(x) => return Ok(x),
            Err(e) => crate::applog::log(format!("麦克风低延迟模式不可用，用普通方式：{e}")),
        }
    }
    let (s, d, n) = start_input_cpal(name, frames_tx, health, small)?;
    Ok((Held::Cpal(s), d, n))
}

/// 低延迟模式打开麦克风：混成单声道 → 变采样率到 48 kHz → 每 10 ms 一帧交给编码线程
fn start_input_ll(name: Option<&str>, frames_tx: SyncSender<Vec<f32>>, health: StreamHealth) -> Result<(Held, String, String), String> {
    let rs = Arc::new(Mutex::new(None::<PushResampler>));
    let rs2 = rs.clone();
    let mut frame = Vec::with_capacity(CAPTURE_FRAME);
    let (s, (rate, ch, period, dname)) = crate::wasapi_ll::start_capture(
        name,
        move |data, ch| {
            let mut g = rs2.lock().unwrap();
            let Some(r) = g.as_mut() else { return };
            for chunk in data.chunks(ch) {
                let mono = chunk.iter().sum::<f32>() / ch as f32;
                r.push(mono, |y| {
                    frame.push(y);
                    if frame.len() == CAPTURE_FRAME {
                        let full = std::mem::replace(&mut frame, Vec::with_capacity(CAPTURE_FRAME));
                        let _ = frames_tx.try_send(full);
                    }
                });
            }
        },
        move |e| {
            crate::applog::log(format!("麦克风低延迟模式出错：{e}"));
            health.failed.store(true, Ordering::Relaxed);
        },
    )?;
    *rs.lock().unwrap() = Some(PushResampler::new(rate, CAPTURE_RATE));
    let desc = format!("{dname}（{rate} Hz, {ch} 声道，低延迟模式 {:.1} ms 周期）", period as f32 * 1000.0 / rate as f32);
    Ok((Held::Ll(s), desc, dname))
}

fn start_input_cpal(
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
) -> Result<(Held, String, String), String> {
    if small && LOW_LATENCY_DEVICE.load(Ordering::Relaxed) {
        match start_output_ll(name, mixer.clone(), settings.clone(), health.clone()) {
            Ok(x) => return Ok(x),
            Err(e) => crate::applog::log(format!("扬声器低延迟模式不可用，用普通方式：{e}")),
        }
    }
    let (s, d, n) = start_output_cpal(name, mixer, settings, health, small)?;
    Ok((Held::Cpal(s), d, n))
}

/// 低延迟模式打开扬声器：和普通方式一样从混音器取声音（变采样率、回声消除参考、软削波）
fn start_output_ll(name: Option<&str>, mixer: Arc<Mutex<Mixer>>, settings: Arc<Settings>, health: StreamHealth) -> Result<(Held, String, String), String> {
    let rs = Arc::new(Mutex::new(None::<PullResampler>));
    let rs2 = rs.clone();
    let mut tap: Vec<f32> = Vec::with_capacity(crate::aec::AEC_FRAME);
    let mut master = Limiter::new(MASTER_THRESHOLD);
    let (s, (rate, ch, period, dname)) = crate::wasapi_ll::start_render(
        name,
        move |out, ch| {
            let mut g = rs2.lock().unwrap();
            let (Some(rs), Ok(mut m)) = (g.as_mut(), mixer.lock()) else {
                out.fill(0.0);
                return;
            };
            if settings.deafened.load(Ordering::Relaxed) {
                m.clear();
                out.fill(0.0);
                return;
            }
            let vol = settings.output_volume.get();
            for frame in out.chunks_mut(ch) {
                let [l, r] = rs.next2(|| {
                    let x = m.next_frame();
                    tap.push((x[0] + x[1]) * 0.5 * vol);
                    if tap.len() == crate::aec::AEC_FRAME {
                        crate::aec::push_render(std::mem::replace(&mut tap, Vec::with_capacity(crate::aec::AEC_FRAME)));
                    }
                    x
                });
                let [l, r] = master.process([l * vol, r * vol]);
                let (l, r) = (soft_clip(l), soft_clip(r));
                if ch >= 2 {
                    frame[0] = l;
                    frame[1] = r;
                    for x in frame.iter_mut().skip(2) {
                        *x = (l + r) * 0.5;
                    }
                } else {
                    frame[0] = (l + r) * 0.5;
                }
            }
        },
        move |e| {
            crate::applog::log(format!("扬声器低延迟模式出错：{e}"));
            health.failed.store(true, Ordering::Relaxed);
        },
    )?;
    *rs.lock().unwrap() = Some(PullResampler::new(SAMPLE_RATE, rate));
    let desc = format!("{dname}（{rate} Hz, {ch} 声道，低延迟模式 {:.1} ms 周期）", period as f32 * 1000.0 / rate as f32);
    Ok((Held::Ll(s), desc, dname))
}

fn start_output_cpal(
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
    let mut master = Limiter::new(MASTER_THRESHOLD);
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
                let [l, r] = rs.next2(|| {
                    let x = m.next_frame();
                    // 回声消除的远端参考：按播放音量，10 ms 一帧（左右平均）
                    tap.push((x[0] + x[1]) * 0.5 * vol);
                    if tap.len() == crate::aec::AEC_FRAME {
                        crate::aec::push_render(std::mem::replace(&mut tap, Vec::with_capacity(crate::aec::AEC_FRAME)));
                    }
                    x
                });
                let [l, r] = master.process([l * vol, r * vol]);
                let (l, r) = (soft_clip(l), soft_clip(r));
                // 两个以上声道：前两个放左右，其余放平均（单声道设备只放平均）
                if ch >= 2 {
                    frame[0] = T::from_sample(l);
                    frame[1] = T::from_sample(r);
                    let mid = T::from_sample((l + r) * 0.5);
                    for x in frame.iter_mut().skip(2) {
                        *x = mid;
                    }
                } else {
                    frame[0] = T::from_sample((l + r) * 0.5);
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
        assert_eq!(m.next_frame()[0], 0.0); // 不到一帧（20 ms）：还在预缓冲
        m.push(5, 0, &f); // 旧包丢弃
        // 测试里两包是同时到的（真实网络约隔 20 ms），会算出一点抖动，所以攒到一帧多一点就开播
        m.push(5, 2, &f);
        // 开播时淡入：几毫秒后到满音量；限幅器有 3 ms 预读（第 57 项），所以开头 144 个采样还是静音
        for _ in 0..LIMIT_LOOKAHEAD {
            assert_eq!(m.next_frame()[0], 0.0, "limiter lookahead");
        }
        let first = m.next_frame()[0];
        assert!(first > 0.0 && first < 0.05, "{first}");
        let mut last = 0.0;
        for _ in 0..400 {
            last = m.next_frame()[0];
        }
        assert!((last - 0.5).abs() < 0.01, "{last}");
        assert!(m.buffer_ms(5).unwrap() <= 25);
    }

    /// 第 57 项：音量开到 173% 的满幅正弦，限幅后峰值不超过门槛，波形没有被削平（失真远小于削波）；小声原样通过（只晚 3 ms）
    #[test]
    fn limiter_keeps_peaks_without_clipping() {
        let n = SAMPLE_RATE as usize;
        let tone: Vec<f32> = (0..n).map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin() * 0.9).collect();
        let mut lim = Limiter::new(LIMIT_THRESHOLD);
        let out: Vec<f32> = tone.iter().map(|&x| lim.process([x * 1.73, x * 1.73])[0]).collect();
        let peak = out[n / 2..].iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak <= LIMIT_THRESHOLD + 0.01, "{peak}");
        // 稳定后输出应该是一个干净的（被缩小的）正弦：和最佳拟合的正弦比，误差很小
        let seg = &out[n / 2..];
        let refs: Vec<f32> = (n / 2..n).map(|i| ((i - LIMIT_LOOKAHEAD) as f32 * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin()).collect();
        let a = seg.iter().zip(&refs).map(|(x, r)| x * r).sum::<f32>() / refs.iter().map(|r| r * r).sum::<f32>();
        let err = seg.iter().zip(&refs).map(|(x, r)| (x - a * r).powi(2)).sum::<f32>() / seg.iter().map(|x| x * x).sum::<f32>();
        let clipped: Vec<f32> = tone[n / 2..].iter().map(|&x| soft_clip(x * 1.73)).collect();
        let a2 = clipped.iter().zip(&tone[n / 2..]).map(|(x, r)| x * r).sum::<f32>() / tone[n / 2..].iter().map(|r| r * r).sum::<f32>();
        let err2 = clipped.iter().zip(&tone[n / 2..]).map(|(x, r)| (x - a2 * r).powi(2)).sum::<f32>() / clipped.iter().map(|x| x * x).sum::<f32>();
        println!("失真：限幅器 {:.1} dB，原来的软削波 {:.1} dB", 10.0 * err.log10(), 10.0 * err2.log10());
        assert!(err < err2 / 100.0, "限幅器失真应比软削波小 20 dB 以上");
        assert!(lim.limited_within(std::time::Duration::from_secs(1)));
        // 小声：原样（晚 3 ms）通过、不算压过
        let mut lim = Limiter::new(LIMIT_THRESHOLD);
        let quiet: Vec<f32> = (0..2000).map(|i| lim.process([tone[i] * 0.3, 0.0])[0]).collect();
        assert!((quiet[1000] - tone[1000 - LIMIT_LOOKAHEAD] * 0.3).abs() < 1e-6);
        assert!(!lim.take_event());
    }

    /// 立体声：左右分开放
    #[test]
    fn stereo_stream() {
        let mut m = Mixer::default();
        let id = SHARE_FLAG | 4;
        let f: Vec<i16> = (0..FRAME_SAMPLES).flat_map(|_| [16384i16, -8192]).collect();
        for seq in 0..6u16 {
            m.push_ch(id, seq, &f, 2);
        }
        let mut x = [0.0; 2];
        for _ in 0..400 {
            x = m.next_frame();
        }
        assert!((x[0] - 0.5).abs() < 0.01 && (x[1] + 0.25).abs() < 0.01, "{x:?}");
    }

    /// 播空时不硬切成 0：相邻两个样本的跳变很小（第 54-b 项）
    #[test]
    fn underrun_fades_out() {
        let mut m = Mixer::default();
        let id = SHARE_FLAG | 7;
        let f = vec![20000i16; FRAME_SAMPLES];
        for seq in 0..5u16 {
            m.push(id, seq, &f);
        }
        let mut prev = 0.0f32;
        let mut max_step = 0.0f32;
        for _ in 0..FRAME_SAMPLES * 8 {
            let x = m.next_frame()[0];
            max_step = max_step.max((x - prev).abs());
            prev = x;
        }
        assert!(prev.abs() < 1e-3, "播完后安静：{prev}");
        assert!(max_step < 0.02, "开头淡入、结尾淡出，没有大跳变：{max_step}");
        assert_eq!(m.stream_stats(id).unwrap().underruns, 1);
    }

    /// 积压不多时不丢包，靠稍微加快播放吃掉（第 36-2 / 54-b 项）
    #[test]
    fn excess_played_faster_not_dropped() {
        let mut m = Mixer::default();
        let id = SHARE_FLAG | 3;
        let f = vec![1000i16; FRAME_SAMPLES];
        for seq in 0..12u16 {
            m.push(id, seq, &f); // 240 ms，目标 70 ms
        }
        let st = m.stream_stats(id).unwrap();
        assert_eq!(st.drops, 0);
        assert!(st.rate_permille > 0 && st.rate_permille <= 5, "{st:?}");
        let before = m.streams[&id].buf.len();
        for _ in 0..4800 {
            m.next_frame()[0];
        }
        let used = before - m.streams[&id].buf.len();
        assert!(used > 4800 && used < 4800 + 30, "100 ms 吃掉 {used} 个样本");
    }

    /// 重采样：1 kHz 正弦 48k → 44.1k 失真很小；22 kHz（44.1k 放不下）被滤掉，不会折回来成底噪（第 55-a 项）
    #[test]
    fn sinc_resampler_quality() {
        let tone = |f: f64, rate: u32, n: usize| (0..n).map(move |i| (std::f64::consts::TAU * f * i as f64 / rate as f64).sin() as f32 * 0.5);
        let mut pr = PullResampler::new(48_000, 44_100);
        let mut src = tone(1000.0, 48_000, 200_000);
        let out: Vec<f32> = (0..44_100).map(|_| pr.next(|| src.next().unwrap_or(0.0))).collect();
        // 和理想正弦比（对齐延迟：16 个输入样本）
        let delay = 16.0 / 48_000.0;
        let (mut sig, mut err) = (0.0f64, 0.0f64);
        for (i, &y) in out.iter().enumerate().skip(1000) {
            let t = i as f64 / 44_100.0 - delay;
            let ideal = (std::f64::consts::TAU * 1000.0 * t).sin() * 0.5;
            sig += ideal * ideal;
            err += (y as f64 - ideal).powi(2);
        }
        let snr = 10.0 * (sig / err).log10();
        assert!(snr > 60.0, "SNR {snr:.1} dB");
        let mut pr = PullResampler::new(48_000, 44_100);
        let mut src = tone(22_500.0, 48_000, 200_000);
        let out: Vec<f32> = (0..44_100).map(|_| pr.next(|| src.next().unwrap_or(0.0))).collect();
        let rms = (out.iter().skip(1000).map(|x| (*x as f64).powi(2)).sum::<f64>() / 43_100.0).sqrt();
        assert!(rms < 0.5 * 0.707 * 0.03, "22.5 kHz 应被滤掉：rms {rms}");
        // 一样的采样率：原样输出
        let mut pr = PullResampler::new(48_000, 48_000);
        assert_eq!(pr.next(|| 0.25), 0.25);
    }

    #[test]
    fn jitter_buffer_adapts() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        // 每 20 ms 准时到：目标保持最小 20 ms
        let mut j = JitterEstimator::default();
        for i in 0..100u64 {
            j.on_arrival(t0 + Duration::from_millis(20 * i), FRAME_SAMPLES);
        }
        assert_eq!(j.target_ms().round() as u32, 20);
        // 到达时间忽早忽晚 ±15 ms：目标明显变大，但不超过上限
        let mut j = JitterEstimator::default();
        for i in 0..200u64 {
            let wobble = if i % 2 == 0 { 0 } else { 30 };
            j.on_arrival(t0 + Duration::from_millis(20 * i + wobble), FRAME_SAMPLES);
        }
        let t = j.target_ms();
        assert!(t > 60.0 && t <= JB_MAX_MS, "{t}");
        // 欠载一次多攒 20 ms
        let before = j.target_ms();
        j.on_underrun();
        assert!(j.target_ms() >= (before + 19.0).min(JB_MAX_MS));
        // 说话停顿后的第一包不算抖动
        let mut j = JitterEstimator::default();
        j.on_arrival(t0, FRAME_SAMPLES);
        j.on_arrival(t0 + Duration::from_secs(3), FRAME_SAMPLES);
        assert_eq!(j.target_ms().round() as u32, 20);
    }

    #[test]
    fn jitter_buffer_drains_excess() {
        let mut m = Mixer::default();
        let f = vec![1000i16; FRAME_SAMPLES];
        // 网络卡了很久，一次到了 20 帧（400 ms）：只保留目标量，不把延迟一直拖着
        for seq in 0..20u16 {
            m.push(9, seq, &f);
        }
        let len = m.streams[&9].buf.len();
        assert!(len <= ms_to_samples(m.streams[&9].jitter.target_ms() + JB_EXCESS_MS), "{len}");
        assert_eq!(m.stream_stats(9).unwrap().drops, 1);
    }

    /// 第 78 项：共享声音跟着画面的缓冲加额外延迟。一下子加很多：先停下攒够再播；一下子减很多：丢掉多出来的；还没开始的那一路先记着
    #[test]
    fn share_audio_extra_delay() {
        let mut m = Mixer::default();
        let sid = SHARE_FLAG | 5;
        m.set_extra_delay(sid, 200.0);
        let f = vec![1000i16; FRAME_SAMPLES];
        m.push(sid, 0, &f);
        let st = m.stream_stats(sid).unwrap();
        assert!(st.target_ms >= 270, "{st:?}"); // 共享声音自己最少 70 ms + 额外 200 ms
        // 攒够目标才开始播
        for seq in 1..20u16 {
            m.push(sid, seq, &f);
        }
        let s = &m.streams[&sid];
        assert!(s.buf.len() >= ms_to_samples(250.0));
        for _ in 0..10 {
            m.next_frame();
        }
        assert!(m.streams[&sid].playing);
        // 关掉缓冲：多出来的 200 ms 直接丢掉，不让声音落在画面后面
        let before = m.streams[&sid].buf.len();
        m.set_extra_delay(sid, 0.0);
        let after = m.streams[&sid].buf.len();
        assert!(before - after >= ms_to_samples(190.0), "{before} -> {after}");
        // 再加回来：停下、攒够再播
        m.set_extra_delay(sid, 150.0);
        assert!(!m.streams[&sid].playing);
        assert_eq!(m.extra_delay(sid), 150.0);
    }
}
