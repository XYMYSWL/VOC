//! 屏幕共享的显卡直通：采集到的画面不拷回内存，在显卡上缩放、转成 NV12，直接交给硬件编码器。
//!
//! 采集库的 D3D11 设备没开视频支持，所以分两边：
//! - 采集线程（采集库的设备）：把每帧拷进一张共享纹理（带 keyed mutex），只是一次显卡内部拷贝；
//! - 编码线程（我们自己在同一块显卡上建的设备，开了视频支持）：打开这张共享纹理，
//!   用显卡的视频处理器缩放 + BGRA→NV12（BT.601 有限范围，和 CPU 路径一致），输出纹理交给编码器。
//! keyed mutex：采集端拿 0、放 1；编码端拿 1、放 0。编码端还没处理完时采集端拿不到 0，就丢这一帧。

use windows::core::{Interface, Result};
use windows::Win32::Foundation::{E_FAIL, HANDLE, HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager};

/// WAIT_TIMEOUT 是「成功」类返回值，windows-rs 会当成 Ok，要自己看原始返回值
const WAIT_TIMEOUT_HR: i32 = 0x102;

/// 拿 keyed mutex；超时返回 false
pub fn acquire(km: &IDXGIKeyedMutex, key: u64, ms: u32) -> bool {
    let hr = unsafe { (Interface::vtable(km).AcquireSync)(km.as_raw(), key, ms) };
    hr.0 == 0 && hr.0 != WAIT_TIMEOUT_HR
}

fn release(km: &IDXGIKeyedMutex, key: u64) {
    unsafe {
        let _ = km.ReleaseSync(key);
    }
}

/// LUID 对应的显卡厂商号（0x10DE NVIDIA、0x1002 AMD、0x8086 Intel）
pub fn vendor_of(luid: LUID) -> Option<u32> {
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory1().ok()?;
        let a: IDXGIAdapter1 = factory.EnumAdapterByLuid(luid).ok()?;
        Some(a.GetDesc1().ok()?.VendorId)
    }
}

/// 设备所在显卡的 LUID
pub fn adapter_luid(dev: &ID3D11Device) -> Result<LUID> {
    unsafe { Ok(dev.cast::<IDXGIDevice>()?.GetAdapter()?.GetDesc()?.AdapterLuid) }
}

/// 采集端：在采集库的设备上维护共享纹理，把每帧拷进去
pub struct CaptureSide {
    tex: Option<(ID3D11Texture2D, IDXGIKeyedMutex, usize, u32, u32)>,
    pub generation: u32,
}

impl Default for CaptureSide {
    fn default() -> Self {
        Self { tex: None, generation: 0 }
    }
}

/// 交给编码线程的一帧（只有句柄和尺寸，画面在显卡上）
#[derive(Clone, Copy, Debug)]
pub struct GpuFrame {
    pub handle: usize,
    pub width: u32,
    pub height: u32,
    pub luid: LUID,
    pub generation: u32,
}

impl CaptureSide {
    /// 拷贝一帧；编码端还在处理上一帧时返回 Ok(None)（丢帧）
    pub fn copy(&mut self, dev: &ID3D11Device, ctx: &ID3D11DeviceContext, src: &ID3D11Texture2D) -> Result<Option<GpuFrame>> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&mut desc) };
        let fresh = !matches!(&self.tex, Some((_, _, _, w, h)) if *w == desc.Width && *h == desc.Height);
        if fresh {
            let d = D3D11_TEXTURE2D_DESC {
                Width: desc.Width,
                Height: desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32,
            };
            let mut t = None;
            unsafe { dev.CreateTexture2D(&d, None, Some(&mut t))? };
            let t = t.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
            let km: IDXGIKeyedMutex = t.cast()?;
            let handle = unsafe { t.cast::<IDXGIResource>()?.GetSharedHandle()? };
            self.tex = Some((t, km, handle.0 as usize, desc.Width, desc.Height));
            // 全局递增：截屏会话重开（例如换成不带鼠标）后新纹理的编号不会和旧的重复，编码端才知道要重新打开
            static NEXT_GENERATION: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
            self.generation = NEXT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let (t, km, handle, w, h) = self.tex.as_ref().unwrap();
        if !acquire(km, 0, 0) {
            return Ok(None);
        }
        unsafe { ctx.CopyResource(t, src) };
        release(km, 1);
        Ok(Some(GpuFrame { handle: *handle, width: *w, height: *h, luid: adapter_luid(dev)?, generation: self.generation }))
    }
}

/// 编码端：自己的设备 + 视频处理器
pub struct Converter {
    pub dev: ID3D11Device,
    pub mgr: IMFDXGIDeviceManager,
    luid: LUID,
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    src: Option<(u32, ID3D11Texture2D, IDXGIKeyedMutex)>,
    vp: Option<VpState>,
    ring: usize,
}

struct VpState {
    key: (u32, u32, u32, u32, u32),
    _en: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    /// 轮流使用几张输出纹理（编码器可能还在异步读上一张）
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
}

impl Converter {
    /// 在指定 LUID 的显卡上建设备（开视频支持），包成编码器要用的 DXGI 设备管理器
    pub fn new(luid: LUID) -> Result<Self> {
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory1()?;
            let adapter: IDXGIAdapter1 = factory.EnumAdapterByLuid(luid)?;
            let mut dev = None;
            let mut ctx = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut dev),
                None,
                Some(&mut ctx),
            )?;
            let dev = dev.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
            let ctx = ctx.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
            if let Ok(mt) = dev.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            let vdev: ID3D11VideoDevice = dev.cast()?;
            let vctx: ID3D11VideoContext = ctx.cast()?;
            let mut token = 0u32;
            let mut mgr = None;
            MFCreateDXGIDeviceManager(&mut token, &mut mgr)?;
            let mgr = mgr.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
            mgr.ResetDevice(&dev, token)?;
            Ok(Self { dev, mgr, luid, vdev, vctx, src: None, vp: None, ring: 0 })
        }
    }

    pub fn luid(&self) -> LUID {
        self.luid
    }

    /// 处理一帧：缩放到 out_w × out_h 的 NV12 纹理（在本设备上）。
    /// keep = 同一帧还要再转一次（低画质层）：用完不还给采集端，留给下一次转换
    pub fn convert(&mut self, f: &GpuFrame, out_w: u32, out_h: u32, fps: u32, keep: bool) -> Result<ID3D11Texture2D> {
        unsafe {
            self.open_src(f)?;
            let key = (f.generation, f.width, f.height, out_w, out_h);
            if !self.vp.as_ref().is_some_and(|v| v.key == key) {
                self.vp = Some(self.make_vp(key, fps)?);
            }
            let (_, _, km) = self.src.as_ref().unwrap();
            if !acquire(km, 1, 100) {
                return Err(windows::core::Error::from(E_FAIL));
            }
            let vp = self.vp.as_ref().unwrap();
            let (tex, view) = &vp.outputs[self.ring % vp.outputs.len()];
            self.ring = self.ring.wrapping_add(1);
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(vp.input.clone())),
                ..Default::default()
            };
            let r = self.vctx.VideoProcessorBlt(&vp.vp, view, 0, &[stream]);
            release(km, if keep { 1 } else { 0 });
            r?;
            Ok(tex.clone())
        }
    }

    /// 打开采集端的共享纹理（换了纹理才重新打开）
    fn open_src(&mut self, f: &GpuFrame) -> Result<()> {
        if !matches!(&self.src, Some((g, _, _)) if *g == f.generation) {
            let mut t: Option<ID3D11Texture2D> = None;
            unsafe { self.dev.OpenSharedResource(HANDLE(f.handle as *mut _), &mut t)? };
            let t = t.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
            let km: IDXGIKeyedMutex = t.cast()?;
            self.src = Some((f.generation, t, km));
            self.vp = None;
        }
        Ok(())
    }

    /// 不处理这一帧，直接把共享纹理还给采集端（否则采集端拿不回来，之后的画面都会卡住）
    pub fn give_back(&mut self, f: &GpuFrame) -> Result<()> {
        self.open_src(f)?;
        let (_, _, km) = self.src.as_ref().unwrap();
        if acquire(km, 1, 100) {
            release(km, 0);
        }
        Ok(())
    }

    unsafe fn make_vp(&self, key: (u32, u32, u32, u32, u32), fps: u32) -> Result<VpState> {
        let (_, in_w, in_h, out_w, out_h) = key;
        let rate = DXGI_RATIONAL { Numerator: fps.max(1), Denominator: 1 };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: in_w,
            InputHeight: in_h,
            OutputFrameRate: rate,
            OutputWidth: out_w,
            OutputHeight: out_h,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        unsafe {
            let en = self.vdev.CreateVideoProcessorEnumerator(&desc)?;
            let vp = self.vdev.CreateVideoProcessor(&en, 0)?;
            let (_, src, _) = self.src.as_ref().unwrap();
            let ivd = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
            };
            let mut input = None;
            self.vdev.CreateVideoProcessorInputView(src, &en, &ivd, Some(&mut input))?;
            let input = input.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
            let mut outputs = Vec::new();
            for _ in 0..4 {
                let d = D3D11_TEXTURE2D_DESC {
                    Width: out_w,
                    Height: out_h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut t = None;
                self.dev.CreateTexture2D(&d, None, Some(&mut t))?;
                let t = t.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
                let ovd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
                };
                let mut v = None;
                self.vdev.CreateVideoProcessorOutputView(&t, &en, &ovd, Some(&mut v))?;
                outputs.push((t, v.ok_or_else(|| windows::core::Error::from(E_FAIL))?));
            }
            // 输入：RGB 全范围；输出：BT.601、16–235（和 CPU 路径 bgra_to_i420 一致）
            let rgb_full = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            let yuv_601_limited = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 1 << 4 };
            self.vctx.VideoProcessorSetStreamColorSpace(&vp, 0, &rgb_full);
            self.vctx.VideoProcessorSetOutputColorSpace(&vp, &yuv_601_limited);
            self.vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            self.vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
            Ok(VpState { key, _en: en, vp, input, outputs })
        }
    }
}

// 只在编码线程里用
unsafe impl Send for Converter {}
