//! One live stream: its control thread owns the low-latency feeder and the
//! mpv player, drains mpv events, keeps latency at the target, recovers from
//! expired tokens and offline phases, and reports a compact status to the UI
//! only when something visible changed. The UI thread never waits for it.

use crate::placement::Placer;
use gk_mpv::{EndReason, Event, Format, Libmpv, Mpv, StreamReader, Value};
use gk_render::Presenter;
use gk_twitch::live::{FeedReader, LiveFeed};
use gk_twitch::playlist::Variant;
use gk_twitch::{Client, Error as TwError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Quality {
    /// The smallest variant that fills the tile.
    #[default]
    Auto,
    /// A variant by name ("1080p60", "720p60", "audio_only", ...).
    Named(String),
}

/// How hard to chase the live edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Latency {
    /// As close as possible (about 0.4 s of buffer).
    #[default]
    Ultra,
    /// A little cushion (1 s).
    Low,
    /// Smooth on shaky networks (2.5 s).
    Smooth,
}

impl Latency {
    pub fn target(self) -> f64 {
        match self {
            Latency::Ultra => 0.4,
            Latency::Low => 1.0,
            Latency::Smooth => 2.5,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Latency::Ultra => "Ultra low",
            Latency::Low => "Low",
            Latency::Smooth => "Smooth",
        }
    }
}

/// Where a stream's video is decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DecodeOn {
    /// The GPU that draws the window, zero-copy (normal).
    #[default]
    Gpu,
    /// A second GPU (by adapter LUID): its own video decoder and scaler;
    /// each finished tile-sized frame is copied over to the main GPU.
    Second((u32, i32)),
    /// The CPU (when the GPU's decoder is full).
    Cpu,
}

impl DecodeOn {
    fn hwdec(self) -> &'static str {
        match self {
            DecodeOn::Gpu | DecodeOn::Second(_) => "d3d11va",
            DecodeOn::Cpu => "no",
        }
    }

    /// The GPU that draws (and so decodes) the video; `None`: the main one.
    pub fn luid(self) -> Option<(u32, i32)> {
        match self {
            DecodeOn::Second(l) => Some(l),
            _ => None,
        }
    }
}

pub enum Cmd {
    Decoder(DecodeOn),
    Quality(Quality),
    TileHeight(u32),
    Volume(f64),
    Mute(bool),
    Visible(bool),
    Latency(Latency),
    Reload,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum State {
    #[default]
    Starting,
    Playing,
    Offline,
    Error(String),
}

/// What the tile shows. Compared before posting, so equal states never wake
/// the UI.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Status {
    pub state: State,
    /// Variant names, best first.
    pub variants: Vec<String>,
    /// The variant being played.
    pub current: String,
    /// Seconds behind our download edge (demuxer + pipe), 0.1 s steps.
    pub buffer_tenths: u32,
    pub speed_up: bool,
    pub drops: i64,
    pub turbo: bool,
    pub ads_expected: bool,
    pub in_ad: bool,
    pub low_latency: bool,
    pub hwdec: String,
    pub video_height: i64,
    /// Where the video is decoded, and that GPU's short name.
    pub decode_on: DecodeOn,
    pub decoder: String,
}

/// The download rate covers this much time (segments arrive every ~2 s).
const RATE_WINDOW: Duration = Duration::from_secs(4);

/// kbit/s between the oldest and newest sample; 0 until a second passed.
fn download_rate(samples: &std::collections::VecDeque<(Instant, u64)>, now: Instant) -> f64 {
    let (Some((t0, b0)), Some((_, b1))) = (samples.front(), samples.back()) else { return 0.0 };
    let secs = now.duration_since(*t0).as_secs_f64();
    if secs < 1.0 {
        return 0.0;
    }
    b1.saturating_sub(*b0) as f64 * 8.0 / 1000.0 / secs
}

/// True while the stats window is open: stream threads then sample mpv
/// twice a second. Closed, the stats cost nothing.
pub static STATS_ON: AtomicBool = AtomicBool::new(false);

/// Many tiles are visible: cheaper scaling (mpv's `fast` profile), which
/// small tiles do not show.
pub static MANY_TILES: AtomicBool = AtomicBool::new(false);

/// Detailed numbers for the stats window (sampled only while it is open).
#[derive(Debug, Clone, Default)]
pub struct StreamStats {
    pub sampled: Option<Instant>,
    /// Frames per second leaving the decoder/filters (mpv's estimate).
    pub decoded_fps: f64,
    /// The stream's own frame rate.
    pub source_fps: f64,
    /// Frames dropped to keep sync (video output).
    pub drops_vo: i64,
    /// Frames the decoder dropped.
    pub drops_decoder: i64,
    /// Frames shown late.
    pub delayed: i64,
    /// Frames shown at the wrong time.
    pub mistimed: i64,
    pub width: i64,
    pub height: i64,
    pub codec: String,
    pub hwdec: String,
    pub video_kbps: f64,
    pub audio_kbps: f64,
    /// What arrives from the network right now.
    pub download_kbps: f64,
    pub cache_s: f64,
    pub pipe_s: f64,
    pub speed: f64,
    pub container: String,
    pub segments_done: u64,
    pub segments_skipped: u64,
    pub reopens: u64,
}

/// Shared by every stream thread.
#[derive(Clone)]
pub struct Env {
    pub lib: Arc<Libmpv>,
    pub client: Arc<Mutex<Client>>,
    pub presenter: Arc<Presenter>,
    /// Wakes the UI (an event loop proxy send).
    pub notify: Arc<dyn Fn() + Send + Sync>,
    pub muted_by_flag: bool,
    /// Which GPU decodes what (placement.rs).
    pub placer: Arc<Mutex<Placer>>,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub quality: Quality,
    pub volume: f64,
    pub muted: bool,
    pub latency: Latency,
}

pub struct StreamHandle {
    #[allow(dead_code)]
    pub id: u64,
    pub channel: String,
    tx: Sender<Cmd>,
    mpv: Arc<OnceLock<Mpv>>,
    status: Arc<Mutex<Status>>,
    stats: Arc<Mutex<StreamStats>>,
    stop: Arc<AtomicBool>,
}

impl StreamHandle {
    pub fn spawn(id: u64, channel: String, env: Env, settings: Settings) -> Self {
        let (tx, rx) = mpsc::channel();
        let mpv = Arc::new(OnceLock::new());
        let status = Arc::new(Mutex::new(Status::default()));
        let stats = Arc::new(Mutex::new(StreamStats::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let ctl = Control {
            stats: stats.clone(),
            id,
            channel: channel.clone(),
            env,
            settings,
            rx,
            mpv_slot: mpv.clone(),
            status: status.clone(),
            stop: stop.clone(),
            posted: Status::default(),
            last_post: Instant::now() - Duration::from_secs(1),
            tile_height: 540,
            visible: true,
            decode_on: DecodeOn::Gpu,
            reopen: false,
            restore_vid: false,
        };
        let name = format!("gk-stream-{id}");
        let _ = std::thread::Builder::new().name(name).spawn(move || {
            let presenter = ctl.env.presenter.clone();
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ctl.run())).is_err() {
                log::error!("stream {id} control thread panicked");
            }
            presenter.remove_stream(id);
        });
        Self { id, channel, tx, mpv, status, stats, stop }
    }

    pub fn stats(&self) -> StreamStats {
        self.stats.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
        if let Some(m) = self.mpv.get() {
            m.wakeup();
        }
    }

    pub fn status(&self) -> Status {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.send(Cmd::Stop);
    }
}

struct Feeder(FeedReader);

impl StreamReader for Feeder {
    fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
    fn cancel(&self) {
        self.0.cancel()
    }
}

struct Control {
    id: u64,
    channel: String,
    env: Env,
    settings: Settings,
    rx: Receiver<Cmd>,
    mpv_slot: Arc<OnceLock<Mpv>>,
    status: Arc<Mutex<Status>>,
    stats: Arc<Mutex<StreamStats>>,
    stop: Arc<AtomicBool>,
    posted: Status,
    last_post: Instant,
    /// The tile's height in pixels, kept across sessions (it may arrive
    /// before the stream is playing).
    tile_height: u32,
    /// The tile is in the visible tab (video decoding on).
    visible: bool,
    decode_on: DecodeOn,
    /// The video moved to another GPU: reopen at the next segment boundary
    /// (the decoder starts over there).
    reopen: bool,
    /// Video was switched off for a move; on again with the next load.
    restore_vid: bool,
}

const P_CACHE: u64 = 1;
const P_DROPS: u64 = 2;
const P_HEIGHT: u64 = 3;
const P_HWDEC: u64 = 4;

enum Next {
    Resolve,
    Stop,
}

/// Demo "channels" (`--demo`) start with `~`, which no real channel name can.
pub fn is_demo(channel: &str) -> bool {
    channel.starts_with('~')
}

/// Generated video for a demo channel (mpv's own lavfi sources: no media
/// files, nothing from anyone else).
fn demo_source(channel: &str) -> &'static str {
    match channel {
        "~aurora" => {
            "av://lavfi:gradients=size=1280x720:rate=30:speed=0.012:nb_colors=4:c0=0x1e1b4b:c1=0x7c3aed:c2=0x22d3ee:c3=0xf472b6"
        }
        "~fractal" => "av://lavfi:mandelbrot=size=1280x720:rate=30",
        "~life" => {
            "av://lavfi:life=size=480x270:rate=30:mold=25:ratio=0.12:life_color=#a78bfa:death_color=#1e1b4b,scale=1280:720:flags=neighbor"
        }
        _ => "av://lavfi:testsrc2=size=1280x720:rate=30",
    }
}

impl Control {
    fn set(&mut self, f: impl FnOnce(&mut Status)) {
        if let Ok(mut s) = self.status.lock() {
            f(&mut s);
        }
        self.post(false);
    }

    /// Wakes the UI if the status changed (rate-limited for buffer-only
    /// changes).
    fn post(&mut self, force: bool) {
        let now = self.status.lock().map(|s| s.clone()).unwrap_or_default();
        if now == self.posted {
            return;
        }
        let only_buffer =
            Status { buffer_tenths: self.posted.buffer_tenths, drops: self.posted.drops, ..now.clone() } == self.posted;
        if only_buffer && !force && self.last_post.elapsed() < Duration::from_millis(500) {
            return;
        }
        self.posted = now;
        self.last_post = Instant::now();
        (self.env.notify)();
    }

    fn create_mpv(&self) -> Result<Mpv, String> {
        let mpv = Mpv::create(self.env.lib.clone()).map_err(|e| e.to_string())?;
        let mute = self.settings.muted || self.env.muted_by_flag;
        let volume = format!("{}", self.settings.volume.clamp(0.0, 130.0));
        // Streams nobody hears decode no audio at all.
        let aid = if self.settings.muted { "no" } else { "auto" };
        let opts: &[(&str, &str)] = &[
            ("config", "no"),
            ("terminal", "no"),
            ("input-default-bindings", "no"),
            ("input-vo-keyboard", "no"),
            ("osc", "no"),
            ("ytdl", "no"),
            ("load-scripts", "no"),
            ("idle", "yes"),
            ("keep-open", "no"),
            ("vo", "libmpv"),
            ("hwdec", "d3d11va"),
            ("profile", "low-latency"),
            ("cache", "yes"),
            ("demuxer-max-bytes", "32MiB"),
            ("demuxer-max-back-bytes", "0"),
            ("audio-buffer", "0.1"),
            ("interpolation", "no"),
            ("mute", if mute { "yes" } else { "no" }),
            ("aid", aid),
            ("volume", &volume),
        ];
        for (k, v) in opts {
            if let Err(e) = mpv.set_option(k, v) {
                log::warn!("stream {}: option {k}={v}: {e}", self.id);
            }
        }
        // Compiled shaders are cached on disk: a render context after the
        // first one (and every later start) skips the compile, which made
        // a start with a hundred streams take many seconds per worker.
        if let Some(dir) = gk_twitch::local_dir().map(|d| d.join("shader-cache")) {
            let _ = std::fs::create_dir_all(&dir);
            for (k, v) in [("gpu-shader-cache", "yes"), ("gpu-shader-cache-dir", &*dir.to_string_lossy())] {
                if let Err(e) = mpv.set_option(k, v) {
                    log::warn!("stream {}: option {k}: {e}", self.id);
                }
            }
        }
        mpv.initialize().map_err(|e| e.to_string())?;
        // mpv's own warnings go to our log (glotzkiste.log).
        let _ = mpv.request_log_messages("warn");
        for (id, name, fmt) in [
            (P_CACHE, "demuxer-cache-duration", Format::Double),
            (P_DROPS, "frame-drop-count", Format::Int64),
            (P_HEIGHT, "video-params/h", Format::Int64),
            (P_HWDEC, "hwdec-current", Format::String),
        ] {
            let _ = mpv.observe(id, name, fmt);
        }
        Ok(mpv)
    }

    fn run(mut self) {
        let mpv = match self.create_mpv() {
            Ok(m) => m,
            Err(e) => {
                self.set(|s| s.state = State::Error(format!("player: {e}")));
                return;
            }
        };
        let _ = self.mpv_slot.set(mpv.clone());
        // The feed currently attached to the `gkfeed://` protocol.
        let current: Arc<Mutex<Option<Arc<LiveFeed>>>> = Arc::new(Mutex::new(None));
        {
            let current = current.clone();
            let opened = mpv.add_stream_protocol(
                "gkfeed",
                Box::new(move |_uri| {
                    let feed = current.lock().ok()?.clone()?;
                    Some(Arc::new(Feeder(feed.reader())) as Arc<dyn StreamReader>)
                }),
            );
            if let Err(e) = opened {
                self.set(|s| s.state = State::Error(format!("player: {e}")));
                return;
            }
        }
        // mpv needs its render context before it loads anything. It goes
        // to the GPU whose decoder has room, the main GPU last.
        let want = self.with_placer(|p| p.choose());
        let (at, err) = self.env.presenter.place_stream(self.id, &mpv, want.luid());
        if let Some(e) = err {
            log::warn!("stream {}: {}: {e}; on the main GPU", self.channel, self.with_placer(|p| p.name(want)));
            if let Some(l) = want.luid() {
                self.with_placer(|p| p.mark_bad(l));
            }
        }
        self.landed(at.map_or(DecodeOn::Gpu, DecodeOn::Second));

        if is_demo(&self.channel) {
            self.demo(&mpv);
            let _ = mpv.command(&["stop"]);
            return;
        }
        let mut backoff = Duration::from_secs(2);
        loop {
            match self.session(&mpv, &current) {
                Ok(Next::Stop) => break,
                Ok(Next::Resolve) => backoff = Duration::from_secs(2),
                Err(wait) => {
                    if !self.idle(&mpv, wait.max(backoff)) {
                        break;
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
            if self.stop.load(Ordering::Relaxed) {
                break;
            }
        }
        if let Ok(mut c) = current.lock() {
            *c = None;
        }
        let _ = mpv.command(&["stop"]);
    }

    /// A demo channel: generated video, played until told to stop.
    fn demo(&mut self, mpv: &Mpv) {
        self.set(|s| {
            s.variants = vec!["source".into()];
            s.current = "source".into();
            s.low_latency = true;
        });
        if let Err(e) = mpv.command(&["loadfile", demo_source(&self.channel)]) {
            self.set(|s| s.state = State::Error(e.to_string()));
            return;
        }
        let mut last_sample = Instant::now() - Duration::from_secs(1);
        loop {
            if STATS_ON.load(Ordering::Relaxed) && last_sample.elapsed() >= Duration::from_millis(500) {
                last_sample = Instant::now();
                let d = |n: &str| mpv.get_double(n).unwrap_or(0.0);
                let i = |n: &str| mpv.get_i64(n).unwrap_or(0);
                let snap = StreamStats {
                    sampled: Some(last_sample),
                    decoded_fps: d("estimated-vf-fps"),
                    source_fps: d("container-fps"),
                    drops_vo: i("frame-drop-count"),
                    drops_decoder: i("decoder-frame-drop-count"),
                    delayed: i("vo-delayed-frame-count"),
                    mistimed: i("mistimed-frame-count"),
                    width: i("video-params/w"),
                    height: i("video-params/h"),
                    codec: mpv.get_string("video-format").unwrap_or_default(),
                    hwdec: mpv.get_string("hwdec-current").unwrap_or_default(),
                    cache_s: d("demuxer-cache-duration").min(0.2),
                    speed: 1.0,
                    container: "generated".into(),
                    ..Default::default()
                };
                if let Ok(mut s) = self.stats.lock() {
                    *s = snap;
                }
            }
            loop {
                match self.rx.try_recv() {
                    Ok(Cmd::Stop) | Err(TryRecvError::Disconnected) => return,
                    Ok(Cmd::Reload) | Ok(Cmd::TileHeight(_)) | Ok(Cmd::Quality(_)) => {}
                    Ok(other) => self.apply_setting(mpv, other),
                    Err(TryRecvError::Empty) => break,
                }
            }
            if self.stop.load(Ordering::Relaxed) {
                return;
            }
            if std::mem::take(&mut self.reopen) {
                self.restore_video(mpv);
                let _ = mpv.command(&["loadfile", demo_source(&self.channel)]);
            }
            while let Some(ev) = mpv.wait_event(0.2) {
                match ev {
                    Event::PropertyChange { id: P_HEIGHT, value: Value::Int64(h), .. } if h > 0 => {
                        self.set(|s| {
                            s.video_height = h;
                            s.state = State::Playing;
                            s.buffer_tenths = 2;
                        });
                    }
                    Event::PropertyChange { id: P_DROPS, value: Value::Int64(d), .. } => self.set(|s| s.drops = d),
                    Event::PropertyChange { id: P_HWDEC, value: Value::String(v), .. } => self.set(|s| s.hwdec = v),
                    Event::Shutdown => return,
                    _ => {}
                }
            }
        }
    }

    /// Waits up to `d` handling commands; false when told to stop.
    fn idle(&mut self, mpv: &Mpv, d: Duration) -> bool {
        let end = Instant::now() + d;
        while Instant::now() < end {
            while let Some(ev) = mpv.wait_event(0.2) {
                if matches!(ev, Event::Shutdown) {
                    return false;
                }
            }
            loop {
                match self.rx.try_recv() {
                    Ok(Cmd::Stop) | Err(TryRecvError::Disconnected) => return false,
                    Ok(Cmd::Reload) => return true,
                    Ok(other) => self.apply_setting(mpv, other),
                    Err(TryRecvError::Empty) => break,
                }
            }
            if self.stop.load(Ordering::Relaxed) {
                return false;
            }
        }
        true
    }

    fn apply_setting(&mut self, mpv: &Mpv, cmd: Cmd) {
        match cmd {
            Cmd::Volume(v) => {
                self.settings.volume = v;
                let _ = mpv.command(&["set", "volume", &format!("{}", v.clamp(0.0, 130.0))]);
            }
            Cmd::Mute(m) => {
                self.settings.muted = m;
                let mute = m || self.env.muted_by_flag;
                let _ = mpv.command(&["set", "mute", if mute { "yes" } else { "no" }]);
                // Muted streams decode no audio; the audible one gets it back.
                let _ = mpv.command(&["set", "aid", if m { "no" } else { "auto" }]);
            }
            Cmd::Visible(v) => {
                self.visible = v;
                if !self.restore_vid {
                    let _ = mpv.command(&["set", "vid", if v { "auto" } else { "no" }]);
                }
            }
            Cmd::Decoder(d) => self.switch_decoder(mpv, d),
            Cmd::Latency(l) => self.settings.latency = l,
            Cmd::Quality(q) => self.settings.quality = q,
            Cmd::TileHeight(h) => self.tile_height = h,
            Cmd::Reload | Cmd::Stop => {}
        }
    }

    /// Decodes on another GPU or the CPU. A move between GPUs takes the
    /// video's render context along; the player reopens at the next segment
    /// boundary, where the new decoder can start cleanly.
    fn with_placer<T>(&self, f: impl FnOnce(&mut Placer) -> T) -> T {
        let mut p = self.env.placer.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut p)
    }

    /// The decoder of this stream's GPU is busy: Auto quality sizes by tile
    /// height alone (a 30 fps variant costs half).
    fn busy(&self) -> bool {
        self.with_placer(|p| p.busy(self.decode_on))
    }

    fn landed(&mut self, on: DecodeOn) {
        self.decode_on = on;
        let name = self.with_placer(|p| p.name(on));
        log::info!("stream {}: video decoding on {name}", self.channel);
        self.set(|s| {
            s.decode_on = on;
            s.decoder = name;
        });
    }

    /// Decodes on another GPU or the CPU. A move between GPUs takes the
    /// video's render context along; the player reopens at the next segment
    /// boundary, where the new decoder can start cleanly.
    fn switch_decoder(&mut self, mpv: &Mpv, d: DecodeOn) {
        if d == self.decode_on {
            return;
        }
        let mut on = d;
        if d.luid() != self.decode_on.luid() {
            // No video output while its render context moves.
            let _ = mpv.command(&["set", "vid", "no"]);
            let (at, err) = self.env.presenter.move_stream(self.id, mpv, d.luid());
            if let Some(e) = err {
                log::warn!("stream {}: cannot decode on {d:?}: {e}", self.channel);
            }
            if at == self.decode_on.luid() {
                // Nothing moved.
                let _ = mpv.command(&["set", "vid", if self.visible { "auto" } else { "no" }]);
                return;
            }
            self.reopen = true;
            self.restore_vid = true;
            if d != DecodeOn::Cpu {
                on = at.map_or(DecodeOn::Gpu, DecodeOn::Second);
            }
        }
        let _ = mpv.command(&["set", "hwdec", on.hwdec()]);
        self.landed(on);
    }

    /// Video on again (if visible) before a load, after a move.
    fn restore_video(&mut self, mpv: &Mpv) {
        if std::mem::take(&mut self.restore_vid) {
            let _ = mpv.command(&["set", "vid", if self.visible { "auto" } else { "no" }]);
        }
    }

    fn pick(&self, variants: &[Variant], tile_height: u32) -> usize {
        match &self.settings.quality {
            Quality::Named(n) => variants.iter().position(|v| &v.name == n),
            Quality::Auto => {
                // Settings may cap automatic quality (a smaller tile still
                // picks smaller).
                // picks smaller). Variants above the cap are left out first,
                // so the 60 fps preference cannot step over it.
                let cap = crate::settings::max_auto_height();
                let capped: Vec<Variant> = if cap > 0 {
                    variants.iter().filter(|v| v.audio_only() || v.height <= cap).cloned().collect()
                } else {
                    Vec::new()
                };
                let pool = if capped.iter().any(|v| !v.audio_only()) { &capped[..] } else { variants };
                let h = if cap > 0 { tile_height.min(cap) } else { tile_height };
                gk_twitch::playlist::pick(pool, h.max(1), !self.busy())
                    .and_then(|p| variants.iter().position(|v| v.name == p.name))
            }
        }
        .unwrap_or(0)
    }

    fn playback(&self) -> Result<gk_twitch::Playback, TwError> {
        let client = self.env.client.lock().map(|c| c.clone()).map_err(|_| TwError::Io("client lock".into()))?;
        client.playback(&self.channel)
    }

    /// One playing session: resolve, feed, play until it ends.
    /// `Err(wait)` asks for a retry after `wait`.
    fn session(&mut self, mpv: &Mpv, current: &Arc<Mutex<Option<Arc<LiveFeed>>>>) -> Result<Next, Duration> {
        self.set(|s| {
            if !matches!(s.state, State::Offline) {
                s.state = State::Starting;
            }
        });
        let pb = match self.playback() {
            Ok(p) => p,
            Err(TwError::Offline) | Err(TwError::NotFound) => {
                self.set(|s| s.state = State::Offline);
                return Err(Duration::from_secs(30));
            }
            Err(e) => {
                self.set(|s| s.state = State::Error(e.to_string()));
                return Err(Duration::from_secs(5));
            }
        };
        // Commands that arrived while connecting (the tile size above all).
        loop {
            match self.rx.try_recv() {
                Ok(Cmd::Stop) | Err(TryRecvError::Disconnected) => return Ok(Next::Stop),
                Ok(Cmd::Reload) => {}
                Ok(other) => self.apply_setting(mpv, other),
                Err(TryRecvError::Empty) => break,
            }
        }
        let mut tile_height: u32 = self.tile_height;
        let mut pending_height: Option<(u32, Instant)> = None;
        let idx = self.pick(&pb.variants, tile_height);
        let http = self.env.client.lock().map(|c| c.http().clone()).unwrap_or_default();
        let wake_mpv = mpv.clone();
        let feed = Arc::new(LiveFeed::start(http, pb.variants.clone(), idx, Arc::new(move || wake_mpv.wakeup())));
        if let Ok(mut c) = current.lock() {
            *c = Some(feed.clone());
        }
        let names: Vec<String> = pb.variants.iter().map(|v| v.name.clone()).collect();
        self.set(|s| {
            s.variants = names;
            s.turbo = pb.turbo;
            s.ads_expected = pb.ads_expected;
            s.current = pb.variants.get(idx).map(|v| v.name.clone()).unwrap_or_default();
        });
        // The player opens the stream once the feeder knows its container
        // (after a pre-roll ad break, that is when live content appears).
        let mut loaded_epoch: Option<u64> = None;
        let mut last_load = Instant::now();

        let mut cache = 0.0f64;
        let mut speed = 1.0f64;
        let mut last_control = Instant::now();
        let mut refreshing_since: Option<Instant> = None;
        let mut last_sample: Option<(Instant, u64)> = None;
        // Bytes received over the last seconds: segments arrive in bursts,
        // so the download rate is averaged over a window (GK-5).
        let mut received: std::collections::VecDeque<(Instant, u64)> = std::collections::VecDeque::new();
        let mut busy = self.busy();
        // Test hook: GK_TEST_RESYNC_AT=<seconds> forces one live-edge resync.
        let mut test_resync = std::env::var("GK_TEST_RESYNC_AT")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .map(|s| Instant::now() + Duration::from_secs_f64(s));
        let mut fast = false;
        loop {
            // Commands first: they must act at once.
            loop {
                match self.rx.try_recv() {
                    Ok(Cmd::Stop) | Err(TryRecvError::Disconnected) => return Ok(Next::Stop),
                    Ok(Cmd::Reload) => return Ok(Next::Resolve),
                    Ok(Cmd::TileHeight(h)) => {
                        if h != tile_height {
                            pending_height = Some((h, Instant::now()));
                        }
                    }
                    Ok(Cmd::Quality(q)) => {
                        self.settings.quality = q;
                        let i = self.pick(&feed.variants(), tile_height);
                        feed.set_variant(i);
                    }
                    Ok(other) => self.apply_setting(mpv, other),
                    Err(TryRecvError::Empty) => break,
                }
            }
            if self.stop.load(Ordering::Relaxed) {
                return Ok(Next::Stop);
            }
            // The video moved to another GPU: start over at the next segment
            // boundary (a keyframe) so the new decoder starts cleanly.
            if loaded_epoch.is_some() && std::mem::take(&mut self.reopen) {
                feed.resync();
            }
            // Auto quality follows the tile once its size settled.
            if let Some((h, since)) = pending_height
                && since.elapsed() > Duration::from_millis(400)
            {
                tile_height = h;
                self.tile_height = h;
                pending_height = None;
                if self.settings.quality == Quality::Auto {
                    let i = self.pick(&feed.variants(), tile_height);
                    feed.set_variant(i);
                }
            }
            let mut ended = false;
            while let Some(ev) = mpv.wait_event(0.1) {
                match ev {
                    Event::PropertyChange { id: P_CACHE, value, .. } => {
                        cache = if let Value::Double(d) = value { d } else { 0.0 };
                    }
                    Event::PropertyChange { id: P_DROPS, value: Value::Int64(d), .. } => self.set(|s| s.drops = d),
                    Event::PropertyChange { id: P_HEIGHT, value, .. } => {
                        let h = if let Value::Int64(h) = value { h } else { 0 };
                        self.set(|s| {
                            s.video_height = h;
                            if h > 0 {
                                s.state = State::Playing;
                            }
                        });
                    }
                    Event::PropertyChange { id: P_HWDEC, value, .. } => {
                        let v = if let Value::String(s) = value { s } else { String::new() };
                        self.set(|s| s.hwdec = v);
                    }
                    // Our own reloads end the previous file with `Stop`.
                    Event::EndFile { reason: EndReason::Eof | EndReason::Error, .. }
                        if loaded_epoch.is_some() && last_load.elapsed() > Duration::from_millis(1500) =>
                    {
                        ended = true
                    }
                    Event::Shutdown => return Ok(Next::Stop),
                    Event::Log { prefix, level, text } => {
                        log::warn!("stream {} mpv[{prefix}] {level}: {}", self.channel, text.trim_end())
                    }
                    _ => {}
                }
            }
            let st = feed.status();
            if let Some(fmt) = st.container.demuxer()
                && loaded_epoch != Some(st.epoch)
            {
                let _ = mpv.command(&["set", "demuxer-lavf-format", fmt]);
                self.restore_video(mpv);
                if let Err(e) = mpv.command(&["loadfile", &format!("gkfeed://{}", self.id)]) {
                    self.set(|s| s.state = State::Error(e.to_string()));
                    return Err(Duration::from_secs(5));
                }
                loaded_epoch = Some(st.epoch);
                last_load = Instant::now();
            }
            if loaded_epoch.is_none() {
                let in_ad = st.in_ad;
                self.set(|s| s.in_ad = in_ad);
            }
            if ended || st.ended {
                if let Ok(mut c) = current.lock() {
                    *c = None;
                }
                let _ = mpv.command(&["stop"]);
                // Offline now, or a hiccup? The next resolve decides.
                return Err(Duration::from_secs(2));
            }
            // A token expires after a while: fetch a fresh playlist and keep
            // the pipe going.
            if st.needs_refresh && refreshing_since.is_none_or(|t| t.elapsed() > Duration::from_secs(10)) {
                refreshing_since = Some(Instant::now());
                match self.playback() {
                    Ok(p) => feed.replace_variants(p.variants),
                    Err(TwError::Offline) | Err(TwError::NotFound) => {
                        if let Ok(mut c) = current.lock() {
                            *c = None;
                        }
                        let _ = mpv.command(&["stop"]);
                        self.set(|s| s.state = State::Offline);
                        return Err(Duration::from_secs(30));
                    }
                    Err(e) => log::warn!("stream {}: refresh: {e}", self.id),
                }
            }
            // Latency control, four times a second.
            if last_control.elapsed() >= Duration::from_millis(250) {
                last_control = Instant::now();
                let behind = cache + feed.buffered_secs();
                let target = self.settings.latency.target();
                let want = if behind > target + 6.0 {
                    // Far behind (a stall): jump to the live edge.
                    // Cleanly, at a segment boundary: the player reopens on
                    // the new epoch (see above).
                    feed.resync();
                    1.0
                } else if behind > target + 1.5 {
                    1.25
                } else if behind > target + 0.35 {
                    1.08
                } else if behind < target * 0.5 {
                    // Running dry: build a small cushion against stutter.
                    0.98
                } else {
                    1.0
                };
                if test_resync.is_some_and(|t| Instant::now() >= t) {
                    test_resync = None;
                    log::warn!("test: resync to the live edge");
                    feed.resync();
                }
                // Decoder load changed: Auto quality may keep 60 fps again,
                // or has to size by tile height alone.
                let now_busy = self.busy();
                if now_busy != busy {
                    busy = now_busy;
                    if self.settings.quality == Quality::Auto {
                        feed.set_variant(self.pick(&feed.variants(), tile_height));
                    }
                }
                let now_fast = MANY_TILES.load(Ordering::Relaxed);
                if now_fast != fast {
                    fast = now_fast;
                    let _ = if fast {
                        mpv.command(&["apply-profile", "fast"])
                    } else {
                        mpv.command(&["apply-profile", "fast", "restore"])
                    };
                }
                if (want - speed).abs() > 1e-6 {
                    speed = want;
                    let _ = mpv.command(&["set", "speed", &format!("{speed}")]);
                }
                let variants = feed.variants();
                let cur = variants.get(st.variant).map(|v| v.name.clone()).unwrap_or_default();
                let tenths = (behind * 10.0).round().clamp(0.0, 999.0) as u32;
                if STATS_ON.load(Ordering::Relaxed)
                    && last_sample.is_none_or(|(t, _)| t.elapsed() >= Duration::from_millis(500))
                {
                    let now = Instant::now();
                    let bytes = feed.bytes_received();
                    received.push_back((now, bytes));
                    while received.len() > 2 && received[1].0.elapsed() >= RATE_WINDOW {
                        received.pop_front();
                    }
                    let download_kbps = download_rate(&received, now);
                    last_sample = Some((now, bytes));
                    let d = |n: &str| mpv.get_double(n).unwrap_or(0.0);
                    let i = |n: &str| mpv.get_i64(n).unwrap_or(0);
                    let snap = StreamStats {
                        sampled: Some(now),
                        decoded_fps: d("estimated-vf-fps"),
                        // Low-latency probing often leaves the container rate
                        // unknown; the playlist states it.
                        source_fps: Some(d("container-fps"))
                            .filter(|f| *f > 0.0)
                            .unwrap_or_else(|| variants.get(st.variant).map(|v| f64::from(v.fps)).unwrap_or(0.0)),
                        drops_vo: i("frame-drop-count"),
                        drops_decoder: i("decoder-frame-drop-count"),
                        delayed: i("vo-delayed-frame-count"),
                        mistimed: i("mistimed-frame-count"),
                        width: i("video-params/w"),
                        height: i("video-params/h"),
                        codec: mpv.get_string("video-format").unwrap_or_default(),
                        hwdec: mpv.get_string("hwdec-current").unwrap_or_default(),
                        video_kbps: d("video-bitrate") / 1000.0,
                        audio_kbps: d("audio-bitrate") / 1000.0,
                        download_kbps,
                        cache_s: cache,
                        pipe_s: feed.buffered_secs(),
                        speed,
                        container: format!("{:?}", st.container),
                        segments_done: st.segments_done,
                        segments_skipped: st.segments_skipped,
                        reopens: st.epoch,
                    };
                    if let Ok(mut s) = self.stats.lock() {
                        *s = snap;
                    }
                }
                self.set(|s| {
                    s.buffer_tenths = tenths;
                    s.speed_up = speed > 1.0;
                    s.in_ad = st.in_ad;
                    s.low_latency = st.low_latency;
                    s.current = cur;
                });
            }
        }
    }
}

#[cfg(test)]
mod rate_tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn download_rate_spans_bursts() {
        let t0 = Instant::now();
        // 1 MB arrives at once, then nothing for 3.5 s: still about 2 Mbit/s
        // over the 4 s window, not 0.
        let samples: VecDeque<(Instant, u64)> = VecDeque::from([
            (t0, 0),
            (t0 + Duration::from_millis(500), 1_000_000),
            (t0 + Duration::from_secs(4), 1_000_000),
        ]);
        let r = download_rate(&samples, t0 + Duration::from_secs(4));
        assert!((r - 2000.0).abs() < 1.0, "{r}");
        // Less than a second of history: no number yet.
        assert_eq!(download_rate(&VecDeque::from([(t0, 0)]), t0 + Duration::from_millis(500)), 0.0);
    }
}
