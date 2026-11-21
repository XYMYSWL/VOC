//! 语音用的 Opus 编解码（直接调 libopus，为了用 Opus 1.5 的 DRED 深度冗余）。
//!
//! DRED：编码端在每个包里附带前面最多约 1 秒的低码率冗余（放在 Opus 包的扩展区，旧版本解码器会忽略）。
//! 解码端发现中间丢了几帧，就从下一个收到的包里把这几帧还原出来，比单纯的丢包补偿（PLC）和带内 FEC（只能补一帧）好得多。

use opusic_sys as ffi;
use std::os::raw::c_int;

pub struct Encoder {
    ptr: *mut ffi::OpusEncoder,
}

// libopus 的编码器 / 解码器状态只在持有它的线程里用
unsafe impl Send for Encoder {}

impl Encoder {
    /// 单声道语音编码器
    pub fn new_voip(sample_rate: u32) -> Option<Self> {
        let mut err: c_int = 0;
        let ptr = unsafe { ffi::opus_encoder_create(sample_rate as i32, 1, ffi::OPUS_APPLICATION_VOIP as c_int, &mut err) };
        (err == ffi::OPUS_OK as c_int && !ptr.is_null()).then_some(Self { ptr })
    }

    fn ctl(&mut self, req: c_int, v: i32) -> bool {
        unsafe { ffi::opus_encoder_ctl(self.ptr, req, v) == ffi::OPUS_OK as c_int }
    }

    pub fn set_bitrate(&mut self, bps: i32) -> bool {
        self.ctl(ffi::OPUS_SET_BITRATE_REQUEST as c_int, bps)
    }

    pub fn set_inband_fec(&mut self, on: bool) -> bool {
        self.ctl(ffi::OPUS_SET_INBAND_FEC_REQUEST as c_int, on as i32)
    }

    pub fn set_packet_loss_perc(&mut self, p: i32) -> bool {
        self.ctl(ffi::OPUS_SET_PACKET_LOSS_PERC_REQUEST as c_int, p)
    }

    /// DRED 冗余时长（单位 10 ms；0 = 关）
    pub fn set_dred_duration(&mut self, frames_10ms: i32) -> bool {
        self.ctl(ffi::OPUS_SET_DRED_DURATION_REQUEST, frames_10ms)
    }

    pub fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Option<usize> {
        let n = unsafe { ffi::opus_encode(self.ptr, pcm.as_ptr(), pcm.len() as i32, out.as_mut_ptr(), out.len() as i32) };
        (n >= 0).then_some(n as usize)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { ffi::opus_encoder_destroy(self.ptr) }
    }
}

pub struct Decoder {
    ptr: *mut ffi::OpusDecoder,
    dred_dec: *mut ffi::OpusDREDDecoder,
    dred: *mut ffi::OpusDRED,
    sample_rate: u32,
}

unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new(sample_rate: u32) -> Option<Self> {
        let mut err: c_int = 0;
        let ptr = unsafe { ffi::opus_decoder_create(sample_rate as i32, 1, &mut err) };
        if err != ffi::OPUS_OK as c_int || ptr.is_null() {
            return None;
        }
        // DRED 解码器可能创建不了（编译时没开 DRED）：那就只用 FEC / PLC
        let dred_dec = unsafe { ffi::opus_dred_decoder_create(&mut err) };
        let dred = unsafe { ffi::opus_dred_alloc(&mut err) };
        Some(Self { ptr, dred_dec, dred, sample_rate })
    }

    /// 解一个包；fec = 用这个包里的带内 FEC 还原它前面那一帧（out 的长度决定还原多少采样）
    pub fn decode(&mut self, data: &[u8], out: &mut [i16], fec: bool) -> Option<usize> {
        let (p, len) = if data.is_empty() { (std::ptr::null(), 0) } else { (data.as_ptr(), data.len() as i32) };
        let n = unsafe { ffi::opus_decode(self.ptr, p, len, out.as_mut_ptr(), out.len() as i32, fec as c_int) };
        (n >= 0).then_some(n as usize)
    }

    /// 用刚收到的包 `pkt` 里带的 DRED 冗余，还原它前面丢掉的 `lost` 帧（每帧 frame 个采样），按时间顺序。
    /// 冗余不够长的那几帧（离得太远的）返回 None，由调用者用 PLC 补
    pub fn dred_recover(&mut self, pkt: &[u8], lost: usize, frame: usize) -> Vec<Option<Vec<i16>>> {
        let mut out = vec![None; lost];
        if self.dred_dec.is_null() || self.dred.is_null() || lost == 0 || pkt.is_empty() {
            return out;
        }
        let mut dred_end: c_int = 0;
        let avail = unsafe {
            ffi::opus_dred_parse(
                self.dred_dec,
                self.dred,
                pkt.as_ptr(),
                pkt.len() as i32,
                (lost * frame).min(self.sample_rate as usize) as i32,
                self.sample_rate as i32,
                &mut dred_end,
                0,
            )
        };
        if avail <= 0 {
            return out;
        }
        for (f, slot) in out.iter_mut().enumerate() {
            let offset = ((lost - f) * frame) as i32;
            if avail < offset {
                continue; // 太远了，冗余里没有
            }
            let mut pcm = vec![0i16; frame];
            let n = unsafe { ffi::opus_decoder_dred_decode(self.ptr, self.dred, offset, pcm.as_mut_ptr(), frame as i32) };
            if n > 0 {
                pcm.truncate(n as usize);
                *slot = Some(pcm);
            }
        }
        out
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            if !self.dred.is_null() {
                ffi::opus_dred_free(self.dred);
            }
            if !self.dred_dec.is_null() {
                ffi::opus_dred_decoder_destroy(self.dred_dec);
            }
            ffi::opus_decoder_destroy(self.ptr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;
    const FRAME: usize = 960;

    fn tone(i: usize) -> Vec<i16> {
        (0..FRAME).map(|n| (((i * FRAME + n) as f32 * 0.03).sin() * 8000.0 * (1.0 + ((i * FRAME + n) as f32 * 0.0007).sin())) as i16).collect()
    }

    /// DRED：连丢 10 帧（200 ms），从下一个包里还原出来；关掉 DRED 时包更小、还原不了
    #[test]
    fn dred_recovers_a_burst() {
        let mut enc = Encoder::new_voip(SR).unwrap();
        assert!(enc.set_bitrate(24_000));
        // 带内 FEC 开着时 DRED 只能用「码率 - 20 kbps」，24 kbps 下几乎没有；丢包高时关掉 FEC 改用 DRED
        assert!(enc.set_inband_fec(false));
        assert!(enc.set_packet_loss_perc(20));
        assert!(enc.set_dred_duration(50), "DRED must be compiled in");
        let mut pkts = Vec::new();
        let mut buf = [0u8; 1500];
        for i in 0..80 {
            let n = enc.encode(&tone(i), &mut buf).unwrap();
            pkts.push(buf[..n].to_vec());
        }
        let mut dec = Decoder::new(SR).unwrap();
        let mut out = vec![0i16; FRAME * 3];
        for p in &pkts[..60] {
            dec.decode(p, &mut out, false).unwrap();
        }
        // 60..70 丢了，70 到了
        let rec = dec.dred_recover(&pkts[70], 10, FRAME);
        // 24 kbps 下冗余大约 130 ms 左右：最近的几帧能还原，更早的留给 PLC
        let got = rec.iter().filter(|r| r.as_ref().is_some_and(|x| x.len() == FRAME)).count();
        assert!(got >= 5, "recovered {got}/10 frames");
        assert!(rec[9].is_some() && rec[8].is_some(), "the frames right before the packet come back first");
        // 还原出来的有声音（不是静音）
        let energy: f64 = rec.iter().flatten().flatten().map(|s| (*s as f64).powi(2)).sum::<f64>();
        assert!(energy > 1e6, "energy {energy}");
        assert!(dec.decode(&pkts[70], &mut out, false).is_some());

        // 不开 DRED：平均包长更小，也还原不出来
        let mut plain = Encoder::new_voip(SR).unwrap();
        plain.set_bitrate(24_000);
        let mut plain_pkts = Vec::new();
        for i in 0..80 {
            let n = plain.encode(&tone(i), &mut buf).unwrap();
            plain_pkts.push(buf[..n].to_vec());
        }
        let avg = |v: &[Vec<u8>]| v.iter().map(|p| p.len()).sum::<usize>() / v.len();
        println!("avg packet: dred {} bytes, plain {} bytes", avg(&pkts), avg(&plain_pkts));
        let mut dec2 = Decoder::new(SR).unwrap();
        assert!(dec2.dred_recover(&plain_pkts[70], 10, FRAME).iter().all(|r| r.is_none()));
    }
}
