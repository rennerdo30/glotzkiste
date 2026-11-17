//! Device, waitable swap chain, stream surfaces and the render loop.

use crate::egui_d3d::{EguiRenderer, create_texture};
use crate::worker::{Delivery, RING, Worker};
use crate::{ANIM_BIT, AnimFrames, DeviceInfo, Stats, UiFrame, VideoTile};
use gk_mpv::{FrameKind, Mpv, RenderContext};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE, HMODULE, HWND, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::WaitForSingleObjectEx;
use windows::core::{Interface, PCWSTR};

type SnapshotReply = mpsc::Sender<Result<(), String>>;

#[derive(Default)]
struct Requests {
    ui: Option<UiFrame>,
    /// Texture changes of UI frames that were replaced before being drawn.
    deltas: egui::TexturesDelta,
    size: Option<(u32, u32)>,
    add: Vec<(u64, Mpv, Option<mpsc::Sender<()>>)>,
    remove: Vec<(u64, Option<mpsc::Sender<()>>)>,
    anim_add: Vec<(u64, Arc<AnimFrames>)>,
    anim_remove: Vec<u64>,
    clear_color: Option<[f32; 4]>,
    /// Hand the tile sizes to the second GPU again (a stream moved there).
    resend_sizes: bool,
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
            || self.resend_sizes
            || self.snapshot.is_some()
            || self.quit
    }
}

struct Shared {
    requests: Mutex<Requests>,
    wake: Condvar,
    /// Set by any stream's mpv update callback (and by another GPU's
    /// thread when it finished a frame).
    video_dirty: AtomicBool,
    /// Every render worker's frames and tile sizes.
    workers: Mutex<Vec<Arc<crate::worker::Shared>>>,
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

/// Where a stream is rendered when not on the render thread itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// A render worker on the main GPU (index into `Presenter::workers`).
    Worker(usize),
    /// The worker of another GPU.
    Other((u32, i32)),
}

/// The handle the UI thread keeps. Every call returns at once, except the
/// stream placement calls, which stream threads make.
pub struct Presenter {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    stats: Arc<Stats>,
    luid: (u32, i32),
    /// Render workers on the main GPU (empty: the render thread renders
    /// every stream itself).
    workers: Vec<Arc<Worker>>,
    /// Workers of other GPUs, started on first use.
    others: Mutex<HashMap<(u32, i32), Arc<Worker>>>,
    placed: Mutex<HashMap<u64, Place>>,
    /// The render thread's fence as an NT handle (workers open it).
    main_fence: Option<usize>,
}

/// How long a stream thread waits for a render thread to take or release a
/// stream (normally a few milliseconds).
const PLACE_TIMEOUT: Duration = Duration::from_secs(3);

/// How often unfinished worker frames are looked at again.
const READY_POLL: Duration = Duration::from_millis(2);

/// Render workers on the main GPU: `GK_RENDER_WORKERS` (0 turns them off),
/// else one per eight hardware threads, two to four.
static GPU_COPY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
static RENDER_WORKERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Copy frames from other GPUs GPU to GPU (default) or read them back. For
/// GPUs used from now on; `GK_BRIDGE=0` wins.
pub fn set_gpu_copy(on: bool) {
    GPU_COPY.store(on, Ordering::Relaxed);
}

/// Render workers on the main GPU (0: automatic) for presenters started from
/// now on; `GK_RENDER_WORKERS` wins.
pub fn set_render_workers(n: usize) {
    RENDER_WORKERS.store(n, Ordering::Relaxed);
}

fn worker_count() -> usize {
    if let Some(n) = std::env::var("GK_RENDER_WORKERS").ok().and_then(|v| v.parse::<usize>().ok()) {
        return n.min(8);
    }
    let set = RENDER_WORKERS.load(Ordering::Relaxed);
    if set > 0 {
        return set.min(8);
    }
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    (threads / 8).clamp(2, 4)
}

impl Presenter {
    /// Starts the render thread for `hwnd` and waits until its device and
    /// swap chain exist (or failed), then the render workers.
    pub fn start(hwnd: isize, size: (u32, u32)) -> Result<(Self, DeviceInfo), String> {
        Self::start_with(hwnd, size, true)
    }

    /// Like `start`; `workers` false for a window without streams (the
    /// stats window), which needs no render workers.
    pub fn start_with(hwnd: isize, size: (u32, u32), workers: bool) -> Result<(Self, DeviceInfo), String> {
        let want_workers = workers;
        let shared = Arc::new(Shared {
            requests: Mutex::new(Requests::default()),
            wake: Condvar::new(),
            video_dirty: AtomicBool::new(false),
            workers: Mutex::new(Vec::new()),
        });
        let stats = Arc::new(Stats::default());
        let (tx, rx) = mpsc::channel();
        let thread = {
            let (shared, stats) = (shared.clone(), stats.clone());
            std::thread::Builder::new()
                .name("gk-render".into())
                .spawn(move || {
                    crate::playback_thread();
                    let hwnd = HWND(hwnd as *mut _);
                    match Renderer::new(hwnd, size, shared.clone()) {
                        Ok((renderer, info, fence)) => {
                            let _ = tx.send(Ok((info, fence)));
                            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                renderer.run(&shared, &stats)
                            }));
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
        let (info, main_fence) = rx.recv().map_err(|_| "the render thread stopped during start".to_owned())??;
        let luid = info.luid;
        let mut workers = Vec::new();
        if let Some(h) = main_fence.filter(|_| want_workers && !info.software) {
            for _ in 0..worker_count() {
                let poke = shared.clone();
                match Worker::start(luid, Delivery::Shared { main_fence: h }, Arc::new(move || poke.poke_video())) {
                    Ok(w) => {
                        if let Ok(mut v) = shared.workers.lock() {
                            v.push(w.shared.clone());
                        }
                        workers.push(Arc::new(w));
                    }
                    Err(e) => {
                        log::warn!("render workers unavailable, the render thread draws every stream: {e}");
                        break;
                    }
                }
            }
        }
        if want_workers {
            log::info!("{} render workers on the main GPU", workers.len());
        }
        let p = Self {
            shared,
            thread: Some(thread),
            stats,
            luid,
            workers,
            others: Mutex::new(HashMap::new()),
            placed: Mutex::new(HashMap::new()),
            main_fence,
        };
        Ok((p, info))
    }

    pub fn stats(&self) -> Arc<Stats> {
        self.stats.clone()
    }

    /// The adapter of the GPU that draws the window.
    pub fn luid(&self) -> (u32, i32) {
        self.luid
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

    /// Adds a stream to the render thread itself (see `place_stream`). Its
    /// video shows wherever a `VideoTile` names `id`.
    pub fn add_stream(&self, id: u64, mpv: Mpv) {
        self.shared.update(|r| r.add.push((id, mpv, None)));
    }

    /// Like `add_stream`; the receiver gets a message once the render
    /// context exists (mpv must not load a file before that).
    pub fn add_stream_ready(&self, id: u64, mpv: Mpv) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        self.shared.update(|r| r.add.push((id, mpv, Some(tx))));
        rx
    }

    fn worker_of(&self, place: Place) -> Option<Arc<Worker>> {
        match place {
            Place::Worker(k) => self.workers.get(k).cloned(),
            Place::Other(l) => self.others.lock().ok()?.get(&l).cloned(),
        }
    }

    pub fn remove_stream(&self, id: u64) {
        let place = self.placed.lock().ok().and_then(|mut p| p.remove(&id));
        if let Some(w) = place.and_then(|p| self.worker_of(p)) {
            w.remove(id, None);
        }
        self.shared.update(|r| r.remove.push((id, None)));
    }

    /// The worker of the GPU with `luid`, started if needed.
    fn other_for(&self, luid: (u32, i32)) -> Result<Arc<Worker>, String> {
        let mut map = self.others.lock().map_err(|_| "GPU list poisoned")?;
        if let Some(w) = map.get(&luid) {
            return Ok(w.clone());
        }
        let shared = self.shared.clone();
        // GPU to GPU unless turned off (GK_BRIDGE=0) or the main thread has
        // no shared fence; the worker falls back to read-back by itself.
        let bridge_off = std::env::var("GK_BRIDGE").is_ok_and(|v| v == "0") || !GPU_COPY.load(Ordering::Relaxed);
        let delivery = match self.main_fence {
            Some(main_fence) if !bridge_off => Delivery::Bridge { main_luid: self.luid, main_fence },
            _ => Delivery::Readback,
        };
        let w = Arc::new(Worker::start(luid, delivery, Arc::new(move || shared.poke_video()))?);
        if let Ok(mut v) = self.shared.workers.lock() {
            v.push(w.shared.clone());
        }
        map.insert(luid, w.clone());
        Ok(w)
    }

    fn attach(&self, id: u64, mpv: &Mpv, place: Place) -> Result<(), String> {
        let w = match place {
            Place::Worker(k) => self.workers.get(k).cloned().ok_or("no such render worker")?,
            Place::Other(l) => self.other_for(l)?,
        };
        let (tx, rx) = mpsc::channel();
        w.add(id, mpv.clone(), tx);
        rx.recv_timeout(PLACE_TIMEOUT).unwrap_or_else(|_| Err("the render worker did not answer".into()))?;
        if let Ok(mut p) = self.placed.lock() {
            p.insert(id, place);
        }
        // The worker needs the tile size now, not at the next UI change.
        self.shared.update(|r| r.resend_sizes = true);
        Ok(())
    }

    /// Creates a stream's render context (and so its video decoder): on the
    /// GPU with `luid` if that is another GPU (rendered at tile size there,
    /// frames copied over), else on the main GPU, by the least busy render
    /// worker (or the render thread itself). Falls back to the main GPU if
    /// another GPU fails. Blocks the calling stream thread a few
    /// milliseconds; returns where the stream went (`None`: the main GPU)
    /// and the error that sent it there.
    pub fn place_stream(&self, id: u64, mpv: &Mpv, luid: Option<(u32, i32)>) -> (Option<(u32, i32)>, Option<String>) {
        let mut err = None;
        if let Some(l) = luid.filter(|l| *l != self.luid) {
            match self.attach(id, mpv, Place::Other(l)) {
                Ok(()) => return (Some(l), None),
                Err(e) => err = Some(e),
            }
        }
        let least = (0..self.workers.len()).min_by_key(|k| self.workers[*k].shared.streams.load(Ordering::Relaxed));
        if let Some(k) = least {
            match self.attach(id, mpv, Place::Worker(k)) {
                Ok(()) => return (None, err),
                Err(e) => log::warn!("stream {id}: render worker: {e}; on the render thread"),
            }
        }
        let _ = self.add_stream_ready(id, mpv.clone()).recv_timeout(PLACE_TIMEOUT);
        (None, err)
    }

    /// Moves a playing stream's render context to another GPU (see
    /// `place_stream`). The player must reopen its file afterwards: the
    /// video output starts over on the new device.
    pub fn move_stream(&self, id: u64, mpv: &Mpv, luid: Option<(u32, i32)>) -> (Option<(u32, i32)>, Option<String>) {
        let to = luid.filter(|l| *l != self.luid);
        let from = self.placed.lock().ok().and_then(|p| p.get(&id).copied());
        let from_gpu = match from {
            Some(Place::Other(l)) => Some(l),
            _ => None,
        };
        if from_gpu == to {
            return (to, None);
        }
        // mpv has one render context at a time: free the current one first.
        if let Some(w) = from.and_then(|p| self.worker_of(p)) {
            if let Ok(mut p) = self.placed.lock() {
                p.remove(&id);
            }
            let (tx, rx) = mpsc::channel();
            w.remove(id, Some(tx));
            let _ = rx.recv_timeout(PLACE_TIMEOUT);
        }
        // The render thread drops its context or textures of the stream.
        let (tx, rx) = mpsc::channel();
        self.shared.update(|r| r.remove.push((id, Some(tx))));
        if rx.recv_timeout(PLACE_TIMEOUT).is_err() {
            return (from_gpu, Some("the render thread did not release the stream".into()));
        }
        self.place_stream(id, mpv, to)
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
        self.workers.clear();
        if let Ok(mut o) = self.others.lock() {
            o.clear();
        }
        if let Some(h) = self.main_fence.take() {
            // SAFETY: the NT handle the render thread created for the workers,
            // which are gone now.
            unsafe {
                let _ = CloseHandle(HANDLE(h as *mut _));
            }
        }
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
    /// mpv asked for a render that has not happened yet.
    pending: bool,
    pending_kind: FrameKind,
    /// When the pending frame is due, in mpv's clock (nanoseconds).
    due_ns: Option<i64>,
    /// A new frame went into the texture since the last present.
    fresh: bool,
}

/// A stream a render worker on the main GPU draws: its shared textures.
struct Linked {
    /// `worker::Shared::index` of that worker.
    worker: usize,
    generation: u64,
    views: Vec<ID3D11ShaderResourceView>,
    /// The newest frame (texture, worker fence value) not yet waited for.
    ready: Option<(usize, u64)>,
    /// The texture the next present samples.
    shown: Option<usize>,
    size: (u32, u32),
}

/// A stream drawn by another GPU: its newest frame, uploaded.
struct Remote {
    tex: ID3D11Texture2D,
    view: ID3D11ShaderResourceView,
    size: (u32, u32),
}

/// Frames are drawn this long before their display time: the next present
/// (at most one refresh later) then shows them on time.
const HEADROOM_NS: i64 = 1_500_000;
/// A due time further away than this is not trusted: draw at once.
const MAX_WAIT_NS: i64 = 1_000_000_000;

/// Nanoseconds to wait before drawing a frame due at `due`.
pub(crate) fn wait_ns(due: Option<i64>, now: i64) -> i64 {
    match due {
        Some(d) => {
            let w = d - HEADROOM_NS - now;
            if w > MAX_WAIT_NS { 0 } else { w }
        }
        None => 0,
    }
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
    remotes: HashMap<u64, Remote>,
    linked: HashMap<u64, Linked>,
    /// Render workers' fences, by worker index.
    worker_fences: HashMap<usize, ID3D11Fence>,
    /// This thread's fence: signalled after each present, so workers know
    /// when a texture is free again.
    fence: Option<(ID3D11DeviceContext4, ID3D11Fence)>,
    fence_value: u64,
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
            D3D11CreateDevice(
                None,
                driver,
                HMODULE::default(),
                flags,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
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
    fn new(hwnd: HWND, size: (u32, u32), shared: Arc<Shared>) -> Result<(Self, DeviceInfo, Option<usize>), String> {
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
            // A fence render workers can open (Windows 10 1703+, WDDM 2.2+);
            // without it the render thread draws every stream itself.
            let fence = (|| -> windows::core::Result<(ID3D11DeviceContext4, ID3D11Fence, usize)> {
                let device5: ID3D11Device5 = device.cast()?;
                let mut fence: Option<ID3D11Fence> = None;
                device5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence)?;
                let fence = fence.ok_or_else(windows::core::Error::empty)?;
                let handle = fence.CreateSharedHandle(None, GENERIC_ALL.0, PCWSTR::null())?;
                Ok((context.cast()?, fence, handle.0 as usize))
            })();
            let (fence, fence_handle) = match fence {
                Ok((c, f, h)) => (Some((c, f)), Some(h)),
                Err(e) => {
                    log::warn!("no shared GPU fence: {e}");
                    (None, None)
                }
            };
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
                    remotes: HashMap::new(),
                    linked: HashMap::new(),
                    worker_fences: HashMap::new(),
                    fence,
                    fence_value: 0,
                    anims: HashMap::new(),
                    visible_anims: Vec::new(),
                    clear: [0.04, 0.04, 0.05, 1.0],
                    epoch: Instant::now(),
                    shared,
                },
                DeviceInfo {
                    adapter: adapter_name,
                    software,
                    luid: (desc.AdapterLuid.LowPart, desc.AdapterLuid.HighPart),
                },
                fence_handle,
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

    /// Time until the earliest stream frame is due.
    fn video_deadline(&mut self) -> Option<Duration> {
        let mut best: Option<i64> = None;
        for s in self.surfaces.values_mut() {
            if s.pending && s.due_ns.is_some() {
                let wait = wait_ns(s.due_ns, s.render.time_ns());
                best = Some(best.map_or(wait, |b| b.min(wait)));
            }
        }
        best.map(|ns| Duration::from_nanos(ns.max(0) as u64))
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
                    let mut deadline = match (self.anim_deadline(), self.video_deadline()) {
                        (Some(a), Some(v)) => Some(a.min(v)),
                        (a, v) => a.or(v),
                    };
                    // Worker frames the GPU has not finished yet: look again
                    // shortly (never wait on the GPU for them).
                    if self.linked.values().any(|l| l.ready.is_some()) {
                        deadline = Some(deadline.map_or(READY_POLL, |d| d.min(READY_POLL)));
                    }
                    match deadline {
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
            for (id, done) in req.remove {
                self.surfaces.remove(&id);
                self.remotes.remove(&id);
                self.linked.remove(&id);
                if let Ok(mut t) = stats.tiles.lock() {
                    t.remove(&id);
                }
                if let Some(tx) = done {
                    let _ = tx.send(());
                }
                draw = true;
            }
            if req.resend_sizes {
                self.send_aux_sizes();
            }
            for (id, mpv, ready) in req.add {
                self.add_surface(id, &mpv);
                if let Some(tx) = ready {
                    let _ = tx.send(());
                }
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
                if let Err(e) =
                    self.egui.upload(&self.device, &self.context, &ui.primitives, ui.pixels_per_point, self.size)
                {
                    log::error!("ui upload: {e}");
                }
                self.videos = ui.videos;
                self.send_aux_sizes();
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
            // 3. Streams: render every frame that is due into its tile
            //    texture; frames due later wait for their time.
            let poked = shared.video_dirty.swap(false, Ordering::AcqRel);
            if self.render_streams(stats, poked) {
                draw = true;
            }
            if poked && self.collect_workers(stats) {
                draw = true;
            }
            if self.promote_ready() {
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

    /// Tells the render workers the tile sizes (they render at tile size).
    fn send_aux_sizes(&self) {
        let auxes = self.shared.workers.lock().map(|a| a.clone()).unwrap_or_default();
        for aux in auxes {
            if let Ok(mut sizes) = aux.sizes.lock() {
                sizes.clear();
                sizes.extend(self.videos.iter().map(|v| (v.id, v.size())));
            }
        }
    }

    /// Opens a worker's new shared textures.
    fn open_ring(&self, ring: &crate::worker::Ring) -> windows::core::Result<Vec<ID3D11ShaderResourceView>> {
        let mut views = Vec::with_capacity(RING);
        for h in ring.handles {
            // SAFETY: a shared handle of a texture on this GPU (legacy, or
            // NT from the bridge; the ring closes those).
            unsafe {
                let tex: ID3D11Texture2D = if ring.nt {
                    self.device.cast::<ID3D11Device1>()?.OpenSharedResource1(HANDLE(h as *mut _))?
                } else {
                    let mut tex: Option<ID3D11Texture2D> = None;
                    self.device.OpenSharedResource(HANDLE(h as *mut _), &mut tex)?;
                    tex.ok_or_else(windows::core::Error::empty)?
                };
                let mut v = None;
                self.device.CreateShaderResourceView(&tex, None, Some(&mut v))?;
                views.push(v.ok_or_else(windows::core::Error::empty)?);
            }
        }
        Ok(views)
    }

    /// Takes what the render workers finished: frames in shared textures
    /// (main GPU) and read-back frames to upload (other GPUs). True if a
    /// visible one changed.
    /// Shows the newest worker frames whose GPU work is finished. A frame
    /// still in flight (its decoder or GPU is busy) waits and the previous
    /// one stays on screen: one slow stream never holds back the present
    /// of all the others. True if a visible tile changed.
    fn promote_ready(&mut self) -> bool {
        let mut changed = false;
        for (id, l) in self.linked.iter_mut() {
            let Some((i, value)) = l.ready else { continue };
            let Some(f) = self.worker_fences.get(&l.worker) else { continue };
            // SAFETY: reading a fence value.
            if unsafe { f.GetCompletedValue() } >= value {
                l.ready = None;
                l.shown = Some(i);
                changed |= self.videos.iter().any(|v| v.id == *id);
            }
        }
        changed
    }

    fn collect_workers(&mut self, stats: &Stats) -> bool {
        let workers = self.shared.workers.lock().map(|a| a.clone()).unwrap_or_default();
        let mut frames = Vec::new();
        let mut changed = false;
        for w in &workers {
            if !self.worker_fences.contains_key(&w.index)
                && let Some(h) = w.fence.lock().ok().and_then(|mut f| f.take())
            {
                // SAFETY: the worker's fence NT handle, closed after opening.
                unsafe {
                    let handle = HANDLE(h as *mut _);
                    let opened = self.device.cast::<ID3D11Device5>().and_then(|d| {
                        let mut f: Option<ID3D11Fence> = None;
                        d.OpenSharedFence(handle, &mut f)?;
                        f.ok_or_else(windows::core::Error::empty)
                    });
                    match opened {
                        Ok(f) => {
                            self.worker_fences.insert(w.index, f);
                        }
                        Err(e) => log::error!("render worker {} fence: {e}", w.index),
                    }
                    let _ = CloseHandle(handle);
                }
            }
            let rings = w.rings.lock().map(|mut r| std::mem::take(&mut *r)).unwrap_or_default();
            for (id, ring) in rings {
                match self.open_ring(&ring) {
                    Ok(views) => {
                        let l = Linked {
                            worker: w.index,
                            generation: ring.generation,
                            views,
                            ready: None,
                            shown: None,
                            size: ring.size,
                        };
                        self.linked.insert(id, l);
                        if let Ok(mut t) = stats.tiles.lock() {
                            (t.entry(id).or_default().width, t.entry(id).or_default().height) = ring.size;
                        }
                    }
                    Err(e) => log::error!("stream {id}: shared textures: {e}"),
                }
            }
            let published = w.published.lock().map(|mut p| std::mem::take(&mut *p)).unwrap_or_default();
            for (id, p) in published {
                let Some(l) = self.linked.get_mut(&id) else { continue };
                if l.generation != p.generation {
                    continue;
                }
                l.ready = Some((p.index, p.value));
                let visible = self.videos.iter().any(|v| v.id == id);
                if !p.redraw {
                    stats.video_frames.fetch_add(1, Ordering::Relaxed);
                }
                if p.copied {
                    stats.copied_frames.fetch_add(1, Ordering::Relaxed);
                    stats.gpu_copied_frames.fetch_add(1, Ordering::Relaxed);
                    stats.copied_bytes.fetch_add(u64::from(l.size.0 * l.size.1 * 4), Ordering::Relaxed);
                }
                if let Ok(mut t) = stats.tiles.lock() {
                    let e = t.entry(id).or_default();
                    if p.redraw {
                        e.redraws += 1;
                    } else {
                        e.frames += 1;
                    }
                    if p.copied {
                        e.copied += 1;
                        e.gpu_copied += 1;
                    }
                    e.visible = visible;
                }
                changed |= visible;
            }
            let repeats = w.repeats.lock().map(|mut r| std::mem::take(&mut *r)).unwrap_or_default();
            if !repeats.is_empty()
                && let Ok(mut t) = stats.tiles.lock()
            {
                for (id, n) in repeats {
                    t.entry(id).or_default().repeats += n;
                }
            }
            if let Ok(mut f) = w.frames.lock()
                && !f.is_empty()
            {
                frames.extend(std::mem::take(&mut *f));
            }
        }
        for (id, f) in frames {
            // A stream whose GPU-to-GPU copy failed sends read-back frames.
            self.linked.remove(&id);
            let t = Instant::now();
            let size = (f.width, f.height);
            if f.rgba.len() != (f.width * f.height * 4) as usize {
                continue;
            }
            if self.remotes.get(&id).map(|r| r.size) != Some(size) {
                match create_target(&self.device, size) {
                    Ok((tex, view)) => {
                        self.remotes.insert(id, Remote { tex, view, size });
                    }
                    Err(e) => {
                        log::error!("second-GPU frame texture {size:?}: {e}");
                        continue;
                    }
                }
            }
            let Some(r) = self.remotes.get(&id) else { continue };
            // SAFETY: width x height RGBA8 rows into a texture of that size.
            unsafe { self.context.UpdateSubresource(&r.tex, 0, None, f.rgba.as_ptr().cast(), f.width * 4, 0) };
            let visible = self.videos.iter().any(|v| v.id == id);
            if !f.redraw {
                stats.video_frames.fetch_add(1, Ordering::Relaxed);
            }
            stats.copied_frames.fetch_add(1, Ordering::Relaxed);
            stats.copied_bytes.fetch_add(f.rgba.len() as u64, Ordering::Relaxed);
            let us = u64::from(f.readback_us) + t.elapsed().as_micros() as u64;
            stats.copy_us.fetch_add(us, Ordering::Relaxed);
            if let Ok(mut t) = stats.tiles.lock() {
                let e = t.entry(id).or_default();
                if f.redraw {
                    e.redraws += 1;
                } else {
                    e.frames += 1;
                }
                e.copied += 1;
                e.copy_us += us;
                (e.width, e.height) = size;
                e.visible = visible;
            }
            changed |= visible;
        }
        changed
    }

    fn add_surface(&mut self, id: u64, mpv: &Mpv) {
        self.remotes.remove(&id);
        self.linked.remove(&id);
        // SAFETY: our device was created without SINGLETHREADED.
        match unsafe { RenderContext::new_d3d11(mpv, self.device.as_raw()) } {
            Ok(mut render) => {
                let shared = self.shared.clone();
                render.set_update_callback(move || shared.poke_video());
                self.surfaces.insert(
                    id,
                    Surface {
                        render,
                        tex: None,
                        has_frame: false,
                        pending: true,
                        pending_kind: FrameKind::None,
                        due_ns: None,
                        fresh: false,
                    },
                );
                self.shared.video_dirty.store(true, Ordering::Release);
            }
            Err(e) => log::error!("stream {id}: render context failed: {e}"),
        }
    }

    /// Renders the stream frames that are due. True if anything visible
    /// changed. `poked`: an mpv update callback fired since the last call.
    fn render_streams(&mut self, stats: &Stats, poked: bool) -> bool {
        let mut changed = false;
        let videos = self.videos.clone();
        for (id, s) in self.surfaces.iter_mut() {
            if poked && RenderContext::frame_ready(s.render.update()) {
                let next = s.render.next_frame();
                s.pending = true;
                s.pending_kind = next.kind;
                s.due_ns = next.due_ns;
            }
            let tile = videos.iter().find(|v| v.id == *id);
            // Size the texture to the tile; hidden streams get a tiny one
            // so mpv's frame queue keeps moving at almost no cost.
            let want = tile.map(|t| t.size()).unwrap_or((16, 16));
            let resized = s.tex.as_ref().map(|t| t.2) != Some(want);
            let due = s.pending && wait_ns(s.due_ns, s.render.time_ns()) <= 0;
            if !due && !(resized && s.has_frame) {
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
            if let Some((tex, _, size)) = &s.tex {
                // SAFETY: a render-target texture on mpv's device.
                match unsafe { s.render.render_d3d11(tex.as_raw(), false) } {
                    Ok(()) => {
                        s.has_frame = true;
                        let kind = if due { s.pending_kind } else { FrameKind::Redraw };
                        if due {
                            s.pending = false;
                            s.due_ns = None;
                        }
                        // Only a new picture counts as a shown frame; a repeat
                        // changes nothing on screen and needs no present.
                        let new_picture = matches!(kind, FrameKind::New | FrameKind::None);
                        if new_picture {
                            s.fresh = true;
                            stats.video_frames.fetch_add(1, Ordering::Relaxed);
                        }
                        if let Ok(mut t) = stats.tiles.lock() {
                            let e = t.entry(*id).or_default();
                            match kind {
                                FrameKind::Repeat => e.repeats += 1,
                                FrameKind::Redraw => e.redraws += 1,
                                _ => e.frames += 1,
                            }
                            (e.width, e.height) = *size;
                            e.visible = tile.is_some();
                        }
                        changed |= tile.is_some() && kind != FrameKind::Repeat;
                    }
                    Err(e) => log::warn!("stream {id}: render: {e}"),
                }
            }
        }
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
            let remotes = &self.remotes;
            let linked = &self.linked;
            let anims = &self.anims;
            let lookup = |u: u64| -> Option<ID3D11ShaderResourceView> {
                if u & ANIM_BIT != 0 {
                    let a = anims.get(&u)?;
                    return Some(a.frames.get(a.index_at(now))?.0.clone());
                }
                if let Some(l) = linked.get(&u) {
                    return l.shown.and_then(|i| l.views.get(i).cloned());
                }
                if let Some(r) = remotes.get(&u) {
                    return Some(r.view.clone());
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
            // Once this frame's GPU work is done, its worker textures are free.
            if let Some((ctx4, fence)) = &self.fence
                && !self.linked.is_empty()
            {
                self.fence_value += 1;
                let _ = ctx4.Signal(fence, self.fence_value);
                let workers = self.shared.workers.lock().map(|a| a.clone()).unwrap_or_default();
                for w in workers {
                    let Ok(mut released) = w.released.lock() else { continue };
                    for (id, l) in self.linked.iter().filter(|(_, l)| l.worker == w.index) {
                        let Some(i) = l.shown else { continue };
                        let entry = released.entry(*id).or_insert((l.generation, [0; RING]));
                        if entry.0 != l.generation {
                            *entry = (l.generation, [0; RING]);
                        }
                        entry.1[i] = self.fence_value;
                    }
                }
            }
            // Only streams whose new frame this present showed.
            for s in self.surfaces.values_mut() {
                if std::mem::take(&mut s.fresh) {
                    s.render.report_swap();
                }
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
    into.resend_sizes |= more.resend_sizes;
    if more.snapshot.is_some() {
        into.snapshot = more.snapshot.take();
    }
    into.quit |= more.quit;
}

fn premultiply(rgba: &[u8]) -> Vec<u8> {
    let mut out = rgba.to_vec();
    for px in out.as_chunks_mut::<4>().0 {
        let a = u16::from(px[3]);
        for c in &mut px[..3] {
            *c = ((u16::from(*c) * a + 127) / 255) as u8;
        }
    }
    out
}

/// A texture mpv renders into and egui samples from.
fn create_target(
    device: &ID3D11Device,
    (w, h): (u32, u32),
) -> windows::core::Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
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
fn write_snapshot(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    tex: &ID3D11Texture2D,
    path: &std::path::Path,
) -> Result<(), String> {
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
            let row = std::slice::from_raw_parts(
                (m.pData as *const u8).add(y * m.RowPitch as usize),
                desc.Width as usize * 4,
            );
            for px in row.as_chunks::<4>().0 {
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
    fn frame_waits_are_bounded() {
        let now = 10_000_000_000;
        assert_eq!(wait_ns(None, now), 0);
        assert_eq!(wait_ns(Some(now + 11_500_000), now), 10_000_000);
        assert!(wait_ns(Some(now - 5), now) < 0);
        // A wildly wrong clock (a unit mix-up) never stalls the video.
        assert_eq!(wait_ns(Some(now * 1000), now), 0);
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
