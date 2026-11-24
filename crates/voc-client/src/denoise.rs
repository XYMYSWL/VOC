//! 麦克风降噪。
//!
//! 所有降噪器处理同一种格式：48 kHz 单声道、480 采样（10 ms）、f32 ∈ [-1, 1]。
//!
//! - [`Rnnoise`]：内置，免费开源的 RNNoise（纯 Rust 移植 nnnoiseless）。
//! - [`Plugin`]：从 DLL 动态加载第三方降噪（例如 Krisp）。DLL 需要导出
//!   `plugins/voc_denoise.h` 里定义的 C 接口；Krisp 的适配层见 `plugins/krisp_shim/`。

use crate::audio::{CAPTURE_FRAME, CAPTURE_RATE};
use serde::{Deserialize, Serialize};
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::Path;
use std::time::{Duration, Instant};
use std::sync::{Arc, Mutex};

pub const PLUGIN_ABI_VERSION: u32 = 1;

pub trait Denoiser: Send {
    /// 原地降噪一帧（CAPTURE_FRAME 个样本）
    fn process(&mut self, frame: &mut [f32]);
    fn name(&self) -> String;
}

/// 编码线程每帧读取；UI 线程替换
pub type DenoiseSlot = Arc<Mutex<Option<Box<dyn Denoiser>>>>;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DenoiseMode {
    Off,
    /// 老版 RNNoise（nnnoiseless，2017 年的模型）
    Rnnoise,
    /// RNNoise 0.2（2024 年的新模型，第 68 项；默认）
    #[default]
    Rnnoise2,
    /// DeepFilterNet3（第 67 / 68 项）：效果更好、更费 CPU，强度见 DFN_LEVELS
    Dfn3,
    /// 外部插件（Krisp 等）
    Plugin,
}

/// DeepFilterNet3 降噪强度：(名字, 噪音最多压低多少 dB, 后置滤波强度（0 = 不开）)
pub const DFN_LEVELS: [(&str, f32, f32); 4] = [("标准", 12.0, 0.0), ("高", 24.0, 0.0), ("极高", 100.0, 0.0), ("暴力", 100.0, 0.03)];


pub fn create(mode: DenoiseMode, plugin_path: &str, model_path: &str, dfn_level: u8) -> Result<Option<Box<dyn Denoiser>>, String> {
    Ok(match mode {
        DenoiseMode::Dfn3 => Some(Box::new(Dfn3::new(dfn_level)?)),
        DenoiseMode::Off => None,
        DenoiseMode::Rnnoise => Some(Box::new(Rnnoise::new())),
        DenoiseMode::Rnnoise2 => Some(Box::new(Rnnoise2::new()?)),
        DenoiseMode::Plugin => {
            if plugin_path.trim().is_empty() {
                return Err("请先填写降噪插件 DLL 路径".into());
            }
            Some(Box::new(Plugin::load(Path::new(plugin_path.trim()), model_path.trim())?))
        }
    })
}

// ------------------------------------------------------------ DeepFilterNet3（crates/deep_filter，v0.5.6）

/// DeepFilterNet3 的模型对象不能跨线程搬：放在自己的线程里加载、处理，每帧通过通道送过去拿回来（只多几微秒）。
/// 加载要一两秒，加载好之前先用 RNNoise 新版；一帧 8 ms 还没回来就这一帧用 RNNoise 新版，处理平均超过 7 ms（一帧 10 ms）就整个退回
pub struct Dfn3 {
    tx: std::sync::mpsc::Sender<Vec<f32>>,
    rx: std::sync::mpsc::Receiver<Vec<f32>>,
    status: std::sync::mpsc::Receiver<Result<(), String>>,
    ready: bool,
    fallback: Rnnoise2,
    level: u8,
    /// 退回 RNNoise 的原因
    gave_up: Option<String>,
    timing: (Duration, u32),
    late: u32,
}

impl Dfn3 {
    pub fn new(level: u8) -> Result<Self, String> {
        let level = level.min(DFN_LEVELS.len() as u8 - 1);
        let (_, lim, pf) = DFN_LEVELS[level as usize];
        let (tx, frames_rx) = std::sync::mpsc::channel::<Vec<f32>>();
        let (out_tx, rx) = std::sync::mpsc::channel::<Vec<f32>>();
        let (st_tx, status) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("dfn3".into())
            .spawn(move || {
                use deep_filter::tract::{DfParams, DfTract, RuntimeParams};
                let mut rp = RuntimeParams::default_with_ch(1).with_atten_lim(lim);
                if pf > 0.0 {
                    rp = rp.with_post_filter(pf);
                }
                let m = std::panic::catch_unwind(|| DfTract::new(DfParams::default(), &rp).map_err(|e| e.to_string()))
                    .unwrap_or_else(|_| Err("模型加载出错".into()))
                    .and_then(|m| if m.hop_size == CAPTURE_FRAME && m.sr == 48_000 { Ok(m) } else { Err(format!("模型帧长 {} / 采样率 {} 不对", m.hop_size, m.sr)) });
                let mut m = match m {
                    Ok(m) => {
                        let _ = st_tx.send(Ok(()));
                        m
                    }
                    Err(e) => {
                        let _ = st_tx.send(Err(e));
                        return;
                    }
                };
                let mut inb = ndarray::Array2::<f32>::zeros((1, CAPTURE_FRAME));
                let mut outb = ndarray::Array2::<f32>::zeros((1, CAPTURE_FRAME));
                // 降噪器被换掉（发送端没了）就结束
                while let Ok(mut f) = frames_rx.recv() {
                    for (d, s) in inb.iter_mut().zip(f.iter()) {
                        *d = *s;
                    }
                    if m.process(inb.view(), outb.view_mut()).is_err() {
                        return;
                    }
                    for (d, s) in f.iter_mut().zip(outb.iter()) {
                        *d = s.clamp(-1.0, 1.0);
                    }
                    if out_tx.send(f).is_err() {
                        return;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { tx, rx, status, ready: false, fallback: Rnnoise2::new()?, level, gave_up: None, timing: (Duration::ZERO, 0), late: 0 })
    }
}

impl Denoiser for Dfn3 {
    fn process(&mut self, frame: &mut [f32]) {
        if !self.ready && self.gave_up.is_none() {
            match self.status.try_recv() {
                Ok(Ok(())) => self.ready = true,
                Ok(Err(e)) => self.gave_up = Some(e),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.gave_up = Some("模型线程退出了".into()),
                Err(_) => {}
            }
        }
        if !self.ready || self.gave_up.is_some() {
            self.fallback.process(frame);
            return;
        }
        let t = Instant::now();
        if self.tx.send(frame.to_vec()).is_err() {
            self.gave_up = Some("处理出错".into());
            self.fallback.process(frame);
            return;
        }
        match self.rx.recv_timeout(Duration::from_millis(8)) {
            Ok(out) => frame.copy_from_slice(&out[..frame.len()]),
            Err(_) => {
                // 这一帧没赶上：用 RNNoise 新版顶上（之后晚到的那一帧丢掉）
                self.late += 1;
                self.fallback.process(frame);
                while self.rx.try_recv().is_ok() {}
            }
        }
        // 太慢（电脑跑不过来，会让声音断断续续）：退回 RNNoise 新版
        self.timing.0 += t.elapsed();
        self.timing.1 += 1;
        if self.timing.1 >= 300 {
            let avg = self.timing.0 / self.timing.1;
            if avg > Duration::from_millis(7) || self.late > 15 {
                self.gave_up = Some(format!("这台电脑跑不过来（每 10 ms 一帧要 {:.1} ms）", avg.as_secs_f32() * 1000.0));
            }
            self.timing = (Duration::ZERO, 0);
            self.late = 0;
        }
    }
    fn name(&self) -> String {
        let lv = DFN_LEVELS[self.level as usize].0;
        match (&self.gave_up, self.ready) {
            (Some(e), _) => format!("RNNoise 新版（DeepFilterNet3 退回了：{e}）"),
            (None, true) => format!("DeepFilterNet3 · {lv}"),
            (None, false) => format!("DeepFilterNet3 · {lv}（加载中，先用 RNNoise 新版）"),
        }
    }
}

// ------------------------------------------------------------ RNNoise 0.2（C 库，build.rs 编进来）

#[repr(C)]
struct DenoiseStateC {
    _p: [u8; 0],
}

unsafe extern "C" {
    fn rnnoise_get_frame_size() -> i32;
    fn rnnoise_create(model: *mut core::ffi::c_void) -> *mut DenoiseStateC;
    fn rnnoise_destroy(st: *mut DenoiseStateC);
    fn rnnoise_process_frame(st: *mut DenoiseStateC, out: *mut f32, input: *const f32) -> f32;
}

pub struct Rnnoise2 {
    st: *mut DenoiseStateC,
    buf: Vec<f32>,
    out: Vec<f32>,
}

// 只在降噪那一个线程里用
unsafe impl Send for Rnnoise2 {}

impl Rnnoise2 {
    pub fn new() -> Result<Self, String> {
        let n = unsafe { rnnoise_get_frame_size() } as usize;
        if n != CAPTURE_FRAME {
            return Err(format!("RNNoise 新版一帧 {n} 个采样，和 VOC 的 {CAPTURE_FRAME} 不一样"));
        }
        let st = unsafe { rnnoise_create(std::ptr::null_mut()) };
        if st.is_null() {
            return Err("RNNoise 新版初始化失败".into());
        }
        Ok(Self { st, buf: vec![0.0; CAPTURE_FRAME], out: vec![0.0; CAPTURE_FRAME] })
    }
}

impl Drop for Rnnoise2 {
    fn drop(&mut self) {
        unsafe { rnnoise_destroy(self.st) };
    }
}

impl Denoiser for Rnnoise2 {
    fn process(&mut self, frame: &mut [f32]) {
        // 和老版一样，期望 i16 量级的浮点数
        for (b, x) in self.buf.iter_mut().zip(frame.iter()) {
            *b = x * 32767.0;
        }
        unsafe { rnnoise_process_frame(self.st, self.out.as_mut_ptr(), self.buf.as_ptr()) };
        for (x, o) in frame.iter_mut().zip(&self.out) {
            *x = (o / 32767.0).clamp(-1.0, 1.0);
        }
    }
    fn name(&self) -> String {
        "RNNoise 0.2".into()
    }
}

// ------------------------------------------------------------ RNNoise（老版，nnnoiseless）

pub struct Rnnoise {
    st: Box<nnnoiseless::DenoiseState<'static>>,
    buf: Vec<f32>,
    out: Vec<f32>,
}

impl Rnnoise {
    pub fn new() -> Self {
        assert_eq!(nnnoiseless::DenoiseState::FRAME_SIZE, CAPTURE_FRAME);
        Self {
            st: nnnoiseless::DenoiseState::new(),
            buf: vec![0.0; CAPTURE_FRAME],
            out: vec![0.0; CAPTURE_FRAME],
        }
    }
}

impl Denoiser for Rnnoise {
    fn process(&mut self, frame: &mut [f32]) {
        // nnnoiseless 期望 i16 量级的浮点数
        for (b, x) in self.buf.iter_mut().zip(frame.iter()) {
            *b = x * 32767.0;
        }
        self.st.process_frame(&mut self.out, &self.buf);
        for (x, o) in frame.iter_mut().zip(&self.out) {
            *x = (o / 32767.0).clamp(-1.0, 1.0);
        }
    }
    fn name(&self) -> String {
        "RNNoise".into()
    }
}

// ------------------------------------------------------------ DLL 插件

type AbiVersionFn = unsafe extern "C" fn() -> u32;
type NameFn = unsafe extern "C" fn() -> *const c_char;
type CreateFn = unsafe extern "C" fn(*const c_char, u32, u32, *mut c_char, u32) -> *mut c_void;
type ProcessFn = unsafe extern "C" fn(*mut c_void, *const f32, *mut f32, u32) -> i32;
type DestroyFn = unsafe extern "C" fn(*mut c_void);

pub struct Plugin {
    handle: *mut c_void,
    process: ProcessFn,
    destroy: DestroyFn,
    name: String,
    out: Vec<f32>,
    failed: bool,
    // 必须最后释放：函数指针依赖它
    _lib: libloading::Library,
}

// 句柄只会被持有它的那一个线程使用（通过 DenoiseSlot 的 Mutex 串行访问）
unsafe impl Send for Plugin {}

fn open_library(path: &Path) -> Result<libloading::Library, libloading::Error> {
    #[cfg(windows)]
    unsafe {
        // 让插件依赖的其他 DLL（比如 Krisp SDK 本体）从插件所在目录加载
        libloading::os::windows::Library::load_with_flags(
            path,
            libloading::os::windows::LOAD_WITH_ALTERED_SEARCH_PATH,
        )
        .map(Into::into)
    }
    #[cfg(not(windows))]
    unsafe {
        libloading::Library::new(path)
    }
}

impl Plugin {
    pub fn load(path: &Path, model_path: &str) -> Result<Self, String> {
        let lib = open_library(path).map_err(|e| format!("加载插件失败：{e}"))?;
        unsafe {
            let abi: AbiVersionFn = *lib
                .get(b"voc_denoise_abi_version\0")
                .map_err(|_| "这不是VOC的降噪插件（缺少 voc_denoise_abi_version）".to_string())?;
            let v = abi();
            if v != PLUGIN_ABI_VERSION {
                return Err(format!("插件接口版本 {v} 与客户端 {PLUGIN_ABI_VERSION} 不兼容"));
            }
            let create: CreateFn = *lib.get(b"voc_denoise_create\0").map_err(|e| e.to_string())?;
            let process: ProcessFn = *lib.get(b"voc_denoise_process\0").map_err(|e| e.to_string())?;
            let destroy: DestroyFn = *lib.get(b"voc_denoise_destroy\0").map_err(|e| e.to_string())?;
            let name = match lib.get::<NameFn>(b"voc_denoise_name\0") {
                Ok(f) => {
                    let p = f();
                    if p.is_null() { "插件".into() } else { CStr::from_ptr(p).to_string_lossy().into_owned() }
                }
                Err(_) => "插件".into(),
            };

            let model = CString::new(model_path).map_err(|_| "模型路径包含非法字符")?;
            let mut err = vec![0u8; 512];
            let handle = create(
                model.as_ptr(),
                CAPTURE_RATE,
                CAPTURE_FRAME as u32,
                err.as_mut_ptr() as *mut c_char,
                err.len() as u32,
            );
            if handle.is_null() {
                let msg = CStr::from_bytes_until_nul(&err)
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                return Err(format!("{name} 初始化失败：{}", if msg.is_empty() { "未知错误" } else { &msg }));
            }
            Ok(Self { handle, process, destroy, name, out: vec![0.0; CAPTURE_FRAME], failed: false, _lib: lib })
        }
    }
}

impl Denoiser for Plugin {
    fn process(&mut self, frame: &mut [f32]) {
        if self.failed {
            return; // 出错后直接透传原声，保证还能说话
        }
        let rc = unsafe { (self.process)(self.handle, frame.as_ptr(), self.out.as_mut_ptr(), frame.len() as u32) };
        if rc == 0 {
            frame.copy_from_slice(&self.out[..frame.len()]);
        } else {
            eprintln!("降噪插件 {} 返回错误 {rc}，已停用", self.name);
            self.failed = true;
        }
    }
    fn name(&self) -> String {
        self.name.clone()
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        unsafe { (self.destroy)(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rnnoise_removes_fan_noise() {
        let mut d = Rnnoise::new();
        let mut seed = 1u32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        };
        let (mut e_in, mut e_out) = (0.0f32, 0.0f32);
        let mut lp = 0.0f32;
        for i in 0..600 {
            let mut f: Vec<f32> = (0..CAPTURE_FRAME)
                .map(|_| {
                    lp += (rnd() - lp) * 0.05; // 低通，模拟风扇/空调这类低频噪声
                    lp * 0.1
                })
                .collect();
            let before: f32 = f.iter().map(|x| x * x).sum();
            d.process(&mut f);
            if i > 300 {
                e_in += before;
                e_out += f.iter().map(|x| x * x).sum::<f32>();
            }
        }
        eprintln!("noise in={e_in} out={e_out} ({:.1} dB)", 10.0 * (e_out / e_in).log10());
        assert!(e_out < e_in * 0.1, "noise not reduced: in={e_in} out={e_out}");
    }

    /// 需要先 `cargo build -p voc-denoise-example`，再设置 VOC_TEST_PLUGIN 为生成的 DLL 路径
    #[test]
    fn example_plugin_loads() {
        let Ok(path) = std::env::var("VOC_TEST_PLUGIN") else {
            eprintln!("跳过：未设置 VOC_TEST_PLUGIN");
            return;
        };
        let mut p = Plugin::load(Path::new(&path), "").expect("load plugin");
        assert_eq!(p.name(), "示例插件（音量减半）");
        let mut f = vec![0.5f32; CAPTURE_FRAME];
        p.process(&mut f);
        assert!(f.iter().all(|x| (*x - 0.25).abs() < 1e-6));
        drop(p);
        assert!(Plugin::load(Path::new(&path), "fail").err().unwrap().contains("故意失败"));
    }

    /// 用 MSVC 编译 plugins/krisp_shim 模板后，设置 VOC_TEST_KRISP_SHIM 指向 DLL
    #[test]
    fn krisp_shim_template_loads() {
        let Ok(path) = std::env::var("VOC_TEST_KRISP_SHIM") else {
            eprintln!("跳过：未设置 VOC_TEST_KRISP_SHIM");
            return;
        };
        let err = Plugin::load(Path::new(&path), "").err().unwrap();
        assert!(err.contains("模型文件路径"), "{err}");
        let mut p = Plugin::load(Path::new(&path), "model.kef").expect("load");
        assert_eq!(p.name(), "Krisp");
        let mut f = vec![0.3f32; CAPTURE_FRAME];
        p.process(&mut f);
        assert!(f.iter().all(|x| *x == 0.3)); // 模板未接 SDK，透传
    }
}

#[cfg(test)]
mod rnnoise2_tests {
    use super::*;

    /// 白噪声：新版、老版都应该压下去很多（新版的 C 库确实编进来、能跑）
    #[test]
    fn rnnoise2_suppresses_white_noise() {
        let mut x = 12345u32;
        let mut noise = || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x as f32 / u32::MAX as f32 - 0.5) * 0.2
        };
        let frames: Vec<Vec<f32>> = (0..300).map(|_| (0..CAPTURE_FRAME).map(|_| noise()).collect()).collect();
        let energy = |d: &mut dyn Denoiser| {
            let mut e = 0.0f64;
            for (i, f) in frames.iter().enumerate() {
                let mut f = f.clone();
                d.process(&mut f);
                if i >= 100 {
                    e += f.iter().map(|v| (*v as f64).powi(2)).sum::<f64>();
                }
            }
            e
        };
        let input: f64 = frames[100..].iter().flatten().map(|v| (*v as f64).powi(2)).sum();
        let new = energy(&mut Rnnoise2::new().unwrap());
        let old = energy(&mut Rnnoise::new());
        eprintln!("白噪声剩下：新版 {:.1} dB，老版 {:.1} dB", 10.0 * (new / input).log10(), 10.0 * (old / input).log10());
        assert!(new < input * 0.1, "new RNNoise should cut white noise by >10 dB");
    }

    /// DeepFilterNet3：加载好之后压白噪声，并量每帧多久（一帧 10 ms，要远小于它）
    #[test]
    fn dfn3_loads_and_suppresses() {
        let mut x = 777u32;
        let mut noise = || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x as f32 / u32::MAX as f32 - 0.5) * 0.2
        };
        for level in [0u8, 3] {
            let mut d = Dfn3::new(level).unwrap();
            let t = Instant::now();
            while !d.name().starts_with("DeepFilterNet3 · ") || d.name().contains("加载中") {
                let mut f: Vec<f32> = (0..CAPTURE_FRAME).map(|_| noise()).collect();
                d.process(&mut f);
                assert!(t.elapsed() < Duration::from_secs(30), "load: {}", d.name());
                std::thread::sleep(Duration::from_millis(5));
            }
            eprintln!("DeepFilterNet3 加载用了 {:.1} 秒", t.elapsed().as_secs_f32());
            let (mut ein, mut eout) = (0f64, 0f64);
            let t = Instant::now();
            for i in 0..400 {
                let mut f: Vec<f32> = (0..CAPTURE_FRAME).map(|_| noise()).collect();
                if i >= 100 {
                    ein += f.iter().map(|v| (*v as f64).powi(2)).sum::<f64>();
                }
                d.process(&mut f);
                if i >= 100 {
                    eout += f.iter().map(|v| (*v as f64).powi(2)).sum::<f64>();
                }
            }
            let per = t.elapsed().as_secs_f32() * 1000.0 / 400.0;
            eprintln!("DeepFilterNet3「{}」：白噪声剩下 {:.1} dB，每帧 {per:.2} ms（{}）", DFN_LEVELS[level as usize].0, 10.0 * (eout / ein).log10(), d.name());
            assert!(d.name().starts_with("DeepFilterNet3"), "{}", d.name());
            assert!(eout < ein * 0.5);
        }
    }
}
