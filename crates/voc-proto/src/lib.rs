//! VOC 共享协议。传输层（握手、加密、抗 DPI）见 [`crypto`]，授权码见 [`auth`]。
//!
//! - 控制通道：TCP，加密帧内是 JSON（`ClientMsg` / `ServerMsg`）。
//! - 语音通道：UDP（与 TCP 同端口号），加密包内容见 [`udp`]；UDP 不通时自动改走 TCP。
//! - 音频：16 kHz 单声道，20 ms 一帧（320 采样），Opus 编码（客户端负责编解码，服务器只转发）。
//! - P2P：同频道成员可以 UDP 打洞直连语音，见 [`p2p`]。
//! - 端到端加密：语音和聊天服务器都看不到，见 [`e2e`]。

use serde::{Deserialize, Serialize};

pub mod auth;
pub mod backup;
pub mod crypto;
pub mod e2e;
pub mod file;
pub mod p2p;
pub mod udp;

/// 7 = 0.6.0（和 0.5.x 不兼容：0.6 的服务器只让 0.6 的客户端登录）
pub const PROTOCOL_VERSION: u32 = 7;

/// 服务器拒绝协议版本不对的客户端时说的话（旧客户端照原样显示）
pub fn version_mismatch_text(server: u32, client: u32) -> String {
    if client < server {
        format!("服务器已经升级（协议 {server}），你的 VOC 太旧（协议 {client}），请点「检查更新」或到 GitHub 下载新版")
    } else {
        format!("服务器版本太旧（协议 {server}，你的 VOC 是 {client}），请管理员在服务器上运行 sudo voc upgrade")
    }
}

/// 客户端看到登录失败消息时换成更准确的说法：旧服务器对比它新的客户端也只会说「请更新客户端」
pub fn explain_login_error(message: &str) -> String {
    // 旧服务器的原话：「协议版本不匹配（服务器 6，客户端 7），请更新客户端」
    let server = message
        .strip_prefix("协议版本不匹配（服务器 ")
        .and_then(|r| r.split('，').next())
        .and_then(|n| n.trim().parse::<u32>().ok());
    match server {
        Some(s) if s < PROTOCOL_VERSION => version_mismatch_text(s, PROTOCOL_VERSION),
        _ => message.to_string(),
    }
}
pub const DEFAULT_PORT: u16 = 7878;
/// 语音编解码 / 混音的采样率。Opus 码流和采样率无关：16 kHz 的旧版客户端和 48 kHz 的新版可以互相解码。
pub const SAMPLE_RATE: u32 = 48_000;
/// 20 ms @ 48 kHz
pub const FRAME_SAMPLES: usize = 960;
pub const MAX_NAME_LEN: usize = 32;
/// 单个语音频道的人数上限
pub const MAX_CHANNEL_USERS: usize = 20;
pub const MAX_CHAT_LEN: usize = 2000;
pub const MAX_LINE_LEN: usize = 64 * 1024;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        name: String,
        version: u32,
        /// 会用恢复凭证（第 37-2 项）：断线后服务器先保留 20 秒，重连时带上 resume 就接回原来的身份
        #[serde(default)]
        can_resume: bool,
        /// 换了网络 / 断了一下：接回原来的身份（编号、频道、UDP 密钥都不变，语音不用重新建立）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume: Option<ResumeToken>,
    },
    /// 主动退出（关闭 / 退出登录）：服务器马上把我移除，不保留 20 秒
    Bye,
    /// 和某人第一次打洞的结果（第 62-1 项，服务器有 FEATURE_PUNCH 才发）：ok = 打通了，ms = 用了多久（没通时是放弃前打了多久）。
    /// 服务器按双方的 NAT 类型汇总打通率，不记 IP
    PunchResult {
        peer: u32,
        ok: bool,
        ms: u32,
    },
    /// 告诉服务器语音走 UDP 还是 TCP（UDP 被封锁/限速时自动切到 TCP）
    Transport {
        udp: bool,
    },
    JoinChannel {
        channel_id: u32,
    },
    LeaveChannel,
    SetState {
        muted: bool,
        deafened: bool,
    },
    CreateChannel {
        name: String,
    },
    Ping,
    /// 公布端到端加密身份（登录后立即发送）
    E2eAnnounce {
        entry: E2eEntry,
    },
    /// 开启 P2P：本机候选地址（局域网 / IPv6）；nat = 自测的 NAT 类型（打洞策略用，测出来后再发一次）
    P2pEnable {
        local_addrs: Vec<String>,
        #[serde(default)]
        nat: Option<NatInfo>,
    },
    P2pDisable,
    /// 我选了和这些人「走服务器」：不要把我的地址给他们，也不要把他们的给我（隐私）
    P2pHide {
        ids: Vec<u32>,
    },
    /// 给某个成员的 ML-KEM 密文与签名（base64），服务器原样转发
    PeerKey {
        to: u32,
        ct: String,
        sig: String,
    },
    /// 给某个成员的端到端加密信封（语音密钥 / 聊天，base64），服务器原样转发
    E2eBlob {
        to: u32,
        blob: String,
    },
    /// 已与这些成员直连：服务器不再替我转发给他们。
    /// ids = 语音直连的人；video = 屏幕共享画面直连的人（两者按上行带宽预算分别分配）
    DirectPeers {
        ids: Vec<u32>,
        #[serde(default)]
        video: Vec<u32>,
        /// 屏幕共享声音直连的人（服务器不再转给他们）；None = 跟 video 一样（旧客户端）。
        /// 只在服务器有 FEATURE_SHARE_AUDIO_ROUTE 时发
        #[serde(default, skip_serializing_if = "Option::is_none")]
        share_audio: Option<Vec<u32>>,
    },
    /// 开始屏幕共享（desc 例如 "1080p60"，显示给别人看；audio = 同时共享电脑声音；
    /// simulcast = 能同时发一路 360P 低画质层给想省流量的观看者）
    ShareStart {
        desc: String,
        #[serde(default)]
        audio: bool,
        #[serde(default)]
        simulcast: bool,
        /// 支持丢包补发（NACK）和冗余校验包（FEC）
        #[serde(default)]
        rtx: bool,
        /// 原画码率（bps），服务器安排观看者互相转发时估算上行用
        #[serde(default)]
        bps: u32,
        /// 只走 P2P（0.6 起）：画面、补发、鼠标、共享声音都不经服务器；打不通直连、也没有观看者能转给他的人看不了
        #[serde(default)]
        p2p_only: bool,
    },
    /// 开始 / 停止伴奏（name = 程序名，None = 停止）
    SetMusic {
        name: Option<String>,
    },
    ShareStop,
    /// 开始 / 停止观看某人的屏幕；low = 要低画质层（共享者支持时）。观看中再发一次可以切换画质
    Watch {
        target: u32,
        on: bool,
        #[serde(default)]
        low: bool,
        /// 观看者选的画面线路：0 自动，1 P2P 直连（打得通就直连，不看上行预算），2 走服务器
        #[serde(default)]
        route: u8,
        /// 我会自己画鼠标（共享者可以不把鼠标画进画面，单独发位置）
        #[serde(default)]
        cursor: bool,
    },
    /// 观看者丢包后请求共享者发关键帧
    KeyframeRequest {
        target: u32,
    },
    /// 长期参考帧：ack = 解码成功的长期参考帧帧号；recover = 丢了帧，请求一帧恢复帧（代替关键帧）
    VideoLtr {
        target: u32,
        #[serde(default)]
        ack: Option<u32>,
        #[serde(default)]
        recover: bool,
    },
    /// 观看者每秒回报一次收某人画面的情况（共享者据此决定加多少冗余、要不要两条路同时发）：
    /// loss = 原始丢包率（千分之几，冗余和补发之前）；rtt = 估计的往返时间（ms）
    VideoReport {
        target: u32,
        loss: u16,
        #[serde(default)]
        rtt: u32,
    },
    /// 本机能解码的视频格式（"av1" / "hevc" / "h264"），共享者据此选择编码格式
    Caps {
        decoders: Vec<String>,
    },
    /// 接下来上传的这张图片（id）只转给 to 一个人（私聊发图）。需要服务器支持 FEATURE_DM_FILE
    FileTarget {
        id: u64,
        to: u32,
    },
    /// 每 2 秒上报一次链路质量，服务器据此安排成员互相中转语音（成员中继）。
    /// server_rtt：到服务器的往返延迟；peers：已打通且稳定的直连；relay_slots：还能帮别人中转几路语音（0 = 不当中继）
    LinkStats {
        server_rtt: u32,
        peers: Vec<PeerStat>,
        relay_slots: u16,
        /// 用户手动选了「走服务器」的人：我和他之间的语音不要安排成员中继
        #[serde(default)]
        via_server: Vec<u32>,
        /// 还能拿出来帮别人转发屏幕共享画面的上行（bps）；0 = 不当画面中继
        #[serde(default)]
        video_relay_bps: u32,
        /// 0.6 语音优先：我最近 2 秒在说话（发出了语音）
        #[serde(default)]
        talking: bool,
        /// 0.6 语音优先：最近 3 秒实际听到了（解出了）谁的语音。服务器发现「他在说、名单上说是直连 / 中转、我却没听到」
        /// 就让说话的人改经服务器发给我（VoiceFallback），不依赖链路判断正确
        #[serde(default)]
        heard: Vec<u32>,
        /// 0.6 用户手动指定的语音中转：[对方, 经谁]（两个方向都经这个人；服务器安排不了就走服务器）
        #[serde(default)]
        manual_via: Vec<[u32; 2]>,
        /// 0.6 按听众保护语音：我经服务器收语音的丢包率（%）。高的话服务器只给我多发一份，不把开销摊给所有人
        #[serde(default)]
        voice_loss: u8,
    },
}

/// 一条直连链路的质量（最近 10 秒）
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerStat {
    pub id: u32,
    pub rtt: u32,
    pub loss: u8,
    pub jitter: u32,
}

/// 成员中继路线：from 的语音由 via 直连转发给 to（服务器不再转发这一路）
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelayRoute {
    pub from: u32,
    pub to: u32,
    pub via: u32,
    /// 经中继的预计往返延迟（ms）：R–A 往返 + R–B 往返 + 转发开销
    #[serde(default)]
    pub ms: u32,
    /// 同一对人走服务器中转的预计往返延迟（ms）：A–服务器往返 + B–服务器往返。界面用来对比
    #[serde(default)]
    pub server_ms: u32,
    /// 有人手动指定了「经 via 中转」（0.6.1）：from 这边就算能直连 to，语音也要改经 via（两个方向都按指定的走）
    #[serde(default)]
    pub manual: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// 观看者互相转发屏幕共享画面的路线：from = 共享者，via 把直连收到的画面转给 to（服务器不再转发这一路）。
    /// 只发给路线上的三个人（有变化才发）
    VideoRelays {
        routes: Vec<RelayRoute>,
    },
    Welcome {
        user_id: u32,
        udp_port: u16,
        server_name: String,
        /// 服务器支持的新功能（旧服务器为空），见 FEATURE_*
        #[serde(default)]
        features: Vec<String>,
        /// NAT 探测端口（UDP，只回答「我看到你的地址」）；0 = 没有
        #[serde(default)]
        probe_port: u16,
        /// 第二个公网 IP 上的探测点「IP:端口」（第 39-2 项；服务器有第二个公网 IP 才有，空 = 没有）
        #[serde(default)]
        probe2: String,
        /// 断线恢复凭证（第 37-2 项；客户端说了 can_resume 才有）
        #[serde(default)]
        resume: String,
    },
    /// 恢复成功（第 37-2 项）：接回原来的身份，之后照常收发
    Resumed {
        user_id: u32,
    },
    Error {
        message: String,
    },
    State {
        channels: Vec<ChannelInfo>,
        users: Vec<UserInfo>,
    },
    System {
        text: String,
    },
    Pong,
    /// 和 peer 同时开始打洞（第 62-2 项）：服务器把这条同一时刻发给双方，收到后过 in_ms 毫秒、减去自己到服务器的单程延迟再开打，
    /// 两边差不多同一刻开始（严格型 NAT、按顺序分配端口的 NAT 吃这个时机）。旧客户端不认识这条，照旧收到名单就打
    PunchAt {
        peer: u32,
        in_ms: u32,
    },
    /// 同频道、开启了 P2P 的成员
    Peers {
        peers: Vec<PeerInfo>,
    },
    PeerKey {
        from: u32,
        ct: String,
        sig: String,
    },
    /// 端到端身份目录：登录后收到全部在线成员的
    Directory {
        entries: Vec<E2eEntry>,
    },
    /// 有人新公布了身份
    E2eJoin {
        entry: E2eEntry,
    },
    E2eBlob {
        from: u32,
        blob: String,
        /// 服务器收到的时间（Unix 毫秒）
        ts: u64,
    },
    /// 发给共享者：现在谁在看你的屏幕；low = 其中要低画质层的人
    Watchers {
        ids: Vec<u32>,
        #[serde(default)]
        low: Vec<u32>,
        /// 其中选了 P2P 直连的人
        #[serde(default)]
        direct: Vec<u32>,
        /// 其中选了走服务器的人
        #[serde(default)]
        server: Vec<u32>,
        /// 其中会自己画鼠标的人
        #[serde(default)]
        cursor: Vec<u32>,
    },
    /// 发给共享者：某观看者请求关键帧
    KeyframeRequest {
        by: u32,
    },
    /// 发给共享者：观看者的长期参考帧确认 / 恢复请求（见 ClientMsg::VideoLtr）
    VideoLtr {
        by: u32,
        #[serde(default)]
        ack: Option<u32>,
        #[serde(default)]
        recover: bool,
    },
    /// 发给共享者：某观看者收画面的情况（见 ClientMsg::VideoReport）
    VideoReport {
        by: u32,
        loss: u16,
        #[serde(default)]
        rtt: u32,
    },
    /// 本频道当前的成员中继路线（有变化才发）
    Relays {
        routes: Vec<RelayRoute>,
    },
    /// 0.6 语音优先：这些人在你说话时没听到你（直连 / 中转那条路实际不通），先把给他们的语音改经服务器发（有变化才发）
    VoiceFallback {
        ids: Vec<u32>,
    },
    /// 服务器马上要停止（重启 / 升级）：客户端显示原因，断开后立刻重连（旧客户端不认识，忽略）
    Restarting {
        #[serde(default)]
        text: String,
    },
}

/// 端到端身份公告（字段均为 base64）。服务器不验证，由各客户端验签。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct E2eEntry {
    pub id: u32,
    pub name: String,
    /// ML-DSA-65 身份公钥
    pub idk: String,
    /// 本次会话的 ML-KEM-768 公钥
    pub ek: String,
    /// Sign(idk, 昵称 | ek)
    pub sig: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PeerInfo {
    pub id: u32,
    /// 候选地址：第一个是服务器观察到的公网地址，其余为对方自报的本地地址
    pub addrs: Vec<String>,
    /// 对方自测的 NAT 类型
    #[serde(default)]
    pub nat: Option<NatInfo>,
}

/// NAT 类型（客户端向服务器的两个端口各问一次自己的公网地址，比较得出）
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct NatInfo {
    /// 见 NAT_*
    pub kind: u8,
    /// 最近一次分配的公网端口（对称型 NAT 端口预测用）
    #[serde(default)]
    pub port: u16,
    /// 对称型 NAT 每换一个目标端口加多少（可预测时）
    #[serde(default)]
    pub step: i16,
}

/// 没有 NAT（公网地址就在本机上）
pub const NAT_OPEN: u8 = 1;
/// 去哪都用同一个映射：好打洞
pub const NAT_CONE: u8 = 2;
/// 对称型，端口按固定步长分配：可以预测
pub const NAT_SYM_PREDICTABLE: u8 = 3;
/// 对称型，端口随机：基本打不通
pub const NAT_SYM_RANDOM: u8 = 4;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ChannelInfo {
    pub id: u32,
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct UserInfo {
    pub id: u32,
    pub name: String,
    pub channel_id: Option<u32>,
    pub muted: bool,
    pub deafened: bool,
    /// 正在屏幕共享时的描述（例如 "1080p60"）
    #[serde(default)]
    pub sharing: Option<String>,
    /// 能解码的视频格式；旧版客户端为空（视为只支持 h264）
    #[serde(default)]
    pub decoders: Vec<String>,
    /// 开启了 P2P 直连（显示打洞状态用）
    #[serde(default)]
    pub p2p: bool,
    /// 语音走 UDP（false = UDP 不通，走 TCP 隧道，无法直连）
    #[serde(default)]
    pub udp: bool,
    /// 正在放伴奏的程序名
    #[serde(default)]
    pub music: Option<String>,
    /// 屏幕共享带声音
    #[serde(default)]
    pub share_audio: bool,
    /// 屏幕共享有低画质层可选
    #[serde(default)]
    pub simulcast: bool,
    /// 屏幕共享支持丢包补发和冗余校验包
    #[serde(default)]
    pub rtx: bool,
    /// 屏幕共享只走 P2P（画面不经服务器）
    #[serde(default)]
    pub share_p2p_only: bool,
}

/// 断线恢复凭证（第 37-2 项）
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ResumeToken {
    pub id: u32,
    pub token: String,
}

/// 恢复失败时服务器回的错误（客户端看到就改走普通重连）
pub const RESUME_FAILED: &str = "resume_failed";

/// 服务器功能：私聊发图（FileTarget）
pub const FEATURE_DM_FILE: &str = "dm_file";
/// 服务器功能：PONG 里附上客户端公网地址
pub const FEATURE_PONG_ADDR: &str = "pong_addr";
/// 服务器功能：转发视频补发请求（udp::NACK）、缓存最近的画面分片直接补发、转发冗余校验包
pub const FEATURE_VIDEO_RTX: &str = "video_rtx";
/// 服务器功能：共享声音的直连名单单独报（DirectPeers.share_audio），不跟画面的直连名单走（第 54 项）
pub const FEATURE_SHARE_AUDIO_ROUTE: &str = "share_audio_route";
/// 服务器收打洞结果（PunchResult）、统一通知同时开打（PunchAt）（第 62 项）
pub const FEATURE_PUNCH: &str = "punch";

pub fn encode_line<T: Serialize>(msg: &T) -> String {
    let mut s = serde_json::to_string(msg).expect("serialize");
    s.push('\n');
    s
}

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_mismatch_explained() {
        // 旧客户端连新服务器：服务器的话里说「你的 VOC 太旧」
        assert!(version_mismatch_text(PROTOCOL_VERSION, 6).contains("太旧"));
        assert!(version_mismatch_text(6, PROTOCOL_VERSION).contains("voc upgrade"));
        // 新客户端连旧服务器：旧服务器说「请更新客户端」，客户端改成「服务器太旧」
        let old = format!("协议版本不匹配（服务器 6，客户端 {PROTOCOL_VERSION}），请更新客户端");
        assert!(explain_login_error(&old).contains("voc upgrade"), "{}", explain_login_error(&old));
        // 其他错误原样
        assert_eq!(explain_login_error("该昵称已被使用"), "该昵称已被使用");
    }

    #[test]
    fn json_roundtrip() {
        let m = ClientMsg::Hello {
            can_resume: false,
            resume: None,
            name: "小明".into(),
            version: PROTOCOL_VERSION,
        };
        let line = encode_line(&m);
        let back: ClientMsg = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(m, back);
    }
}
