//! The window, the event loop and the model. Drawing lives in `ui`.

use crate::Options;
use crate::chat_state::ChatState;
use crate::images::Images;
use crate::layout::{Pane, Preset, Tab, WindowGeom};
use crate::stream::{Cmd, Env, Latency, Settings, StreamHandle};
use crate::workers::Pool;
use gk_render::{Presenter, UiFrame, VideoTile};
use gk_twitch::{Auth, ChannelInfo, Client, Http};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowId};

#[derive(Debug, Clone)]
pub enum UserEvent {
    /// Something changed off the UI thread; run an egui pass.
    Wake,
    /// libmpv is loaded (or failed).
    LibReady,
}

type LibSlot = Arc<Mutex<Option<Result<Arc<gk_mpv::Libmpv>, String>>>>;
type Slot<T> = Arc<Mutex<Option<T>>>;

#[derive(Default)]
pub struct UpdateState {
    pub found: Option<gk_update::Candidate>,
    pub checking: bool,
    pub installing: bool,
    /// The last result shown in the About window (ok?, text).
    pub message: Option<(bool, String)>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum LoginMode {
    #[default]
    Extension,
    DevTools,
}

#[derive(Default)]
pub struct LoginDialog {
    pub open: bool,
    pub paste: String,
    pub busy: bool,
    pub message: Option<(bool, String)>,
    pub mode: LoginMode,
    /// Installed browsers (default first), once detected.
    pub browsers: Option<Vec<crate::browsers::Browser>>,
    pub browser: usize,
    /// A cookies file the login came from: it holds the whole browser
    /// session, so the dialog offers to delete it.
    pub imported_file: Option<std::path::PathBuf>,
}

/// The stats in their own OS window (UI only: its own swap chain, no video).
struct StatsWindow {
    window: Arc<Window>,
    state: egui_winit::State,
    ctx: egui::Context,
    presenter: Presenter,
    needs_pass: bool,
    last_pass: Instant,
}

/// Moves streams off a full GPU video decoder.
#[derive(Default)]
struct Valve {
    /// Samples in a row with the decoder at 90 % or more, per GPU.
    hot: HashMap<(u32, i32), u32>,
    /// Dropped frames of the streams on each GPU at the last sample.
    drops: HashMap<(u32, i32), i64>,
    last_move: Option<Instant>,
    last_cpu: Option<Instant>,
    checked: Option<Instant>,
    /// Samples in a row a stream on another GPU decoded in software.
    no_hw: HashMap<u64, u32>,
    /// Test hook (`GK_TEST_MOVE=<seconds>`): then the first playing stream
    /// moves to the next GPU.
    test_at: Option<Instant>,
}

/// The "random streams" dialog.
pub(crate) struct RandomDialog {
    pub open: bool,
    pub count: u32,
    pub category: String,
    /// Index into `ui::LANGUAGES` (0: any).
    pub language: usize,
    pub busy: bool,
    pub message: Option<(bool, String)>,
    pub categories: Vec<String>,
    categories_asked: bool,
}

impl Default for RandomDialog {
    fn default() -> Self {
        Self {
            open: false,
            count: 10,
            category: String::new(),
            language: 0,
            busy: false,
            message: None,
            categories: Vec::new(),
            categories_asked: false,
        }
    }
}

pub struct App {
    pub(crate) options: Options,
    proxy: EventLoopProxy<UserEvent>,
    window: Option<Arc<Window>>,
    egui_state: Option<egui_winit::State>,
    pub(crate) ctx: egui::Context,
    pub(crate) presenter: Option<Arc<Presenter>>,
    lib: LibSlot,
    pub(crate) pool: Pool,
    pub(crate) client: Arc<Mutex<Client>>,
    pub(crate) auth: Option<Auth>,
    pub(crate) streams: HashMap<u64, StreamHandle>,
    tile_heights: HashMap<u64, u32>,
    visible: HashMap<u64, bool>,
    muted: HashMap<u64, bool>,
    pub(crate) tabs: Vec<Tab>,
    pub(crate) active: usize,
    pub(crate) maximized: Option<egui_tiles::TileId>,
    next_id: u64,
    pub(crate) latency: Latency,
    pub(crate) chat: ChatState,
    pub(crate) images: Images,
    pub(crate) login: LoginDialog,
    login_result: Slot<(Result<Auth, String>, Option<std::path::PathBuf>)>,
    browsers_slot: Slot<Vec<crate::browsers::Browser>>,
    pub(crate) add_text: String,
    pub(crate) followed: Vec<ChannelInfo>,
    followed_slot: Slot<Result<Vec<ChannelInfo>, String>>,
    pub(crate) followed_loading: bool,
    followed_at: Option<Instant>,
    /// The viewer's settings (GK-15) and their window.
    pub(crate) settings: crate::settings::Settings,
    pub(crate) settings_open: bool,
    /// The "random streams" dialog (GK-12).
    pub(crate) random: RandomDialog,
    random_slot: Slot<Result<Vec<String>, String>>,
    categories_slot: Slot<Vec<String>>,
    pub(crate) info: HashMap<String, ChannelInfo>,
    info_slot: Slot<Vec<ChannelInfo>>,
    info_at: Option<Instant>,
    fonts_slot: Slot<egui::FontDefinitions>,
    pub(crate) renaming: Option<usize>,
    /// Scroll position and input of each chat pane.
    pub(crate) chat_views: HashMap<egui_tiles::TileId, crate::ui::chat::ChatView>,
    pub(crate) stats_open: bool,
    pub(crate) about_open: bool,
    pub(crate) update: UpdateState,
    update_slot: Slot<(bool, Result<crate::update::Outcome, String>)>,
    install_slot: Slot<Result<(), String>>,
    quit: bool,
    pub(crate) logo: Option<egui::TextureHandle>,
    pub(crate) stats_on_tiles: bool,
    pub(crate) sampler: crate::ui::stats::Sampler,
    pub(crate) tile_rects: Vec<crate::ui::stats::TileRect>,
    pub(crate) content_rect: egui::Rect,
    pub(crate) adapter: String,
    render_luid: (u32, i32),
    gpus: crate::gpu::Shared,
    stats_win: Option<StatsWindow>,
    stats_ui: crate::ui::stats::Ui,
    font_defs: Option<egui::FontDefinitions>,
    valve: Valve,
    /// Which GPU decodes which stream (placement.rs).
    pub(crate) placer: Arc<Mutex<crate::placement::Placer>>,
    /// Streams whose GPU the viewer chose by hand: never moved automatically.
    pub(crate) pinned: std::collections::HashSet<u64>,
    pub(crate) error: Option<String>,
    saved_json: String,
    save_check: Instant,
    persist: bool,
    /// Something that may change the saved state happened since the last check.
    save_dirty: bool,
    geom: Option<WindowGeom>,
    needs_pass: bool,
    repaint_at: Option<Instant>,
    smoke_until: Option<Instant>,
    started: Instant,
    passes: u64,
    pass_us: u64,
    last_pass: Instant,
    /// At most one UI pass per display refresh.
    pass_interval: Duration,
    exit_code: i32,
}

pub fn run(options: Options) -> i32 {
    let event_loop = match EventLoop::<UserEvent>::with_user_event().build() {
        Ok(e) => e,
        Err(e) => {
            log::error!("event loop: {e}");
            return 1;
        }
    };
    let proxy = event_loop.create_proxy();
    let mut app = App::new(options, proxy);
    if let Err(e) = event_loop.run_app(&mut app) {
        log::error!("event loop: {e}");
        return 1;
    }
    app.save_now();
    app.streams.clear();
    app.exit_code
}

/// Checks the token and attaches the browser identity (user agent from the
/// browser's version, device id from its cookie). Runs on a worker.
fn finish_login(
    http: &Http,
    found: gk_twitch::auth::LoginInput,
    browser: Option<&crate::browsers::Browser>,
) -> Result<Auth, String> {
    let mut a = gk_twitch::auth::validate(http, &found.token).map_err(|e| e.to_string())?;
    a.device_id = found.device_id;
    a.user_agent = browser.and_then(crate::browsers::user_agent).map(|(ua, _)| ua);
    Ok(a)
}

/// "Chrome 142" from a user agent we built.
pub fn ua_label(ua: &str) -> Option<String> {
    let find = |key: &str| ua.split(key).nth(1).map(|r| r.split('.').next().unwrap_or_default().to_owned());
    if let Some(v) = find("Edg/") {
        Some(format!("Edge {v}"))
    } else if let Some(v) = find("Firefox/") {
        Some(format!("Firefox {v}"))
    } else {
        find("Chrome/").map(|v| format!("Chrome {v}"))
    }
}

fn wake_fn(proxy: &EventLoopProxy<UserEvent>) -> Arc<dyn Fn() + Send + Sync> {
    let proxy = proxy.clone();
    Arc::new(move || {
        let _ = proxy.send_event(UserEvent::Wake);
    })
}

impl App {
    fn new(options: Options, proxy: EventLoopProxy<UserEvent>) -> Self {
        // libmpv loads (and on first run unpacks) while the window opens.
        let lib: LibSlot = Arc::new(Mutex::new(None));
        {
            let (lib, proxy) = (lib.clone(), proxy.clone());
            let _ = std::thread::Builder::new().name("gk-libmpv".into()).spawn(move || {
                let r = crate::libmpv::load();
                if let Ok(mut slot) = lib.lock() {
                    *slot = Some(r);
                }
                let _ = proxy.send_event(UserEvent::LibReady);
            });
        }
        let pool = Pool::new(4);
        let wake = wake_fn(&proxy);
        let fonts_slot: Slot<egui::FontDefinitions> = Arc::default();
        {
            let (slot, wake) = (fonts_slot.clone(), wake.clone());
            pool.spawn(move || {
                if let Some(defs) = crate::fonts::load() {
                    if let Ok(mut s) = slot.lock() {
                        *s = Some(defs);
                    }
                    wake();
                }
            });
        }
        let smoke = options.smoke.is_some();
        // Smoke runs never use the viewer's account or saved layout.
        let auth = if smoke { None } else { gk_twitch::auth::store::load() };
        let http = auth.as_ref().and_then(|a| a.user_agent.as_deref()).map(Http::with_user_agent).unwrap_or_default();
        let client = Arc::new(Mutex::new(Client::new(http.clone(), auth.clone())));
        let creds = auth.as_ref().map(|a| gk_chat::Credentials { login: a.login.clone(), token: a.token.clone() });
        let chat = ChatState::new(creds, pool.clone(), client.clone(), wake.clone());
        let images = Images::new(pool.clone(), http, wake);

        // The saved state: always for the viewer; for smoke runs only from
        // their own folder (they never touch the viewer's state).
        if let Some(dir) = &options.state_dir {
            crate::layout::set_dir(dir.clone());
        }
        let persist = (!smoke && !options.demo) || options.state_dir.is_some();
        let (mut tabs, mut active, mut latency, mut geom) = (Vec::new(), 0, Latency::Ultra, None);
        if persist && let Some(saved) = crate::layout::load() {
            tabs = saved.tabs;
            active = saved.active;
            latency = saved.latency;
            geom = saved.window;
        }
        if !options.channels.is_empty() {
            let panes = options.channels.iter().map(|c| Pane::stream(c)).collect();
            let title = if tabs.is_empty() { "Main".to_owned() } else { format!("Tab {}", tabs.len() + 1) };
            let mut tab = Tab::new(&title, panes);
            if options.chat || options.chat_all {
                let first = options.channels[0].clone();
                tab.apply(Preset::Grid, Some(first));
            }
            if options.chat_all {
                for t in tab.tree.tiles.tiles_mut() {
                    if let egui_tiles::Tile::Pane(Pane::Chat { mode, .. }) = t {
                        *mode = crate::layout::ChatMode::AllStreams;
                    }
                }
            }
            tabs.push(tab);
            active = tabs.len() - 1;
        }
        if options.demo {
            // Generated streams and a combined sample chat (README screenshots).
            let panes = ["~aurora", "~fractal", "~life", "~signal"].iter().map(|c| Pane::stream(c)).collect();
            let mut tab = Tab::new("Demo", panes);
            tab.apply(Preset::Spotlight, Some("~aurora".into()));
            for t in tab.tree.tiles.tiles_mut() {
                if let egui_tiles::Tile::Pane(Pane::Chat { mode, .. }) = t {
                    *mode = crate::layout::ChatMode::AllStreams;
                }
            }
            tabs = vec![tab, Tab::new("Esports", vec![]), Tab::new("Chill", vec![])];
            active = 0;
        }
        if tabs.is_empty() {
            tabs.push(Tab::new("Main", vec![]));
        }
        active = active.min(tabs.len() - 1);
        let ctx = egui::Context::default();
        crate::ui::style(&ctx);
        let stats_open = options.stats;
        let settings = crate::settings::Settings::load();
        settings.apply();
        let mut app = Self {
            smoke_until: options.smoke.map(|s| Instant::now() + Duration::from_secs_f64(s)),
            options,
            proxy,
            window: None,
            egui_state: None,
            ctx,
            presenter: None,
            lib,
            pool,
            client,
            auth,
            streams: HashMap::new(),
            tile_heights: HashMap::new(),
            visible: HashMap::new(),
            muted: HashMap::new(),
            tabs,
            active,
            maximized: None,
            next_id: 1,
            latency,
            chat,
            images,
            login: LoginDialog::default(),
            login_result: Arc::default(),
            browsers_slot: Arc::default(),
            add_text: String::new(),
            followed: Vec::new(),
            followed_slot: Arc::default(),
            followed_loading: false,
            followed_at: None,
            settings,
            settings_open: false,
            random: RandomDialog::default(),
            random_slot: Arc::default(),
            categories_slot: Arc::default(),
            info: HashMap::new(),
            info_slot: Arc::default(),
            info_at: None,
            fonts_slot,
            renaming: None,
            chat_views: HashMap::new(),
            stats_open: false,
            about_open: false,
            update: UpdateState::default(),
            update_slot: Arc::default(),
            install_slot: Arc::default(),
            quit: false,
            logo: None,
            stats_on_tiles: false,
            sampler: Default::default(),
            tile_rects: Vec::new(),
            content_rect: egui::Rect::NOTHING,
            adapter: String::new(),
            render_luid: (0, 0),
            gpus: crate::gpu::start(),
            stats_win: None,
            stats_ui: Default::default(),
            font_defs: None,
            placer: Arc::new(Mutex::new(crate::placement::Placer::default())),
            pinned: Default::default(),
            valve: Valve {
                checked: Some(Instant::now()),
                test_at: std::env::var("GK_TEST_MOVE")
                    .ok()
                    .and_then(|v| v.parse::<f64>().ok())
                    .map(|s| Instant::now() + Duration::from_secs_f64(s.max(0.0))),
                ..Default::default()
            },
            error: None,
            saved_json: String::new(),
            save_check: Instant::now(),
            persist,
            save_dirty: false,
            geom,
            needs_pass: true,
            repaint_at: None,
            started: Instant::now(),
            passes: 0,
            pass_us: 0,
            last_pass: Instant::now() - Duration::from_secs(1),
            pass_interval: Duration::from_micros(16_666),
            exit_code: 0,
        };
        app.stats_open = stats_open;
        app.login.open = app.options.login;
        if let Some(n) = app.options.random {
            app.tabs.push(Tab::new(&format!("Random {n}"), vec![]));
            app.active = app.tabs.len() - 1;
            app.random.count = n;
            app.random.category = app.options.random_category.clone().unwrap_or_default();
            let lang = app.options.random_language.clone();
            app.random.language =
                crate::ui::LANGUAGES.iter().position(|(_, c)| c.is_some() && *c == lang.as_deref()).unwrap_or(0);
            app.add_random();
        }
        app
    }

    fn lib(&self) -> Option<Arc<gk_mpv::Libmpv>> {
        self.lib.lock().ok()?.as_ref()?.as_ref().ok().cloned()
    }

    pub(crate) fn wake(&self) -> Arc<dyn Fn() + Send + Sync> {
        wake_fn(&self.proxy)
    }

    /// Starts players for new stream panes, stops removed ones, pauses video
    /// decoding outside the active tab, and moves the sound to the focused
    /// stream of the active tab.
    fn sync_streams(&mut self) {
        let (Some(lib), Some(presenter)) = (self.lib(), self.presenter.clone()) else { return };
        let mut wanted: HashMap<u64, (bool, bool)> = HashMap::new(); // id -> (visible, audible)
        let mut chats: HashSet<String> = HashSet::new();
        for (ti, tab) in self.tabs.iter_mut().enumerate() {
            let active = ti == self.active;
            if tab.audio.is_none() {
                tab.audio = tab.stream_channels().into_iter().next();
            }
            let audio = tab.audio.clone();
            let tab_streams = tab.stream_channels();
            for tile in tab.tree.tiles.tiles_mut() {
                match tile {
                    egui_tiles::Tile::Pane(Pane::Stream(p)) => {
                        if p.id == 0 || !self.streams.contains_key(&p.id) {
                            let id = self.next_id;
                            self.next_id += 1;
                            p.id = id;
                            let env = Env {
                                lib: lib.clone(),
                                client: self.client.clone(),
                                presenter: presenter.clone(),
                                notify: wake_fn(&self.proxy),
                                muted_by_flag: self.options.mute,
                                placer: self.placer.clone(),
                            };
                            let audible = active && audio.as_deref() == Some(p.channel.as_str());
                            let settings = Settings {
                                quality: p.quality.clone(),
                                volume: p.volume,
                                muted: !audible,
                                latency: self.latency,
                            };
                            self.streams.insert(id, StreamHandle::spawn(id, p.channel.clone(), env, settings));
                            self.muted.insert(id, !audible);
                        }
                        let e = wanted.entry(p.id).or_default();
                        e.0 |= active;
                        e.1 |= active && audio.as_deref() == Some(p.channel.as_str());
                    }
                    egui_tiles::Tile::Pane(Pane::Chat { channel, mode }) => {
                        chats.extend(crate::layout::chat_channels(channel, mode, &tab_streams));
                    }
                    _ => {}
                }
            }
        }
        self.streams.retain(|id, _| wanted.contains_key(id));
        self.tile_heights.retain(|id, _| wanted.contains_key(id));
        self.visible.retain(|id, _| wanted.contains_key(id));
        self.muted.retain(|id, _| wanted.contains_key(id));
        for (id, (vis, audible)) in wanted {
            let Some(s) = self.streams.get(&id) else { continue };
            if self.visible.get(&id) != Some(&vis) {
                self.visible.insert(id, vis);
                s.send(Cmd::Visible(vis));
            }
            if self.muted.get(&id) != Some(&!audible) {
                self.muted.insert(id, !audible);
                s.send(Cmd::Mute(!audible));
            }
        }
        self.chat.sync_channels(&chats);
    }

    pub(crate) fn set_latency(&mut self, l: Latency) {
        self.latency = l;
        for s in self.streams.values() {
            s.send(Cmd::Latency(l));
        }
    }

    pub(crate) fn add_channel(&mut self, raw: &str) {
        let login =
            raw.trim().trim_start_matches("https://").trim_start_matches("www.").trim_start_matches("twitch.tv/");
        let login = login.split(['/', '?']).next().unwrap_or_default().to_ascii_lowercase();
        if !gk_twitch::api::valid_login(&login) {
            return;
        }
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.add_stream(&login);
            if tab.audio.is_none() {
                tab.audio = Some(login);
            }
        }
        self.maximized = None;
    }

    /// The browser picked in the login dialog (else the default one).
    fn login_browser(&self) -> Option<crate::browsers::Browser> {
        self.login.browsers.as_ref().and_then(|b| b.get(self.login.browser).or_else(|| b.first()).cloned())
    }

    pub(crate) fn start_login(&mut self, input: String) {
        let Some(found) = gk_twitch::auth::parse_login_input(&input) else {
            self.login.message = Some((
                false,
                "No Twitch login in that. Paste the auth-token value, or what the extension copied.".into(),
            ));
            return;
        };
        self.login.busy = true;
        self.login.message = None;
        let browser = self.login_browser();
        let (slot, wake, http) =
            (self.login_result.clone(), self.wake(), self.client.lock().map(|c| c.http().clone()).unwrap_or_default());
        self.pool.spawn(move || {
            let r = finish_login(&http, found, browser.as_ref());
            if let Ok(a) = &r
                && let Err(e) = gk_twitch::auth::store::save(a)
            {
                log::warn!("could not store the login: {e}");
            }
            if let Ok(mut s) = slot.lock() {
                *s = Some((r, None));
            }
            wake();
        });
    }

    /// Opens or closes the stats window to match `stats_open`.
    fn sync_stats_window(&mut self, event_loop: &ActiveEventLoop) {
        if !self.stats_open {
            self.stats_win = None;
            return;
        }
        if self.stats_win.is_some() {
            return;
        }
        use winit::platform::windows::WindowAttributesExtWindows;
        let mut attrs = Window::default_attributes()
            .with_title("Glotzkiste stats")
            .with_min_inner_size(LogicalSize::new(560.0, 320.0));
        {
            use winit::platform::windows::IconExtWindows;
            let small = winit::window::Icon::from_resource(1, Some(PhysicalSize::new(32, 32))).ok();
            let big = winit::window::Icon::from_resource(1, Some(PhysicalSize::new(256, 256))).ok();
            attrs = attrs.with_window_icon(small).with_taskbar_icon(big);
        }
        attrs = if self.smoke_until.is_some() {
            attrs
                .with_inner_size(PhysicalSize::new(1400, 800))
                .with_position(PhysicalPosition::new(-20000, -21000))
                .with_skip_taskbar(true)
                .with_active(false)
        } else {
            attrs.with_inner_size(LogicalSize::new(1100.0, 700.0))
        };
        let Ok(window) = event_loop.create_window(attrs).map(Arc::new) else {
            log::error!("cannot open the stats window");
            self.stats_open = false;
            return;
        };
        let Ok(RawWindowHandle::Win32(h)) = window.window_handle().map(|h| h.as_raw()) else {
            self.stats_open = false;
            return;
        };
        let size = window.inner_size();
        // The stats window shows no streams: no render workers.
        let presenter = match Presenter::start_with(h.hwnd.get(), (size.width, size.height), false) {
            Ok((p, _)) => p,
            Err(e) => {
                log::error!("stats window: {e}");
                self.stats_open = false;
                return;
            }
        };
        presenter.set_clear_color(crate::ui::BG_LINEAR);
        let ctx = egui::Context::default();
        crate::ui::style(&ctx);
        if let Some(defs) = &self.font_defs {
            ctx.set_fonts(defs.clone());
        }
        let state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            None,
            Some(8192),
        );
        self.stats_win = Some(StatsWindow {
            window,
            state,
            ctx,
            presenter,
            needs_pass: true,
            last_pass: Instant::now() - Duration::from_secs(1),
        });
    }

    /// One pass of the stats window (twice a second, and on input).
    /// Updates the stats rates (at most once a second).
    fn sample_stats(&mut self, p: &Presenter) {
        let drops = self
            .streams
            .iter()
            .map(|(id, s)| {
                let st = s.stats();
                (*id, (st.drops_vo, st.drops_decoder))
            })
            .collect();
        let gpus = self.gpus.lock().map(|g| g.clone()).unwrap_or_default();
        self.sampler.sample(&p.stats(), self.passes, self.pass_us, drops, &gpus, self.render_luid);
    }

    fn stats_pass(&mut self) {
        let Some(w) = self.stats_win.as_mut() else { return };
        w.needs_pass = false;
        w.last_pass = Instant::now();
        if let Some(p) = self.presenter.clone() {
            self.sample_stats(&p);
        }
        let Some(w) = self.stats_win.as_mut() else { return };
        let raw = w.state.take_egui_input(&w.window);
        let ids: Vec<u64> = self.streams.keys().copied().collect();
        let gpus = self.gpus.lock().map(|g| g.clone()).unwrap_or_default();
        let c = crate::ui::stats::Ctx {
            rects: &self.tile_rects,
            statuses: ids.iter().filter_map(|id| Some((*id, self.streams.get(id)?.status()))).collect(),
            stats: ids.iter().filter_map(|id| Some((*id, self.streams.get(id)?.stats()))).collect(),
            chat_lines: self.chat.channels.iter().map(|(k, v)| (k.clone(), v.buffer.len())).collect(),
            rates: &self.sampler.rates,
            adapter: &self.adapter,
            gpus: &gpus,
            render_luid: self.render_luid,
            on_tiles: &mut self.stats_on_tiles,
            state: &mut self.stats_ui,
        };
        let mut c = Some(c);
        let full = w.ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default().frame(egui::Frame::NONE.fill(crate::ui::BG).inner_margin(10)).show(
                ui,
                |ui| {
                    if let Some(c) = c.take() {
                        crate::ui::stats::contents(ui, c);
                    }
                },
            );
        });
        w.state.handle_platform_output(&w.window, full.platform_output);
        let ppp = full.pixels_per_point;
        let primitives = w.ctx.tessellate(full.shapes, ppp);
        w.presenter.submit(UiFrame {
            primitives,
            textures_delta: full.textures_delta,
            pixels_per_point: ppp,
            videos: Vec::new(),
        });
        if full.viewport_output.get(&egui::ViewportId::ROOT).is_some_and(|v| v.repaint_delay.is_zero()) {
            w.needs_pass = true;
        }
        // The main window shows the numbers on the tiles.
        if self.stats_on_tiles {
            self.needs_pass = true;
        }
    }

    /// Every 2 s: gives the placer the decoders' load, and when a GPU's
    /// decoder stays full while its streams drop frames, moves the newest
    /// stream there to the next GPU with room (the main GPU last), else to
    /// the CPU.
    fn run_valve(&mut self) {
        use crate::stream::{DecodeOn, State, Status};
        if self.valve.checked.is_some_and(|c| c.elapsed() < Duration::from_secs(2)) {
            return;
        }
        self.valve.checked = Some(Instant::now());
        let gpus = self.gpus.lock().map(|g| g.clone()).unwrap_or_default();
        let visible = self.visible.values().filter(|v| **v).count();
        crate::stream::MANY_TILES.store(visible > 4, std::sync::atomic::Ordering::Relaxed);
        let main = self.render_luid;
        let gpu_of = |d: DecodeOn| match d {
            DecodeOn::Gpu => Some(main),
            DecodeOn::Second(l) => Some(l),
            DecodeOn::Cpu => None,
        };
        // Visible, playing streams (hidden ones decode no video).
        let mut playing: Vec<(u64, Status)> = self
            .streams
            .iter()
            .filter(|(id, _)| self.visible.get(id) == Some(&true))
            .map(|(id, s)| (*id, s.status()))
            .filter(|(_, st)| matches!(st.state, State::Playing) && !st.in_ad)
            .collect();
        playing.sort_by_key(|(id, _)| *id);
        self.pinned.retain(|id| self.streams.contains_key(id));
        let mut streams_on: HashMap<(u32, i32), u32> = HashMap::new();
        for (_, st) in &playing {
            if let Some(l) = gpu_of(st.decode_on) {
                *streams_on.entry(l).or_default() += 1;
            }
        }
        let placer = self.placer.clone();
        let mut placer = placer.lock().unwrap_or_else(|p| p.into_inner());
        placer.update(gpus.clone(), streams_on);

        // Software decoding on another GPU gains nothing: that GPU has no
        // hardware decoder for these streams. Back to the main GPU.
        for (id, st) in &playing {
            let demo = self.streams.get(id).is_some_and(|s| crate::stream::is_demo(&s.channel));
            match st.decode_on {
                DecodeOn::Second(l) if !demo && st.video_height > 0 && (st.hwdec.is_empty() || st.hwdec == "no") => {
                    let n = self.valve.no_hw.entry(*id).or_default();
                    *n += 1;
                    if *n >= 2 {
                        self.valve.no_hw.remove(id);
                        placer.mark_bad(l);
                        self.streams[id].send(Cmd::Decoder(DecodeOn::Gpu));
                    }
                }
                _ => {
                    self.valve.no_hw.remove(id);
                }
            }
        }

        // Test hook (GK_TEST_MOVE=<seconds>): the first playing stream moves
        // to the next GPU in placement order.
        if let Some(at) = self.valve.test_at
            && Instant::now() >= at
            && let Some((id, st)) = playing.first()
        {
            self.valve.test_at = None;
            let gpus_menu: Vec<DecodeOn> =
                placer.menu().into_iter().map(|(d, _)| d).filter(|d| *d != DecodeOn::Cpu).collect();
            let i = gpus_menu.iter().position(|d| *d == st.decode_on).map_or(0, |i| i + 1);
            match gpus_menu.get(i % gpus_menu.len().max(1)).copied().filter(|d| *d != st.decode_on) {
                Some(to) => {
                    log::warn!("test: stream {id} moves from {} to {}", placer.name(st.decode_on), placer.name(to));
                    self.streams[id].send(Cmd::Decoder(to));
                }
                None => log::warn!("test: only one GPU"),
            }
        }

        for g in &gpus {
            let h = self.valve.hot.entry(g.luid).or_default();
            *h = if g.decode_pct >= 90.0 { *h + 1 } else { 0 };
        }
        let mut drops: HashMap<(u32, i32), i64> = HashMap::new();
        for (_, st) in &playing {
            if let Some(l) = gpu_of(st.decode_on) {
                *drops.entry(l).or_default() += st.drops;
            }
        }
        let cooling = self.valve.last_move.is_some_and(|t| t.elapsed() < Duration::from_secs(4));
        let mut moved = cooling;
        for g in &gpus {
            let now = drops.get(&g.luid).copied().unwrap_or(0);
            let before = self.valve.drops.insert(g.luid, now).unwrap_or(now);
            if moved || self.valve.hot.get(&g.luid).copied().unwrap_or(0) < 2 || now <= before {
                continue;
            }
            let Some((id, _)) =
                playing.iter().rev().find(|(id, st)| gpu_of(st.decode_on) == Some(g.luid) && !self.pinned.contains(id))
            else {
                continue;
            };
            let target = placer.choose_except(Some(g.luid)).unwrap_or(DecodeOn::Cpu);
            if target == DecodeOn::Cpu && self.valve.last_cpu.is_some_and(|t| t.elapsed() < Duration::from_secs(10)) {
                continue;
            }
            log::warn!(
                "{}: video decoder at {:.0} % with dropped frames: stream {id} moves to {}",
                g.name,
                g.decode_pct,
                placer.name(target)
            );
            self.streams[id].send(Cmd::Decoder(target));
            self.valve.last_move = Some(Instant::now());
            if target == DecodeOn::Cpu {
                self.valve.last_cpu = Some(Instant::now());
            }
            self.valve.hot.insert(g.luid, 0);
            moved = true;
        }

        // Balancing (GK-2): streams go back toward the preferred GPUs. One
        // stream at a time (a move costs it about two seconds of picture):
        // from the CPU to any GPU with room, else from a GPU whose decoder is
        // at 60 % or more to an earlier GPU in order (the main GPU last)
        // with room. Streams placed by hand stay.
        let settled = self.valve.last_move.is_none_or(|t| t.elapsed() >= Duration::from_secs(6));
        if moved || !settled {
            return;
        }
        let load = |l: (u32, i32)| gpus.iter().find(|g| g.luid == l).map_or(0.0, |g| g.decode_pct);
        let mut candidates: Vec<&(u64, Status)> = playing.iter().filter(|(id, _)| !self.pinned.contains(id)).collect();
        // Worst placed first (CPU, then the main GPU, ...), newest first.
        candidates.sort_by_key(|(id, st)| (std::cmp::Reverse(placer.rank(st.decode_on)), std::cmp::Reverse(*id)));
        for (id, st) in candidates {
            let target = match st.decode_on {
                DecodeOn::Cpu => placer.choose_except(None),
                d => match gpu_of(d) {
                    Some(l) if load(l) >= 60.0 => placer.earlier_with_room(l),
                    _ => None,
                },
            };
            let Some(to) = target else { continue };
            log::info!(
                "balancing: stream {id} moves from {} ({:.0} %) to {}",
                placer.name(st.decode_on),
                gpu_of(st.decode_on).map_or(100.0, load),
                placer.name(to)
            );
            self.streams[id].send(Cmd::Decoder(to));
            self.valve.last_move = Some(Instant::now());
            break;
        }
    }

    /// Looks for a newer release (daily on start, or `force` from About).
    pub(crate) fn check_updates(&mut self, force: bool) {
        if self.update.checking || self.options.smoke.is_some() || (!force && !self.settings.auto_update) {
            return;
        }
        self.update.checking = true;
        let (slot, wake) = (self.update_slot.clone(), self.wake());
        crate::update::spawn_check(force, self.settings.channel(), move |r| {
            if let Ok(mut s) = slot.lock() {
                *s = Some((force, r));
            }
            wake();
        });
    }

    /// Installs the found update: download, verify, start the setup, close.
    pub(crate) fn install_update(&mut self) {
        let Some(c) = self.update.found.clone() else { return };
        if self.update.installing {
            return;
        }
        if !crate::update::installed() {
            self.ctx.open_url(egui::OpenUrl::new_tab(crate::update::RELEASE_PAGE));
            return;
        }
        self.update.installing = true;
        let (slot, wake) = (self.install_slot.clone(), self.wake());
        crate::update::spawn_install(c, move |r| {
            if let Ok(mut s) = slot.lock() {
                *s = Some(r);
            }
            wake();
        });
    }

    /// Finds the installed browsers once (registry, on a worker).
    pub(crate) fn detect_browsers(&mut self) {
        if self.login.browsers.is_some() {
            return;
        }
        self.login.browsers = Some(Vec::new());
        let (slot, wake) = (self.browsers_slot.clone(), self.wake());
        self.pool.spawn(move || {
            let list = crate::browsers::detect();
            if let Ok(mut s) = slot.lock() {
                *s = Some(list);
            }
            wake();
        });
    }

    /// Deletes the cookies file the login came from.
    pub(crate) fn delete_imported_file(&mut self) {
        let Some(path) = self.login.imported_file.take() else { return };
        let wake = self.wake();
        self.pool.spawn(move || {
            if let Err(e) = std::fs::remove_file(&path) {
                log::warn!("deleting {}: {e}", path.display());
            }
            wake();
        });
        self.login.message = Some((true, "Deleted the cookies file.".into()));
    }

    pub(crate) fn pick_cookie_file(&mut self) {
        self.login_from_file(None);
    }

    /// Logs in from a cookie file: `path`, or one the viewer picks. The
    /// dialog, the read and the check all run on a worker.
    pub(crate) fn login_from_file(&mut self, path: Option<std::path::PathBuf>) {
        let (slot, wake) = (self.login_result.clone(), self.wake());
        self.login.busy = true;
        self.login.message = None;
        let http = self.client.lock().map(|c| c.http().clone()).unwrap_or_default();
        let browser = self.login_browser();
        self.pool.spawn(move || {
            let file = path.or_else(|| {
                rfd::FileDialog::new()
                    .set_title("Your Twitch cookies (cookies.txt)")
                    .add_filter("Cookies", &["txt", "json"])
                    .pick_file()
            });
            let used = file.clone();
            let r = match file {
                None => Err(String::new()),
                Some(path) => match std::fs::read_to_string(&path) {
                    Err(e) => Err(format!("Could not read the file: {e}")),
                    Ok(text) => match gk_twitch::auth::parse_login_input(&text) {
                        None => Err(
                            "That file has no Twitch login. Export it while twitch.tv is open and you are logged in."
                                .into(),
                        ),
                        Some(found) => finish_login(&http, found, browser.as_ref()).inspect(|a| {
                            if let Err(e) = gk_twitch::auth::store::save(a) {
                                log::warn!("could not store the login: {e}");
                            }
                        }),
                    },
                },
            };
            let used = used.filter(|_| r.is_ok());
            if let Ok(mut s) = slot.lock() {
                *s = Some((r, used));
            }
            wake();
        });
    }

    pub(crate) fn logout(&mut self) {
        gk_twitch::auth::store::clear();
        self.apply_auth(None);
        self.login.message = Some((true, "Logged out.".into()));
    }

    fn apply_auth(&mut self, auth: Option<Auth>) {
        self.auth = auth.clone();
        // Requests identify as the viewer's browser from now on.
        let http = auth.as_ref().and_then(|a| a.user_agent.as_deref()).map(Http::with_user_agent).unwrap_or_default();
        if let Ok(mut c) = self.client.lock() {
            *c = Client::new(http, auth.clone());
        }
        self.chat.set_credentials(auth.map(|a| gk_chat::Credentials { login: a.login, token: a.token }));
        self.followed.clear();
        self.followed_at = None;
        // New playback tokens: with the account's Turbo status.
        for s in self.streams.values() {
            s.send(Cmd::Reload);
        }
    }

    pub(crate) fn refresh_followed(&mut self, force: bool) {
        if self.auth.is_none() || self.followed_loading {
            return;
        }
        if !force && self.followed_at.is_some_and(|t| t.elapsed() < Duration::from_secs(90)) {
            return;
        }
        self.followed_loading = true;
        self.followed_at = Some(Instant::now());
        let (slot, wake, client) = (self.followed_slot.clone(), self.wake(), self.client.clone());
        self.pool.spawn(move || {
            let r = client
                .lock()
                .map(|c| c.clone())
                .map_err(|_| "busy".to_string())
                .and_then(|c| c.followed_live().map_err(|e| e.to_string()));
            if let Ok(mut s) = slot.lock() {
                *s = Some(r);
            }
            wake();
        });
    }

    /// After a change in the settings window: applies and saves them.
    pub(crate) fn settings_changed(&mut self) {
        self.settings.apply();
        if let Ok(mut pl) = self.placer.lock() {
            pl.set_others(self.settings.other_gpus);
        }
        let s = self.settings.clone();
        self.pool.spawn(move || s.save());
    }

    /// The most watched categories, for the random-streams dialog (once).
    pub(crate) fn fetch_categories(&mut self) {
        if self.random.categories_asked {
            return;
        }
        self.random.categories_asked = true;
        let (slot, wake, client) = (self.categories_slot.clone(), self.wake(), self.client.clone());
        self.pool.spawn(move || {
            let Some(c) = client.lock().map(|c| c.clone()).ok() else { return };
            match c.top_categories(40) {
                Ok(list) => {
                    if let Ok(mut s) = slot.lock() {
                        *s = Some(list);
                    }
                    wake();
                }
                Err(e) => log::warn!("top categories: {e}"),
            }
        });
    }

    /// Adds random live channels to the current tab (GK-12): the dialog's
    /// count, category and language; channels already open are left out.
    pub(crate) fn add_random(&mut self) {
        if self.random.busy {
            return;
        }
        let count = self.random.count.clamp(1, 100) as usize;
        let category = Some(self.random.category.trim().to_owned()).filter(|c| !c.is_empty());
        let language = crate::ui::LANGUAGES.get(self.random.language).and_then(|(_, code)| *code);
        let open: Vec<String> = self.tabs.iter().flat_map(|t| t.stream_channels()).collect();
        self.random.busy = true;
        self.random.message = None;
        let (slot, wake, client) = (self.random_slot.clone(), self.wake(), self.client.clone());
        self.pool.spawn(move || {
            let r = client.lock().map(|c| c.clone()).map_err(|_| "busy".to_string()).and_then(|c| {
                c.random_live(count, category.as_deref(), language, &open).map_err(|e| match e {
                    gk_twitch::Error::NotFound => "No such category.".to_string(),
                    e => e.to_string(),
                })
            });
            if let Ok(mut s) = slot.lock() {
                *s = Some(r);
            }
            wake();
        });
    }

    /// Title, category and viewers for the streams in the tabs, every minute.
    fn refresh_info(&mut self) {
        if self.info_at.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
            return;
        }
        let logins: Vec<String> =
            self.tabs.iter().flat_map(|t| t.stream_channels()).collect::<HashSet<_>>().into_iter().collect();
        if logins.is_empty() {
            return;
        }
        self.info_at = Some(Instant::now());
        let (slot, wake, client) = (self.info_slot.clone(), self.wake(), self.client.clone());
        self.pool.spawn(move || {
            let Some(c) = client.lock().map(|c| c.clone()).ok() else { return };
            let refs: Vec<&str> = logins.iter().map(String::as_str).collect();
            let mut all = Vec::new();
            for chunk in refs.chunks(50) {
                if let Ok(list) = c.channels(chunk) {
                    all.extend(list);
                }
            }
            if let Ok(mut s) = slot.lock() {
                *s = Some(all);
            }
            wake();
        });
    }

    /// Takes results from workers.
    fn poll(&mut self) {
        if let Some(defs) = self.fonts_slot.lock().ok().and_then(|mut s| s.take()) {
            if let Some(w) = &self.stats_win {
                w.ctx.set_fonts(defs.clone());
            }
            self.font_defs = Some(defs.clone());
            self.ctx.set_fonts(defs);
            self.chat.generation += 1;
        }
        if let Some((force, r)) = self.update_slot.lock().ok().and_then(|mut s| s.take()) {
            self.update.checking = false;
            use crate::update::Outcome;
            match r {
                Ok(Outcome::Found(c)) => {
                    self.update.message = Some((true, format!("Glotzkiste {} is available.", c.version)));
                    self.update.found = Some(c);
                }
                Ok(Outcome::UpToDate) => {
                    self.update.message = force
                        .then(|| (true, format!("Glotzkiste {} is the newest version.", env!("CARGO_PKG_VERSION"))))
                }
                Ok(Outcome::Off(why)) => self.update.message = Some((false, why)),
                Ok(Outcome::Skipped) => {}
                Err(e) => {
                    log::warn!("update check: {e}");
                    if force {
                        self.update.message = Some((false, format!("The check failed: {e}")));
                    }
                }
            }
        }
        if let Some(r) = self.install_slot.lock().ok().and_then(|mut s| s.take()) {
            self.update.installing = false;
            match r {
                // The setup waits for us to close, installs and restarts us.
                Ok(()) => self.quit = true,
                Err(e) => {
                    self.update.message = Some((false, e));
                    self.about_open = true;
                }
            }
        }
        if let Some(list) = self.browsers_slot.lock().ok().and_then(|mut s| s.take()) {
            self.login.browsers = Some(list);
            self.login.browser = 0;
        }
        if let Some((r, file)) = self.login_result.lock().ok().and_then(|mut s| s.take()) {
            self.login.busy = false;
            if file.is_some() {
                self.login.imported_file = file;
            }
            match r {
                Ok(a) => {
                    self.login.message = Some((true, format!("Logged in as {}.", a.login)));
                    self.login.paste.clear();
                    self.apply_auth(Some(a));
                    self.refresh_followed(true);
                }
                Err(e) if e.is_empty() => {}
                Err(e) => self.login.message = Some((false, e)),
            }
        }
        if let Some(r) = self.followed_slot.lock().ok().and_then(|mut s| s.take()) {
            self.followed_loading = false;
            match r {
                Ok(list) => self.followed = list,
                Err(e) => log::warn!("followed channels: {e}"),
            }
        }
        if let Some(list) = self.categories_slot.lock().ok().and_then(|mut s| s.take()) {
            self.random.categories = list;
        }
        if let Some(r) = self.random_slot.lock().ok().and_then(|mut s| s.take()) {
            self.random.busy = false;
            match &r {
                Ok(list) => log::info!("random streams: {}", list.len()),
                Err(e) => log::warn!("random streams: {e}"),
            }
            match r {
                Ok(list) if list.is_empty() => {
                    self.random.message = Some((false, "Nothing live matches that.".into()));
                }
                Ok(list) => {
                    let wanted = self.random.count.clamp(1, 100) as usize;
                    for login in &list {
                        self.add_channel(login);
                    }
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        tab.apply(crate::layout::Preset::Grid, None);
                    }
                    let n = list.len();
                    self.random.message = Some((
                        true,
                        if n < wanted {
                            format!("Added {n} streams (only {n} live matched).")
                        } else {
                            format!("Added {n} stream{}.", if n == 1 { "" } else { "s" })
                        },
                    ));
                }
                Err(e) => self.random.message = Some((false, e)),
            }
        }
        if let Some(list) = self.info_slot.lock().ok().and_then(|mut s| s.take()) {
            for c in list {
                self.info.insert(c.login.clone(), c);
            }
        }
        self.chat.poll();
        self.images.poll(&self.ctx, self.presenter.as_deref());
    }

    fn save_now(&mut self) {
        if !self.persist {
            return;
        }
        if let Some(json) = crate::layout::to_json(&self.tabs, self.active, self.latency, self.geom)
            && json != self.saved_json
        {
            crate::layout::write(json.clone());
            self.saved_json = json;
        }
    }

    /// Saves the layout off the UI thread when it changed (checked every 2 s).
    fn save_soon(&mut self) {
        if !self.save_dirty || self.save_check.elapsed() < Duration::from_secs(2) || !self.persist {
            return;
        }
        self.save_check = Instant::now();
        self.save_dirty = false;
        if let Some(json) = crate::layout::to_json(&self.tabs, self.active, self.latency, self.geom)
            && json != self.saved_json
        {
            self.saved_json = json.clone();
            self.pool.spawn(move || crate::layout::write(json));
        }
    }

    /// Remembers where the window is (not in smoke runs: those sit offscreen).
    fn track_window(&mut self) {
        if self.smoke_until.is_some() || self.options.smoke.is_some() {
            return;
        }
        let Some(w) = &self.window else { return };
        if w.is_minimized() == Some(true) {
            return;
        }
        let maximized = w.is_maximized();
        let size = w.inner_size();
        let pos = w.outer_position().ok();
        let mut g = self.geom.unwrap_or(WindowGeom { x: 0, y: 0, w: size.width, h: size.height, maximized });
        g.maximized = maximized;
        // Keep the normal size and place while maximized, so un-maximizing
        // after a restart goes back to them.
        if !maximized && size.width > 0 && size.height > 0 {
            g.w = size.width;
            g.h = size.height;
            if let Some(p) = pos {
                g.x = p.x;
                g.y = p.y;
            }
        }
        self.geom = Some(g);
        self.save_dirty = true;
    }

    /// A one-line summary of the restored state (smoke runs).
    fn describe_state(&self) -> String {
        let tabs: Vec<String> = self
            .tabs
            .iter()
            .map(|t| {
                format!(
                    "{}[{}{}{}]",
                    t.title,
                    t.stream_channels().join(","),
                    if t.has_chat() { " +chat" } else { "" },
                    t.audio.as_deref().map(|a| format!(" audio={a}")).unwrap_or_default()
                )
            })
            .collect();
        format!("state: active={} latency={:?} tabs={}", self.active, self.latency, tabs.join(" "))
    }

    fn pass(&mut self) {
        let Some(window) = self.window.clone() else { return };
        let t0 = Instant::now();
        self.poll();
        let Some(state) = self.egui_state.as_mut() else { return };
        let mut raw = state.take_egui_input(&window);
        if self.smoke_until.is_some()
            && let Some((x, y)) = self.options.pointer
        {
            raw.events.push(egui::Event::PointerMoved(egui::pos2(x, y)));
        }
        let mut videos: Vec<VideoTile> = Vec::new();
        let mut heights: Vec<(u64, u32)> = Vec::new();
        self.images.frame += 1;
        let stats_shown = self.stats_open || self.stats_on_tiles;
        crate::stream::STATS_ON.store(stats_shown, std::sync::atomic::Ordering::Relaxed);
        if stats_shown && let Some(p) = self.presenter.clone() {
            self.sample_stats(&p);
        }
        let ctx = self.ctx.clone();
        let full = ctx.run_ui(raw, |ui| {
            crate::ui::draw(self, ui, &mut videos, &mut heights);
        });
        self.passes += 1;
        if let Some(state) = self.egui_state.as_mut() {
            state.handle_platform_output(&window, full.platform_output);
        }
        let ppp = full.pixels_per_point;
        let mut textures_delta = full.textures_delta;
        if let Some(presenter) = &self.presenter {
            let primitives = self.ctx.tessellate(full.shapes, ppp);
            presenter.submit(UiFrame { primitives, textures_delta, pixels_per_point: ppp, videos });
        } else {
            textures_delta.clear();
        }
        for (id, h) in heights {
            if self.tile_heights.get(&id) != Some(&h) {
                self.tile_heights.insert(id, h);
                if let Some(s) = self.streams.get(&id) {
                    s.send(Cmd::TileHeight(h));
                }
            }
        }
        self.repaint_at = None;
        if let Some(vp) = full.viewport_output.get(&egui::ViewportId::ROOT) {
            if vp.repaint_delay.is_zero() {
                self.needs_pass = true;
            } else if vp.repaint_delay < Duration::from_secs(3600) {
                self.repaint_at = Some(Instant::now() + vp.repaint_delay);
            }
        }
        self.sync_streams();
        self.refresh_info();
        self.save_dirty = true;
        self.save_soon();
        self.pass_us += t0.elapsed().as_micros() as u64;
    }

    fn finish_smoke(&mut self, event_loop: &ActiveEventLoop) {
        self.smoke_until = None;
        let Some(presenter) = self.presenter.clone() else {
            event_loop.exit();
            return;
        };
        if let Some(path) = self.options.snapshot.clone() {
            self.pass();
            if self.stats_win.is_some() {
                self.stats_pass();
                if let Some(w) = &self.stats_win {
                    let stats_path = path.with_file_name(format!(
                        "{}-stats.png",
                        path.file_stem().and_then(|s| s.to_str()).unwrap_or("snapshot")
                    ));
                    if let Err(e) = w.presenter.snapshot(stats_path) {
                        log::error!("stats snapshot: {e}");
                    }
                }
            }
            if let Err(e) = presenter.snapshot(path) {
                log::error!("snapshot: {e}");
            }
        }
        let st = presenter.stats();
        let secs = self.started.elapsed().as_secs_f64();
        let ld = |a: &std::sync::atomic::AtomicU64| a.load(std::sync::atomic::Ordering::Relaxed);
        println!(
            "smoke: secs={secs:.1} presents={} ui_passes={} ui_pass_ms_total={} ui_frames={} video_frames={} anim_redraws={} render_busy_ms={} slow_waits={}",
            ld(&st.presents),
            self.passes,
            self.pass_us / 1000,
            ld(&st.ui_frames),
            ld(&st.video_frames),
            ld(&st.anim_redraws),
            ld(&st.busy_us) / 1000,
            ld(&st.slow_waits)
        );
        if ld(&st.copied_frames) > 0 {
            println!(
                "copies: frames={} gpu_to_gpu={} MB={:.1} cpu_ms_total={:.1} cpu_us_per_readback={:.0}",
                ld(&st.copied_frames),
                ld(&st.gpu_copied_frames),
                ld(&st.copied_bytes) as f64 / 1e6,
                ld(&st.copy_us) as f64 / 1000.0,
                ld(&st.copy_us) as f64 / (ld(&st.copied_frames) - ld(&st.gpu_copied_frames)).max(1) as f64
            );
        }
        let tiles = st.tiles.lock().map(|t| t.clone()).unwrap_or_default();
        let mut ids: Vec<&u64> = self.streams.keys().collect();
        ids.sort();
        for id in &ids {
            if let Some(t) = tiles.get(id) {
                println!(
                    "frames {id}: new={} ({:.1}/s) repeats={} redraws={} tile={}x{}",
                    t.frames,
                    t.frames as f64 / secs,
                    t.repeats,
                    t.redraws,
                    t.width,
                    t.height
                );
            }
        }
        for id in ids {
            let s = &self.streams[id];
            let st = s.status();
            println!(
                "stream {id} {}: state={:?} variant={} buffer={:.1}s drops={} hwdec={} height={} turbo={} ads_expected={} low_latency={} in_ad={} decoder={}",
                s.channel,
                st.state,
                st.current,
                st.buffer_tenths as f64 / 10.0,
                st.drops,
                st.hwdec,
                st.video_height,
                st.turbo,
                st.ads_expected,
                st.low_latency,
                st.in_ad,
                st.decoder
            );
        }
        println!("{}", self.describe_state());
        for g in self.gpus.lock().map(|g| g.clone()).unwrap_or_default() {
            println!("gpu: {} decode={:.2}% render={}", g.name, g.decode_pct, g.luid == self.render_luid);
        }
        for (name, ch) in &self.chat.channels {
            println!("chat {name}: lines={} room_id={} emotes={}", ch.buffer.len(), ch.room_id, ch.emotes.len());
        }
        event_loop.exit();
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        use winit::platform::windows::WindowAttributesExtWindows;
        let (w, h) = self.options.size.unwrap_or((1600, 900));
        let mut attrs =
            Window::default_attributes().with_title("Glotzkiste").with_min_inner_size(LogicalSize::new(480.0, 270.0));
        {
            // The exe's own icon resource (build.rs): small for the title
            // bar, large for the taskbar and Alt-Tab.
            use winit::platform::windows::IconExtWindows;
            let small = winit::window::Icon::from_resource(1, Some(PhysicalSize::new(32, 32))).ok();
            let big = winit::window::Icon::from_resource(1, Some(PhysicalSize::new(256, 256))).ok();
            attrs = attrs.with_window_icon(small).with_taskbar_icon(big);
        }
        // Restore the saved place if it is still on a connected monitor.
        let saved = self.geom.filter(|g| {
            self.options.size.is_none()
                && g.w >= 320
                && g.h >= 200
                && event_loop.available_monitors().any(|m| {
                    let (p, s) = (m.position(), m.size());
                    let (cx, cy) = (g.x + g.w as i32 / 2, g.y + 20);
                    cx >= p.x && cx < p.x + s.width as i32 && cy >= p.y && cy < p.y + s.height as i32
                })
        });
        attrs = if self.smoke_until.is_some() {
            // Agent runs never show on the owner's screen (AGENTS.md).
            attrs
                .with_inner_size(PhysicalSize::new(w, h))
                .with_position(PhysicalPosition::new(-20000, -20000))
                .with_skip_taskbar(true)
                .with_active(false)
        } else if let Some(g) = saved {
            attrs
                .with_inner_size(PhysicalSize::new(g.w, g.h))
                .with_position(PhysicalPosition::new(g.x, g.y))
                .with_maximized(g.maximized)
        } else {
            attrs
                .with_inner_size(LogicalSize::new(w as f64, h as f64))
                .with_maximized(self.geom.is_some_and(|g| g.maximized))
        };
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                log::error!("cannot create the window: {e}");
                self.exit_code = 1;
                event_loop.exit();
                return;
            }
        };
        let hwnd = match window.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
            _ => {
                log::error!("no Win32 window handle");
                self.exit_code = 1;
                event_loop.exit();
                return;
            }
        };
        let size = window.inner_size();
        match Presenter::start(hwnd, (size.width, size.height)) {
            Ok((p, info)) => {
                log::info!("adapter: {} software={}", info.adapter, info.software);
                self.adapter =
                    if info.software { format!("{} (software)", info.adapter) } else { info.adapter.clone() };
                self.render_luid = info.luid;
                if let Ok(mut pl) = self.placer.lock() {
                    *pl = crate::placement::Placer::new(info.luid, crate::gpu::adapters());
                    pl.set_others(self.settings.other_gpus);
                }
                p.set_clear_color(crate::ui::BG_LINEAR);
                self.presenter = Some(Arc::new(p));
            }
            Err(e) => self.error = Some(format!("Video output failed: {e}")),
        }
        let state = egui_winit::State::new(
            self.ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            None,
            Some(8192),
        );
        // Installed release builds look for updates once a day.
        self.check_updates(false);
        // The UI never updates faster than the display can show.
        if let Some(mhz) = window.current_monitor().and_then(|m| m.refresh_rate_millihertz()).filter(|m| *m >= 24_000) {
            self.pass_interval = Duration::from_micros(1_000_000_000 / u64::from(mhz));
        }
        self.egui_state = Some(state);
        self.window = Some(window);
        self.needs_pass = true;
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Wake => self.needs_pass = true,
            UserEvent::LibReady => {
                if let Some(Some(Err(e))) = self.lib.lock().ok().map(|s| s.clone()) {
                    self.error = Some(format!("The video library could not be loaded: {e}"));
                }
                self.needs_pass = true;
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if let Some(w) = self.stats_win.as_mut()
            && w.window.id() == id
        {
            if w.state.on_window_event(&w.window, &event).repaint {
                w.needs_pass = true;
            }
            match event {
                WindowEvent::CloseRequested => {
                    self.stats_open = false;
                    self.stats_win = None;
                    self.needs_pass = true;
                }
                WindowEvent::Resized(size) => {
                    w.presenter.resize(size.width, size.height);
                    w.needs_pass = true;
                }
                _ => {}
            }
            return;
        }
        if let (Some(state), Some(window)) = (self.egui_state.as_mut(), self.window.as_ref()) {
            let r = state.on_window_event(window, &event);
            if r.repaint {
                self.needs_pass = true;
            }
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(p) = &self.presenter {
                    p.resize(size.width, size.height);
                }
                self.track_window();
                self.needs_pass = true;
            }
            WindowEvent::RedrawRequested => self.needs_pass = true,
            WindowEvent::Moved(_) => self.track_window(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.quit {
            event_loop.exit();
            return;
        }
        self.sync_stats_window(event_loop);
        self.run_valve();
        if self.smoke_until.is_some_and(|t| Instant::now() >= t) {
            self.finish_smoke(event_loop);
            return;
        }
        if self.repaint_at.is_some_and(|t| Instant::now() >= t) {
            self.needs_pass = true;
        }
        // One pass for everything that arrived since the last one, and at
        // most one per display refresh (mouse moves can arrive at 1000 Hz);
        // no frame loop of our own (AGENTS.md performance rule 3).
        let mut next = self.smoke_until;
        if self.needs_pass {
            let ready = self.last_pass + self.pass_interval;
            if Instant::now() >= ready {
                self.needs_pass = false;
                self.last_pass = Instant::now();
                self.pass();
            } else {
                next = Some(next.map_or(ready, |n| n.min(ready)));
            }
        }
        self.save_soon();
        if let Some(t) = self.repaint_at {
            next = Some(next.map_or(t, |n| n.min(t)));
        }
        // The stats window refreshes twice a second while open.
        if let Some(w) = &self.stats_win {
            let due = w.last_pass + Duration::from_millis(500);
            if w.needs_pass || Instant::now() >= due {
                self.stats_pass();
            }
            if let Some(w) = &self.stats_win {
                let t = w.last_pass + Duration::from_millis(500);
                next = Some(next.map_or(t, |n| n.min(t)));
            }
        }
        // The valve looks at the decoder every 2 s while streams play.
        if !self.streams.is_empty() {
            let t = self.valve.checked.unwrap_or_else(Instant::now) + Duration::from_secs(2);
            next = Some(next.map_or(t, |n| n.min(t)));
        }
        // A pending save wakes the loop once, even when nothing else moves.
        if self.persist && self.save_dirty {
            let t = self.save_check + Duration::from_secs(2);
            next = Some(next.map_or(t, |n| n.min(t)));
        }
        event_loop.set_control_flow(match next {
            Some(t) => ControlFlow::WaitUntil(t),
            None => ControlFlow::Wait,
        });
    }
}
