//! 按程序采集声音（Windows 进程回环录音，WASAPI Process Loopback）。
//! - 伴奏：只录某个程序（含它的子进程）发出的声音；
//! - 屏幕共享声音：录除了 VOC 自己以外的全部声音（队友的说话声、提示音不会被录回去）。
//! 输出 48 kHz 单声道，每 20 ms 一帧（960 个样本）。需要 Windows 10 build 20348 以上（实际上是 Windows 11）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// 采集哪些声音
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// 只要这个进程（含子进程）
    Include(u32),
    /// 除了这个进程（含子进程）以外的全部
    Exclude(u32),
}

pub const RATE: u32 = 48_000;
pub const FRAME: usize = 960;

/// 正在采集；drop 即停止
pub struct Capture {
    stop: Arc<AtomicBool>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// 系统是否支持按程序录音
pub fn supported() -> bool {
    os_build() >= 20348
}

/// Windows 内部版本号（例如 22631）
pub fn os_build() -> u32 {
    #[cfg(windows)]
    {
        use windows::core::HSTRING;
        use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
        let mut buf = [0u16; 32];
        let mut len = (buf.len() * 2) as u32;
        let ok = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                &HSTRING::from("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
                &HSTRING::from("CurrentBuildNumber"),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr() as *mut _),
                Some(&mut len),
            )
        }
        .is_ok();
        if ok {
            let n = (len as usize / 2).saturating_sub(1);
            return String::from_utf16_lossy(&buf[..n]).trim().parse().unwrap_or(0);
        }
    }
    0
}

/// 开始采集；每凑满 20 ms 调一次 on_frame（在采集线程里）
pub fn start(target: Target, on_frame: impl FnMut(&[f32]) + Send + 'static) -> Result<Capture, String> {
    if !supported() {
        return Err("按程序录音需要 Windows 11（Windows 10 build 20348 以上）".into());
    }
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let s = stop.clone();
    std::thread::Builder::new()
        .name("loopback".into())
        .spawn(move || {
            crate::audio::boost_audio_thread();
            #[cfg(windows)]
            win::run(target, s, ready_tx, on_frame);
        })
        .map_err(|e| e.to_string())?;
    match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(Capture { stop }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("打开录音超时".into()),
    }
}

/// 设置某个程序在 Windows 音量合成器里的音量（0–1）。只影响本机扬声器里听到的大小。
pub fn set_app_volume(pid: u32, volume: f32) -> bool {
    #[cfg(windows)]
    {
        win::set_app_volume(pid, volume).unwrap_or(false)
    }
    #[cfg(not(windows))]
    false
}

/// 读某个程序在 Windows 音量合成器里的音量（0–1）
pub fn app_volume(pid: u32) -> Option<f32> {
    #[cfg(windows)]
    {
        win::app_volume(pid).ok().flatten()
    }
    #[cfg(not(windows))]
    None
}

/// 正在发声 / 用过声音的程序：(进程 id, 程序名)，不含 VOC 自己
pub fn audio_apps() -> Vec<(u32, String)> {
    #[cfg(windows)]
    {
        win::audio_apps().unwrap_or_default()
    }
    #[cfg(not(windows))]
    Vec::new()
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::sync::mpsc::Sender;
    use windows::core::{implement, Interface, Ref, Result as WinResult, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
    use windows::Win32::System::Com::{CoInitializeEx, IAgileObject, IAgileObject_Impl, BLOB, CLSCTX_ALL, COINIT_MULTITHREADED};
    use windows::Win32::System::Threading::{
        CreateEventW, OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::System::Variant::VT_BLOB;

    #[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
    struct Done(Sender<()>);

    impl IActivateAudioInterfaceCompletionHandler_Impl for Done_Impl {
        fn ActivateCompleted(&self, _op: Ref<IActivateAudioInterfaceAsyncOperation>) -> WinResult<()> {
            let _ = self.0.send(());
            Ok(())
        }
    }
    impl IAgileObject_Impl for Done_Impl {}

    fn open(target: Target) -> std::result::Result<IAudioClient, String> {
        let err = |e: windows::core::Error| format!("打开录音失败：{}", e.message());
        let (pid, mode) = match target {
            Target::Include(p) => (p, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE),
            Target::Exclude(p) => (p, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE),
        };
        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS { TargetProcessId: pid, ProcessLoopbackMode: mode },
            },
        };
        // 参数在栈上，不能让 PROPVARIANT 析构时去释放它
        let pv = std::mem::ManuallyDrop::new(PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_BLOB,
                    Anonymous: PROPVARIANT_0_0_0 {
                        blob: BLOB {
                            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                            pBlobData: &mut params as *mut _ as *mut u8,
                        },
                    },
                    ..Default::default()
                }),
            },
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let handler: IActivateAudioInterfaceCompletionHandler = Done(tx).into();
        unsafe {
            let op = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*pv), &handler).map_err(err)?;
            rx.recv_timeout(std::time::Duration::from_secs(5)).map_err(|_| "打开录音超时".to_string())?;
            let mut hr = windows::core::HRESULT(0);
            let mut unk = None;
            op.GetActivateResult(&mut hr, &mut unk).map_err(err)?;
            hr.ok().map_err(err)?;
            let client: IAudioClient = unk.ok_or("打开录音失败")?.cast().map_err(err)?;
            // 进程回环不支持查询混音格式，要自己指定：48 kHz 立体声 16 位
            let fmt = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_PCM as u16,
                nChannels: 2,
                nSamplesPerSec: RATE,
                wBitsPerSample: 16,
                nBlockAlign: 4,
                nAvgBytesPerSec: RATE * 4,
                cbSize: 0,
            };
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                    200_000, // 20 ms（100 ns 单位）
                    0,
                    &fmt,
                    None,
                )
                .map_err(err)?;
            Ok(client)
        }
    }

    pub fn run(target: Target, stop: Arc<AtomicBool>, ready: Sender<std::result::Result<(), String>>, mut on_frame: impl FnMut(&[f32])) {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let client = match open(target) {
            Ok(c) => c,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let r = unsafe {
            (|| -> WinResult<(IAudioCaptureClient, windows::Win32::Foundation::HANDLE)> {
                let ev = CreateEventW(None, false, false, None)?;
                client.SetEventHandle(ev)?;
                let cap: IAudioCaptureClient = client.GetService()?;
                client.Start()?;
                Ok((cap, ev))
            })()
        };
        let (cap, ev) = match r {
            Ok(x) => x,
            Err(e) => {
                let _ = ready.send(Err(format!("开始录音失败：{}", e.message())));
                return;
            }
        };
        let _ = ready.send(Ok(()));
        let mut acc: Vec<f32> = Vec::with_capacity(FRAME * 2);
        while !stop.load(Ordering::Relaxed) {
            if unsafe { WaitForSingleObject(ev, 100) } != WAIT_OBJECT_0 {
                continue;
            }
            loop {
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                let got = unsafe { cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None) };
                if got.is_err() || frames == 0 {
                    break;
                }
                let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
                if silent || data.is_null() {
                    acc.extend(std::iter::repeat_n(0.0, frames as usize));
                } else {
                    let s = unsafe { std::slice::from_raw_parts(data as *const i16, frames as usize * 2) };
                    acc.extend(s.chunks(2).map(|lr| (lr[0] as f32 + lr[1] as f32) / 65536.0));
                }
                unsafe {
                    let _ = cap.ReleaseBuffer(frames);
                }
                while acc.len() >= FRAME {
                    on_frame(&acc[..FRAME]);
                    acc.drain(..FRAME);
                }
            }
        }
        unsafe {
            let _ = client.Stop();
            let _ = CloseHandle(ev);
        }
    }

    /// 枚举默认播放设备上的音频会话，得到用过声音的程序
    pub fn audio_apps() -> WinResult<Vec<(u32, String)>> {
        use windows::Win32::System::Com::{CoCreateInstance, COINIT_APARTMENTTHREADED};
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let mgr: IAudioSessionManager2 = dev.Activate(CLSCTX_ALL, None)?;
            let list = mgr.GetSessionEnumerator()?;
            let me = std::process::id();
            let mut out: Vec<(u32, String)> = Vec::new();
            for i in 0..list.GetCount()? {
                let Ok(c) = list.GetSession(i) else { continue };
                let Ok(c2) = c.cast::<IAudioSessionControl2>() else { continue };
                let Ok(pid) = c2.GetProcessId() else { continue };
                if pid == 0 || pid == me || out.iter().any(|(p, _)| *p == pid) {
                    continue;
                }
                if let Some(name) = process_name(pid) {
                    out.push((pid, name));
                }
            }
            out.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
            Ok(out)
        }
    }

    pub fn set_app_volume(pid: u32, volume: f32) -> WinResult<bool> {
        use windows::Win32::System::Com::{CoCreateInstance, COINIT_APARTMENTTHREADED};
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let mgr: IAudioSessionManager2 = dev.Activate(CLSCTX_ALL, None)?;
            let list = mgr.GetSessionEnumerator()?;
            let mut done = false;
            for i in 0..list.GetCount()? {
                let Ok(c) = list.GetSession(i) else { continue };
                let Ok(c2) = c.cast::<IAudioSessionControl2>() else { continue };
                if c2.GetProcessId().ok() != Some(pid) {
                    continue;
                }
                if let Ok(v) = c.cast::<ISimpleAudioVolume>() {
                    done |= v.SetMasterVolume(volume.clamp(0.0, 1.0), std::ptr::null()).is_ok();
                }
            }
            Ok(done)
        }
    }

    pub fn app_volume(pid: u32) -> WinResult<Option<f32>> {
        use windows::Win32::System::Com::{CoCreateInstance, COINIT_APARTMENTTHREADED};
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let mgr: IAudioSessionManager2 = dev.Activate(CLSCTX_ALL, None)?;
            let list = mgr.GetSessionEnumerator()?;
            for i in 0..list.GetCount()? {
                let Ok(c) = list.GetSession(i) else { continue };
                let Ok(c2) = c.cast::<IAudioSessionControl2>() else { continue };
                if c2.GetProcessId().ok() == Some(pid) {
                    if let Ok(v) = c.cast::<ISimpleAudioVolume>() {
                        return Ok(v.GetMasterVolume().ok());
                    }
                }
            }
            Ok(None)
        }
    }

    fn process_name(pid: u32) -> Option<String> {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = [0u16; 520];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
            let _ = CloseHandle(h);
            if !ok {
                return None;
            }
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            let file = path.rsplit('\\').next().unwrap_or(&path).to_string();
            Some(file.trim_end_matches(".exe").trim_end_matches(".EXE").to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 真机：用 PowerShell 播一段 440 Hz 的声音，只录这个进程，应该录到声音；
    /// 再「排除这个进程」录，就不该有它的声音（期间别的程序别出声）。
    /// cargo test -p voc-client loopback_real -- --ignored --nocapture
    #[test]
    #[ignore]
    fn loopback_real_capture() {
        assert!(supported(), "需要 Windows 11");
        // 生成 3 秒 440 Hz 的 WAV
        let dir = std::env::temp_dir().join("voc-loopback-test");
        let _ = std::fs::create_dir_all(&dir);
        let wav = dir.join("tone.wav");
        let n = RATE as usize * 5;
        let mut w = Vec::new();
        let data_len = (n * 2) as u32;
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data_len).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&RATE.to_le_bytes());
        w.extend_from_slice(&(RATE * 2).to_le_bytes());
        w.extend_from_slice(&2u16.to_le_bytes());
        w.extend_from_slice(&16u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..n {
            let v = ((i as f32 * 440.0 * std::f32::consts::TAU / RATE as f32).sin() * 3000.0) as i16;
            w.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(&wav, w).unwrap();
        let mut player = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &format!("(New-Object Media.SoundPlayer '{}').PlaySync()", wav.display())])
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let pid = player.id();
        let measure = |t: Target| {
            let peak = Arc::new(Mutex::new(0.0f32));
            let p = peak.clone();
            let cap = start(t, move |f| {
                let rms = (f.iter().map(|x| x * x).sum::<f32>() / f.len() as f32).sqrt();
                let mut g = p.lock().unwrap();
                *g = g.max(rms);
            })
            .expect("start capture");
            std::thread::sleep(std::time::Duration::from_millis(600));
            drop(cap);
            let v = *peak.lock().unwrap();
            v
        };
        let inc = measure(Target::Include(pid));
        let exc = measure(Target::Exclude(pid));
        // 把播放器在音量合成器里调到 10%：录到的声音变不变？（决定能不能用它做「本地监听音量」）
        let set = set_app_volume(pid, 0.1);
        let inc_low = measure(Target::Include(pid));
        println!("音量合成器调到 10%（设置成功：{set}）后录到：{inc_low:.4}");
        let _ = player.wait();
        println!("只录播放器：{inc:.4}，排除播放器：{exc:.4}");
        assert!(inc > 0.01, "should capture the player's tone");
        assert!(exc < inc / 4.0, "excluded process leaked into capture");
    }
}
