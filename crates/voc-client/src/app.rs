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
    /// 共享声音的线路（第 54 项）：0 自动（跟语音走），1 总是经服务器，2 总是直连
    share_audio_route: u8,
    /// 共享声音、伴奏用立体声发（第 55-b 项；多占一点流量）
    stream_stereo: bool,
    /// 语音 10 ms 一包（第 36-3 项：少 10 ms 延迟，每条路多约 30 kbps）
    voice_10ms: bool,
    /// 声卡低延迟模式（第 36 项语音第 1 条：IAudioClient3 最小周期，驱动不支持自动退回）
    low_latency_device: bool,
    /// 0.6.2 及以前的按键说话键（虚拟键码，0 = 语音激活）：启动时迁移到 ptt_on / ptt_key 后清零
    ptt_vk: u32,
    /// 说话方式：按键说话（false = 语音激活）。按键单独存，切回语音激活时不丢（第 58 项）
    ptt_on: bool,
    ptt_key: crate::hotkey::Hotkey,
    /// 松开说话键后再多发多久（毫秒，0～1000）
    ptt_tail_ms: u32,
    /// 被 @、收到私聊时弹 Windows 通知
    notify: bool,
    /// 语音音质（千比特每秒，第 68 项）：32 / 64 / 96 / 128，默认 96
    voice_kbps: u32,
    /// 对服务器隐藏昵称（第 60-6 项，默认开）：服务器和旧版本的人只看到「成员-XXXX」，真名只经端到端加密发给成员
    hide_name: bool,
    /// DeepFilterNet3 降噪强度（denoise::DFN_LEVELS 的下标，默认 1 = 高）
    dfn_level: u8,
    /// 0.6.5：老配置里默认的 RNNoise（老版）换成新版一次（第 68 项）
    denoise_v2_migrated: bool,
    /// 观看诊断记录（第 69-d 项）：看共享时每秒把观看数据写一行进日志
    view_diag_log: bool,
    /// 外观（第 59 项）：0 跟随系统，1 浅色，2 深色
    theme: u8,
    /// 配色风格（第 63 项）：theme::STYLES 的下标
    palette: u8,
    /// 毛玻璃效果（第 59 项，默认关）：窗口背景用 Windows 11 的云母，弹框后面的画面模糊
    glass: bool,
    /// 全局快捷键：闭麦 / 静音（游戏里也有效）
    mute_key: crate::hotkey::Hotkey,
    deafen_key: crate::hotkey::Hotkey,
    /// 开始 / 停止共享屏幕、弹出 / 收回观看窗口（第 58 项）
    share_key: crate::hotkey::Hotkey,
    pop_key: crate::hotkey::Hotkey,
    /// 低带宽模式（手机热点、流量少的网络）
    low_bandwidth: bool,
    /// 0.5.x 的手动线路（一个选择）：启动时迁移到 peer_routes 后清空
    routes: HashMap<String, HashMap<String, voice::RoutePref>>,
    /// 0.6 起的手动线路：服务器地址 → 昵称 → 语音 / 画面 / 不透露 IP（没有的按自动）
    peer_routes: HashMap<String, HashMap<String, voice::PeerRoute>>,
    /// 上次所在的语音频道（按服务器），开机自启自动登录后回到这里
    last_channel: HashMap<String, u32>,
    /// 观看屏幕共享时用显卡硬解
    hw_decode: bool,
    /// 观看屏幕共享：流畅优先（匀速播放，默认）/ 延迟优先
    smooth_playback: bool,
    /// 0.5.1 起默认硬解：老配置里的「软解」迁移一次（老配置没有这个字段 → false → 迁移）
    #[serde(default)]
    hw_decode_migrated: bool,
    /// 启动时检查更新
    auto_update: bool,
    /// 中国大陆加速更新：经国内 GitHub 镜像下载
    update_accel: bool,
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

impl Persisted {
    /// 按键页上每个动作现在的按键（顺序见 hotkey::ACTIONS）
    pub(crate) fn keys(&self) -> [crate::hotkey::Hotkey; 5] {
        [self.ptt_key, self.mute_key, self.deafen_key, self.share_key, self.pop_key]
    }

    pub(crate) fn key_mut(&mut self, action: usize) -> &mut crate::hotkey::Hotkey {
        match action {
            crate::hotkey::PTT => &mut self.ptt_key,
            crate::hotkey::MUTE => &mut self.mute_key,
            crate::hotkey::DEAFEN => &mut self.deafen_key,
            crate::hotkey::SHARE => &mut self.share_key,
            _ => &mut self.pop_key,
        }
    }

    /// 交给后台检测线程的切换类快捷键（第 i 位 = 动作 i+1）
    fn toggle_keys(&self) -> Vec<crate::hotkey::Hotkey> {
        self.keys()[1..].to_vec()
    }

    /// 音频线程用的按键说话键（0 = 语音激活）
    pub(crate) fn ptt_packed(&self) -> u32 {
        if self.ptt_on {
            self.ptt_key.pack()
        } else {
            0
        }
    }

    /// 读配置后：解开用 Windows 凭据保护加密的身份种子、授权码（第 41-6 项）。返回解不开的项（换了电脑 / Windows 用户）
    fn unseal_secrets(&mut self) -> Vec<&'static str> {
        let mut lost = Vec::new();
        let mut one = |v: &mut String, what: &'static str| match crate::secret::open(v) {
            Some(p) => *v = p,
            None => {
                v.clear();
                if !lost.contains(&what) {
                    lost.push(what);
                }
            }
        };
        one(&mut self.identity_seed, "加密身份");
        one(&mut self.code, "记住的授权码");
        for s in &mut self.servers {
            one(&mut s.code, "记住的授权码");
        }
        lost
    }

    /// 写配置前：把机密加密，返回原文（写完用 restore_secrets 放回，内存里一直是明文）
    fn seal_secrets(&mut self) -> Vec<String> {
        let mut plain = vec![self.identity_seed.clone(), self.code.clone()];
        plain.extend(self.servers.iter().map(|s| s.code.clone()));
        self.identity_seed = crate::secret::seal(&self.identity_seed);
        self.code = crate::secret::seal(&self.code);
        for s in &mut self.servers {
            s.code = crate::secret::seal(&s.code);
        }
        plain
    }

    fn restore_secrets(&mut self, plain: Vec<String>) {
        let mut it = plain.into_iter();
        self.identity_seed = it.next().unwrap_or_default();
        self.code = it.next().unwrap_or_default();
        for s in &mut self.servers {
            s.code = it.next().unwrap_or_default();
        }
    }
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
            denoise: DenoiseMode::Rnnoise2,
            denoise_v2_migrated: true,
            dfn_level: 1,
            hide_name: true,
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
            share_audio_route: 0,
            stream_stereo: false,
            voice_10ms: false,
            low_latency_device: false,
            ptt_vk: 0,
            ptt_on: false,
            ptt_key: Default::default(),
            ptt_tail_ms: 200,
            notify: true,
            view_diag_log: false,
            voice_kbps: 96,
            theme: 0,
            palette: 2,
            glass: false,
            mute_key: crate::hotkey::default_key(crate::hotkey::MUTE),
            deafen_key: crate::hotkey::default_key(crate::hotkey::DEAFEN),
            share_key: Default::default(),
            pop_key: Default::default(),
            low_bandwidth: false,
            routes: HashMap::new(),
            peer_routes: HashMap::new(),
            last_channel: HashMap::new(),
            hw_decode: true,
            smooth_playback: true,
            hw_decode_migrated: true,
            auto_update: true,
            update_accel: false,
            sfx: true,
            sfx_volume: 0.7,
            identity_seed: String::new(),
            pins: HashMap::new(),
        }
    }
}

enum ChatLine {
    /// id = (发送者, 编号)，对方是旧版本时没有（不能回应）；reactions = 表情 → 回应的人
    User { from: String, text: String, ts: u64, mine: bool, id: Option<(u32, u64)>, reactions: Vec<(String, Vec<u32>)> },
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
    /// 服务器给的名字（隐藏昵称的人是占位名「成员-XXXX」），按编号；显示用的真名套在 users 里（第 60-6 项）
    server_names: HashMap<u32, String>,
    /// 演示模式下为 None
    conn: Option<Conn>,
    /// 服务器说它马上要重启（第 53-5 项）：(说明, 收到的时间)。之后断线就显示这句、马上重连
    restarting: Option<(String, std::time::Instant)>,
    /// 断线恢复凭证（第 37-2 项）：(我的编号, 凭证)；旧服务器没有
    resume: Option<(u32, String)>,
    /// 正在用凭证恢复：(连接结果, 开始的时间)。这期间界面、语音都不断
    resuming: Option<(Receiver<ConnectResult>, std::time::Instant)>,
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

    /// 系统消息里服务器写的占位名换成真名（第 60-6 项；显示时换，真名后到的也能换上）
    pub(crate) fn real_names_in(&self, t: &str) -> String {
        let mut out = t.to_string();
        for u in &self.users {
            if let Some(sn) = self.server_names.get(&u.id) {
                if sn != &u.name && sn.starts_with("成员-") {
                    out = out.replace(sn.as_str(), &u.name);
                }
            }
        }
        out
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
    /// 是服务器重启造成的断开（服务器事先通知了）
    server_restart: bool,
}

/// 第 n 次失败后等多久再试：1、2、5、10、20、30 秒，之后一直 30 秒
/// 断开原因前面加这个标记 = 服务器事先通知了重启（begin_reconnect 去掉它）
const RESTART_MARK: &str = "\u{1}restart\u{1}";

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

/// 消息里有没有 @这个人（名字后面紧跟字母数字的不算，例如 @小明明 不算 @小明）
pub fn mentions(text: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let pat = format!("@{name}");
    text.match_indices(&pat).any(|(i, _)| text[i + pat.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric()))
}

/// 正在输入的 @：返回 @ 后面已经打的字（输入框末尾是 "@xx" 时）
pub fn mention_prefix(input: &str) -> Option<&str> {
    let at = input.rfind('@')?;
    let rest = &input[at + 1..];
    let ok_before = input[..at].chars().last().is_none_or(|c| c.is_whitespace());
    (ok_before && !rest.contains(char::is_whitespace) && rest.chars().count() <= voc_proto::MAX_NAME_LEN).then_some(rest)
}

/// 给某条消息加上 / 取消某人的表情回应
fn apply_reaction(lines: &mut [ChatLine], msg: (u32, u64), emoji: &str, by: u32, on: bool) {
    for l in lines.iter_mut().rev() {
        if let ChatLine::User { id: Some(i), reactions, .. } = l {
            if *i != msg {
                continue;
            }
            match reactions.iter().position(|(e, _)| e == emoji) {
                Some(k) => {
                    let who = &mut reactions[k].1;
                    who.retain(|w| *w != by);
                    if on {
                        who.push(by);
                    }
                    if who.is_empty() {
                        reactions.remove(k);
                    }
                }
                None if on => reactions.push((emoji.to_string(), vec![by])),
                None => {}
            }
            return;
        }
    }
}

/// 告诉服务器的伴奏名（真正的程序名端到端发给成员，见 Profile）
pub const MUSIC_PLACEHOLDER: &str = "伴奏";

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
    /// 共享窗口里选「一个窗口」时的候选窗口（第 60-1 项）
    share_windows: Vec<video::ShareWindow>,
    share_pick_window: bool,
    share_error: Option<String>,
    views: HashMap<u32, ViewerRef>,
    adding_channel: bool,
    settings_tab: u8,
    sfx_seq: u16,
    updater: crate::update::Updater,
    /// 要用这个凭证做断线恢复（下一帧开始，第 37-2 项）
    pending_resume: Option<(u32, String)>,
    /// Wi-Fi 低延迟模式（第 37-1 项）：Some = 在频道里试过（里面的 None = 没有 Wi-Fi / 不支持）
    wifi: Option<Option<crate::wifi::LowLatency>>,
    /// 上次点「复制诊断信息」的时间（显示「已复制」）
    diag_copied: Option<std::time::Instant>,
    /// 诊断信息里附上「编号 ↔ 昵称」对照（默认不附）
    diag_names: bool,
    log_path_copied: Option<std::time::Instant>,
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
    /// 全局快捷键检测
    hotkeys: crate::hotkey::Watcher,
    /// 正在给哪个动作录键（按键页的录键框，第 58 项）
    key_edit: Option<KeyEdit>,
    /// 按键页上的一行提示（例如恢复默认时撞了别的键）
    key_note: Option<String>,
    /// 中间显示语音房间（否则显示文字聊天，第 59 项）；上次看到的语音频道（换频道时自动打开语音房间）
    room_view: bool,
    room_seen_ch: Option<u32>,
    /// 120 帧共享时上行 / 编码跟不上（第 64 项）：从什么时候开始不够、提醒文字、通知弹过没有
    share_bw_low_since: Option<std::time::Instant>,
    share_bw_warn: Option<String>,
    share_bw_toasted: bool,
    view_diag_at: std::time::Instant,
    /// 测上行带宽（第 71 项）
    speed: Arc<Mutex<crate::speedtest::State>>,
    /// 选了 DeepFilterNet3 的「暴力」档：弹「收到！暴力了嗷」
    show_violent: bool,
    /// 聊天搜索：关键字、当前第几条、要不要滚过去
    chat_search: String,
    chat_search_hit: usize,
    chat_search_jump: bool,
    /// 毛玻璃（第 59 项）：这次启动窗口是透明的（云母）；云母已经设过；弹框后面的模糊背景；等截图回来
    glass_window: bool,
    mica_done: bool,
    glass_backdrop: Option<crate::glass::Backdrop>,
    glass_pending: Option<std::time::Instant>,
    glass_shot: Arc<Mutex<Option<egui::ColorImage>>>,
    /// 窗口不在前台时收到的消息数（托盘显示，回到窗口清零）
    unread: u32,
    /// 这一轮收消息攒下的：新消息数、要弹的通知 (标题, 内容)
    unread_new: u32,
    pending_notes: Vec<(String, String)>,
    /// 正在接收的图片
    images: crate::images::ImageStore,
    /// 后台准备好的待发送图片（选图 / 粘贴 / 拖入）
    image_jobs: (mpsc::Sender<Result<crate::images::Prepared, String>>, Receiver<Result<crate::images::Prepared, String>>),
    /// 正在看大图
    image_view: Option<egui::TextureHandle>,
    /// 测试模式：VOC_TEST_SEND_IMAGE 的图片已经发过
    test_image_sent: bool,
    /// 测试模式：VOC_TEST_SHARE 已经开始过共享；VOC_TEST_LOG 上次写日志的时间
    test_share_started: bool,
    test_speed_started: bool,
    test_speed_logged: bool,
    /// 测试：共享者强制 AV1、本机解不了，没去看（第 61 项）
    test_av1_blocked: bool,
    test_log_at: std::time::Instant,
    /// 测试模式：写了几行日志（VOC_TEST_CHAT 按它每 5 秒发一条）
    test_ticks: u32,
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

/// 观看窗口要显示的共享者信息（每帧从成员列表更新一次）
#[derive(Clone, Default)]
pub(crate) struct ViewInfo {
    pub name: String,
    pub desc: String,
    pub simulcast: bool,
    pub share_audio: bool,
    pub p2p_only: bool,
}

/// 观看窗口里点了什么（主循环下一次处理；弹出窗口独立刷新，所以不能直接改主界面的状态）
#[derive(Default)]
pub(crate) struct ViewOut {
    pub relayer: bool,
    pub smooth: Option<bool>,
    pub go_full: bool,
    pub toggle_pop: bool,
    pub close: bool,
}

impl ViewOut {
    pub fn any(&self) -> bool {
        self.relayer || self.smooth.is_some() || self.go_full || self.toggle_pop || self.close
    }
}

/// 弹出的观看窗口独立刷新（第 56-b 项）：主界面和那个窗口都要用，所以放在锁里
pub(crate) type ViewerRef = Arc<Mutex<Viewer>>;

pub(crate) struct Viewer {
    view: Arc<View>,
    tex: Option<egui::TextureHandle>,
    seq: u64,
    size: (usize, usize),
    /// 看低画质层（流畅 360P）
    low: bool,
    /// 画面线路：0 自动，1 P2P 直连，2 走服务器
    route: u8,
    /// 掉帧原因用：上次的累计统计和时间、这一秒的速率
    rx_prev: Option<(crate::vrecv::RxStats, std::time::Instant)>,
    /// 最新一帧是 NV12（在显卡上转颜色画）
    gpu: Option<crate::nv12view::Nv12>,
    /// 弹出成独立窗口了（第 53-4 项）、独立窗口置顶
    popped: bool,
    /// 这一帧嵌在语音房间里画过了（第 59 项）：不再浮观看窗口
    pub(crate) embedded: bool,
    on_top: bool,
    rates: crate::theme::ViewRates,
    /// 实际换上屏幕的帧（第 56-a 项）：最近 1 秒每一帧的时间；从解码交出到画上屏幕的平均等待（ms）
    shown: std::collections::VecDeque<std::time::Instant>,
    paint_ms: f32,
    info: ViewInfo,
    out: ViewOut,
}

impl Viewer {
    pub(crate) fn new_ref(view: Arc<View>, low: bool, route: u8) -> ViewerRef {
        Arc::new(Mutex::new(Viewer {
            view,
            tex: None,
            seq: 0,
            size: (0, 0),
            low,
            route,
            rx_prev: None,
            gpu: None,
            popped: false,
            embedded: false,
            on_top: false,
            rates: Default::default(),
            shown: Default::default(),
            paint_ms: 0.0,
            info: Default::default(),
            out: Default::default(),
        }))
    }

    /// 实际显示的帧数（最近 1 秒）和画面间隔的抖动（ms，间隔的标准差）
    pub(crate) fn shown_stats(&self) -> (u32, f32) {
        let n = self.shown.len();
        if n < 3 {
            return (n as u32, 0.0);
        }
        let iv: Vec<f32> = self.shown.iter().zip(self.shown.iter().skip(1)).map(|(a, b)| b.duration_since(*a).as_secs_f32() * 1000.0).collect();
        let mean = iv.iter().sum::<f32>() / iv.len() as f32;
        let var = iv.iter().map(|x| (x - mean).powi(2)).sum::<f32>() / iv.len() as f32;
        (n as u32, var.sqrt())
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::setup_fonts(&cc.egui_ctx);
        crate::nv12view::init(cc);
        crate::theme::apply(&cc.egui_ctx);
        // 测试模式（VOC_AUTOLOGIN）从默认配置开始、每次新身份：不受本机真实设置影响（例如真实配置开着「只走 P2P」），
        // 几个测试客户端也不会共用同一个身份
        let test_login = std::env::var_os("VOC_AUTOLOGIN").is_some() && std::env::var_os("VOC_DEMO").is_none();
        let mut cfg: Persisted = if test_login {
            Persisted::default()
        } else {
            cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default()
        };
        let lost = cfg.unseal_secrets();
        if !lost.is_empty() {
            crate::applog::log(format!("配置：{} 是在别的电脑 / Windows 用户下加密的，这里解不开，已重置", lost.join("、")));
        }
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
        // 0.6.3：按键说话的键从一个虚拟键码改成可带修饰键的组合，模式单独存（第 58 项）
        if cfg.ptt_vk != 0 {
            cfg.ptt_key = crate::hotkey::Hotkey::key(cfg.ptt_vk);
            cfg.ptt_on = true;
            cfg.ptt_vk = 0;
        }
        cfg.ptt_tail_ms = cfg.ptt_tail_ms.min(1000);
        settings.ptt_key.store(cfg.ptt_packed(), Ordering::Relaxed);
        settings.ptt_tail_ms.store(cfg.ptt_tail_ms, Ordering::Relaxed);
        crate::audio::LOW_BANDWIDTH.store(cfg.low_bandwidth, Ordering::Relaxed);
        crate::audio::VOICE_BITRATE.store(cfg.voice_kbps.clamp(16, 128) * 1000, Ordering::Relaxed);
        settings.input_gain.set(cfg.input_gain);
        settings.output_volume.set(cfg.output_volume);
        // 测试：VOC_TEST_TONE=1 时不用麦克风、自己「一直说话」（见 voice.rs），也不从音箱放声音
        if std::env::var_os("VOC_TEST_TONE").is_some() {
            settings.output_volume.set(0.0);
        }
        let mixer = Arc::new(Mutex::new(Mixer::default()));
        let voice = Arc::new(VoiceState::default());
        // 0.6.5：降噪默认换成 RNNoise 新版（老配置里是老版的改一次；选了别的不动）
        if !cfg.denoise_v2_migrated {
            if cfg.denoise == DenoiseMode::Rnnoise {
                cfg.denoise = DenoiseMode::Rnnoise2;
            }
            cfg.denoise_v2_migrated = true;
        }
        // 0.5.1 起观看默认用显卡解码（出错会自动退回软解）；老配置里默认的「软解」改一次
        if !cfg.hw_decode_migrated {
            cfg.hw_decode = true;
            cfg.hw_decode_migrated = true;
        }
        // 0.6：每人的线路从一个选择分成语音 / 画面 / 不透露 IP（「走服务器」= 不透露 IP，「强制直连」= 语音和画面都强制直连）
        for (server, m) in std::mem::take(&mut cfg.routes) {
            let dst = cfg.peer_routes.entry(server).or_default();
            for (name, r) in m {
                let pr = voice::PeerRoute::from_old(r);
                if !pr.is_default() {
                    dst.entry(name).or_insert(pr);
                }
            }
        }
        video::HW_DECODE.store(cfg.hw_decode, Ordering::Relaxed);
        video::SMOOTH_PLAYBACK.store(cfg.smooth_playback, Ordering::Relaxed);
        audio::LOW_LATENCY_DEVICE.store(cfg.low_latency_device, Ordering::Relaxed);
        voice.video.set_ctx(cc.egui_ctx.clone()); // 收到新视频帧时唤醒界面
        let (frames_tx, frames_rx) = mpsc::sync_channel(100);
        let denoise: DenoiseSlot = Arc::new(Mutex::new(None));
        voice::spawn_encoder(frames_rx, settings.clone(), voice.clone(), mixer.clone(), denoise.clone());

        let hotkeys = crate::hotkey::Watcher::spawn(cfg.toggle_keys(), cc.egui_ctx.clone());
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
            share_windows: Vec::new(),
            share_pick_window: false,
            share_error: None,
            views: HashMap::new(),
            adding_channel: false,
            settings_tab: 0,
            sfx_seq: 0,
            updater: crate::update::Updater::default(),
            wifi: None,
            pending_resume: None,
            diag_copied: None,
            diag_names: false,
            log_path_copied: None,
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
            hotkeys,
            key_edit: None,
            key_note: None,
            room_view: true,
            room_seen_ch: None,
            share_bw_low_since: None,
            share_bw_warn: None,
            share_bw_toasted: false,
            view_diag_at: std::time::Instant::now(),
            speed: Default::default(),
            show_violent: false,
            chat_search: String::new(),
            chat_search_hit: 0,
            chat_search_jump: false,
            glass_window: crate::glass::startup_enabled(),
            mica_done: false,
            glass_backdrop: None,
            glass_pending: None,
            glass_shot: Default::default(),
            unread: 0,
            unread_new: 0,
            pending_notes: Vec::new(),
            images: crate::images::ImageStore::default(),
            image_jobs: mpsc::channel(),
            image_view: None,
            test_image_sent: false,
            test_share_started: false,
            test_speed_started: false,
            test_speed_logged: false,
            test_av1_blocked: false,
            test_log_at: std::time::Instant::now(),
            test_ticks: 0,
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
            app.updater.accel.store(app.cfg.update_accel, Ordering::Relaxed);
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
            // VOC_TEST_NO_P2P：关掉 P2P，测经服务器中转
            if std::env::var_os("VOC_TEST_NO_P2P").is_some() {
                app.cfg.p2p = false;
            }
            app.start_connect(&cc.egui_ctx);
            app.rejoin = parts.get(3).and_then(|c| c.parse().ok());
        } else if app.cfg.auto_update {
            app.updater.check(cc.egui_ctx.clone());
        }
        app
    }

    fn apply_denoise(&mut self) {
        let r = denoise::create(self.cfg.denoise, &self.cfg.denoise_plugin, &self.cfg.denoise_model, self.cfg.dfn_level);
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
        self.spawn_connect_with(None)
    }

    /// resume = 断线恢复凭证（第 37-2 项）
    /// 登录时告诉服务器的名字：隐藏昵称时是按本机身份算的占位名「成员-XXXX」（每次一样，第 60-6 项）
    pub(crate) fn wire_name(&self) -> String {
        if self.cfg.hide_name {
            placeholder_name(&self.identity_seed)
        } else {
            self.cfg.name.trim().to_string()
        }
    }

    fn spawn_connect_with(&self, resume: Option<(u32, String)>) -> Receiver<ConnectResult> {
        let ctx = &self.egui_ctx;
        let addr = self.cfg.server.clone();
        let name = self.wire_name();
        let code = self.code_input.clone();
        let (voice, mixer) = (self.voice.clone(), self.mixer.clone());
        let (tx, rx) = mpsc::channel();
        let ctx2 = ctx.clone();
        std::thread::spawn(move || {
            let notify_ctx = ctx2.clone();
            let mut pcm = Vec::new();
            crate::traffic::reset(); // 「本次流量」从登录起算
            let r = net::connect(
                &addr,
                &name,
                &code,
                resume,
                move || notify_ctx.request_repaint(),
                move |kind, payload| match kind {
                    voc_proto::crypto::KIND_VIDEO => voice::on_server_video(payload, &voice, false),
                    // UDP 不通时经 TCP 收到的伴奏 / 共享声音 / 鼠标
                    voc_proto::crypto::KIND_STREAM => voice::on_tcp_stream(payload, &voice, &mixer, &mut pcm),
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
        // 主动离开：告诉服务器马上移除我（不用保留 20 秒等恢复，第 37-2 项）
        if let Screen::Main(s) = &self.screen {
            s.send(ClientMsg::Bye);
        }
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
        // 服务器事先通知了重启：不等，马上开始重连（连不上再按原来的间隔）
        let server_restart = reason.starts_with(RESTART_MARK);
        let reason = reason.trim_start_matches(RESTART_MARK).to_string();
        self.screen = Screen::Reconnecting(Box::new(Reconnect {
            reason,
            attempt: 0,
            next_at: std::time::Instant::now() + if server_restart { Duration::from_millis(300) } else { reconnect_delay(0) },
            pending: None,
            server_restart,
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
            server_names: HashMap::new(),
            chat_input: String::new(),
            restarting: None,
            resume: None,
            resuming: None,
            new_channel: String::new(),
            server_key,
            e2e: None,
            features: Vec::new(),
        }
    }

    /// 处理收消息时攒下的提醒：VOC 窗口不在前台（缩到托盘 / 在玩游戏）时记未读；
    /// 被 @ 或收到私聊时弹 Windows 通知（安装版），任务栏图标闪一下
    fn flush_notes(&mut self) {
        let notes = std::mem::take(&mut self.pending_notes);
        let new = std::mem::take(&mut self.unread_new);
        let focused = !self.hidden && self.egui_ctx.input(|i| i.viewport().focused.unwrap_or(true));
        if focused || self.demo.active {
            return;
        }
        self.unread += new;
        if !self.cfg.notify || notes.is_empty() {
            return;
        }
        self.egui_ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Informational));
        for (title, body) in notes.into_iter().take(3) {
            let body: String = body.chars().take(80).collect();
            crate::notify::toast(&title, &body);
        }
    }

    /// 观看诊断记录（第 69-d 项）：每秒给每个正在看的共享写一行日志
    fn log_view_diag(&mut self) {
        if !self.cfg.view_diag_log || self.views.is_empty() || self.view_diag_at.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.view_diag_at = std::time::Instant::now();
        for (id, v) in &self.views {
            let Ok(v) = v.try_lock() else { continue };
            let vw = &v.view;
            let (shown, jitter) = v.shown_stats();
            let rx = self.voice.video.rx_stats(*id);
            let (loss, fec, nack, gave_up, keys, rtt) = rx
                .map(|(s, k, r)| ((s.data_lost * 1000 / s.data_total.max(1)) as f32 / 10.0, s.fec_recovered, s.rtx_recovered, s.gaps, k, r))
                .unwrap_or_default();
            let info = vw.sharer_info.lock().unwrap().map(|(i, _)| (i.sent_fps, i.kbps, i.rate_pct, i.congested));
            crate::applog::log(format!(
                "观看诊断 #{id}：收到 {} 交出 {} 显示 {shown} fps · 间隔抖动 {jitter:.1} ms · 解码 {:.1} ms · 显示等待 {} ms · 丢包 {loss}% 冗余救回 {fec} 补发 {nack} 放弃 {gave_up} 要关键帧 {keys} · 往返 {rtt} ms · 共享者(发出帧, kbps, 码率%, 拥塞) {:?}",
                vw.rx_fps.load(Ordering::Relaxed),
                vw.fps.load(Ordering::Relaxed),
                vw.decode_us.load(Ordering::Relaxed) as f32 / 1000.0,
                vw.show_ms.load(Ordering::Relaxed),
                info
            ));
        }
    }

    /// 120 帧共享（第 64 项）：码率比例连续 5 秒低于 70%（上行在降码率）、或者采集 / 发出跟不上 100 帧，就提醒共享者改回 60 帧；
    /// 通知只弹一次（共享者可能在游戏里看不到窗口），不会自动改帧率
    fn check_share_bandwidth(&mut self) {
        let Some(sh) = self.sharer.as_ref().filter(|s| s.cfg.fps > 60) else {
            self.share_bw_low_since = None;
            self.share_bw_warn = None;
            self.share_bw_toasted = false;
            return;
        };
        let st = &sh.stats;
        let pct = st.rate_pct.load(Ordering::Relaxed);
        let (cap, sent) = (st.capture_fps.load(Ordering::Relaxed), st.fps.load(Ordering::Relaxed));
        let started = sh.started.elapsed() > Duration::from_secs(5);
        let fec = st.fec_pct.load(Ordering::Relaxed);
        let why = if pct > 0 && pct < 70 {
            Some(format!("上行带宽不够 120 帧（现在只发得出设定码率的 {pct}%）"))
        } else if st.congested.load(Ordering::Relaxed) {
            Some("上行带宽不够 120 帧（发出去的画面在排队，延迟在涨）".to_string())
        } else if started && fec >= 60 {
            // 冗余包比例是按观看者报的丢包加的：加到这么高说明他们那边丢得很厉害
            Some(format!("观看的人那边丢包很多（冗余包已经加到 {fec}%），带宽可能撑不住 120 帧"))
        } else if started && sent > 0 && sent < 100 && cap >= 100 {
            Some(format!("编码跟不上 120 帧（现在每秒只发出 {sent} 帧）"))
        } else {
            None
        };
        match why {
            Some(w) => {
                let since = *self.share_bw_low_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() >= Duration::from_secs(5) {
                    let text = format!("{w}，建议改回 60 帧");
                    if !self.share_bw_toasted {
                        self.share_bw_toasted = true;
                        crate::applog::log(format!("屏幕共享：{text}"));
                        crate::notify::toast("VOC 屏幕共享", &text);
                    }
                    self.share_bw_warn = Some(text);
                }
            }
            None => {
                self.share_bw_low_since = None;
                self.share_bw_warn = None;
            }
        }
    }

    /// 全局快捷键按下了：切换闭麦 / 静音，响一声提示（游戏里看不到窗口也知道切成功了）
    fn handle_hotkeys(&mut self) {
        self.check_share_bandwidth();
        self.log_view_diag();
        let fired = self.hotkeys.take();
        if fired == 0 {
            return;
        }
        // 第 i 位 = 动作 i+1（见 Persisted::toggle_keys）
        let bit = |a: usize| fired & (1 << (a - 1)) != 0;
        let st = self.settings.clone();
        for (a, flag) in [(crate::hotkey::MUTE, &st.muted), (crate::hotkey::DEAFEN, &st.deafened)] {
            if !bit(a) {
                continue;
            }
            let on = !flag.fetch_xor(true, Ordering::Relaxed);
            self.hotkey_chime(on);
        }
        if bit(crate::hotkey::SHARE) {
            if self.sharer.is_some() {
                self.stop_share();
                self.hotkey_chime(true);
            } else if self.in_voice_channel() {
                match self.start_share() {
                    Ok(()) => self.hotkey_chime(false),
                    Err(e) => crate::notify::toast("VOC", &format!("屏幕共享开不了：{e}")),
                }
            }
        }
        if bit(crate::hotkey::POP) && !self.views.is_empty() {
            // 有没弹出的就全部弹出，否则全部收回
            let pop = self.views.values().any(|v| !v.lock().unwrap().popped);
            for v in self.views.values() {
                let mut v = v.lock().unwrap();
                if v.popped != pop {
                    v.out.toggle_pop = true;
                }
            }
        }
        self.sync_voice_state();
    }

    /// 快捷键提示音：低音 = 闭麦 / 静音 / 停止共享，高音 = 恢复 / 开始共享
    fn hotkey_chime(&mut self, low: bool) {
        if self.cfg.sfx {
            let mut m = self.mixer.lock().unwrap();
            m.set_volume(SFX_ID, self.cfg.sfx_volume);
            for f in audio::chime(if low { Chime::MuteOn } else { Chime::MuteOff }).chunks(voc_proto::FRAME_SAMPLES) {
                self.sfx_seq = self.sfx_seq.wrapping_add(1);
                m.push(SFX_ID, self.sfx_seq, f);
            }
        }
    }

    fn in_voice_channel(&self) -> bool {
        match &self.screen {
            Screen::Main(s) => s.users.iter().any(|u| u.id == s.user_id && u.channel_id.is_some()),
            _ => false,
        }
    }

    /// 按共享设置开始共享屏幕（共享窗口的「开始共享」按钮和快捷键共用）
    pub(crate) fn start_share(&mut self) -> Result<(), String> {
        let Screen::Main(s) = &self.screen else { return Err("还没登录".into()) };
        let eff = self.cfg.share.effective(self.cfg.low_bandwidth);
        match Sharer::start(eff.clone(), self.voice.clone()) {
            Ok(sh) => {
                s.send(ClientMsg::ShareStart {
                    desc: eff.desc(),
                    audio: self.cfg.share_audio && crate::loopback::supported(),
                    simulcast: true,
                    rtx: true,
                    bps: eff.bitrate(),
                    p2p_only: eff.p2p_only,
                });
                self.sharer = Some(sh);
                self.share_confirmed = false;
                self.share_error = None;
                Ok(())
            }
            Err(e) => {
                crate::applog::log(format!("屏幕共享开不了：{e}"));
                self.share_error = Some(e.clone());
                Err(e)
            }
        }
    }

    pub(crate) fn stop_share(&mut self) {
        self.sharer = None;
        self.share_confirmed = false;
        if let Screen::Main(s) = &self.screen {
            s.send(ClientMsg::ShareStop);
        }
    }

    /// 改了快捷键后交给后台检测线程
    fn apply_hotkeys(&self) {
        self.hotkeys.set_keys(self.cfg.toggle_keys());
        self.settings.ptt_key.store(self.cfg.ptt_packed(), Ordering::Relaxed);
        self.settings.ptt_tail_ms.store(self.cfg.ptt_tail_ms, Ordering::Relaxed);
    }

    fn sync_voice_state(&self) {
        if let Screen::Main(s) = &self.screen {
            s.send(ClientMsg::SetState {
                muted: self.settings.muted.load(Ordering::Relaxed),
                deafened: self.settings.deafened.load(Ordering::Relaxed),
            });
        }
    }

    /// 测试模式（配合 VOC_AUTOLOGIN）：
    /// VOC_TEST_SHARE=720 / 1080 进频道后自动共享屏幕；VOC_TEST_WATCH=main / low 自动看第一个在共享的人；
    /// VOC_TEST_LOG=文件 每秒追加一行共享 / 观看状态
    fn test_share_and_watch(&mut self) {
        // VOC_TEST_OUT_VOL=百分比：播放音量（本机两个客户端测共享声音时设 0，免得观看端放出来又被共享端录回去）
        if let Some(v) = std::env::var("VOC_TEST_OUT_VOL").ok().and_then(|v| v.parse::<f32>().ok()) {
            self.settings.output_volume.set(v / 100.0);
        }
        let Screen::Main(s) = &mut self.screen else { return };
        if s.me().and_then(|u| u.channel_id).is_none() {
            return;
        }
        // VOC_TEST_SPEEDTEST=1：登录后测一次上行带宽，结果写进日志（第 71 项）
        if std::env::var_os("VOC_TEST_SPEEDTEST").is_some() && !self.test_speed_started && self.voice.current_link().is_some_and(|l| l.server_rtt_ms() > 0) {
            self.test_speed_started = true;
            let l = self.voice.current_link().unwrap();
            let server_ok = s.features.iter().any(|f| f == voc_proto::FEATURE_SPEEDTEST);
            let server = server_ok.then(|| {
                let (l1, l2) = (l.clone(), l.clone());
                let c: Box<dyn Fn(bool) + Send> = Box::new(move |s| l1.speed_control(s));
                let p: Box<dyn Fn(usize) + Send> = Box::new(move |n| l2.speed_probe(n));
                (l.server_rtt_ms(), c, p)
            });
            crate::speedtest::spawn(self.speed.clone(), self.egui_ctx.clone(), server);
        }
        if self.test_speed_started && !self.test_speed_logged {
            let st = self.speed.lock().unwrap();
            if !st.running && !st.rows.is_empty() {
                self.test_speed_logged = true;
                for r in &st.rows {
                    crate::applog::log(format!("测速：{} 延迟 {:?} ms 上行 {:?} Mbps {} {}", r.name, r.latency_ms, r.mbps, if r.dropped { "（离谱，去掉）" } else { "" }, r.note));
                }
                crate::applog::log(format!("测速：平均 {:?} Mbps", st.chosen));
            }
        }
        if let (Ok(h), false) = (std::env::var("VOC_TEST_SHARE"), self.test_share_started) {
            self.test_share_started = true;
            let mut cfg = self.cfg.share.clone();
            cfg.height = h.parse().unwrap_or(720);
            // VOC_TEST_FPS / VOC_TEST_QUALITY（0 标准 1 高 2 极高）；VOC_TEST_RTX=0 模拟不支持补发的旧版共享者
            let env = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u32>().ok());
            cfg.fps = env("VOC_TEST_FPS").unwrap_or(cfg.fps);
            cfg.quality = env("VOC_TEST_QUALITY").map(|q| q as u8).unwrap_or(cfg.quality);
            // VOC_TEST_SKIP_STATIC=0/1、VOC_TEST_AUTO_RES=0/1
            cfg.skip_static = env("VOC_TEST_SKIP_STATIC").map(|v| v != 0).unwrap_or(cfg.skip_static);
            cfg.auto_res = env("VOC_TEST_AUTO_RES").map(|v| v != 0).unwrap_or(cfg.auto_res);
            cfg.p2p_only = env("VOC_TEST_P2P_ONLY").map(|v| v != 0).unwrap_or(cfg.p2p_only);
            // VOC_TEST_FORCE_AV1=1：强制 AV1（第 61 项）
            cfg.force_av1 = env("VOC_TEST_FORCE_AV1") == Some(1);
            // VOC_TEST_SHARE_WINDOW=标题里的一段字：只共享这个窗口（第 60-1 项）
            if let Ok(t) = std::env::var("VOC_TEST_SHARE_WINDOW") {
                cfg.window = video::window_list().into_iter().find(|w| w.title.contains(&t));
                eprintln!("VOC_TEST_SHARE_WINDOW：{:?}", cfg.window.as_ref().map(|w| (&w.title, &w.exe)));
            }
            let rtx = env("VOC_TEST_RTX") != Some(0);
            // VOC_TEST_SHARE_AUDIO=1 同时共享电脑声音；VOC_TEST_SHARE_AUDIO_ROUTE=0/1/2；VOC_TEST_STEREO=0/1
            let audio = env("VOC_TEST_SHARE_AUDIO") == Some(1);
            self.cfg.share_audio = audio;
            self.cfg.share_audio_route = env("VOC_TEST_SHARE_AUDIO_ROUTE").map(|v| v as u8).unwrap_or(self.cfg.share_audio_route);
            self.cfg.stream_stereo = env("VOC_TEST_STEREO").map(|v| v != 0).unwrap_or(self.cfg.stream_stereo);
            match Sharer::start(cfg.clone(), self.voice.clone()) {
                Ok(sh) => {
                    s.send(ClientMsg::ShareStart { desc: cfg.desc(), audio, simulcast: true, rtx, bps: cfg.bitrate(), p2p_only: cfg.p2p_only });
                    self.sharer = Some(sh);
                }
                Err(e) => eprintln!("VOC_TEST_SHARE 失败：{e}"),
            }
        }
        if let (Ok(mode), true) = (std::env::var("VOC_TEST_WATCH"), self.views.is_empty()) {
            if let Some(u) = s.users.iter().find(|u| u.id != s.user_id && u.sharing.as_deref().is_some_and(|d| video::av1_only(d)) && !video::can_decode_av1()) {
                // 共享者强制 AV1、本机解不了：和界面上一样不去看（第 61 项）
                if !self.test_av1_blocked {
                    self.test_av1_blocked = true;
                    crate::applog::log(format!("测试：{} 的共享只用 AV1，本机解不了，不观看", u.id));
                }
            } else if let Some(id) = s.users.iter().find(|u| u.id != s.user_id && u.sharing.is_some()).map(|u| u.id) {
                let low = mode == "low";
                let route = match std::env::var("VOC_TEST_VIDEO_ROUTE").as_deref() {
                    Ok("p2p") => 1,
                    Ok("server") => 2,
                    _ => 0,
                };
                s.send(ClientMsg::Watch { target: id, on: true, low, route, cursor: true });
                if let Some(view) = self.voice.video.watch(id, true) {
                    let v = Viewer::new_ref(view, low, route);
                    // VOC_TEST_POPPED=1：一开始就弹出成独立窗口（第 56-d 项对比用）
                    v.lock().unwrap().popped = std::env::var_os("VOC_TEST_POPPED").is_some();
                    self.views.insert(id, v);
                }
            }
        }
        let Ok(path) = std::env::var("VOC_TEST_LOG") else { return };
        if self.test_log_at.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.test_log_at = std::time::Instant::now();
        // VOC_TEST_CHAT=1：每 5 秒在频道里发一条文字（端到端加密）
        self.test_ticks += 1;
        if std::env::var_os("VOC_TEST_CHAT").is_some() && self.test_ticks % 5 == 0 {
            if let Some(e2e) = &s.e2e {
                let text = format!("测试消息 {}", self.test_ticks / 5);
                let (id, _, _) = e2e.lock().unwrap().send_chat(&text);
                let me = s.me().map(|u| u.name.clone()).unwrap_or_default();
                s.chat.push(ChatLine::User { from: me, text, ts: voc_proto::now_millis(), mine: true, id: Some((s.user_id, id)), reactions: Vec::new() });
            }
        }
        let chat_in = s.chat.iter().filter(|l| matches!(l, ChatLine::User { mine: false, .. })).count();
        let (img_done, img_all) = s.chat.iter().fold((0, 0), |(d, a), l| match l {
            ChatLine::Image(i) if !i.mine => (d + usize::from(i.receiving.is_none() && i.error.is_none()), a + 1),
            _ => (d, a),
        });
        let mut line = format!(
            "{} 闭麦 {} 静音 {} 聊天收 {chat_in} 图片收 {img_done}/{img_all} 累计字节(语音发,语音收,共享发,共享收) {:?} ",
            voc_proto::now_millis(),
            self.settings.muted.load(Ordering::Relaxed),
            self.settings.deafened.load(Ordering::Relaxed),
            {
                let t = crate::traffic::snapshot();
                (t[0].0, t[0].1, t[1].0, t[1].1)
            }
        );
        if let Some(l) = self.voice.current_link() {
            line += &format!(
                "NAT {:?} 第二探测点 {:?} 线路最大包 {:?} 模拟路由器拦掉 {} 生日攻击 {:?} 候选 {:?} ",
                *l.nat.lock().unwrap(),
                l.nat_detail(),
                l.path_mtu(),
                l.nat_emu.as_ref().map(|e| e.dropped.load(Ordering::Relaxed)).unwrap_or(0),
                l.bday_debug(),
                l.peer_cands_debug()
            );
            // 三人测试：每个人的语音这一秒放了几帧（满分 50）、其中补出来的几帧；中转路线；语音优先改走服务器的人
            let mut rx: Vec<(u32, (u32, u32))> = std::mem::take(&mut *self.voice.voice_rx.lock().unwrap()).into_iter().collect();
            rx.sort();
            line += &format!(
                "语音收 {} 中转 {} 改走服务器 {:?} ",
                rx.iter().map(|(id, (n, c))| format!("#{id}:{n}/{c}")).collect::<Vec<_>>().join(","),
                l.relays().iter().map(|r| format!("#{}→#{}经#{}", r.from, r.to, r.via)).collect::<Vec<_>>().join(","),
                l.server_fallback.lock().unwrap().iter().collect::<Vec<_>>()
            );
            let q: Vec<String> = l.direct_peers().iter().map(|(id, q)| format!("#{id}:{}ms/{}%{}", q.rtt_ms, q.loss_pct, if q.unstable { "不稳" } else { "" })).collect();
            line += &format!("直连质量 {} ", q.join(","));
        }
        if let Some(l) = self.voice.current_link() {
            let (v, d) = l.allocation();
            line += &format!(
                "udp {:?} 语音直连 {v:?} 画面直连 {d:?} 观看者 {} 陌生丢 {} 匀速丢 {} 发送失败 {} 发包 {} 收包 {} 最慢 {} us ",
                self.voice.transport_udp(),
                l.watcher_count(),
                l.unknown_dropped.load(Ordering::Relaxed),
                l.pace_dropped_total(),
                crate::voice::SEND_ERRORS.load(Ordering::Relaxed),
                crate::voice::SENT_PKTS.swap(0, Ordering::Relaxed),
                crate::voice::RECV_PKTS.swap(0, Ordering::Relaxed),
                crate::voice::RECV_MAX_US.swap(0, Ordering::Relaxed)
            );
        }
        if let Some(sh) = &self.sharer {
            let st = &sh.stats;
            line += &format!(
                "| 共享 鼠标单独发 {} 恢复帧 {} 变化区域 {} 占 {}‰ 冗余 {} 静止跳过 {} 降到 {} {}系统给 {} fps 采集 {} fps 发送 {} fps {} kbps {}x{} 编码器 {:?} 方式 {:?} 错误 {:?} 码率比例 {}% 120帧提醒 {:?} ",
                st.cursor_separate.load(Ordering::Relaxed),
                st.ltr_recovers.load(Ordering::Relaxed),
                st.dirty_last.load(Ordering::Relaxed),
                st.dirty_area.load(Ordering::Relaxed),
                st.fec_pct.load(Ordering::Relaxed),
                st.static_skipped.load(Ordering::Relaxed),
                st.auto_height.load(Ordering::Relaxed),
                if st.congested.load(Ordering::Relaxed) { "拥塞 " } else { "" },
                st.source_fps.load(Ordering::Relaxed),
                st.capture_fps.load(Ordering::Relaxed),
                st.fps.load(Ordering::Relaxed),
                st.kbps.load(Ordering::Relaxed),
                st.width.load(Ordering::Relaxed),
                st.height.load(Ordering::Relaxed),
                st.encoder.lock().unwrap(),
                st.pipeline.lock().unwrap(),
                st.error.lock().unwrap(),
                st.rate_pct.load(Ordering::Relaxed),
                self.share_bw_warn
            );
        }
        for (id, v) in &self.views {
            let v = v.lock().unwrap();
            let f = v.view.frame.lock().unwrap();
            let (w, h, seq) = f.as_ref().map(|f| (f.width, f.height, f.seq)).unwrap_or((0, 0, 0));
            line += &format!(
                "| 看 #{id} {w}x{h} 已解 {seq} 帧 {} fps 解码器 {:?} 线路 {:?} 鼠标 {:?} 恢复(校验,补发,放弃,要关键帧,往返,恢复帧) {:?} ",
                v.view.fps.load(Ordering::Relaxed),
                v.view.decoder.lock().unwrap(),
                self.voice.video_direct(*id).map(|d| if d { "直连" } else { "服务器" }),
                v.view.cursor.lock().unwrap().map(|c| (c.0, c.1, c.2, c.3, c.4.elapsed().as_millis())),
                self.voice.video.rx_stats(*id).map(|(s, k, rtt)| (s.fec_recovered, s.rtx_recovered, s.gaps, k, rtt, self.voice.video.recovered_count(*id)))
            );
            // 0.6.2：显卡转颜色、共享者状态、共享声音播放情况
            let sid = crate::audio::stream_mixer_id(voc_proto::udp::SHARE_AUDIO, *id);
            line += &format!(
                "实际显示 {} fps 间隔抖动 {:.1} ms 等着画上屏幕 {:.1} ms 弹出 {} 要补发 {} 片 显卡转颜色 {} 收到 {} fps 显示耗时 {} ms 每帧解码 {} us 共享者 {:?} 共享声音 {:?} ",
                v.shown_stats().0,
                v.shown_stats().1,
                v.paint_ms,
                v.popped,
                self.voice.video.rx_stats(*id).map(|(s, _, _)| s.nack_frags).unwrap_or(0),
                f.as_ref().is_some_and(|f| f.nv12.is_some()),
                v.view.rx_fps.load(Ordering::Relaxed),
                v.view.show_ms.load(Ordering::Relaxed),
                v.view.decode_us.load(Ordering::Relaxed),
                v.view.sharer_info.lock().unwrap().map(|(i, _)| i),
                self.mixer.lock().unwrap().stream_stats(sid)
            );
        }
        if self.share_audio_src.is_some() {
            line += &format!("| 共享声音在发 录音不连续 {} ", crate::loopback::GLITCHES.load(Ordering::Relaxed));
        }
        use std::io::Write;
        if self.test_av1_blocked {
            line += "| 看不了：共享者强制 AV1，本机解不了 AV1 ";
        }
        // 名单（第 60-6 项测试：服务器给的名字 → 显示的名字）
        line += &format!("| 名单 {:?} ", s.users.iter().map(|u| (s.server_names.get(&u.id).cloned().unwrap_or_default(), u.name.clone())).collect::<Vec<_>>());
        if let Some(sh) = &self.sharer {
            line += &format!("| 编码器 {} ", sh.stats.encoder.lock().unwrap());
            if let Some(l) = self.voice.current_link() {
                line += &format!("| 冗余计划 {:?} ", l.fec_plan_debug());
            }
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{line}");
        }
    }

    /// 把手动线路（按昵称记）换成当前的 id 交给链路
    fn sync_routes(&self) {
        let Screen::Main(s) = &self.screen else { return };
        let Some(l) = self.voice.current_link() else { return };
        let id_of = |n: &str| s.users.iter().find(|u| u.name == n && u.id != s.user_id).map(|u| u.id);
        let mut map: HashMap<u32, voice::PeerPref> = match self.cfg.peer_routes.get(&s.server_key) {
            Some(m) => s
                .users
                .iter()
                .filter(|u| u.id != s.user_id)
                .filter_map(|u| m.get(&u.name).filter(|r| !r.is_default()).map(|r| (u.id, voice::PeerPref::from_route(r, id_of))))
                .collect(),
            None => HashMap::new(),
        };
        // 测试：VOC_TEST_VIA="B=C;…" 和 B 的语音经 C 中转（两个方向）
        if self.test_mode {
            if let Ok(v) = std::env::var("VOC_TEST_VIA") {
                for (peer, via) in v.split(';').filter_map(|p| p.split_once('=')) {
                    if let Some(id) = id_of(peer.trim()) {
                        let r = voice::PeerRoute { voice: voice::VoiceRoute::Via(via.trim().to_string()), ..Default::default() };
                        map.insert(id, voice::PeerPref::from_route(&r, id_of));
                    }
                }
            }
        }
        l.set_route_prefs(map);
    }

    /// 诊断信息（给别人排查问题用）：版本、系统、网络、声卡、编解码器。
    /// 不含聊天、密钥、授权码、服务器地址和任何人的 IP。
    fn diagnostics(&self) -> String {
        // 路径里的 Windows 用户名换成 %USERPROFILE%（诊断信息会发给别人）
        fn log_path_for_diag(p: &std::path::Path) -> String {
            let s = p.display().to_string();
            match std::env::var("USERPROFILE") {
                Ok(home) if !home.is_empty() && s.to_lowercase().starts_with(&home.to_lowercase()) => format!("%USERPROFILE%{}", &s[home.len()..]),
                _ => s,
            }
        }
        use std::fmt::Write;
        let mut d = String::new();
        let _ = writeln!(d, "VOC 诊断信息");
        let _ = writeln!(d, "版本：{}", crate::update::VERSION);
        let _ = writeln!(d, "系统：{}", os_version());
        let _ = writeln!(d, "安装方式：{}", if crate::update::is_packaged() { "安装版（MSIX）" } else { "绿色版" });
        let _ = writeln!(d, "时区：{}（日志和聊天时间按这个时区显示）", crate::applog::tz_label());
        if let Some(st) = &self.streams {
            // 播放设备不是 48 kHz 时要变采样率（第 55-a 项）
            let _ = writeln!(d, "声卡：输入 {} · 输出 {}", st.input_desc, st.output_desc);
        }
        let _ = writeln!(d, "\n[网络]");
        let _ = writeln!(d, "Wi-Fi 低延迟模式：{}", crate::wifi::state_text());
        match (&self.screen, self.voice.current_link()) {
            (Screen::Main(s), Some(l)) => {
                let tr = match self.voice.transport_udp() {
                    Some(true) => "UDP",
                    Some(false) => "TCP 隧道（UDP 不通）",
                    None => "未知",
                };
                let _ = writeln!(d, "已连接：是 · 传输 {tr} · 到服务器 {} ms", self.voice.ping_ms.load(Ordering::Relaxed));
                let _ = writeln!(d, "低带宽模式：{}", if self.cfg.low_bandwidth { "开" } else { "关" });
                if let Some(t) = l.nat_detail() {
                    let _ = writeln!(d, "NAT（第二个探测点）：{t}");
                }
                // 第 37-4 项：线路能走多大的包（加密后的 UDP 字节数）
                let (mtus, max_sent) = l.path_mtu();
                if !mtus.is_empty() {
                    let list: Vec<String> = mtus.iter().map(|(k, m)| format!("{} {m}", k.map(|i| format!("#{i}")).unwrap_or("服务器".into()))).collect();
                    let _ = writeln!(d, "线路能走的最大包（字节）：{} · VOC 实际发过最大 {max_sent}", list.join(" · "));
                    for (k, m) in &mtus {
                        if *m > 0 && *m < max_sent {
                            let who = k.map(|i| format!("和 #{i} 的直连")).unwrap_or("到服务器的线路".into());
                            let _ = writeln!(d, "⚠ {who}只能走 {m} 字节的包（可能在用 VPN），比 VOC 发的大包小：大包会被拆开或丢掉，画面可能卡");
                        }
                    }
                }
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
                let _ = writeln!(d, "本次流量：{}", crate::traffic::summary());
                match &self.sharer {
                    Some(sh) => {
                        let st = &sh.stats;
                        let _ = writeln!(
                            d,
                            "屏幕共享：{} {} · 观看 {} 人（其中流畅 {} 人）· 实际 {}×{} {} fps {} kbps · 流畅层码率 {}% · 冗余 {} · {} · {}",
                            sh.cfg.desc(),
                            video::QUALITIES.get(sh.cfg.quality as usize).map(|q| q.0).unwrap_or("?"),
                            l.watcher_count(),
                            st.low_watchers.load(Ordering::Relaxed),
                            st.width.load(Ordering::Relaxed),
                            st.height.load(Ordering::Relaxed),
                            st.fps.load(Ordering::Relaxed),
                            st.kbps.load(Ordering::Relaxed),
                            st.rate_pct.load(Ordering::Relaxed),
                            match st.fec_pct.load(Ordering::Relaxed) {
                                0 => "无".to_string(),
                                p => format!(
                                    "{p}%（观看者原始丢包最高 {:.1}%，两条路同时发 {} 人，累计补发 {} 片，恢复帧 {} 帧）",
                                    st.viewer_loss.load(Ordering::Relaxed) as f32 / 10.0,
                                    st.dual_viewers.load(Ordering::Relaxed),
                                    st.resent_total.load(Ordering::Relaxed),
                                    st.ltr_recovers.load(Ordering::Relaxed)
                                ),
                            },
                            st.encoder.lock().unwrap(),
                            st.pipeline.lock().unwrap(),
                        );
                    }
                    None => {
                        let _ = writeln!(d, "屏幕共享：没在共享");
                    }
                }
                let _ = writeln!(d, "伴奏：{}", if self.music.is_some() { "正在放" } else { "没放" });
                if self.share_audio_src.is_some() {
                    let route = ["自动（跟语音走）", "总是经服务器", "总是直连"][self.cfg.share_audio_route.min(2) as usize];
                    let _ = writeln!(
                        d,
                        "共享声音：在发 · 线路 {route}{} · {} {} kbps · 录音数据不连续 {} 次",
                        if voice::SERVER_SHARE_AUDIO_ROUTE.load(Ordering::Relaxed) { "" } else { "（服务器旧，实际跟画面走）" },
                        if self.cfg.stream_stereo { "立体声" } else { "单声道" },
                        crate::streams::bitrate(self.cfg.stream_stereo, self.cfg.low_bandwidth) / 1000,
                        crate::loopback::GLITCHES.load(Ordering::Relaxed)
                    );
                }
                let loss = self.voice.current_link().map(|l| l.voice_path_loss()).unwrap_or(0);
                let _ = writeln!(
                    d,
                    "语音抗丢包：{}（发出方向线路丢包约 {loss}%）",

                    if self.voice.dred_active.load(Ordering::Relaxed) { "DRED 深度冗余" } else { "带内 FEC" }
                );
                let _ = writeln!(
                    d,
                    "语音包：{}（设置{}）· 同频道都认识带冗余的语音包：{}",
                    if voice::VOICE_10MS.load(Ordering::Relaxed) { "10 ms" } else { "20 ms" },
                    if self.cfg.voice_10ms { "开" } else { "关" },
                    if voice::VOICE_RED_OK.load(Ordering::Relaxed) { "是" } else { "否" }
                );
                let _ = writeln!(d, "在看别人的共享：{} 路", self.views.len());
                if let Some(l) = self.voice.current_link() {
                    let (relaying, relayed) = l.video_relay_info();
                    if !relaying.is_empty() || !relayed.is_empty() {
                        let _ = writeln!(
                            d,
                            "画面转发：我帮人转 {} 路（{}）· 经别人转给我 {} 路（{}）",
                            relaying.len(),
                            relaying.iter().map(|r| format!("#{}→#{}", r.from, r.to)).collect::<Vec<_>>().join(" "),
                            relayed.len(),
                            relayed.iter().map(|r| format!("#{} 经 #{}", r.from, r.via)).collect::<Vec<_>>().join(" ")
                        );
                    }
                }
                for id in self.views.keys() {
                    let sid = crate::audio::stream_mixer_id(voc_proto::udp::SHARE_AUDIO, *id);
                    if let Some(a) = self.mixer.lock().unwrap().stream_stats(sid) {
                        let _ = writeln!(
                            d,
                            "  #{id} 的共享声音：缓冲 {} ms（目标 {}）· 丢包补音 {} 帧 · 播空 {} 次 · 积压丢弃 {} 次 · 播放速度 {:+.1}% · 音量 {:.0}% · 峰值被压低 {} 次",
                            a.buffered_ms,
                            a.target_ms,
                            a.concealed,
                            a.underruns,
                            a.drops,
                            a.rate_permille as f32 / 10.0,
                            self.mixer.lock().unwrap().volume(sid) * 100.0,
                            a.limited
                        );
                    }
                    if let Some((st, keys, rtt)) = self.voice.video.rx_stats(*id) {
                        let rtx_on = self.voice.rtx_sharers.lock().unwrap().contains(id);
                        let loss = if st.data_total > 0 { st.data_lost as f64 * 100.0 / st.data_total as f64 } else { 0.0 };
                        let _ = writeln!(
                            d,
                            "  看 #{id}：原始丢包 {loss:.1}%（{}/{} 片）· 校验包恢复 {} 片 · 请求补发 {} 片、收回 {} 片 · 放弃 {} 帧 · 要关键帧 {} 次 · 靠恢复帧接上 {} 次 · 往返约 {rtt} ms · 对方{}补发",
                            st.data_lost,
                            st.data_total,
                            st.fec_recovered,
                            st.nack_frags,
                            st.rtx_recovered,
                            st.gaps,
                            keys,
                            self.voice.video.recovered_count(*id),
                            if rtx_on { "支持" } else { "不支持（旧版本）" }
                        );
                    }
                }
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
            let ptt = st.ptt_key.load(Ordering::Relaxed);
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
                if ptt == 0 {
                    "语音激活".to_string()
                } else {
                    let k = crate::hotkey::Hotkey::unpack(ptt);
                    format!("按键说话（{}{}，拖尾 {} ms）", k.label(), if k.focus_only { "，仅前台" } else { "" }, st.ptt_tail_ms.load(Ordering::Relaxed))
                },
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
        // 最近的运行日志（不含聊天、授权码、密钥、IP）
        let log = crate::applog::tail(40);
        if !log.is_empty() {
            let _ = writeln!(d, "
[最近日志]
{log}");
        }
        if let Some(p) = crate::applog::path() {
            let _ = writeln!(d, "日志文件：{}", log_path_for_diag(&p));
        }
        // 用户勾了才附：日志和上面都只写编号
        if self.diag_names {
            if let Screen::Main(s) = &self.screen {
                let _ = writeln!(d, "\n[编号 ↔ 昵称]");
                for u in &s.users {
                    let _ = writeln!(d, "#{} = {}{}", u.id, u.name, if u.id == s.user_id { "（我）" } else { "" });
                }
            }
        }
        d
    }

    /// 窗口隐藏时也要跑的事：网络、托盘、关窗口缩托盘、更新、设备拔插
    fn background(&mut self, ctx: &egui::Context) {
        self.handle_hotkeys();
        if self.updater.get() == crate::update::UpdateState::Launched {
            self.quitting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        let dropped = self.poll_network();
        self.flush_notes();
        if let Some(d) = dropped {
            crate::applog::log(format!("和服务器断开：{}", d.reason));
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
                // 等一会儿（约 8 秒，频道里每个人的端到端加密都建好了）再发，测「大家都收到」
                if others > 0 && self.last_device_check.elapsed() > Duration::from_millis(1500) && self.test_ticks >= 8 {
                    self.test_image_sent = true;
                    let p = std::path::PathBuf::from(path);
                    queue_image(&self.image_jobs.0, ctx, move || crate::images::prepare_file(&p));
                }
            }
        }
        if self.test_mode {
            self.test_share_and_watch();
        }
        // 屏幕共享的声音：共享时开，停了就关（录除了 VOC 自己以外的全部声音）
        voice::SHARE_AUDIO_ROUTE.store(self.cfg.share_audio_route.min(2), Ordering::Relaxed);
        // 语音 10 ms 一包 / 带冗余：同频道的人都认识才用（旧版本按 20 ms 一包估抖动、不认识冗余包）
        if let Screen::Main(s) = &self.screen {
            let my_ch = s.me().and_then(|u| u.channel_id);
            let others: Vec<u32> = s.users.iter().filter(|u| u.id != s.user_id && my_ch.is_some() && u.channel_id == my_ch).map(|u| u.id).collect();
            let (ten, red) = match &s.e2e {
                Some(e) => {
                    let e = e.lock().unwrap();
                    (
                        others.iter().all(|id| e.member_has(*id, voc_proto::e2e::FEATURE_VOICE10)),
                        others.iter().all(|id| e.member_has(*id, voc_proto::e2e::FEATURE_VOICE_RED)),
                    )
                }
                None => (false, false),
            };
            voice::VOICE_10MS.store(self.cfg.voice_10ms && ten, Ordering::Relaxed);
            voice::VOICE_RED_OK.store(red, Ordering::Relaxed);
        }
        let want_share_audio = self.sharer.is_some() && self.cfg.share_audio && crate::loopback::supported();
        // 改了立体声 / 单声道：重新开一路
        if self.share_audio_src.as_ref().is_some_and(|s| s.stereo != self.cfg.stream_stereo) {
            self.share_audio_src = None;
        }
        if !want_share_audio {
            self.share_audio_src = None;
            self.share_audio_failed = false;
        } else if self.share_audio_src.is_none() && !self.share_audio_failed {
            // 共享的是窗口、选了只录这个程序：只录它（含子进程）的声音；否则录除了 VOC 以外的全部（第 60-2 项）
            let target = match self.sharer.as_ref().and_then(|s| s.cfg.window.clone()) {
                // UWP 应用的窗口属于 ApplicationFrameHost，声音是另一个进程放的：只录窗口那个进程会没声音，改录全部
                Some(w) if self.cfg.share.audio_app_only && !w.exe.eq_ignore_ascii_case("ApplicationFrameHost.exe") => crate::loopback::Target::Include(w.pid),
                _ => crate::loopback::Target::Exclude(std::process::id()),
            };
            match crate::streams::start(voc_proto::udp::SHARE_AUDIO, target, self.voice.clone(), Arc::new(audio::AtomicF32::new(1.0)), self.cfg.stream_stereo) {
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
            if !self.hidden && ctx.input(|i| i.viewport().focused.unwrap_or(true)) {
                self.unread = 0;
            }
            let status = if self.unread > 0 { format!("{status} · {} 条未读", self.unread) } else { status };
            t.set_state(m, d, &status);
            t.set_unread(self.unread > 0);
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
        match crate::streams::start(voc_proto::udp::MUSIC, crate::loopback::Target::Include(pid), self.voice.clone(), gain.clone(), self.cfg.stream_stereo) {
            Ok(src) => {
                let prev_volume = crate::loopback::app_volume(pid).unwrap_or(1.0);
                if let Screen::Main(s) = &self.screen {
                    // 程序名只端到端发给成员；服务器只知道「在放伴奏」
                    s.send(ClientMsg::SetMusic { name: Some(MUSIC_PLACEHOLDER.into()) });
                    if let Some(e2e) = &s.e2e {
                        e2e.lock().unwrap().set_profile(voc_proto::e2e::Profile { music: Some(name.clone()), ..Default::default() });
                    }
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
                if let Some(e2e) = &s.e2e {
                    e2e.lock().unwrap().set_profile(Default::default());
                }
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
        self.wifi = None;
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

        // 断线恢复（第 37-2 项）：后台在用凭证重连，界面和语音都不动
        if let Some(r) = self.pending_resume.take() {
            let rx = self.spawn_connect_with(Some(r));
            if let Screen::Main(s) = &mut self.screen {
                s.resuming = Some((rx, std::time::Instant::now()));
            }
        }
        if let Screen::Main(s) = &mut self.screen {
            if let Some((rx, at)) = &s.resuming {
                match rx.try_recv() {
                    Ok(Ok((conn, events))) => {
                        if let Some(l) = self.voice.current_link() {
                            l.set_tcp(conn.tx.clone());
                        }
                        s.conn = Some(conn);
                        s.events = events;
                        s.resuming = None;
                    }
                    Ok(Err(e)) => {
                        crate::applog::log(format!("断线恢复没成功（{e}），改成重新登录"));
                        s.resuming = None;
                        return Some(Dropped { reason: "与服务器的连接断开了".into(), retry: true });
                    }
                    Err(mpsc::TryRecvError::Empty) if at.elapsed() > Duration::from_secs(18) => {
                        crate::applog::log("断线恢复超时，改成重新登录");
                        s.resuming = None;
                        return Some(Dropped { reason: "与服务器的连接断开了".into(), retry: true });
                    }
                    Err(mpsc::TryRecvError::Empty) => return None,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        s.resuming = None;
                        return Some(Dropped { reason: "与服务器的连接断开了".into(), retry: true });
                    }
                }
            }
        }
        let Screen::Main(s) = &mut self.screen else { return None };
        while let Ok(ev) = s.events.try_recv() {
            match ev {
                NetEvent::Closed(reason) => {
                    // 服务器事先说了要重启：显示服务器给的说明（不是网络问题）
                    let restarting = s.restarting.as_ref().filter(|(_, at)| at.elapsed() < Duration::from_secs(20)).map(|(t, _)| t.clone());
                    // 网络断了 / 换了网络，服务器给过恢复凭证：先试着接回原来的身份，语音不断（第 37-2 项）
                    if let (None, Some(r), false) = (&restarting, &s.resume, self.demo.active) {
                        crate::applog::log("和服务器的连接断了：用恢复凭证重连（语音照常走 UDP）");
                        s.chat.push(ChatLine::System("网络断了一下，正在恢复连接…（语音尽量不断）".into()));
                        self.pending_resume = Some(r.clone());
                        self.egui_ctx.request_repaint();
                        break;
                    }
                    let reason = match restarting {
                        Some(text) => format!("{RESTART_MARK}{text}"),
                        None => reason,
                    };
                    return Some(Dropped { reason, retry: true });
                }
                NetEvent::File(data) => {
                    let ev = self.images.on_chunk(&data);
                    apply_image_events(s, ev, &self.egui_ctx);
                }
                NetEvent::Msg(msg) => match msg {
                    ServerMsg::Welcome { user_id, udp_port, server_name, features, probe_port, probe2, resume } => {
                        s.resume = (!resume.is_empty()).then(|| (user_id, resume));
                        s.user_id = user_id;
                        self.voice.server_rtx.store(features.iter().any(|f| f == voc_proto::FEATURE_VIDEO_RTX), Ordering::Relaxed);
                        voice::SERVER_SHARE_AUDIO_ROUTE.store(features.iter().any(|f| f == voc_proto::FEATURE_SHARE_AUDIO_ROUTE), Ordering::Relaxed);
                        // VOC_TEST_NO_PUNCH=1：当成旧服务器（对比测试用）
                        let punch = features.iter().any(|f| f == voc_proto::FEATURE_PUNCH) && std::env::var_os("VOC_TEST_NO_PUNCH").is_none();
                        voice::SERVER_PUNCH.store(punch, Ordering::Relaxed);
                        crate::applog::log(format!("登录成功：服务器「{server_name}」，功能 {features:?}，NAT 探测端口 {}", if probe_port > 0 { "有" } else { "没有" }));
                        s.features = features;
                        s.server_name = server_name;
                        let Some(conn) = &s.conn else { continue };
                        let mut udp_addr = conn.server_addr;
                        udp_addr.set_port(udp_port);
                        let pins = self.cfg.pins.get(&s.server_key).cloned().unwrap_or_default();
                        // 身份公告会经过服务器：里面的名字用登录时的名字（隐藏昵称时是占位名），真名只放进加密的资料里（第 60-6 项）
                        let wire = if self.cfg.hide_name {
                            placeholder_name(&self.identity_seed)
                        } else {
                            self.cfg.name.trim().to_string()
                        };
                        let e2e = Arc::new(Mutex::new(E2e::new(user_id, &wire, Identity::from_seed(&self.identity_seed), pins)));
                        e2e.lock().unwrap().set_real_name(self.cfg.name.trim());
                        s.e2e = Some(e2e.clone());
                        if let Some(m) = &self.music {
                            e2e.lock().unwrap().set_profile(voc_proto::e2e::Profile { music: Some(m.name.clone()), ..Default::default() });
                        }
                        if let Err(e) = voice::connect(
                            udp_addr,
                            probe_port,
                            probe2.parse().ok(),
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
                        // 服务器给的是占位名：换成端到端收到的真名（第 60-6 项）；授权码标记交给身份核对
                        s.server_names = s.users.iter().map(|u| (u.id, u.name.clone())).collect();
                        if let Some(e) = &s.e2e {
                            e.lock().unwrap().tags = s.users.iter().map(|u| (u.id, u.tag.clone())).collect();
                        }
                        apply_real_names(s, self.cfg.name.trim());
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
                        // Wi-Fi 低延迟模式：在语音频道里才开，离开频道就关（第 37-1 项）；没有 Wi-Fi 时只试一次
                        if in_ch && self.wifi.is_none() {
                            self.wifi = Some(crate::wifi::LowLatency::enable());
                        } else if !in_ch && self.wifi.is_some() {
                            self.wifi = None;
                        }
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
                        *self.voice.rtx_sharers.lock().unwrap() = s.users.iter().filter(|u| u.rtx).map(|u| u.id).collect();
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
                    ServerMsg::Watchers { ids, low, direct, server, cursor } => {
                        if let Some(l) = self.voice.current_link() {
                            l.set_watchers(ids, low);
                            l.set_watcher_routes(direct, server);
                            l.set_watchers_cursor(cursor);
                        }
                    }
                    ServerMsg::VideoLtr { by, ack, recover } => {
                        if let Some(l) = self.voice.current_link() {
                            l.on_video_ltr(by, ack, recover);
                        }
                    }
                    ServerMsg::VideoReport { by, loss, rtt } => {
                        if let Some(l) = self.voice.current_link() {
                            l.on_video_report(by, loss, rtt);
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
                    ServerMsg::SpeedTestResult { kbps } => *crate::speedtest::SERVER_RESULT.lock().unwrap() = Some(kbps),
                    ServerMsg::ListenLoss { pct } => voice::LISTEN_LOSS.store(pct.min(100), Ordering::Relaxed),
                    ServerMsg::PunchAt { peer, in_ms } => {
                        if let Some(l) = self.voice.current_link() {
                            l.punch_at(peer, in_ms);
                        }
                    }
                    ServerMsg::Relays { routes } => {
                        if let Some(l) = self.voice.current_link() {
                            l.set_relays(routes);
                        }
                    }
                    ServerMsg::Resumed { user_id } => {
                        crate::applog::log(format!("断线恢复成功（#{user_id}），语音没有重新建立"));
                        s.chat.push(ChatLine::System("已恢复连接".into()));
                    }
                    ServerMsg::Restarting { text } => {
                        let text = if text.is_empty() { "服务器正在重启，几秒后会自动重新连上".to_string() } else { text };
                        crate::applog::log(format!("服务器通知：{text}"));
                        s.chat.push(ChatLine::System(text.clone()));
                        s.restarting = Some((text, std::time::Instant::now()));
                    }
                    ServerMsg::VoiceFallback { ids } => {
                        if let Some(l) = self.voice.current_link() {
                            l.set_server_fallback(ids);
                        }
                    }
                    ServerMsg::VideoRelays { routes } => {
                        if let Some(l) = self.voice.current_link() {
                            l.set_video_relays(routes);
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
                                let my_name = s.me().map(|u| u.name.clone()).unwrap_or_default();
                                // 私聊、或频道里有人 @我：VOC 不在前台时提醒
                                if c.dm || mentions(&c.text, &my_name) {
                                    let title = if c.dm { format!("{from} 发来私聊") } else { format!("{from} 在频道里 @了你") };
                                    self.pending_notes.push((title, c.text.clone()));
                                }
                                self.unread_new += 1;
                                let line = ChatLine::User { from: from.clone(), text: c.text, ts, mine: false, id: c.id.map(|i| (c.from, i)), reactions: Vec::new() };
                                if c.dm {
                                    s.push_dm(&from, line);
                                } else {
                                    s.chat.push(line);
                                }
                            }
                            Some(crate::e2e::BlobIn::Image { from, meta, dm }) => {
                                self.unread_new += 1;
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
                            Some(crate::e2e::BlobIn::React { by, r }) => {
                                let peer = s.name_of(by);
                                let lines = if r.dm { s.lines_for(Some(&peer)) } else { &mut s.chat };
                                apply_reaction(lines, (r.from, r.id), &r.emoji, by, r.on);
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
            tag: String::new(),
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
            rtx: false,
            share_p2p_only: false,
        };
        let (_tx, events) = mpsc::channel();
        let mut s = Session {
            conn: None,
            server_names: HashMap::new(),
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
                ChatLine::User {
                    from: "阿豪".into(),
                    text: "@小明 今晚八点三角洲，来不来".into(),
                    ts: now - 600_000,
                    mine: false,
                    id: Some((2, 1)),
                    reactions: vec![("👍".into(), vec![1, 3, 4])],
                },
                ChatLine::User { from: "阿豪".into(), text: "我先开个屏幕给你们看配装".into(), ts: now - 590_000, mine: false, id: None, reactions: Vec::new() },
                ChatLine::User { from: "小明".into(), text: "来！我把降噪开了，键盘声应该听不到了".into(), ts: now - 420_000, mine: true, id: None, reactions: Vec::new() },
                ChatLine::System("阿豪 开始了屏幕共享".into()),
                ChatLine::User { from: "阿杰".into(), text: "我这边网络有点卡，先闭麦听你们说".into(), ts: now - 120_000, mine: false, id: None, reactions: Vec::new() },
                ChatLine::User { from: "Mia".into(), text: "1080p60 好清楚 👍".into(), ts: now - 30_000, mine: false, id: Some((5, 1)), reactions: vec![("❤".into(), vec![2])] },
            ],
            chat_input: String::new(),
            restarting: None,
            resume: None,
            resuming: None,
            new_channel: String::new(),
            server_key: "voc.example.com".into(),
            e2e: None,
            dms: vec![DmThread {
                peer: "阿豪".into(),
                lines: vec![ChatLine::User { from: "阿豪".into(), text: "等下帮我看看配装，别在频道里说".into(), ts: now - 60_000, mine: false, id: None, reactions: Vec::new() }],
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
        let rt = |from, to, via, ms, server_ms| voc_proto::RelayRoute { from, to, via, ms, server_ms, manual: false };
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
            *view.frame.lock().unwrap() = Some(video::DecodedFrame { width: w, height: h, rgba, nv12: None, seq: 1, at: std::time::Instant::now() });
            self.views.insert(2, Viewer::new_ref(view, false, 0));
            if kind == "fullscreen" {
                self.fullscreen = Some(2); // 演示只画全屏界面，不真的切换窗口全屏
            }
        }
        s.chat_input = String::new();
        self.screen = Screen::Main(Box::new(s));
        self.show_settings = kind == "settings";
        self.settings_tab = std::env::var("VOC_TAB").ok().and_then(|t| t.parse().ok()).unwrap_or(0);
        // VOC_THEME=1 浅色 / 2 深色（截图用）
        if let Some(t) = std::env::var("VOC_THEME").ok().and_then(|t| t.parse().ok()) {
            self.cfg.theme = t;
        }
        if let Some(t) = std::env::var("VOC_PALETTE").ok().and_then(|t| t.parse().ok()) {
            self.cfg.palette = t;
        }
        // VOC_SEARCH=关键字：聊天搜索框里先填好（截图用）
        if let Ok(q) = std::env::var("VOC_SEARCH") {
            self.chat_search = q;
            self.chat_search_hit = usize::MAX;
            self.chat_search_jump = true;
        }
        // VOC_GLASS=1：弹框后面的毛玻璃（截图用）
        self.cfg.glass = std::env::var_os("VOC_GLASS").is_some();
        // VOC_DEMO_KEYS=1：按键页演示冲突提示；=2 再打开录键框（第 58 项）
        if let Ok(k) = std::env::var("VOC_DEMO_KEYS") {
            use crate::hotkey::Hotkey;
            self.cfg.ptt_on = true;
            self.cfg.ptt_key = Hotkey::key(0x4D);
            self.cfg.mute_key = crate::hotkey::default_key(crate::hotkey::MUTE);
            self.cfg.deafen_key = Hotkey { vk: 0x44, ctrl: true, ..Default::default() };
            self.cfg.share_key = Hotkey::key(0x77);
            if k == "2" {
                self.begin_key_edit(crate::hotkey::SHARE);
            }
        }
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

/// 隐藏昵称时告诉服务器的占位名（第 60-6 项）：按本机身份算，每次一样；6 位十六进制，几个人撞名（服务器会拒绝登录）的机会很小
pub(crate) fn placeholder_name(seed: &[u8; 32]) -> String {
    let h = seed.iter().fold(0x811c_9dc5u32, |h, b| (h ^ *b as u32).wrapping_mul(0x0100_0193));
    format!("成员-{:06X}", h & 0xFF_FFFF)
}

/// 把服务器给的名字换成显示用的名字（第 60-6 项）：自己用本机的昵称，别人用端到端收到的真名（还没收到就用服务器给的），
/// 同名的后面加「 #编号」区分。以 server_names 为底，重复调用结果一样
fn apply_real_names(s: &mut Session, my_real: &str) {
    let profiles: HashMap<u32, String> = match &s.e2e {
        Some(e) => {
            let mut e = e.lock().unwrap();
            e.profiles_dirty = false;
            e.profiles.iter().filter_map(|(id, p)| p.name.clone().map(|n| (*id, n))).collect()
        }
        None => HashMap::new(),
    };
    let me = s.user_id;
    for u in s.users.iter_mut() {
        let base = s.server_names.get(&u.id).cloned().unwrap_or_else(|| u.name.clone());
        u.name = if u.id == me && !my_real.is_empty() { my_real.to_string() } else { profiles.get(&u.id).cloned().unwrap_or(base) };
    }
    let mut count: HashMap<String, u32> = HashMap::new();
    for u in &s.users {
        *count.entry(u.name.clone()).or_default() += 1;
    }
    for u in s.users.iter_mut() {
        if count.get(&u.name).copied().unwrap_or(0) > 1 {
            u.name = format!("{} #{}", u.name, u.id);
        }
    }
}

/// 按键页的录键框（第 58 项）：录哪个动作、录到的键、有什么问题
pub(crate) struct KeyEdit {
    pub action: usize,
    pub cap: crate::hotkey::Capture,
    /// 录完了、等用户处理问题（换过来 / 仍然使用 / 重新录）
    pub got: Option<(crate::hotkey::Hotkey, crate::hotkey::Problem)>,
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
    fn mention_detection() {
        assert!(mentions("@小明 来一下", "小明"));
        assert!(mentions("快来@小明", "小明"));
        assert!(mentions("@小明，开黑", "小明"));
        assert!(!mentions("@小明明 来", "小明"), "longer name is someone else");
        assert!(!mentions("小明 来", "小明"));
        assert!(!mentions("@ 来", ""));
        assert_eq!(mention_prefix("你好 @小"), Some("小"));
        assert_eq!(mention_prefix("@"), Some(""));
        assert_eq!(mention_prefix("邮箱a@b"), None, "not after a space");
        assert_eq!(mention_prefix("@小明 好"), None, "already finished");
    }

    #[test]
    fn reactions_toggle() {
        let mut lines = vec![ChatLine::User { from: "a".into(), text: "x".into(), ts: 0, mine: false, id: Some((1, 5)), reactions: Vec::new() }];
        let get = |l: &[ChatLine]| match &l[0] {
            ChatLine::User { reactions, .. } => reactions.clone(),
            _ => unreachable!(),
        };
        apply_reaction(&mut lines, (1, 5), "👍", 2, true);
        apply_reaction(&mut lines, (1, 5), "👍", 3, true);
        apply_reaction(&mut lines, (1, 5), "👍", 3, true); // 重复不算两次
        apply_reaction(&mut lines, (1, 6), "👍", 3, true); // 别的消息
        assert_eq!(get(&lines), vec![("👍".to_string(), vec![2, 3])]);
        apply_reaction(&mut lines, (1, 5), "👍", 2, false);
        apply_reaction(&mut lines, (1, 5), "👍", 3, false);
        assert!(get(&lines).is_empty());
    }

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
            ChatLine::User { from: "小明".into(), text: "你好".into(), ts: 0, mine: false, id: None, reactions: Vec::new() },
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
