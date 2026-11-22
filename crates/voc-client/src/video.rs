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
use voc_proto::udp::{self, VIDEO_KEY};

/// 观看时用硬件解码（DXVA）；否则 CPU 软解
pub static HW_DECODE: AtomicBool = AtomicBool::new(false);
/// 新开的观看窗口默认的播放方式：true = 流畅优先（默认），false = 延迟优先（设置里记住上次的选择）
pub static SMOOTH_PLAYBACK: AtomicBool = AtomicBool::new(true);

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
    /// 画质（码率档位）：0 标准 / 1 高 / 2 极高
    pub quality: u8,
    /// 画面不动时少发（按系统报告的屏幕变化区域判断，至少每秒仍发一帧）
    pub skip_static: bool,
    /// 网络差时自动降分辨率（原画层也会跟着降）
    pub auto_res: bool,
    /// 只走 P2P：画面、补发、鼠标、共享声音都不发给服务器；打不通直连、也没有观看者能转给他的人看不了。
    /// 不受直连上行预算限制，能打通的观看者都直连发（人多时可能占满上行）
    pub p2p_only: bool,
    /// 按延迟变化提前降码率（第 36 项，默认关）：画面走的路往返时间在涨（排队了、还没丢包）就先降码率，空了再升。
    /// 打开才会调「原画」层的码率（第 20 项：原画默认不调）
    pub delay_bwe: bool,
}

impl Default for ShareConfig {
    fn default() -> Self {
        Self { height: 720, fps: 30, monitor: 0, encoder: "auto".into(), quality: 1, skip_static: true, auto_res: false, p2p_only: false, delay_bwe: false }
    }
}

/// 自动降分辨率：按码率比例选高度（1080 → 900 → 720 → 540，不超过设定值）
pub fn res_for_scale(full: u32, scale: f32) -> u32 {
    let mut steps = vec![full];
    steps.extend([900, 720, 540].into_iter().filter(|h| *h < full));
    let i = if scale >= 0.85 {
        0
    } else if scale >= 0.6 {
        1
    } else if scale >= 0.42 {
        2
    } else {
        3
    };
    steps[i.min(steps.len() - 1)]
}

/// 画质档位：(名字, 码率倍数)。1080P 60 帧约 5.5 / 12 / 25 Mbps
pub const QUALITIES: [(&str, f32); 3] = [("标准", 1.0), ("高", 2.2), ("极高", 4.5)];

impl ShareConfig {
    /// 低带宽模式下的实际设置：最高 480P 30 帧
    pub fn effective(&self, low_bandwidth: bool) -> Self {
        let mut c = self.clone();
        if low_bandwidth {
            c.height = c.height.min(480);
            c.fps = c.fps.min(30);
            c.quality = 0;
        }
        c
    }

    /// 低画质层：360P，帧率同原画，标准码率
    pub fn low_layer(&self) -> Self {
        Self { height: 360, quality: 0, ..self.clone() }
    }

    pub fn desc(&self) -> String {
        format!("{}p{}", self.height, self.fps)
    }

    /// 目标码率（bps）
    pub fn bitrate(&self) -> u32 {
        let kbps = match (self.height, self.fps >= 60) {
            (h, false) if h <= 360 => 450,
            (h, true) if h <= 360 => 650,
            (h, false) if h <= 480 => 900,
            (h, true) if h <= 480 => 1400,
            (h, false) if h <= 720 => 2000,
            (h, true) if h <= 720 => 3000,
            (_, false) => 3500,
            (_, true) => 5500,
        };
        let mult = QUALITIES.get(self.quality as usize).map(|q| q.1).unwrap_or(1.0);
        (kbps as f32 * mult) as u32 * 1000
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

/// 按偏好和观看者能解的格式，决定用哪种格式、哪个编码器（None = openh264 软件编码）。
/// capture_vendor：画面所在显卡的厂商号。同一种格式有好几块显卡能编时优先用它（才能走显卡直通）
pub fn plan_encoder(pref: &str, allowed: &[Codec], hw: &[HwEncoderInfo], capture_vendor: Option<u32>) -> (Codec, Option<HwEncoderInfo>) {
    if pref == "software" {
        return (Codec::H264, None);
    }
    let vendor = pref.strip_prefix("vendor:").and_then(|v| u32::from_str_radix(v, 16).ok());
    for codec in [Codec::Av1, Codec::Hevc, Codec::H264] {
        if !allowed.contains(&codec) {
            continue;
        }
        let ok = |e: &&HwEncoderInfo| e.codec == codec && vendor.is_none_or(|v| e.vendor == v);
        let same_gpu = hw.iter().filter(ok).find(|e| Some(e.vendor) == capture_vendor);
        if let Some(e) = same_gpu.or_else(|| hw.iter().find(ok)) {
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
    /// 显卡直通用的设备和视频处理器
    gpu: Option<crate::mf::gpu::Converter>,
    /// 码率比例（网络拥堵时降低，0.3–1.0）
    rate_scale: f32,
    /// 自动降分辨率时的实际高度（None = 按设定）
    height: Option<u32>,
    /// 下一帧的长期参考帧安排；已经送进编码器、还没出来的帧的安排（硬件编码器可能晚一帧出）
    ltr_next: LtrTag,
    ltr_inflight: std::collections::VecDeque<LtrTag>,
    /// 编码器重建过几次（重建后长期参考帧全没了）
    ltr_epoch: u32,
}

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub key: bool,
    pub width: usize,
    pub height: usize,
    pub codec: Codec,
    /// 这一帧存进了第几个长期参考槽
    pub ltr_mark: Option<u8>,
    /// 恢复帧（只参考长期参考帧）
    pub recover: bool,
}

/// 下一帧的长期参考帧安排
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LtrTag {
    pub mark: Option<u8>,
    pub use_slot: Option<u8>,
}

impl VideoEncoder {
    pub fn new(cfg: ShareConfig) -> Self {
        Self {
            cfg,
            cur: None,
            yuv: Vec::new(),
            nv12: Vec::new(),
            gpu: None,
            rate_scale: 1.0,
            height: None,
            ltr_next: LtrTag::default(),
            ltr_inflight: Default::default(),
            ltr_epoch: 0,
        }
    }

    /// 当前编码器支持长期参考帧
    pub fn ltr_supported(&self) -> bool {
        matches!(&self.cur, Some((_, _, _, _, Backend::Hw(e))) if e.ltr)
    }

    /// 编码器重建过几次（变了就说明长期参考帧全没了）
    pub fn ltr_epoch(&self) -> u32 {
        self.ltr_epoch
    }

    /// 安排下一帧的长期参考帧
    pub fn set_ltr(&mut self, tag: LtrTag) {
        self.ltr_next = tag;
    }

    /// 硬件编码前：把这一帧的长期参考帧安排交给编码器，记下来等输出时对上
    fn apply_ltr(ltr_next: &mut LtrTag, inflight: &mut std::collections::VecDeque<LtrTag>, enc: &HwEncoder) {
        let tag = std::mem::take(ltr_next);
        if enc.ltr {
            enc.set_ltr(tag.mark, tag.use_slot);
        }
        inflight.push_back(if enc.ltr { tag } else { LtrTag::default() });
    }

    fn tag_outputs(inflight: &mut std::collections::VecDeque<LtrTag>, pkts: Vec<(Vec<u8>, bool)>, dw: usize, dh: usize, codec: Codec) -> Vec<EncodedFrame> {
        pkts.into_iter()
            .map(|(data, key)| {
                let t = inflight.pop_front().unwrap_or_default();
                EncodedFrame { data, key, width: dw, height: dh, codec, ltr_mark: t.mark, recover: t.use_slot.is_some() && !key }
            })
            .collect()
    }

    /// 临时改编码高度（自动降分辨率）。下一帧按新尺寸重建编码器，从关键帧开始
    pub fn set_height(&mut self, h: u32) {
        self.height = (h != self.cfg.height).then_some(h);
    }

    fn target_size(&self, sw: usize, sh: usize) -> (usize, usize) {
        ShareConfig { height: self.height.unwrap_or(self.cfg.height), ..self.cfg.clone() }.target_size(sw, sh)
    }

    /// 调整码率比例（硬件编码器即时生效；openh264 不支持中途改，保持原码率）
    pub fn set_rate_scale(&mut self, scale: f32) {
        self.rate_scale = scale;
        if let Some((_, _, codec, _, Backend::Hw(enc))) = &self.cur {
            enc.set_bitrate((self.cfg.bitrate_for(*codec) as f32 * scale) as u32);
        }
    }

    /// 显卡直通：画面在显卡上缩放、转 NV12，直接交给硬件编码器（不经过内存）
    pub fn encode_gpu(
        &mut self,
        f: &crate::mf::gpu::GpuFrame,
        plan: &(Codec, Option<HwEncoderInfo>),
        force_key: bool,
        keep: bool,
    ) -> Result<Vec<EncodedFrame>, String> {
        let (codec, Some(info)) = plan else { return Err("没有硬件编码器".into()) };
        let (dw, dh) = self.target_size(f.width as usize, f.height as usize);
        if !self.gpu.as_ref().is_some_and(|g| g.luid() == f.luid) {
            self.gpu = Some(crate::mf::gpu::Converter::new(f.luid).map_err(|e| format!("显卡设备创建失败：{e}"))?);
            self.cur = None;
        }
        let name = format!("{} · 显卡直通", info.name);
        let fresh = !matches!(&self.cur, Some((w, h, c, n, _)) if *w == dw && *h == dh && c == codec && *n == name);
        if fresh {
            mfenc::mf_init().map_err(|e| e.to_string())?;
            let g = self.gpu.as_ref().unwrap();
            let bitrate = (self.cfg.bitrate_for(*codec) as f32 * self.rate_scale) as u32;
            let enc = HwEncoder::new_on_device(info, dw as u32, dh as u32, self.cfg.fps, bitrate, g.mgr.clone(), g.dev.clone())
                .map_err(|e| format!("{} 初始化失败：{e}", info.name))?;
            self.cur = Some((dw, dh, *codec, name, Backend::Hw(enc)));
            self.ltr_epoch += 1;
            self.ltr_inflight.clear();
        }
        let tex = self.gpu.as_mut().unwrap().convert(f, dw as u32, dh as u32, self.cfg.fps, keep).map_err(|e| format!("显卡缩放失败：{e}"))?;
        let Some((_, _, codec, _, Backend::Hw(enc))) = self.cur.as_mut() else { return Err("编码器状态不对".into()) };
        if force_key || fresh {
            enc.force_keyframe();
        }
        let codec = *codec;
        Self::apply_ltr(&mut self.ltr_next, &mut self.ltr_inflight, enc);
        let pkts = enc.encode_texture(&tex).map_err(|e| format!("{} 编码失败：{e}", enc.name))?;
        Ok(Self::tag_outputs(&mut self.ltr_inflight, pkts, dw, dh, codec))
    }

    /// 跳过一帧显卡画面：把共享纹理还给采集端
    pub fn give_back(&mut self, f: &crate::mf::gpu::GpuFrame) {
        if !self.gpu.as_ref().is_some_and(|g| g.luid() == f.luid) {
            match crate::mf::gpu::Converter::new(f.luid) {
                Ok(c) => {
                    self.gpu = Some(c);
                    self.cur = None;
                }
                Err(_) => return,
            }
        }
        let _ = self.gpu.as_mut().unwrap().give_back(f);
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

    /// 当前编码器开了逐行刷新（不用定期发关键帧）
    pub fn intra_refresh(&self) -> bool {
        matches!(&self.cur, Some((_, _, _, _, Backend::Hw(e))) if e.intra_refresh)
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
        let (dw, dh) = self.target_size(sw, sh);
        let (codec, hw) = plan;
        let name = hw.as_ref().map(|h| h.name.clone()).unwrap_or_else(|| "OpenH264（CPU）".into());
        let fresh = !matches!(&self.cur, Some((w, h, c, n, _)) if *w == dw && *h == dh && c == codec && *n == name);
        if fresh {
            // 尺寸 / 格式 / 编码器变了就重建
            let backend = match hw {
                Some(info) => {
                    mfenc::mf_init().map_err(|e| e.to_string())?;
                    Backend::Hw(
                        HwEncoder::new(info, dw as u32, dh as u32, self.cfg.fps, (self.cfg.bitrate_for(*codec) as f32 * self.rate_scale) as u32)
                            .map_err(|e| format!("{} 初始化失败：{e}", info.name))?,
                    )
                }
                None => Backend::Soft(Self::make_soft(&self.cfg)?),
            };
            self.cur = Some((dw, dh, *codec, name, backend));
            self.ltr_epoch += 1;
            self.ltr_inflight.clear();
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
                self.ltr_next = LtrTag::default();
                Ok(vec![EncodedFrame {
                    data,
                    key: matches!(ft, FrameType::IDR | FrameType::I),
                    width: dw,
                    height: dh,
                    codec,
                    ltr_mark: None,
                    recover: false,
                }])
            }
            Backend::Hw(enc) => {
                if force_key || fresh {
                    enc.force_keyframe();
                }
                i420_to_nv12(&self.yuv, dw, dh, &mut self.nv12);
                Self::apply_ltr(&mut self.ltr_next, &mut self.ltr_inflight, enc);
                let pkts = enc.encode(&self.nv12).map_err(|e| format!("{} 编码失败：{e}", enc.name))?;
                Ok(Self::tag_outputs(&mut self.ltr_inflight, pkts, dw, dh, codec))
            }
        }
    }
}

// ------------------------------------------------------------ 共享者

#[derive(Default)]
pub struct ShareStats {
    /// 实际采集到的帧率
    pub capture_fps: AtomicU32,
    /// 系统实际给了多少帧（限帧之前；比所选帧率低说明屏幕 / 游戏本身就没那么多帧）
    pub source_fps: AtomicU32,
    /// 最近一秒判断为拥塞（延迟在涨 / 发送队列积压）
    pub congested: AtomicBool,
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
    /// 画面处理方式：显卡直通 / CPU 转换
    pub pipeline: Mutex<String>,
    /// 当前码率占设定值的百分比（网络拥堵时自动降低）
    pub rate_pct: AtomicU32,
    /// 冗余校验包：校验片占数据片的百分比（原画层），0 = 没加
    pub fec_pct: AtomicU32,
    /// 两条路同时发（直连 + 经服务器）的观看者人数
    pub dual_viewers: AtomicU32,
    /// 发了几帧恢复帧（代替关键帧）
    pub ltr_recovers: AtomicU32,
    /// 鼠标单独发（截屏不带鼠标）
    pub cursor_separate: AtomicBool,
    /// 被请求补发的分片累计、观看者回报的最大原始丢包率（千分之几）
    pub resent_total: AtomicU32,
    pub viewer_loss: AtomicU32,
    /// 每秒因为画面没变而没发的帧数
    pub static_skipped: AtomicU32,
    /// 调试：最近一帧系统报告的变化区域数（-1 = 取不到）、面积占比（千分之几）
    pub dirty_last: std::sync::atomic::AtomicI32,
    pub dirty_area: AtomicU32,
    /// 自动降分辨率后的高度（0 = 没降）
    pub auto_height: AtomicU32,
    /// 看低画质层的人数
    pub low_watchers: AtomicU32,
}

struct RawFrame {
    w: usize,
    h: usize,
    data: Vec<u8>,
}

/// 采集到的一帧：拷回内存的 BGRA，或留在显卡上的共享纹理
enum CapFrame {
    Cpu(RawFrame),
    Gpu(crate::mf::gpu::GpuFrame),
}

struct CapFlags {
    tx: SyncSender<CapFrame>,
    /// 画面不动时少发；有人要关键帧时不跳
    skip_static: bool,
    force_key: Arc<AtomicBool>,
    /// 走显卡直通（编码线程判断不能用时关掉，之后改拷回内存）
    gpu: Arc<AtomicBool>,
    stats: Arc<ShareStats>,
    stop: Arc<AtomicBool>,
    interval: Duration,
}

struct CapHandler {
    flags: CapFlags,
    /// 下一帧的预定时间（按时间表限帧）
    next_due: Option<Instant>,
    /// 这一秒：(开始, 收下的帧, 系统给的帧)
    sec: (Instant, u32, u32),
    scratch: Vec<u8>,
    side: crate::mf::gpu::CaptureSide,
    /// 系统确实在报告变化区域（报告过非空的才信「没变化」）
    dirty_works: bool,
    /// 上次真正发出一帧的时间
    last_sent: Option<Instant>,
    skipped: u32,
}

/// 按时间表限帧：下一帧的预定时间 = 上一帧的**预定**时间 + 间隔（不是实际收到的时间 + 间隔）。
/// 系统给帧的时间是一格一格的（对齐显示器刷新、只在画面变化时给），「离上一帧太近就丢」每次都要多等一格，
/// 实际帧率总比所选的低（选 30 只有 20 多、选 60 只有 50 多）；按时间表取，早一点晚一点互相抵消，平均正好是所选帧率。
/// 提前半个间隔以内到的也收；落后超过一个间隔（画面静止了一阵）就从现在重新排，不会一下子补一串。
fn pace_accept(next_due: &mut Option<Instant>, now: Instant, interval: Duration) -> bool {
    let due = match *next_due {
        None => now,
        Some(d) if now + interval / 2 < d => return false,
        Some(d) if now > d + interval => now,
        Some(d) => d,
    };
    *next_due = Some(due + interval);
    true
}

/// 变化面积小于这个比例（千分之几）算「变化很小」（时钟、托盘图标、光标、打字）
const SMALL_CHANGE_PERMILLE: u32 = 20;

/// 这一帧能不能跳过（系统确实会报告变化区域、没人在等关键帧时）：
/// 完全没变 → 至少每秒发一帧；变化很小 → 最多每秒 10 帧；大范围变化 → 照常发
fn can_skip_static(dirty: Option<(usize, u32)>, dirty_works: bool, since_sent: Option<Duration>, want_key: bool) -> bool {
    let (Some((n, area)), true, false, Some(since)) = (dirty, dirty_works, want_key, since_sent) else { return false };
    if n == 0 {
        since < Duration::from_secs(1)
    } else {
        area < SMALL_CHANGE_PERMILLE && since < Duration::from_millis(100)
    }
}

impl windows_capture::capture::GraphicsCaptureApiHandler for CapHandler {
    type Flags = CapFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: windows_capture::capture::Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            flags: ctx.flags,
            next_due: None,
            sec: (Instant::now(), 0, 0),
            scratch: Vec::new(),
            side: Default::default(),
            dirty_works: false,
            last_sent: None,
            skipped: 0,
        })
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
        // 按时间表限帧（平均正好是所选帧率，见 pace_accept）
        let now = Instant::now();
        self.sec.2 += 1;
        if self.sec.0.elapsed() >= Duration::from_secs(1) {
            let secs = self.sec.0.elapsed().as_secs_f64();
            self.flags.stats.capture_fps.store((self.sec.1 as f64 / secs).round() as u32, Ordering::Relaxed);
            self.flags.stats.source_fps.store((self.sec.2 as f64 / secs).round() as u32, Ordering::Relaxed);
            self.flags.stats.static_skipped.store(self.skipped, Ordering::Relaxed);
            self.sec = (now, 0, 0);
            self.skipped = 0;
        }
        if !pace_accept(&mut self.next_due, now, self.flags.interval) {
            return Ok(());
        }
        self.sec.1 += 1;
        // 画面不动时少发：系统报告这一帧没有变化区域就跳过
        if self.flags.skip_static {
            let total = (frame.width() as i64 * frame.height() as i64).max(1);
            let dirty = frame.dirty_regions().ok().map(|r| {
                let area: i64 = r.iter().map(|x| x.width as i64 * x.height as i64).sum();
                (r.len(), (area * 1000 / total).min(1000) as u32)
            });
            self.flags.stats.dirty_last.store(dirty.map(|d| d.0 as i32).unwrap_or(-1), Ordering::Relaxed);
            self.flags.stats.dirty_area.store(dirty.map(|d| d.1).unwrap_or(0), Ordering::Relaxed);
            if dirty.is_some_and(|d| d.0 > 0) {
                self.dirty_works = true;
            }
            let since = self.last_sent.map(|t| t.elapsed());
            if can_skip_static(dirty, self.dirty_works, since, self.flags.force_key.load(Ordering::Relaxed)) {
                self.skipped += 1;
                return Ok(());
            }
        }
        self.last_sent = Some(Instant::now());
        // 显卡直通：只在显卡内部拷一份，画面不回内存
        if self.flags.gpu.load(Ordering::Relaxed) {
            match self.side.copy(frame.device(), frame.device_context(), frame.as_raw_texture()) {
                Ok(Some(g)) => {
                    return match self.flags.tx.try_send(CapFrame::Gpu(g)) {
                        Ok(()) | Err(TrySendError::Full(_)) => Ok(()),
                        Err(TrySendError::Disconnected(_)) => {
                            control.stop();
                            Ok(())
                        }
                    };
                }
                Ok(None) => return Ok(()), // 编码端还在处理上一帧：丢这一帧
                Err(_) => self.flags.gpu.store(false, Ordering::Relaxed),
            }
        }
        let fb = frame.buffer()?;
        let (w, h) = (fb.width() as usize, fb.height() as usize);
        let data = fb.as_nopadding_buffer(&mut self.scratch).to_vec();
        // 编码跟不上就丢帧，只保留最新的（低延迟）
        match self.flags.tx.try_send(CapFrame::Cpu(RawFrame { w, h, data })) {
            Ok(()) | Err(TrySendError::Full(_)) => Ok(()),
            Err(TrySendError::Disconnected(_)) => {
                control.stop();
                Ok(())
            }
        }
    }
}

/// 屏幕采集的参数（换「带不带鼠标」时用同样的参数重开）
struct CaptureCfg {
    monitor_index: usize,
    interval: Duration,
    flags: CapFlags,
}

type CapControl = windows_capture::capture::CaptureControl<CapHandler, Box<dyn std::error::Error + Send + Sync>>;

impl CaptureCfg {
    /// 开始采集；返回 (控制, 「变化区域」能不能用)。「画面变化小时少发」要系统报告变化区域，旧版 Windows 不支持时改回普通采集
    fn start(&self, with_cursor: bool) -> Result<(CapControl, bool), String> {
        let r = self.start_with(with_cursor, self.flags.skip_static);
        match r {
            Ok(c) => Ok((c, self.flags.skip_static)),
            Err(_) if self.flags.skip_static => self.start_with(with_cursor, false).map(|c| (c, false)),
            Err(e) => Err(e),
        }
    }

    fn start_with(&self, with_cursor: bool, skip: bool) -> Result<CapControl, String> {
        use windows_capture::capture::GraphicsCaptureApiHandler;
        use windows_capture::settings::*;
        let monitor = windows_capture::monitor::Monitor::from_index(self.monitor_index + 1)
            .or_else(|_| windows_capture::monitor::Monitor::primary())
            .map_err(|e| format!("找不到显示器：{e}"))?;
        let f = &self.flags;
        let settings = Settings::new(
            monitor,
            if with_cursor { CursorCaptureSettings::WithCursor } else { CursorCaptureSettings::WithoutCursor },
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Default,
            // 系统这边只挡住远超所选帧率的部分，具体取哪帧由 pace_accept 按时间表决定（两道门槛叠加会少帧）
            MinimumUpdateIntervalSettings::Custom(self.interval / 2),
            if skip { DirtyRegionSettings::ReportOnly } else { DirtyRegionSettings::Default },
            ColorFormat::Bgra8,
            CapFlags {
                tx: f.tx.clone(),
                skip_static: skip,
                force_key: f.force_key.clone(),
                gpu: f.gpu.clone(),
                stats: f.stats.clone(),
                stop: f.stop.clone(),
                interval: f.interval,
            },
        );
        CapHandler::start_free_threaded(settings).map_err(|e| e.to_string())
    }
}

/// 观看者都会自己画鼠标时换成不带鼠标的截屏（鼠标单独发，画面卡、少发帧时鼠标也顺）；有人不会画就换回带鼠标的
fn capture_manager(cap: CaptureCfg, mut ctl: CapControl, skip_ok: bool, stop: Arc<AtomicBool>, now: Arc<AtomicBool>, want: Arc<AtomicBool>) {
    let mut cap = cap;
    cap.flags.skip_static = skip_ok;
    loop {
        std::thread::sleep(Duration::from_millis(200));
        if stop.load(Ordering::Relaxed) {
            let _ = ctl.stop();
            return;
        }
        let w = want.load(Ordering::Relaxed);
        if w == now.load(Ordering::Relaxed) {
            continue;
        }
        let _ = ctl.stop();
        match cap.start_with(w, cap.flags.skip_static) {
            Ok(c) => {
                ctl = c;
                now.store(w, Ordering::Relaxed);
                cap.flags.stats.cursor_separate.store(!w, Ordering::Relaxed);
                cap.flags.force_key.store(true, Ordering::Relaxed);
            }
            Err(_) => match cap.start_with(!w, cap.flags.skip_static) {
                Ok(c) => {
                    ctl = c; // 换不过去就保持原样
                    want.store(!w, Ordering::Relaxed);
                }
                Err(_) => return,
            },
        }
    }
}

/// 共享的显示器在桌面上的位置（物理像素）
#[cfg(windows)]
fn monitor_rect(device: &str) -> Option<(i32, i32, i32, i32)> {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW};
    struct Ctx {
        name: String,
        out: Option<(i32, i32, i32, i32)>,
    }
    unsafe extern "system" fn cb(m: HMONITOR, _: HDC, _: *mut RECT, l: LPARAM) -> BOOL {
        let ctx = unsafe { &mut *(l.0 as *mut Ctx) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(m, &mut info as *mut _ as *mut MONITORINFO) }.as_bool() {
            let len = info.szDevice.iter().position(|c| *c == 0).unwrap_or(info.szDevice.len());
            if String::from_utf16_lossy(&info.szDevice[..len]) == ctx.name {
                let r = info.monitorInfo.rcMonitor;
                ctx.out = Some((r.left, r.top, r.right - r.left, r.bottom - r.top));
                return BOOL(0);
            }
        }
        BOOL(1)
    }
    let mut ctx = Ctx { name: device.to_string(), out: None };
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    ctx.out
}

/// 当前鼠标：(x, y 桌面物理像素, 形状, 显示着)
#[cfg(windows)]
fn cursor_now() -> Option<(i32, i32, u8, bool)> {
    use windows::Win32::UI::WindowsAndMessaging::*;
    let mut ci = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
    unsafe { GetCursorInfo(&mut ci) }.ok()?;
    let visible = ci.flags.0 & CURSOR_SHOWING.0 != 0;
    let shape = {
        let is = |id: windows::core::PCWSTR| unsafe { LoadCursorW(None, id) }.is_ok_and(|h| h.0 == ci.hCursor.0);
        if is(IDC_IBEAM) {
            udp::CURSOR_IBEAM
        } else if is(IDC_HAND) {
            udp::CURSOR_HAND
        } else if is(IDC_CROSS) {
            udp::CURSOR_CROSS
        } else if is(IDC_WAIT) || is(IDC_APPSTARTING) {
            udp::CURSOR_WAIT
        } else if is(IDC_SIZEWE) {
            udp::CURSOR_SIZE_WE
        } else if is(IDC_SIZENS) {
            udp::CURSOR_SIZE_NS
        } else if is(IDC_SIZEALL) {
            udp::CURSOR_SIZE_ALL
        } else {
            udp::CURSOR_ARROW
        }
    };
    Some((ci.ptScreenPos.x, ci.ptScreenPos.y, shape, visible))
}

/// 截屏不带鼠标时，每 16 ms 看一次鼠标，有变化就发给观看者（没变化也每 250 ms 发一次，观看端据此知道还在）
fn cursor_loop(device: String, state: Arc<VoiceState>, stop: Arc<AtomicBool>, in_video: Arc<AtomicBool>) {
    let mut rect = monitor_rect(&device);
    let (mut last, mut last_sent, mut rect_at) = (None, Instant::now(), Instant::now());
    let mut buf = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(16));
        if in_video.load(Ordering::Relaxed) {
            continue;
        }
        if rect_at.elapsed() > Duration::from_secs(5) {
            rect = monitor_rect(&device); // 改了分辨率 / 显示器排列
            rect_at = Instant::now();
        }
        let (Some((rx, ry, rw, rh)), Some((x, y, shape, vis)), Some(link)) = (rect, cursor_now(), state.current_link()) else { continue };
        if !link.has_watchers() {
            continue;
        }
        let inside = x >= rx && y >= ry && x < rx + rw && y < ry + rh;
        let nx = (((x - rx).clamp(0, rw) as f32 / rw.max(1) as f32) * 65535.0) as u16;
        let ny = (((y - ry).clamp(0, rh) as f32 / rh.max(1) as f32) * 65535.0) as u16;
        let now = (nx, ny, shape, vis && inside);
        if Some(now) == last && last_sent.elapsed() < Duration::from_millis(250) {
            continue;
        }
        last = Some(now);
        last_sent = Instant::now();
        let Some(e2e) = state.e2e.lock().unwrap().clone() else { continue };
        let blob = e2e.lock().unwrap().encrypt_stream(voc_proto::e2e::STREAM_CURSOR, &udp::cursor_payload(nx, ny, shape, now.3));
        link.send_stream(udp::CURSOR, &blob, &mut buf);
    }
}

pub struct Sharer {
    pub cfg: ShareConfig,
    state: Arc<VoiceState>,
    stop: Arc<AtomicBool>,
    force_key: Arc<AtomicBool>,
    /// 观看者请求关键帧的次数（丢包的信号，码率自适应用）
    key_requests: Arc<AtomicU32>,
    pub stats: Arc<ShareStats>,
}

impl Sharer {
    pub fn start(cfg: ShareConfig, state: Arc<VoiceState>) -> Result<Self, String> {
        let monitor = windows_capture::monitor::Monitor::from_index(cfg.monitor + 1)
            .or_else(|_| windows_capture::monitor::Monitor::primary())
            .map_err(|e| format!("找不到显示器：{e}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let force_key = Arc::new(AtomicBool::new(true));
        let stats = Arc::new(ShareStats::default());
        let (tx, rx) = mpsc::sync_channel::<CapFrame>(1);
        let gpu = Arc::new(AtomicBool::new(std::env::var_os("VOC_NO_GPU_PIPE").is_none()));
        let key_requests = Arc::new(AtomicU32::new(0));
        let interval = Duration::from_secs_f64(1.0 / cfg.fps as f64);
        let device = monitor.device_name().unwrap_or_default();
        let cap = CaptureCfg {
            monitor_index: cfg.monitor,
            interval,
            flags: CapFlags { tx, skip_static: cfg.skip_static, force_key: force_key.clone(), gpu: gpu.clone(), stats: stats.clone(), stop: stop.clone(), interval },
        };
        // 一开始把鼠标画进画面；观看者全都会自己画鼠标时换成不带鼠标的截屏、单独发鼠标位置
        let (ctl, skip_ok) = cap.start(true).map_err(|e| format!("屏幕采集启动失败：{e}"))?;
        let cursor_in_video = Arc::new(AtomicBool::new(true));
        let want_cursor_in_video = Arc::new(AtomicBool::new(true));
        {
            let (stop, now, want) = (stop.clone(), cursor_in_video.clone(), want_cursor_in_video.clone());
            std::thread::Builder::new()
                .name("capture-mgr".into())
                .spawn(move || capture_manager(cap, ctl, skip_ok, stop, now, want))
                .map_err(|e| e.to_string())?;
        }
        {
            let (stop, now, st) = (stop.clone(), cursor_in_video.clone(), state.clone());
            let _ = std::thread::Builder::new().name("cursor".into()).spawn(move || cursor_loop(device, st, stop, now));
        }

        // 画面码率计入 P2P 上行预算
        if let Some(l) = state.current_link() {
            l.set_video_bps(cfg.bitrate());
        }
        crate::voice::SHARE_P2P_ONLY.store(cfg.p2p_only, Ordering::Relaxed);
        let (st, fk, sp, stx, c2, kr) = (state.clone(), force_key.clone(), stop.clone(), stats.clone(), cfg.clone(), key_requests.clone());
        let wc = want_cursor_in_video.clone();
        std::thread::Builder::new()
            .name("video-encode".into())
            .spawn(move || encode_loop(c2, rx, st, fk, sp, stx, gpu, kr, wc))
            .map_err(|e| e.to_string())?;
        Ok(Self { cfg, state, stop, force_key, key_requests, stats })
    }

    pub fn request_keyframe(&self) {
        self.force_key.store(true, Ordering::Relaxed);
        self.key_requests.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for Sharer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(l) = self.state.current_link() {
            l.set_video_bps(0);
        }
        crate::voice::SHARE_P2P_ONLY.store(false, Ordering::Relaxed);
    }
}

/// Reed-Solomon 冗余比例（校验片 / 数据片），按观看者回报的原始丢包率（千分之几，最近 5 秒最大值）：
/// 不到 0.5% 不加；30% 丢包时 1.2 倍（一组 k 片数据配 1.2k 片校验，收到任意 k 片就能还原）
pub fn fec_ratio(loss_permille: u16) -> f32 {
    match loss_permille {
        0..5 => 0.0,
        5..30 => 0.15,
        30..80 => 0.35,
        80..150 => 0.6,
        150..250 => 0.9,
        // 0.6：丢包更高时冗余继续往上加（以前最多 120%，丢一半时整组常常救不回）。拥塞时不加（见 congestion_cap）
        250..350 => 1.2,
        350..450 => 1.5,
        _ => 2.0,
    }
}

/// 关键帧的冗余更高（丢了要等下一张关键帧，代价大）：至少 10%，最多 1.5 倍
pub fn key_fec_ratio(r: f32) -> f32 {
    (r * 1.5).clamp(0.1, voc_proto::udp::RS_MAX_RATIO)
}

/// 长期参考帧（只用于原画层）：两个槽轮流存，观看者解码成功后确认。
/// 有人丢了帧就发一帧只参考「所有观看者都确认过的那个槽」的恢复帧，代替整张关键帧
struct LtrState {
    epoch: u32,
    /// 两个槽里各存的是哪一帧
    slots: [Option<u32>; 2],
    /// 每个观看者确认过的长期参考帧（只留最近几个）
    acks: HashMap<u32, Vec<u32>>,
    last_mark: Instant,
    last_recover: Instant,
    /// 统计：发了几帧恢复帧
    recovers: u32,
}

/// 多久存一个新的长期参考帧
const LTR_MARK_EVERY: Duration = Duration::from_secs(1);
/// 恢复帧最快多久发一次
const LTR_RECOVER_GAP: Duration = Duration::from_millis(250);

impl LtrState {
    fn new() -> Self {
        let past = Instant::now() - Duration::from_secs(10);
        Self { epoch: u32::MAX, slots: [None; 2], acks: HashMap::new(), last_mark: past, last_recover: past, recovers: 0 }
    }

    fn reset(&mut self, epoch: u32) {
        self.epoch = epoch;
        self.slots = [None; 2];
        self.acks.clear();
    }

    fn ack(&mut self, by: u32, frame: u32) {
        let v = self.acks.entry(by).or_default();
        if !v.contains(&frame) {
            v.push(frame);
            if v.len() > 8 {
                v.remove(0);
            }
        }
    }

    /// 能用长期参考帧的观看者（确认过的）
    fn capable(&self, ids: &[u32]) -> Vec<u32> {
        ids.iter().copied().filter(|i| self.acks.contains_key(i)).collect()
    }

    /// 所有能用的观看者都确认过的槽（取最新的那个）
    fn common_slot(&self, cap: &[u32]) -> Option<u8> {
        if cap.is_empty() {
            return None;
        }
        (0..2u8)
            .filter_map(|s| self.slots[s as usize].map(|f| (s, f)))
            .filter(|(_, f)| cap.iter().all(|i| self.acks.get(i).is_some_and(|v| v.contains(f))))
            .max_by_key(|(_, f)| *f)
            .map(|(s, _)| s)
    }

    /// 存新长期参考帧用哪个槽：不能覆盖大家都确认过的那个
    fn free_slot(&self, cap: &[u32]) -> u8 {
        match self.common_slot(cap) {
            Some(s) => 1 - s,
            None => match self.slots {
                [None, _] => 0,
                [_, None] => 1,
                [Some(a), Some(b)] => u8::from(b < a),
            },
        }
    }
}

/// 观看者原始丢包率 ≥ 这个值（千分之几）且是直连的：画面直连和经服务器两条路同时发
pub const DUAL_PATH_LOSS: u16 = 100;
/// 观看者请求的关键帧，最快这么久发一张（新观看者加入不受限）
const KEY_REQUEST_GAP: Duration = Duration::from_millis(500);

/// 码率自适应：每秒看一次。观看者频繁要关键帧（≥2 次 / 秒）或匀速队列丢包 → 降到 70%（最低 30%）；
/// 连续 4 秒正常 → 每秒回升 10%，直到设定值
/// 拥塞时冗余比例最多这么多
const CONGESTED_FEC_CAP: f32 = 0.35;

pub fn adapt_rate(scale: f32, calm_secs: &mut u32, key_requests: u32, pace_drops: u32) -> f32 {
    if key_requests >= 2 || pace_drops > 0 {
        *calm_secs = 0;
        (scale * 0.7).max(0.3)
    } else {
        *calm_secs += 1;
        if *calm_secs >= 4 {
            (scale * 1.1).min(1.0)
        } else {
            scale
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn encode_loop(
    cfg: ShareConfig,
    rx: Receiver<CapFrame>,
    state: Arc<VoiceState>,
    force_key: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    stats: Arc<ShareStats>,
    gpu_flag: Arc<AtomicBool>,
    key_requests: Arc<AtomicU32>,
    want_cursor_in_video: Arc<AtomicBool>,
) {
    let pref = cfg.encoder.clone();
    let (main_bps, low_bps) = (cfg.bitrate(), cfg.low_layer().bitrate());
    let (cfg_auto_res, cfg_height, cfg_delay_bwe) = (cfg.auto_res, cfg.height, cfg.delay_bwe);
    // 冗余校验包：按观看者请求补发的比例（≈丢包率）决定每几片加一个校验包，0 = 不加
    let (mut fec_main, mut fec_low) = (0.0f32, 0.0f32);
    let mut ltr = LtrState::new();
    let mut low_enc = VideoEncoder::new(cfg.low_layer());
    let mut enc = VideoEncoder::new(cfg);
    let mut low_frame_no = 0u32;
    let mut gpu_broken = std::env::var_os("VOC_NO_GPU_PIPE").is_some();
    let mut cap_vendor: Option<u32> = None;
    let (mut scale, mut calm) = (1.0f32, 0u32);
    stats.rate_pct.store(100, Ordering::Relaxed);
    let mut hw_broken = false;
    let mut frame_no = 0u32;
    let mut buf = Vec::new();
    let (mut sec_start, mut sec_frames, mut sec_bytes) = (Instant::now(), 0u32, 0usize);
    let (mut sec_enc, mut sec_skip, mut sec_all) = (Duration::ZERO, 0u32, 0u32);
    let mut last_key = Instant::now();
    let mut last_watchers = 0u32;
    while !stop.load(Ordering::Relaxed) {
        let Ok(frame) = rx.recv_timeout(Duration::from_millis(300)) else { continue };
        // 不处理的显卡画面一定要把共享纹理还给采集端，否则采集端再也拿不到它，之后的画面全被丢掉
        //（以前「先开共享、过一会儿才有人看」时画面永远是「正在等待画面」就是这个原因）
        let skip = |enc: &mut VideoEncoder, frame: &CapFrame| {
            if let CapFrame::Gpu(g) = frame {
                enc.give_back(g);
            }
        };
        let Some(link) = state.current_link() else {
            skip(&mut enc, &frame);
            continue;
        };
        let ver = link.watchers_version();
        if !link.has_watchers() {
            last_watchers = ver;
            skip(&mut enc, &frame);
            continue; // 没人看就不编码，省 CPU 和流量
        }
        let mut new_watcher = false;
        if ver != last_watchers {
            last_watchers = ver;
            new_watcher = true; // 有新观看者，马上给关键帧
        }
        // 观看者要的关键帧最快 0.5 秒一张（高延迟时请求会一直来，连发大关键帧只会把线路挤满）；
        // 每 10 秒兜底一个关键帧（编码器开了逐行刷新时 60 秒）
        let periodic = Duration::from_secs(if enc.intra_refresh() { 60 } else { 10 });
        let requested = force_key.load(Ordering::Relaxed) && last_key.elapsed() >= KEY_REQUEST_GAP;
        if new_watcher || requested {
            force_key.store(false, Ordering::Relaxed);
        }
        let key = new_watcher || requested || last_key.elapsed() > periodic;
        // 两层：原画给大多数人，360P 低画质层给选了「流畅」的人；哪层没人看就不编码
        let (main_ids, low_ids) = (link.layer_watchers(false), link.layer_watchers(true));
        let (want_main, want_low) = (!main_ids.is_empty(), !low_ids.is_empty());
        stats.low_watchers.store(low_ids.len() as u32, Ordering::Relaxed);
        // 长期参考帧（原画层、编码器支持时）：关键帧存进槽 0；定期存新的；有人丢帧就发恢复帧
        if want_main && enc.ltr_supported() {
            if enc.ltr_epoch() != ltr.epoch {
                ltr.reset(enc.ltr_epoch());
            }
            for (by, f) in link.take_ltr_acks() {
                ltr.ack(by, f);
            }
            let cap = ltr.capable(&main_ids);
            let mut tag = LtrTag::default();
            if key {
                tag.mark = Some(0);
            } else if link.take_ltr_recover() {
                match ltr.common_slot(&cap) {
                    Some(s) if ltr.last_recover.elapsed() >= LTR_RECOVER_GAP => {
                        tag.use_slot = Some(s);
                        ltr.last_recover = Instant::now();
                        ltr.recovers += 1;
                    }
                    Some(_) => link.keep_ltr_recover(),
                    None => force_key.store(true, Ordering::Relaxed), // 没有大家都确认过的：只能发关键帧
                }
            }
            if tag.use_slot.is_none() && tag.mark.is_none() && ltr.last_mark.elapsed() >= LTR_MARK_EVERY {
                tag.mark = Some(ltr.free_slot(&cap));
            }
            if tag.mark.is_some() {
                ltr.last_mark = Instant::now();
            }
            enc.set_ltr(tag);
            stats.ltr_recovers.store(ltr.recovers, Ordering::Relaxed);
        } else {
            // 不用长期参考帧时，恢复请求当关键帧请求
            if link.take_ltr_recover() {
                force_key.store(true, Ordering::Relaxed);
            }
            let _ = link.take_ltr_acks();
        }
        // 选格式：这一层所有观看者都能解的里面最省带宽的；硬件编码器出过错就退回软件编码
        let caps_of = |ids: &[u32]| -> Vec<Vec<String>> {
            let all = state.caps.lock().unwrap();
            ids.iter().map(|id| all.get(id).cloned().unwrap_or_default()).collect()
        };
        let pref_now = if hw_broken { "software" } else { pref.as_str() };
        let plan = plan_encoder(pref_now, &common_decoders(&caps_of(&main_ids)), hw_encoders(), cap_vendor);
        // 两层都有人看时，低画质层尽量和原画用同一种格式（同一块显卡上混用 H.264 和 AV1 两个硬件编码器时，实测低画质层解不出来）
        let low_allowed = common_decoders(&caps_of(&low_ids));
        let plan_low = if want_main && low_allowed.contains(&plan.0) {
            plan.clone()
        } else {
            plan_encoder(pref_now, &low_allowed, hw_encoders(), cap_vendor)
        };
        let used: Vec<&(Codec, Option<HwEncoderInfo>)> =
            [(want_main, &plan), (want_low, &plan_low)].into_iter().filter(|x| x.0).map(|x| x.1).collect();
        // 显卡直通的条件：用硬件编码器，且编码器和画面在同一块显卡上（笔记本核显出画面、独显编码时不行）
        let same_gpu = |vendor: Option<u32>| used.iter().all(|p| p.1.as_ref().is_some_and(|i| vendor.is_none_or(|v| v == i.vendor)));
        let gpu_ok = !gpu_broken && same_gpu(cap_vendor);
        gpu_flag.store(gpu_ok, Ordering::Relaxed);
        let te = Instant::now();
        let r: Result<Vec<(bool, EncodedFrame)>, String> = match frame {
            CapFrame::Gpu(g) => {
                if cap_vendor.is_none() {
                    cap_vendor = crate::mf::gpu::vendor_of(g.luid);
                    if cap_vendor.is_none() || !same_gpu(cap_vendor) {
                        gpu_flag.store(false, Ordering::Relaxed);
                        enc.give_back(&g);
                        continue; // 换成拷回内存，下一帧走 CPU 转换
                    }
                }
                if !gpu_ok {
                    enc.give_back(&g);
                    continue;
                }
                // 同一帧转两次时，第一次转完不把画面还给采集端（keep）
                let mut out = Vec::new();
                let mut err = None;
                if want_main {
                    match enc.encode_gpu(&g, &plan, key, want_low) {
                        Ok(f) => out.extend(f.into_iter().map(|f| (false, f))),
                        Err(e) => err = Some(e),
                    }
                }
                if want_low {
                    match low_enc.encode_gpu(&g, &plan_low, key, false) {
                        Ok(f) => out.extend(f.into_iter().map(|f| (true, f))),
                        Err(e) => err = err.or(Some(e)),
                    }
                }
                match err {
                    None => {
                        *stats.pipeline.lock().unwrap() = "显卡直通".into();
                        Ok(out)
                    }
                    Some(e) => {
                        // 显卡直通不行：这次共享改用 CPU 转换（硬件编码照用）
                        *stats.error.lock().unwrap() = Some(format!("显卡直通不可用（{e}），已改用 CPU 转换"));
                        crate::applog::log(format!("屏幕共享：显卡直通不可用（{e}），改用 CPU 转换"));
                        gpu_broken = true;
                        gpu_flag.store(false, Ordering::Relaxed);
                        force_key.store(true, Ordering::Relaxed);
                        enc.give_back(&g); // 可能停在两层之间，确保还给采集端
                        continue;
                    }
                }
            }
            CapFrame::Cpu(raw) => {
                *stats.pipeline.lock().unwrap() = "CPU 转换".into();
                let mut out = Vec::new();
                let mut err = None;
                if want_main {
                    match enc.encode(&raw.data, raw.w, raw.h, &plan, key) {
                        Ok(f) => out.extend(f.into_iter().map(|f| (false, f))),
                        Err(e) => err = Some(e),
                    }
                }
                if want_low {
                    match low_enc.encode(&raw.data, raw.w, raw.h, &plan_low, key) {
                        Ok(f) => out.extend(f.into_iter().map(|f| (true, f))),
                        Err(e) => err = err.or(Some(e)),
                    }
                }
                match err {
                    None => Ok(out),
                    Some(e) => Err(e),
                }
            }
        };
        sec_enc += te.elapsed();
        sec_all += 1;
        let frames = match r {
            Ok(f) => f,
            Err(e) => {
                *stats.error.lock().unwrap() = Some(format!("{e}，已改用 CPU 编码"));
                crate::applog::log(format!("屏幕共享：{e}，改用 CPU 编码"));
                hw_broken = true;
                force_key.store(true, Ordering::Relaxed);
                continue;
            }
        };
        if frames.is_empty() {
            sec_skip += 1;
        }
        let desc = match (want_main, want_low) {
            (true, true) => enc.describe().map(|d| format!("{d} + 360P 低画质层")),
            (false, true) => low_enc.describe().map(|d| format!("{d}（只有低画质层有人看）")),
            _ => enc.describe(),
        };
        if let Some(d) = desc {
            *stats.encoder.lock().unwrap() = if enc.intra_refresh() { format!("{d} · 逐行刷新") } else { d };
        }
        let Some(e2e) = state.e2e.lock().unwrap().clone() else { continue };
        for (low, ef) in frames {
            if ef.key {
                last_key = Instant::now();
            }
            let sealed = e2e.lock().unwrap().encrypt_video(&with_dims(&ef));
            if sealed.len() > udp::MAX_FRAGS as usize * udp::VIDEO_FRAG {
                // 接收端不收超过上限的帧，发了也白发（正常码率下不会出现）
                crate::applog::log(format!("屏幕共享：一帧 {} 字节超过上限，丢弃", sealed.len()));
                continue;
            }
            // 两层各自编帧号（接收端按帧号判断丢帧）
            let mut flags = udp::video_flags(ef.key, ef.codec.id());
            if !low {
                if ef.key {
                    ltr.slots = [None; 2];
                }
                if let Some(s) = ef.ltr_mark.filter(|s| *s < 2) {
                    ltr.slots[s as usize] = Some(frame_no);
                    flags |= udp::VIDEO_LTR;
                }
                if ef.recover {
                    flags |= udp::VIDEO_RECOVER;
                }
            }
            let no = if low {
                flags |= udp::VIDEO_LOW;
                &mut low_frame_no
            } else {
                &mut frame_no
            };
            let mut frags = udp::video_fragments(*no, flags, &sealed);
            let r = if low { fec_low } else { fec_main };
            let r = if ef.key { key_fec_ratio(r) } else { r };
            if r > 0.0 {
                frags.extend(udp::rs_parities(*no, flags, &sealed, r));
            }
            *no = no.wrapping_add(1);
            link.send_video(&frags, &mut buf, low);
            sec_bytes += sealed.len();
            // 帧率、分辨率按主要的那一层统计
            if low != want_main {
                sec_frames += 1;
                stats.width.store(ef.width as u32, Ordering::Relaxed);
                stats.height.store(ef.height as u32, Ordering::Relaxed);
            }
        }
        if sec_start.elapsed() >= Duration::from_secs(1) {
            // 码率自适应：默认只调「流畅」层，「原画」层固定按设定码率（选原画的人要的就是清楚，网络扛不住的自己切流畅）。
            // 用户打开「网络差时自动降分辨率」后，原画层也跟着调：码率按比例降，分辨率按档位降
            let (sent, nacked) = link.take_video_loss();
            // 补发比例 ≥ 5% 也算拥堵
            let lossy = sent >= 50 && nacked as f32 / sent as f32 >= 0.05;
            // 延迟在涨（排队）：选了「按延迟提前降码率」才据此降码率（还没丢包就先降）
            let congested = link.congested();
            stats.congested.store(congested, Ordering::Relaxed);
            let early = cfg_delay_bwe && congested;
            let keys = key_requests.swap(0, Ordering::Relaxed) + if lossy || early { 2 } else { 0 };
            let new_scale = adapt_rate(scale, &mut calm, keys, link.take_pace_drops());
            if (new_scale - scale).abs() > 0.01 {
                scale = new_scale;
                low_enc.set_rate_scale(scale);
                if cfg_auto_res || cfg_delay_bwe {
                    enc.set_rate_scale(scale);
                }
            }
            if cfg_auto_res {
                let h = res_for_scale(cfg_height, scale);
                enc.set_height(h);
                stats.auto_height.store(if h < cfg_height { h } else { 0 }, Ordering::Relaxed);
            }
            let main_now = if cfg_auto_res || cfg_delay_bwe { (main_bps as f32 * scale) as u32 } else { main_bps };
            link.set_video_bps(if want_main { main_now } else { 0 } + if want_low { (low_bps as f32 * scale) as u32 } else { 0 });
            // 观看者都会自己画鼠标：截屏不带鼠标、单独发；有人不会（旧版本）就画进画面
            want_cursor_in_video.store(!link.all_watchers_draw_cursor(), Ordering::Relaxed);
            // 冗余比例按各层观看者回报的丢包率；丢包很高的直连观看者两条路同时发
            let (loss_main, loss_low) = (link.layer_loss(false), link.layer_loss(true));
            (fec_main, fec_low) = (fec_ratio(loss_main), fec_ratio(loss_low));
            // 拥塞时不加冗余（冗余也占带宽，越加越堵；第 40-2 项）：最多保持中等冗余，先靠降码率 / 控速
            if congested {
                (fec_main, fec_low) = (fec_main.min(CONGESTED_FEC_CAP), fec_low.min(CONGESTED_FEC_CAP));
            }
            stats.fec_pct.store((fec_main * 100.0).round() as u32, Ordering::Relaxed);
            link.set_fec_pct((fec_main.max(fec_low) * 100.0).round() as u32);
            stats.viewer_loss.store(loss_main.max(loss_low) as u32, Ordering::Relaxed);
            stats.dual_viewers.store(link.update_dual_video(DUAL_PATH_LOSS) as u32, Ordering::Relaxed);
            stats.resent_total.fetch_add(nacked, Ordering::Relaxed);
            stats.rate_pct.store((scale * 100.0).round() as u32, Ordering::Relaxed);
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
    /// 共享者单独发来的鼠标：(x, y 0–1, 形状, 显示着, 收到时间)
    pub cursor: Mutex<Option<(f32, f32, u8, bool, Instant)>>,
    /// 播放方式（第 36 项）：true = 流畅优先（默认）：帧成串到达时按原来的间隔匀速显示，缓冲自动伸缩（网络稳时一两帧）；
    /// false = 延迟优先：到了就显示，积压时直接跳到最新一帧（0.5.x 的做法）
    pub smooth: AtomicBool,
}

/// 流畅优先时最多攒几帧匀速放（再多就跳过旧的，延迟不会越积越大）
const SMOOTH_MAX_BACKLOG: usize = 4;

struct Stream {
    /// 帧重组缓冲（乱序、补发、冗余校验包都在这里处理）
    asm: crate::vrecv::Assembler,
    need_key: bool,
    last_req: Option<Instant>,
    /// 请求过几次关键帧（统计）
    key_requests: u32,
    /// 确认过共享者的长期参考帧（之后丢了帧可以要恢复帧，不用等关键帧）；这一轮已经要过几次恢复帧
    ltr_ok: bool,
    recover_reqs: u32,
    /// 统计：靠恢复帧接上的次数
    recovered: u32,
    /// 上次回报丢包率的时间
    last_report: Instant,
    dec_tx: SyncSender<(u8, Vec<u8>)>,
    view: Arc<View>,
}

/// 收包 / 定时检查后要做的事
#[derive(Default, Debug, PartialEq)]
pub struct RxActions {
    /// 要向这些共享者请求关键帧
    pub keyframes: Vec<u32>,
    /// 要请求补发：(共享者, 画质层, 帧号, 缺的分片号)
    pub nacks: Vec<(u32, u8, u32, Vec<u16>)>,
    /// 回报给共享者的原始丢包率：(共享者, 千分之几, 往返估计 ms)
    pub reports: Vec<(u32, u16, u32)>,
    /// 解码成功的长期参考帧：(共享者, 帧号)
    pub ltr_acks: Vec<(u32, u32)>,
    /// 请求恢复帧（代替关键帧）的共享者
    pub recovers: Vec<u32>,
}

/// 处理某个共享者的画面时需要的外部信息
#[derive(Default, Debug, Clone, Copy)]
pub struct SenderInfo {
    /// 他支持补发
    pub rtx: bool,
    /// 线路往返时间参考（ms，0 = 不知道）
    pub rtt_hint: u32,
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
        view.smooth.store(SMOOTH_PLAYBACK.load(Ordering::Relaxed), Ordering::Relaxed);
        let (tx, rx) = mpsc::sync_channel::<(u8, Vec<u8>)>(64);
        let (v, ctx) = (view.clone(), self.ctx.lock().unwrap().clone());
        std::thread::Builder::new().name("video-decode".into()).spawn(move || decode_loop(rx, v, ctx)).ok()?;
        s.insert(
            from,
            Stream {
                asm: Default::default(),
                need_key: true,
                last_req: None,
                key_requests: 0,
                ltr_ok: false,
                recover_reqs: 0,
                recovered: 0,
                last_report: Instant::now(),
                dec_tx: tx,
                view: view.clone(),
            },
        );
        Some(view)
    }

    pub fn clear(&self) {
        self.streams.lock().unwrap().clear();
    }

    /// 收到一个视频分片（服务器中转 / P2P / TCP 隧道）。rtx(共享者) = 他支持补发
    pub fn on_packet(
        &self,
        payload: &[u8],
        info: impl Fn(u32) -> SenderInfo,
        mut decrypt: impl FnMut(u32, &[u8]) -> Option<Vec<u8>>,
    ) -> RxActions {
        let mut acts = RxActions::default();
        let Some((from, f, data)) = udp::read_server_video(payload) else { return acts };
        let mut streams = self.streams.lock().unwrap();
        let Some(s) = streams.get_mut(&from) else { return acts }; // 没在看就丢弃
        let now = Instant::now();
        if s.asm.add(f, data, now) {
            s.need_key = true;
        }
        s.pump(from, now, info(from), &mut decrypt, &mut acts);
        acts
    }

    /// 定时检查（没有新包时也要发现尾巴丢片、超时放弃）
    pub fn tick(&self, info: impl Fn(u32) -> SenderInfo, mut decrypt: impl FnMut(u32, &[u8]) -> Option<Vec<u8>>) -> RxActions {
        let mut acts = RxActions::default();
        let now = Instant::now();
        for (from, s) in self.streams.lock().unwrap().iter_mut() {
            s.pump(*from, now, info(*from), &mut decrypt, &mut acts);
        }
        acts
    }

    /// 统计：(校验包恢复的片数, 补发收回的片数)
    /// 收某人画面的统计：(累计统计, 请求关键帧次数, 当前往返估计 ms)
    pub fn rx_stats(&self, from: u32) -> Option<(crate::vrecv::RxStats, u32, u32)> {
        self.streams.lock().unwrap().get(&from).map(|s| (s.asm.stats, s.key_requests, s.asm.rtt() as u32))
    }

    /// 收到共享者的鼠标位置
    pub fn set_cursor(&self, from: u32, x: u16, y: u16, shape: u8, visible: bool) {
        if let Some(s) = self.streams.lock().unwrap().get(&from) {
            *s.view.cursor.lock().unwrap() = Some((x as f32 / 65535.0, y as f32 / 65535.0, shape, visible, Instant::now()));
            if let Some(c) = &*self.ctx.lock().unwrap() {
                c.request_repaint();
            }
        }
    }

    /// 靠恢复帧（长期参考帧）接上的次数
    pub fn recovered_count(&self, from: u32) -> u32 {
        self.streams.lock().unwrap().get(&from).map(|s| s.recovered).unwrap_or(0)
    }
}

impl Stream {
    fn pump(
        &mut self,
        from: u32,
        now: Instant,
        info: SenderInfo,
        decrypt: &mut impl FnMut(u32, &[u8]) -> Option<Vec<u8>>,
        acts: &mut RxActions,
    ) {
        let rtx = info.rtx;
        self.asm.set_rtt_hint(info.rtt_hint);
        if self.view.decode_failed.swap(false, Ordering::Relaxed) {
            self.need_key = true;
        }
        let before_layer = self.asm.layer();
        // 断档后能接着解的：关键帧；确认过长期参考帧的话，恢复帧也行
        let restart = if self.need_key { VIDEO_KEY | if self.ltr_ok { udp::VIDEO_RECOVER } else { 0 } } else { 0 };
        let out = self.asm.poll(now, restart, rtx);
        if out.gap {
            self.need_key = true; // 中间丢了帧，之后的非关键帧解不对
        }
        if self.asm.layer() != before_layer {
            self.ltr_ok = false; // 换了画质层：长期参考帧是另一层的
        }
        for (frame_id, flags, blob) in out.frames {
            let key = flags & VIDEO_KEY != 0;
            let recover = flags & udp::VIDEO_RECOVER != 0 && self.ltr_ok;
            match decrypt(from, &blob) {
                Some(bits) if key || recover || !self.need_key => {
                    if (key || recover) && self.need_key {
                        self.need_key = false;
                        self.recover_reqs = 0;
                        if recover && !key {
                            self.recovered += 1;
                        }
                    }
                    if self.dec_tx.try_send((udp::flags_codec(flags), bits)).is_err() {
                        self.need_key = true; // 解码跟不上，丢帧后要从关键帧重新开始
                    } else if flags & udp::VIDEO_LTR != 0 {
                        // 长期参考帧解了：告诉共享者，以后丢帧可以从它恢复
                        self.ltr_ok = true;
                        acts.ltr_acks.push((from, frame_id));
                    }
                }
                Some(_) => {} // 等关键帧 / 恢复帧
                None => self.need_key = true,
            }
        }
        let layer = self.asm.layer();
        acts.nacks.extend(out.nacks.into_iter().map(|(frame, idxs)| (from, layer, frame, idxs)));
        // 需要关键帧时按往返时间限速请求（至少 300 ms 一次；高延迟时更慢，免得共享者连发大关键帧把线路挤满）
        let gap = Duration::from_millis((self.asm.rtt() * 2.0 + 100.0).max(300.0) as u64);
        if self.need_key && self.last_req.is_none_or(|t| now.duration_since(t) > gap) {
            self.last_req = Some(now);
            // 确认过长期参考帧：先要恢复帧（只有关键帧的零头大小）；要了 3 次还没接上再要关键帧
            if self.ltr_ok && self.recover_reqs < 3 {
                self.recover_reqs += 1;
                acts.recovers.push(from);
            } else {
                self.key_requests += 1;
                acts.keyframes.push(from);
            }
        }
        // 每秒回报一次原始丢包率（共享者据此调冗余）
        if now.duration_since(self.last_report) >= Duration::from_secs(1) {
            self.last_report = now;
            if let Some(loss) = self.asm.take_loss() {
                acts.reports.push((from, loss, self.asm.rtt() as u32));
            }
        }
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
    // 硬解连续出错这么多次：这一路改用软解（有的显卡驱动对某些码流解不了）
    const HW_FAIL_LIMIT: u32 = 5;
    let (mut hw_fails, mut hw_broken) = (0u32, false);
    // 匀速播放：帧间隔的估计（按收到的节奏）、下一帧最早什么时候显示
    let mut interval = Duration::from_millis(33);
    let mut last_arrival: Option<Instant> = None;
    let mut next_present: Option<Instant> = None;
    while let Ok(first) = rx.recv() {
        // 必须每帧都解码（P 帧依赖前一帧）。延迟优先：积压时只把最新一帧转成 RGBA 显示；
        // 流畅优先：最后几帧按间隔匀速显示，更早的跳过
        let mut batch = vec![first];
        batch.extend(rx.try_iter());
        let last = batch.len() - 1;
        let now = Instant::now();
        if let Some(t) = last_arrival {
            let dt = now.duration_since(t).as_secs_f32() / batch.len() as f32;
            if dt < 0.2 {
                let est = interval.as_secs_f32() * 0.9 + dt * 0.1;
                interval = Duration::from_secs_f32(est.clamp(0.005, 0.1));
            }
        }
        last_arrival = Some(now);
        let smooth = view.smooth.load(Ordering::Relaxed);
        for (i, (cid, bits)) in batch.iter().enumerate() {
            let codec = Codec::from_id(*cid);
            let Some((w0, h0, bits)) = split_dims(codec, bits) else { continue };
            let hw = HW_DECODE.load(Ordering::Relaxed) && !hw_broken;
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
            let show = if smooth { i + SMOOTH_MAX_BACKLOG > last } else { i == last };
            let mut publish = |w: usize, h: usize, fill: &mut dyn FnMut(&mut Vec<u8>)| {
                let mut rgba = std::mem::take(&mut spare);
                rgba.resize(w * h * 4, 0);
                fill(&mut rgba);
                if smooth {
                    // 按估计的帧间隔匀速放（略快一点，缓冲会慢慢消掉、延迟不会越积越多）
                    if let Some(p) = next_present {
                        let now = Instant::now();
                        if p > now {
                            std::thread::sleep(p - now);
                        }
                    }
                    next_present = Some(Instant::now() + interval.mul_f32(0.85));
                }
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
                        if !frames.is_empty() {
                            hw_fails = 0;
                        }
                        if let (true, Some(f)) = (show, frames.last()) {
                            publish(f.width, f.height, &mut |buf| nv12_to_rgba(&f.data, f.width, f.height, buf));
                        }
                    }
                    Err(_) => {
                        view.decode_failed.store(true, Ordering::Relaxed);
                        if m.hardware {
                            hw_fails += 1;
                            if hw_fails >= HW_FAIL_LIMIT && !hw_broken {
                                hw_broken = true; // 下一帧起换成软解
                                crate::applog::log(format!("观看：{} 硬解连续出错，改用软解", codec.label()));
                            }
                        }
                    }
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
                key_requests += rx.on_packet(&udp::server_video(7, &f), |_| SenderInfo::default(), |from, b| rk.decrypt_video(from, b)).keyframes.len();
            }
        }
        std::thread::sleep(Duration::from_millis(500));
        let fr = view.frame.lock().unwrap();
        let fr = fr.as_ref().expect("decoded at least one frame");
        assert_eq!((fr.width, fr.height), (854, 480));
        assert_eq!(fr.rgba.len(), 854 * 480 * 4);
        assert!(key_requests >= 1, "packet loss must trigger a keyframe request");
        // 没在看的人的包直接丢弃
        assert_eq!(rx.on_packet(&udp::server_video(99, &udp::video_fragments(0, VIDEO_KEY, b"x")[0]), |_| SenderInfo { rtx: true, rtt_hint: 0 }, |_, _| None), RxActions::default());
    }

    #[test]
    fn layer_switch_restarts_from_keyframe() {
        let rx = VideoRx::default();
        let _view = rx.watch(7, true).unwrap();
        let need_key = || rx.streams.lock().unwrap()[&7].need_key;
        let feed = |frame: u32, flags: u8| {
            let f = udp::video_fragments(frame, flags, b"x");
            rx.on_packet(&udp::server_video(7, &f[0]), |_| SenderInfo { rtx: true, rtt_hint: 0 }, |_, b| Some(b.to_vec()));
        };
        // 原画：关键帧后连续的 P 帧不用再要关键帧
        feed(100, udp::VIDEO_KEY);
        feed(101, 0);
        assert!(!need_key());
        // 换到低画质层：帧号从 0 开始，必须等它的关键帧
        feed(0, udp::VIDEO_LOW);
        assert!(need_key());
        feed(1, udp::VIDEO_LOW | udp::VIDEO_KEY);
        feed(2, udp::VIDEO_LOW);
        assert!(!need_key());
    }

    /// 支持补发时：缺片先请求补发，不请求关键帧；补到后接着播
    #[test]
    fn loss_is_nacked_instead_of_keyframe() {
        let rx = VideoRx::default();
        let _view = rx.watch(7, true).unwrap();
        let data = vec![5u8; 4000];
        let f0 = udp::video_fragments(0, udp::VIDEO_KEY, &data);
        let f1 = udp::video_fragments(1, 0, &data);
        for f in &f0 {
            rx.on_packet(&udp::server_video(7, f), |_| SenderInfo { rtx: true, rtt_hint: 0 }, |_, b| Some(b.to_vec())); // 开头没关键帧时会要一次，正常
        }
        let mut acts = RxActions::default();
        for (i, f) in f1.iter().enumerate() {
            if i != 1 {
                let a = rx.on_packet(&udp::server_video(7, f), |_| SenderInfo { rtx: true, rtt_hint: 0 }, |_, b| Some(b.to_vec()));
                acts.keyframes.extend(a.keyframes);
                acts.nacks.extend(a.nacks);
            }
        }
        assert!(acts.keyframes.is_empty(), "{acts:?}");
        std::thread::sleep(Duration::from_millis(40));
        let a = rx.tick(|_| SenderInfo { rtx: true, rtt_hint: 0 }, |_, b| Some(b.to_vec()));
        assert_eq!(a.nacks, vec![(7, 0, 1, vec![1])]);
        assert!(a.keyframes.is_empty());
        let a = rx.on_packet(&udp::server_video(7, &f1[1]), |_| SenderInfo { rtx: true, rtt_hint: 0 }, |_, b| Some(b.to_vec()));
        assert_eq!(a, RxActions::default());
        assert!(!rx.streams.lock().unwrap()[&7].need_key);
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
    fn bitrate_adapts_to_congestion() {
        let mut calm = 0;
        let mut s = 1.0f32;
        // 观看者一秒内要了 3 次关键帧：降到 70%
        s = adapt_rate(s, &mut calm, 3, 0);
        assert!((s - 0.7).abs() < 1e-4);
        // 匀速队列丢包：再降
        s = adapt_rate(s, &mut calm, 0, 2);
        assert!((s - 0.49).abs() < 1e-4);
        // 一直拥堵：最低 30%
        for _ in 0..10 {
            s = adapt_rate(s, &mut calm, 5, 0);
        }
        assert!((s - 0.3).abs() < 1e-4);
        // 偶尔一次关键帧请求（新观看者）不算拥堵
        let t = adapt_rate(s, &mut calm, 1, 0);
        assert_eq!(t, s);
        // 连续 4 秒正常后每秒回升 10%，最多到 100%
        for _ in 0..3 {
            s = adapt_rate(s, &mut calm, 0, 0);
        }
        let before = s;
        s = adapt_rate(s, &mut calm, 0, 0);
        assert!(s > before);
        for _ in 0..30 {
            s = adapt_rate(s, &mut calm, 0, 0);
        }
        assert_eq!(s, 1.0);
    }

    #[test]
    fn ltr_slots_protect_common_reference() {
        let mut l = LtrState::new();
        l.reset(1);
        l.slots = [Some(10), None];
        // 没人确认过：没有公共槽；新的存进空槽
        assert_eq!(l.common_slot(&l.capable(&[1, 2])), None);
        assert_eq!(l.free_slot(&[]), 1);
        l.ack(1, 10);
        l.ack(2, 10);
        let cap = l.capable(&[1, 2, 3]); // 3 是旧版本，从不确认
        assert_eq!(cap, vec![1, 2]);
        assert_eq!(l.common_slot(&cap), Some(0));
        // 大家都确认的槽 0 不能被覆盖
        assert_eq!(l.free_slot(&cap), 1);
        l.slots[1] = Some(40);
        l.ack(1, 40);
        assert_eq!(l.common_slot(&cap), Some(0), "only one viewer has 40");
        l.ack(2, 40);
        assert_eq!(l.common_slot(&cap), Some(1));
        assert_eq!(l.free_slot(&cap), 0);
    }

    #[test]
    fn fec_follows_reported_loss() {
        assert_eq!(fec_ratio(0), 0.0);
        assert_eq!(fec_ratio(10), 0.15);
        assert_eq!(fec_ratio(100), 0.6);
        assert_eq!(fec_ratio(300), 1.2);
        // 关键帧更高，但有上限
        assert_eq!(key_fec_ratio(0.0), 0.1);
        assert!((key_fec_ratio(0.6) - 0.9).abs() < 1e-6);
        assert_eq!(key_fec_ratio(2.0), voc_proto::udp::RS_MAX_RATIO);
        assert_eq!(fec_ratio(500), 2.0);
    }

    /// 系统给帧的间隔是一格一格的（这里模拟 144 / 70 / 60 帧的源，带一点抖动），按时间表限帧后平均帧率要接近所选值
    #[test]
    fn pacing_reaches_target_fps() {
        let run = |source_hz: f64, target: u32| -> f64 {
            let interval = Duration::from_secs_f64(1.0 / target as f64);
            let t0 = Instant::now();
            let mut due = None;
            let mut n = 0u32;
            let mut seed = 12345u32;
            for i in 0..(source_hz as u32 * 10) {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let jitter = (seed >> 16) as f64 / 65536.0 * 0.002; // 0～2 ms 线程调度抖动
                let t = t0 + Duration::from_secs_f64(i as f64 / source_hz + jitter);
                if super::pace_accept(&mut due, t, interval) {
                    n += 1;
                }
            }
            n as f64 / 10.0
        };
        for (src, target) in [(144.0, 60), (144.0, 30), (70.0, 30), (180.0, 60), (60.0, 60), (60.0, 30), (61.0, 60)] {
            let fps = run(src, target);
            assert!(fps >= target as f64 * 0.97 && fps <= target as f64 * 1.02, "source {src} target {target}: got {fps}");
        }
        // 源本身不够快时：有多少给多少
        assert!(run(40.0, 60) >= 39.0);
        // 画面静止一阵后恢复：不会一下子补一串
        let interval = Duration::from_millis(33);
        let t0 = Instant::now();
        let mut due = None;
        assert!(super::pace_accept(&mut due, t0, interval));
        assert!(super::pace_accept(&mut due, t0 + Duration::from_secs(5), interval));
        assert!(!super::pace_accept(&mut due, t0 + Duration::from_secs(5) + Duration::from_millis(7), interval));
    }

    #[test]
    fn auto_resolution_steps() {
        assert_eq!(res_for_scale(1080, 1.0), 1080);
        assert_eq!(res_for_scale(1080, 0.7), 900);
        assert_eq!(res_for_scale(1080, 0.49), 720);
        assert_eq!(res_for_scale(1080, 0.3), 540);
        assert_eq!(res_for_scale(720, 0.7), 540);
        assert_eq!(res_for_scale(720, 0.3), 540);
        assert_eq!(res_for_scale(480, 0.3), 480, "never below the lowest step the user picked");
    }

    #[test]
    fn static_frames_skipped_only_when_safe() {
        let ms = |m| Some(Duration::from_millis(m));
        assert!(can_skip_static(Some((0, 0)), true, ms(100), false), "nothing changed");
        assert!(!can_skip_static(Some((0, 0)), true, ms(1200), false), "at least one frame per second");
        // 变化很小（时钟、光标）：最多每秒 10 帧
        assert!(can_skip_static(Some((1, 4)), true, ms(50), false));
        assert!(!can_skip_static(Some((1, 4)), true, ms(110), false));
        // 大范围变化：照常发
        assert!(!can_skip_static(Some((2, 300)), true, ms(5), false), "screen changed a lot");
        assert!(!can_skip_static(Some((0, 0)), false, ms(100), false), "system never reported regions: can't trust empty");
        assert!(!can_skip_static(None, true, ms(100), false), "no dirty info");
        assert!(!can_skip_static(Some((0, 0)), true, ms(100), true), "someone waits for a keyframe");
        assert!(!can_skip_static(Some((0, 0)), true, None, false), "first frame");
    }

    #[test]
    fn quality_tiers() {
        let c = |q| ShareConfig { height: 1080, fps: 60, quality: q, ..Default::default() };
        assert_eq!((c(0).bitrate(), c(1).bitrate(), c(2).bitrate()), (5_500_000, 12_100_000, 24_750_000));
        assert_eq!(c(2).effective(true).quality, 0, "low bandwidth forces standard");
        assert_eq!(c(2).low_layer().bitrate(), 650_000);
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
        assert_eq!(plan_encoder("auto", &all, &hw, None).0, Codec::Av1);
        // 有人不能解 AV1 → HEVC；有旧版客户端（没上报）→ 只能 H.264
        let some = common_decoders(&[v(&["av1", "hevc", "h264"]), v(&["hevc", "h264"])]);
        assert_eq!(plan_encoder("auto", &some, &hw, None).0, Codec::Hevc);
        // 画面在 AMD 显卡上：同样是 HEVC，优先用 AMD 的（能走显卡直通）；格式优先级不变
        assert_eq!(plan_encoder("auto", &some, &hw, Some(0x1002)).1.unwrap().name, "AMD HEVC");
        assert_eq!(plan_encoder("auto", &some, &hw, Some(0x10de)).1.unwrap().name, "NV HEVC");
        assert_eq!(plan_encoder("auto", &all, &hw, Some(0x1002)).1.unwrap().name, "NV AV1");
        let old = common_decoders(&[v(&["av1", "hevc", "h264"]), vec![]]);
        assert_eq!(old, vec![Codec::H264]);
        assert_eq!(plan_encoder("auto", &old, &hw, None).1.unwrap().name, "AMD H264");
        assert!(plan_encoder("vendor:10de", &old, &hw, Some(0x10de)).1.is_none(), "NVIDIA has no H.264 here -> software");
        // 指定 AMD：只用 AMD 的编码器
        let (c, e) = plan_encoder("vendor:1002", &all, &hw, Some(0x10de));
        assert_eq!((c, e.unwrap().name.as_str()), (Codec::Hevc, "AMD HEVC"));
        assert_eq!(plan_encoder("software", &all, &hw, None), (Codec::H264, None));
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

    /// 中途强制关键帧后，新来的观看者（新解码器）从这一帧开始能不能解（H.264 要带 SPS/PPS 的 IDR）
    #[test]
    #[ignore]
    fn forced_keyframe_starts_fresh_decoder() {
        let (sw, sh) = (1280, 720);
        let nal_types = |d: &[u8]| -> Vec<u8> {
            let mut v = Vec::new();
            let mut i = 0;
            while i + 3 < d.len() {
                if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
                    v.push(d[i + 3] & 0x1f);
                    i += 3;
                }
                i += 1;
            }
            v
        };
        let mut encs: Vec<(String, Codec, Option<HwEncoderInfo>)> =
            hw_encoders().iter().filter(|i| i.codec == Codec::H264).map(|i| (i.name.clone(), i.codec, Some(i.clone()))).collect();
        encs.push(("OpenH264".into(), Codec::H264, None));
        for (name, codec, info) in encs {
            let mut enc = VideoEncoder::new(ShareConfig { height: 720, fps: 30, ..Default::default() });
            let plan = (codec, info);
            let mut pkts = Vec::new();
            for t in 0..40 {
                pkts.extend(enc.encode(&test_frame(sw, sh, t), sw, sh, &plan, t == 0 || t == 20).unwrap());
            }
            let keys: Vec<(usize, Vec<u8>)> = pkts.iter().enumerate().filter(|(_, p)| p.key).map(|(i, p)| (i, nal_types(&p.data))).collect();
            println!("{name}: keyframes (index, NAL types) {keys:?}");
            // 从第二个关键帧开始喂给新解码器
            let start = keys.get(1).map(|k| k.0).unwrap_or(usize::MAX);
            let mut d = make_decoder(codec, false, 1280, 720).expect("decoder");
            let mut got = 0;
            for p in pkts.iter().skip(start) {
                match &mut d {
                    Dec::Mf(m) => got += m.decode(&p.data).map(|f| f.len()).unwrap_or(0),
                    Dec::Open(o) => got += o.decode(&p.data).ok().flatten().is_some() as usize,
                }
            }
            println!("{name}: fresh decoder from 2nd keyframe decoded {got} frames");
            assert!(keys.len() >= 2 && keys[1].1.contains(&7) && keys[1].1.contains(&5), "{name}: forced keyframe must be IDR with SPS");
            assert!(got > 0, "{name}: a new viewer must be able to start from a forced keyframe");
        }
    }

    /// 真实显卡：长期参考帧（LTR）和峰值受限可变码率支不支持（需要显卡，默认不跑）
    #[test]
    #[ignore]
    fn ltr_and_vbr_support() {
        use windows::Win32::Media::MediaFoundation::*;
        mfenc::mf_init().unwrap();
        for info in hw_encoders() {
            let Ok(enc) = HwEncoder::new(info, 1280, 720, 30, 4_000_000) else { continue };
            let ltr_ctl = enc.supports(&CODECAPI_AVEncVideoLTRBufferControl);
            let ltr_mark = enc.supports(&CODECAPI_AVEncVideoMarkLTRFrame);
            let ltr_use = enc.supports(&CODECAPI_AVEncVideoUseLTRFrame);
            let ltr_set = enc.try_set(&CODECAPI_AVEncVideoLTRBufferControl, 0x0001_0002);
            let max_rate = enc.supports(&CODECAPI_AVEncCommonMaxBitRate);
            let vbr = enc.try_set(&CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32);
            let quality = enc.try_set(&CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_Quality.0 as u32);
            println!(
                "{}: LTR buffer={ltr_ctl} mark={ltr_mark} use={ltr_use} set={ltr_set} | MaxBitRate={max_rate} PeakConstrainedVBR={vbr} Quality={quality}",
                info.name
            );
        }
    }

    /// 真实显卡：哪些编码器接受逐行刷新、开了以后关键帧间隔和能否正常解码（需要显卡，默认不跑）
    #[test]
    #[ignore]
    fn intra_refresh_support() {
        mfenc::mf_init().unwrap();
        let (w, h) = (1280usize, 720usize);
        for info in hw_encoders() {
            let Ok(mut enc) = HwEncoder::new(info, w as u32, h as u32, 30, 4_000_000) else { continue };
            let mut pkts = Vec::new();
            for t in 0..90 {
                let mut i420 = Vec::new();
                bgra_to_i420(&test_frame(w, h, t), w, h, w, h, &mut i420);
                let mut nv12 = Vec::new();
                i420_to_nv12(&i420, w, h, &mut nv12);
                if t == 0 {
                    enc.force_keyframe();
                }
                pkts.extend(enc.encode(&nv12).unwrap());
            }
            let keys = pkts.iter().filter(|p| p.1).count();
            let mut decoded = 0;
            if local_decoders().contains(&info.codec) {
                if let Some(mut d) = make_decoder(info.codec, true, w as u32, h as u32) {
                    for p in &pkts {
                        if let Dec::Mf(m) = &mut d {
                            decoded += m.decode(&p.0).map(|f| f.len()).unwrap_or(0);
                        }
                    }
                }
            }
            println!("{}: intra_refresh={} packets={} keyframes={keys} decoded={decoded}", info.name, enc.intra_refresh, pkts.len());
        }
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
