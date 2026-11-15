//! The low-latency live feeder.
//!
//! mpv's own HLS reader ignores Twitch's `#EXT-X-TWITCH-PREFETCH` segments
//! and starts several segments behind the live edge. This feeder instead:
//! - starts at the first prefetch segment, the one still being encoded;
//! - streams every segment's bytes into one continuous MPEG-TS pipe the
//!   moment they arrive (chunked transfer), and opens the next segment's
//!   request while the current one is still streaming;
//! - skips stitched ad segments (no Turbo needed to avoid them, though the
//!   picture then holds during a break);
//! - switches quality at a segment boundary without restarting the player,
//!   and takes a fresh playlist after a token refresh without a gap.
//!
//! mpv reads the pipe through its stream callback API (`gk-mpv`), so no local
//! server and no extra copy beyond the pipe exist. Every thread here only
//! blocks on the network or on the pipe; none touches the UI.

use crate::http::Http;
use crate::playlist::Variant;
use std::collections::VecDeque;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Bytes the pipe holds before the downloader waits for the player.
pub const PIPE_CAPACITY: usize = 8 * 1024 * 1024;
/// Largest single segment we accept.
const SEGMENT_LIMIT: usize = 24 * 1024 * 1024;
const CHUNK: usize = 16 * 1024;

/// One media segment from a media playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub seq: u64,
    pub url: String,
    pub duration: f64,
    /// Program date time, seconds since the Unix epoch (extrapolated for
    /// prefetch segments).
    pub pdt: Option<f64>,
    pub ad: bool,
    pub prefetch: bool,
}

/// A parsed media playlist.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MediaPlaylist {
    pub segments: Vec<Segment>,
    pub target_duration: f64,
    pub ended: bool,
}

/// Parses `2026-11-15T12:46:52.441Z` (UTC, or with a +hh:mm offset).
pub fn parse_pdt(s: &str) -> Option<f64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    let (time, offset) = if let Some(t) = rest.strip_suffix('Z') {
        (t, 0.0)
    } else if let Some(i) = rest.rfind(['+', '-']) {
        let (t, o) = rest.split_at(i);
        let sign = if o.starts_with('-') { -1.0 } else { 1.0 };
        let (oh, om) = o[1..].split_once(':').unwrap_or((&o[1..], "0"));
        (t, sign * (oh.parse::<f64>().ok()? * 3600.0 + om.parse::<f64>().ok()? * 60.0))
    } else {
        (rest, 0.0)
    };
    let mut t = time.split(':');
    let (hh, mm, ss): (f64, f64, f64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    // Days from civil (Howard Hinnant's algorithm).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days as f64 * 86_400.0 + hh * 3600.0 + mm * 60.0 + ss - offset)
}

fn attr<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let i = line.find(&format!("{key}="))?;
    let v = &line[i + key.len() + 1..];
    Some(if let Some(q) = v.strip_prefix('"') { q.split('"').next().unwrap_or_default() } else { v.split(',').next().unwrap_or_default() })
}

/// Parses a Twitch media playlist, including prefetch segments and ad marks.
pub fn parse_media(text: &str) -> MediaPlaylist {
    let mut out = MediaPlaylist { target_duration: 2.0, ..Default::default() };
    let mut seq: u64 = 0;
    let mut pending_dur = 0.0;
    let mut pending_title: Option<String> = None;
    let mut pending_pdt: Option<f64> = None;
    let mut discontinuity = false;
    let mut ad_ranges: Vec<(f64, f64)> = Vec::new();
    let in_ad = |pdt: Option<f64>, ranges: &[(f64, f64)]| pdt.is_some_and(|p| ranges.iter().any(|(s, e)| p >= *s && p < *e));
    for line in text.lines().map(str::trim) {
        if let Some(v) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            seq = v.parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            out.target_duration = v.parse().unwrap_or(2.0);
        } else if let Some(v) = line.strip_prefix("#EXTINF:") {
            let (d, title) = v.split_once(',').unwrap_or((v, ""));
            pending_dur = d.parse().unwrap_or(2.0);
            pending_title = Some(title.to_owned());
        } else if let Some(v) = line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:") {
            pending_pdt = parse_pdt(v);
        } else if line.starts_with("#EXT-X-DISCONTINUITY") {
            discontinuity = true;
        } else if line.starts_with("#EXT-X-TWITCH-LIVE-SEQUENCE") {
            if out.segments.last().is_some_and(|s| !s.ad) {
                discontinuity = false;
            }
        } else if let Some(v) = line.strip_prefix("#EXT-X-DATERANGE:") {
            let is_ad = attr(v, "CLASS") == Some("twitch-stitched-ad")
                || attr(v, "ID").is_some_and(|id| id.starts_with("stitched-ad-"));
            if is_ad {
                if let Some(start) = attr(v, "START-DATE").and_then(parse_pdt) {
                    let dur: f64 = attr(v, "DURATION").and_then(|d| d.parse().ok()).unwrap_or(60.0);
                    ad_ranges.push((start, start + dur));
                }
            }
        } else if let Some(url) = line.strip_prefix("#EXT-X-TWITCH-PREFETCH:") {
            let Some(last) = out.segments.last().cloned() else { continue };
            let regular: Vec<&Segment> = out.segments.iter().filter(|s| !s.prefetch).collect();
            let duration = if last.prefetch {
                last.duration
            } else {
                regular.iter().map(|s| s.duration).sum::<f64>() / regular.len().max(1) as f64
            };
            let pdt = last.pdt.map(|p| p + last.duration);
            let ad = discontinuity || in_ad(pdt, &ad_ranges);
            out.segments.push(Segment { seq: last.seq + 1, url: url.to_owned(), duration, pdt, ad, prefetch: true });
        } else if line.starts_with("#EXT-X-ENDLIST") {
            out.ended = true;
        } else if !line.is_empty() && !line.starts_with('#') {
            let title = pending_title.take().unwrap_or_default();
            let ad = title.contains("Amazon") || in_ad(pending_pdt, &ad_ranges);
            // Twitch sometimes puts a discontinuity between live segments.
            if !ad && out.segments.last().is_some_and(|s| !s.ad) {
                discontinuity = false;
            }
            let s = Segment { seq, url: line.to_owned(), duration: pending_dur, pdt: pending_pdt.take(), ad, prefetch: false };
            seq += 1;
            out.segments.push(s);
        }
    }
    out
}

/// Where the feeder stands, for the tile's status line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FeedStatus {
    /// Index into the variant list currently being downloaded.
    pub variant: usize,
    /// Playing through an ad break (ad segments are being skipped).
    pub in_ad: bool,
    /// The stream offers prefetch segments (low-latency mode works).
    pub low_latency: bool,
    /// The stream ended (`#EXT-X-ENDLIST`) or the playlist is gone.
    pub ended: bool,
    /// The playlist answered 403/404 several times: the token likely expired;
    /// call [`LiveFeed::replace_variants`] with fresh ones.
    pub needs_refresh: bool,
    pub last_error: Option<String>,
    pub segments_done: u64,
    pub segments_skipped: u64,
}

struct Pipe {
    buf: VecDeque<u8>,
    closed: bool,
    cancelled: bool,
}

struct Control {
    variants: Vec<Variant>,
    want: usize,
    /// Bumped on every variant or playlist change; queued segments from an
    /// older generation are dropped.
    generation: u64,
}

struct Shared {
    pipe: Mutex<Pipe>,
    readable: Condvar,
    writable: Condvar,
    control: Mutex<Control>,
    queue: Mutex<VecDeque<(u64, Segment)>>,
    queued: Condvar,
    status: Mutex<FeedStatus>,
    stop: AtomicBool,
    bytes_read: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The read side of a feed, handed to the player. Cheap to clone.
#[derive(Clone)]
pub struct FeedReader {
    shared: Arc<Shared>,
}

impl FeedReader {
    /// Blocks until bytes arrive. `Ok(0)` is the end of the stream; an error
    /// means the read was cancelled.
    pub fn read(&self, out: &mut [u8]) -> std::io::Result<usize> {
        let mut pipe = lock(&self.shared.pipe);
        loop {
            if pipe.cancelled {
                return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"));
            }
            if !pipe.buf.is_empty() {
                let n = out.len().min(pipe.buf.len());
                let (a, b) = pipe.buf.as_slices();
                if n <= a.len() {
                    out[..n].copy_from_slice(&a[..n]);
                } else {
                    out[..a.len()].copy_from_slice(a);
                    out[a.len()..n].copy_from_slice(&b[..n - a.len()]);
                }
                pipe.buf.drain(..n);
                self.shared.bytes_read.fetch_add(n as u64, Ordering::Relaxed);
                self.shared.writable.notify_one();
                return Ok(n);
            }
            if pipe.closed {
                return Ok(0);
            }
            pipe = self.shared.readable.wait(pipe).unwrap_or_else(|p| p.into_inner());
        }
    }

    /// Unblocks a pending read (mpv's cancel callback).
    pub fn cancel(&self) {
        lock(&self.shared.pipe).cancelled = true;
        self.shared.readable.notify_all();
    }
}

/// A running live feed. Dropping it stops its threads.
pub struct LiveFeed {
    shared: Arc<Shared>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl LiveFeed {
    /// Starts feeding `variants[initial]` (variants as from
    /// [`crate::Client::playback`]). `on_change` is called (from a feed
    /// thread) whenever [`FeedStatus`] changes; it should only wake the UI.
    pub fn start(http: Http, variants: Vec<Variant>, initial: usize, on_change: Arc<dyn Fn() + Send + Sync>) -> Self {
        let initial = initial.min(variants.len().saturating_sub(1));
        let shared = Arc::new(Shared {
            pipe: Mutex::new(Pipe { buf: VecDeque::with_capacity(1024 * 1024), closed: false, cancelled: false }),
            readable: Condvar::new(),
            writable: Condvar::new(),
            control: Mutex::new(Control { variants, want: initial, generation: 0 }),
            queue: Mutex::new(VecDeque::new()),
            queued: Condvar::new(),
            status: Mutex::new(FeedStatus { variant: initial, ..Default::default() }),
            stop: AtomicBool::new(false),
            bytes_read: AtomicU64::new(0),
        });
        let mut threads = Vec::new();
        {
            let (shared, http, cb) = (shared.clone(), http.clone(), on_change.clone());
            threads.push(spawn("gk-hls-playlist", move || playlist_loop(&shared, &http, &*cb)));
        }
        {
            let (shared, cb) = (shared.clone(), on_change);
            threads.push(spawn("gk-hls-writer", move || writer_loop(&shared, &http, &*cb)));
        }
        Self { shared, threads }
    }

    /// A reader for a (re)opened player stream; clears an earlier cancel.
    pub fn reader(&self) -> FeedReader {
        lock(&self.shared.pipe).cancelled = false;
        FeedReader { shared: self.shared.clone() }
    }

    pub fn status(&self) -> FeedStatus {
        lock(&self.shared.status).clone()
    }

    pub fn variants(&self) -> Vec<Variant> {
        lock(&self.shared.control).variants.clone()
    }

    /// Switches quality at the next segment boundary; the player keeps going.
    pub fn set_variant(&self, index: usize) {
        let mut c = lock(&self.shared.control);
        if index < c.variants.len() && index != c.want {
            c.want = index;
            c.generation += 1;
        }
    }

    /// Takes a fresh variant list after a token refresh; keeps the quality
    /// with the same name if it still exists.
    pub fn replace_variants(&self, variants: Vec<Variant>) {
        let mut c = lock(&self.shared.control);
        let name = c.variants.get(c.want).map(|v| v.name.clone()).unwrap_or_default();
        c.want = variants.iter().position(|v| v.name == name).unwrap_or(0);
        c.variants = variants;
        c.generation += 1;
        let mut st = lock(&self.shared.status);
        st.needs_refresh = false;
        st.last_error = None;
    }

    /// Bytes waiting in the pipe (not yet read by the player).
    pub fn buffered_bytes(&self) -> usize {
        lock(&self.shared.pipe).buf.len()
    }

    /// Seconds waiting in the pipe, estimated from the variant's bandwidth.
    pub fn buffered_secs(&self) -> f64 {
        let bytes = self.buffered_bytes() as f64;
        let c = lock(&self.shared.control);
        let bw = c.variants.get(c.want).map(|v| v.bandwidth).unwrap_or(0).max(100_000) as f64;
        bytes * 8.0 / bw
    }

    /// Drops everything buffered, so playback jumps to the live edge (the
    /// player must reopen the stream afterwards; see the app's resync).
    pub fn drop_buffered(&self) {
        lock(&self.shared.pipe).buf.clear();
        self.shared.writable.notify_all();
    }
}

impl Drop for LiveFeed {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        {
            let mut p = lock(&self.shared.pipe);
            p.closed = true;
            p.cancelled = true;
        }
        self.shared.readable.notify_all();
        self.shared.writable.notify_all();
        self.shared.queued.notify_all();
        // The threads end within one network timeout; they are not joined
        // here, so dropping a feed never blocks the UI.
        self.threads.clear();
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> std::thread::JoinHandle<()> {
    let run = move || {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
            log::error!("live feed thread panicked");
        }
    };
    match std::thread::Builder::new().name(name.into()).spawn(run) {
        Ok(h) => h,
        Err(_) => std::thread::spawn(|| {}),
    }
}

fn set_status(shared: &Shared, cb: &(dyn Fn() + Send + Sync), f: impl FnOnce(&mut FeedStatus)) {
    let changed = {
        let mut st = lock(&shared.status);
        let before = st.clone();
        f(&mut st);
        *st != before
    };
    if changed {
        cb();
    }
}

fn sleep_unless_stopped(shared: &Shared, d: Duration) {
    let end = Instant::now() + d;
    while !shared.stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= end {
            break;
        }
        std::thread::sleep((end - now).min(Duration::from_millis(50)));
    }
}

/// Polls the media playlist and queues new segments in order.
fn playlist_loop(shared: &Shared, http: &Http, cb: &(dyn Fn() + Send + Sync)) {
    let mut generation = u64::MAX;
    // Position of the next segment we want, by sequence and by time.
    let mut next_seq: Option<u64> = None;
    let mut next_pdt: Option<f64> = None;
    let mut failures = 0u32;
    // Nothing queued yet (only ads so far): the first content starts at the
    // live edge, not after the ad break.
    let mut queued_any = false;
    while !shared.stop.load(Ordering::Relaxed) {
        let (url, gen_now, want) = {
            let c = lock(&shared.control);
            (c.variants.get(c.want).map(|v| v.url.clone()), c.generation, c.want)
        };
        let Some(url) = url else {
            sleep_unless_stopped(shared, Duration::from_millis(500));
            continue;
        };
        let switched = gen_now != generation;
        let started = Instant::now();
        let text = match http.get_text(&url, &[]) {
            Ok(t) => {
                failures = 0;
                t
            }
            Err(e) => {
                failures += 1;
                let refresh = matches!(e, crate::Error::Status(403 | 404 | 410)) && failures >= 2;
                set_status(shared, cb, |s| {
                    s.last_error = Some(e.to_string());
                    s.needs_refresh |= refresh;
                });
                sleep_unless_stopped(shared, Duration::from_millis(if refresh { 1000 } else { 500 }));
                continue;
            }
        };
        let pl = parse_media(&text);
        let low_latency = pl.segments.iter().any(|s| s.prefetch);
        if switched {
            generation = gen_now;
            // Queued but not started segments came from the old variant:
            // drop them and take the same positions from the new one.
            let mut q = lock(&shared.queue);
            if let Some((_, first)) = q.iter().find(|(g, _)| *g != gen_now) {
                next_seq = Some(first.seq);
                next_pdt = first.pdt;
            }
            q.retain(|(g, _)| *g == gen_now);
        }
        let start_index = match (next_seq.filter(|_| queued_any), next_pdt) {
            (None, _) => {
                // First load: start at the first prefetch segment (being
                // encoded right now), otherwise two segments from the end.
                let live: Vec<usize> = (0..pl.segments.len()).filter(|&i| !pl.segments[i].ad).collect();
                pl.segments.iter().position(|s| s.prefetch && !s.ad).or_else(|| live.iter().rev().nth(1).copied()).unwrap_or(0)
            }
            (Some(seq), pdt) => {
                // Prefer time alignment (sequence numbers can differ between
                // variants and across token refreshes).
                let by_time = pdt.and_then(|p| pl.segments.iter().position(|s| s.pdt.is_some_and(|sp| sp > p - 0.25)));
                by_time
                    .or_else(|| pl.segments.iter().position(|s| s.seq >= seq))
                    .unwrap_or(pl.segments.len())
            }
        };
        let mut added = false;
        let mut skipped = 0u64;
        let mut any_ad = false;
        {
            let mut q = lock(&shared.queue);
            for s in pl.segments.iter().skip(start_index) {
                if queued_any && next_seq.is_some_and(|n| s.seq < n) && !switched {
                    continue;
                }
                next_seq = Some(s.seq + 1);
                next_pdt = s.pdt.map(|p| p + s.duration);
                if s.ad {
                    any_ad = true;
                    skipped += 1;
                    continue;
                }
                q.push_back((gen_now, s.clone()));
                added = true;
                queued_any = true;
            }
            // Never hold more than ~30 segments: a stalled player jumps ahead.
            while q.len() > 30 {
                q.pop_front();
            }
        }
        if added {
            shared.queued.notify_all();
        }
        set_status(shared, cb, |st| {
            st.variant = want;
            st.low_latency = low_latency;
            st.in_ad = any_ad;
            st.ended = pl.ended;
            st.last_error = None;
            st.segments_skipped += skipped;
        });
        if pl.ended {
            // Let the writer drain, then close the pipe.
            let mut q = lock(&shared.queue);
            q.push_back((u64::MAX, Segment { seq: u64::MAX, url: String::new(), duration: 0.0, pdt: None, ad: false, prefetch: false }));
            shared.queued.notify_all();
            return;
        }
        // Low latency: the next prefetch segment appears about one segment
        // duration later; poll at half that to stay ahead of it.
        let interval = if low_latency { (pl.target_duration / 2.0).clamp(0.5, 1.0) } else { (pl.target_duration / 2.0).clamp(1.0, 3.0) };
        let elapsed = started.elapsed().as_secs_f64();
        sleep_unless_stopped(shared, Duration::from_secs_f64((interval - elapsed).max(0.1)));
    }
}

type Chunks = mpsc::Receiver<Result<Vec<u8>, String>>;

/// Opens a segment and streams its body as chunks on a channel.
fn fetch(http: &Http, url: String, stop: Arc<AtomicBool>) -> Chunks {
    let (tx, rx) = mpsc::channel();
    let http = http.clone();
    let _ = std::thread::Builder::new().name("gk-hls-fetch".into()).spawn(move || {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let resp = http.agent().get(&url).call();
            let mut resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(crate::http::net(e).to_string()));
                    return;
                }
            };
            let mut reader = resp.body_mut().as_reader();
            let mut total = 0usize;
            loop {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let mut buf = vec![0u8; CHUNK];
                match reader.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => {
                        total += n;
                        if total > SEGMENT_LIMIT {
                            let _ = tx.send(Err("segment too large".into()));
                            return;
                        }
                        buf.truncate(n);
                        if tx.send(Ok(buf)).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e.to_string()));
                        return;
                    }
                }
            }
        }));
    });
    rx
}

fn next_segment(shared: &Shared) -> Option<(u64, Segment)> {
    let mut q = lock(&shared.queue);
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return None;
        }
        if let Some(s) = q.pop_front() {
            return Some(s);
        }
        q = shared.queued.wait_timeout(q, Duration::from_millis(200)).map(|r| r.0).unwrap_or_else(|p| p.into_inner().0);
    }
}

fn peek_segment(shared: &Shared) -> Option<(u64, Segment)> {
    lock(&shared.queue).front().cloned()
}

/// Writes segments into the pipe in order, one request ahead.
fn writer_loop(shared: &Shared, http: &Http, cb: &(dyn Fn() + Send + Sync)) {
    let stop = Arc::new(AtomicBool::new(false));
    // The segment opened ahead of time: (generation, seq, chunks).
    let mut ahead: Option<(u64, u64, Chunks)> = None;
    while let Some((generation, seg)) = next_segment(shared) {
        if seg.seq == u64::MAX {
            break; // end of stream marker
        }
        let current_gen = lock(&shared.control).generation;
        if generation != current_gen {
            continue;
        }
        let chunks = match ahead.take() {
            Some((g, seq, rx)) if g == generation && seq == seg.seq => rx,
            _ => fetch(http, seg.url.clone(), stop.clone()),
        };
        // Open the next segment now, so its first byte is already on the way
        // when this one ends.
        if let Some((g, next)) = peek_segment(shared) {
            if next.seq != u64::MAX {
                ahead = Some((g, next.seq, fetch(http, next.url, stop.clone())));
            }
        }
        let mut ok = true;
        let deadline = Duration::from_secs_f64((seg.duration * 3.0).max(6.0));
        loop {
            if shared.stop.load(Ordering::Relaxed) {
                stop.store(true, Ordering::Relaxed);
                return;
            }
            match chunks.recv_timeout(deadline) {
                Ok(Ok(bytes)) => {
                    if !push(shared, &bytes) {
                        stop.store(true, Ordering::Relaxed);
                        return;
                    }
                }
                Ok(Err(e)) => {
                    ok = false;
                    set_status(shared, cb, |s| s.last_error = Some(format!("segment {}: {e}", seg.seq)));
                    break;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    ok = false;
                    set_status(shared, cb, |s| s.last_error = Some(format!("segment {} timed out", seg.seq)));
                    break;
                }
            }
        }
        set_status(shared, cb, |s| {
            if ok {
                s.segments_done += 1;
            } else {
                s.segments_skipped += 1;
            }
        });
    }
    stop.store(true, Ordering::Relaxed);
    lock(&shared.pipe).closed = true;
    shared.readable.notify_all();
    set_status(shared, cb, |s| s.ended = true);
}

/// Appends bytes, waiting while the pipe is full. False when stopping.
fn push(shared: &Shared, bytes: &[u8]) -> bool {
    let mut pipe = lock(&shared.pipe);
    let mut rest = bytes;
    while !rest.is_empty() {
        if shared.stop.load(Ordering::Relaxed) || pipe.closed {
            return false;
        }
        let room = PIPE_CAPACITY.saturating_sub(pipe.buf.len());
        if room == 0 {
            pipe = shared.writable.wait_timeout(pipe, Duration::from_millis(200)).map(|r| r.0).unwrap_or_else(|p| p.into_inner().0);
            continue;
        }
        let n = room.min(rest.len());
        pipe.buf.extend(&rest[..n]);
        rest = &rest[n..];
        shared.readable.notify_one();
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const LL: &str = "#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:6
#EXT-X-MEDIA-SEQUENCE:944
#EXT-X-TWITCH-LIVE-SEQUENCE:944
#EXT-X-PROGRAM-DATE-TIME:2026-11-15T12:46:52.441Z
#EXTINF:2.000,live
https://c/944.ts
#EXT-X-PROGRAM-DATE-TIME:2026-11-15T12:46:54.441Z
#EXTINF:2.000,live
https://c/945.ts
#EXT-X-TWITCH-PREFETCH:https://c/946.ts
#EXT-X-TWITCH-PREFETCH:https://c/947.ts
";

    #[test]
    fn pdt_parses_to_epoch() {
        assert_eq!(parse_pdt("1970-01-01T00:00:00Z"), Some(0.0));
        let t = parse_pdt("2026-11-15T12:46:52.441Z").unwrap();
        assert!((t - 1_794_746_812.441).abs() < 1e-3, "{t}");
        assert_eq!(parse_pdt("2026-11-15T21:46:52.441+09:00"), Some(t));
    }

    #[test]
    fn prefetch_segments_follow_the_last_segment() {
        let p = parse_media(LL);
        let seqs: Vec<(u64, bool)> = p.segments.iter().map(|s| (s.seq, s.prefetch)).collect();
        assert_eq!(seqs, [(944, false), (945, false), (946, true), (947, true)]);
        assert_eq!(p.segments[2].url, "https://c/946.ts");
        let pdt = p.segments[3].pdt.unwrap() - p.segments[1].pdt.unwrap();
        assert!((pdt - 4.0).abs() < 1e-6);
        assert!(p.segments.iter().all(|s| !s.ad));
    }

    #[test]
    fn ads_are_marked() {
        let text = "#EXTM3U
#EXT-X-MEDIA-SEQUENCE:10
#EXT-X-DATERANGE:ID=\"stitched-ad-1\",CLASS=\"twitch-stitched-ad\",START-DATE=\"2026-11-15T12:00:02.000Z\",DURATION=4.0
#EXT-X-PROGRAM-DATE-TIME:2026-11-15T12:00:00.000Z
#EXTINF:2.000,live
https://c/10.ts
#EXT-X-DISCONTINUITY
#EXT-X-PROGRAM-DATE-TIME:2026-11-15T12:00:02.000Z
#EXTINF:2.000,Amazon|123
https://c/11.ts
#EXT-X-PROGRAM-DATE-TIME:2026-11-15T12:00:04.000Z
#EXTINF:2.000,
https://c/12.ts
#EXT-X-TWITCH-PREFETCH:https://c/13.ts
";
        let p = parse_media(text);
        let ads: Vec<bool> = p.segments.iter().map(|s| s.ad).collect();
        assert_eq!(ads, [false, true, true, true]);
    }

    #[test]
    fn pipe_read_and_close() {
        let shared = Arc::new(Shared {
            pipe: Mutex::new(Pipe { buf: VecDeque::new(), closed: false, cancelled: false }),
            readable: Condvar::new(),
            writable: Condvar::new(),
            control: Mutex::new(Control { variants: vec![], want: 0, generation: 0 }),
            queue: Mutex::new(VecDeque::new()),
            queued: Condvar::new(),
            status: Mutex::new(FeedStatus::default()),
            stop: AtomicBool::new(false),
            bytes_read: AtomicU64::new(0),
        });
        let r = FeedReader { shared: shared.clone() };
        let t = std::thread::spawn(move || {
            let mut got = Vec::new();
            let mut buf = [0u8; 3];
            loop {
                match r.read(&mut buf) {
                    Ok(0) => break got,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                    Err(_) => break got,
                }
            }
        });
        assert!(push(&shared, b"hello "));
        assert!(push(&shared, b"world"));
        lock(&shared.pipe).closed = true;
        shared.readable.notify_all();
        assert_eq!(t.join().unwrap(), b"hello world");
    }
}
