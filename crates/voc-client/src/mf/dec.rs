//! Media Foundation 视频解码（H.264 / HEVC / AV1）。
//! 硬解 = 给解码器绑定 D3D11 设备（DXVA，由显卡解码）；软解 = 不绑设备，用 CPU 解码。

use super::enc::{d3d_manager_any, Codec};
use std::mem::ManuallyDrop;
use windows::core::*;
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;

/// 显卡是否支持 AV1 硬件解码（DXVA AV1 Profile 0）。
/// 系统的 AV1 扩展在软解时只能正确处理与编码最大尺寸一致的画面，所以 AV1 一律走硬解，不支持就不上报 AV1。
pub fn gpu_av1_decode_supported() -> bool {
    use windows::Win32::Graphics::Direct3D11::{ID3D11VideoDevice, D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0};
    let Ok((_, dev)) = d3d_manager_any() else { return false };
    let Ok(vd) = dev.cast::<ID3D11VideoDevice>() else { return false };
    unsafe {
        let n = vd.GetVideoDecoderProfileCount();
        (0..n).any(|i| vd.GetVideoDecoderProfile(i).is_ok_and(|g| g == D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0))
    }
}

pub fn decoder_available(codec: Codec) -> bool {
    find_decoder(codec).is_ok()
}

fn find_decoder(codec: Codec) -> Result<IMFActivate> {
    let it = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: codec.subtype() };
    let mut pp: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut n = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&it),
            None,
            &mut pp,
            &mut n,
        )?;
        let first = if n > 0 { (*pp).take() } else { None };
        for i in 1..n as usize {
            drop((*pp.add(i)).take());
        }
        if !pp.is_null() {
            CoTaskMemFree(Some(pp as _));
        }
        first.ok_or_else(|| Error::from(E_FAIL))
    }
}

fn tag(e: Error, what: &str) -> Error {
    Error::new(e.code(), format!("{what}: {}", e.message()))
}

pub struct MfDecoder {
    mft: IMFTransform,
    pub hardware: bool,
    width: u32,
    height: u32,
    stride: u32,
    disp_w: u32,
    disp_h: u32,
    /// 创建时给的尺寸：输出格式里没写尺寸时用它
    init_w: u32,
    init_h: u32,
    _d3d: Option<(IMFDXGIDeviceManager, ID3D11Device)>,
}

unsafe impl Send for MfDecoder {}

/// 解码出的一帧 NV12（已去掉行填充）
pub struct Nv12Frame {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

impl MfDecoder {
    /// `width` / `height`：画面尺寸（AV1 / HEVC 解码器需要事先知道）
    pub fn new(codec: Codec, hardware: bool, width: u32, height: u32) -> Result<Self> {
        let a = find_decoder(codec)?;
        unsafe {
            let mft: IMFTransform = a.ActivateObject()?;
            let d3d = if hardware {
                let d = d3d_manager_any()?;
                mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, std::mem::transmute::<*mut std::ffi::c_void, usize>(d.0.as_raw()))?;
                Some(d)
            } else {
                None
            };
            if let Ok(attrs) = mft.GetAttributes() {
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &codec.subtype())?;
            // AV1 / HEVC 解码器要求先给尺寸和帧率
            inp.SetUINT64(&MF_MT_FRAME_SIZE, ((width.max(16) as u64) << 32) | height.max(16) as u64)?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, (60u64 << 32) | 1)?;
            inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            mft.SetInputType(0, &inp, 0)?;
            let mut dec = Self { mft, hardware, width: 0, height: 0, stride: 0, disp_w: 0, disp_h: 0, init_w: width, init_h: height, _d3d: d3d };
            dec.set_output_nv12()?;
            dec.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            dec.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(dec)
        }
    }

    fn set_output_nv12(&mut self) -> Result<()> {
        unsafe {
            let mut i = 0;
            loop {
                let t = self.mft.GetOutputAvailableType(0, i)?;
                if t.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    match t.GetUINT64(&MF_MT_FRAME_SIZE) {
                        Ok(sz) if (sz >> 32) > 0 && (sz as u32) > 0 => {
                            self.width = (sz >> 32) as u32;
                            self.height = sz as u32;
                        }
                        _ => {
                            // 有的解码器（AV1 扩展）初始输出类型不带尺寸：用已知尺寸补上
                            self.width = self.init_w;
                            self.height = self.init_h;
                            let _ = t.SetUINT64(&MF_MT_FRAME_SIZE, ((self.width as u64) << 32) | self.height as u64);
                        }
                    }
                    self.stride = self.width;
                    self.mft.SetOutputType(0, &t, 0)?;
                    let mut area = MFVideoArea::default();
                    let mut got = 0u32;
                    let raw = std::slice::from_raw_parts_mut(&mut area as *mut _ as *mut u8, std::mem::size_of::<MFVideoArea>());
                    if t.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, raw, Some(&mut got)).is_ok() && area.Area.cx > 0 {
                        self.disp_w = area.Area.cx as u32;
                        self.disp_h = area.Area.cy as u32;
                    } else {
                        self.disp_w = self.width;
                        self.disp_h = self.height;
                    }
                    return Ok(());
                }
                i += 1;
            }
        }
    }

    /// 送入一帧码流，取出所有解好的画面
    pub fn decode(&mut self, data: &[u8]) -> Result<Vec<Nv12Frame>> {
        unsafe {
            let buf = MFCreateMemoryBuffer(data.len() as u32)?;
            let mut p: *mut u8 = std::ptr::null_mut();
            buf.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            buf.Unlock()?;
            buf.SetCurrentLength(data.len() as u32)?;
            let s = MFCreateSample()?;
            s.AddBuffer(&buf)?;
            s.SetSampleTime(0)?;
            self.mft.ProcessInput(0, &s, 0).map_err(|e| tag(e, "ProcessInput"))?;
        }
        let mut out = Vec::new();
        while let Some(f) = self.pull()? {
            out.push(f);
        }
        Ok(out)
    }

    fn pull(&mut self) -> Result<Option<Nv12Frame>> {
        // 有的解码器（例如 AV1 扩展）按更大的行跨度对齐，报告的大小不够：缓冲过小就加倍重试
        let mut scale = 1;
        loop {
            match self.pull_with(scale) {
                Err(e) if e.code() == MF_E_BUFFERTOOSMALL && scale < 8 => scale *= 2,
                r => return r,
            }
        }
    }

    fn pull_with(&mut self, scale: u32) -> Result<Option<Nv12Frame>> {
        unsafe {
            let info = self.mft.GetOutputStreamInfo(0)?;
            let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;
            let sample = if provides {
                None
            } else {
                let s = MFCreateSample()?;
                // 有的解码器报告的缓冲大小为 0 或按旧尺寸算，按当前画面自己算一个够大的
                // 解码器可能按更宽的行跨度写（例如 2048），缓冲不够时这一帧会被直接丢掉，所以一开始就给足：
                // 宽对齐 256、高对齐 64，再留 25% 余量
                let aw = (self.width.max(64) + 255) & !255;
                let ah = (self.height.max(64) + 63) & !63;
                let need = (aw * ah * 3 / 2 * 5 / 4).max(info.cbSize).max(1 << 16) * scale;
                s.AddBuffer(&MFCreateMemoryBuffer(need)?)?;
                Some(s)
            };
            let mut db = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let r = self.mft.ProcessOutput(0, &mut db, &mut status);
            let sample = ManuallyDrop::take(&mut db[0].pSample);
            let _ = ManuallyDrop::take(&mut db[0].pEvents);
            match r {
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.set_output_nv12().map_err(|e| tag(e, "SetOutputType"))?; // 分辨率等变了：重新协商输出格式后再取
                    return self.pull();
                }
                Err(e) => return Err(tag(e, &format!("ProcessOutput w={} h={} cb={}", self.width, self.height, info.cbSize))),
                Ok(()) => {}
            }
            let Some(sample) = sample else { return Ok(None) };
            let (w, h) = (self.disp_w as usize & !1, self.disp_h as usize & !1);
            let coded_h = self.height as usize;
            if w == 0 || h == 0 {
                return Ok(None);
            }
            let b = sample.ConvertToContiguousBuffer()?;
            // 优先用 2D 接口拿到真实行跨度
            let mut data = vec![0u8; w * h * 3 / 2];
            if let Ok(b2) = b.cast::<IMF2DBuffer>() {
                let mut p: *mut u8 = std::ptr::null_mut();
                let mut pitch = 0i32;
                b2.Lock2D(&mut p, &mut pitch)?;
                let pitch = pitch.unsigned_abs() as usize;
                // NV12：Y 平面后面紧跟 UV 平面；硬解时平面高度可能按 16/32 对齐，从缓冲长度推算
                let total = b.GetCurrentLength().unwrap_or(0) as usize;
                let plane_h = if total >= pitch * coded_h * 3 / 2 { (total / pitch) * 2 / 3 } else { coded_h };
                for y in 0..h {
                    std::ptr::copy_nonoverlapping(p.add(y * pitch), data.as_mut_ptr().add(y * w), w);
                }
                for y in 0..h / 2 {
                    std::ptr::copy_nonoverlapping(p.add((plane_h + y) * pitch), data.as_mut_ptr().add(w * h + y * w), w);
                }
                b2.Unlock2D()?;
            } else {
                let mut p: *mut u8 = std::ptr::null_mut();
                let mut n = 0u32;
                b.Lock(&mut p, None, Some(&mut n))?;
                let n = (n as usize).min(data.len());
                std::ptr::copy_nonoverlapping(p, data.as_mut_ptr(), n);
                b.Unlock()?;
            }
            Ok(Some(Nv12Frame { width: w, height: h, data }))
        }
    }
}

impl Drop for MfDecoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}
