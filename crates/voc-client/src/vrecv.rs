//! 屏幕共享接收端的帧重组缓冲：按帧号攒分片、乱序也能拼回去、缺片时请求补发（NACK）、
//! 用冗余校验包（FEC）直接算回丢的那一片，然后严格按帧号顺序交出完整的帧。
//!
//! 共享者支持补发时，缺了某一帧会等一小会儿（约 3 个往返）；等不到才放弃这一帧，
//! 由上层改为请求关键帧（老办法）。共享者不支持补发时只容忍很短的乱序。

use std::collections::HashMap;
use std::time::{Duration, Instant};
use voc_proto::udp::{self, VideoFrag, NACK_WHOLE, VIDEO_LOW};

/// 最多往前攒多少帧（60 帧 = 1 秒）；超出就整个重来
const MAX_AHEAD: u32 = 90;
/// 同一帧最多请求补发几次
const MAX_NACKS: u8 = 3;
/// 最新一帧的尾巴缺片时，等这么久没来才算丢（还可能在路上）
const TAIL_WAIT: Duration = Duration::from_millis(25);
/// 共享者不支持补发时，缺帧最多等这么久（乱序）
const NO_RTX_WAIT: Duration = Duration::from_millis(30);

struct Partial {
    /// 0 = 一片都没收到（整帧丢了，或者还没到）
    count: u16,
    frags: Vec<Option<Vec<u8>>>,
    parity: HashMap<u16, Vec<u8>>,
    got: usize,
    flags: u8,
    first: Instant,
    last_nack: Option<Instant>,
    nacks: u8,
}

impl Partial {
    fn new(now: Instant) -> Self {
        Self { count: 0, frags: Vec::new(), parity: HashMap::new(), got: 0, flags: 0, first: now, last_nack: None, nacks: 0 }
    }

    fn complete(&self) -> bool {
        self.count > 0 && self.got == self.count as usize
    }

    fn missing(&self) -> Vec<u16> {
        if self.count == 0 {
            return vec![NACK_WHOLE];
        }
        let m: Vec<u16> = (0..self.count).filter(|i| self.frags[*i as usize].is_none()).collect();
        if m.len() > udp::NACK_MAX {
            vec![NACK_WHOLE]
        } else {
            m
        }
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
    /// 估计的补发往返时间（ms）
    rtt_ms: f32,
    /// 统计：用校验包恢复的片数、补发收回的片数
    pub fec_recovered: u32,
    pub rtx_recovered: u32,
}

impl Default for Assembler {
    fn default() -> Self {
        Self { layer: 0, next: None, newest: 0, pending: HashMap::new(), rtt_ms: 100.0, fec_recovered: 0, rtx_recovered: 0 }
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

    fn retry_after(&self) -> Duration {
        Duration::from_millis((self.rtt_ms * 1.5).clamp(40.0, 400.0) as u64)
    }

    /// 缺帧最多等多久就放弃（改为请求关键帧）
    fn give_up_after(&self, rtx: bool) -> Duration {
        if rtx {
            Duration::from_millis((self.rtt_ms * 3.0 + 60.0).clamp(150.0, 800.0) as u64)
        } else {
            NO_RTX_WAIT
        }
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
            if f.is_parity() {
                p.parity.entry(f.idx).or_insert_with(|| data.to_vec());
            } else if p.frags[f.idx as usize].is_none() {
                p.frags[f.idx as usize] = Some(data.to_vec());
                p.got += 1;
            }
            let direct = p.got - before;
            p.try_fec();
            self.fec_recovered += (p.got - before - direct) as u32;
            // 补发请求发出后收到的缺片：估计往返时间
            p.last_nack.filter(|_| direct > 0).map(|t| now.duration_since(t))
        };
        if let Some(d) = rtt_sample {
            self.rtx_recovered += 1;
            self.rtt_ms = self.rtt_ms * 0.8 + d.as_secs_f32() * 1000.0 * 0.2;
        }
        gap
    }

    /// 交出能交的帧、放弃等不到的帧、列出要请求补发的。
    /// want_key = 上层正在等关键帧：后面有完整关键帧的话直接跳过去。rtx = 共享者支持补发
    pub fn poll(&mut self, now: Instant, want_key: bool, rtx: bool) -> Output {
        let mut out = Output::default();
        let Some(mut next) = self.next else { return out };
        if want_key {
            let span = self.newest.wrapping_sub(next);
            if let Some(k) = (0..=span)
                .map(|d| next.wrapping_add(d))
                .find(|n| self.pending.get(n).is_some_and(|p| p.complete() && p.flags & udp::VIDEO_KEY != 0))
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
                let p = self.pending.remove(&next).unwrap();
                out.frames.push((next, p.flags, p.frags.into_iter().flatten().flatten().collect()));
                next = next.wrapping_add(1);
                continue;
            }
            // 后面已经有帧到了（不是还在路上的最新一帧），且等太久了：放弃这一帧
            let behind = next != self.newest || now.duration_since(p.first) > give_up * 2;
            let exhausted = p.nacks >= MAX_NACKS && p.last_nack.is_some_and(|t| now.duration_since(t) > self.retry_after());
            if behind && (now.duration_since(p.first) > give_up || exhausted) {
                self.pending.remove(&next);
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
            let newest = self.newest;
            let span = newest.wrapping_sub(next);
            if span < MAX_AHEAD + 1 {
                for n in (0..=span).map(|d| next.wrapping_add(d)) {
                    let Some(p) = self.pending.get_mut(&n) else { continue };
                    if p.complete() || p.nacks >= MAX_NACKS {
                        continue;
                    }
                    // 最新一帧可能还在陆续到达：尾巴缺片等一会儿再要
                    if n == newest && now.duration_since(p.first) < TAIL_WAIT {
                        continue;
                    }
                    if p.last_nack.is_some_and(|t| now.duration_since(t) < retry) {
                        continue;
                    }
                    p.last_nack = Some(now);
                    p.nacks += 1;
                    out.nacks.push((n, p.missing()));
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
        let o = a.poll(t, true, true);
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
        let o = a.poll(t, true, true);
        assert!(o.frames.is_empty());
        assert_eq!(o.nacks, vec![(0, vec![2])]);
        // 马上再 poll 不重复请求
        assert!(a.poll(t + Duration::from_millis(5), true, true).nacks.is_empty());
        // 补发到了：两帧按顺序出来，往返时间估计向 60 ms 靠
        feed(&mut a, &f0[2], t + Duration::from_millis(60));
        let o = a.poll(t + Duration::from_millis(60), true, true);
        assert_eq!(o.frames.iter().map(|f| f.0).collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(a.rtx_recovered, 1);
        assert!(a.rtt_ms < 100.0);
    }

    #[test]
    fn whole_missing_frame_requested_and_given_up() {
        let mut a = Assembler::default();
        let t = Instant::now();
        for p in frame(0, true, 100).1 {
            feed(&mut a, &p, t);
        }
        assert_eq!(a.poll(t, true, true).frames.len(), 1);
        // 帧 1 整个丢了，帧 2 到了
        for p in frame(2, false, 100).1 {
            feed(&mut a, &p, t);
        }
        let o = a.poll(t, false, true);
        assert_eq!(o.nacks, vec![(1, vec![NACK_WHOLE])]);
        // 一直没补到：超时放弃，报告断档，帧 2 照样交出（上层会因为断档丢掉它、等关键帧）
        let o = a.poll(t + Duration::from_secs(1), false, true);
        assert!(o.gap);
        assert_eq!(o.frames.iter().map(|f| f.0).collect::<Vec<_>>(), vec![2]);
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
        let o = a.poll(t, true, true);
        assert_eq!(o.frames.len(), 1);
        assert_eq!(o.frames[0].2, data);
        assert_eq!(a.fec_recovered, 2);
        assert!(o.nacks.is_empty());
    }

    #[test]
    fn without_rtx_gap_is_reported_quickly() {
        let mut a = Assembler::default();
        let t = Instant::now();
        for p in frame(0, true, 100).1 {
            feed(&mut a, &p, t);
        }
        let _ = a.poll(t, true, false);
        let (_, f1) = frame(1, false, 3000);
        feed(&mut a, &f1[0], t); // 帧 1 缺片
        for p in frame(2, false, 100).1 {
            feed(&mut a, &p, t);
        }
        let o = a.poll(t + Duration::from_millis(40), false, false);
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
        let o = a.poll(t, true, true);
        assert_eq!(o.frames.iter().map(|f| f.0).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn layer_switch_and_far_jump_reset() {
        let mut a = Assembler::default();
        let t = Instant::now();
        for p in frame(0, true, 100).1 {
            feed(&mut a, &p, t);
        }
        let _ = a.poll(t, true, true);
        let low = video_fragments(500, VIDEO_KEY | VIDEO_LOW, b"x");
        let (f, d) = read_video_frag(&low[0]).unwrap();
        assert!(a.add(f, d, t), "layer switch is a gap");
        assert_eq!(a.poll(t, true, true).frames.len(), 1);
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
        let o = a.poll(t, true, true);
        assert_eq!(o.frames.iter().map(|f| f.0).collect::<Vec<_>>(), vec![u32::MAX - 1, u32::MAX, 0, 1]);
    }
}
