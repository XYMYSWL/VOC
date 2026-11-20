//! VOC 共享协议。传输层（握手、加密、抗 DPI）见 [`crypto`]，授权码见 [`auth`]。
//!
//! - 控制通道：TCP，加密帧内是 JSON（`ClientMsg` / `ServerMsg`）。
//! - 语音通道：UDP（与 TCP 同端口号），加密包内容见 [`udp`]；UDP 不通时自动改走 TCP。
//! - 音频：16 kHz 单声道，20 ms 一帧（320 采样），Opus 编码（客户端负责编解码，服务器只转发）。
//! - P2P：同频道成员可以 UDP 打洞直连语音，见 [`p2p`]。
//! - 端到端加密：语音和聊天服务器都看不到，见 [`e2e`]。

use serde::{Deserialize, Serialize};

pub mod auth;
pub mod crypto;
pub mod e2e;
pub mod p2p;
pub mod udp;

pub const PROTOCOL_VERSION: u32 = 6;
pub const DEFAULT_PORT: u16 = 7878;
pub const SAMPLE_RATE: u32 = 16_000;
/// 20 ms @ 16 kHz
pub const FRAME_SAMPLES: usize = 320;
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
    /// 开启 P2P：本机候选地址（局域网 / IPv6）
    P2pEnable {
        local_addrs: Vec<String>,
    },
    P2pDisable,
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
    },
    /// 开始屏幕共享（desc 例如 "1080p60"，显示给别人看）
    ShareStart {
        desc: String,
    },
    ShareStop,
    /// 开始 / 停止观看某人的屏幕
    Watch {
        target: u32,
        on: bool,
    },
    /// 观看者丢包后请求共享者发关键帧
    KeyframeRequest {
        target: u32,
    },
    /// 本机能解码的视频格式（"av1" / "hevc" / "h264"），共享者据此选择编码格式
    Caps {
        decoders: Vec<String>,
    },
    /// 每 2 秒上报一次链路质量，服务器据此安排成员互相中转语音（成员中继）。
    /// server_rtt：到服务器的往返延迟；peers：已打通且稳定的直连；relay_slots：还能帮别人中转几路语音（0 = 不当中继）
    LinkStats {
        server_rtt: u32,
        peers: Vec<PeerStat>,
        relay_slots: u16,
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
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome {
        user_id: u32,
        udp_port: u16,
        server_name: String,
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
    /// 发给共享者：现在谁在看你的屏幕
    Watchers {
        ids: Vec<u32>,
    },
    /// 发给共享者：某观看者请求关键帧
    KeyframeRequest {
        by: u32,
    },
    /// 本频道当前的成员中继路线（有变化才发）
    Relays {
        routes: Vec<RelayRoute>,
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
}

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
}

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
    fn json_roundtrip() {
        let m = ClientMsg::Hello {
            name: "小明".into(),
            version: PROTOCOL_VERSION,
        };
        let line = encode_line(&m);
        let back: ClientMsg = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(m, back);
    }
}
