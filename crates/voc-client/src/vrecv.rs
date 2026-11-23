//! 屏幕共享接收端的帧重组缓冲：按帧号攒分片、乱序也能拼回去、缺片时请求补发（NACK）、
//! 用冗余校验包（FEC）直接算回丢的那一片，然后严格按帧号顺序交出完整的帧。
//!
//! 冗余校验有两种：0.5.0 的异或校验（每组只能救一片）和 0.5.1 起的 Reed-Solomon（一组里收到任意 k 片就能还原）。
//! 共享者支持补发时，缺了某一帧会等一小会儿（约 2 个往返）；等不到才放弃这一帧，
//! 由上层改为请求关键帧（老办法）。共享者不支持补发时只容忍很短的乱序。
//! 还统计原始丢包率（冗余和补发之前），每秒回报给共享者，决定加多少冗余。

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use voc_proto::udp::{self, VideoFrag, NACK_WHOLE, VIDEO_LOW};

/// 最多往前攒多少帧（60 帧 = 1 秒）；超出就整个重来
const MAX_AHEAD: u32 = 90;
/// 所有在等的帧加起来最多占多少个分片位置（约 4 个最大帧；每片最多 VIDEO_FRAG 字节，合计几十 MB 以内）
const MAX_PENDING_SLOTS: usize = 4 * udp::MAX_FRAGS as usize;
/// 同一帧最多请求补发几次
const MAX_NACKS: u8 = 3;
/// 最新一帧的尾巴缺片时，等这么久没来才算丢（还可能在路上）
const TAIL_WAIT_MIN: Duration = Duration::from_millis(8);
const TAIL_WAIT_MAX: Duration = Duration::from_millis(60);
/// 共享者不支持补发时，缺帧最多等这么久（乱序）
const NO_RTX_WAIT: Duration = Duration::from_millis(30);

struct Partial {
    /// 0 = 一片都没收到（整帧丢了，或者还没到）
    count: u16,
    frags: Vec<Option<Vec<u8>>>,
    /// 异或校验包：分片号 → 数据
    parity: HashMap<u16, Vec<u8>>,
    /// Reed-Solomon 校验包：组的第一片 → (组信息, 各校验片)
    rs: HashMap<usize, (udp::RsParity, Vec<Option<Vec<u8>>>)>,
    got: usize,
    /// 直接收到的数据片（不算冗余恢复和补发）
    direct: usize,
    /// 已经算进丢包统计
    counted: bool,
    flags: u8,
    first: Instant,
    last_nack: Option<Instant>,
    nacks: u8,
}

impl Partial {
    fn new(now: Instant) -> Self {
        Self {
            count: 0,
            frags: Vec::new(),
            parity: HashMap::new(),
            rs: HashMap::new(),
            got: 0,
            direct: 0,
            counted: false,
            flags: 0,
            first: now,
            last_nack: None,
            nacks: 0,
        }
    }

    fn complete(&self) -> bool {
        self.count > 0 && self.got == self.count as usize
    }

    /// 要请求补发的分片。有 Reed-Solomon 校验的组只要「还差几片」那么多，不用把缺的全要回来
    fn missing(&self) -> Vec<u16> {
        if self.count == 0 {
            return vec![NACK_WHOLE];
        }
        let mut m: Vec<u16> = Vec::new();
        let mut i = 0usize;
        let count = self.count as usize;
        while i < count {
            if let Some((p, sh)) = self.rs.values().find(|(p, _)| p.first == i) {
                let have = (p.first..p.first + p.k).filter(|j| self.frags[*j].is_some()).count() + sh.iter().flatten().count();
                let need = p.k.saturating_sub(have);
                m.extend((p.first..p.first + p.k).filter(|j| self.frags[*j].is_none()).take(need).map(|j| j as u16));
                i = p.first + p.k;
            } else {
                if self.frags[i].is_none() {
                    m.push(i as u16);
                }
                i += 1;
            }
        }
        // 缺很多片也只要缺的那些（补发请求会自动改用位图），不整帧重发（0.6）
        m
    }

    /// 每组只缺一片且有校验包时算回来
    fn try_fec(&mut self) {
        if self.count == 0 || self.parity.is_empty() {
            return;
        }
        let count = self.count;
        let mut recovered = Vec::new();
        for (idx, p) in &self.parity {
            let f = VideoFrag { frame: 0, idx: *idx, count, flags: self.flags };
            let Some((_, first, n)) = udp::fec_group(&f, p) else { continue };
            let lost: Vec<usize> = (first..first + n).filter(|i| self.frags[*i].is_none()).collect();
            if lost.len() != 1 {
                continue;
            }
            let others: Vec<&[u8]> = (first..first + n).filter(|i| *i != lost[0]).filter_map(|i| self.frags[i].as_deref()).collect();
            if let Some(d) = udp::fec_recover(p, &others) {
                recovered.push((lost[0], d));
            }
        }
        for (i, d) in recovered {
            if self.frags[i].is_none() {
                self.frags[i] = Some(d);
                self.got += 1;
            }
        }
    }

    /// Reed-Solomon：一组里收到的片数够了就把缺的数据片全算回来
    fn try_rs(&mut self) {
        let count = self.count as usize;
        let mut recovered = Vec::new();
        for (p, sh) in self.rs.values() {
            let range = p.first..p.first + p.k;
            if range.clone().all(|j| self.frags[j].is_some()) {
                continue;
            }
            let data: Vec<Option<&[u8]>> = range.clone().map(|j| self.frags[j].as_deref()).collect();
            let parity: Vec<Option<&[u8]>> = sh.iter().map(|x| x.as_deref()).collect();
            if let Some(all) = udp::rs_recover(count, p, &data, &parity) {
                recovered.extend(range.zip(all));
            }
        }
        for (i, d) in recovered {
            if self.frags[i].is_none() {
                self.frags[i] = Some(d);
                self.got += 1;
            }
        }
    }
}

/// 接收统计（累计）
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxStats {
    /// 用校验包恢复的片数
    pub fec_recovered: u32,
    /// 补发收回的片数
    pub rtx_recovered: u32,
    /// 请求补发的片数
    pub nack_frags: u32,
    /// 等不到、放弃的帧数
    pub gaps: u32,
    /// 原始数据片：应收 / 直接没收到（冗余和补发之前）
    pub data_total: u64,
    pub data_lost: u64,
    /// 放弃以后它的包又到了几次（放弃得太早；会自动多等）
    pub late_after_giveup: u32,
}

/// 一次处理的结果
#[derive(Default, Debug)]
pub struct Output {
    /// 按顺序交出的完整帧：(帧号, flags, 数据)
    pub frames: Vec<(u32, u8, Vec<u8>)>,
    /// 中间丢了帧（放弃等待 / 换了画质层 / 落后太多）：之后的非关键帧解不对，要等关键帧
    pub gap: bool,
    /// 要请求补发的：(帧号, 缺的分片号)
    pub nacks: Vec<(u32, Vec<u16>)>,
}

pub struct Assembler {
    layer: u8,
    next: Option<u32>,
    newest: u32,
    pending: HashMap<u32, Partial>,
    /// 估计的补发往返时间（ms，按实际补发回来的时间估计）
    rtt_ms: f32,
    /// 线路往返时间的参考值（直连的 ping / 到服务器的 ping 推算），估计值不低于它
    rtt_hint: f32,
    pub stats: RxStats,
    /// 这一段时间的原始丢包：(应收, 没收到)，回报给共享者后清零
    window: (u64, u64),
    /// 原包迟到（乱序）的程度（ms）：后一帧都开始到了，前一帧的原包才到，晚了多少。决定最新一帧尾巴缺片等多久再要补发（第 42-3 项）
    reorder_ms: f32,
    /// 一帧从第一片到最后一片（都是原包、没补发）要多久（ms）：发送端匀速发，大帧要好几毫秒才发完，
    /// 还在路上的尾巴不能当丢了去要补发
    spread_ms: f32,
    /// 最近放弃的帧：(帧号, 放弃的时间, 最后一次要补发的时间)。放弃以后它的包还陆续到 = 放弃得太早
    gave_up: VecDeque<(u32, Instant, Option<Instant>)>,
    /// 放弃以后还晚到了多久（ms，取最近的较大值、慢慢回落）：加进放弃时限。
    /// 往返估计只看「到服务器的 ping」时，「共享者→服务器→我」整条路慢很多就会过早放弃、一直要关键帧
    late_extra_ms: f32,
}

/// 记住最近放弃的多少帧
const GAVE_UP_KEEP: usize = 64;

/// 播放时限（ms）：一帧从开始收到起，预计补发回来时已经超过这么久，就不再请求补发（直接放弃、从恢复帧 / 关键帧接上），
/// 不为了救旧帧让画面越拖越晚（第 41-3 项）
const PLAY_DEADLINE_MS: f32 = 600.0;

impl Default for Assembler {
    fn default() -> Self {
        Self {
            layer: 0,
            next: None,
            newest: 0,
            pending: HashMap::new(),
            rtt_ms: 100.0,
            rtt_hint: 0.0,
            stats: RxStats::default(),
            window: (0, 0),
            reorder_ms: 0.0,
            spread_ms: 20.0,
            gave_up: VecDeque::new(),
            late_extra_ms: 0.0,
        }
    }
}

impl Assembler {
    pub fn layer(&self) -> u8 {
        self.layer
    }

    fn reset(&mut self, next: Option<u32>) {
        self.pending.clear();
        self.next = next;
        if let Some(n) = next {
            self.newest = n;
        }
    }

    /// 线路往返时间的参考值（ms，0 = 不知道）
    pub fn set_rtt_hint(&mut self, ms: u32) {
        self.rtt_hint = ms as f32;
    }

    /// 当前用的往返时间估计（ms）
    pub fn rtt(&self) -> f32 {
        self.rtt_ms.max(self.rtt_hint)
    }

    /// 最新一帧尾巴缺片等多久再要补发：线路稳（不乱序）就早点要，乱序多就多等原包一会儿（0.5.x 固定 25 ms）
    fn tail_wait(&self) -> Duration {
        let ms = (self.reorder_ms * 1.2).max(self.spread_ms * 1.3) + 5.0;
        Duration::from_millis(ms.clamp(TAIL_WAIT_MIN.as_millis() as f32, TAIL_WAIT_MAX.as_millis() as f32) as u64)
    }

    fn retry_after(&self) -> Duration {
        Duration::from_millis((self.rtt() * 1.2 + 20.0).clamp(40.0, 600.0) as u64)
    }

    /// 缺帧最多等多久就放弃（改为请求关键帧）：约 2 个往返，补发只当兜底；
    /// 以前放弃得太早（放弃后它的包还到了）就多等那么久
    fn give_up_after(&self, rtx: bool) -> Duration {
        if rtx {
            Duration::from_millis((self.rtt() * 2.0 + 100.0 + self.late_extra_ms).clamp(150.0, 1500.0) as u64)
        } else {
            NO_RTX_WAIT + Duration::from_millis(self.late_extra_ms.min(300.0) as u64)
        }
    }

    /// 已经放弃的帧又来了包：说明等得不够久。用它修正往返估计（是补发回来的）和放弃时限（晚了多少）
    fn late_for_abandoned(&mut self, frame: u32, now: Instant) {
        let Some(i) = self.gave_up.iter().position(|e| e.0 == frame) else { return };
        let (_, at, nack) = self.gave_up.remove(i).unwrap();
        self.stats.late_after_giveup += 1;
        if let Some(t) = nack {
            // 补发请求发出到现在：真实的往返（往大的方向调得快）
            let s = now.duration_since(t).as_secs_f32() * 1000.0;
            if s > self.rtt_ms {
                self.rtt_ms = self.rtt_ms * 0.5 + s * 0.5;
            }
        }
        let late = now.duration_since(at).as_secs_f32() * 1000.0;
        self.late_extra_ms = self.late_extra_ms.max(late.min(600.0));
    }

    /// 一帧处理完（交出或放弃）时计入丢包统计
    fn account(stats: &mut RxStats, window: &mut (u64, u64), p: &mut Partial) {
        if p.counted || p.count == 0 {
            return;
        }
        p.counted = true;
        let (total, lost) = (p.count as u64, (p.count as usize - p.direct.min(p.count as usize)) as u64);
        stats.data_total += total;
        stats.data_lost += lost;
        window.0 += total;
        window.1 += lost;
    }

    /// 取走这一段时间的原始丢包率（千分之几）；样本太少返回 None
    pub fn take_loss(&mut self) -> Option<u16> {
        let (total, lost) = std::mem::take(&mut self.window);
        (total >= 20).then(|| (lost * 1000 / total).min(1000) as u16)
    }

    /// 加入一个分片；返回是否发生了断档（换层 / 落后太多）
    pub fn add(&mut self, f: VideoFrag, data: &[u8], now: Instant) -> bool {
        let mut gap = false;
        let layer = f.flags & VIDEO_LOW;
        if layer != self.layer {
            self.layer = layer;
            self.reset(None);
            gap = true;
        }
        let next = *self.next.get_or_insert(f.frame);
        let ahead = f.frame.wrapping_sub(next);
        if ahead >= 0x8000_0000 {
            self.late_for_abandoned(f.frame, now);
            return gap; // 已经交出或放弃了的帧：迟到 / 重复
        }
        if ahead > MAX_AHEAD {
            self.reset(Some(f.frame));
            gap = true;
        }
        let next = self.next.unwrap();
        if f.frame.wrapping_sub(next) > self.newest.wrapping_sub(next) || self.pending.is_empty() {
            // 中间整帧没到的先占位，之后按「整帧缺失」请求补发
            let mut n = if self.pending.is_empty() { next } else { self.newest.wrapping_add(1) };
            while n != f.frame {
                self.pending.entry(n).or_insert_with(|| Partial::new(now));
                n = n.wrapping_add(1);
            }
            self.newest = f.frame;
        }
        // 在等的帧的分片位置总数有上限：坏包 / 恶意包报一个很大的分片数时不会把内存吃光
        let new_slots = self.pending.get(&f.frame).map_or(true, |p| p.count == 0);
        if new_slots && self.pending.values().map(|p| p.count as usize).sum::<usize>() + f.count as usize > MAX_PENDING_SLOTS {
            return gap;
        }
        // 后一帧什么时候开始到的（这一帧的原包要是现在才到，就是乱序晚到了这么久）
        let next_first = self.pending.get(&f.frame.wrapping_add(1)).map(|q| q.first);
        let mut orig_arrival = false;
        let rtt_sample = {
            let p = self.pending.entry(f.frame).or_insert_with(|| Partial::new(now));
            if p.count == 0 {
                p.count = f.count;
                p.frags = vec![None; f.count as usize];
            }
            if p.count != f.count {
                return gap; // 坏包
            }
            p.flags = f.flags & !0; // 所有分片 flags 相同
            let before = p.got;
            if f.is_rs_parity() {
                if let Some((info, sh)) = udp::read_rs_parity(&f, data) {
                    let e = p.rs.entry(info.first).or_insert_with(|| (info, vec![None; info.m]));
                    if e.0 == (udp::RsParity { j: e.0.j, ..info }) && info.j < e.1.len() && e.1[info.j].is_none() {
                        e.1[info.j] = Some(sh.to_vec());
                    }
                }
            } else if f.is_parity() {
                p.parity.entry(f.idx).or_insert_with(|| data.to_vec());
            } else if p.frags[f.idx as usize].is_none() {
                p.frags[f.idx as usize] = Some(data.to_vec());
                p.got += 1;
                if p.last_nack.is_none() {
                    p.direct += 1;
                    orig_arrival = true;
                }
            }
            let arrived = p.got - before;
            // 一帧的原包全到齐了（没靠补发 / 校验）：记下从第一片到最后一片用了多久
            let spread = (orig_arrival && p.complete() && p.direct == p.count as usize).then(|| now.duration_since(p.first).as_secs_f32() * 1000.0);
            if !p.complete() {
                p.try_fec();
                p.try_rs();
            }
            self.stats.fec_recovered += (p.got - before - arrived) as u32;
            // 补发请求发出后收到的缺片：估计往返时间
            (p.last_nack.filter(|_| arrived > 0).map(|t| now.duration_since(t)), spread)
        };
        let (rtt_sample, spread) = rtt_sample;
        if let Some(s) = spread {
            // 取最近的较大值，慢慢回落
            self.spread_ms = (self.spread_ms * 0.98).max(s.min(TAIL_WAIT_MAX.as_secs_f32() * 1000.0));
        }
        if let Some(d) = rtt_sample {
            self.stats.rtx_recovered += 1;
            self.rtt_ms = self.rtt_ms * 0.8 + d.as_secs_f32() * 1000.0 * 0.2;
        }
        // 乱序程度：取最近的较大值，慢慢回落（线路变稳后尾巴缺片又能早点要）
        if orig_arrival {
            let late = next_first.map(|t| now.duration_since(t).as_secs_f32() * 1000.0).unwrap_or(0.0);
            self.reorder_ms = (self.reorder_ms * 0.995).max(late.min(TAIL_WAIT_MAX.as_secs_f32() * 1000.0));
        }
        gap
    }

    /// 交出能交的帧、放弃等不到的帧、列出要请求补发的。
    /// restart = 上层正在等能重新开始的帧（flags 里有这些位的：关键帧，或者能用的恢复帧），0 = 没在等：
    /// 后面有完整的这种帧就直接跳过去。rtx = 共享者支持补发
    pub fn poll(&mut self, now: Instant, restart: u8, rtx: bool) -> Output {
        let mut out = Output::default();
        let Some(mut next) = self.next else { return out };
        if restart != 0 {
            let span = self.newest.wrapping_sub(next);
            if let Some(k) = (0..=span)
                .map(|d| next.wrapping_add(d))
                .find(|n| self.pending.get(n).is_some_and(|p| p.complete() && p.flags & restart != 0))
            {
                while next != k {
                    self.pending.remove(&next);
                    next = next.wrapping_add(1);
                }
            }
        }
        let give_up = self.give_up_after(rtx);
        loop {
            let Some(p) = self.pending.get(&next) else { break };
            if p.complete() {
                let mut p = self.pending.remove(&next).unwrap();
                Self::account(&mut self.stats, &mut self.window, &mut p);
                // 一切正常时多等的时间慢慢回落（30 帧时约 8 秒减半）
                self.late_extra_ms *= 0.997;
                out.frames.push((next, p.flags, p.frags.into_iter().flatten().flatten().collect()));
                next = next.wrapping_add(1);
                continue;
            }
            // 后面已经有帧到了（不是还在路上的最新一帧），且等太久了：放弃这一帧
            let behind = next != self.newest || now.duration_since(p.first) > give_up * 2;
            let exhausted = p.nacks >= MAX_NACKS && p.last_nack.is_some_and(|t| now.duration_since(t) > self.retry_after());
            if behind && (now.duration_since(p.first) > give_up || exhausted) {
                if let Some(mut p) = self.pending.remove(&next) {
                    Self::account(&mut self.stats, &mut self.window, &mut p);
                    if self.gave_up.len() >= GAVE_UP_KEEP {
                        self.gave_up.pop_front();
                    }
                    self.gave_up.push_back((next, now, p.last_nack));
                }
                self.stats.gaps += 1;
                next = next.wrapping_add(1);
                out.gap = true;
                continue;
            }
            break;
        }
        self.next = Some(next);
        if self.pending.is_empty() {
            self.newest = next.wrapping_sub(1);
        }
        if rtx {
            let retry = self.retry_after();
            let tail_wait = self.tail_wait();
            let rtt = self.rtt();
            let newest = self.newest;
            let span = newest.wrapping_sub(next);
            if span < MAX_AHEAD + 1 {
                for n in (0..=span).map(|d| next.wrapping_add(d)) {
                    let Some(p) = self.pending.get_mut(&n) else { continue };
                    if p.complete() || p.nacks >= MAX_NACKS {
                        continue;
                    }
                    // 最新一帧可能还在陆续到达：尾巴缺片等一会儿再要（按测到的乱序程度）
                    if n == newest && now.duration_since(p.first) < tail_wait {
                        continue;
                    }
                    // 跨帧交错的帧（第 34-5 项）：它和下一帧的分片是交替来的，下一帧开始到了不代表它的原包丢了，等这一对都到得差不多再要
                    if p.flags & udp::VIDEO_INTERLEAVED != 0 && newest.wrapping_sub(n) <= 1 && now.duration_since(p.first) < tail_wait * 2 {
                        continue;
                    }
                    if p.last_nack.is_some_and(|t| now.duration_since(t) < retry) {
                        continue;
                    }
                    // 补发回来时已经过了播放时限：不要了（等放弃后从恢复帧 / 关键帧接上）
                    if now.duration_since(p.first).as_secs_f32() * 1000.0 + rtt > PLAY_DEADLINE_MS {
                        continue;
                    }
                    let miss = p.missing();
                    if miss.is_empty() {
                        continue;
                    }
                    p.last_nack = Some(now);
                    p.nacks += 1;
                    self.stats.nack_frags += miss.len() as u32;
                    out.nacks.push((n, miss));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use voc_proto::udp::{fec_parities, read_video_frag, video_fragments, VIDEO_KEY};

    fn frame(no: u32, key: bool, len: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
        let data: Vec<u8> = (0..len).map(|i| (i as u32).wrapping_mul(31).wrapping_add(no) as u8).collect();
        let frags = video_fragments(no, if key { VIDEO_KEY } else { 0 }, &data);
        (data, frags)
    }

    fn feed(a: &mut Assembler, pkt: &[u8], now: Instant) {
        let (f, d) = read_video_frag(pkt).unwrap();
        a.add(f, d, now);
    }

    /// 第 34-5 项：交错发的两帧，第二帧开始到了、第一帧还缺片：带交错标记的先不要补发，不带标记的照旧马上要
    #[test]
    fn interleaved_pair_waits_before_nack() {
        use voc_proto::udp::{VIDEO_FLAGS_AT, VIDEO_INTERLEAVED};
        for il in [false, true] {
            let mut a = Assembler::default();
            let t = Instant::now();
            let (_, f0) = frame(0, true, 1080 * 4);
            let (_, mut f1) = frame(1, false, 1080 * 4);
            let (_, f2) = frame(2, false, 1080 * 4);
            let mut f0 = f0;
            if il {
                for f in f0.iter_mut().chain(f1.iter_mut()) {
                    f[VIDEO_FLAGS_AT] |= VIDEO_INTERLEAVED;
                }
            }
            for f in &f0 {
                feed(&mut a, f, t);
            }
            a.poll(t, 0, true);
            // 帧 1 只到了第 0、2 片（第 1 片在路上），帧 2 开始到了
            feed(&mut a, &f1[0], t);
            feed(&mut a, &f1[2], t);
            feed(&mut a, &f2[0], t + Duration::from_millis(2));
            let out = a.poll(t + Duration::from_millis(3), 0, true);
            let asked = out.nacks.iter().any(|(n, _)| *n == 1);
            assert_eq!(asked, !il, "交错 = {il}");
        }
    }

    #[test]
    fn in_order_and_reordered_frames_come_out_in_order() {
        let mut a = Assembler::default();
        let t = Instant::now();
        let (d0, f0) = frame(0, true, 3000);
        let (d1, f1) = frame(1, false, 2500);
        let (d2, f2) = frame(2, false, 2000);
        // 帧 0 分片倒序，帧 2 比帧 1 先到
        for p in f0.iter().rev().chain(f2.iter()).chain(f1.iter()) {
            feed(&mut a, p, t);
        }
        let o = a.poll(t, VIDEO_KEY, true);
        assert_eq!(o.frames.iter().map(|f| f.0).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!((&o.frames[0].2, &o.frames[1].2, &o.frames[2].2), (&d0, &d1, &d2));
        assert!(!o.gap && o.nacks.is_empty());
    }

    #[test]
    fn missing_fragment_is_nacked_then_filled() {
        let mut a = Assembler::default();
        let t = Instant::now();
        let (_, f0) = frame(0, true, 5000);
        let (_, f1) = frame(1, false, 500);
        for (i, p) in f0.iter().enumerate() {
            if i != 2 {
                feed(&mut a, p, t);
            }
        }
        feed(&mut a, &f1[0], t);
        let o = a.poll(t, VIDEO_KEY, true);
        assert!(o.frames.is_empty());
        assert_eq!(o.nacks, vec![(0, vec![2])]);
        // 马上再 poll 不重复请求
        assert!(a.poll(t + Duration::from_millis(5), VIDEO_KEY, true).nacks.is_empty());
        // 补发到了：两帧按顺序出来，往返时间估计向 60 ms 靠
        feed(&mut a, &f0[2], t + Duration::from_millis(60));
        let o = a.poll(t + Duration::from_millis(60), VIDEO_KEY, true);
        assert_eq!(o.frames.iter().map(|f| f.0).collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(a.stats.rtx_recovered, 1);
        assert!(a.rtt_ms < 100.0);
    }

    #[test]
    fn whole_missing_frame_requested_and_given_up() {
        let mut a = Assembler::default();
        let t = Instant::now();
        for p in frame(0, true, 100).1 {
            feed(&mut a, &p, t);
        }
        assert_eq!(a.poll(t, VIDEO_KEY, true).frames.len(), 1);
        // 帧 1 整个丢了，帧 2 到了
        for p in frame(2, false, 100).1 {
            feed(&mut a, &p, t);
        }
        let o = a.poll(t, 0, true);
        assert_eq!(o.nacks, vec![(1, vec![NACK_WHOLE])]);
        // 一直没补到：超时放弃，报告断档，帧 2 照样交出（上层会因为断档丢掉它、等关键帧）
        let o = a.poll(t + Duration::from_secs(1), 0, true);
        assert!(o.gap);
        assert_eq!(o.frames.iter().map(|f| f.0).collect::<Vec<_>>(), vec![2]);
    }

    /// 放弃得太早：放弃以后补发才到（整条路比 ping 估的慢）→ 往返估计调大、之后多等一会儿；一切正常后慢慢减回去
    #[test]
    fn late_packets_after_give_up_widen_the_window() {
        let mut a = Assembler::default();
        a.set_rtt_hint(20); // 以为线路很快（只看了到服务器的 ping）
        let t = Instant::now();
        for p in frame(0, true, 100).1 {
            feed(&mut a, &p, t);
        }
        a.poll(t, VIDEO_KEY, true);
        for p in frame(2, false, 100).1 {
            feed(&mut a, &p, t);
        }
        assert_eq!(a.poll(t, 0, true).nacks, vec![(1, vec![NACK_WHOLE])]);
        let before = a.give_up_after(true);
        let o = a.poll(t + Duration::from_millis(400), 0, true);
        assert!(o.gap, "gave up on frame 1");
        // 放弃以后，帧 1 的补发才到（请求后 900 ms）
        for p in frame(1, false, 100).1 {
            feed(&mut a, &p, t + Duration::from_millis(900));
        }
        assert_eq!(a.stats.late_after_giveup, 1);
        assert!(a.rtt() > 300.0, "rtt raised from the late retransmission: {}", a.rtt());
        let after = a.give_up_after(true);
        assert!(after > before + Duration::from_millis(400), "{before:?} -> {after:?}");
        // 同一帧再来包不重复计
        for p in frame(1, false, 100).1 {
            feed(&mut a, &p, t + Duration::from_millis(950));
        }
        assert_eq!(a.stats.late_after_giveup, 1);
        // 之后一直正常：多等的时间慢慢回落
        let extra = a.late_extra_ms;
        for n in 3..300u32 {
            for p in frame(n, false, 100).1 {
                feed(&mut a, &p, t + Duration::from_secs(1));
            }
            a.poll(t + Duration::from_secs(1), 0, true);
        }
        assert!(a.late_extra_ms < extra * 0.5, "{extra} -> {}", a.late_extra_ms);
    }

    #[test]
    fn fec_recovers_without_nack() {
        let mut a = Assembler::default();
        let t = Instant::now();
        let data: Vec<u8> = (0..9000u32).map(|i| (i * 13) as u8).collect();
        let frags = video_fragments(0, VIDEO_KEY, &data);
        let par = fec_parities(0, VIDEO_KEY, &data, 5);
        for (i, p) in frags.iter().enumerate() {
            if i != 1 && i != 7 {
                feed(&mut a, p, t); // 两组各丢一片
            }
        }
        for p in &par {
            feed(&mut a, p, t);
        }
        let o = a.poll(t, VIDEO_KEY, true);
        assert_eq!(o.frames.len(), 1);
        assert_eq!(o.frames[0].2, data);
        assert_eq!(a.stats.fec_recovered, 2);
        assert!(o.nacks.is_empty());
    }

    /// Reed-Solomon：30% 丢包、100% 冗余时整帧当场还原，不用补发；补发只要「还差几片」
    #[test]
    fn rs_recovers_heavy_loss_and_nacks_only_whats_needed() {
        let mut a = Assembler::default();
        let t = Instant::now();
        let data: Vec<u8> = (0..40_000u32).map(|i| (i.wrapping_mul(2246822519) >> 11) as u8).collect();
        let frags = video_fragments(0, VIDEO_KEY, &data);
        let par = voc_proto::udp::rs_parities(0, VIDEO_KEY, &data, 1.0);
        let n = frags.len();
        // 每 10 个包丢 3 个
        for (i, p) in frags.iter().chain(par.iter()).enumerate() {
            if i % 10 >= 3 {
                feed(&mut a, p, t);
            }
        }
        let o = a.poll(t + Duration::from_millis(1), VIDEO_KEY, true);
        assert_eq!(o.frames.len(), 1, "recovered without retransmission");
        assert_eq!(o.frames[0].2, data);
        assert!(o.nacks.is_empty());
        assert!(a.stats.fec_recovered > 0);
        // 丢包统计：数据片原始丢失约 30%
        let loss = a.take_loss().unwrap();
        assert!((250..=350).contains(&loss), "loss {loss}");
        assert_eq!(a.stats.data_total, n as u64);

        // 第二帧：丢得太多，冗余救不回来 → 补发只要还差的那几片
        let frags = video_fragments(1, 0, &data);
        let par = voc_proto::udp::rs_parities(1, 0, &data, 0.2);
        for (i, p) in frags.iter().enumerate() {
            if i % 2 == 0 {
                feed(&mut a, p, t);
            }
        }
        for p in &par {
            feed(&mut a, p, t);
        }
        let o = a.poll(t + Duration::from_millis(50), 0, true);
        assert!(o.frames.is_empty());
        let need = n - n.div_ceil(2) - par.len();
        assert_eq!(o.nacks, vec![(1, (0..n).filter(|i| i % 2 == 1).take(need).map(|i| i as u16).collect::<Vec<_>>())]);
        // 补发到了：整帧还原
        for i in (0..n).filter(|i| i % 2 == 1).take(need) {
            feed(&mut a, &frags[i], t + Duration::from_millis(80));
        }
        let o = a.poll(t + Duration::from_millis(80), 0, true);
        assert_eq!(o.frames.len(), 1);
        assert_eq!(o.frames[0].2, data);
    }

    #[test]
    fn rtt_hint_stretches_waits() {
        let mut a = Assembler::default();
        a.set_rtt_hint(400);
        assert!(a.give_up_after(true) >= Duration::from_millis(900));
        assert!(a.retry_after() >= Duration::from_millis(480));
        a.set_rtt_hint(0);
        assert_eq!(a.rtt(), 100.0);
    }

    #[test]
    fn without_rtx_gap_is_reported_quickly() {
        let mut a = Assembler::default();
        let t = Instant::now();
        for p in frame(0, true, 100).1 {
            feed(&mut a, &p, t);
        }
        let _ = a.poll(t, VIDEO_KEY, false);
        let (_, f1) = frame(1, false, 3000);
        feed(&mut a, &f1[0], t); // 帧 1 缺片
        for p in frame(2, false, 100).1 {
            feed(&mut a, &p, t);
        }
        let o = a.poll(t + Duration::from_millis(40), 0, false);
        assert!(o.gap && o.nacks.is_empty());
        assert_eq!(o.frames.iter().map(|f| f.0).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn waiting_for_key_jumps_to_complete_keyframe() {
        let mut a = Assembler::default();
        let t = Instant::now();
        let (_, f0) = frame(0, false, 3000);
        feed(&mut a, &f0[0], t); // 帧 0 不完整
        for p in frame(1, true, 100).1 {
            feed(&mut a, &p, t);
        }
        let o = a.poll(t, VIDEO_KEY, true);
        assert_eq!(o.frames.iter().map(|f| f.0).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn layer_switch_and_far_jump_reset() {
        let mut a = Assembler::default();
        let t = Instant::now();
        for p in frame(0, true, 100).1 {
            feed(&mut a, &p, t);
        }
        let _ = a.poll(t, VIDEO_KEY, true);
        let low = video_fragments(500, VIDEO_KEY | VIDEO_LOW, b"x");
        let (f, d) = read_video_frag(&low[0]).unwrap();
        assert!(a.add(f, d, t), "layer switch is a gap");
        assert_eq!(a.poll(t, VIDEO_KEY, true).frames.len(), 1);
        let far = video_fragments(5000, VIDEO_LOW, b"y");
        let (f, d) = read_video_frag(&far[0]).unwrap();
        assert!(a.add(f, d, t), "far jump is a gap");
    }

    #[test]
    fn frame_numbers_wrap() {
        let mut a = Assembler::default();
        let t = Instant::now();
        for no in [u32::MAX - 1, u32::MAX, 0, 1] {
            for p in frame(no, no == u32::MAX - 1, 100).1 {
                feed(&mut a, &p, t);
            }
        }
        let o = a.poll(t, VIDEO_KEY, true);
        assert_eq!(o.frames.iter().map(|f| f.0).collect::<Vec<_>>(), vec![u32::MAX - 1, u32::MAX, 0, 1]);
    }
}
