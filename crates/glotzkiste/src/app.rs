//! The window, the egui pass and the model (tabs of tile layouts).

use crate::stream::{Cmd, Env, Latency, Quality, Settings, State, StreamHandle};
use crate::Options;
use gk_render::{Presenter, UiFrame, VideoTile};
use gk_twitch::{Client, Http};
use std::collections::HashMap;
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StreamPane {
    pub channel: String,
    pub quality: Quality,
    pub volume: f64,
    pub muted: bool,
    #[serde(skip)]
    pub id: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Pane {
    Stream(StreamPane),
    Chat { channel: String },
    Empty,
}

pub struct Tab {
    pub title: String,
    pub tree: egui_tiles::Tree<Pane>,
}

type LibSlot = Arc<Mutex<Option<Result<Arc<gk_mpv::Libmpv>, String>>>>;

struct App {
    options: Options,
    proxy: EventLoopProxy<UserEvent>,
    window: Option<Arc<Window>>,
    egui_state: Option<egui_winit::State>,
    ctx: egui::Context,
    presenter: Option<Arc<Presenter>>,
    lib: LibSlot,
    client: Arc<Mutex<Client>>,
    streams: HashMap<u64, StreamHandle>,
    tile_heights: HashMap<u64, u32>,
    visible: HashMap<u64, bool>,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    latency: Latency,
    needs_pass: bool,
    repaint_at: Option<Instant>,
    smoke_until: Option<Instant>,
    started: Instant,
    error: Option<String>,
    passes: u64,
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
    app.streams.clear();
    app.exit_code
}

fn grid_tree(id: &str, panes: Vec<Pane>) -> egui_tiles::Tree<Pane> {
    if panes.is_empty() {
        return egui_tiles::Tree::new_grid(egui::Id::unique(id), vec![Pane::Empty]);
    }
    egui_tiles::Tree::new_grid(egui::Id::unique(id), panes)
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
        let auth = gk_twitch::auth::store::load();
        let client = Arc::new(Mutex::new(Client::new(Http::new(), auth)));
        let panes: Vec<Pane> = options
            .channels
            .iter()
            .map(|c| Pane::Stream(StreamPane { channel: c.clone(), quality: Quality::Auto, volume: 70.0, muted: false, id: 0 }))
            .collect();
        let tabs = vec![Tab { title: "Main".into(), tree: grid_tree("tab-0", panes) }];
        let smoke_until = options.smoke.map(|s| Instant::now() + Duration::from_secs_f64(s));
        let ctx = egui::Context::default();
        ctx.set_visuals(egui::Visuals::dark());
        Self {
            options,
            proxy,
            window: None,
            egui_state: None,
            ctx,
            presenter: None,
            lib,
            client,
            streams: HashMap::new(),
            tile_heights: HashMap::new(),
            visible: HashMap::new(),
            tabs,
            active: 0,
            next_id: 1,
            latency: Latency::Ultra,
            needs_pass: true,
            repaint_at: None,
            smoke_until,
            started: Instant::now(),
            error: None,
            passes: 0,
            exit_code: 0,
        }
    }

    fn lib(&self) -> Option<Arc<gk_mpv::Libmpv>> {
        self.lib.lock().ok()?.as_ref()?.as_ref().ok().cloned()
    }

    /// Starts players for new stream panes, stops removed ones, and pauses
    /// video decoding of streams outside the active tab.
    fn sync_streams(&mut self) {
        let (Some(lib), Some(presenter)) = (self.lib(), self.presenter.clone()) else { return };
        let mut wanted: HashMap<u64, bool> = HashMap::new();
        for (ti, tab) in self.tabs.iter_mut().enumerate() {
            for tile in tab.tree.tiles.tiles_mut() {
                if let egui_tiles::Tile::Pane(Pane::Stream(p)) = tile {
                    if p.id == 0 || !self.streams.contains_key(&p.id) {
                        let id = self.next_id;
                        self.next_id += 1;
                        p.id = id;
                        let proxy = self.proxy.clone();
                        let env = Env {
                            lib: lib.clone(),
                            client: self.client.clone(),
                            presenter: presenter.clone(),
                            notify: Arc::new(move || {
                                let _ = proxy.send_event(UserEvent::Wake);
                            }),
                            muted_by_flag: self.options.mute,
                        };
                        let settings = Settings { quality: p.quality.clone(), volume: p.volume, muted: p.muted, latency: self.latency };
                        self.streams.insert(id, StreamHandle::spawn(id, p.channel.clone(), env, settings));
                    }
                    *wanted.entry(p.id).or_default() |= ti == self.active;
                }
            }
        }
        self.streams.retain(|id, _| wanted.contains_key(id));
        self.tile_heights.retain(|id, _| wanted.contains_key(id));
        for (id, vis) in wanted {
            if self.visible.get(&id) != Some(&vis) {
                self.visible.insert(id, vis);
                if let Some(s) = self.streams.get(&id) {
                    s.send(Cmd::Visible(vis));
                }
            }
        }
    }

    fn pass(&mut self) {
        let (Some(window), Some(state)) = (self.window.clone(), self.egui_state.as_mut()) else { return };
        let raw = state.take_egui_input(&window);
        let mut videos: Vec<VideoTile> = Vec::new();
        let mut heights: Vec<(u64, u32)> = Vec::new();
        let ctx = self.ctx.clone();
        let full = ctx.run_ui(raw, |ui| {
            self.ui(ui, &mut videos, &mut heights);
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
    }

    fn ui(&mut self, ui: &mut egui::Ui, videos: &mut Vec<VideoTile>, heights: &mut Vec<(u64, u32)>) {
        let ctx = ui.ctx().clone();
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(egui::Color32::from_rgb(10, 10, 12))).show(ui, |ui| {
            if let Some(e) = &self.error {
                ui.centered_and_justified(|ui| ui.label(egui::RichText::new(e).color(egui::Color32::LIGHT_RED)));
                return;
            }
            let ppp = ctx.pixels_per_point();
            let mut behavior = TileBehavior { streams: &self.streams, videos, heights, ppp };
            if let Some(tab) = self.tabs.get_mut(self.active) {
                tab.tree.ui(&mut behavior, ui);
            }
        });
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
            "smoke: secs={secs:.1} presents={} ui_passes={} ui_frames={} video_frames={} anim_redraws={} render_busy_ms={} slow_waits={}",
            ld(&st.presents),
            self.passes,
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
        event_loop.exit();
    }
}

struct TileBehavior<'a> {
    streams: &'a HashMap<u64, StreamHandle>,
    videos: &'a mut Vec<VideoTile>,
    heights: &'a mut Vec<(u64, u32)>,
    ppp: f32,
}

impl egui_tiles::Behavior<Pane> for TileBehavior<'_> {
    fn pane_ui(&mut self, ui: &mut egui::Ui, _tile: egui_tiles::TileId, pane: &mut Pane) -> egui_tiles::UiResponse {
        let rect = ui.max_rect();
        match pane {
            Pane::Stream(p) => {
                // Align to whole pixels: the stream texture is exactly this
                // size, so it maps 1:1 to the screen.
                let px = [
                    (rect.min.x * self.ppp).round() as i32,
                    (rect.min.y * self.ppp).round() as i32,
                    (rect.max.x * self.ppp).round() as i32,
                    (rect.max.y * self.ppp).round() as i32,
                ];
                let aligned = egui::Rect::from_min_max(
                    egui::pos2(px[0] as f32 / self.ppp, px[1] as f32 / self.ppp),
                    egui::pos2(px[2] as f32 / self.ppp, px[3] as f32 / self.ppp),
                );
                let status = self.streams.get(&p.id).map(|s| s.status()).unwrap_or_default();
                if p.id != 0 && px[2] > px[0] && px[3] > px[1] {
                    ui.painter().rect_filled(aligned, 0.0, egui::Color32::BLACK);
                    ui.painter().image(
                        egui::TextureId::User(p.id),
                        aligned,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                    self.videos.push(VideoTile { id: p.id, rect_px: px });
                    self.heights.push((p.id, (px[3] - px[1]).max(1) as u32));
                }
                let msg = match &status.state {
                    _ if status.in_ad => Some(format!("{}: ad break, skipping it (log in with Turbo for none)", p.channel)),
                    State::Starting => Some(format!("{} · connecting…", p.channel)),
                    State::Offline => Some(format!("{} is offline", p.channel)),
                    State::Error(e) => Some(format!("{}: {e}", p.channel)),
                    State::Playing => None,
                };
                if let Some(m) = msg {
                    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, m, egui::FontId::proportional(16.0), egui::Color32::from_gray(200));
                }
                let label = format!("{}  {}  {:.1}s", p.channel, status.current, status.buffer_tenths as f64 / 10.0);
                ui.painter().text(rect.min + egui::vec2(8.0, 6.0), egui::Align2::LEFT_TOP, label, egui::FontId::proportional(13.0), egui::Color32::from_white_alpha(200));
                egui_tiles::UiResponse::None
            }
            Pane::Chat { channel } => {
                ui.label(format!("chat: {channel}"));
                egui_tiles::UiResponse::None
            }
            Pane::Empty => {
                ui.centered_and_justified(|ui| ui.label("Empty"));
                egui_tiles::UiResponse::None
            }
        }
    }

    fn tab_title_for_pane(&mut self, pane: &Pane) -> egui::WidgetText {
        match pane {
            Pane::Stream(p) => p.channel.clone().into(),
            Pane::Chat { channel } => format!("chat · {channel}").into(),
            Pane::Empty => "empty".into(),
        }
    }

    fn gap_width(&self, _style: &egui::Style) -> f32 {
        2.0
    }

    fn simplification_options(&self) -> egui_tiles::SimplificationOptions {
        egui_tiles::SimplificationOptions { all_panes_must_have_tabs: false, ..Default::default() }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        use winit::platform::windows::WindowAttributesExtWindows;
        let (w, h) = self.options.size.unwrap_or((1600, 900));
        let mut attrs = Window::default_attributes()
            .with_title("Glotzkiste")
            .with_min_inner_size(LogicalSize::new(480.0, 270.0));
        attrs = if self.smoke_until.is_some() {
            // Agent runs never show on the owner's screen (AGENTS.md).
            attrs
                .with_inner_size(PhysicalSize::new(w, h))
                .with_position(PhysicalPosition::new(-20000, -20000))
                .with_skip_taskbar(true)
                .with_active(false)
        } else {
            attrs.with_inner_size(LogicalSize::new(w as f64, h as f64))
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
                self.sync_streams();
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
                self.needs_pass = true;
            }
            WindowEvent::RedrawRequested => {
                self.needs_pass = false;
                self.pass();
            }
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
        let mut next = self.smoke_until;
        if let Some(t) = self.repaint_at {
            next = Some(next.map_or(t, |n| n.min(t)));
        }
        event_loop.set_control_flow(match next {
            Some(t) => ControlFlow::WaitUntil(t),
            None => ControlFlow::Wait,
        });
    }
}
