use crate::audio::{self, AudioStreams, Chime, Mixer, Settings, LOOPBACK_ID, SFX_ID};
use crate::denoise::{self, DenoiseMode, DenoiseSlot};
use crate::e2e::{E2e, Trust};
use crate::net::{self, Conn, NetEvent};
use crate::video::{self, ShareConfig, Sharer, View};
use crate::voice::{self, PeerQuality, VoiceState};
use eframe::egui;
use std::collections::HashMap;
use voc_proto::e2e::Identity;
use voc_proto::{ChannelInfo, ClientMsg, E2eEntry, ServerMsg, UserInfo};
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod ui;

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Persisted {
    server: String,
    name: String,
    input_device: Option<String>,
    output_device: Option<String>,
    vad_threshold_db: f32,
    input_gain: f32,
    output_volume: f32,
    denoise: DenoiseMode,
    /// Krisp 等外部降噪插件 DLL
    denoise_plugin: String,
    /// 插件的模型文件（Krisp 的 .kef 等）
    denoise_model: String,
    /// 保存的服务器列表
    servers: Vec<SavedServer>,
    /// 是否在本机记住授权码（明文保存在 VOC 的配置文件里）
    remember_code: bool,
    code: String,
    /// P2P 直连语音（省服务器带宽，但会向同频道成员暴露自己的 IP）
    p2p: bool,
    /// 屏幕共享设置（分辨率 / 帧率 / 显示器）
    share: ShareConfig,
    /// P2P 直连可用的上行带宽（Mbps）：超出部分走服务器中转
    p2p_budget_mbps: u32,
    /// 允许帮队友中转语音（成员中继）
    relay: bool,
    /// 关闭窗口时缩到托盘（语音不断）
    close_to_tray: bool,
    /// 回声消除（外放时用）
    aec: bool,
    /// 屏幕共享时同时共享电脑声音
    share_audio: bool,
    /// 按键说话：虚拟键码，0 = 语音激活
    ptt_vk: u32,
    /// 低带宽模式（手机热点、流量少的网络）
    low_bandwidth: bool,
    /// 手动选的线路：服务器地址 → 昵称 → 选择（没有的按自动）
    routes: HashMap<String, HashMap<String, voice::RoutePref>>,
    /// 上次所在的语音频道（按服务器），开机自启自动登录后回到这里
    last_channel: HashMap<String, u32>,
    /// 观看屏幕共享时用显卡硬解
    hw_decode: bool,
    /// 启动时检查更新
    auto_update: bool,
    /// 进出频道提示音
    sfx: bool,
    sfx_volume: f32,
    /// 端到端加密身份私钥种子（base64；ML-DSA-65）
    identity_seed: String,
    /// 记住的身份：服务器地址 → 昵称 → 身份指纹
    pins: HashMap<String, HashMap<String, String>>,
}

#[derive(Serialize, Deserialize, Clone, Default, PartialEq)]
#[serde(default)]
struct SavedServer {
    label: String,
    addr: String,
    name: String,
    code: String,
}

impl Default for Persisted {
    fn default() -> Self {
        Self {
            server: "127.0.0.1".into(),
            name: String::new(),
            input_device: None,
            output_device: None,
            vad_threshold_db: -45.0,
            input_gain: 1.0,
            output_volume: 1.0,
            denoise: DenoiseMode::Rnnoise,
            denoise_plugin: String::new(),
            denoise_model: String::new(),
            servers: Vec::new(),
            remember_code: true,
            code: String::new(),
            p2p: true,
            share: ShareConfig::default(),
            p2p_budget_mbps: 15,
            relay: true,
            close_to_tray: true,
            aec: true,
            share_audio: true,
            ptt_vk: 0,
            low_bandwidth: false,
            routes: HashMap::new(),
            last_channel: HashMap::new(),
            hw_decode: false,
            auto_update: true,
            sfx: true,
            sfx_volume: 0.7,
            identity_seed: String::new(),
            pins: HashMap::new(),
        }
    }
}

enum ChatLine {
    User { from: String, text: String, ts: u64, mine: bool },
    System(String),
    Image(Box<ImageLine>),
}

/// 和某个人的私聊（只在内存里；关闭标签或关掉 VOC 时清除）
struct DmThread {
    /// 对方昵称（重连后 id 会变，按昵称找人）
    peer: String,
    lines: Vec<ChatLine>,
    unread: u32,
}

/// 聊天里的一张图片（只在内存里）
struct ImageLine {
    from: String,
    ts: u64,
    mine: bool,
    /// (发送者, 图片 id)
    key: (u32, u64),
    tex: Option<egui::TextureHandle>,
    /// 发送进度：已发块数（后台线程更新）/ 总块数
    sending: Option<(Arc<std::sync::atomic::AtomicU32>, u32)>,
    /// 接收进度：已收块数 / 总块数
    receiving: Option<(u32, u32)>,
    error: Option<String>,
}

/// 在后台准备一张图片（压缩到 20 MB 以内），准备好后由 logic() 自动发送
pub(crate) fn queue_image(
    tx: &mpsc::Sender<Result<crate::images::Prepared, String>>,
    ctx: &egui::Context,
    job: impl FnOnce() -> Result<crate::images::Prepared, String> + Send + 'static,
) {
    let (tx, ctx) = (tx.clone(), ctx.clone());
    let _ = std::thread::Builder::new().name("image-prepare".into()).spawn(move || {
        let _ = tx.send(job());
        ctx.request_repaint();
    });
}

/// 把图片接收进度 / 结果写到对应的聊天行上
fn apply_image_events(s: &mut Session, events: Vec<crate::images::ImageEvent>, ctx: &egui::Context) {
    use crate::images::ImageEvent as E;
    for ev in events {
        let key = match &ev {
            E::Progress { key, .. } | E::Ready { key, .. } | E::Failed { key, .. } => *key,
        };
        let mut lines = s.chat.iter_mut().rev().chain(s.dms.iter_mut().flat_map(|t| t.lines.iter_mut().rev()));
        let Some(line) = lines.find_map(|l| match l {
            ChatLine::Image(i) if i.key == key => Some(i),
            _ => None,
        }) else {
            continue;
        };
        match ev {
            E::Progress { got, total, .. } => line.receiving = Some((got, total)),
            E::Ready { image, .. } => {
                line.tex = Some(ctx.load_texture(format!("img-{}-{}", key.0, key.1), image, Default::default()));
                line.receiving = None;
            }
            E::Failed { reason, .. } => {
                line.error = Some(reason);
                line.receiving = None;
            }
        }
    }
}

struct Session {
    /// 演示模式下为 None
    conn: Option<Conn>,
    /// 上一次看到的（我所在频道, 同频道其他人），用来判断谁进谁出、播提示音
    last_members: Option<(Option<u32>, Vec<u32>)>,
    events: Receiver<NetEvent>,
    user_id: u32,
    server_name: String,
    channels: Vec<ChannelInfo>,
    users: Vec<UserInfo>,
    chat: Vec<ChatLine>,
    /// 私聊（按对方昵称）
    dms: Vec<DmThread>,
    /// 正在看的私聊；None = 频道聊天
    chat_tab: Option<String>,
    chat_input: String,
    new_channel: String,
    /// 用于记住身份的服务器标识（登录时填的地址）
    server_key: String,
    e2e: Option<Arc<Mutex<E2e>>>,
    /// 服务器支持的新功能（Welcome 里带来）
    features: Vec<String>,
}

impl Session {
    fn send(&self, m: ClientMsg) {
        if let Some(c) = &self.conn {
            c.send(m);
        }
    }

    fn me(&self) -> Option<&UserInfo> {
        self.users.iter().find(|u| u.id == self.user_id)
    }

    fn id_by_name(&self, name: &str) -> Option<u32> {
        self.users.iter().find(|u| u.name == name && u.id != self.user_id).map(|u| u.id)
    }

    /// 和某人的私聊（没有就新建）
    fn dm(&mut self, peer: &str) -> &mut DmThread {
        let i = match self.dms.iter().position(|t| t.peer == peer) {
            Some(i) => i,
            None => {
                self.dms.push(DmThread { peer: peer.to_string(), lines: Vec::new(), unread: 0 });
                self.dms.len() - 1
            }
        };
        &mut self.dms[i]
    }

    /// 收到私聊：没在看这段对话就记一条未读
    fn push_dm(&mut self, peer: &str, line: ChatLine) {
        let viewing = self.chat_tab.as_deref() == Some(peer);
        let t = self.dm(peer);
        t.lines.push(line);
        if !viewing {
            t.unread += 1;
        }
    }

    /// 频道聊天（None）或某段私聊的消息列表
    fn lines_for(&mut self, peer: Option<&str>) -> &mut Vec<ChatLine> {
        match peer {
            Some(p) => &mut self.dm(p).lines,
            None => &mut self.chat,
        }
    }

    fn name_of(&self, id: u32) -> String {
        self.users.iter().find(|u| u.id == id).map(|u| u.name.clone()).unwrap_or_else(|| format!("#{id}"))
    }

    fn on_e2e_entry(&mut self, entry: E2eEntry) {
        let Some(e2e) = &self.e2e else { return };
        let name = entry.name.clone();
        match e2e.lock().unwrap().on_entry(entry) {
            Err(why) => self.chat.push(ChatLine::System(format!("⚠ {why}"))),
            Ok(Trust::Changed) => self.chat.push(ChatLine::System(format!(
                "⚠ {name} 的加密身份和上次不一样（可能换了电脑，也可能有人冒充）。已暂停与其加密通信，请与对方核对安全码后在其名字上右键「信任」。"
            ))),
            Ok(_) => {}
        }
    }
}

type ConnectResult = Result<(Conn, Receiver<NetEvent>), String>;

enum Screen {
    Login { error: Option<String> },
    Connecting(Receiver<ConnectResult>),
    Main(Box<Session>),
    /// 网络断开后自动重连
    Reconnecting(Box<Reconnect>),
}

/// 正在放的伴奏
pub(crate) struct MusicPlaying {
    pub pid: u32,
    pub name: String,
    /// 开始前这个程序在音量合成器里的音量（停止时恢复）
    prev_volume: f32,
    /// 我这边听到的音量（5%–100%）
    pub monitor: f32,
    /// 发送增益 = 1 / monitor：本地调小了，录到的也变小，要补回来，别人听到的才不变
    gain: Arc<audio::AtomicF32>,
    _src: crate::streams::Source,
}

/// 估算嘴到耳延迟要用的本机参数
pub struct LatencyInputs {
    device_ms: u32,
    denoise_ms: u32,
    mixer: Arc<Mutex<Mixer>>,
}

impl LatencyInputs {
    /// 估算「他说话 → 我听到」的延迟（ms）和各部分说明。network_one_way：当前线路的单程网络延迟
    pub fn mouth_to_ear(&self, user: u32, network_one_way: u32) -> (u32, String) {
        let device = self.device_ms;
        // 采集声卡 + 降噪 + 20 ms 打包 + Opus 预读约 7 ms（按对方和我用同样设置估算）
        let capture = device + self.denoise_ms + 20 + 7;
        let buffer = self.mixer.lock().unwrap().buffer_ms(user).unwrap_or(20);
        let total = capture + network_one_way + buffer + device;
        (
            total,
            format!("对方采集与编码约 {capture} ms + 网络约 {network_one_way} ms + 抖动缓冲 {buffer} ms + 你的声卡 {device} ms"),
        )
    }
}

/// 自动重连的状态
struct Reconnect {
    /// 上次断开 / 连接失败的原因
    reason: String,
    /// 已经失败了几次
    attempt: u32,
    /// 下一次尝试的时间
    next_at: std::time::Instant,
    /// 正在进行的连接
    pending: Option<Receiver<ConnectResult>>,
}

/// 第 n 次失败后等多久再试：1、2、5、10、20、30 秒，之后一直 30 秒
pub fn reconnect_delay(attempt: u32) -> Duration {
    const STEPS: [u64; 6] = [1, 2, 5, 10, 20, 30];
    Duration::from_secs(STEPS[(attempt as usize).min(STEPS.len() - 1)])
}

/// 连接断开的原因，以及要不要自动重连
pub struct Dropped {
    pub reason: String,
    /// true = 网络断开，自动重连；false = 服务器主动断开（吊销、顶号等）或登录失败
    pub retry: bool,
}

/// 关闭 VOC 时清空聊天：先把内容覆盖成 0 再释放（阅后即焚）
/// 把字符串内容覆盖成 0 再清空（全 0 字节仍是合法 UTF-8）
fn wipe_string(s: &mut String) {
    unsafe { s.as_bytes_mut().fill(0) };
    s.clear();
}

/// 清除私聊（关闭私聊标签、关掉 VOC 时）
fn wipe_dm(t: &mut DmThread) {
    wipe_string(&mut t.peer);
    wipe_chat(&mut t.lines);
}

fn wipe_chat(lines: &mut Vec<ChatLine>) {
    let wipe = wipe_string;
    for l in lines.iter_mut() {
        match l {
            ChatLine::User { from, text, .. } => {
                wipe(from);
                wipe(text);
            }
            ChatLine::System(t) => wipe(t),
            ChatLine::Image(i) => {
                wipe(&mut i.from);
                i.tex = None; // 释放贴图
            }
        }
    }
    lines.clear();
}

pub struct App {
    cfg: Persisted,
    code_input: String,
    show_code: bool,
    server_label: String,
    screen: Screen,
    settings: Arc<Settings>,
    mixer: Arc<Mutex<Mixer>>,
    voice: Arc<VoiceState>,
    frames_tx: mpsc::SyncSender<Vec<f32>>,
    streams: Option<AudioStreams>,
    audio_errors: Vec<String>,
    show_settings: bool,
    input_devices: Vec<String>,
    output_devices: Vec<String>,
    denoise: DenoiseSlot,
    /// 当前降噪状态说明（成功为 Ok，失败为错误信息）
    denoise_status: Result<String, String>,
    identity_seed: [u8; 32],
    sharer: Option<Sharer>,
    /// 服务器已确认我在共享（之后若服务器说没在共享，就停掉本地采集）
    share_confirmed: bool,
    show_share: bool,
    monitors: Vec<String>,
    share_error: Option<String>,
    views: HashMap<u32, Viewer>,
    adding_channel: bool,
    settings_tab: u8,
    sfx_seq: u16,
    updater: crate::update::Updater,
    /// 上次点「复制诊断信息」的时间（显示「已复制」）
    diag_copied: Option<std::time::Instant>,
    /// 断开后保留的聊天（按服务器），只在内存里，关闭 VOC 时清空
    chat_store: HashMap<String, (Vec<ChatLine>, Vec<DmThread>)>,
    /// 重连成功后要回到的语音频道
    rejoin: Option<u32>,
    egui_ctx: egui::Context,
    /// VOC_AUTOLOGIN 测试模式：不保存配置
    test_mode: bool,
    tray: Option<crate::tray::Tray>,
    /// 用户选了「退出」：真正关闭，不缩到托盘
    quitting: bool,
    /// 窗口当前是否隐藏在托盘
    hidden: bool,
    autostart: crate::autostart::AutoStart,
    devices: Arc<Mutex<Option<audio::DeviceSnapshot>>>,
    last_device_check: std::time::Instant,
    /// 10 ms 小缓冲老是欠载：这次运行期间改用默认缓冲
    big_audio_buffer: bool,
    /// 正在等用户按下新的说话键
    ptt_capture: bool,
    /// 正在接收的图片
    images: crate::images::ImageStore,
    /// 后台准备好的待发送图片（选图 / 粘贴 / 拖入）
    image_jobs: (mpsc::Sender<Result<crate::images::Prepared, String>>, Receiver<Result<crate::images::Prepared, String>>),
    /// 正在看大图
    image_view: Option<egui::TextureHandle>,
    /// 测试模式：VOC_TEST_SEND_IMAGE 的图片已经发过
    test_image_sent: bool,
    /// 正在放的伴奏
    music: Option<MusicPlaying>,
    /// 伴奏窗口
    music_window: bool,
    music_apps: Vec<(u32, String)>,
    music_error: Option<String>,
    /// 屏幕共享的声音（共享时自动开）
    share_audio_src: Option<crate::streams::Source>,
    share_audio_failed: bool,
    /// 全屏观看谁的屏幕
    fullscreen: Option<u32>,
    /// 全屏时最后一次动鼠标（2 秒不动就隐藏工具栏）
    fs_mouse_at: std::time::Instant,
    last_xruns: u32,
    demo: Demo,
}

struct Viewer {
    view: Arc<View>,
    tex: Option<egui::TextureHandle>,
    seq: u64,
    size: (usize, usize),
    /// 看低画质层（流畅 360P）
    low: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::setup_fonts(&cc.egui_ctx);
        crate::theme::apply(&cc.egui_ctx);
        let mut cfg: Persisted =
            cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
        // 端到端加密身份：第一次运行时生成，之后一直沿用
        let identity_seed: [u8; 32] = voc_proto::e2e::unb64(&cfg.identity_seed)
            .and_then(|b| b.try_into().ok())
            .unwrap_or_else(|| {
                let seed = Identity::generate().seed();
                cfg.identity_seed = voc_proto::e2e::b64(&seed);
                seed
            });

        let settings = Arc::new(Settings::default());
        settings.vad_threshold_db.set(cfg.vad_threshold_db);
        settings.aec.store(cfg.aec, Ordering::Relaxed);
        settings.ptt_vk.store(cfg.ptt_vk, Ordering::Relaxed);
        crate::audio::LOW_BANDWIDTH.store(cfg.low_bandwidth, Ordering::Relaxed);
        settings.input_gain.set(cfg.input_gain);
        settings.output_volume.set(cfg.output_volume);
        let mixer = Arc::new(Mutex::new(Mixer::default()));
        let voice = Arc::new(VoiceState::default());
        video::HW_DECODE.store(cfg.hw_decode, Ordering::Relaxed);
        voice.video.set_ctx(cc.egui_ctx.clone()); // 收到新视频帧时唤醒界面
        let (frames_tx, frames_rx) = mpsc::sync_channel(100);
        let denoise: DenoiseSlot = Arc::new(Mutex::new(None));
        voice::spawn_encoder(frames_rx, settings.clone(), voice.clone(), mixer.clone(), denoise.clone());

        let mut app = Self {
            code_input: cfg.code.clone(),
            show_code: false,
            server_label: String::new(),
            cfg,
            screen: Screen::Login { error: None },
            settings,
            mixer,
            voice,
            frames_tx,
            streams: None,
            audio_errors: Vec::new(),
            show_settings: false,
            input_devices: Vec::new(),
            output_devices: Vec::new(),
            denoise,
            denoise_status: Ok(String::new()),
            identity_seed,
            sharer: None,
            share_confirmed: false,
            show_share: false,
            monitors: Vec::new(),
            share_error: None,
            views: HashMap::new(),
            adding_channel: false,
            settings_tab: 0,
            sfx_seq: 0,
            updater: crate::update::Updater::default(),
            diag_copied: None,
            chat_store: HashMap::new(),
            rejoin: None,
            egui_ctx: cc.egui_ctx.clone(),
            demo: Demo::default(),
            test_mode: false,
            tray: None,
            quitting: false,
            hidden: false,
            autostart: crate::autostart::AutoStart::Off,
            devices: audio::watch_devices(cc.egui_ctx.clone()),
            last_device_check: std::time::Instant::now(),
            big_audio_buffer: false,
            ptt_capture: false,
            images: crate::images::ImageStore::default(),
            image_jobs: mpsc::channel(),
            image_view: None,
            test_image_sent: false,
            music: None,
            music_window: false,
            music_apps: Vec::new(),
            music_error: None,
            share_audio_src: None,
            share_audio_failed: false,
            fullscreen: None,
            fs_mouse_at: std::time::Instant::now(),
            last_xruns: 0,
        };
        app.restart_audio();
        app.apply_denoise();
        if std::env::var("VOC_DEMO").is_err() {
            let icon = crate::window_icon();
            app.tray = crate::tray::Tray::new(&cc.egui_ctx, icon.rgba, icon.width, icon.height);
            app.autostart = crate::autostart::state();
        }
        let at_startup = crate::autostart::launched_at_startup();
        app.hidden = at_startup;
        if at_startup && app.tray.is_none() {
            // 托盘建不起来就别藏窗口
            cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            app.hidden = false;
        }
        if let Ok(kind) = std::env::var("VOC_DEMO") {
            app.load_demo(&kind);
        } else if at_startup && !app.cfg.server.trim().is_empty() && !app.code_input.trim().is_empty() {
            // 开机自启：自动登录上次的服务器并回到上次的频道
            app.start_connect(&cc.egui_ctx);
            app.rejoin = app.cfg.last_channel.get(app.cfg.server.trim()).copied();
        } else if let Ok(spec) = std::env::var("VOC_AUTOLOGIN") {
            // 测试用：VOC_AUTOLOGIN="地址|昵称|授权码|频道ID"，自动登录并进频道；不读写保存的配置
            let parts: Vec<&str> = spec.split('|').collect();
            app.test_mode = true;
            app.cfg.server = parts.first().unwrap_or(&"").to_string();
            app.cfg.name = parts.get(1).unwrap_or(&"tester").to_string();
            app.code_input = parts.get(2).unwrap_or(&"").to_string();
            app.cfg.pins.clear();
            app.start_connect(&cc.egui_ctx);
            app.rejoin = parts.get(3).and_then(|c| c.parse().ok());
        } else if app.cfg.auto_update {
            app.updater.check(cc.egui_ctx.clone());
        }
        app
    }

    fn apply_denoise(&mut self) {
        let r = denoise::create(self.cfg.denoise, &self.cfg.denoise_plugin, &self.cfg.denoise_model);
        let mut slot = self.denoise.lock().unwrap();
        *slot = None; // 先释放旧的（插件 DLL 可能被替换）
        self.denoise_status = match r {
            Ok(Some(d)) => {
                let s = format!("已启用：{}", d.name());
                *slot = Some(d);
                Ok(s)
            }
            Ok(None) => Ok("未启用降噪".into()),
            Err(e) => Err(e),
        };
    }

    fn restart_audio(&mut self) {
        self.streams = None; // 先释放旧设备
        let (s, errs) = audio::start_streams_with(
            self.cfg.input_device.as_deref(),
            self.cfg.output_device.as_deref(),
            self.frames_tx.clone(),
            self.mixer.clone(),
            self.settings.clone(),
            !self.big_audio_buffer,
        );
        self.last_xruns = 0;
        self.streams = Some(s);
        self.audio_errors = errs;
    }

    fn refresh_devices(&mut self) {
        self.input_devices = audio::input_device_names();
        self.output_devices = audio::output_device_names();
    }

    fn start_connect(&mut self, _ctx: &egui::Context) {
        self.rejoin = None;
        self.screen = Screen::Connecting(self.spawn_connect());
    }

    /// 后台建立连接，结果从返回的通道里取
    fn spawn_connect(&self) -> Receiver<ConnectResult> {
        let ctx = &self.egui_ctx;
        let addr = self.cfg.server.clone();
        let name = self.cfg.name.clone();
        let code = self.code_input.clone();
        let (voice, mixer) = (self.voice.clone(), self.mixer.clone());
        let (tx, rx) = mpsc::channel();
        let ctx2 = ctx.clone();
        std::thread::spawn(move || {
            let notify_ctx = ctx2.clone();
            let mut pcm = Vec::new();
            let r = net::connect(
                &addr,
                &name,
                &code,
                move || notify_ctx.request_repaint(),
                move |kind, payload| match kind {
                    voc_proto::crypto::KIND_VIDEO => voice::on_server_video(payload, &voice),
                    _ => voice::on_server_voice(payload, &voice, &mixer, &mut pcm),
                },
            );
            let _ = tx.send(r);
            ctx2.request_repaint();
        });
        rx
    }

    /// 离开主界面前：关掉共享 / 观看 / 语音，把聊天存起来（重新连上同一个服务器时接着显示）
    fn leave_main(&mut self) {
        self.stop_music();
        self.share_audio_src = None;
        self.fullscreen = None;
        self.sharer = None;
        self.share_confirmed = false;
        self.views.clear();
        self.voice.disconnect();
        self.mixer.lock().unwrap().clear();
        if let Screen::Main(s) = &mut self.screen {
            if s.conn.is_some() {
                let chat = std::mem::take(&mut s.chat);
                let dms = std::mem::take(&mut s.dms);
                self.chat_store.insert(s.server_key.clone(), (chat, dms));
            }
        }
    }

    /// 网络断开：记住所在频道，开始自动重连
    fn begin_reconnect(&mut self, reason: String) {
        if let Screen::Main(s) = &self.screen {
            self.rejoin = s.me().and_then(|u| u.channel_id);
        }
        self.leave_main();
        self.screen = Screen::Reconnecting(Box::new(Reconnect {
            reason,
            attempt: 0,
            next_at: std::time::Instant::now() + reconnect_delay(0),
            pending: None,
        }));
    }

    fn new_session(&mut self, conn: Conn, events: Receiver<NetEvent>) -> Session {
        let server_key = self.cfg.server.trim().to_string();
        let (chat, dms) = self.chat_store.remove(&server_key).unwrap_or_default();
        Session {
            conn: Some(conn),
            last_members: None,
            events,
            user_id: 0,
            server_name: String::new(),
            channels: Vec::new(),
            users: Vec::new(),
            chat,
            dms,
            chat_tab: None,
            chat_input: String::new(),
            new_channel: String::new(),
            server_key,
            e2e: None,
            features: Vec::new(),
        }
    }

    fn sync_voice_state(&self) {
        if let Screen::Main(s) = &self.screen {
            s.send(ClientMsg::SetState {
                muted: self.settings.muted.load(Ordering::Relaxed),
                deafened: self.settings.deafened.load(Ordering::Relaxed),
            });
        }
    }

    /// 把手动线路（按昵称记）换成当前的 id 交给链路
    fn sync_routes(&self) {
        let Screen::Main(s) = &self.screen else { return };
        let Some(l) = self.voice.current_link() else { return };
        let map: HashMap<u32, voice::RoutePref> = match self.cfg.routes.get(&s.server_key) {
            Some(m) => s
                .users
                .iter()
                .filter(|u| u.id != s.user_id)
                .filter_map(|u| m.get(&u.name).map(|r| (u.id, *r)))
                .filter(|(_, r)| *r != voice::RoutePref::Auto)
                .collect(),
            None => HashMap::new(),
        };
        l.set_route_prefs(map);
    }

    /// 诊断信息（给别人排查问题用）：版本、系统、网络、声卡、编解码器。
    /// 不含聊天、密钥、授权码、服务器地址和任何人的 IP。
    fn diagnostics(&self) -> String {
        use std::fmt::Write;
        let mut d = String::new();
        let _ = writeln!(d, "VOC 诊断信息");
        let _ = writeln!(d, "版本：{}", crate::update::VERSION);
        let _ = writeln!(d, "系统：{}", os_version());
        let _ = writeln!(d, "安装方式：{}", if crate::update::is_packaged() { "安装版（MSIX）" } else { "绿色版" });
        let _ = writeln!(d, "\n[网络]");
        match (&self.screen, self.voice.current_link()) {
            (Screen::Main(s), Some(l)) => {
                let tr = match self.voice.transport_udp() {
                    Some(true) => "UDP",
                    Some(false) => "TCP 隧道（UDP 不通）",
                    None => "未知",
                };
                let _ = writeln!(d, "已连接：是 · 传输 {tr} · 到服务器 {} ms", self.voice.ping_ms.load(Ordering::Relaxed));
                let _ = writeln!(d, "低带宽模式：{}", if self.cfg.low_bandwidth { "开" } else { "关" });
                let _ = writeln!(d, "P2P：{} · 上行预算 {} Mbps · 帮队友中转：{}", if l.p2p_on() { "开" } else { "关" }, self.cfg.p2p_budget_mbps, if self.cfg.relay { "开" } else { "关" });
                let states = l.punch_states();
                let count = |f: &dyn Fn(&voice::PunchState) -> bool| states.values().filter(|s| f(s)).count();
                let _ = writeln!(
                    d,
                    "直连：已打通 {} · 打洞中 {} · 打不通 {} · 等待密钥 {}",
                    count(&|s| matches!(s, voice::PunchState::Connected(_))),
                    count(&|s| matches!(s, voice::PunchState::Punching(_))),
                    count(&|s| matches!(s, voice::PunchState::Failed(_))),
                    count(&|s| matches!(s, voice::PunchState::WaitingKeys)),
                );
                for q in l.direct_peers().values() {
                    let _ = writeln!(d, "  链路：{} ms · 丢包 {}% · 抖动 {} ms{}", q.rtt_ms, q.loss_pct, q.jitter_ms, if q.unstable { " · 不稳" } else { "" });
                }
                // 自检结论（去掉含 IP 的内容）
                let rep = crate::netcheck::analyze(&l.netcheck_snapshot());
                for it in rep.items.iter().filter(|i| i.label != "本机地址" && i.label != "公网地址") {
                    let v = if it.label == "端口映射" {
                        crate::netcheck::portmap_text_without_ip(&l.portmap_status(), it)
                    } else {
                        it.value.clone()
                    };
                    let _ = writeln!(d, "{}：{}", it.label, v);
                }
                let (now, avg, peak) = l.upload_kbps();
                let _ = writeln!(d, "上行：当前 {now} kbps · 最近 30 秒平均 {avg} · 峰值 {peak}");
                match &self.sharer {
                    Some(sh) => {
                        let st = &sh.stats;
                        let _ = writeln!(
                            d,
                            "屏幕共享：{} · 观看 {} 人（其中流畅 {} 人）· 实际 {}×{} {} fps {} kbps · 码率 {}% · {} · {}",
                            sh.cfg.desc(),
                            l.watcher_count(),
                            st.low_watchers.load(Ordering::Relaxed),
                            st.width.load(Ordering::Relaxed),
                            st.height.load(Ordering::Relaxed),
                            st.fps.load(Ordering::Relaxed),
                            st.kbps.load(Ordering::Relaxed),
                            st.rate_pct.load(Ordering::Relaxed),
                            st.encoder.lock().unwrap(),
                            st.pipeline.lock().unwrap(),
                        );
                    }
                    None => {
                        let _ = writeln!(d, "屏幕共享：没在共享");
                    }
                }
                let _ = writeln!(d, "伴奏：{}", if self.music.is_some() { "正在放" } else { "没放" });
                let _ = writeln!(d, "在看别人的共享：{} 路", self.views.len());
                // 同频道每个人：加密、线路、有没有收到他的声音（排查「我能听到他、他听不到我」）
                let yn = |b: bool| if b { "是" } else { "否" };
                let my_ch = s.me().and_then(|u| u.channel_id);
                if let (Some(ch), Some(e2e)) = (my_ch, &s.e2e) {
                    let _ = writeln!(d, "\n[同频道成员]");
                    let e = e2e.lock().unwrap();
                    let punch = l.punch_states();
                    let (alloc, _) = l.allocation();
                    let routes = l.relays();
                    for u in s.users.iter().filter(|u| u.id != s.user_id && u.channel_id == Some(ch)) {
                        let enc = match e.member_status(u.id) {
                            None => "还没收到他的加密身份".to_string(),
                            Some((t, pair, theirs, mine)) => format!(
                                "加密 {} · 成对密钥 {} · 收到他的语音密钥 {} · 我的语音密钥已发给他 {}",
                                match t {
                                    Trust::New | Trust::Known => "正常",
                                    Trust::Changed => "身份已变化（暂停）",
                                },
                                yn(pair),
                                yn(theirs),
                                yn(mine)
                            ),
                        };
                        let me = s.user_id;
                        let out = if alloc.contains(&u.id) {
                            "直连".to_string()
                        } else if let Some(r) = routes.iter().find(|r| r.from == me && r.to == u.id) {
                            format!("经 {} 中转", s.name_of(r.via))
                        } else {
                            "服务器".to_string()
                        };
                        let inbound = routes.iter().find(|r| r.from == u.id && r.to == me).map(|r| format!(" · 他的声音经 {} 中转过来", s.name_of(r.via)));
                        let ps = match punch.get(&u.id) {
                            Some(voice::PunchState::Connected(q)) => format!("已打通 {} ms", q.rtt_ms),
                            Some(voice::PunchState::Punching(t)) => format!("打洞中 {t}s"),
                            Some(voice::PunchState::Failed(_)) => "打不通".into(),
                            Some(voice::PunchState::WaitingKeys) => "等待密钥".into(),
                            None => "没有打洞".into(),
                        };
                        let _ = writeln!(
                            d,
                            "{}：{} · 我的声音发给他走 {}{} · 打洞 {} · 手动线路 {} · 最近 3 秒收到他的声音 {}{}{}",
                            u.name,
                            enc,
                            out,
                            inbound.unwrap_or_default(),
                            ps,
                            l.route_pref(u.id).label(),
                            yn(self.voice.heard_within(u.id, Duration::from_secs(3))),
                            if u.muted { " · 他闭麦了" } else { "" },
                            if u.deafened { " · 他开了静音" } else { "" },
                        );
                    }
                }
                let me = l.my_id();
                let routes = l.relays();
                let _ = writeln!(
                    d,
                    "成员中继：经队友收听 {} 路 · 我在转发 {} 路",
                    routes.iter().filter(|r| r.to == me).count(),
                    routes.iter().filter(|r| r.via == me).count()
                );
            }
            _ => {
                let _ = writeln!(d, "已连接：否");
            }
        }
        let _ = writeln!(d, "\n[声音]");
        let _ = writeln!(d, "麦克风：{}", self.cfg.input_device.as_deref().unwrap_or("系统默认"));
        let _ = writeln!(d, "扬声器：{}", self.cfg.output_device.as_deref().unwrap_or("系统默认"));
        let _ = writeln!(d, "降噪：{:?} · 语音激活阈值 {} dB", self.cfg.denoise, self.cfg.vad_threshold_db);
        {
            let st = &self.settings;
            let ptt = st.ptt_vk.load(Ordering::Relaxed);
            let _ = writeln!(
                d,
                "麦克风状态：{} · 说话方式 {} · 当前电平 {:.0} dB · 正在发送 {} · 回声消除 {} · 麦克风完全没声音 {}",
                if st.deafened.load(Ordering::Relaxed) {
                    "静音中"
                } else if st.muted.load(Ordering::Relaxed) {
                    "闭麦中"
                } else {
                    "开"
                },
                if ptt == 0 { "语音激活".to_string() } else { format!("按键说话（{}）", vk_name(ptt)) },
                st.input_level_db.get(),
                if st.transmitting.load(Ordering::Relaxed) { "是" } else { "否" },
                if st.aec.load(Ordering::Relaxed) { "开" } else { "关" },
                if st.mic_silent.load(Ordering::Relaxed) { "是（检查 Windows 麦克风隐私设置）" } else { "否" },
            );
        }
        let _ = writeln!(d, "\n[视频]");
        let enc: Vec<String> = video::hw_encoders().iter().map(|e| format!("{} ({})", e.name, e.gpu)).collect();
        let _ = writeln!(d, "硬件编码器：{}", if enc.is_empty() { "无".into() } else { enc.join("、") });
        let dec: Vec<&str> = video::local_decoders().iter().map(|c| c.key()).collect();
        let _ = writeln!(d, "可解码：{} · 解码方式 {}", dec.join(" / "), if self.cfg.hw_decode { "硬解" } else { "软解" });
        d
    }

    /// 窗口隐藏时也要跑的事：网络、托盘、关窗口缩托盘、更新、设备拔插
    fn background(&mut self, ctx: &egui::Context) {
        if self.updater.get() == crate::update::UpdateState::Launched {
            self.quitting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if let Some(d) = self.poll_network() {
            if d.retry && !self.demo.active {
                self.begin_reconnect(d.reason);
            } else {
                self.go_login(Some(d.reason));
            }
        }
        self.sync_routes();
        // 测试模式：进频道 2 秒后自动发一张图（VOC_TEST_SEND_IMAGE=路径）
        if self.test_mode && !self.test_image_sent {
            if let (Ok(path), Screen::Main(s)) = (std::env::var("VOC_TEST_SEND_IMAGE"), &self.screen) {
                let others = s.users.iter().filter(|u| u.id != s.user_id && u.channel_id.is_some() && u.channel_id == s.me().and_then(|m| m.channel_id)).count();
                if others > 0 && self.last_device_check.elapsed() > Duration::from_millis(1500) {
                    self.test_image_sent = true;
                    let p = std::path::PathBuf::from(path);
                    queue_image(&self.image_jobs.0, ctx, move || crate::images::prepare_file(&p));
                }
            }
        }
        // 屏幕共享的声音：共享时开，停了就关（录除了 VOC 自己以外的全部声音）
        let want_share_audio = self.sharer.is_some() && self.cfg.share_audio && crate::loopback::supported();
        if !want_share_audio {
            self.share_audio_src = None;
            self.share_audio_failed = false;
        } else if self.share_audio_src.is_none() && !self.share_audio_failed {
            let target = crate::loopback::Target::Exclude(std::process::id());
            match crate::streams::start(voc_proto::udp::SHARE_AUDIO, target, self.voice.clone(), Arc::new(audio::AtomicF32::new(1.0))) {
                Ok(src) => self.share_audio_src = Some(src),
                Err(e) => {
                    self.share_audio_failed = true;
                    if let Screen::Main(s) = &mut self.screen {
                        s.chat.push(ChatLine::System(format!("共享声音没能开启：{e}")));
                    }
                }
            }
        }
        // 正在看某人带声音的屏幕共享时，他的伴奏已经在共享声音里了：单独那一路不再播，免得听到两遍
        if let Screen::Main(s) = &self.screen {
            let dup: std::collections::HashSet<u32> = self
                .views
                .keys()
                .filter(|id| s.users.iter().any(|u| u.id == **id && u.share_audio && u.music.is_some()))
                .map(|id| audio::stream_mixer_id(voc_proto::udp::MUSIC, *id))
                .collect();
            self.mixer.lock().unwrap().set_suppressed(dup);
        }
        // 图片：准备好的发出去；后台解码完的贴到聊天里
        let jobs: Vec<_> = self.image_jobs.1.try_iter().collect();
        for job in jobs {
            match job {
                Ok(p) => self.send_image(p, ctx),
                Err(e) => {
                    if let Screen::Main(s) = &mut self.screen {
                        let tab = s.chat_tab.clone();
                        s.lines_for(tab.as_deref()).push(ChatLine::System(format!("图片没有发出：{e}")));
                    }
                }
            }
        }
        let ev = self.images.poll();
        if !ev.is_empty() {
            if let Screen::Main(s) = &mut self.screen {
                apply_image_events(s, ev, ctx);
            }
        }
        // 托盘
        let actions = self.tray.as_ref().map(|t| t.poll()).unwrap_or_default();
        for a in actions {
            match a {
                crate::tray::TrayAction::Show => {
                    self.hidden = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                crate::tray::TrayAction::ToggleMute => {
                    self.settings.muted.fetch_xor(true, Ordering::Relaxed);
                    self.sync_voice_state();
                }
                crate::tray::TrayAction::ToggleDeaf => {
                    self.settings.deafened.fetch_xor(true, Ordering::Relaxed);
                    self.sync_voice_state();
                }
                crate::tray::TrayAction::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
        if let Some(t) = &self.tray {
            let (m, d) = (self.settings.muted.load(Ordering::Relaxed), self.settings.deafened.load(Ordering::Relaxed));
            let status = match &self.screen {
                Screen::Main(s) => {
                    let ch = s.me().and_then(|u| u.channel_id).and_then(|c| s.channels.iter().find(|x| x.id == c)).map(|c| c.name.clone());
                    let mut st = ch.map(|c| format!("在「{c}」")).unwrap_or_else(|| "已连接".into());
                    if d {
                        st.push_str(" · 已静音");
                    } else if m {
                        st.push_str(" · 已闭麦");
                    }
                    st
                }
                Screen::Reconnecting(_) => "正在重连…".into(),
                _ => "未连接".into(),
            };
            t.set_state(m, d, &status);
        }
        // 关窗口 → 缩到托盘
        if ctx.input(|i| i.viewport().close_requested()) && self.cfg.close_to_tray && self.tray.is_some() && !self.quitting && !self.demo.active {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.hidden = true;
        }
        // 耳机 / 麦克风拔插、默认设备变化：每 2 秒对一次
        if self.last_device_check.elapsed() >= Duration::from_secs(2) && !self.demo.active {
            self.last_device_check = std::time::Instant::now();
            let snap = self.devices.lock().unwrap().clone();
            if let (Some(snap), Some(st)) = (snap, &self.streams) {
                let want_in = audio::wanted_device(self.cfg.input_device.as_deref(), &snap.inputs, snap.default_input.as_ref());
                let want_out = audio::wanted_device(self.cfg.output_device.as_deref(), &snap.outputs, snap.default_output.as_ref());
                let failed = st.failed.load(Ordering::Relaxed);
                // 2 秒里欠载 / 溢出超过 5 次：小缓冲扛不住，改用默认缓冲
                let xruns = st.xruns.load(Ordering::Relaxed);
                let too_many_xruns = st.small_buffer && xruns.saturating_sub(self.last_xruns) > 5;
                self.last_xruns = xruns;
                if too_many_xruns {
                    self.big_audio_buffer = true;
                }
                let in_changed = want_in.as_deref().unwrap_or("") != st.input_name;
                let out_changed = want_out.as_deref().unwrap_or("") != st.output_name;
                if failed || in_changed || out_changed || too_many_xruns {
                    self.restart_audio();
                    self.input_devices = snap.inputs.clone();
                    self.output_devices = snap.outputs.clone();
                    if let (Screen::Main(s), Some(st)) = (&mut self.screen, &self.streams) {
                        if in_changed && !st.input_name.is_empty() {
                            s.chat.push(ChatLine::System(format!("麦克风已切换到：{}", st.input_name)));
                        }
                        if out_changed && !st.output_name.is_empty() {
                            s.chat.push(ChatLine::System(format!("扬声器已切换到：{}", st.output_name)));
                        }
                    }
                }
            }
        }
        ctx.request_repaint_after(Duration::from_millis(if self.hidden { 100 } else { 50 }));
    }

    /// 估算嘴到耳延迟要用的本机参数
    fn latency_inputs(&self) -> LatencyInputs {
        LatencyInputs {
            device_ms: if self.streams.as_ref().is_some_and(|s| s.small_buffer) { 10 } else { 20 },
            denoise_ms: if self.cfg.denoise == DenoiseMode::Off { 0 } else { 10 },
            mixer: self.mixer.clone(),
        }
    }


    /// 开始伴奏：录这个程序的声音发给频道里的人
    pub(crate) fn start_music(&mut self, pid: u32, name: String) {
        self.stop_music();
        let gain = Arc::new(audio::AtomicF32::new(1.0));
        match crate::streams::start(voc_proto::udp::MUSIC, crate::loopback::Target::Include(pid), self.voice.clone(), gain.clone()) {
            Ok(src) => {
                let prev_volume = crate::loopback::app_volume(pid).unwrap_or(1.0);
                if let Screen::Main(s) = &self.screen {
                    s.send(ClientMsg::SetMusic { name: Some(name.clone()) });
                }
                self.music = Some(MusicPlaying { pid, name, prev_volume, monitor: 1.0, gain, _src: src });
                self.music_error = None;
            }
            Err(e) => self.music_error = Some(e),
        }
    }

    /// 停止伴奏，恢复那个程序原来的音量
    pub(crate) fn stop_music(&mut self) {
        if let Some(m) = self.music.take() {
            crate::loopback::set_app_volume(m.pid, m.prev_volume);
            if let Screen::Main(s) = &self.screen {
                s.send(ClientMsg::SetMusic { name: None });
            }
        }
    }

    /// 调本地监听音量：程序在音量合成器里调小，发送时按比例补回来
    pub(crate) fn set_music_monitor(&mut self, v: f32) {
        if let Some(m) = &mut self.music {
            m.monitor = v.clamp(0.05, 1.0);
            crate::loopback::set_app_volume(m.pid, m.monitor * m.prev_volume.max(0.05));
            m.gain.set(1.0 / m.monitor);
        }
    }

    /// 端到端加密后发给同频道的人；在私聊标签里只发给对方
    fn send_image(&mut self, p: crate::images::Prepared, ctx: &egui::Context) {
        let Screen::Main(s) = &mut self.screen else { return };
        if s.conn.is_none() || s.e2e.is_none() {
            return;
        }
        let dm = s.chat_tab.clone();
        let to: Vec<u32> = match &dm {
            Some(peer) => {
                let id = s.id_by_name(peer);
                let why = if !s.features.iter().any(|f| f == voc_proto::FEATURE_DM_FILE) {
                    Some("服务器版本较旧，私聊发图需要先升级服务器（发文字不受影响）")
                } else if id.is_none() {
                    Some("对方不在线，图片没有发出")
                } else {
                    None
                };
                if let Some(w) = why {
                    s.dm(peer).lines.push(ChatLine::System(w.into()));
                    return;
                }
                vec![id.unwrap()]
            }
            None => {
                let Some(ch) = s.me().and_then(|u| u.channel_id) else {
                    s.chat.push(ChatLine::System("进入语音频道后才能发图片（图片只发给同频道的人；私聊可以右键对方名字）".into()));
                    return;
                };
                s.users.iter().filter(|u| u.id != s.user_id && u.channel_id == Some(ch)).map(|u| u.id).collect()
            }
        };
        let (Some(conn), Some(e2e)) = (&s.conn, &s.e2e) else { return };
        let (id, key) = crate::images::new_id_and_key();
        let total = voc_proto::file::chunk_count(p.bytes.len());
        let meta = voc_proto::file::ImageMeta {
            id,
            key: voc_proto::e2e::b64(&key),
            size: p.bytes.len() as u32,
            chunks: total,
            mime: p.mime.to_string(),
            width: p.width,
            height: p.height,
        };
        if dm.is_some() {
            conn.send(ClientMsg::FileTarget { id, to: to[0] });
        }
        let missing = e2e.lock().unwrap().send_image_meta(&to, &meta, dm.is_some());
        let progress = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let sender = conn.sender();
        let mut bytes = p.bytes;
        // 一个能收的人都没有就不上传
        let nobody = to.len() == missing.len();
        if nobody {
            crate::images::wipe(&mut bytes);
        } else {
            crate::images::spawn_upload(sender, id, key, bytes, progress.clone());
        }
        let me = s.me().map(|u| u.name.clone()).unwrap_or_default();
        let error = match (to.is_empty(), nobody, dm.is_some()) {
            (true, _, _) => Some("频道里没有其他人".to_string()),
            (_, true, true) => Some("和对方的加密尚未建立（或身份待核对），没有发出".to_string()),
            (_, true, false) => Some("同频道的人都还没建立加密，没有发出".to_string()),
            _ => None,
        };
        let user_id = s.user_id;
        s.lines_for(dm.as_deref()).push(ChatLine::Image(Box::new(ImageLine {
            from: me,
            ts: voc_proto::now_millis(),
            mine: true,
            key: (user_id, id),
            tex: Some(ctx.load_texture(format!("img-mine-{id}"), p.preview, Default::default())),
            sending: (!nobody).then(|| (progress, total)),
            receiving: None,
            error,
        })));
        if !missing.is_empty() && !nobody {
            s.chat.push(ChatLine::System(format!("以下成员收不到这张图（加密尚未建立或身份待核对）：{}", missing.join("、"))));
        }
    }

    fn go_login(&mut self, error: Option<String>) {
        self.leave_main();
        self.rejoin = None;
        self.screen = Screen::Login { error };
    }

    /// 处理网络事件；返回 Some 表示连接已断开
    fn poll_network(&mut self) -> Option<Dropped> {
        let fail = |reason: String| Some(Dropped { reason, retry: false });
        // 连接中
        if let Screen::Connecting(rx) = &self.screen {
            match rx.try_recv() {
                Ok(Ok((conn, events))) => {
                    let s = self.new_session(conn, events);
                    self.screen = Screen::Main(Box::new(s));
                }
                Ok(Err(e)) => return fail(e),
                Err(mpsc::TryRecvError::Empty) => return None,
                Err(mpsc::TryRecvError::Disconnected) => return fail("连接线程异常退出".into()),
            }
        }
        // 自动重连
        if let Screen::Reconnecting(r) = &mut self.screen {
            match r.pending.as_ref().map(|rx| rx.try_recv()) {
                Some(Ok(Ok((conn, events)))) => {
                    let mut s = self.new_session(conn, events);
                    s.chat.push(ChatLine::System("已重新连接".into()));
                    self.screen = Screen::Main(Box::new(s));
                }
                Some(Ok(Err(e))) => {
                    r.attempt += 1;
                    r.reason = e;
                    r.pending = None;
                    r.next_at = std::time::Instant::now() + reconnect_delay(r.attempt);
                    return None;
                }
                Some(Err(mpsc::TryRecvError::Disconnected)) => {
                    r.attempt += 1;
                    r.pending = None;
                    r.next_at = std::time::Instant::now() + reconnect_delay(r.attempt);
                    return None;
                }
                Some(Err(mpsc::TryRecvError::Empty)) => return None,
                None => {
                    if std::time::Instant::now() >= r.next_at {
                        let rx = self.spawn_connect();
                        if let Screen::Reconnecting(r) = &mut self.screen {
                            r.pending = Some(rx);
                        }
                    }
                    return None;
                }
            }
        }

        let Screen::Main(s) = &mut self.screen else { return None };
        while let Ok(ev) = s.events.try_recv() {
            match ev {
                NetEvent::Closed(reason) => return Some(Dropped { reason, retry: true }),
                NetEvent::File(data) => {
                    let ev = self.images.on_chunk(&data);
                    apply_image_events(s, ev, &self.egui_ctx);
                }
                NetEvent::Msg(msg) => match msg {
                    ServerMsg::Welcome { user_id, udp_port, server_name, features } => {
                        s.user_id = user_id;
                        s.features = features;
                        s.server_name = server_name;
                        let Some(conn) = &s.conn else { continue };
                        let mut udp_addr = conn.server_addr;
                        udp_addr.set_port(udp_port);
                        let pins = self.cfg.pins.get(&s.server_key).cloned().unwrap_or_default();
                        let e2e = Arc::new(Mutex::new(E2e::new(
                            user_id,
                            self.cfg.name.trim(),
                            Identity::from_seed(&self.identity_seed),
                            pins,
                        )));
                        s.e2e = Some(e2e.clone());
                        if let Err(e) = voice::connect(
                            udp_addr,
                            &conn.keys,
                            user_id,
                            self.cfg.p2p,
                            e2e,
                            conn.sender(),
                            self.voice.clone(),
                            self.mixer.clone(),
                        ) {
                            s.chat.push(ChatLine::System(format!("语音通道建立失败：{e}")));
                        }
                        if let Some(l) = self.voice.current_link() {
                            l.set_budget(self.cfg.p2p_budget_mbps * 1_000_000);
                            l.set_relay_enabled(self.cfg.relay);
                        }
                        s.send(ClientMsg::Caps { decoders: video::local_decoders().iter().map(|c| c.key().to_string()).collect() });
                        s.send(ClientMsg::SetState {
                            muted: self.settings.muted.load(Ordering::Relaxed),
                            deafened: self.settings.deafened.load(Ordering::Relaxed),
                        });
                    }
                    ServerMsg::Error { message } => return fail(message),
                    ServerMsg::State { channels, users } => {
                        s.channels = channels;
                        s.users = users;
                        if let Some(ch) = s.me().and_then(|u| u.channel_id) {
                            if !self.test_mode {
                                self.cfg.last_channel.insert(s.server_key.clone(), ch);
                            }
                        }
                        // 重连后回到原来的语音频道
                        if let (Some(ch), Some(me)) = (self.rejoin, s.me()) {
                            if me.channel_id.is_none() && s.channels.iter().any(|c| c.id == ch) {
                                s.send(ClientMsg::JoinChannel { channel_id: ch });
                            }
                            self.rejoin = None;
                        }
                        let in_ch = s.me().is_some_and(|u| u.channel_id.is_some());
                        if let Some(l) = self.voice.current_link() {
                            l.in_channel.store(in_ch, Ordering::Relaxed);
                            let my_ch = s.me().and_then(|u| u.channel_id);
                            let others = s
                                .users
                                .iter()
                                .filter(|u| u.id != s.user_id && my_ch.is_some() && u.channel_id == my_ch)
                                .map(|u| u.id)
                                .collect();
                            l.set_others(others);
                        }
                        let ids: Vec<u32> = s.users.iter().map(|u| u.id).collect();
                        self.mixer.lock().unwrap().retain_users(|id| ids.contains(&id));
                        *self.voice.caps.lock().unwrap() = s.users.iter().map(|u| (u.id, u.decoders.clone())).collect();
                        // 进出提示音：只看我所在的频道；第一次收到状态（刚登录）不响
                        let my_ch = s.me().and_then(|u| u.channel_id);
                        let mut now_members: Vec<u32> = s
                            .users
                            .iter()
                            .filter(|u| u.id != s.user_id && my_ch.is_some() && u.channel_id == my_ch)
                            .map(|u| u.id)
                            .collect();
                        now_members.sort();
                        let chime = match &s.last_members {
                            Some((prev_ch, _)) if *prev_ch != my_ch => Some(if my_ch.is_some() { Chime::Join } else { Chime::Leave }),
                            Some((_, prev)) if my_ch.is_some() => {
                                if now_members.iter().any(|m| !prev.contains(m)) {
                                    Some(Chime::Join)
                                } else if prev.iter().any(|m| !now_members.contains(m)) {
                                    Some(Chime::Leave)
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        };
                        s.last_members = Some((my_ch, now_members));
                        if let (Some(c), true) = (chime, self.cfg.sfx) {
                            let mut m = self.mixer.lock().unwrap();
                            m.set_volume(SFX_ID, self.cfg.sfx_volume);
                            for f in audio::chime(c).chunks(voc_proto::FRAME_SAMPLES) {
                                self.sfx_seq = self.sfx_seq.wrapping_add(1);
                                m.push(SFX_ID, self.sfx_seq, f);
                            }
                        }
                        // 共享：服务器确认后若又变成没在共享（换了频道等），停掉本地采集
                        let me_sharing = s.me().is_some_and(|u| u.sharing.is_some());
                        if self.sharer.is_some() {
                            if me_sharing {
                                self.share_confirmed = true;
                            } else if self.share_confirmed || !in_ch {
                                self.sharer = None;
                                self.share_confirmed = false;
                            }
                        }
                        // 观看：对方停止共享或不在同一频道了就关掉
                        let my_ch = s.me().and_then(|u| u.channel_id);
                        let still: Vec<u32> = s
                            .users
                            .iter()
                            .filter(|u| u.sharing.is_some() && my_ch.is_some() && u.channel_id == my_ch)
                            .map(|u| u.id)
                            .collect();
                        let gone: Vec<u32> = self.views.keys().copied().filter(|id| !still.contains(id)).collect();
                        for id in gone {
                            self.views.remove(&id);
                            self.voice.video.watch(id, false);
                        }
                        if let Some(e2e) = &s.e2e {
                            let my_ch = s.me().and_then(|u| u.channel_id);
                            let others = s
                                .users
                                .iter()
                                .filter(|u| u.id != s.user_id && my_ch.is_some() && u.channel_id == my_ch)
                                .map(|u| u.id)
                                .collect();
                            let mut e = e2e.lock().unwrap();
                            e.retain_online(&ids);
                            e.set_channel_others(others); // 成员变化 → 换语音密钥
                        }
                    }
                    ServerMsg::System { text } => s.chat.push(ChatLine::System(text)),
                    ServerMsg::Pong => {}
                    ServerMsg::Watchers { ids, low } => {
                        if let Some(l) = self.voice.current_link() {
                            l.set_watchers(ids, low);
                        }
                    }
                    ServerMsg::KeyframeRequest { .. } => {
                        if let Some(sh) = &self.sharer {
                            sh.request_keyframe();
                        }
                    }
                    ServerMsg::Peers { peers } => {
                        if let Some(l) = self.voice.current_link() {
                            l.on_peers(peers);
                        }
                    }
                    ServerMsg::Relays { routes } => {
                        if let Some(l) = self.voice.current_link() {
                            l.set_relays(routes);
                        }
                    }
                    ServerMsg::Directory { entries } => {
                        for entry in entries {
                            s.on_e2e_entry(entry);
                        }
                    }
                    ServerMsg::E2eJoin { entry } => s.on_e2e_entry(entry),
                    ServerMsg::PeerKey { from, ct, sig } => {
                        if let Some(e2e) = &s.e2e {
                            e2e.lock().unwrap().on_peer_key(from, &ct, &sig);
                        }
                    }
                    ServerMsg::E2eBlob { from, blob, ts } => {
                        let msg = s.e2e.as_ref().and_then(|e| e.lock().unwrap().on_blob(from, &blob));
                        match msg {
                            Some(crate::e2e::BlobIn::Chat(c)) => {
                                let from = s.name_of(c.from);
                                let line = ChatLine::User { from: from.clone(), text: c.text, ts, mine: false };
                                if c.dm {
                                    s.push_dm(&from, line);
                                } else {
                                    s.chat.push(line);
                                }
                            }
                            Some(crate::e2e::BlobIn::Image { from, meta, dm }) => {
                                let total = meta.chunks;
                                let name = s.name_of(from);
                                let line = ChatLine::Image(Box::new(ImageLine {
                                    from: name.clone(),
                                    ts,
                                    mine: false,
                                    key: (from, meta.id),
                                    tex: None,
                                    sending: None,
                                    receiving: Some((0, total)),
                                    error: None,
                                }));
                                if dm {
                                    s.push_dm(&name, line);
                                } else {
                                    s.chat.push(line);
                                }
                                let ev = self.images.on_meta(from, meta);
                                apply_image_events(s, ev, &self.egui_ctx);
                            }
                            None => {}
                        }
                    }
                },
            }
        }
        // 端到端加密层产生的消息（公钥交换、语音密钥、聊天信封）交给服务器转发
        if let Some(e2e) = &s.e2e {
            let mut e = e2e.lock().unwrap();
            for m in e.take_outgoing() {
                s.send(m);
            }
            if e.pins_dirty {
                e.pins_dirty = false;
                self.cfg.pins.insert(s.server_key.clone(), e.pins.clone());
            }
        }
        if s.chat.len() > 1000 {
            s.chat.drain(..s.chat.len() - 1000);
        }
        None
    }

}

// ============================================================ 演示 / 截图模式（开发用）
//
//   $env:VOC_DEMO="main"; $env:VOC_SHOT="shot.png"; voc-client.exe
// VOC_DEMO = login | main | settings | share | viewer，用假数据渲染界面（不联网）；
// VOC_SHOT 给出时，渲染几帧后截图保存为 PNG 并退出。

#[derive(Default)]
struct Demo {
    active: bool,
    shot: Option<String>,
    frames: u32,
    shot_requested: bool,
    /// id → (质量, 语音直连, 画面直连)
    quality: HashMap<u32, (PeerQuality, bool, bool)>,
    e2e: HashMap<u32, (Trust, String, bool)>,
    speaking: Vec<u32>,
    transport: Option<bool>,
    ping: u32,
    relays: Vec<voc_proto::RelayRoute>,
    punch: HashMap<u32, voice::PunchState>,
}

impl App {
    fn load_demo(&mut self, kind: &str) {
        self.demo.active = true;
        self.demo.shot = std::env::var("VOC_SHOT").ok();
        let code = voc_proto::auth::AuthCode::generate().display();
        self.cfg.servers = vec![
            SavedServer { label: "开黑服".into(), addr: "voc.example.com".into(), name: "小明".into(), code: code.clone() },
            SavedServer { label: "公司内网".into(), addr: "10.0.0.8".into(), name: "小明".into(), code: String::new() },
        ];
        self.cfg.server = "voc.example.com".into();
        self.cfg.name = "小明".into();
        self.code_input = code;
        if kind == "login" {
            self.screen = Screen::Login { error: Some("服务器没有响应：授权码错误或已失效、地址不对，或本机时间偏差超过 10 分钟".into()) };
            return;
        }
        let now = voc_proto::now_millis();
        let ch = |id: u32, name: &str| ChannelInfo { id, name: name.into() };
        let u = |id: u32, name: &str, c: Option<u32>, muted: bool, deafened: bool, sharing: Option<&str>| UserInfo {
            decoders: vec![],
            id,
            name: name.into(),
            channel_id: c,
            muted,
            deafened,
            sharing: sharing.map(Into::into),
            p2p: true,
            udp: true,
            music: None,
            share_audio: false,
            simulcast: sharing.is_some(),
        };
        let (_tx, events) = mpsc::channel();
        let mut s = Session {
            conn: None,
            last_members: None,
            events,
            user_id: 1,
            server_name: "VOC 服务器".into(),
            channels: vec![ch(1, "大厅"), ch(2, "开黑一号"), ch(3, "开黑二号"), ch(4, "挂机")],
            users: vec![
                u(1, "小明", Some(2), false, false, None),
                UserInfo { share_audio: true, ..u(2, "阿豪", Some(2), false, false, Some("1080p60")) },
                UserInfo { music: Some("网易云音乐".into()), ..u(3, "阿杰", Some(2), true, false, None) },
                u(4, "小鱼", Some(2), false, false, None),
                u(5, "Mia", Some(2), false, true, None),
                u(10, "小林", Some(2), false, false, None),
                UserInfo { udp: false, ..u(11, "大熊", Some(2), false, false, None) },
                u(6, "老王", Some(1), false, false, None),
                u(7, "七七", Some(1), false, false, None),
                u(9, "豆豆", Some(3), false, false, None),
                u(8, "路人甲", None, false, false, None),
            ],
            chat: vec![
                ChatLine::System("小明 进入了语音频道「开黑一号」".into()),
                ChatLine::User { from: "阿豪".into(), text: "今晚八点三角洲，来不来".into(), ts: now - 600_000, mine: false },
                ChatLine::User { from: "阿豪".into(), text: "我先开个屏幕给你们看配装".into(), ts: now - 590_000, mine: false },
                ChatLine::User { from: "小明".into(), text: "来！我把降噪开了，键盘声应该听不到了".into(), ts: now - 420_000, mine: true },
                ChatLine::System("阿豪 开始了屏幕共享".into()),
                ChatLine::User { from: "阿杰".into(), text: "我这边网络有点卡，先闭麦听你们说".into(), ts: now - 120_000, mine: false },
                ChatLine::User { from: "Mia".into(), text: "1080p60 好清楚 👍".into(), ts: now - 30_000, mine: false },
            ],
            chat_input: String::new(),
            new_channel: String::new(),
            server_key: "voc.example.com".into(),
            e2e: None,
            dms: vec![DmThread {
                peer: "阿豪".into(),
                lines: vec![ChatLine::User { from: "阿豪".into(), text: "等下帮我看看配装，别在频道里说".into(), ts: now - 60_000, mine: false }],
                unread: 1,
            }],
            chat_tab: None,
            features: Vec::new(),
        };
        let q = |rtt, loss, jitter, unstable| PeerQuality { rtt_ms: rtt, loss_pct: loss, jitter_ms: jitter, unstable };
        self.demo.quality = [
            (2, (q(23, 0, 3, false), true, true)),
            (3, (q(48, 1, 6, false), true, false)),
            (4, (q(161, 22, 95, true), false, false)),
            (5, (q(37, 0, 4, false), false, false)),
        ]
        .into();
        let code = |n: u8| format!("{n}1834 07215 66390 41827 90512 3{n}648");
        self.demo.e2e = [
            (2, (Trust::Known, code(2), true)),
            (3, (Trust::Known, code(3), true)),
            (4, (Trust::Known, code(4), true)),
            (5, (Trust::Changed, code(5), false)),
            (6, (Trust::Known, code(6), true)),
            (7, (Trust::New, code(7), false)),
        ]
        .into();
        // 小鱼和我打不通，经阿豪中继；我帮阿豪把语音转给 Mia
        let rt = |from, to, via, ms, server_ms| voc_proto::RelayRoute { from, to, via, ms, server_ms };
        self.demo.relays = vec![rt(4, 1, 2, 62, 148), rt(1, 4, 2, 62, 148), rt(2, 5, 1, 64, 133)];
        use voice::PunchState::*;
        self.demo.punch = [
            (2, Connected(q(23, 0, 3, false))),
            (3, Connected(q(48, 1, 6, false))),
            (4, Failed(42)),
            (5, Connected(q(37, 0, 4, false))),
            (10, Punching(3)),
        ]
        .into();
        self.demo.quality.remove(&4); // 小鱼打不通，经阿豪中继
        self.demo.speaking = vec![2];
        self.demo.transport = Some(true);
        self.demo.ping = 49;
        if kind == "viewer" || kind == "fullscreen" {
            let view = Arc::new(View::default());
            let (w, h) = (1280usize, 720usize);
            let mut rgba = vec![0u8; w * h * 4];
            for y in 0..h {
                for x in 0..w {
                    let i = (y * w + x) * 4;
                    rgba[i] = (30 + x * 60 / w) as u8;
                    rgba[i + 1] = (40 + y * 80 / h) as u8;
                    rgba[i + 2] = (90 + (x + y) * 90 / (w + h)) as u8;
                    rgba[i + 3] = 255;
                }
            }
            *view.frame.lock().unwrap() = Some(video::DecodedFrame { width: w, height: h, rgba, seq: 1 });
            self.views.insert(2, Viewer { view, tex: None, seq: 0, size: (0, 0), low: false });
            if kind == "fullscreen" {
                self.fullscreen = Some(2); // 演示只画全屏界面，不真的切换窗口全屏
            }
        }
        s.chat_input = String::new();
        self.screen = Screen::Main(Box::new(s));
        self.show_settings = kind == "settings";
        self.settings_tab = std::env::var("VOC_TAB").ok().and_then(|t| t.parse().ok()).unwrap_or(0);
        self.show_share = kind == "share";
    }

    fn demo_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.demo.shot.clone() else { return };
        self.demo.frames += 1;
        if self.demo.frames >= 30 && !self.demo.shot_requested {
            self.demo.shot_requested = true;
            let c = ctx.clone();
            ctx.request_screenshot(move |img| {
                if let Err(e) = save_png(&path, &img) {
                    eprintln!("截图保存失败：{e}");
                }
                c.send_viewport_cmd(egui::ViewportCommand::Close);
            });
        }
        ctx.request_repaint();
    }
}

fn save_png(path: &str, img: &egui::ColorImage) -> Result<(), String> {
    let [w, h] = img.size;
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().map_err(|e| e.to_string())?;
    let data: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
    wr.write_image_data(&data).map_err(|e| e.to_string())
}

/// Windows 版本（例如 "Microsoft Windows [版本 10.0.26200.xxxx]"）
/// 系统版本，例如「Windows 11 Pro 24H2（内部版本 26100.1234）」。
/// 读注册表（以前用 `cmd /c ver`，中文系统输出 GBK，解出来是乱码）
fn os_version() -> String {
    #[cfg(windows)]
    {
        use windows::core::HSTRING;
        use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ};
        const KEY: &str = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion";
        let sz = |name: &str| -> Option<String> {
            let mut buf = [0u16; 128];
            let mut len = (buf.len() * 2) as u32;
            let ok = unsafe {
                RegGetValueW(
                    HKEY_LOCAL_MACHINE,
                    &HSTRING::from(KEY),
                    &HSTRING::from(name),
                    RRF_RT_REG_SZ,
                    None,
                    Some(buf.as_mut_ptr() as *mut _),
                    Some(&mut len),
                )
            }
            .is_ok();
            let n = (len as usize / 2).saturating_sub(1);
            ok.then(|| String::from_utf16_lossy(&buf[..n]).trim().to_string()).filter(|s| !s.is_empty())
        };
        let ubr = {
            let mut v = 0u32;
            let mut len = 4u32;
            let ok = unsafe {
                RegGetValueW(
                    HKEY_LOCAL_MACHINE,
                    &HSTRING::from(KEY),
                    &HSTRING::from("UBR"),
                    RRF_RT_REG_DWORD,
                    None,
                    Some(&mut v as *mut u32 as *mut _),
                    Some(&mut len),
                )
            }
            .is_ok();
            ok.then_some(v)
        };
        if let (Some(product), Some(build)) = (sz("ProductName"), sz("CurrentBuildNumber")) {
            // Windows 11 的注册表里产品名仍写着 Windows 10
            let product = if build.parse::<u32>().unwrap_or(0) >= 22000 { product.replace("Windows 10", "Windows 11") } else { product };
            let display = sz("DisplayVersion").map(|d| format!(" {d}")).unwrap_or_default();
            let ubr = ubr.map(|u| format!(".{u}")).unwrap_or_default();
            return format!("{product}{display}（内部版本 {build}{ubr}）");
        }
    }
    std::env::consts::OS.to_string()
}

/// 按键说话键的显示名
pub fn vk_name(vk: u32) -> String {
    match vk {
        0x04 => return "鼠标中键".into(),
        0x05 => return "鼠标侧键 1".into(),
        0x06 => return "鼠标侧键 2".into(),
        _ => {}
    }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyNameTextW, MapVirtualKeyW, MAPVK_VK_TO_VSC};
        let sc = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
        let mut buf = [0u16; 64];
        let n = GetKeyNameTextW((sc << 16) as i32, &mut buf);
        if n > 0 {
            return String::from_utf16_lossy(&buf[..n as usize]);
        }
    }
    format!("按键 {vk:#04x}")
}

/// 等待用户按下一个键（键盘或鼠标中键 / 侧键）当作说话键。Esc 取消。左右键不算。
pub fn capture_ptt_key() -> Option<Option<u32>> {
    const VK_ESCAPE: u32 = 0x1B;
    if voice::key_down(VK_ESCAPE) {
        return Some(None);
    }
    (0x04u32..=0xFE).filter(|&vk| vk != VK_ESCAPE && !(0x10..=0x12).contains(&vk)).find(|&vk| voice::key_down(vk)).map(Some)
}

impl Drop for App {
    /// 关闭 VOC：聊天记录覆盖清零后释放，不留在内存里
    fn drop(&mut self) {
        if let Screen::Main(s) = &mut self.screen {
            wipe_chat(&mut s.chat);
            s.dms.iter_mut().for_each(wipe_dm);
            s.dms.clear();
        }
        for (chat, dms) in self.chat_store.values_mut() {
            wipe_chat(chat);
            dms.iter_mut().for_each(wipe_dm);
            dms.clear();
        }
        self.chat_store.clear();
        self.images.wipe_all();
        self.image_view = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ptt_key_names() {
        assert_eq!(vk_name(0x05), "鼠标侧键 1");
        assert_eq!(vk_name(0x06), "鼠标侧键 2");
        assert!(!vk_name(0x41).is_empty()); // A
        assert!(!vk_name(0x14).is_empty()); // CapsLock
    }

    #[test]
    fn reconnect_backoff() {
        let secs: Vec<u64> = (0..8).map(|a| reconnect_delay(a).as_secs()).collect();
        assert_eq!(secs, vec![1, 2, 5, 10, 20, 30, 30, 30]);
    }

    #[test]
    fn chat_is_wiped() {
        let mut t = String::from("密码是 1234");
        let (p, n) = (t.as_ptr(), t.len());
        wipe_string(&mut t);
        assert!(t.is_empty());
        // clear 不释放缓冲区，可以检查原来的字节已经被覆盖成 0
        let old = unsafe { std::slice::from_raw_parts(p, n) };
        assert!(old.iter().all(|&b| b == 0));
        let mut chat = vec![
            ChatLine::User { from: "小明".into(), text: "你好".into(), ts: 0, mine: false },
            ChatLine::System("小明 进入了频道".into()),
        ];
        wipe_chat(&mut chat);
        assert!(chat.is_empty());
    }
}
#[cfg(test)]
mod os_version_probe {
    #[test]
    fn os_version_is_readable() {
        let v = super::os_version();
        println!("os_version = {v}");
        assert!(v.starts_with("Windows") && v.contains("内部版本") && !v.contains('\u{FFFD}'), "{v}");
    }
}
