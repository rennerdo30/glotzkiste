//! Emote, badge and avatar images: download with a disk cache, decode
//! (PNG, GIF, WebP, animated or not) into premultiplied-ready RGBA frames,
//! with hard caps on size and frame count.

use crate::http::Http;
use crate::{Error, Result};
use image::{AnimationDecoder, ImageDecoder, RgbaImage};
use std::io::Cursor;
use std::path::PathBuf;

pub const MAX_DOWNLOAD: u64 = 4 * 1024 * 1024;
pub const MAX_SIDE: u32 = 128;
pub const MAX_FRAMES: usize = 300;
pub const CACHE_LIMIT: u64 = 256 * 1024 * 1024;

/// One frame, straight (non-premultiplied) RGBA8.
#[derive(Clone)]
pub struct Frame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub delay_ms: u32,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Frame({}x{}, {} ms)", self.width, self.height, self.delay_ms)
    }
}

#[derive(Clone, Debug)]
pub struct DecodedImage {
    pub frames: Vec<Frame>,
}

impl DecodedImage {
    pub fn animated(&self) -> bool {
        self.frames.len() > 1
    }
    pub fn size(&self) -> (u32, u32) {
        self.frames.first().map(|f| (f.width, f.height)).unwrap_or((0, 0))
    }
}

fn cache_dir() -> Option<PathBuf> {
    let dir = crate::local_dir()?.join("cache").join("img");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn cache_path(url: &str) -> Option<PathBuf> {
    Some(cache_dir()?.join(format!("{:016x}", crate::fnv1a(url.as_bytes()))))
}

/// Downloads `url` (or reads it from the disk cache) and decodes it.
pub fn fetch_decoded(http: &Http, url: &str) -> Result<DecodedImage> {
    let cached = cache_path(url);
    if let Some(bytes) = cached.as_ref().and_then(|p| std::fs::read(p).ok()) {
        if let Ok(img) = decode(&bytes) {
            return Ok(img);
        }
    }
    let bytes = http.get_bytes(url, &[], MAX_DOWNLOAD)?;
    let img = decode(&bytes)?;
    if let Some(p) = cached {
        let tmp = p.with_extension("tmp");
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
    Ok(img)
}

fn shrink(img: RgbaImage) -> RgbaImage {
    let (w, h) = img.dimensions();
    if w <= MAX_SIDE && h <= MAX_SIDE {
        return img;
    }
    let scale = MAX_SIDE as f32 / w.max(h) as f32;
    let (nw, nh) = (((w as f32 * scale).round() as u32).max(1), ((h as f32 * scale).round() as u32).max(1));
    image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle)
}

fn frame_from(img: RgbaImage, delay_ms: u32) -> Frame {
    let img = shrink(img);
    let (width, height) = img.dimensions();
    // Browsers clamp tiny GIF delays to 100 ms; so do we (10 ms floor otherwise).
    let delay_ms = if delay_ms < 20 { 100 } else { delay_ms };
    Frame { rgba: img.into_raw(), width, height, delay_ms }
}

fn frames_from<'a>(decoder: impl AnimationDecoder<'a>) -> Result<Vec<Frame>> {
    let mut out = Vec::new();
    for f in decoder.into_frames() {
        let f = f.map_err(|e| Error::Image(e.to_string()))?;
        let (n, d) = f.delay().numer_denom_ms();
        let delay = if d == 0 { 100 } else { n / d };
        out.push(frame_from(f.into_buffer(), delay));
        if out.len() >= MAX_FRAMES {
            break;
        }
    }
    if out.is_empty() { Err(Error::Image("no frames".into())) } else { Ok(out) }
}

/// Decodes PNG, GIF or WebP bytes; animated formats keep every frame (capped).
pub fn decode(bytes: &[u8]) -> Result<DecodedImage> {
    let fmt = image::guess_format(bytes).map_err(|e| Error::Image(e.to_string()))?;
    let err = |e: image::ImageError| Error::Image(e.to_string());
    let check = |w: u32, h: u32| {
        if w == 0 || h == 0 || w > 4096 || h > 4096 { Err(Error::Image(format!("bad size {w}x{h}"))) } else { Ok(()) }
    };
    let frames = match fmt {
        image::ImageFormat::Gif => {
            let d = image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).map_err(err)?;
            let (w, h) = d.dimensions();
            check(w, h)?;
            frames_from(d)?
        }
        image::ImageFormat::WebP => {
            let d = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).map_err(err)?;
            let (w, h) = d.dimensions();
            check(w, h)?;
            if d.has_animation() {
                frames_from(d)?
            } else {
                let img = image::DynamicImage::from_decoder(d).map_err(err)?;
                vec![frame_from(img.to_rgba8(), 0)]
            }
        }
        _ => {
            let img = image::load_from_memory_with_format(bytes, fmt).map_err(err)?;
            check(img.width(), img.height())?;
            vec![frame_from(img.to_rgba8(), 0)]
        }
    };
    Ok(DecodedImage { frames })
}

/// Deletes the oldest cache files until the cache is under its limit.
/// Call once at start on a worker.
pub fn trim_cache() {
    let Some(dir) = cache_dir() else { return };
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            m.is_file().then(|| (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= CACHE_LIMIT {
        return;
    }
    files.sort_by_key(|f| f.0);
    for (_, len, path) in files {
        if total <= CACHE_LIMIT * 3 / 4 {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = RgbaImage::from_pixel(w, h, image::Rgba([255, 0, 0, 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img).write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    #[test]
    fn decodes_and_shrinks() {
        let d = decode(&png(28, 28)).unwrap();
        assert_eq!(d.size(), (28, 28));
        assert!(!d.animated());
        let big = decode(&png(512, 256)).unwrap();
        assert_eq!(big.size(), (128, 64));
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(decode(b"definitely not an image").is_err());
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn animated_gif_keeps_frames() {
        use image::codecs::gif::GifEncoder;
        let mut out = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut out);
            for i in 0..3u8 {
                let img = RgbaImage::from_pixel(8, 8, image::Rgba([i * 80, 0, 0, 255]));
                let f = image::Frame::from_parts(img, 0, 0, image::Delay::from_numer_denom_ms(50, 1));
                enc.encode_frame(f).unwrap();
            }
        }
        let d = decode(&out).unwrap();
        assert_eq!(d.frames.len(), 3);
        assert_eq!(d.frames[0].delay_ms, 50);
    }
}
