//! 语音包内容（加密前的明文；外层封装见 [`crate::crypto::DatagramSealer`]）。语音帧是 Opus 数据。
//! 同样的内容在 UDP 不可用时放进 TCP 帧（kind = `crypto::KIND_VOICE`）传输。
//!
//! - PING（客户端→服务器）/ PONG（服务器→客户端）：`ts_ms u64`；PING 末尾多一个字节 1 时，
//!   新版服务器在 PONG 里接着附上它看到的客户端公网地址：`ts_ms u64 | 1 | ip(4 或 16 字节) | port u16`
//! - VOICE 客户端→服务器：`seq u16 | 语音帧`
//! - VOICE 服务器→客户端：`from_id u32 | seq u16 | 语音帧`
//! - VIDEO 客户端→服务器：`frame u32 | idx u16 | count u16 | flags u8 | 分片数据`（一帧加密后的视频切成多片）
//! - VIDEO 服务器→客户端（及 P2P 直连）：`from_id u32 |` 同上
//!   分片号 idx ≥ count 的是冗余校验包：count ≤ idx < 2·count 是 0.5.0 的异或校验（[`fec_parities`]），
//!   idx ≥ 2·count 是 0.5.1 起的 Reed-Solomon 校验（[`rs_parities`]，0.5.0 的客户端和服务器会当坏包丢掉，不会误用）
//! - NACK（观看者→服务器 / P2P 直连给共享者）：`target u32 | layer u8 | frame u32 | n u16 | idx u16 × n`，
//!   请求补发某一帧缺的分片；idx = [`NACK_WHOLE`] 表示整帧都没收到。
//!   服务器→共享者时开头的 u32 换成请求者编号

pub const PING: u8 = 1;
pub const PONG: u8 = 2;
pub const VOICE: u8 = 3;
pub const VIDEO: u8 = 4;
/// 伴奏（某个程序的声音，发给整个频道）。格式同 VOICE
pub const MUSIC: u8 = 5;
/// 屏幕共享的声音（发给观看者）。格式同 VOICE
pub const SHARE_AUDIO: u8 = 6;
/// 视频补发请求
pub const NACK: u8 = 7;
/// 屏幕共享的鼠标位置和形状（发给观看者）。格式同 VOICE，内容见 [`cursor_payload`]
pub const CURSOR: u8 = 8;

/// 鼠标：x、y 是在共享画面里的位置（0–65535 对应左到右 / 上到下），shape 见 CURSOR_*，visible = 显示着
pub fn cursor_payload(x: u16, y: u16, shape: u8, visible: bool) -> [u8; 6] {
    let mut v = [0u8; 6];
    v[..2].copy_from_slice(&x.to_le_bytes());
    v[2..4].copy_from_slice(&y.to_le_bytes());
    v[4] = shape;
    v[5] = visible as u8;
    v
}

pub fn read_cursor(b: &[u8]) -> Option<(u16, u16, u8, bool)> {
    (b.len() >= 6).then(|| (u16::from_le_bytes([b[0], b[1]]), u16::from_le_bytes([b[2], b[3]]), b[4], b[5] != 0))
}

/// 带冗余的语音（第 34-3 项）：发出方向丢包高时，把前一两帧的 Opus 包顺带塞进这一包，接收端丢了前一包也能原样还原。
/// 格式（在端到端加密里面）：0xFF | 份数 k | k 个长度（u16，最旧的在前）| k 个旧包 | 这一包。
/// 单声道 Opus 包的第一个字节不会是 0xFF（0xFF 表示立体声），不会和普通语音包混淆
pub const VOICE_RED_MARK: u8 = 0xFF;

pub fn voice_red(primary: &[u8], older: &[&[u8]]) -> Vec<u8> {
    let mut v = Vec::with_capacity(2 + older.len() * 2 + older.iter().map(|o| o.len()).sum::<usize>() + primary.len());
    v.push(VOICE_RED_MARK);
    v.push(older.len() as u8);
    for o in older {
        v.extend_from_slice(&(o.len() as u16).to_le_bytes());
    }
    for o in older {
        v.extend_from_slice(o);
    }
    v.extend_from_slice(primary);
    v
}

/// 拆开带冗余的语音：(旧包，最旧的在前；这一包)。不是这种格式就返回 None（普通语音包）
pub fn read_voice_red(d: &[u8]) -> Option<(Vec<&[u8]>, &[u8])> {
    if d.first() != Some(&VOICE_RED_MARK) {
        return None;
    }
    let k = *d.get(1)? as usize;
    let mut pos = 2 + k * 2;
    if k == 0 || k > 4 || d.len() < pos {
        return None;
    }
    let mut older = Vec::with_capacity(k);
    for i in 0..k {
        let len = u16::from_le_bytes([d[2 + i * 2], d[3 + i * 2]]) as usize;
        older.push(d.get(pos..pos + len)?);
        pos += len;
    }
    let primary = d.get(pos..).filter(|p| !p.is_empty())?;
    Some((older, primary))
}

/// 共享者每秒附在鼠标包后面的状态（第 53-1 / 36-6 项）。旧客户端只读前 6 字节，多出来的忽略
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShareInfo {
    /// 共享者选的帧率
    pub target_fps: u8,
    /// 系统实际给了多少帧（屏幕 / 游戏本身的帧率）
    pub source_fps: u8,
    /// 采集到的帧率
    pub capture_fps: u8,
    /// 编码发出的帧率
    pub sent_fps: u8,
    /// 每秒因为画面没变没发的帧
    pub static_skipped: u8,
    /// 每秒被码率控制跳过的帧
    pub rate_skipped: u8,
    /// 平均每帧编码耗时（ms）
    pub encode_ms: u8,
    /// 共享者判断上行拥塞（在降码率）
    pub congested: bool,
    /// 实际发送码率（kbps）
    pub kbps: u16,
    /// 码率占设定值的百分比（拥塞时自动降低）
    pub rate_pct: u8,
    /// 共享声音：最近一秒录音「数据不连续」（录音线程没跟上）的次数
    pub audio_glitches: u8,
    /// 共享的是窗口时：0 正常，1 被最小化，2 已关闭（第 60-1 项；旧版本没有这个字节，读出来是 0）
    pub window_state: u8,
}

const SHARE_INFO_V1: u8 = 1;
const SHARE_INFO_LEN: usize = 13;

/// 鼠标包 + 共享状态
pub fn cursor_with_info(x: u16, y: u16, shape: u8, visible: bool, i: &ShareInfo) -> Vec<u8> {
    let mut v = cursor_payload(x, y, shape, visible).to_vec();
    v.push(SHARE_INFO_V1);
    v.extend_from_slice(&[i.target_fps, i.source_fps, i.capture_fps, i.sent_fps, i.static_skipped, i.rate_skipped, i.encode_ms, i.congested as u8]);
    v.extend_from_slice(&i.kbps.to_le_bytes());
    v.push(i.rate_pct);
    v.push(i.audio_glitches);
    v.push(i.window_state);
    v
}

pub fn read_share_info(b: &[u8]) -> Option<ShareInfo> {
    let t = b.get(6..)?;
    if t.len() < SHARE_INFO_LEN || t[0] != SHARE_INFO_V1 {
        return None;
    }
    Some(ShareInfo {
        target_fps: t[1],
        source_fps: t[2],
        capture_fps: t[3],
        sent_fps: t[4],
        static_skipped: t[5],
        rate_skipped: t[6],
        encode_ms: t[7],
        congested: t[8] != 0,
        kbps: u16::from_le_bytes([t[9], t[10]]),
        rate_pct: t[11],
        audio_glitches: t[12],
        window_state: t.get(13).copied().unwrap_or(0),
    })
}

pub const CURSOR_ARROW: u8 = 0;
pub const CURSOR_IBEAM: u8 = 1;
pub const CURSOR_HAND: u8 = 2;
pub const CURSOR_CROSS: u8 = 3;
pub const CURSOR_WAIT: u8 = 4;
pub const CURSOR_SIZE_WE: u8 = 5;
pub const CURSOR_SIZE_NS: u8 = 6;
pub const CURSOR_SIZE_ALL: u8 = 7;
/// NACK 里表示「整帧都没收到」
pub const NACK_WHOLE: u16 = 0xFFFF;
/// 一个 NACK 最多请求多少个分片
pub const NACK_MAX: usize = 256;

/// 视频分片数据最大长度（加上各层包头后单个 UDP 包 < 1200 字节，避免 IP 分片）
pub const VIDEO_FRAG: usize = 1080;
/// 一帧最多多少个数据分片（约 8.8 MB；25 Mbps 的关键帧也远小于这个）。超过的帧发送端不发、接收端不收
pub const MAX_FRAGS: u16 = 8192;
/// 视频分片 flags：bit0 = 关键帧，bit1-2 = 编码格式（0 H.264，1 HEVC，2 AV1），bit3 = 低画质层（360P）
pub const VIDEO_KEY: u8 = 1;
pub const VIDEO_LOW: u8 = 8;
/// 这一帧被编码器存成长期参考帧（观看者解码成功后回确认）
pub const VIDEO_LTR: u8 = 16;
/// 恢复帧：只参考所有观看者都确认过的长期参考帧，丢过帧的观看者从这里接着解，不用等关键帧
pub const VIDEO_RECOVER: u8 = 32;
/// 跨帧交错（第 34-5 项）：这帧和相邻一帧的分片是交替发的，接收端等这一对都到齐（或超时）再要补发。旧客户端不看这一位
pub const VIDEO_INTERLEAVED: u8 = 64;
/// 帧内容前面带 4 字节截屏时间（共享者的毫秒计时，u32 小端，第 56-c 项）。只在所有观看者都认识时才带（旧版本会把它当成画面数据）
pub const VIDEO_TS: u8 = 128;
/// 分片包头里 flags 的位置（frame u32 | idx u16 | count u16 | flags u8）
pub const VIDEO_FLAGS_AT: usize = 8;
pub const CODEC_H264: u8 = 0;
pub const CODEC_HEVC: u8 = 1;
pub const CODEC_AV1: u8 = 2;

/// 客户端发的 PING：时间戳 + 「请附上我的公网地址」标记（旧服务器原样回显，不受影响）
pub fn ping_payload(ts_ms: u64) -> [u8; 9] {
    let mut p = [1u8; 9];
    p[..8].copy_from_slice(&ts_ms.to_le_bytes());
    p
}

/// 过滤行为测试（第 39-2 项）：发给探测端口，服务器从「另一个 IP」的探测点回。收得到 = 我的 NAT 对没联系过的地址也放行
pub fn filter_test_payload(ts_ms: u64) -> [u8; 9] {
    let mut p = ping_payload(ts_ms);
    p[8] = FILTER_TEST;
    p
}
pub const FILTER_TEST: u8 = 2;

/// 探测线路能走多大的包（第 37-4 项）：时间戳 | 3 | 补零到 len 字节。服务器 / 队友原样回显
pub const MTU_PROBE: u8 = 3;
pub fn mtu_probe_payload(ts_ms: u64, len: usize) -> Vec<u8> {
    let mut v = ts_ms.to_le_bytes().to_vec();
    v.push(MTU_PROBE);
    v.resize(len.max(9), 0);
    v
}
pub fn is_mtu_probe(d: &[u8]) -> bool {
    d.len() > 64 && d[8] == MTU_PROBE
}

/// 服务器回的 PONG：客户端要了就附上观察到的地址，否则原样回显
pub fn pong_payload(ping: &[u8], observed: std::net::SocketAddr) -> Vec<u8> {
    if ping.len() != 9 || ping[8] != 1 {
        return ping.to_vec();
    }
    let mut v = ping.to_vec();
    match observed.ip().to_canonical() {
        std::net::IpAddr::V4(ip) => v.extend_from_slice(&ip.octets()),
        std::net::IpAddr::V6(ip) => v.extend_from_slice(&ip.octets()),
    }
    v.extend_from_slice(&observed.port().to_le_bytes());
    v
}

/// 解析 PONG：(时间戳, 服务器看到的我的公网地址)
pub fn read_pong(data: &[u8]) -> Option<(u64, Option<std::net::SocketAddr>)> {
    let ts = u64::from_le_bytes(data.get(..8)?.try_into().ok()?);
    let addr = match data.len() {
        15 => {
            let ip: [u8; 4] = data[9..13].try_into().ok()?;
            Some(std::net::SocketAddr::from((ip, u16::from_le_bytes([data[13], data[14]]))))
        }
        27 => {
            let ip: [u8; 16] = data[9..25].try_into().ok()?;
            Some(std::net::SocketAddr::from((ip, u16::from_le_bytes([data[25], data[26]]))))
        }
        _ => None,
    };
    Some((ts, addr))
}

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
    let chunks = frame_chunks(data);
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
    // idx ≥ count 是冗余校验包：异或校验最多 count 个，Reed-Solomon 校验在 2·count 之后、最多 4·count 个（0.6 起；以前 2·count）。
    // 分片数和每片长度都有上限（坏包 / 恶意包不能让接收端按很大的数分配内存）
    let ok = f.count > 0
        && f.count <= MAX_FRAGS
        && (f.idx as u32) < f.count as u32 * 6
        && b.len() - VIDEO_HDR <= VIDEO_FRAG + RS_HDR;
    ok.then_some((f, &b[VIDEO_HDR..]))
}

impl VideoFrag {
    /// 冗余校验包（不是数据分片）
    pub fn is_parity(&self) -> bool {
        self.idx >= self.count
    }

    /// Reed-Solomon 校验包（0.5.1 起）
    pub fn is_rs_parity(&self) -> bool {
        self.idx as u32 >= self.count as u32 * 2
    }
}

/// Reed-Solomon：每组最多多少个数据分片（GF(2^8) 一组数据 + 校验最多 255 片）
pub const RS_GROUP: usize = 64;
/// 校验片数最多是数据片数的多少倍（一般的组；0.6 起 2.0，以前 1.5）
pub const RS_MAX_RATIO: f32 = 2.0;
/// 片数很少的组（小帧，码率低时很常见）同样丢包率下整组救不回的概率高得多（丢 50%、4 片一组、200% 冗余仍有约 7% 救不回），
/// 丢包高时按片数加倍冗余，上限更高（复查 34-1）
pub fn rs_small_group_ratio(k: usize, ratio: f32) -> f32 {
    if ratio < 0.9 {
        return ratio.min(RS_MAX_RATIO);
    }
    match k {
        0..=4 => (ratio * 2.0).min(4.0),
        5..=8 => (ratio * 1.5).min(3.0),
        _ => ratio.min(RS_MAX_RATIO),
    }
}
const RS_HDR: usize = 11;

/// 一帧的数据分片怎么分组：[(第一片, 片数)]，尽量平均
pub fn rs_groups(count: usize) -> Vec<(usize, usize)> {
    if count == 0 {
        return Vec::new();
    }
    let g = count.div_ceil(RS_GROUP);
    let (base, extra) = (count / g, count % g);
    let mut first = 0;
    (0..g)
        .map(|i| {
            let k = base + usize::from(i < extra);
            let r = (first, k);
            first += k;
            r
        })
        .collect()
}

/// 一组 k 片数据配多少片校验
pub fn rs_parity_count(k: usize, ratio: f32) -> usize {
    if ratio <= 0.0 || k == 0 {
        return 0;
    }
    ((k as f32 * rs_small_group_ratio(k, ratio) - 1e-3).ceil() as usize).clamp(1, (255 - k).min(4 * k))
}

fn rs_shard(c: &[u8]) -> Vec<u8> {
    let mut v = c.to_vec();
    v.resize(VIDEO_FRAG, 0);
    v
}

/// Reed-Solomon 校验包：每组 k 片数据配 m 片校验（m = k × ratio），一组里收到任意 k 片就能还原整组。
/// 校验包分片号从 2·count 起，数据 = `版本 1 u8 | 组的第一片 u16 | 组片数 k u16 | 第几片校验 j u8 | 校验片数 m u8 | 整帧长度 u32 | 校验数据（VIDEO_FRAG 字节）`
pub fn rs_parities(frame: u32, flags: u8, data: &[u8], ratio: f32) -> Vec<Vec<u8>> {
    use reed_solomon_erasure::galois_8::ReedSolomon;
    let chunks = frame_chunks(data);
    let count = chunks.len();
    let mut out = Vec::new();
    let mut idx = count * 2;
    for (first, k) in rs_groups(count) {
        let m = rs_parity_count(k, ratio);
        if m == 0 {
            continue;
        }
        let Ok(rs) = ReedSolomon::new(k, m) else { continue };
        let mut shards: Vec<Vec<u8>> = chunks[first..first + k].iter().map(|c| rs_shard(c)).collect();
        shards.extend((0..m).map(|_| vec![0u8; VIDEO_FRAG]));
        if rs.encode(&mut shards).is_err() {
            continue;
        }
        for (j, sh) in shards[k..].iter().enumerate() {
            if idx > u16::MAX as usize {
                return out;
            }
            let mut v = Vec::with_capacity(VIDEO_HDR + RS_HDR + VIDEO_FRAG);
            v.extend_from_slice(&frame.to_le_bytes());
            v.extend_from_slice(&(idx as u16).to_le_bytes());
            v.extend_from_slice(&(count as u16).to_le_bytes());
            v.push(flags);
            v.push(1);
            v.extend_from_slice(&(first as u16).to_le_bytes());
            v.extend_from_slice(&(k as u16).to_le_bytes());
            v.push(j as u8);
            v.push(m as u8);
            v.extend_from_slice(&(data.len() as u32).to_le_bytes());
            v.extend_from_slice(sh);
            out.push(v);
            idx += 1;
        }
    }
    out
}

/// 解析出的 Reed-Solomon 校验包
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RsParity {
    pub first: usize,
    pub k: usize,
    pub j: usize,
    pub m: usize,
    pub total_len: usize,
}

pub fn read_rs_parity<'a>(f: &VideoFrag, d: &'a [u8]) -> Option<(RsParity, &'a [u8])> {
    if !f.is_rs_parity() || d.len() != RS_HDR + VIDEO_FRAG || d[0] != 1 {
        return None;
    }
    let p = RsParity {
        first: u16::from_le_bytes([d[1], d[2]]) as usize,
        k: u16::from_le_bytes([d[3], d[4]]) as usize,
        j: d[5] as usize,
        m: d[6] as usize,
        total_len: u32::from_le_bytes(d[7..11].try_into().ok()?) as usize,
    };
    let count = f.count as usize;
    let ok = p.k > 0 && p.m > 0 && p.j < p.m && p.k + p.m <= 255 && p.first + p.k <= count
        && p.total_len <= count * VIDEO_FRAG && p.total_len + VIDEO_FRAG > count * VIDEO_FRAG;
    ok.then_some((p, &d[RS_HDR..]))
}

/// 用一组里收到的数据片和校验片还原缺的数据片。data 长度 = k（按组内顺序），parity 长度 = m。
/// 收到的总片数 ≥ k 才能还原；返回整组 k 片数据（已按真实长度截断）
pub fn rs_recover(count: usize, p: &RsParity, data: &[Option<&[u8]>], parity: &[Option<&[u8]>]) -> Option<Vec<Vec<u8>>> {
    use reed_solomon_erasure::galois_8::ReedSolomon;
    if data.len() != p.k || parity.len() != p.m {
        return None;
    }
    let have = data.iter().chain(parity.iter()).filter(|x| x.is_some()).count();
    if have < p.k {
        return None;
    }
    let rs = ReedSolomon::new(p.k, p.m).ok()?;
    let mut shards: Vec<Option<Vec<u8>>> = data.iter().map(|d| d.map(rs_shard)).chain(parity.iter().map(|x| x.map(|x| x.to_vec()))).collect();
    rs.reconstruct_data(&mut shards).ok()?;
    let last_len = p.total_len - (count - 1) * VIDEO_FRAG;
    Some(
        shards
            .into_iter()
            .take(p.k)
            .enumerate()
            .map(|(i, sh)| {
                let mut v = sh.unwrap_or_default();
                v.truncate(if p.first + i == count - 1 { last_len } else { VIDEO_FRAG });
                v
            })
            .collect(),
    )
}

fn frame_chunks(data: &[u8]) -> Vec<&[u8]> {
    if data.is_empty() {
        vec![&[][..]]
    } else {
        data.chunks(VIDEO_FRAG).collect()
    }
}

/// 冗余校验包：每 k 个数据分片一组，每组一个异或校验包，组内丢一片就能直接算回来。
/// 校验包 idx = count + 组号，数据 = `k u8 | 各片长度的异或 u16 | 各片数据（补零到等长）的异或`
pub fn fec_parities(frame: u32, flags: u8, data: &[u8], k: usize) -> Vec<Vec<u8>> {
    let k = k.clamp(1, 255);
    let chunks = frame_chunks(data);
    let count = chunks.len() as u16;
    chunks
        .chunks(k)
        .enumerate()
        .map(|(g, group)| {
            let width = group.iter().map(|c| c.len()).max().unwrap_or(0);
            let mut x = vec![0u8; width];
            let mut len = 0u16;
            for c in group {
                len ^= c.len() as u16;
                for (a, b) in x.iter_mut().zip(c.iter()) {
                    *a ^= b;
                }
            }
            let mut v = Vec::with_capacity(VIDEO_HDR + 3 + width);
            v.extend_from_slice(&frame.to_le_bytes());
            v.extend_from_slice(&(count + g as u16).to_le_bytes());
            v.extend_from_slice(&count.to_le_bytes());
            v.push(flags);
            v.push(k as u8);
            v.extend_from_slice(&len.to_le_bytes());
            v.extend_from_slice(&x);
            v
        })
        .collect()
}

/// 校验包覆盖的数据分片范围：(k, 第一片, 片数)
pub fn fec_group(f: &VideoFrag, parity: &[u8]) -> Option<(usize, usize, usize)> {
    let k = *parity.first()? as usize;
    if !f.is_parity() || k == 0 {
        return None;
    }
    let g = (f.idx - f.count) as usize;
    let first = g * k;
    let count = f.count as usize;
    (first < count).then(|| (k, first, k.min(count - first)))
}

/// 用校验包和同组其余分片算回丢的那一片
pub fn fec_recover(parity: &[u8], others: &[&[u8]]) -> Option<Vec<u8>> {
    if parity.len() < 3 {
        return None;
    }
    let mut len = u16::from_le_bytes([parity[1], parity[2]]);
    let mut x = parity[3..].to_vec();
    for o in others {
        len ^= o.len() as u16;
        for (a, b) in x.iter_mut().zip(o.iter()) {
            *a ^= b;
        }
    }
    let len = len as usize;
    (len <= x.len()).then(|| {
        x.truncate(len);
        x
    })
}

/// NACK 的画质层字节里这一位 = 缺片用「起始编号 + 位图」表示（0.6，大帧缺很多片时用）
const NACK_BITMAP: u8 = 0x80;

/// 补发请求：target（或服务器转给共享者时的请求者）| 画质层 | 帧号 | 缺的分片。
/// 缺的片不多：逐个列出（最多 NACK_MAX 个）；很多（大帧）：`起始编号 u16 | 位图字节数 u16 | 位图`，
/// 只补缺的那些，不用整帧重发（0.5.x 超过 256 片就整帧重发，第 42-4 项）
pub fn nack_payload(target: u32, layer: u8, frame: u32, idxs: &[u16]) -> Vec<u8> {
    let mut v = Vec::with_capacity(11 + idxs.len().min(NACK_MAX) * 2);
    v.extend_from_slice(&target.to_le_bytes());
    let list = idxs.len() <= NACK_MAX || idxs.contains(&NACK_WHOLE);
    v.push(if list { layer } else { layer | NACK_BITMAP });
    v.extend_from_slice(&frame.to_le_bytes());
    if list {
        let idxs = &idxs[..idxs.len().min(NACK_MAX)];
        v.extend_from_slice(&(idxs.len() as u16).to_le_bytes());
        for i in idxs {
            v.extend_from_slice(&i.to_le_bytes());
        }
    } else {
        let start = *idxs.iter().min().unwrap();
        let end = *idxs.iter().max().unwrap();
        let nbytes = ((end - start) as usize / 8 + 1).min(MAX_FRAGS as usize / 8);
        let mut bm = vec![0u8; nbytes];
        for i in idxs {
            let o = (i - start) as usize;
            if o / 8 < nbytes {
                bm[o / 8] |= 1 << (o % 8);
            }
        }
        v.extend_from_slice(&start.to_le_bytes());
        v.extend_from_slice(&(nbytes as u16).to_le_bytes());
        v.extend_from_slice(&bm);
    }
    v
}

pub fn read_nack(b: &[u8]) -> Option<(u32, u8, u32, Vec<u16>)> {
    if b.len() < 11 {
        return None;
    }
    let target = u32::from_le_bytes(b[..4].try_into().ok()?);
    let layer = b[4] & VIDEO_LOW;
    let frame = u32::from_le_bytes(b[5..9].try_into().ok()?);
    if b[4] & NACK_BITMAP != 0 {
        if b.len() < 13 {
            return None;
        }
        let start = u16::from_le_bytes([b[9], b[10]]) as usize;
        let nbytes = u16::from_le_bytes([b[11], b[12]]) as usize;
        if nbytes > MAX_FRAGS as usize / 8 || b.len() < 13 + nbytes {
            return None;
        }
        let idxs = (0..nbytes * 8).filter(|o| b[13 + o / 8] & (1 << (o % 8)) != 0).map(|o| start + o).filter(|i| *i < MAX_FRAGS as usize).map(|i| i as u16).collect();
        return Some((target, layer, frame, idxs));
    }
    let n = u16::from_le_bytes([b[9], b[10]]) as usize;
    if n > NACK_MAX || b.len() < 11 + n * 2 {
        return None;
    }
    let idxs = (0..n).map(|i| u16::from_le_bytes([b[11 + i * 2], b[12 + i * 2]])).collect();
    Some((target, layer, frame, idxs))
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
    #[test]
    fn voice_red_roundtrip() {
        use super::*;
        let b = voice_red(b"main", &[b"a1", b"bb2"]);
        let (old, main) = read_voice_red(&b).unwrap();
        assert_eq!(old, vec![&b"a1"[..], &b"bb2"[..]]);
        assert_eq!(main, b"main");
        assert!(read_voice_red(&[0x48, 1, 2]).is_none(), "普通 Opus 包");
        assert!(read_voice_red(&[0xFF, 1, 9, 0, 1]).is_none(), "长度不对");
    }

    #[test]
    fn share_info_roundtrip() {
        use super::*;
        let i = ShareInfo { target_fps: 60, source_fps: 58, capture_fps: 57, sent_fps: 50, static_skipped: 3, rate_skipped: 4, encode_ms: 6, congested: true, kbps: 12_345, rate_pct: 80, audio_glitches: 2, window_state: 1 };
        let b = cursor_with_info(100, 200, CURSOR_HAND, true, &i);
        assert_eq!(read_cursor(&b), Some((100, 200, CURSOR_HAND, true)));
        assert_eq!(read_share_info(&b), Some(i));
        assert_eq!(read_share_info(&cursor_payload(1, 2, 0, false)), None);
    }

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

    /// 丢 50%、冗余 200%：小帧（片数少）也要大概率救回来——按二项分布算失败率
    #[test]
    fn small_groups_get_more_parity_under_heavy_loss() {
        // n 片里至少收到 k 片的概率（每片独立丢 50%）
        let ok_prob = |k: usize, m: usize| {
            let n = k + m;
            let mut c = 1.0f64; // C(n, i)
            let mut fail = 0.0;
            for i in 0..k {
                if i > 0 {
                    c = c * (n - i + 1) as f64 / i as f64;
                }
                fail += c;
            }
            1.0 - fail / 2f64.powi(n as i32)
        };
        for k in [1usize, 2, 4, 8, 16, 64] {
            let m = rs_parity_count(k, 2.0);
            assert!(m <= 4 * k);
            // 1 片的帧发 5 份：96.9%；2 片以上都在 98.9% 以上（0.5.1 的 120% 下 4 片一组只有约 75%）
            assert!(ok_prob(k, m) > 0.965, "k={k} m={m}: {}", ok_prob(k, m));
        }
        // 丢包不高时不加倍（省带宽）
        assert_eq!(rs_parity_count(4, 0.35), 2);
        // 编号范围装得下：一帧最多 4·count 个校验片
        let data = vec![7u8; VIDEO_FRAG * 3];
        let par = rs_parities(1, 0, &data, 2.0);
        assert_eq!(par.len(), 12);
        assert!(par.iter().all(|p| read_video_frag(p).is_some()));
    }

    #[test]
    fn nack_list_and_bitmap() {
        // 少：逐个列出
        let p = nack_payload(7, VIDEO_LOW, 99, &[1, 5, 9]);
        assert_eq!(read_nack(&p), Some((7, VIDEO_LOW, 99, vec![1, 5, 9])));
        // 整帧
        assert_eq!(read_nack(&nack_payload(7, 0, 1, &[NACK_WHOLE])).unwrap().3, vec![NACK_WHOLE]);
        // 多（大帧缺 600 片）：位图，一个都不少，包也不大
        let many: Vec<u16> = (100..1300).filter(|i| i % 2 == 0).collect();
        let p = nack_payload(7, 0, 5, &many);
        assert!(p.len() < 200, "bitmap is compact: {}", p.len());
        assert_eq!(read_nack(&p), Some((7, 0, 5, many)));
        // 坏包不认
        let mut bad = nack_payload(7, 0, 5, &(0..400).collect::<Vec<u16>>());
        bad.truncate(20);
        assert!(read_nack(&bad).is_none());
    }

    #[test]
    fn fragment_limits() {
        let mut f = video_fragments(1, 0, b"abc")[0].clone();
        assert!(read_video_frag(&f).is_some());
        // 分片数超过上限：不认
        f[6..8].copy_from_slice(&(MAX_FRAGS + 1).to_le_bytes());
        assert!(read_video_frag(&f).is_none());
        f[6..8].copy_from_slice(&MAX_FRAGS.to_le_bytes());
        assert!(read_video_frag(&f).is_some());
        // 一片的数据超长：不认
        let mut long = video_fragments(1, 0, b"abc")[0].clone();
        long.resize(9 + VIDEO_FRAG + RS_HDR + 1, 0);
        assert!(read_video_frag(&long).is_none());
    }

    #[test]
    fn fec_recovers_one_lost_fragment_per_group() {
        let data: Vec<u8> = (0..(VIDEO_FRAG * 7 + 123) as u32).map(|i| (i * 7) as u8).collect();
        let frags = video_fragments(5, VIDEO_KEY, &data);
        let par = fec_parities(5, VIDEO_KEY, &data, 3);
        assert_eq!((frags.len(), par.len()), (8, 3));
        let chunks: Vec<&[u8]> = frags.iter().map(|f| read_video_frag(f).unwrap().1).collect();
        for p in &par {
            let (f, d) = read_video_frag(p).unwrap();
            assert!(f.is_parity());
            let (_, first, n) = fec_group(&f, d).unwrap();
            // 组内每一片轮流当作丢了
            for lost in first..first + n {
                let others: Vec<&[u8]> = (first..first + n).filter(|i| *i != lost).map(|i| chunks[i]).collect();
                assert_eq!(fec_recover(d, &others).unwrap(), chunks[lost], "group from {first}, lost {lost}");
            }
        }
        // 超出范围的校验包号不认
        let mut bad = par[0].clone();
        bad[4..6].copy_from_slice(&48u16.to_le_bytes()); // count = 8：校验包号要 < 6·count
        assert!(read_video_frag(&bad).is_none());
    }

    #[test]
    fn rs_recovers_up_to_m_lost_per_group() {
        // 150 片：分成 3 组（50 片一组），每组配 50 × 0.6 = 30 片校验
        let data: Vec<u8> = (0..(VIDEO_FRAG * 149 + 333) as u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        let frags = video_fragments(9, VIDEO_KEY, &data);
        let count = frags.len();
        assert_eq!(count, 150);
        assert_eq!(rs_groups(count), vec![(0, 50), (50, 50), (100, 50)]);
        let par = rs_parities(9, VIDEO_KEY, &data, 0.6);
        assert_eq!(par.len(), 90);
        let chunks: Vec<&[u8]> = frags.iter().map(|f| read_video_frag(f).unwrap().1).collect();
        let parsed: Vec<(RsParity, &[u8])> = par
            .iter()
            .map(|p| {
                let (f, d) = read_video_frag(p).unwrap();
                assert!(f.is_parity() && f.is_rs_parity() && f.idx as usize >= 2 * count);
                assert!(fec_group(&f, d).is_none(), "must not be mistaken for xor parity");
                read_rs_parity(&f, d).unwrap()
            })
            .collect();
        // 每组丢 30 片（包括最后一片短的），正好能还原
        for (gi, (first, k)) in rs_groups(count).into_iter().enumerate() {
            let ps: Vec<&(RsParity, &[u8])> = parsed.iter().filter(|(p, _)| p.first == first).collect();
            let info = ps[0].0;
            assert_eq!((info.k, info.m, info.total_len), (k, 30, data.len()));
            let lost = |i: usize| (i * 7 + gi) % 5 < 2 && i < 75; // 丢一部分数据片
            let data_in: Vec<Option<&[u8]>> = (0..k).map(|i| (!lost(i)).then_some(chunks[first + i])).collect();
            let lost_n = data_in.iter().filter(|d| d.is_none()).count();
            let mut parity_in: Vec<Option<&[u8]>> = vec![None; 30];
            for (p, d) in &ps {
                parity_in[p.j] = Some(*d);
            }
            // 再丢掉一些校验片，让总丢失正好 30
            for j in 0..(30 - lost_n.min(30)) {
                if lost_n + j < 30 {
                    parity_in[j] = None;
                }
            }
            let back = rs_recover(count, &info, &data_in, &parity_in).expect("recoverable");
            for i in 0..k {
                assert_eq!(back[i], chunks[first + i], "group {gi} shard {i}");
            }
            // 再多丢一片就还原不了
            let mut fewer = parity_in.clone();
            if let Some(x) = fewer.iter_mut().find(|x| x.is_some()) {
                *x = None;
            }
            assert!(rs_recover(count, &info, &data_in, &fewer).is_none());
        }
        // 不加校验
        assert!(rs_parities(9, 0, &data, 0.0).is_empty());
        // 小帧：1 片数据，至少 1 片校验
        let small = rs_parities(1, 0, b"abc", 0.1);
        assert_eq!(small.len(), 1);
        let (f, d) = read_video_frag(&small[0]).unwrap();
        let (p, sh) = read_rs_parity(&f, d).unwrap();
        assert_eq!(rs_recover(1, &p, &[None], &[Some(sh)]).unwrap(), vec![b"abc".to_vec()]);
    }

    #[test]
    fn nack_roundtrip() {
        let p = nack_payload(9, VIDEO_LOW, 77, &[1, 5, NACK_WHOLE]);
        assert_eq!(read_nack(&p), Some((9, VIDEO_LOW, 77, vec![1, 5, NACK_WHOLE])));
        assert!(read_nack(&p[..12]).is_none());
        // 0.6：超过 NACK_MAX 个改用位图，一个都不丢（以前截到 NACK_MAX 个）
        let many: Vec<u16> = (0..1000).collect();
        assert_eq!(read_nack(&nack_payload(1, 0, 2, &many)).unwrap().3, many);
    }

    #[test]
    fn pong_with_address() {
        let ping = ping_payload(123);
        let v4: std::net::SocketAddr = "[::ffff:1.2.3.4]:5000".parse().unwrap();
        assert_eq!(read_pong(&pong_payload(&ping, v4)), Some((123, Some("1.2.3.4:5000".parse().unwrap()))));
        let v6: std::net::SocketAddr = "[2001:db8::1]:7".parse().unwrap();
        assert_eq!(read_pong(&pong_payload(&ping, v6)), Some((123, Some(v6))));
        // 旧客户端（8 字节）：原样回显；旧服务器回显 9 字节：没有地址
        assert_eq!(pong_payload(&123u64.to_le_bytes(), v4), 123u64.to_le_bytes().to_vec());
        assert_eq!(read_pong(&ping), Some((123, None)));
        assert_eq!(read_pong(&[1, 2]), None);
    }

    #[test]
    fn seq_wrap() {
        assert!(seq_newer(1, 65535));
        assert!(!seq_newer(65535, 1));
        assert!(seq_newer(10, 9));
    }
}
