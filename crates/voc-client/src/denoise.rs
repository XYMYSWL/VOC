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
    #[default]
    Rnnoise,
    /// 外部插件（Krisp 等）
    Plugin,
}


pub fn create(mode: DenoiseMode, plugin_path: &str, model_path: &str) -> Result<Option<Box<dyn Denoiser>>, String> {
    Ok(match mode {
        DenoiseMode::Off => None,
        DenoiseMode::Rnnoise => Some(Box::new(Rnnoise::new())),
        DenoiseMode::Plugin => {
            if plugin_path.trim().is_empty() {
                return Err("请先填写降噪插件 DLL 路径".into());
            }
            Some(Box::new(Plugin::load(Path::new(plugin_path.trim()), model_path.trim())?))
        }
    })
}

// ------------------------------------------------------------ RNNoise

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
