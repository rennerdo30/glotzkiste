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
    pub(crate) info: HashMap<String, ChannelInfo>,
    info_slot: Slot<Vec<ChannelInfo>>,
    info_at: Option<Instant>,
    fonts_slot: Slot<egui::FontDefinitions>,
    pub(crate) renaming: Option<usize>,
    pub(crate) stats_open: bool,
    pub(crate) stats_on_tiles: bool,
    pub(crate) sampler: crate::ui::stats::Sampler,
    pub(crate) tile_rects: Vec<crate::ui::stats::TileRect>,
    pub(crate) content_rect: egui::Rect,
    pub(crate) adapter: String,
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
fn finish_login(http: &Http, found: gk_twitch::auth::LoginInput, browser: Option<&crate::browsers::Browser>) -> Result<Auth, String> {
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
        let persist = !smoke || options.state_dir.is_some();
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
            if options.chat {
                let first = options.channels[0].clone();
                tab.apply(Preset::Grid, Some(first));
            }
            tabs.push(tab);
            active = tabs.len() - 1;
        }
        if tabs.is_empty() {
            tabs.push(Tab::new("Main", vec![]));
        }
        active = active.min(tabs.len() - 1);
        let ctx = egui::Context::default();
        crate::ui::style(&ctx);
        let stats_open = options.stats;
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
            info: HashMap::new(),
            info_slot: Arc::default(),
            info_at: None,
            fonts_slot,
            renaming: None,
            stats_open: false,
            stats_on_tiles: false,
            sampler: Default::default(),
            tile_rects: Vec::new(),
            content_rect: egui::Rect::NOTHING,
            adapter: String::new(),
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
            exit_code: 0,
        };
        app.stats_open = stats_open;
        app.login.open = app.options.login;
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
                            };
                            let audible = active && audio.as_deref() == Some(p.channel.as_str());
                            let settings = Settings { quality: p.quality.clone(), volume: p.volume, muted: !audible, latency: self.latency };
                            self.streams.insert(id, StreamHandle::spawn(id, p.channel.clone(), env, settings));
                            self.muted.insert(id, !audible);
                        }
                        let e = wanted.entry(p.id).or_default();
                        e.0 |= active;
                        e.1 |= active && audio.as_deref() == Some(p.channel.as_str());
                    }
                    egui_tiles::Tile::Pane(Pane::Chat { channel }) => {
                        chats.insert(channel.clone());
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
        let login = raw.trim().trim_start_matches("https://").trim_start_matches("www.").trim_start_matches("twitch.tv/");
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
            self.login.message = Some((false, "No Twitch login in that. Paste the auth-token value, or what the extension copied.".into()));
            return;
        };
        self.login.busy = true;
        self.login.message = None;
        let browser = self.login_browser();
        let (slot, wake, http) = (self.login_result.clone(), self.wake(), self.client.lock().map(|c| c.http().clone()).unwrap_or_default());
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
            let file = path.or_else(|| rfd::FileDialog::new().set_title("Your Twitch cookies (cookies.txt)").add_filter("Cookies", &["txt", "json"]).pick_file());
            let used = file.clone();
            let r = match file {
                None => Err(String::new()),
                Some(path) => match std::fs::read_to_string(&path) {
                    Err(e) => Err(format!("Could not read the file: {e}")),
                    Ok(text) => match gk_twitch::auth::parse_login_input(&text) {
                        None => Err("That file has no Twitch login. Export it while twitch.tv is open and you are logged in.".into()),
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
            let r = client.lock().map(|c| c.clone()).map_err(|_| "busy".to_string()).and_then(|c| c.followed_live().map_err(|e| e.to_string()));
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
        let logins: Vec<String> = self.tabs.iter().flat_map(|t| t.stream_channels()).collect::<HashSet<_>>().into_iter().collect();
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
            self.ctx.set_fonts(defs);
            for ch in self.chat.channels.values_mut() {
                ch.view.invalidate();
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
            .map(|t| format!("{}[{}{}{}]", t.title, t.stream_channels().join(","), if t.has_chat() { " +chat" } else { "" }, t.audio.as_deref().map(|a| format!(" audio={a}")).unwrap_or_default()))
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
        if stats_shown && let Some(p) = &self.presenter {
            self.sampler.sample(&p.stats(), self.passes, self.pass_us);
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
        let mut ids: Vec<&u64> = self.streams.keys().collect();
        ids.sort();
        for id in ids {
            let s = &self.streams[id];
            let st = s.status();
            println!(
                "stream {id} {}: state={:?} variant={} buffer={:.1}s drops={} hwdec={} height={} turbo={} ads_expected={} low_latency={} in_ad={}",
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
                st.in_ad
            );
        }
        println!("{}", self.describe_state());
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
        let mut attrs = Window::default_attributes().with_title("Glotzkiste").with_min_inner_size(LogicalSize::new(480.0, 270.0));
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
            attrs.with_inner_size(PhysicalSize::new(g.w, g.h)).with_position(PhysicalPosition::new(g.x, g.y)).with_maximized(g.maximized)
        } else {
            attrs.with_inner_size(LogicalSize::new(w as f64, h as f64)).with_maximized(self.geom.is_some_and(|g| g.maximized))
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
                self.adapter = if info.software { format!("{} (software)", info.adapter) } else { info.adapter.clone() };
                p.set_clear_color(crate::ui::BG_LINEAR);
                self.presenter = Some(Arc::new(p));
            }
            Err(e) => self.error = Some(format!("Video output failed: {e}")),
        }
        let state = egui_winit::State::new(self.ctx.clone(), egui::ViewportId::ROOT, &window, Some(window.scale_factor() as f32), None, Some(8192));
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

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
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
        if self.smoke_until.is_some_and(|t| Instant::now() >= t) {
            self.finish_smoke(event_loop);
            return;
        }
        if self.repaint_at.is_some_and(|t| Instant::now() >= t) {
            self.needs_pass = true;
        }
        // One pass for everything that arrived since the last one; no frame
        // loop of our own (AGENTS.md performance rule 3).
        if self.needs_pass {
            self.needs_pass = false;
            self.pass();
        }
        self.save_soon();
        let mut next = self.smoke_until;
        if let Some(t) = self.repaint_at {
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
