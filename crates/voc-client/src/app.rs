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
    chat_input: String,
    new_channel: String,
    /// 用于记住身份的服务器标识（登录时填的地址）
    server_key: String,
    e2e: Option<Arc<Mutex<E2e>>>,
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
    demo: Demo,
}

struct Viewer {
    view: Arc<View>,
    tex: Option<egui::TextureHandle>,
    seq: u64,
    size: (usize, usize),
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
            demo: Demo::default(),
        };
        app.restart_audio();
        app.apply_denoise();
        if let Ok(kind) = std::env::var("VOC_DEMO") {
            app.load_demo(&kind);
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
        let (s, errs) = audio::start_streams(
            self.cfg.input_device.as_deref(),
            self.cfg.output_device.as_deref(),
            self.frames_tx.clone(),
            self.mixer.clone(),
            self.settings.clone(),
        );
        self.streams = Some(s);
        self.audio_errors = errs;
    }

    fn refresh_devices(&mut self) {
        self.input_devices = audio::input_device_names();
        self.output_devices = audio::output_device_names();
    }

    fn start_connect(&mut self, ctx: &egui::Context) {
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
        self.screen = Screen::Connecting(rx);
    }

    fn sync_voice_state(&self) {
        if let Screen::Main(s) = &self.screen {
            s.send(ClientMsg::SetState {
                muted: self.settings.muted.load(Ordering::Relaxed),
                deafened: self.settings.deafened.load(Ordering::Relaxed),
            });
        }
    }

    fn go_login(&mut self, error: Option<String>) {
        self.sharer = None;
        self.share_confirmed = false;
        self.views.clear();
        self.voice.disconnect();
        self.mixer.lock().unwrap().clear();
        self.screen = Screen::Login { error };
    }

    /// 处理网络事件；返回 Some(原因) 表示连接已断开
    fn poll_network(&mut self) -> Option<String> {
        // 连接中
        if let Screen::Connecting(rx) = &self.screen {
            match rx.try_recv() {
                Ok(Ok((conn, events))) => {
                    self.screen = Screen::Main(Box::new(Session {
                        conn: Some(conn),
                        last_members: None,
                        events,
                        user_id: 0,
                        server_name: String::new(),
                        channels: Vec::new(),
                        users: Vec::new(),
                        chat: Vec::new(),
                        chat_input: String::new(),
                        new_channel: String::new(),
                        server_key: self.cfg.server.trim().to_string(),
                        e2e: None,
                    }));
                }
                Ok(Err(e)) => return Some(e),
                Err(mpsc::TryRecvError::Empty) => return None,
                Err(mpsc::TryRecvError::Disconnected) => return Some("连接线程异常退出".into()),
            }
        }

        let Screen::Main(s) = &mut self.screen else { return None };
        while let Ok(ev) = s.events.try_recv() {
            match ev {
                NetEvent::Closed(reason) => return Some(reason),
                NetEvent::Msg(msg) => match msg {
                    ServerMsg::Welcome { user_id, udp_port, server_name } => {
                        s.user_id = user_id;
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
                        }
                        s.send(ClientMsg::Caps { decoders: video::local_decoders().iter().map(|c| c.key().to_string()).collect() });
                        s.send(ClientMsg::SetState {
                            muted: self.settings.muted.load(Ordering::Relaxed),
                            deafened: self.settings.deafened.load(Ordering::Relaxed),
                        });
                    }
                    ServerMsg::Error { message } => return Some(message),
                    ServerMsg::State { channels, users } => {
                        s.channels = channels;
                        s.users = users;
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
                    ServerMsg::Watchers { ids } => {
                        if let Some(l) = self.voice.current_link() {
                            l.set_watchers(ids);
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
                        if let Some(c) = msg {
                            let from = s.name_of(c.from);
                            s.chat.push(ChatLine::User { from, text: c.text, ts, mine: false });
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
                u(2, "阿豪", Some(2), false, false, Some("1080p60")),
                u(3, "阿杰", Some(2), true, false, None),
                u(4, "小鱼", Some(2), false, false, None),
                u(5, "Mia", Some(2), false, true, None),
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
        self.demo.speaking = vec![2];
        self.demo.transport = Some(true);
        self.demo.ping = 49;
        if kind == "viewer" {
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
            self.views.insert(2, Viewer { view, tex: None, seq: 0, size: (0, 0) });
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
