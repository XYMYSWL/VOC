//! 聊天图片：选图 / 粘贴 / 拖入 → 压到 20 MB 以内 → 端到端加密分块上传；接收端拼块、解密、解码。
//! 图片只在内存里，不写任何文件；自己的缓冲用完就覆盖清零（阅后即焚）。

use crate::net::Outgoing;
use eframe::egui;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use voc_proto::file::{self, ImageMeta, CHUNK_PLAIN, MAX_IMAGE_BYTES};

/// 聊天里显示用的图片最长边（原图更大时缩小，省显存）
const DISPLAY_MAX: u32 = 2048;
/// 超过 20 MB 时缩到的最长边
const SHRINK_MAX: u32 = 4096;
/// 解码时允许的最大尺寸（防止恶意图片撑爆内存）
const DECODE_MAX_SIDE: u32 = 16384;
const DECODE_MAX_ALLOC: u64 = 768 * 1024 * 1024;
/// 上传速度上限（字节 / 秒）：和语音共用 TCP 连接，别把它挤满
const UPLOAD_BPS: f64 = 4.0 * 1024.0 * 1024.0;
/// 接收多久没收完就作废
const RECEIVE_TTL: Duration = Duration::from_secs(120);
/// 同时最多接收几张
const MAX_INCOMING: usize = 6;

/// 覆盖清零
pub fn wipe(v: &mut Vec<u8>) {
    v.fill(0);
    v.clear();
}

/// 准备好要发送的图片
pub struct Prepared {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    /// 自己这边显示用
    pub preview: egui::ColorImage,
}

fn limits() -> image::Limits {
    let mut l = image::Limits::default();
    l.max_image_width = Some(DECODE_MAX_SIDE);
    l.max_image_height = Some(DECODE_MAX_SIDE);
    l.max_alloc = Some(DECODE_MAX_ALLOC);
    l
}

fn decode(bytes: &[u8]) -> Result<(image::DynamicImage, image::ImageFormat), String> {
    let mut r = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().map_err(|e| e.to_string())?;
    r.limits(limits());
    let fmt = r.format().ok_or("不是支持的图片格式（支持 PNG、JPEG、GIF、WebP、BMP）")?;
    let img = r.decode().map_err(|e| format!("图片解码失败：{e}"))?;
    Ok((img, fmt))
}

/// 转成显示用的贴图数据（最长边不超过 2048）
pub fn to_display(img: &image::DynamicImage) -> egui::ColorImage {
    let img = if img.width().max(img.height()) > DISPLAY_MAX {
        img.resize(DISPLAY_MAX, DISPLAY_MAX, image::imageops::FilterType::Triangle)
    } else {
        img.clone()
    };
    let rgba = img.to_rgba8();
    egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw())
}

fn mime_of(fmt: image::ImageFormat) -> &'static str {
    match fmt {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        image::ImageFormat::Bmp => "image/bmp",
        _ => "application/octet-stream",
    }
}

fn encode_jpeg(img: &image::DynamicImage, quality: u8) -> Vec<u8> {
    let mut out = Vec::new();
    let rgb = img.to_rgb8();
    let _ = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality).encode_image(&rgb);
    out
}

fn encode_png(img: &image::DynamicImage) -> Vec<u8> {
    let mut out = Vec::new();
    let _ = img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png);
    out
}

/// 原图不超过 20 MB 且格式通用就原样发（不损失画质）；否则缩小、转 JPEG，压到 20 MB 以内
fn finish(mut bytes: Vec<u8>, img: image::DynamicImage, fmt: image::ImageFormat) -> Result<Prepared, String> {
    let preview = to_display(&img);
    let ok_format = matches!(fmt, image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::Gif | image::ImageFormat::WebP);
    if bytes.len() <= MAX_IMAGE_BYTES && ok_format {
        return Ok(Prepared { bytes, mime: mime_of(fmt), width: img.width(), height: img.height(), preview });
    }
    wipe(&mut bytes);
    let small = if img.width().max(img.height()) > SHRINK_MAX {
        img.resize(SHRINK_MAX, SHRINK_MAX, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };
    for q in [90u8, 80, 70, 55] {
        let mut out = encode_jpeg(&small, q);
        if out.len() <= MAX_IMAGE_BYTES {
            return Ok(Prepared { bytes: out, mime: "image/jpeg", width: small.width(), height: small.height(), preview });
        }
        wipe(&mut out);
    }
    Err("图片太大，压缩后仍超过 20 MB".into())
}

/// 从文件准备（选图 / 拖入）
pub fn prepare_file(path: &std::path::Path) -> Result<Prepared, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > 200 * 1024 * 1024 {
        return Err("文件太大".into());
    }
    let bytes = std::fs::read(path).map_err(|e| format!("读取失败：{e}"))?;
    let (img, fmt) = decode(&bytes)?;
    finish(bytes, img, fmt)
}

/// 从剪贴板的 RGBA 像素准备（截图粘贴）：先存 PNG，太大再转 JPEG
pub fn prepare_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Result<Prepared, String> {
    let buf = image::RgbaImage::from_raw(width, height, rgba).ok_or("剪贴板图片格式不对")?;
    let img = image::DynamicImage::ImageRgba8(buf);
    let png = encode_png(&img);
    finish(png, img, image::ImageFormat::Png)
}

/// 新图片的 id 和密钥
pub fn new_id_and_key() -> (u64, [u8; 32]) {
    let r = voc_proto::auth::random_bytes::<40>();
    (u64::from_le_bytes(r[..8].try_into().unwrap()), r[8..].try_into().unwrap())
}

/// 后台加密并匀速上传；progress 记录已发出的块数。发完把明文覆盖清零。
pub fn spawn_upload(tx: Sender<Outgoing>, id: u64, key: [u8; 32], mut bytes: Vec<u8>, progress: Arc<AtomicU32>) {
    let _ = std::thread::Builder::new().name("image-upload".into()).spawn(move || {
        let total = file::chunk_count(bytes.len());
        let start = Instant::now();
        let mut sent_bytes = 0usize;
        for (seq, chunk) in bytes.chunks(CHUNK_PLAIN).enumerate() {
            let cipher = file::encrypt_chunk(&key, id, seq as u32, chunk);
            if tx.send(Outgoing::File(file::client_chunk(id, seq as u32, total, &cipher))).is_err() {
                break;
            }
            sent_bytes += chunk.len();
            progress.store(seq as u32 + 1, Ordering::Relaxed);
            // 限速
            let due = Duration::from_secs_f64(sent_bytes as f64 / UPLOAD_BPS);
            if let Some(wait) = due.checked_sub(start.elapsed()) {
                std::thread::sleep(wait);
            }
        }
        wipe(&mut bytes);
    });
}

/// 正在接收的一张图
struct Incoming {
    meta: Option<ImageMeta>,
    /// 密文块
    chunks: Vec<Option<Vec<u8>>>,
    got: u32,
    total: u32,
    last: Instant,
}

/// 接收进度 / 结果
pub enum ImageEvent {
    Progress { key: (u32, u64), got: u32, total: u32 },
    Ready { key: (u32, u64), image: egui::ColorImage },
    Failed { key: (u32, u64), reason: String },
}

/// 接收端：按 (发送者, 图片 id) 拼块，收齐后在后台解密、解码
pub struct ImageStore {
    incoming: HashMap<(u32, u64), Incoming>,
    done_tx: Sender<ImageEvent>,
    done_rx: Receiver<ImageEvent>,
}

impl Default for ImageStore {
    fn default() -> Self {
        let (done_tx, done_rx) = mpsc::channel();
        Self { incoming: HashMap::new(), done_tx, done_rx }
    }
}

impl ImageStore {
    fn entry(&mut self, key: (u32, u64), total: u32) -> Option<&mut Incoming> {
        if !self.incoming.contains_key(&key) {
            if self.incoming.len() >= MAX_INCOMING {
                return None;
            }
            self.incoming.insert(
                key,
                Incoming { meta: None, chunks: vec![None; total as usize], got: 0, total, last: Instant::now() },
            );
        }
        let e = self.incoming.get_mut(&key)?;
        (e.total == total).then_some(e)
    }

    /// 收到密钥和信息（端到端信封）
    pub fn on_meta(&mut self, from: u32, meta: ImageMeta) -> Vec<ImageEvent> {
        let key = (from, meta.id);
        let total = meta.chunks;
        let Some(e) = self.entry(key, total) else { return vec![ImageEvent::Failed { key, reason: "同时接收的图片太多".into() }] };
        e.meta = Some(meta);
        e.last = Instant::now();
        self.try_finish(key)
    }

    /// 收到一块密文（服务器转来的）
    pub fn on_chunk(&mut self, data: &[u8]) -> Vec<ImageEvent> {
        let Some((from, id, seq, total, cipher)) = file::read_server_chunk(data) else { return Vec::new() };
        let key = (from, id);
        let Some(e) = self.entry(key, total) else { return Vec::new() };
        e.last = Instant::now();
        let slot = &mut e.chunks[seq as usize];
        if slot.is_none() {
            *slot = Some(cipher.to_vec());
            e.got += 1;
        }
        let mut ev = vec![ImageEvent::Progress { key, got: e.got, total: e.total }];
        ev.extend(self.try_finish(key));
        ev
    }

    /// 密钥和所有块都到了：后台解密、解码
    fn try_finish(&mut self, key: (u32, u64)) -> Vec<ImageEvent> {
        let ready = self.incoming.get(&key).is_some_and(|e| e.meta.is_some() && e.got == e.total);
        if !ready {
            return Vec::new();
        }
        let mut e = self.incoming.remove(&key).unwrap();
        let meta = e.meta.take().unwrap();
        let tx = self.done_tx.clone();
        let _ = std::thread::Builder::new().name("image-decode".into()).spawn(move || {
            let r = (|| -> Result<egui::ColorImage, String> {
                let k: [u8; 32] = voc_proto::e2e::unb64(&meta.key)
                    .and_then(|b| b.try_into().ok())
                    .ok_or("图片密钥格式不对")?;
                let mut plain = Vec::with_capacity(meta.size as usize);
                for (seq, c) in e.chunks.iter_mut().enumerate() {
                    let mut c = c.take().ok_or("缺块")?;
                    let p = file::decrypt_chunk(&k, meta.id, seq as u32, &c).ok_or("图片校验失败（可能被篡改）");
                    wipe(&mut c);
                    let mut p = p?;
                    plain.extend_from_slice(&p);
                    wipe(&mut p);
                }
                if plain.len() != meta.size as usize {
                    wipe(&mut plain);
                    return Err("图片大小不对".into());
                }
                let r = decode(&plain).map(|(img, _)| to_display(&img));
                wipe(&mut plain);
                r
            })();
            let _ = tx.send(match r {
                Ok(image) => ImageEvent::Ready { key, image },
                Err(reason) => ImageEvent::Failed { key, reason },
            });
        });
        Vec::new()
    }

    /// 后台解码完成的、以及超时的
    pub fn poll(&mut self) -> Vec<ImageEvent> {
        let mut ev: Vec<ImageEvent> = self.done_rx.try_iter().collect();
        let now = Instant::now();
        let stale: Vec<(u32, u64)> =
            self.incoming.iter().filter(|(_, e)| now.duration_since(e.last) > RECEIVE_TTL).map(|(k, _)| *k).collect();
        for key in stale {
            if let Some(mut e) = self.incoming.remove(&key) {
                for c in e.chunks.iter_mut().flatten() {
                    wipe(c);
                }
                ev.push(ImageEvent::Failed { key, reason: "接收超时（对方网络太慢或已断开）".into() });
            }
        }
        ev
    }

    /// 关闭 VOC：没收完的也清零
    pub fn wipe_all(&mut self) {
        for e in self.incoming.values_mut() {
            for c in e.chunks.iter_mut().flatten() {
                wipe(c);
            }
        }
        self.incoming.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_png(w: u32, h: u32) -> Vec<u8> {
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([x as u8, y as u8, 128, 255])));
        encode_png(&img)
    }

    /// 发送端加密分块 → 服务器格式 → 接收端拼块、解密、解码（块乱序、密钥后到也行）
    #[test]
    fn image_roundtrip_through_store() {
        let png = test_png(300, 200);
        let (id, key) = new_id_and_key();
        let total = file::chunk_count(png.len());
        let meta = ImageMeta {
            id,
            key: voc_proto::e2e::b64(&key),
            size: png.len() as u32,
            chunks: total,
            mime: "image/png".into(),
            width: 300,
            height: 200,
        };
        let mut store = ImageStore::default();
        let mut wire: Vec<Vec<u8>> = png
            .chunks(CHUNK_PLAIN)
            .enumerate()
            .map(|(s, p)| file::server_chunk(9, &file::client_chunk(id, s as u32, total, &file::encrypt_chunk(&key, id, s as u32, p))))
            .collect();
        wire.reverse();
        for w in &wire {
            store.on_chunk(w);
        }
        store.on_meta(9, meta);
        let t = Instant::now();
        loop {
            let ev = store.poll();
            if let Some(ImageEvent::Ready { key: k, image }) = ev.into_iter().find(|e| matches!(e, ImageEvent::Ready { .. })) {
                assert_eq!(k, (9, id));
                assert_eq!(image.size, [300, 200]);
                assert_eq!(image.pixels[201].r(), 201); // 第一行 x=201 的像素
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(5), "decode did not finish");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn tampered_image_is_rejected() {
        let png = test_png(64, 64);
        let (id, key) = new_id_and_key();
        let mut store = ImageStore::default();
        let mut c = file::encrypt_chunk(&key, id, 0, &png);
        c[3] ^= 0xff;
        store.on_chunk(&file::server_chunk(5, &file::client_chunk(id, 0, 1, &c)));
        store.on_meta(
            5,
            ImageMeta { id, key: voc_proto::e2e::b64(&key), size: png.len() as u32, chunks: 1, mime: "image/png".into(), width: 64, height: 64 },
        );
        let t = Instant::now();
        loop {
            if let Some(ImageEvent::Failed { reason, .. }) = store.poll().into_iter().next() {
                assert!(reason.contains("篡改"), "{reason}");
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn small_files_are_sent_as_is_and_huge_ones_shrink() {
        let png = test_png(50, 40);
        let (img, fmt) = decode(&png).unwrap();
        let p = finish(png.clone(), img, fmt).unwrap();
        assert_eq!(p.bytes, png, "small PNG unchanged");
        assert_eq!(p.mime, "image/png");
        // 剪贴板 5000×5000 噪点图：PNG 远超 20 MB，应缩小并转成 JPEG
        let mut seed = 1u32;
        let raw: Vec<u8> = (0..5000 * 5000 * 4)
            .map(|_| {
                seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
                (seed >> 16) as u8
            })
            .collect();
        let p = prepare_rgba(5000, 5000, raw).unwrap();
        assert!(p.bytes.len() <= MAX_IMAGE_BYTES, "{}", p.bytes.len());
        assert_eq!(p.mime, "image/jpeg");
        assert!(p.width <= SHRINK_MAX && p.height <= SHRINK_MAX);
        assert!(p.preview.size[0] <= DISPLAY_MAX as usize);
    }

    #[test]
    fn wipe_zeroes_buffer() {
        let mut v = vec![7u8; 100];
        let p = v.as_ptr();
        wipe(&mut v);
        assert!(v.is_empty());
        assert!(unsafe { std::slice::from_raw_parts(p, 100) }.iter().all(|&b| b == 0));
    }
}
