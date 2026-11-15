//! The usher master playlist: one variant per quality.

/// One playable quality of a live stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    /// "1080p60", "720p60", "480p", "audio_only", ...
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub bandwidth: u64,
    pub source: bool,
    pub url: String,
}

impl Variant {
    pub fn audio_only(&self) -> bool {
        self.height == 0
    }
}

fn attr<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    // Attributes are KEY=value or KEY="value", comma-separated; quoted values
    // may contain commas (CODECS).
    let mut rest = line;
    while let Some(pos) = rest.find(key) {
        let before_ok = pos == 0 || matches!(rest.as_bytes()[pos - 1], b',' | b':');
        let after = &rest[pos + key.len()..];
        if before_ok && after.starts_with('=') {
            let v = &after[1..];
            return Some(if let Some(q) = v.strip_prefix('"') {
                q.split('"').next().unwrap_or_default()
            } else {
                v.split(',').next().unwrap_or_default()
            });
        }
        rest = &rest[pos + key.len()..];
    }
    None
}

/// Parses a master playlist; variants come out sorted best first, audio-only
/// last. Unknown lines are ignored.
pub fn parse_master(text: &str) -> Vec<Variant> {
    let mut out = Vec::new();
    let mut pending: Option<Variant> = None;
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let (width, height) = attr(rest, "RESOLUTION")
                .and_then(|r| r.split_once('x'))
                .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                .unwrap_or((0, 0));
            let name = attr(rest, "IVS-NAME")
                .or_else(|| attr(rest, "STABLE-VARIANT-ID"))
                .or_else(|| attr(rest, "VIDEO"))
                .map(str::to_owned)
                .unwrap_or_else(|| if height > 0 { format!("{height}p") } else { "audio_only".into() });
            pending = Some(Variant {
                name,
                width,
                height,
                fps: attr(rest, "FRAME-RATE").and_then(|f| f.parse().ok()).unwrap_or(0.0),
                bandwidth: attr(rest, "BANDWIDTH").and_then(|b| b.parse().ok()).unwrap_or(0),
                source: attr(rest, "IVS-VARIANT-SOURCE") == Some("source")
                    || attr(rest, "VIDEO") == Some("chunked"),
                url: String::new(),
            });
        } else if !line.is_empty() && !line.starts_with('#') {
            if let Some(mut v) = pending.take() {
                v.url = line.to_owned();
                out.push(v);
            }
        }
    }
    out.sort_by(|a, b| {
        (b.height, b.fps as u32, b.bandwidth).cmp(&(a.height, a.fps as u32, a.bandwidth))
    });
    out
}

/// The smallest video variant at least `tile_height` pixels tall (or the
/// best one if none is), never audio-only. A stream offered at 50+ fps keeps
/// that frame rate (Twitch's 480p and below are 30 fps): smooth motion
/// matters more than pixels, except in tiny tiles.
pub fn pick_for_height(variants: &[Variant], tile_height: u32) -> Option<&Variant> {
    let video: Vec<&Variant> = variants.iter().filter(|v| !v.audio_only()).collect();
    let max_fps = video.iter().map(|v| v.fps).fold(0.0f32, f32::max);
    let pool: Vec<&Variant> = if max_fps >= 50.0 && tile_height >= 300 {
        video.iter().copied().filter(|v| v.fps >= 50.0).collect()
    } else {
        video.clone()
    };
    let pool = if pool.is_empty() { video } else { pool };
    // Allow 10 % upscaling before stepping up a quality.
    let need = tile_height.saturating_mul(9) / 10;
    pool.iter().rev().find(|v| v.height >= need).or_else(|| pool.first()).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = r#"#EXTM3U
#EXT-X-SESSION-DATA:DATA-ID="NODE",VALUE="x"
#EXT-X-STREAM-INF:BANDWIDTH=230000,RESOLUTION=284x160,CODECS="avc1.4D400C,mp4a.40.2",FRAME-RATE=30.000,STABLE-VARIANT-ID="160p30",IVS-NAME="160p",IVS-VARIANT-SOURCE="transcode"
https://a/160.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=7386873,RESOLUTION=1920x1080,CODECS="avc1.64002A,mp4a.40.2",FRAME-RATE=60.000,STABLE-VARIANT-ID="1080p60",IVS-NAME="1080p60",IVS-VARIANT-SOURCE="source"
https://a/1080.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=3422999,RESOLUTION=1280x720,CODECS="avc1.4D4020,mp4a.40.2",FRAME-RATE=60.000,STABLE-VARIANT-ID="720p60",IVS-NAME="720p60",IVS-VARIANT-SOURCE="transcode"
https://a/720.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=1427999,RESOLUTION=852x480,CODECS="avc1.4D401F,mp4a.40.2",FRAME-RATE=30.000,STABLE-VARIANT-ID="480p30",IVS-NAME="480p",IVS-VARIANT-SOURCE="transcode"
https://a/480.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=160000,CODECS="mp4a.40.2",STABLE-VARIANT-ID="audio_only",IVS-NAME="audio_only",IVS-VARIANT-SOURCE="transcode"
https://a/audio.m3u8
"#;

    #[test]
    fn parses_and_sorts_variants() {
        let v = parse_master(MASTER);
        let names: Vec<&str> = v.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["1080p60", "720p60", "480p", "160p", "audio_only"]);
        assert!(v[0].source && !v[1].source);
        assert_eq!((v[0].width, v[0].height, v[0].fps), (1920, 1080, 60.0));
        assert_eq!(v[1].url, "https://a/720.m3u8");
        assert!(v[4].audio_only());
    }

    #[test]
    fn picks_the_smallest_variant_that_fills_the_tile() {
        let v = parse_master(MASTER);
        assert_eq!(pick_for_height(&v, 540).map(|v| v.name.as_str()), Some("720p60"));
        // 480p is 30 fps on a 60 fps stream: 720p60 keeps the motion smooth.
        assert_eq!(pick_for_height(&v, 500).map(|v| v.name.as_str()), Some("720p60"));
        assert_eq!(pick_for_height(&v, 320).map(|v| v.name.as_str()), Some("720p60"));
        // Tiny tiles may drop to 30 fps.
        assert_eq!(pick_for_height(&v, 280).map(|v| v.name.as_str()), Some("480p"));
        assert_eq!(pick_for_height(&v, 1080).map(|v| v.name.as_str()), Some("1080p60"));
        assert_eq!(pick_for_height(&v, 2160).map(|v| v.name.as_str()), Some("1080p60"));
        assert_eq!(pick_for_height(&v, 100).map(|v| v.name.as_str()), Some("160p"));
    }
}
