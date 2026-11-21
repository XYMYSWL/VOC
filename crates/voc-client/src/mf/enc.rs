//! Media Foundation 硬件视频编码（NVENC / AMF / QSV 通过各厂商的 MFT）。
//! 硬件 MFT 是异步的：按事件驱动——收到 NeedInput 才能喂帧，收到 HaveOutput 才能取码流。

use std::mem::ManuallyDrop;
use windows::core::*;
use windows::Win32::Foundation::{E_FAIL, HMODULE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Variant::VARIANT;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    /// 协议里的编号（见 voc_proto::udp::CODEC_*）
    pub fn id(self) -> u8 {
        match self {
            Codec::H264 => voc_proto::udp::CODEC_H264,
            Codec::Hevc => voc_proto::udp::CODEC_HEVC,
            Codec::Av1 => voc_proto::udp::CODEC_AV1,
        }
    }

    pub fn from_id(id: u8) -> Codec {
        match id {
            voc_proto::udp::CODEC_HEVC => Codec::Hevc,
            voc_proto::udp::CODEC_AV1 => Codec::Av1,
            _ => Codec::H264,
        }
    }

    /// 能力上报用的名字
    pub fn key(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
            Codec::Av1 => "av1",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
            Codec::Av1 => "AV1",
        }
    }

    /// 同等画质下相对 H.264 的码率系数（越省带宽越小）
    pub fn bitrate_factor(self) -> f32 {
        match self {
            Codec::H264 => 1.0,
            Codec::Hevc => 0.75,
            Codec::Av1 => 0.6,
        }
    }

    pub fn subtype(self) -> GUID {
        match self {
            Codec::H264 => MFVideoFormat_H264,
            Codec::Hevc => MFVideoFormat_HEVC,
            Codec::Av1 => MFVideoFormat_AV1,
        }
    }
}

/// 一个可用的硬件编码器
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HwEncoderInfo {
    pub name: String,
    pub codec: Codec,
    /// PCI 厂商号（0x10DE NVIDIA / 0x1002 AMD / 0x8086 Intel）
    pub vendor: u32,
    /// 对应显卡的名字
    pub gpu: String,
}

pub fn mf_init() -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL)
    }
}

fn enum_encoders(codec: Codec) -> Result<Vec<IMFActivate>> {
    let it = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let ot = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: codec.subtype() };
    let mut pp: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut n = 0u32;
    let mut out = Vec::new();
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&it),
            Some(&ot),
            &mut pp,
            &mut n,
        )?;
        for i in 0..n as usize {
            if let Some(a) = (*pp.add(i)).take() {
                out.push(a);
            }
        }
        if !pp.is_null() {
            CoTaskMemFree(Some(pp as _));
        }
    }
    Ok(out)
}

fn friendly_name(a: &IMFActivate) -> String {
    unsafe {
        let mut name = PWSTR::null();
        let mut len = 0u32;
        if a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len).is_ok() && !name.is_null() {
            let s = name.to_string().unwrap_or_default();
            CoTaskMemFree(Some(name.0 as _));
            return s;
        }
    }
    "未知编码器".into()
}

/// 列出所有硬件编码器（按编解码类型，去重同名）
pub fn list_hw_encoders() -> Vec<HwEncoderInfo> {
    let mut v = Vec::new();
    for codec in [Codec::Av1, Codec::Hevc, Codec::H264] {
        if let Ok(list) = enum_encoders(codec) {
            let mut seen = Vec::new();
            for a in list.iter() {
                let name = friendly_name(a);
                if !seen.contains(&name) {
                    seen.push(name.clone());
                    let vendor = vendor_id(a).unwrap_or(0);
                    let gpu = adapter_name(vendor).unwrap_or_else(|| name.clone());
                    v.push(HwEncoderInfo { name, codec, vendor, gpu });
                }
            }
        }
    }
    v
}

/// 某厂商的第一块显卡的名字
fn adapter_name(vendor: u32) -> Option<String> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut i = 0;
        while let Ok(a) = factory.EnumAdapters1(i) {
            let d = a.GetDesc1().ok()?;
            if d.VendorId == vendor {
                return Some(String::from_utf16_lossy(&d.Description).trim_end_matches('\0').trim().to_string());
            }
            i += 1;
        }
        None
    }
}

/// 编码器 MFT 标注的硬件厂商（"VEN_10DE" 之类）→ PCI 厂商号
fn vendor_id(a: &IMFActivate) -> Option<u32> {
    unsafe {
        let mut s = PWSTR::null();
        let mut len = 0u32;
        a.GetAllocatedString(&MFT_ENUM_HARDWARE_VENDOR_ID_Attribute, &mut s, &mut len).ok()?;
        let v = s.to_string().ok()?;
        CoTaskMemFree(Some(s.0 as _));
        u32::from_str_radix(v.trim_start_matches("VEN_"), 16).ok()
    }
}

/// 在指定厂商的显卡上创建 D3D11 设备，包成 MF 的 DXGI 设备管理器（AMD 等编码器必须要）
fn d3d_manager(vendor: u32) -> Result<(IMFDXGIDeviceManager, ID3D11Device)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut i = 0;
        let adapter = loop {
            let a = factory.EnumAdapters1(i)?;
            if a.GetDesc1()?.VendorId == vendor {
                break a;
            }
            i += 1;
        };
        let mut dev = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut dev),
            None,
            None,
        )?;
        let dev = dev.ok_or_else(|| Error::from(E_FAIL))?;
        if let Ok(mt) = dev.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
        let mut token = 0u32;
        let mut mgr = None;
        MFCreateDXGIDeviceManager(&mut token, &mut mgr)?;
        let mgr = mgr.ok_or_else(|| Error::from(E_FAIL))?;
        mgr.ResetDevice(&dev, token)?;
        Ok((mgr, dev))
    }
}

/// 任意一块独立显卡（优先 NVIDIA/AMD/Intel 的第一块非软件显卡）上的 D3D11 设备管理器，用于硬解
pub fn d3d_manager_any() -> Result<(IMFDXGIDeviceManager, ID3D11Device)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut i = 0;
        while let Ok(a) = factory.EnumAdapters1(i) {
            let d = a.GetDesc1()?;
            if [0x10DE, 0x1002, 0x8086].contains(&d.VendorId) {
                return d3d_manager(d.VendorId);
            }
            i += 1;
        }
        Err(Error::from(E_FAIL))
    }
}

/// 喂给编码器的一帧：内存里的 NV12，或显卡上的 NV12 纹理
enum Input<'a> {
    Mem(&'a [u8]),
    Tex(&'a ID3D11Texture2D),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RateMode {
    Cbr,
    PeakVbr,
    Quality,
}

/// 码率控制方式（默认见下；测试时 VOC_TEST_RC=cbr / vbr / quality 切换）
pub fn rate_mode() -> RateMode {
    match std::env::var("VOC_TEST_RC").as_deref() {
        Ok("vbr") => RateMode::PeakVbr,
        Ok("quality") => RateMode::Quality,
        _ => RateMode::Cbr,
    }
}

pub struct HwEncoder {
    mft: IMFTransform,
    events: IMFMediaEventGenerator,
    codec_api: Option<ICodecAPI>,
    width: u32,
    height: u32,
    fps: u32,
    frame_no: i64,
    need_input: u32,
    pub name: String,
    /// 编码器接受了「逐行刷新」（画面按条带逐帧刷新，不再定期发整张关键帧）
    pub intra_refresh: bool,
    /// 编码器接受了长期参考帧（2 个槽）
    pub ltr: bool,
    _d3d: Option<(IMFDXGIDeviceManager, ID3D11Device)>,
}

// MFT 只在编码线程里使用
unsafe impl Send for HwEncoder {}

fn set_u64(mt: &IMFMediaType, key: &GUID, hi: u32, lo: u32) -> Result<()> {
    unsafe { mt.SetUINT64(key, ((hi as u64) << 32) | lo as u64) }
}

impl HwEncoder {
    pub fn new(info: &HwEncoderInfo, width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        // 先不绑显卡直接试（NVIDIA 可以）；不行再绑定对应厂商的 D3D11 设备重试（AMD 需要）
        match Self::open(info, width, height, fps, bitrate, None) {
            Ok(e) => Ok(e),
            Err(_) => {
                let list = enum_encoders(info.codec)?;
                let a = list.into_iter().find(|a| friendly_name(a) == info.name).ok_or_else(|| Error::from(E_FAIL))?;
                let d3d = d3d_manager(vendor_id(&a).ok_or_else(|| Error::from(E_FAIL))?)?;
                Self::open(info, width, height, fps, bitrate, Some(d3d))
            }
        }
    }

    /// 绑定到指定的 D3D11 设备（显卡直通：输入直接是这个设备上的 NV12 纹理）
    pub fn new_on_device(
        info: &HwEncoderInfo,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        mgr: IMFDXGIDeviceManager,
        dev: ID3D11Device,
    ) -> Result<Self> {
        Self::open(info, width, height, fps, bitrate, Some((mgr, dev)))
    }

    /// 运行中调整码率（网络拥堵时降低）；可变码率时调的是上限
    pub fn set_bitrate(&self, bitrate: u32) {
        if let Some(api) = &self.codec_api {
            unsafe {
                match rate_mode() {
                    RateMode::Cbr => {
                        let _ = api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bitrate));
                    }
                    RateMode::PeakVbr => {
                        let _ = api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bitrate / 2));
                        let _ = api.SetValue(&CODECAPI_AVEncCommonMaxBitRate, &VARIANT::from(bitrate));
                    }
                    RateMode::Quality => {
                        let _ = api.SetValue(&CODECAPI_AVEncCommonMaxBitRate, &VARIANT::from(bitrate));
                    }
                }
            }
        }
    }

    fn open(info: &HwEncoderInfo, width: u32, height: u32, fps: u32, bitrate: u32, d3d: Option<(IMFDXGIDeviceManager, ID3D11Device)>) -> Result<Self> {
        let list = enum_encoders(info.codec)?;
        let a = list.into_iter().find(|a| friendly_name(a) == info.name).ok_or_else(|| Error::from(E_FAIL))?;
        unsafe {
            let mft: IMFTransform = a.ActivateObject()?;
            let attrs = mft.GetAttributes()?;
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?; // 必须先解锁异步，才能发其它消息
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            if let Some((mgr, _)) = &d3d {
                mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, std::mem::transmute::<*mut std::ffi::c_void, usize>(mgr.as_raw()))?;
            }

            let codec_api: Option<ICodecAPI> = mft.cast().ok();
            if let Some(api) = &codec_api {
                let set = |g: &GUID, v: VARIANT| {
                    let _ = api.SetValue(g, &v);
                };
                set(&CODECAPI_AVLowLatencyMode, VARIANT::from(true));
                // 码率控制（VOC_TEST_RC 可切换做对比）：cbr = 固定码率；vbr = 峰值受限可变码率（平均一半、峰值封顶）；
                // quality = 画质优先、码率封顶
                match rate_mode() {
                    RateMode::Cbr => {
                        set(&CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32));
                        set(&CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate));
                    }
                    RateMode::PeakVbr => {
                        set(&CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32));
                        set(&CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate / 2));
                        set(&CODECAPI_AVEncCommonMaxBitRate, VARIANT::from(bitrate));
                    }
                    RateMode::Quality => {
                        set(&CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_Quality.0 as u32));
                        set(&CODECAPI_AVEncCommonQuality, VARIANT::from(70u32));
                        set(&CODECAPI_AVEncCommonMaxBitRate, VARIANT::from(bitrate));
                        set(&CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate));
                    }
                }
                set(&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
                set(&CODECAPI_AVEncMPVGOPSize, VARIANT::from(fps * 10));
            }
            // 逐行刷新：用 1 秒刷完整个画面，丢包后自愈，也去掉定期整张关键帧的瞬间大流量。
            // 不是所有显卡编码器都支持，支持的才把关键帧间隔拉长
            let intra_refresh = std::env::var_os("VOC_NO_INTRA_REFRESH").is_none()
                && codec_api.as_ref().is_some_and(|api| {
                    api.IsSupported(&CODECAPI_AVEncVideoGradualIntraRefresh).is_ok()
                        && api.SetValue(&CODECAPI_AVEncVideoGradualIntraRefresh, &VARIANT::from(fps)).is_ok()
                });
            if let (true, Some(api)) = (intra_refresh, &codec_api) {
                let _ = api.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(fps * 60));
            }
            // 长期参考帧：2 个槽（低 16 位 = 槽数，高 16 位 = 信任模式 1）。开了以后关键帧只由我们按需强制，
            // 编码器自己不要定期插 IDR（IDR 会清掉长期参考帧，观看者那边对不上）
            let ltr = std::env::var_os("VOC_NO_LTR").is_none()
                && codec_api.as_ref().is_some_and(|api| api.SetValue(&CODECAPI_AVEncVideoLTRBufferControl, &VARIANT::from(0x0001_0002u32)).is_ok());
            if let (true, Some(api)) = (ltr, &codec_api) {
                let _ = api.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(fps * 600));
            }

            // 先设输出类型，再设输入类型（编码器的要求）
            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &info.codec.subtype())?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            set_u64(&out, &MF_MT_FRAME_SIZE, width, height)?;
            set_u64(&out, &MF_MT_FRAME_RATE, fps, 1)?;
            set_u64(&out, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            mft.SetOutputType(0, &out, 0)?;

            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            set_u64(&inp, &MF_MT_FRAME_SIZE, width, height)?;
            set_u64(&inp, &MF_MT_FRAME_RATE, fps, 1)?;
            set_u64(&inp, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            mft.SetInputType(0, &inp, 0)?;

            mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let events: IMFMediaEventGenerator = mft.cast()?;
            Ok(Self { mft, events, codec_api, width, height, fps, frame_no: 0, need_input: 0, name: info.name.clone(), intra_refresh, ltr, _d3d: d3d })
        }
    }

    /// 编码器声明支持某个 ICodecAPI 参数（实测用）
    #[allow(dead_code)]
    pub fn supports(&self, g: &GUID) -> bool {
        self.codec_api.as_ref().is_some_and(|api| unsafe { api.IsSupported(g).is_ok() })
    }

    /// 试着设置某个参数（实测用）：Ok = 编码器接受
    #[allow(dead_code)]
    pub fn try_set(&self, g: &GUID, v: u32) -> bool {
        self.codec_api.as_ref().is_some_and(|api| unsafe { api.SetValue(g, &VARIANT::from(v)).is_ok() })
    }

    /// 下一帧：mark = 存进第几个长期参考槽；use_slot = 只参考这个槽里的长期参考帧（恢复帧）
    pub fn set_ltr(&self, mark: Option<u8>, use_slot: Option<u8>) {
        let Some(api) = &self.codec_api else { return };
        unsafe {
            if let Some(m) = mark {
                let _ = api.SetValue(&CODECAPI_AVEncVideoMarkLTRFrame, &VARIANT::from(m as u32));
            }
            if let Some(u) = use_slot {
                let _ = api.SetValue(&CODECAPI_AVEncVideoUseLTRFrame, &VARIANT::from(1u32 << u));
            }
        }
    }

    pub fn force_keyframe(&self) {
        if let Some(api) = &self.codec_api {
            unsafe {
                let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
            }
        }
    }

    /// 编码一帧 NV12（w×h + w×h/2）。返回这次拿到的所有码流包 (数据, 是否关键帧)。
    pub fn encode(&mut self, nv12: &[u8]) -> Result<Vec<(Vec<u8>, bool)>> {
        self.encode_input(Input::Mem(nv12))
    }

    /// 编码一张 NV12 纹理（显卡直通；纹理必须在 new_on_device 给的设备上）
    pub fn encode_texture(&mut self, tex: &ID3D11Texture2D) -> Result<Vec<(Vec<u8>, bool)>> {
        self.encode_input(Input::Tex(tex))
    }

    fn encode_input(&mut self, input: Input) -> Result<Vec<(Vec<u8>, bool)>> {
        let mut out = Vec::new();
        // 处理事件直到喂进这一帧；顺便把已有输出取走。最多等 ~500 ms 防止卡死。
        let start = std::time::Instant::now();
        loop {
            if self.need_input > 0 {
                self.feed(&input)?;
                self.need_input -= 1;
                break;
            }
            let ev = unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) };
            match ev {
                Ok(ev) => self.handle_event(&ev, &mut out)?,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if start.elapsed() > std::time::Duration::from_millis(500) {
                        return Err(Error::from(E_FAIL));
                    }
                    std::thread::sleep(std::time::Duration::from_micros(300));
                }
                Err(e) => return Err(e),
            }
        }
        // 把立刻可取的输出拿走（不阻塞）
        loop {
            match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(ev) => self.handle_event(&ev, &mut out)?,
                Err(_) => break,
            }
        }
        Ok(out)
    }

    fn handle_event(&mut self, ev: &IMFMediaEvent, out: &mut Vec<(Vec<u8>, bool)>) -> Result<()> {
        let t = unsafe { ev.GetType()? };
        if t == METransformNeedInput.0 as u32 {
            self.need_input += 1;
        } else if t == METransformHaveOutput.0 as u32 {
            if let Some(pkt) = self.pull()? {
                out.push(pkt);
            }
        }
        Ok(())
    }

    fn feed(&mut self, input: &Input) -> Result<()> {
        unsafe {
            let buf = match input {
                Input::Mem(nv12) => {
                    let len = (self.width * self.height * 3 / 2) as usize;
                    let buf = MFCreateMemoryBuffer(len as u32)?;
                    let mut p: *mut u8 = std::ptr::null_mut();
                    buf.Lock(&mut p, None, None)?;
                    std::ptr::copy_nonoverlapping(nv12.as_ptr(), p, len.min(nv12.len()));
                    buf.Unlock()?;
                    buf.SetCurrentLength(len as u32)?;
                    buf
                }
                Input::Tex(t) => MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, *t, 0, false)?,
            };
            let s = MFCreateSample()?;
            s.AddBuffer(&buf)?;
            let dur = 10_000_000i64 / self.fps as i64;
            s.SetSampleTime(self.frame_no * dur)?;
            s.SetSampleDuration(dur)?;
            self.frame_no += 1;
            self.mft.ProcessInput(0, &s, 0)
        }
    }

    fn pull(&mut self) -> Result<Option<(Vec<u8>, bool)>> {
        unsafe {
            let info = self.mft.GetOutputStreamInfo(0)?;
            let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;
            let sample = if provides {
                None
            } else {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(info.cbSize.max(1 << 20))?)?;
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
            if let Err(e) = r {
                if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                    return Ok(None);
                }
                return Err(e);
            }
            let Some(sample) = sample else { return Ok(None) };
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let b = sample.ConvertToContiguousBuffer()?;
            let mut p: *mut u8 = std::ptr::null_mut();
            let mut n = 0u32;
            b.Lock(&mut p, None, Some(&mut n))?;
            let data = std::slice::from_raw_parts(p, n as usize).to_vec();
            b.Unlock()?;
            Ok(Some((data, key)))
        }
    }
}

impl Drop for HwEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            if let Ok(a) = self.mft.cast::<IMFShutdown>() {
                let _ = a.Shutdown();
            }
        }
    }
}
