//! 语音包内容（加密前的明文；外层封装见 [`crate::crypto::DatagramSealer`]）。语音帧是 Opus 数据。
//! 同样的内容在 UDP 不可用时放进 TCP 帧（kind = `crypto::KIND_VOICE`）传输。
//!
//! - PING（客户端→服务器）/ PONG（服务器→客户端）：`ts_ms u64`
//! - VOICE 客户端→服务器：`seq u16 | 语音帧`
//! - VOICE 服务器→客户端：`from_id u32 | seq u16 | 语音帧`
//! - VIDEO 客户端→服务器：`frame u32 | idx u16 | count u16 | flags u8 | 分片数据`（一帧加密后的视频切成多片）
//! - VIDEO 服务器→客户端（及 P2P 直连）：`from_id u32 |` 同上

pub const PING: u8 = 1;
pub const PONG: u8 = 2;
pub const VOICE: u8 = 3;
pub const VIDEO: u8 = 4;

/// 视频分片数据最大长度（加上各层包头后单个 UDP 包 < 1200 字节，避免 IP 分片）
pub const VIDEO_FRAG: usize = 1080;
/// 视频分片 flags：bit0 = 关键帧，bit1-2 = 编码格式（0 H.264，1 HEVC，2 AV1）
pub const VIDEO_KEY: u8 = 1;
pub const CODEC_H264: u8 = 0;
pub const CODEC_HEVC: u8 = 1;
pub const CODEC_AV1: u8 = 2;

pub fn video_flags(key: bool, codec: u8) -> u8 {
    (key as u8) | ((codec & 3) << 1)
}

pub fn flags_codec(flags: u8) -> u8 {
    (flags >> 1) & 3
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFrag {
    pub frame: u32,
    pub idx: u16,
    pub count: u16,
    pub flags: u8,
}

const VIDEO_HDR: usize = 9;

/// 把一帧（已加密）切成若干分片，每片 = 分片头 | 数据
pub fn video_fragments(frame: u32, flags: u8, data: &[u8]) -> Vec<Vec<u8>> {
    let chunks: Vec<&[u8]> = if data.is_empty() { vec![&[][..]] } else { data.chunks(VIDEO_FRAG).collect() };
    let count = chunks.len() as u16;
    chunks
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut v = Vec::with_capacity(VIDEO_HDR + c.len());
            v.extend_from_slice(&frame.to_le_bytes());
            v.extend_from_slice(&(i as u16).to_le_bytes());
            v.extend_from_slice(&count.to_le_bytes());
            v.push(flags);
            v.extend_from_slice(c);
            v
        })
        .collect()
}

pub fn read_video_frag(b: &[u8]) -> Option<(VideoFrag, &[u8])> {
    if b.len() < VIDEO_HDR {
        return None;
    }
    let f = VideoFrag {
        frame: u32::from_le_bytes(b[..4].try_into().ok()?),
        idx: u16::from_le_bytes([b[4], b[5]]),
        count: u16::from_le_bytes([b[6], b[7]]),
        flags: b[8],
    };
    (f.count > 0 && f.idx < f.count).then_some((f, &b[VIDEO_HDR..]))
}

/// 服务器→客户端的视频包：from | 分片
pub fn server_video(from: u32, frag: &[u8]) -> Vec<u8> {
    let mut v = from.to_le_bytes().to_vec();
    v.extend_from_slice(frag);
    v
}

pub fn read_server_video(b: &[u8]) -> Option<(u32, VideoFrag, &[u8])> {
    let from = u32::from_le_bytes(b.get(..4)?.try_into().ok()?);
    let (f, d) = read_video_frag(&b[4..])?;
    Some((from, f, d))
}

pub const MAX_PACKET: usize = 1400;

pub fn client_voice(seq: u16, frame: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(2 + frame.len());
    v.extend_from_slice(&seq.to_le_bytes());
    v.extend_from_slice(frame);
    v
}

pub fn read_client_voice(b: &[u8]) -> Option<(u16, &[u8])> {
    (b.len() >= 2).then(|| (u16::from_le_bytes([b[0], b[1]]), &b[2..]))
}

pub fn server_voice(from: u32, seq: u16, frame: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(6 + frame.len());
    v.extend_from_slice(&from.to_le_bytes());
    v.extend_from_slice(&seq.to_le_bytes());
    v.extend_from_slice(frame);
    v
}

pub fn read_server_voice(b: &[u8]) -> Option<(u32, u16, &[u8])> {
    if b.len() < 6 {
        return None;
    }
    Some((u32::from_le_bytes(b[..4].try_into().ok()?), u16::from_le_bytes([b[4], b[5]]), &b[6..]))
}

/// 序号比较（考虑回绕）：a 是否比 b 新
pub fn seq_newer(a: u16, b: u16) -> bool {
    a != b && a.wrapping_sub(b) < 0x8000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_payloads() {
        let c = client_voice(65535, b"abc");
        assert_eq!(read_client_voice(&c), Some((65535, &b"abc"[..])));
        let s = server_voice(9, 3, b"xy");
        assert_eq!(read_server_voice(&s), Some((9, 3, &b"xy"[..])));
    }

    #[test]
    fn video_fragmenting() {
        let data: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let frags = video_fragments(77, VIDEO_KEY, &data);
        assert_eq!(frags.len(), 5000usize.div_ceil(VIDEO_FRAG));
        let mut back = Vec::new();
        for (i, f) in frags.iter().enumerate() {
            let s = server_video(9, f);
            let (from, h, d) = read_server_video(&s).unwrap();
            assert_eq!((from, h.frame, h.idx as usize, h.count as usize, h.flags), (9, 77, i, frags.len(), VIDEO_KEY));
            back.extend_from_slice(d);
        }
        assert_eq!(back, data);
        assert!(read_video_frag(&[0; 4]).is_none());
    }

    #[test]
    fn seq_wrap() {
        assert!(seq_newer(1, 65535));
        assert!(!seq_newer(65535, 1));
        assert!(seq_newer(10, 9));
    }
}
