//! 屏幕共享。
//!
//! 发送：Windows Graphics Capture 采集 → 缩放 + 色彩转换 → 编码（优先显卡硬件编码 AV1 > HEVC > H.264，
//!      按所有观看者都能解码的格式协商；没有硬件编码器时用 openh264 软件编码 H.264）
//!      → 端到端加密整帧（发送者视频密钥）→ 切片 → 能直连的观看者 P2P 直发，其余经服务器中转。
//! 接收：按帧重组 → 端到端解密 → 解码（Media Foundation 软解 / 硬解，或 openh264）→ RGBA → 界面贴图。
//! 丢包不重传（保证低延迟）：发现缺帧就丢弃到下一个关键帧，并向共享者请求关键帧。

use crate::mf::dec::MfDecoder;
use crate::mf::enc::{self as mfenc, Codec, HwEncoder, HwEncoderInfo};
use crate::voice::VoiceState;
use openh264::encoder::{BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, RateControlMode, UsageType};
use openh264::formats::{YUVBuffer, YUVSource};
use openh264::OpenH264API;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use voc_proto::udp::{self, VideoFrag, VIDEO_KEY};

/// 观看时用硬件解码（DXVA）；否则 CPU 软解
pub static HW_DECODE: AtomicBool = AtomicBool::new(false);

// ------------------------------------------------------------ 配置

#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ShareConfig {
    /// 480 / 720 / 1080
    pub height: u32,
    /// 30 / 60
    pub fps: u32,
    pub monitor: usize,
    /// 编码器：auto（自动挑最好的显卡编码器）/ software（CPU）/ vendor:<厂商号>（指定显卡）
    pub encoder: String,
}

impl Default for ShareConfig {
    fn default() -> Self {
        Self { height: 720, fps: 30, monitor: 0, encoder: "auto".into() }
    }
}

impl ShareConfig {
    pub fn desc(&self) -> String {
        format!("{}p{}", self.height, self.fps)
    }

    /// 目标码率（bps）
    pub fn bitrate(&self) -> u32 {
        let kbps = match (self.height, self.fps >= 60) {
            (h, false) if h <= 480 => 900,
            (h, true) if h <= 480 => 1400,
            (h, false) if h <= 720 => 2000,
            (h, true) if h <= 720 => 3000,
            (_, false) => 3500,
            (_, true) => 5500,
        };
        kbps * 1000
    }

    /// 某种编码格式下的目标码率：同等画质下 HEVC / AV1 需要的码率更低
    pub fn bitrate_for(&self, codec: Codec) -> u32 {
        (self.bitrate() as f32 * codec.bitrate_factor()) as u32
    }

    /// 按源分辨率和目标高度算出编码尺寸（保持宽高比，偶数，不放大）
    pub fn target_size(&self, sw: usize, sh: usize) -> (usize, usize) {
        let dh = (self.height as usize).min(sh).max(2) & !1;
        let dw = (((dh * sw + sh / 2) / sh) + 1).max(2) & !1; // 四舍五入到偶数（854×480）
        (dw, dh)
    }
}

/// 本机所有显卡硬件编码器（第一次调用时探测，之后缓存）
pub fn hw_encoders() -> &'static [HwEncoderInfo] {
    static L: OnceLock<Vec<HwEncoderInfo>> = OnceLock::new();
    L.get_or_init(|| {
        std::thread::spawn(|| {
            if mfenc::mf_init().is_err() {
                return Vec::new();
            }
            mfenc::list_hw_encoders()
        })
        .join()
        .unwrap_or_default()
    })
}

/// 本机能解码的格式（H.264 总是可以：内置 openh264）
pub fn local_decoders() -> &'static [Codec] {
    static L: OnceLock<Vec<Codec>> = OnceLock::new();
    L.get_or_init(|| {
        std::thread::spawn(|| {
            let mut v = Vec::new();
            if mfenc::mf_init().is_ok() {
                if crate::mf::dec::decoder_available(Codec::Av1) && crate::mf::dec::gpu_av1_decode_supported() {
                    v.push(Codec::Av1);
                }
                if crate::mf::dec::decoder_available(Codec::Hevc) {
                    v.push(Codec::Hevc);
                }
            }
            v.push(Codec::H264);
            v
        })
        .join()
        .unwrap_or_else(|_| vec![Codec::H264])
    })
}

/// 编码器选项：(配置值, 显示名)
pub fn encoder_choices() -> Vec<(String, String)> {
    let mut v = vec![("auto".to_string(), "自动（推荐：挑最好的显卡编码器）".to_string())];
    let mut vendors: Vec<(u32, String, Vec<&'static str>)> = Vec::new();
    for e in hw_encoders() {
        match vendors.iter_mut().find(|x| x.0 == e.vendor) {
            Some(x) => {
                if !x.2.contains(&e.codec.label()) {
                    x.2.push(e.codec.label())
                }
            }
            None => vendors.push((e.vendor, e.gpu.clone(), vec![e.codec.label()])),
        }
    }
    for (vendor, gpu, codecs) in vendors {
        v.push((format!("vendor:{vendor:x}"), format!("{gpu}（{}）", codecs.join(" / "))));
    }
    v.push(("software".into(), "CPU 软件编码（OpenH264，仅 H.264）".into()));
    v
}

/// 按偏好和观看者能解的格式，决定用哪种格式、哪个编码器（None = openh264 软件编码）
pub fn plan_encoder(pref: &str, allowed: &[Codec], hw: &[HwEncoderInfo]) -> (Codec, Option<HwEncoderInfo>) {
    if pref == "software" {
        return (Codec::H264, None);
    }
    let vendor = pref.strip_prefix("vendor:").and_then(|v| u32::from_str_radix(v, 16).ok());
    for codec in [Codec::Av1, Codec::Hevc, Codec::H264] {
        if !allowed.contains(&codec) {
            continue;
        }
        if let Some(e) = hw.iter().find(|e| e.codec == codec && vendor.is_none_or(|v| e.vendor == v)) {
            return (codec, Some(e.clone()));
        }
    }
    (Codec::H264, None)
}

/// 所有观看者都能解的格式（旧版客户端没上报能力，视为只支持 H.264）
pub fn common_decoders(caps: &[Vec<String>]) -> Vec<Codec> {
    [Codec::Av1, Codec::Hevc, Codec::H264]
        .into_iter()
        .filter(|c| *c == Codec::H264 || (!caps.is_empty() && caps.iter().all(|list| list.iter().any(|x| x == c.key()))))
        .collect()
}

pub fn monitor_names() -> Vec<String> {
    windows_capture::monitor::Monitor::enumerate()
        .map(|ms| {
            ms.iter()
                .enumerate()
                .map(|(i, m)| {
                    let size = match (m.width(), m.height()) {
                        (Ok(w), Ok(h)) => format!("{w}×{h}"),
                        _ => String::new(),
                    };
                    let name = m.device_string().or_else(|_| m.name()).unwrap_or_default();
                    format!("显示器 {}  {size}  {name}", i + 1)
                })
                .collect()
        })
        .unwrap_or_default()
}

// ------------------------------------------------------------ 色彩转换

/// BGRA (sw×sh) → I420 (dw×dh)。亮度最近邻取样，色度 2×2 平均；BT.601 有限范围。按行分块多线程。
pub fn bgra_to_i420(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize, out: &mut Vec<u8>) {
    out.resize(dw * dh * 3 / 2, 0);
    let xs: Vec<usize> = (0..dw).map(|x| (x * sw / dw).min(sw - 1) * 4).collect();
    let ys: Vec<usize> = (0..dh).map(|y| (y * sh / dh).min(sh - 1) * sw * 4).collect();
    let (yp, uv) = out.split_at_mut(dw * dh);
    let (up, vp) = uv.split_at_mut(dw * dh / 4);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
    let rows_per = (dh / 2).div_ceil(threads).max(1) * 2;
    std::thread::scope(|s| {
        let (mut yrest, mut urest, mut vrest) = (&mut yp[..], &mut up[..], &mut vp[..]);
        let mut y0 = 0;
        while y0 < dh {
            let rows = rows_per.min(dh - y0);
            let (yc, yr) = yrest.split_at_mut(rows * dw);
            yrest = yr;
            let (uc, ur) = urest.split_at_mut(rows / 2 * dw / 2);
            urest = ur;
            let (vc, vr) = vrest.split_at_mut(rows / 2 * dw / 2);
            vrest = vr;
            let (xs, ys) = (&xs, &ys);
            s.spawn(move || {
                for r in 0..rows {
                    let row = ys[y0 + r];
                    let yrow = &mut yc[r * dw..(r + 1) * dw];
                    for (x, yv) in yrow.iter_mut().enumerate() {
                        let p = row + xs[x];
                        let (b, g, rr) = (src[p] as i32, src[p + 1] as i32, src[p + 2] as i32);
                        *yv = (((66 * rr + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                    }
                    if r % 2 == 0 {
                        let row2 = ys[(y0 + r + 1).min(dh - 1)];
                        let base = r / 2 * dw / 2;
                        for cx in 0..dw / 2 {
                            let (mut sb, mut sg, mut sr) = (0i32, 0i32, 0i32);
                            for p in [row + xs[cx * 2], row + xs[cx * 2 + 1], row2 + xs[cx * 2], row2 + xs[cx * 2 + 1]] {
                                sb += src[p] as i32;
                                sg += src[p + 1] as i32;
                                sr += src[p + 2] as i32;
                            }
                            let (b, g, rr) = (sb / 4, sg / 4, sr / 4);
                            uc[base + cx] = (((-38 * rr - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
                            vc[base + cx] = (((112 * rr - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
                        }
                    }
                }
            });
            y0 += rows;
        }
    });
}

/// I420 → NV12（UV 交织），硬件编码器要 NV12
pub fn i420_to_nv12(i420: &[u8], w: usize, h: usize, out: &mut Vec<u8>) {
    out.resize(w * h * 3 / 2, 0);
    out[..w * h].copy_from_slice(&i420[..w * h]);
    let q = w * h / 4;
    let (u, v) = (&i420[w * h..w * h + q], &i420[w * h + q..w * h + 2 * q]);
    for (i, d) in out[w * h..].chunks_exact_mut(2).enumerate() {
        d[0] = u[i];
        d[1] = v[i];
    }
}

/// NV12 → RGBA（BT.601 有限范围），多线程
pub fn nv12_to_rgba(nv12: &[u8], w: usize, h: usize, out: &mut Vec<u8>) {
    out.resize(w * h * 4, 0);
    let (yp, uv) = nv12.split_at(w * h);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
    let rows_per = (h / 2).div_ceil(threads).max(1) * 2;
    std::thread::scope(|s| {
        for (ci, chunk) in out.chunks_mut(rows_per * w * 4).enumerate() {
            let y0 = ci * rows_per;
            s.spawn(move || {
                for (r, row) in chunk.chunks_mut(w * 4).enumerate() {
                    let y = y0 + r;
                    for x in 0..w {
                        let yy = yp[y * w + x] as i32 - 16;
                        let ci = (y / 2) * w + (x & !1);
                        let (u, v) = (uv[ci] as i32 - 128, uv[ci + 1] as i32 - 128);
                        let c = 298 * yy;
                        let px = &mut row[x * 4..x * 4 + 4];
                        px[0] = ((c + 409 * v + 128) >> 8).clamp(0, 255) as u8;
                        px[1] = ((c - 100 * u - 208 * v + 128) >> 8).clamp(0, 255) as u8;
                        px[2] = ((c + 516 * u + 128) >> 8).clamp(0, 255) as u8;
                        px[3] = 255;
                    }
                }
            });
        }
    });
}

// ------------------------------------------------------------ 编码器（与采集分离，便于测试）

enum Backend {
    Soft(Encoder),
    Hw(HwEncoder),
}

pub struct VideoEncoder {
    cfg: ShareConfig,
    /// (宽, 高, 格式, 编码器名, 后端)
    cur: Option<(usize, usize, Codec, String, Backend)>,
    yuv: Vec<u8>,
    nv12: Vec<u8>,
}

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub key: bool,
    pub width: usize,
    pub height: usize,
    pub codec: Codec,
}

impl VideoEncoder {
    pub fn new(cfg: ShareConfig) -> Self {
        Self { cfg, cur: None, yuv: Vec::new(), nv12: Vec::new() }
    }

    fn make_soft(cfg: &ShareConfig) -> Result<Encoder, String> {
        let c = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(cfg.bitrate()))
            .max_frame_rate(FrameRate::from_hz(cfg.fps as f32))
            .skip_frames(false) // 保帧率：画面变化大时降清晰度，而不是丢帧
            .complexity(Complexity::Low)
            .num_threads(4);
        Encoder::with_api_config(OpenH264API::from_source(), c).map_err(|e| e.to_string())
    }

    /// 当前在用的编码器描述，例如 "AV1 · NVIDIA AV1 Encoder MFT"
    pub fn describe(&self) -> Option<String> {
        self.cur.as_ref().map(|(_, _, c, n, _)| format!("{} · {n}", c.label()))
    }

    /// 编码一帧 BGRA。`plan` = 用哪种格式 / 哪个硬件编码器（None 为 openh264）。
    /// 返回本次得到的码流（硬件编码器偶尔会延迟一帧输出，所以可能是 0 或多个）。
    pub fn encode(
        &mut self,
        bgra: &[u8],
        sw: usize,
        sh: usize,
        plan: &(Codec, Option<HwEncoderInfo>),
        force_key: bool,
    ) -> Result<Vec<EncodedFrame>, String> {
        let (dw, dh) = self.cfg.target_size(sw, sh);
        let (codec, hw) = plan;
        let name = hw.as_ref().map(|h| h.name.clone()).unwrap_or_else(|| "OpenH264（CPU）".into());
        let fresh = !matches!(&self.cur, Some((w, h, c, n, _)) if *w == dw && *h == dh && c == codec && *n == name);
        if fresh {
            // 尺寸 / 格式 / 编码器变了就重建
            let backend = match hw {
                Some(info) => {
                    mfenc::mf_init().map_err(|e| e.to_string())?;
                    Backend::Hw(
                        HwEncoder::new(info, dw as u32, dh as u32, self.cfg.fps, self.cfg.bitrate_for(*codec))
                            .map_err(|e| format!("{} 初始化失败：{e}", info.name))?,
                    )
                }
                None => Backend::Soft(Self::make_soft(&self.cfg)?),
            };
            self.cur = Some((dw, dh, *codec, name, backend));
        }
        bgra_to_i420(bgra, sw, sh, dw, dh, &mut self.yuv);
        let (_, _, codec, _, backend) = self.cur.as_mut().unwrap();
        let codec = *codec;
        match backend {
            Backend::Soft(enc) => {
                let buf = YUVBuffer::from_vec(self.yuv.clone(), dw, dh);
                if force_key {
                    enc.force_intra_frame();
                }
                let bs = enc.encode(&buf).map_err(|e| e.to_string())?;
                let ft = bs.frame_type();
                let data = bs.to_vec();
                if data.is_empty() || matches!(ft, FrameType::Skip | FrameType::Invalid) {
                    return Ok(Vec::new());
                }
                Ok(vec![EncodedFrame { data, key: matches!(ft, FrameType::IDR | FrameType::I), width: dw, height: dh, codec }])
            }
            Backend::Hw(enc) => {
                if force_key || fresh {
                    enc.force_keyframe();
                }
                i420_to_nv12(&self.yuv, dw, dh, &mut self.nv12);
                let pkts = enc.encode(&self.nv12).map_err(|e| format!("{} 编码失败：{e}", enc.name))?;
                Ok(pkts.into_iter().map(|(data, key)| EncodedFrame { data, key, width: dw, height: dh, codec }).collect())
            }
        }
    }
}

// ------------------------------------------------------------ 共享者

#[derive(Default)]
pub struct ShareStats {
    /// 实际采集到的帧率
    pub capture_fps: AtomicU32,
    /// 编码发送的帧率
    pub fps: AtomicU32,
    /// 平均每帧编码耗时（毫秒，含色彩转换）
    pub encode_ms: AtomicU32,
    /// 每秒被码率控制跳过的帧数
    pub skipped: AtomicU32,
    pub kbps: AtomicU32,
    pub width: AtomicU32,
    pub height: AtomicU32,
    pub error: Mutex<Option<String>>,
    /// 正在用的格式和编码器
    pub encoder: Mutex<String>,
}

struct RawFrame {
    w: usize,
    h: usize,
    data: Vec<u8>,
}

struct CapFlags {
    tx: SyncSender<RawFrame>,
    stats: Arc<ShareStats>,
    stop: Arc<AtomicBool>,
    interval: Duration,
}

struct CapHandler {
    flags: CapFlags,
    last: Option<Instant>,
    sec: (Instant, u32),
    scratch: Vec<u8>,
}

impl windows_capture::capture::GraphicsCaptureApiHandler for CapHandler {
    type Flags = CapFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: windows_capture::capture::Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { flags: ctx.flags, last: None, sec: (Instant::now(), 0), scratch: Vec::new() })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut windows_capture::frame::Frame,
        control: windows_capture::graphics_capture_api::InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.flags.stop.load(Ordering::Relaxed) {
            control.stop();
            return Ok(());
        }
        // 按目标帧率限速
        if self.last.is_some_and(|t| t.elapsed() < self.flags.interval.mul_f32(0.9)) {
            return Ok(());
        }
        self.last = Some(Instant::now());
        self.sec.1 += 1;
        if self.sec.0.elapsed() >= Duration::from_secs(1) {
            let fps = (self.sec.1 as f64 / self.sec.0.elapsed().as_secs_f64()).round() as u32;
            self.flags.stats.capture_fps.store(fps, Ordering::Relaxed);
            self.sec = (Instant::now(), 0);
        }
        let fb = frame.buffer()?;
        let (w, h) = (fb.width() as usize, fb.height() as usize);
        let data = fb.as_nopadding_buffer(&mut self.scratch).to_vec();
        // 编码跟不上就丢帧，只保留最新的（低延迟）
        match self.flags.tx.try_send(RawFrame { w, h, data }) {
            Ok(()) | Err(TrySendError::Full(_)) => Ok(()),
            Err(TrySendError::Disconnected(_)) => {
                control.stop();
                Ok(())
            }
        }
    }
}

pub struct Sharer {
    pub cfg: ShareConfig,
    state: Arc<VoiceState>,
    stop: Arc<AtomicBool>,
    force_key: Arc<AtomicBool>,
    pub stats: Arc<ShareStats>,
}

impl Sharer {
    pub fn start(cfg: ShareConfig, state: Arc<VoiceState>) -> Result<Self, String> {
        use windows_capture::capture::GraphicsCaptureApiHandler;
        use windows_capture::settings::*;
        let monitor = windows_capture::monitor::Monitor::from_index(cfg.monitor + 1)
            .or_else(|_| windows_capture::monitor::Monitor::primary())
            .map_err(|e| format!("找不到显示器：{e}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let force_key = Arc::new(AtomicBool::new(true));
        let stats = Arc::new(ShareStats::default());
        let (tx, rx) = mpsc::sync_channel::<RawFrame>(1);
        let interval = Duration::from_secs_f64(1.0 / cfg.fps as f64);
        let settings = Settings::new(
            monitor,
            CursorCaptureSettings::WithCursor,
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Custom(interval.mul_f32(0.9)),
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            CapFlags { tx, stats: stats.clone(), stop: stop.clone(), interval },
        );
        CapHandler::start_free_threaded(settings).map_err(|e| format!("屏幕采集启动失败：{e}"))?;

        // 画面码率计入 P2P 上行预算
        if let Some(l) = state.current_link() {
            l.set_video_bps(cfg.bitrate());
        }
        let (st, fk, sp, stx, c2) = (state.clone(), force_key.clone(), stop.clone(), stats.clone(), cfg.clone());
        std::thread::Builder::new()
            .name("video-encode".into())
            .spawn(move || encode_loop(c2, rx, st, fk, sp, stx))
            .map_err(|e| e.to_string())?;
        Ok(Self { cfg, state, stop, force_key, stats })
    }

    pub fn request_keyframe(&self) {
        self.force_key.store(true, Ordering::Relaxed);
    }
}

impl Drop for Sharer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(l) = self.state.current_link() {
            l.set_video_bps(0);
        }
    }
}

fn encode_loop(
    cfg: ShareConfig,
    rx: Receiver<RawFrame>,
    state: Arc<VoiceState>,
    force_key: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    stats: Arc<ShareStats>,
) {
    let pref = cfg.encoder.clone();
    let mut enc = VideoEncoder::new(cfg);
    let mut hw_broken = false;
    let mut frame_no = 0u32;
    let mut buf = Vec::new();
    let (mut sec_start, mut sec_frames, mut sec_bytes) = (Instant::now(), 0u32, 0usize);
    let (mut sec_enc, mut sec_skip, mut sec_all) = (Duration::ZERO, 0u32, 0u32);
    let mut last_key = Instant::now();
    let mut last_watchers = 0u32;
    while !stop.load(Ordering::Relaxed) {
        let Ok(raw) = rx.recv_timeout(Duration::from_millis(300)) else { continue };
        let Some(link) = state.current_link() else { continue };
        let ver = link.watchers_version();
        if !link.has_watchers() {
            last_watchers = ver;
            continue; // 没人看就不编码，省 CPU 和流量
        }
        if ver != last_watchers {
            last_watchers = ver;
            force_key.store(true, Ordering::Relaxed); // 有新观看者，马上给关键帧
        }
        // 每 10 秒兜底一个关键帧
        let key = force_key.swap(false, Ordering::Relaxed) || last_key.elapsed() > Duration::from_secs(10);
        // 选格式：所有观看者都能解的里面最省带宽的；硬件编码器出过错就退回软件编码
        let caps: Vec<Vec<String>> = {
            let all = state.caps.lock().unwrap();
            link.watcher_ids().iter().map(|id| all.get(id).cloned().unwrap_or_default()).collect()
        };
        let plan = plan_encoder(if hw_broken { "software" } else { &pref }, &common_decoders(&caps), hw_encoders());
        let te = Instant::now();
        let r = enc.encode(&raw.data, raw.w, raw.h, &plan, key);
        sec_enc += te.elapsed();
        sec_all += 1;
        let frames = match r {
            Ok(f) => f,
            Err(e) => {
                *stats.error.lock().unwrap() = Some(format!("{e}，已改用 CPU 编码"));
                hw_broken = true;
                force_key.store(true, Ordering::Relaxed);
                continue;
            }
        };
        if frames.is_empty() {
            sec_skip += 1;
        }
        if let Some(d) = enc.describe() {
            *stats.encoder.lock().unwrap() = d;
        }
        let Some(e2e) = state.e2e.lock().unwrap().clone() else { continue };
        for ef in frames {
            if ef.key {
                last_key = Instant::now();
            }
            let sealed = e2e.lock().unwrap().encrypt_video(&with_dims(&ef));
            let frags = udp::video_fragments(frame_no, udp::video_flags(ef.key, ef.codec.id()), &sealed);
            frame_no = frame_no.wrapping_add(1);
            link.send_video(&frags, &mut buf);
            sec_frames += 1;
            sec_bytes += sealed.len();
            stats.width.store(ef.width as u32, Ordering::Relaxed);
            stats.height.store(ef.height as u32, Ordering::Relaxed);
        }
        if sec_start.elapsed() >= Duration::from_secs(1) {
            let secs = sec_start.elapsed().as_secs_f64();
            stats.fps.store((sec_frames as f64 / secs).round() as u32, Ordering::Relaxed);
            stats.kbps.store((sec_bytes as f64 * 8.0 / 1000.0 / secs) as u32, Ordering::Relaxed);
            stats.encode_ms.store((sec_enc.as_secs_f64() * 1000.0 / sec_all.max(1) as f64).round() as u32, Ordering::Relaxed);
            stats.skipped.store(sec_skip, Ordering::Relaxed);
            (sec_start, sec_frames, sec_bytes) = (Instant::now(), 0, 0);
            (sec_enc, sec_skip, sec_all) = (Duration::ZERO, 0, 0);
        }
    }
}

/// 帧负载：H.264 保持原样（兼容旧客户端）；AV1 / HEVC 前加 宽(u16) | 高(u16)，解码器需要事先知道尺寸
pub fn with_dims(ef: &EncodedFrame) -> Vec<u8> {
    if ef.codec == Codec::H264 {
        return ef.data.clone();
    }
    let mut v = Vec::with_capacity(ef.data.len() + 4);
    v.extend_from_slice(&(ef.width as u16).to_le_bytes());
    v.extend_from_slice(&(ef.height as u16).to_le_bytes());
    v.extend_from_slice(&ef.data);
    v
}

/// 拆出 (宽, 高, 码流)；H.264 没有尺寸头
fn split_dims(codec: Codec, data: &[u8]) -> Option<(u32, u32, &[u8])> {
    if codec == Codec::H264 {
        return Some((0, 0, data));
    }
    if data.len() < 4 {
        return None;
    }
    let w = u16::from_le_bytes([data[0], data[1]]) as u32;
    let h = u16::from_le_bytes([data[2], data[3]]) as u32;
    Some((w, h, &data[4..]))
}

// ------------------------------------------------------------ 接收

pub struct DecodedFrame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
    pub seq: u64,
}

/// 一路正在观看的视频（界面读取）
#[derive(Default)]
pub struct View {
    pub frame: Mutex<Option<DecodedFrame>>,
    pub fps: AtomicU32,
    seq: AtomicU64,
    decode_failed: AtomicBool,
    /// 当前解码方式，例如 "AV1 硬解"
    pub decoder: Mutex<String>,
}

struct Stream {
    cur: Option<u32>,
    frags: Vec<Option<Vec<u8>>>,
    got: usize,
    flags: u8,
    last_done: Option<u32>,
    need_key: bool,
    last_req: Option<Instant>,
    dec_tx: SyncSender<(u8, Vec<u8>)>,
    view: Arc<View>,
}

#[derive(Default)]
pub struct VideoRx {
    streams: Mutex<HashMap<u32, Stream>>,
    ctx: Mutex<Option<eframe::egui::Context>>,
}

impl VideoRx {
    pub fn set_ctx(&self, ctx: eframe::egui::Context) {
        *self.ctx.lock().unwrap() = Some(ctx);
    }

    /// 开始 / 停止观看某人
    pub fn watch(&self, from: u32, on: bool) -> Option<Arc<View>> {
        let mut s = self.streams.lock().unwrap();
        if !on {
            s.remove(&from); // 解码线程随通道关闭退出
            return None;
        }
        if let Some(st) = s.get(&from) {
            return Some(st.view.clone());
        }
        let view = Arc::new(View::default());
        let (tx, rx) = mpsc::sync_channel::<(u8, Vec<u8>)>(64);
        let (v, ctx) = (view.clone(), self.ctx.lock().unwrap().clone());
        std::thread::Builder::new().name("video-decode".into()).spawn(move || decode_loop(rx, v, ctx)).ok()?;
        s.insert(
            from,
            Stream {
                cur: None,
                frags: Vec::new(),
                got: 0,
                flags: 0,
                last_done: None,
                need_key: true,
                last_req: None,
                dec_tx: tx,
                view: view.clone(),
            },
        );
        Some(view)
    }

    pub fn clear(&self) {
        self.streams.lock().unwrap().clear();
    }

    /// 收到一个视频分片（服务器中转 / P2P / TCP 隧道）。返回 true 表示需要向发送者请求关键帧。
    pub fn on_packet(&self, payload: &[u8], decrypt: impl FnOnce(u32, &[u8]) -> Option<Vec<u8>>) -> Option<u32> {
        let (from, f, data) = udp::read_server_video(payload)?;
        let mut streams = self.streams.lock().unwrap();
        let s = streams.get_mut(&from)?; // 没在看就丢弃
        if s.view.decode_failed.swap(false, Ordering::Relaxed) {
            s.need_key = true;
        }
        let complete = s.add(f, data);
        if let Some((frame_id, flags, blob)) = complete {
            let key = flags & VIDEO_KEY != 0;
            match decrypt(from, &blob) {
                Some(bits) if key || !s.need_key => {
                    if key {
                        s.need_key = false;
                    }
                    if s.dec_tx.try_send((udp::flags_codec(flags), bits)).is_err() {
                        s.need_key = true; // 解码跟不上，丢帧后要从关键帧重新开始
                    }
                }
                Some(_) => {} // 等关键帧
                None => s.need_key = true,
            }
            s.last_done = Some(frame_id);
        }
        // 需要关键帧时，最多每 300 ms 请求一次
        if s.need_key && s.last_req.is_none_or(|t| t.elapsed() > Duration::from_millis(300)) {
            s.last_req = Some(Instant::now());
            return Some(from);
        }
        None
    }
}

impl Stream {
    /// 加入分片；凑齐一帧时返回 (帧号, flags, 数据)
    fn add(&mut self, f: VideoFrag, data: &[u8]) -> Option<(u32, u8, Vec<u8>)> {
        match self.cur {
            Some(c) if f.frame == c => {}
            Some(c) if f.frame.wrapping_sub(c) > 0x8000_0000 => return None, // 旧帧的迟到分片
            _ => {
                // 开始新的一帧：上一帧没收齐，或中间整帧丢失 → 之后的 P 帧都解不对，等关键帧
                let continuous = self.last_done.is_some_and(|d| f.frame == d.wrapping_add(1));
                if !continuous {
                    self.need_key = true;
                }
                self.cur = Some(f.frame);
                self.frags = vec![None; f.count as usize];
                self.got = 0;
                self.flags = f.flags;
            }
        }
        if f.count as usize != self.frags.len() || self.frags[f.idx as usize].is_some() {
            return None;
        }
        self.frags[f.idx as usize] = Some(data.to_vec());
        self.got += 1;
        if self.got < self.frags.len() {
            return None;
        }
        let all: Vec<u8> = self.frags.drain(..).flatten().flatten().collect();
        self.cur = None;
        Some((f.frame, self.flags, all))
    }
}

enum Dec {
    Open(openh264::decoder::Decoder),
    Mf(MfDecoder),
}

fn make_decoder(codec: Codec, hw: bool, w: u32, h: u32) -> Option<Dec> {
    // AV1 一律硬解（系统 AV1 扩展的软解对非满尺寸画面不可靠；不支持硬解的电脑不会上报 AV1）
    let hw = hw || codec == Codec::Av1;
    // 全部优先走 Media Foundation（系统 H.264 解码器支持显卡编码器常用的 High 档次）；
    // 系统没有解码器（例如 Windows N 版）时 H.264 退回内置 openh264（只支持 Baseline，配合 openh264 编码）
    if mfenc::mf_init().is_err() {
        return (codec == Codec::H264).then(|| openh264::decoder::Decoder::new().ok().map(Dec::Open)).flatten();
    }
    match MfDecoder::new(codec, hw, w, h) {
        Ok(d) => Some(Dec::Mf(d)),
        Err(_) if hw => MfDecoder::new(codec, false, w, h).ok().map(Dec::Mf), // 硬解不行就退回软解
        Err(_) if codec == Codec::H264 => openh264::decoder::Decoder::new().ok().map(Dec::Open),
        Err(_) => None,
    }
}

fn decode_loop(rx: Receiver<(u8, Vec<u8>)>, view: Arc<View>, ctx: Option<eframe::egui::Context>) {
    let mut dec: Option<(Codec, bool, (u32, u32), Dec)> = None;
    let (mut sec_start, mut sec_frames) = (Instant::now(), 0u32);
    let mut spare: Vec<u8> = Vec::new();
    while let Ok(first) = rx.recv() {
        // 必须每帧都解码（P 帧依赖前一帧），但积压时只把最新一帧转成 RGBA 显示
        let mut batch = vec![first];
        batch.extend(rx.try_iter());
        let last = batch.len() - 1;
        for (i, (cid, bits)) in batch.iter().enumerate() {
            let codec = Codec::from_id(*cid);
            let Some((w0, h0, bits)) = split_dims(codec, bits) else { continue };
            let hw = HW_DECODE.load(Ordering::Relaxed);
            // 格式 / 尺寸 / 软硬解设置变了就换解码器（H.264 不带尺寸，尺寸恒为 0）
            if !matches!(&dec, Some((c, h, dims, _)) if *c == codec && *h == hw && *dims == (w0, h0)) {
                dec = make_decoder(codec, hw, w0, h0).map(|d| (codec, hw, (w0, h0), d));
                *view.decoder.lock().unwrap() = match &dec {
                    Some((c, _, _, Dec::Mf(m))) => format!("{} {}", c.label(), if m.hardware { "硬解" } else { "软解" }),
                    Some((c, _, _, Dec::Open(_))) => format!("{} 软解（OpenH264）", c.label()),
                    None => format!("本机无法解码 {}", codec.label()),
                };
                view.decode_failed.store(true, Ordering::Relaxed); // 新解码器要从关键帧开始
            }
            let Some((_, _, _, d)) = dec.as_mut() else { continue };
            let show = i == last;
            let mut publish = |w: usize, h: usize, fill: &mut dyn FnMut(&mut Vec<u8>)| {
                let mut rgba = std::mem::take(&mut spare);
                rgba.resize(w * h * 4, 0);
                fill(&mut rgba);
                let seq = view.seq.fetch_add(1, Ordering::Relaxed) + 1;
                if let Some(o) = view.frame.lock().unwrap().replace(DecodedFrame { width: w, height: h, rgba, seq }) {
                    spare = o.rgba; // 复用缓冲
                }
                sec_frames += 1;
                if let Some(c) = &ctx {
                    c.request_repaint();
                }
            };
            match d {
                Dec::Open(o) => match o.decode(bits) {
                    Ok(Some(yuv)) if show => {
                        let (w, h) = yuv.dimensions();
                        publish(w, h, &mut |buf| yuv.write_rgba8(buf));
                    }
                    Ok(_) => {}
                    Err(_) => view.decode_failed.store(true, Ordering::Relaxed),
                },
                Dec::Mf(m) => match m.decode(bits) {
                    Ok(frames) => {
                        if let (true, Some(f)) = (show, frames.last()) {
                            publish(f.width, f.height, &mut |buf| nv12_to_rgba(&f.data, f.width, f.height, buf));
                        }
                    }
                    Err(_) => view.decode_failed.store(true, Ordering::Relaxed),
                },
            }
        }
        if sec_start.elapsed() >= Duration::from_secs(1) {
            view.fps.store((sec_frames as f64 / sec_start.elapsed().as_secs_f64()).round() as u32, Ordering::Relaxed);
            (sec_start, sec_frames) = (Instant::now(), 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_frame(w: usize, h: usize, t: usize) -> Vec<u8> {
        let mut v = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let p = (y * w + x) * 4;
                v[p] = ((x + t * 7) % 256) as u8;
                v[p + 1] = ((y + t * 3) % 256) as u8;
                v[p + 2] = (((x ^ y) + t) % 256) as u8;
                v[p + 3] = 255;
            }
        }
        v
    }

    #[test]
    fn target_sizes() {
        let c = |h| ShareConfig { height: h, fps: 30, ..Default::default() };
        assert_eq!(c(1080).target_size(1920, 1080), (1920, 1080));
        assert_eq!(c(720).target_size(1920, 1080), (1280, 720));
        assert_eq!(c(480).target_size(1920, 1080), (854, 480));
        assert_eq!(c(1080).target_size(1280, 720), (1280, 720)); // 不放大
        assert_eq!(c(720).target_size(2560, 1440), (1280, 720));
        assert!(c(1080).bitrate() > c(720).bitrate() && c(720).bitrate() > c(480).bitrate());
    }

    /// 编码 → 加密 → 分片 → 乱序/丢包 → 重组 → 解密 → 解码，全流程
    #[test]
    fn encode_fragment_reassemble_decode() {
        use voc_proto::e2e::{ReceiverKeys, SenderKey};
        let mut sk = SenderKey::new(1);
        let mut rk = ReceiverKeys::default();
        rk.install(&sk.export());
        let rx = VideoRx::default();
        let view = rx.watch(7, true).unwrap();
        let mut enc = VideoEncoder::new(ShareConfig { height: 480, fps: 30, ..Default::default() });
        let plan = (Codec::H264, None);
        let (sw, sh) = (1280, 720);
        let mut key_requests = 0;
        let mut sent = 0;
        for t in 0..12u32 {
            let ef = match enc.encode(&test_frame(sw, sh, t as usize), sw, sh, &plan, t == 0 || t == 8).unwrap().pop() {
                Some(f) => f,
                None => continue,
            };
            assert_eq!((ef.width, ef.height), (854, 480));
            if t == 0 {
                assert!(ef.key);
            }
            let sealed = sk.encrypt_video(7, &ef.data);
            let mut frags = udp::video_fragments(sent, udp::video_flags(ef.key, ef.codec.id()), &sealed);
            sent += 1;
            frags.reverse(); // 乱序到达
            if t == 4 && frags.len() > 1 {
                frags.pop(); // 第 4 帧丢一片 → 之后要等关键帧
            }
            for f in frags {
                if rx.on_packet(&udp::server_video(7, &f), |from, b| rk.decrypt_video(from, b)).is_some() {
                    key_requests += 1;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
        let fr = view.frame.lock().unwrap();
        let fr = fr.as_ref().expect("decoded at least one frame");
        assert_eq!((fr.width, fr.height), (854, 480));
        assert_eq!(fr.rgba.len(), 854 * 480 * 4);
        assert!(key_requests >= 1, "packet loss must trigger a keyframe request");
        // 没在看的人的包直接丢弃
        assert!(rx.on_packet(&udp::server_video(99, &udp::video_fragments(0, VIDEO_KEY, b"x")[0]), |_, _| None).is_none());
    }

    #[test]
    fn bgra_to_i420_colors() {
        // 纯白 → Y≈235，U=V≈128；纯黑 → Y≈16
        let white = vec![255u8; 4 * 4 * 4];
        let mut out = Vec::new();
        bgra_to_i420(&white, 4, 4, 4, 4, &mut out);
        assert!(out[..16].iter().all(|&y| (234..=236).contains(&y)));
        assert!(out[16..].iter().all(|&c| (127..=129).contains(&c)));
        let black = [0u8, 0, 0, 255].repeat(16);
        bgra_to_i420(&black, 4, 4, 2, 2, &mut out);
        assert!(out[..4].iter().all(|&y| y == 16));
    }

    #[test]
    fn codec_negotiation() {
        let hw = vec![
            HwEncoderInfo { name: "NV AV1".into(), codec: Codec::Av1, vendor: 0x10de, gpu: "RTX".into() },
            HwEncoderInfo { name: "NV HEVC".into(), codec: Codec::Hevc, vendor: 0x10de, gpu: "RTX".into() },
            HwEncoderInfo { name: "AMD HEVC".into(), codec: Codec::Hevc, vendor: 0x1002, gpu: "Radeon".into() },
            HwEncoderInfo { name: "AMD H264".into(), codec: Codec::H264, vendor: 0x1002, gpu: "Radeon".into() },
        ];
        let v = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // 所有观看者都能解 AV1 → AV1
        let all = common_decoders(&[v(&["av1", "hevc", "h264"]), v(&["av1", "hevc", "h264"])]);
        assert_eq!(plan_encoder("auto", &all, &hw).0, Codec::Av1);
        // 有人不能解 AV1 → HEVC；有旧版客户端（没上报）→ 只能 H.264
        let some = common_decoders(&[v(&["av1", "hevc", "h264"]), v(&["hevc", "h264"])]);
        assert_eq!(plan_encoder("auto", &some, &hw).0, Codec::Hevc);
        let old = common_decoders(&[v(&["av1", "hevc", "h264"]), vec![]]);
        assert_eq!(old, vec![Codec::H264]);
        assert_eq!(plan_encoder("auto", &old, &hw).1.unwrap().name, "AMD H264");
        assert!(plan_encoder("vendor:10de", &old, &hw).1.is_none(), "NVIDIA has no H.264 here -> software");
        // 指定 AMD：只用 AMD 的编码器
        let (c, e) = plan_encoder("vendor:1002", &all, &hw);
        assert_eq!((c, e.unwrap().name.as_str()), (Codec::Hevc, "AMD HEVC"));
        assert_eq!(plan_encoder("software", &all, &hw), (Codec::H264, None));
        // 码率：AV1 比 H.264 省
        let cfg = ShareConfig { height: 1080, fps: 60, ..Default::default() };
        assert!(cfg.bitrate_for(Codec::Av1) < cfg.bitrate_for(Codec::Hevc));
        assert!(cfg.bitrate_for(Codec::Hevc) < cfg.bitrate_for(Codec::H264));
    }

    #[test]
    fn nv12_roundtrip_colors() {
        // 纯红 BGRA → I420 → NV12 → RGBA，颜色应基本还原
        let red = [0u8, 0, 255, 255].repeat(16);
        let mut i420 = Vec::new();
        bgra_to_i420(&red, 4, 4, 4, 4, &mut i420);
        let mut nv12 = Vec::new();
        i420_to_nv12(&i420, 4, 4, &mut nv12);
        let mut rgba = Vec::new();
        nv12_to_rgba(&nv12, 4, 4, &mut rgba);
        assert!(rgba[0] > 230 && rgba[1] < 30 && rgba[2] < 30, "{:?}", &rgba[..4]);
    }

    /// 真实显卡：每种硬件编码器编 → 系统解码器软解 / 硬解，画面尺寸正确（需要显卡，默认不跑）
    #[test]
    #[ignore]
    fn hw_encode_decode_all_codecs() {
        let (sw, sh) = (1920, 1080);
        for info in hw_encoders() {
            if !local_decoders().contains(&info.codec) {
                continue;
            }
            let mut enc = VideoEncoder::new(ShareConfig { height: 720, fps: 60, ..Default::default() });
            let plan = (info.codec, Some(info.clone()));
            let mut pkts = Vec::new();
            for t in 0..20 {
                pkts.extend(enc.encode(&test_frame(sw, sh, t), sw, sh, &plan, t == 0).unwrap());
            }
            assert!(pkts[0].key, "{} first frame must be key", info.name);
            for hw in [false, true] {
                let mut d = make_decoder(info.codec, hw, 1280, 720).expect("decoder");
                let mut got = 0;
                for (pi, p) in pkts.iter().enumerate() {
                    if let Dec::Mf(m) = &mut d {
                        let r = m.decode(&p.data);
                        if let Err(e) = &r {
                            panic!("{} hw={hw} packet {pi} ({} bytes, key={}): {e}", info.name, p.data.len(), p.key);
                        }
                        for f in r.unwrap() {
                            assert_eq!((f.width, f.height), (1280, 720), "{}", info.name);
                            got += 1;
                        }
                    } else if let Dec::Open(o) = &mut d {
                        if o.decode(&p.data).unwrap().is_some() {
                            got += 1;
                        }
                    }
                }
                println!("{} -> decode hw={hw}: {got}/{} frames", info.name, pkts.len());
                assert!(got >= pkts.len() - 2);
            }
        }
    }
}
