//! 声卡低延迟模式（第 36 项语音第 1 条）：直接用 Windows 的 IAudioClient3 以「共享模式最小周期」打开麦克风和扬声器
//! （驱动支持时一般约 3 ms 一次，原来 cpal 打开的是约 10 ms）。驱动不支持更小的周期、格式不是 32 位浮点、或打开失败时返回 Err，
//! 调用方退回原来的方式（cpal），不影响使用。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// 正在运行的低延迟流；drop 即停止
pub struct LlStream {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for LlStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// 打开成功后的信息：(采样率, 声道数, 每次多少帧, 设备名)
pub type Opened = (u32, usize, u32, String);

/// 播放：fill(交错的 f32 缓冲, 声道数) 填满要播的声音
pub fn start_render(name: Option<&str>, fill: impl FnMut(&mut [f32], usize) + Send + 'static, on_error: impl Fn(String) + Send + 'static) -> Result<(LlStream, Opened), String> {
    start(false, name, Mode::Render(Box::new(fill)), Box::new(on_error))
}

/// 录音：on_data(交错的 f32, 声道数)
pub fn start_capture(name: Option<&str>, on_data: impl FnMut(&[f32], usize) + Send + 'static, on_error: impl Fn(String) + Send + 'static) -> Result<(LlStream, Opened), String> {
    start(true, name, Mode::Capture(Box::new(on_data)), Box::new(on_error))
}

type RenderFn = Box<dyn FnMut(&mut [f32], usize) + Send>;
type CaptureFn = Box<dyn FnMut(&[f32], usize) + Send>;

enum Mode {
    Render(RenderFn),
    Capture(CaptureFn),
}

fn start(input: bool, name: Option<&str>, mode: Mode, on_error: Box<dyn Fn(String) + Send>) -> Result<(LlStream, Opened), String> {
    #[cfg(windows)]
    {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<Result<Opened, String>>();
        let (s, name) = (stop.clone(), name.map(str::to_string));
        let thread = std::thread::Builder::new()
            .name(if input { "ll-capture".into() } else { "ll-render".into() })
            .spawn(move || {
                crate::audio::boost_audio_thread();
                win::run(input, name.as_deref(), mode, s, tx, on_error);
            })
            .map_err(|e| e.to_string())?;
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(o)) => Ok((LlStream { stop, thread: Some(thread) }, o)),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err("打开超时".into())
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (input, name, mode, on_error);
        Err("只支持 Windows".into())
    }
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::sync::mpsc::Sender;
    use windows::core::{Interface, GUID};
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::Media::Audio::*;
    use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
    use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ};
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

    fn find(en: &IMMDeviceEnumerator, input: bool, name: Option<&str>) -> windows::core::Result<IMMDevice> {
        let flow = if input { eCapture } else { eRender };
        if let Some(name) = name {
            unsafe {
                let list = en.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
                for i in 0..list.GetCount()? {
                    let d = list.Item(i)?;
                    let Ok(store) = d.OpenPropertyStore(STGM_READ) else { continue };
                    let Ok(v) = store.GetValue(&PKEY_Device_FriendlyName) else { continue };
                    let n = v.Anonymous.Anonymous.Anonymous.pwszVal.to_string().unwrap_or_default();
                    if n == name {
                        return Ok(d);
                    }
                }
            }
        }
        unsafe { en.GetDefaultAudioEndpoint(flow, eConsole) }
    }

    fn is_float32(f: &WAVEFORMATEX) -> bool {
        if f.wBitsPerSample != 32 {
            return false;
        }
        if f.wFormatTag as u32 == WAVE_FORMAT_IEEE_FLOAT {
            return true;
        }
        if f.wFormatTag as u32 == WAVE_FORMAT_EXTENSIBLE && f.cbSize >= 22 {
            let ext = unsafe { &*(f as *const WAVEFORMATEX as *const WAVEFORMATEXTENSIBLE) };
            let sub: GUID = unsafe { std::ptr::addr_of!(ext.SubFormat).read_unaligned() };
            return sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
        }
        false
    }

    pub fn run(input: bool, name: Option<&str>, mut mode: Mode, stop: Arc<AtomicBool>, ready: Sender<Result<Opened, String>>, on_error: Box<dyn Fn(String) + Send>) {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let opened = (|| -> Result<(IAudioClient3, *mut WAVEFORMATEX, u32, String), String> {
            let e = |w: &str, err: windows::core::Error| format!("{w}：{}", err.message());
            unsafe {
                let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|x| e("设备列表", x))?;
                let dev = find(&en, input, name).map_err(|x| e("找设备", x))?;
                let dname = dev
                    .OpenPropertyStore(STGM_READ)
                    .and_then(|s| s.GetValue(&PKEY_Device_FriendlyName))
                    .map(|v| v.Anonymous.Anonymous.Anonymous.pwszVal.to_string().unwrap_or_default())
                    .unwrap_or_default();
                let client: IAudioClient3 = dev.Activate(CLSCTX_ALL, None).map_err(|x| e("IAudioClient3", x))?;
                let fmt = client.GetMixFormat().map_err(|x| e("格式", x))?;
                if !is_float32(&*fmt) {
                    CoTaskMemFree(Some(fmt as _));
                    return Err("声卡格式不是 32 位浮点".into());
                }
                let (mut def, mut fun, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
                client.GetSharedModeEnginePeriod(fmt, &mut def, &mut fun, &mut min, &mut max).map_err(|x| e("查询周期", x))?;
                if min == 0 || min >= def {
                    CoTaskMemFree(Some(fmt as _));
                    return Err(format!("驱动不支持更小的周期（默认 {def} 帧）"));
                }
                client.InitializeSharedAudioStream(AUDCLNT_STREAMFLAGS_EVENTCALLBACK, min, fmt, None).map_err(|x| e("打开", x))?;
                Ok((client, fmt, min, dname))
            }
        })();
        let (client, fmt, period, dname) = match opened {
            Ok(x) => x,
            Err(err) => {
                let _ = ready.send(Err(err));
                return;
            }
        };
        let (rate, ch) = unsafe { ((*fmt).nSamplesPerSec, (*fmt).nChannels.max(1) as usize) };
        unsafe { CoTaskMemFree(Some(fmt as _)) };
        let started = unsafe {
            (|| -> windows::core::Result<(windows::Win32::Foundation::HANDLE, Option<IAudioRenderClient>, Option<IAudioCaptureClient>, u32)> {
                let ev = CreateEventW(None, false, false, None)?;
                client.SetEventHandle(ev)?;
                let size = client.GetBufferSize()?;
                let (r, c) = if input {
                    (None, Some(client.GetService::<IAudioCaptureClient>()?))
                } else {
                    let r: IAudioRenderClient = client.GetService()?;
                    // 先填一段静音，避免一开始就欠载
                    let _ = r.GetBuffer(size).and_then(|_| r.ReleaseBuffer(size, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32));
                    (Some(r), None)
                };
                client.Start()?;
                Ok((ev, r, c, size))
            })()
        };
        let (ev, render, capture, size) = match started {
            Ok(x) => x,
            Err(err) => {
                let _ = ready.send(Err(format!("启动：{}", err.message())));
                return;
            }
        };
        let _ = ready.send(Ok((rate, ch, period, dname)));
        let mut scratch: Vec<f32> = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            if unsafe { WaitForSingleObject(ev, 200) } != WAIT_OBJECT_0 {
                continue;
            }
            let r = unsafe {
                (|| -> windows::core::Result<()> {
                    match &mut mode {
                        Mode::Render(fill) => {
                            let render = render.as_ref().unwrap();
                            let n = size - client.GetCurrentPadding()?;
                            if n == 0 {
                                return Ok(());
                            }
                            let p = render.GetBuffer(n)? as *mut f32;
                            let out = std::slice::from_raw_parts_mut(p, n as usize * ch);
                            fill(out, ch);
                            render.ReleaseBuffer(n, 0)?;
                        }
                        Mode::Capture(on_data) => {
                            let cap = capture.as_ref().unwrap();
                            while cap.GetNextPacketSize()? > 0 {
                                let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                                cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                                let len = frames as usize * ch;
                                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                                    scratch.clear();
                                    scratch.resize(len, 0.0);
                                    on_data(&scratch, ch);
                                } else {
                                    on_data(std::slice::from_raw_parts(data as *const f32, len), ch);
                                }
                                cap.ReleaseBuffer(frames)?;
                            }
                        }
                    }
                    Ok(())
                })()
            };
            if let Err(err) = r {
                // 设备拔了 / 被独占：交给上层重开（会退回 cpal）
                on_error(err.message().to_string());
                break;
            }
        }
        unsafe {
            let _ = client.Stop();
            let _ = CloseHandle(ev);
        }
        let _ = client.as_raw();
    }
}

#[cfg(test)]
mod tests {
    /// 本机：试着用低延迟模式打开默认扬声器和麦克风，跑 300 ms，看回调周期（驱动不支持时打印原因，不算失败）
    #[test]
    #[ignore]
    fn open_default_devices() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        match super::start_render(None, move |b, _| {
            c.fetch_add(1, Ordering::Relaxed);
            b.fill(0.0);
        }, |e| println!("出错：{e}"))
        {
            Ok((s, o)) => {
                std::thread::sleep(std::time::Duration::from_millis(300));
                drop(s);
                println!("扬声器：{o:?}，300 ms 回调 {} 次（每次 {:.1} ms）", calls.load(Ordering::Relaxed), o.2 as f32 * 1000.0 / o.0 as f32);
            }
            Err(e) => println!("扬声器不能用低延迟模式：{e}"),
        }
        let frames = Arc::new(AtomicU32::new(0));
        let f = frames.clone();
        match super::start_capture(None, move |d, ch| {
            f.fetch_add((d.len() / ch) as u32, Ordering::Relaxed);
        }, |e| println!("出错：{e}"))
        {
            Ok((s, o)) => {
                std::thread::sleep(std::time::Duration::from_millis(300));
                drop(s);
                println!("麦克风：{o:?}，300 ms 收到 {} 帧", frames.load(Ordering::Relaxed));
            }
            Err(e) => println!("麦克风不能用低延迟模式：{e}"),
        }
    }
}
