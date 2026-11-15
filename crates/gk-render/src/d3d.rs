//! Device, waitable swap chain, stream surfaces and the render loop.

use crate::egui_d3d::{EguiRenderer, create_texture};
use crate::{ANIM_BIT, AnimFrames, DeviceInfo, Stats, UiFrame, VideoTile};
use gk_mpv::{Mpv, RenderContext};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HANDLE, HMODULE, HWND, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::WaitForSingleObjectEx;
use windows::core::Interface;

type SnapshotReply = mpsc::Sender<Result<(), String>>;

#[derive(Default)]
struct Requests {
    ui: Option<UiFrame>,
    /// Texture changes of UI frames that were replaced before being drawn.
    deltas: egui::TexturesDelta,
    size: Option<(u32, u32)>,
    add: Vec<(u64, Mpv)>,
    remove: Vec<u64>,
    anim_add: Vec<(u64, Arc<AnimFrames>)>,
    anim_remove: Vec<u64>,
    clear_color: Option<[f32; 4]>,
    snapshot: Option<(std::path::PathBuf, SnapshotReply)>,
    quit: bool,
}

impl Requests {
    /// Drops pending texture changes on purpose (shutting down).
    fn clear_deltas(&mut self) {
        self.deltas.clear();
        if let Some(ui) = self.ui.as_mut() {
            ui.textures_delta.clear();
        }
    }

    fn pending(&self) -> bool {
        self.ui.is_some()
            || !self.deltas.is_empty()
            || self.size.is_some()
            || !self.add.is_empty()
            || !self.remove.is_empty()
            || !self.anim_add.is_empty()
            || !self.anim_remove.is_empty()
            || self.clear_color.is_some()
            || self.snapshot.is_some()
            || self.quit
    }
}

struct Shared {
    requests: Mutex<Requests>,
    wake: Condvar,
    /// Set by any stream's mpv update callback.
    video_dirty: AtomicBool,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut Requests)) {
        let mut r = self.requests.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut r);
        self.wake.notify_one();
    }
    fn poke_video(&self) {
        self.video_dirty.store(true, Ordering::Release);
        let _g = self.requests.lock().unwrap_or_else(|p| p.into_inner());
        self.wake.notify_one();
    }
}

/// The handle the UI thread keeps. Every call returns at once.
pub struct Presenter {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    stats: Arc<Stats>,
}

impl Presenter {
    /// Starts the render thread for `hwnd` and waits until its device and
    /// swap chain exist (or failed).
    pub fn start(hwnd: isize, size: (u32, u32)) -> Result<(Self, DeviceInfo), String> {
        let shared = Arc::new(Shared { requests: Mutex::new(Requests::default()), wake: Condvar::new(), video_dirty: AtomicBool::new(false) });
        let stats = Arc::new(Stats::default());
        let (tx, rx) = mpsc::channel();
        let thread = {
            let (shared, stats) = (shared.clone(), stats.clone());
            std::thread::Builder::new()
                .name("gk-render".into())
                .spawn(move || {
                    let hwnd = HWND(hwnd as *mut _);
                    match Renderer::new(hwnd, size, shared.clone()) {
                        Ok((renderer, info)) => {
                            let _ = tx.send(Ok(info));
                            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| renderer.run(&shared, &stats)));
                            if r.is_err() {
                                log::error!("render thread panicked");
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e));
                        }
                    }
                })
                .map_err(|e| e.to_string())?
        };
        let info = rx.recv().map_err(|_| "the render thread stopped during start".to_owned())??;
        Ok((Self { shared, thread: Some(thread), stats }, info))
    }

    pub fn stats(&self) -> Arc<Stats> {
        self.stats.clone()
    }

    pub fn resize(&self, w: u32, h: u32) {
        self.shared.update(|r| r.size = Some((w, h)));
    }

    /// Hands over a new UI. An earlier, undrawn UI is replaced; its texture
    /// changes are kept and applied in order.
    pub fn submit(&self, frame: UiFrame) {
        self.shared.update(|r| {
            if let Some(mut old) = r.ui.take() {
                r.deltas.append(std::mem::take(&mut old.textures_delta));
            }
            r.ui = Some(frame);
        });
    }

    /// Adds a stream; the render thread creates its mpv render context on
    /// the shared device. Its video shows wherever a `VideoTile` names `id`.
    pub fn add_stream(&self, id: u64, mpv: Mpv) {
        self.shared.update(|r| r.add.push((id, mpv)));
    }

    pub fn remove_stream(&self, id: u64) {
        self.shared.update(|r| r.remove.push(id));
    }

    /// Registers an animated image as user texture `id` (must have `ANIM_BIT`).
    pub fn set_animation(&self, id: u64, frames: Arc<AnimFrames>) {
        self.shared.update(|r| r.anim_add.push((id | ANIM_BIT, frames)));
    }

    pub fn remove_animation(&self, id: u64) {
        self.shared.update(|r| r.anim_remove.push(id | ANIM_BIT));
    }

    /// The colour behind everything (linear 0..1, gamma-encoded like egui).
    pub fn set_clear_color(&self, rgba: [f32; 4]) {
        self.shared.update(|r| r.clear_color = Some(rgba));
    }

    /// Draws one frame and writes it to `path` as PNG (for `--snapshot`).
    pub fn snapshot(&self, path: std::path::PathBuf) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.shared.update(|r| r.snapshot = Some((path, tx)));
        rx.recv_timeout(Duration::from_secs(10)).map_err(|_| "snapshot timed out".to_owned())?
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        self.shared.update(|r| r.quit = true);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.shared.update(|r| r.clear_deltas());
    }
}

struct Surface {
    // Declared first: dropped before the textures it renders into.
    render: RenderContext,
    tex: Option<(ID3D11Texture2D, ID3D11ShaderResourceView, (u32, u32))>,
    has_frame: bool,
}

struct Anim {
    frames: Vec<(ID3D11ShaderResourceView, u32)>,
    total_ms: u64,
}

impl Anim {
    fn index_at(&self, ms: u64) -> usize {
        if self.total_ms == 0 || self.frames.len() <= 1 {
            return 0;
        }
        let mut t = ms % self.total_ms;
        for (i, (_, d)) in self.frames.iter().enumerate() {
            if t < u64::from(*d) {
                return i;
            }
            t -= u64::from(*d);
        }
        0
    }
    /// Milliseconds until the frame after the one shown at `ms`.
    fn next_change(&self, ms: u64) -> Option<u64> {
        if self.total_ms == 0 || self.frames.len() <= 1 {
            return None;
        }
        let mut t = ms % self.total_ms;
        for (_, d) in &self.frames {
            let d = u64::from(*d);
            if t < d {
                return Some(d - t);
            }
            t -= d;
        }
        Some(1)
    }
}

struct Renderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain2,
    waitable: HANDLE,
    target: Option<(ID3D11Texture2D, ID3D11RenderTargetView)>,
    size: (u32, u32),
    egui: EguiRenderer,
    videos: Vec<VideoTile>,
    surfaces: HashMap<u64, Surface>,
    anims: HashMap<u64, Anim>,
    /// Animations drawn by the current UI and the frame each shows.
    visible_anims: Vec<(u64, usize)>,
    clear: [f32; 4],
    epoch: Instant,
    shared: Arc<Shared>,
}

const SWAP_FLAGS: DXGI_SWAP_CHAIN_FLAG = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;

fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext, bool), String> {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0];
    // Hardware with video support (d3d11va), then hardware, then WARP.
    let attempts = [
        (D3D_DRIVER_TYPE_HARDWARE, D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT, false),
        (D3D_DRIVER_TYPE_HARDWARE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, false),
        (D3D_DRIVER_TYPE_WARP, D3D11_CREATE_DEVICE_BGRA_SUPPORT, true),
    ];
    let mut last = String::new();
    for (driver, flags, software) in attempts {
        let (mut device, mut context) = (None, None);
        // SAFETY: out pointers are valid locals.
        let r = unsafe {
            D3D11CreateDevice(None, driver, HMODULE::default(), flags, Some(&levels), D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut context))
        };
        match (r, device, context) {
            (Ok(()), Some(d), Some(c)) => return Ok((d, c, software)),
            (Err(e), _, _) => last = e.to_string(),
            _ => last = "no device returned".into(),
        }
    }
    Err(format!("no Direct3D 11 device: {last}"))
}

impl Renderer {
    fn new(hwnd: HWND, size: (u32, u32), shared: Arc<Shared>) -> Result<(Self, DeviceInfo), String> {
        let (device, context, software) = create_device()?;
        let e = |e: windows::core::Error| e.to_string();
        // SAFETY: COM calls on valid interfaces with valid descriptors.
        unsafe {
            // Several decoders (one per stream, on mpv's threads) share this
            // device's immediate context with our render thread.
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            let dxgi: IDXGIDevice = device.cast().map_err(e)?;
            let adapter = dxgi.GetAdapter().map_err(e)?;
            let desc = adapter.GetDesc().map_err(e)?;
            let adapter_name = String::from_utf16_lossy(&desc.Description).trim_end_matches('\0').to_owned();
            let factory: IDXGIFactory2 = adapter.GetParent().map_err(e)?;
            let sc_desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: size.0.max(1),
                Height: size.1.max(1),
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: SWAP_FLAGS.0 as u32,
                Stereo: false.into(),
            };
            let sc1 = factory.CreateSwapChainForHwnd(&device, hwnd, &sc_desc, None, None).map_err(e)?;
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
            let swap_chain: IDXGISwapChain2 = sc1.cast().map_err(e)?;
            swap_chain.SetMaximumFrameLatency(1).map_err(e)?;
            let waitable = swap_chain.GetFrameLatencyWaitableObject();
            let egui = EguiRenderer::new(&device)?;
            Ok((
                Self {
                    device,
                    context,
                    swap_chain,
                    waitable,
                    target: None,
                    size,
                    egui,
                    videos: Vec::new(),
                    surfaces: HashMap::new(),
                    anims: HashMap::new(),
                    visible_anims: Vec::new(),
                    clear: [0.04, 0.04, 0.05, 1.0],
                    epoch: Instant::now(),
                    shared,
                },
                DeviceInfo { adapter: adapter_name, software },
            ))
        }
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Time until the next visible animation frame changes.
    fn anim_deadline(&self) -> Option<Duration> {
        let now = self.now_ms();
        self.visible_anims
            .iter()
            .filter_map(|(id, _)| self.anims.get(id)?.next_change(now))
            .min()
            .map(Duration::from_millis)
    }

    fn run(mut self, shared: &Shared, stats: &Stats) {
        let mut need_wait_for_swap = true;
        loop {
            // 1. Sleep until there is work: a request, a video frame or the
            //    next animation frame.
            let mut req = {
                let mut r = shared.requests.lock().unwrap_or_else(|p| p.into_inner());
                loop {
                    if r.pending() || shared.video_dirty.load(Ordering::Acquire) {
                        break;
                    }
                    match self.anim_deadline() {
                        Some(d) => {
                            let (g, t) = shared.wake.wait_timeout(r, d).unwrap_or_else(|p| p.into_inner());
                            r = g;
                            if t.timed_out() {
                                break;
                            }
                        }
                        None => r = shared.wake.wait(r).unwrap_or_else(|p| p.into_inner()),
                    }
                }
                std::mem::take(&mut *r)
            };
            if req.quit {
                req.clear_deltas();
                return;
            }
            // 2. Wait for the swap chain to accept a frame, so whatever
            //    arrives meanwhile goes into this one present.
            if need_wait_for_swap && self.size.0 > 0 && self.size.1 > 0 {
                let t = Instant::now();
                // SAFETY: the waitable handle belongs to our swap chain.
                let w = unsafe { WaitForSingleObjectEx(self.waitable, 100, true) };
                if w != WAIT_OBJECT_0 || t.elapsed() > Duration::from_millis(50) {
                    stats.slow_waits.fetch_add(1, Ordering::Relaxed);
                }
                // Merge what arrived while waiting.
                let more = std::mem::take(&mut *shared.requests.lock().unwrap_or_else(|p| p.into_inner()));
                merge(&mut req, more);
                if req.quit {
                    req.clear_deltas();
                    return;
                }
            }
            let busy = Instant::now();
            let mut draw = false;
            if let Some(c) = req.clear_color {
                self.clear = c;
                draw = true;
            }
            for id in req.remove {
                self.surfaces.remove(&id);
                draw = true;
            }
            for (id, mpv) in req.add {
                self.add_surface(id, &mpv);
            }
            for id in req.anim_remove {
                self.anims.remove(&id);
            }
            for (id, frames) in req.anim_add {
                if let Some(a) = self.make_anim(&frames) {
                    self.anims.insert(id, a);
                    draw = true;
                }
            }
            if let Some(size) = req.size
                && size != self.size
            {
                self.resize(size);
                draw = true;
            }
            // All texture changes in order: undrawn older UIs first.
            let mut delta = std::mem::take(&mut req.deltas);
            let mut ui_frame = req.ui.take();
            if let Some(ui) = ui_frame.as_mut() {
                delta.append(std::mem::take(&mut ui.textures_delta));
            }
            self.egui.set_textures(&self.device, &self.context, &delta);
            if let Some(ui) = ui_frame.take() {
                if let Err(e) = self.egui.upload(&self.device, &self.context, &ui.primitives, ui.pixels_per_point, self.size) {
                    log::error!("ui upload: {e}");
                }
                self.videos = ui.videos;
                let now = self.now_ms();
                self.visible_anims = self
                    .egui
                    .user_ids
                    .iter()
                    .filter(|u| **u & ANIM_BIT != 0)
                    .filter_map(|u| Some((*u, self.anims.get(u)?.index_at(now))))
                    .collect();
                stats.ui_frames.fetch_add(1, Ordering::Relaxed);
                draw = true;
            }
            // 3. Streams: render every new frame into its tile texture.
            if shared.video_dirty.swap(false, Ordering::AcqRel) && self.render_streams(stats) {
                draw = true;
            }
            // 4. Animations whose frame changed.
            let now = self.now_ms();
            for (id, shown) in self.visible_anims.iter_mut() {
                if let Some(a) = self.anims.get(id) {
                    let i = a.index_at(now);
                    if i != *shown {
                        *shown = i;
                        draw = true;
                        stats.anim_redraws.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            if req.snapshot.is_some() {
                draw = true;
            }
            need_wait_for_swap = false;
            if draw && self.size.0 > 0 && self.size.1 > 0 {
                match self.frame(req.snapshot) {
                    Ok(()) => {
                        stats.presents.fetch_add(1, Ordering::Relaxed);
                        need_wait_for_swap = true;
                    }
                    Err(e) => log::error!("render failed: {e}"),
                }
            }
            self.egui.free_textures(&delta);
            delta.clear();
            stats.busy_us.fetch_add(busy.elapsed().as_micros() as u64, Ordering::Relaxed);
        }
    }

    fn add_surface(&mut self, id: u64, mpv: &Mpv) {
        // SAFETY: our device was created without SINGLETHREADED.
        match unsafe { RenderContext::new_d3d11(mpv, self.device.as_raw()) } {
            Ok(mut render) => {
                let shared = self.shared.clone();
                render.set_update_callback(move || shared.poke_video());
                self.surfaces.insert(id, Surface { render, tex: None, has_frame: false });
                self.shared.video_dirty.store(true, Ordering::Release);
            }
            Err(e) => log::error!("stream {id}: render context failed: {e}"),
        }
    }

    /// Renders new frames of all streams. True if anything visible changed.
    fn render_streams(&mut self, stats: &Stats) -> bool {
        let mut changed = false;
        let videos = self.videos.clone();
        for (id, s) in self.surfaces.iter_mut() {
            let flags = s.render.update();
            let tile = videos.iter().find(|v| v.id == *id);
            // Size the texture to the tile; hidden streams get a tiny one
            // so mpv's frame queue keeps moving at almost no cost.
            let want = tile.map(|t| t.size()).unwrap_or((16, 16));
            let resized = s.tex.as_ref().map(|t| t.2) != Some(want);
            if !RenderContext::frame_ready(flags) && !(resized && s.has_frame) {
                continue;
            }
            if resized {
                match create_target(&self.device, want) {
                    Ok((t, v)) => s.tex = Some((t, v, want)),
                    Err(e) => {
                        log::error!("stream texture {want:?}: {e}");
                        continue;
                    }
                }
            }
            if let Some((tex, _, _)) = &s.tex {
                // SAFETY: a render-target texture on mpv's device.
                match unsafe { s.render.render_d3d11(tex.as_raw(), false) } {
                    Ok(()) => {
                        s.has_frame = true;
                        stats.video_frames.fetch_add(1, Ordering::Relaxed);
                        changed |= tile.is_some();
                    }
                    Err(e) => log::warn!("stream {id}: render: {e}"),
                }
            }
        }
        // Streams whose tile size changed but that have no new frame yet are
        // handled above (re-render of the current frame).
        changed
    }

    fn make_anim(&self, a: &AnimFrames) -> Option<Anim> {
        let mut frames = Vec::new();
        let mut total = 0u64;
        for (rgba, delay) in &a.frames {
            if rgba.len() != (a.width * a.height * 4) as usize {
                continue;
            }
            let pm = premultiply(rgba);
            let (t, v) = create_texture(&self.device, a.width, a.height).ok()?;
            // SAFETY: width x height RGBA8 data.
            unsafe { self.context.UpdateSubresource(&t, 0, None, pm.as_ptr().cast(), a.width * 4, 0) };
            frames.push((v, *delay));
            total += u64::from(*delay);
        }
        (!frames.is_empty()).then_some(Anim { frames, total_ms: total })
    }

    fn resize(&mut self, size: (u32, u32)) {
        self.target = None;
        self.size = size;
        if size.0 == 0 || size.1 == 0 {
            return;
        }
        // SAFETY: no outstanding references to the buffers (target dropped).
        if let Err(e) = unsafe { self.swap_chain.ResizeBuffers(0, size.0, size.1, DXGI_FORMAT_UNKNOWN, SWAP_FLAGS) } {
            log::error!("ResizeBuffers: {e}");
        }
    }

    fn frame(&mut self, snapshot: Option<(std::path::PathBuf, SnapshotReply)>) -> Result<(), String> {
        let e = |e: windows::core::Error| e.to_string();
        // SAFETY: COM calls on interfaces owned by this thread.
        unsafe {
            if self.target.is_none() {
                let tex: ID3D11Texture2D = self.swap_chain.GetBuffer(0).map_err(e)?;
                let mut rtv = None;
                self.device.CreateRenderTargetView(&tex, None, Some(&mut rtv)).map_err(e)?;
                self.target = Some((tex, rtv.ok_or("no render target view")?));
            }
            let Some((tex, rtv)) = self.target.clone() else { return Ok(()) };
            self.context.ClearRenderTargetView(&rtv, &self.clear);
            let now = self.now_ms();
            let surfaces = &self.surfaces;
            let anims = &self.anims;
            let lookup = |u: u64| -> Option<ID3D11ShaderResourceView> {
                if u & ANIM_BIT != 0 {
                    let a = anims.get(&u)?;
                    return Some(a.frames.get(a.index_at(now))?.0.clone());
                }
                let s = surfaces.get(&u)?;
                if !s.has_frame {
                    return None;
                }
                s.tex.as_ref().map(|t| t.1.clone())
            };
            self.egui.draw(&self.context, &rtv, self.size, &lookup)?;
            if let Some((path, reply)) = snapshot {
                let _ = reply.send(write_snapshot(&self.device, &self.context, &tex, &path));
            }
            self.swap_chain.Present(1, DXGI_PRESENT(0)).ok().map_err(e)?;
            for s in self.surfaces.values_mut() {
                s.render.report_swap();
            }
        }
        Ok(())
    }
}

fn merge(into: &mut Requests, more: Requests) {
    let mut more = more;
    if let Some(ui) = more.ui.take() {
        if let Some(mut old) = into.ui.take() {
            into.deltas.append(std::mem::take(&mut old.textures_delta));
        }
        into.ui = Some(ui);
    }
    into.deltas.append(std::mem::take(&mut more.deltas));
    if more.size.is_some() {
        into.size = more.size;
    }
    into.add.append(&mut more.add);
    into.remove.append(&mut more.remove);
    into.anim_add.append(&mut more.anim_add);
    into.anim_remove.append(&mut more.anim_remove);
    if more.clear_color.is_some() {
        into.clear_color = more.clear_color;
    }
    if more.snapshot.is_some() {
        into.snapshot = more.snapshot.take();
    }
    into.quit |= more.quit;
}

fn premultiply(rgba: &[u8]) -> Vec<u8> {
    let mut out = rgba.to_vec();
    for px in out.chunks_exact_mut(4) {
        let a = u16::from(px[3]);
        for c in &mut px[..3] {
            *c = ((u16::from(*c) * a + 127) / 255) as u8;
        }
    }
    out
}

/// A texture mpv renders into and egui samples from.
fn create_target(device: &ID3D11Device, (w, h): (u32, u32)) -> windows::core::Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        ..Default::default()
    };
    let (mut t, mut v) = (None, None);
    // SAFETY: valid descriptor and out pointers.
    unsafe {
        device.CreateTexture2D(&desc, None, Some(&mut t))?;
        let t = t.ok_or_else(windows::core::Error::empty)?;
        device.CreateShaderResourceView(&t, None, Some(&mut v))?;
        Ok((t, v.ok_or_else(windows::core::Error::empty)?))
    }
}

/// Copies the back buffer to a CPU-readable texture and writes a PNG.
fn write_snapshot(device: &ID3D11Device, ctx: &ID3D11DeviceContext, tex: &ID3D11Texture2D, path: &std::path::Path) -> Result<(), String> {
    let e = |e: windows::core::Error| e.to_string();
    // SAFETY: a staging copy of a texture on this device, mapped for read and
    // unmapped before returning.
    let (w, h, pixels) = unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        tex.GetDesc(&mut desc);
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging = None;
        device.CreateTexture2D(&desc, None, Some(&mut staging)).map_err(e)?;
        let staging = staging.ok_or("no staging texture")?;
        ctx.CopyResource(&staging, tex);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m)).map_err(e)?;
        let mut pixels = Vec::with_capacity((desc.Width * desc.Height * 4) as usize);
        for y in 0..desc.Height as usize {
            let row = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize), desc.Width as usize * 4);
            for px in row.chunks_exact(4) {
                pixels.extend_from_slice(&[px[2], px[1], px[0], 255]);
            }
        }
        ctx.Unmap(&staging, 0);
        (desc.Width, desc.Height, pixels)
    };
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&pixels).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anim(delays: &[u32]) -> Anim {
        // Views are not needed for timing; build with dummy-free logic.
        Anim { frames: Vec::new(), total_ms: delays.iter().map(|d| u64::from(*d)).sum() }
    }

    #[test]
    fn premultiplies() {
        assert_eq!(premultiply(&[255, 128, 0, 128]), vec![128, 64, 0, 128]);
        assert_eq!(premultiply(&[10, 20, 30, 255]), vec![10, 20, 30, 255]);
    }

    #[test]
    fn empty_animation_never_schedules() {
        let a = anim(&[]);
        assert_eq!(a.next_change(5), None);
        assert_eq!(a.index_at(5), 0);
    }
}
