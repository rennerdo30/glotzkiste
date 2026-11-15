//! One live stream: its control thread owns the low-latency feeder and the
//! mpv player, drains mpv events, keeps latency at the target, recovers from
//! expired tokens and offline phases, and reports a compact status to the UI
//! only when something visible changed. The UI thread never waits for it.

use gk_mpv::{EndReason, Event, Format, Libmpv, Mpv, StreamReader, Value};
use gk_render::Presenter;
use gk_twitch::live::{FeedReader, LiveFeed};
use gk_twitch::playlist::{Variant, pick_for_height};
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
    /// A second, NVIDIA GPU (copied back; mpv can target only NVIDIA cards).
    Nvidia,
    /// The CPU (when the GPU's decoder is full).
    Cpu,
}

impl DecodeOn {
    fn hwdec(self) -> &'static str {
        match self {
            DecodeOn::Gpu => "d3d11va",
            DecodeOn::Nvidia => "nvdec-copy",
            DecodeOn::Cpu => "no",
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
    /// Moved off the main GPU's decoder because it was full.
    pub offloaded: Option<DecodeOn>,
}

/// True while the stats window is open: stream threads then sample mpv
/// twice a second. Closed, the stats cost nothing.
pub static STATS_ON: AtomicBool = AtomicBool::new(false);

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
}

const P_CACHE: u64 = 1;
const P_DROPS: u64 = 2;
const P_HEIGHT: u64 = 3;
const P_HWDEC: u64 = 4;

enum Next {
    Resolve,
    Stop,
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
        let only_buffer = Status { buffer_tenths: self.posted.buffer_tenths, drops: self.posted.drops, ..now.clone() } == self.posted;
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
            ("volume", &volume),
        ];
        for (k, v) in opts {
            if let Err(e) = mpv.set_option(k, v) {
                log::warn!("stream {}: option {k}={v}: {e}", self.id);
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
        self.env.presenter.add_stream(self.id, mpv.clone());

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
            }
            Cmd::Visible(v) => {
                let _ = mpv.command(&["set", "vid", if v { "auto" } else { "no" }]);
            }
            Cmd::Decoder(d) => {
                // mpv re-creates the decoder at the next frame.
                let _ = mpv.command(&["set", "hwdec", d.hwdec()]);
                self.set(|s| s.offloaded = (d != DecodeOn::Gpu).then_some(d));
            }
            Cmd::Latency(l) => self.settings.latency = l,
            Cmd::Quality(q) => self.settings.quality = q,
            Cmd::TileHeight(h) => self.tile_height = h,
            Cmd::Reload | Cmd::Stop => {}
        }
    }

    fn pick(&self, variants: &[Variant], tile_height: u32) -> usize {
        match &self.settings.quality {
            Quality::Named(n) => variants.iter().position(|v| &v.name == n),
            Quality::Auto => pick_for_height(variants, tile_height.max(1)).and_then(|p| variants.iter().position(|v| v.name == p.name)),
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
                    Event::Log { prefix, level, text } => log::warn!("stream {} mpv[{prefix}] {level}: {}", self.channel, text.trim_end()),
                    _ => {}
                }
            }
            let st = feed.status();
            if let Some(fmt) = st.container.demuxer()
                && loaded_epoch != Some(st.epoch)
            {
                let _ = mpv.command(&["set", "demuxer-lavf-format", fmt]);
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
                    feed.drop_buffered();
                    let _ = mpv.command(&["loadfile", &format!("gkfeed://{}", self.id)]);
                    last_load = Instant::now();
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
                if (want - speed).abs() > 1e-6 {
                    speed = want;
                    let _ = mpv.command(&["set", "speed", &format!("{speed}")]);
                }
                let variants = feed.variants();
                let cur = variants.get(st.variant).map(|v| v.name.clone()).unwrap_or_default();
                let tenths = (behind * 10.0).round().clamp(0.0, 999.0) as u32;
                if STATS_ON.load(Ordering::Relaxed) && last_sample.is_none_or(|(t, _)| t.elapsed() >= Duration::from_millis(500)) {
                    let now = Instant::now();
                    let bytes = feed.bytes_received();
                    let download_kbps = match last_sample {
                        Some((t, b)) => (bytes.saturating_sub(b)) as f64 * 8.0 / 1000.0 / now.duration_since(t).as_secs_f64().max(0.001),
                        None => 0.0,
                    };
                    last_sample = Some((now, bytes));
                    let d = |n: &str| mpv.get_double(n).unwrap_or(0.0);
                    let i = |n: &str| mpv.get_i64(n).unwrap_or(0);
                    let snap = StreamStats {
                        sampled: Some(now),
                        decoded_fps: d("estimated-vf-fps"),
                        // Low-latency probing often leaves the container rate
                        // unknown; the playlist states it.
                        source_fps: Some(d("container-fps")).filter(|f| *f > 0.0).unwrap_or_else(|| {
                            variants.get(st.variant).map(|v| f64::from(v.fps)).unwrap_or(0.0)
                        }),
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
