//! Render workers: threads with their own D3D11 device that run mpv's
//! per-frame work for a share of the streams, so it spreads over several
//! cores instead of piling up on the thread that presents.
//!
//! - **On the main GPU** (`Delivery::Shared`): a stream renders at tile size
//!   into a ring of textures the main device opens as well (same GPU: no
//!   copy at all). GPU fences order the work: the worker signals when a
//!   frame is drawn, the main thread makes its GPU wait for that before
//!   sampling, and signals when a present no longer needs a texture, which
//!   the worker's GPU waits for before drawing into it again. No thread ever
//!   waits on the CPU.
//! - **On another GPU** (`Delivery::Readback`): mpv cannot send copy-back
//!   decoding to a chosen adapter, so the worker's device lives on that GPU;
//!   the stream decodes and scales there and each finished tile-sized frame
//!   is read back (without waiting for that GPU) and uploaded by the main
//!   thread: one copy per frame at tile size, the price of another decoder.

use crate::d3d::wait_ns;
use gk_mpv::{FrameKind, Mpv, RenderContext};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE, HMODULE};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{Interface, PCWSTR};

/// Shared textures per stream on the main GPU.
pub(crate) const RING: usize = 3;

/// How a worker hands its frames to the main render thread.
#[derive(Clone, Copy)]
pub(crate) enum Delivery {
    /// Same GPU: shared textures, ordered by the main thread's fence (an NT
    /// handle the worker opens).
    Shared { main_fence: usize },
    /// Another GPU: read back, uploaded by the main thread.
    Readback,
}

/// A finished frame read back from another GPU (RGBA8 rows, tightly packed).
pub(crate) struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Microseconds spent reading it back.
    pub readback_us: u32,
    /// The same picture drawn again (mpv asked for a redraw).
    pub redraw: bool,
}

/// New shared textures of a stream (legacy shared handles; they need no
/// closing).
pub(crate) struct Ring {
    pub generation: u64,
    pub size: (u32, u32),
    pub handles: [usize; RING],
}

/// The newest frame drawn into a shared texture: ready on the GPU once the
/// worker's fence reaches `value`.
#[derive(Clone, Copy)]
pub(crate) struct Published {
    pub generation: u64,
    pub index: usize,
    pub value: u64,
    /// The same picture drawn again (mpv asked for a redraw).
    pub redraw: bool,
}

type Reply = mpsc::Sender<Result<(), String>>;

#[derive(Default)]
struct Requests {
    add: Vec<(u64, Mpv, Reply)>,
    remove: Vec<(u64, Option<Reply>)>,
    quit: bool,
}

pub(crate) struct Shared {
    /// Unique per worker (the main thread keys the worker's fence by it).
    pub index: usize,
    requests: Mutex<Requests>,
    wake: Condvar,
    dirty: AtomicBool,
    /// Streams on this worker (placement picks the least loaded).
    pub streams: AtomicUsize,
    /// Tile size per stream, set by the main render thread.
    pub sizes: Mutex<HashMap<u64, (u32, u32)>>,
    /// Readback: the newest frame per stream.
    pub frames: Mutex<HashMap<u64, Frame>>,
    /// Shared: new texture rings for the main thread to open.
    pub rings: Mutex<HashMap<u64, Ring>>,
    /// Shared: the newest drawn frame per stream.
    pub published: Mutex<HashMap<u64, Published>>,
    /// Shared: per stream and ring generation, the main fence value after
    /// which each texture is no longer sampled.
    pub released: Mutex<HashMap<u64, (u64, [u64; RING])>>,
    /// Shared: this worker's fence as an NT handle, taken (and closed) by
    /// the main thread.
    pub fence: Mutex<Option<usize>>,
    /// Repeats per stream since the main thread last took them (mpv asked
    /// for a render that showed nothing new; not handed over).
    pub repeats: Mutex<HashMap<u64, u64>>,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut Requests)) {
        let mut r = self.requests.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut r);
        self.wake.notify_one();
    }
    fn poke(&self) {
        self.dirty.store(true, Ordering::Release);
        let _g = self.requests.lock().unwrap_or_else(|p| p.into_inner());
        self.wake.notify_one();
    }
}

static NEXT_INDEX: AtomicUsize = AtomicUsize::new(0);

/// A render worker thread.
pub(crate) struct Worker {
    pub shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    /// Starts a worker with a device on the adapter with `luid`.
    /// `frame_ready` wakes the main render thread.
    pub fn start(
        luid: (u32, i32),
        delivery: Delivery,
        frame_ready: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, String> {
        let index = NEXT_INDEX.fetch_add(1, Ordering::Relaxed);
        let shared = Arc::new(Shared {
            index,
            requests: Mutex::new(Requests::default()),
            wake: Condvar::new(),
            dirty: AtomicBool::new(false),
            streams: AtomicUsize::new(0),
            sizes: Mutex::new(HashMap::new()),
            frames: Mutex::new(HashMap::new()),
            rings: Mutex::new(HashMap::new()),
            published: Mutex::new(HashMap::new()),
            released: Mutex::new(HashMap::new()),
            fence: Mutex::new(None),
            repeats: Mutex::new(HashMap::new()),
        });
        let (tx, rx) = mpsc::channel();
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name(format!("gk-render-{index}"))
            .spawn(move || {
                let setup = device_on(luid).and_then(|(device, context)| {
                    let fences = match delivery {
                        Delivery::Shared { main_fence } => Some(open_fences(&device, &context, main_fence, &s)?),
                        Delivery::Readback => None,
                    };
                    Ok((device, context, fences))
                });
                match setup {
                    Ok((device, context, fences)) => {
                        let _ = tx.send(Ok(()));
                        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            run(&s, &device, &context, fences, &*frame_ready)
                        }));
                        if r.is_err() {
                            log::error!("render worker {index} panicked");
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        rx.recv().map_err(|_| "the render worker stopped during start".to_owned())??;
        Ok(Self { shared, thread: Some(thread) })
    }

    pub fn add(&self, id: u64, mpv: Mpv, reply: Reply) {
        self.shared.update(|r| r.add.push((id, mpv, reply)));
    }

    pub fn remove(&self, id: u64, reply: Option<Reply>) {
        self.shared.update(|r| r.remove.push((id, reply)));
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.shared.update(|r| r.quit = true);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Ok(mut f) = self.shared.fence.lock()
            && let Some(h) = f.take()
        {
            // SAFETY: an NT handle this worker created and nobody took.
            unsafe {
                let _ = CloseHandle(HANDLE(h as *mut _));
            }
        }
    }
}

/// A device with video support on the adapter with `luid`.
fn device_on(luid: (u32, i32)) -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let e = |e: windows::core::Error| e.to_string();
    // SAFETY: COM calls with valid out values.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(e)?;
        let mut i = 0;
        let adapter = loop {
            let a = factory.EnumAdapters1(i).map_err(|_| "that GPU is not present".to_owned())?;
            i += 1;
            let d = a.GetDesc1().map_err(e)?;
            if (d.AdapterLuid.LowPart, d.AdapterLuid.HighPart) == luid {
                break a;
            }
        };
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let (mut device, mut context) = (None, None);
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .map_err(e)?;
        let device = device.ok_or("no device")?;
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
        Ok((device, context.ok_or("no context")?))
    }
}

/// This worker's fences: its own (shared with the main thread through
/// `Shared::fence`) and the main thread's.
struct Fences {
    context: ID3D11DeviceContext4,
    own: ID3D11Fence,
    main: ID3D11Fence,
    value: u64,
}

fn open_fences(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    main_fence: usize,
    shared: &Shared,
) -> Result<Fences, String> {
    let e = |e: windows::core::Error| format!("GPU fences: {e}");
    // SAFETY: COM calls on this worker's device; the main fence handle stays
    // open in the presenter while workers run.
    unsafe {
        let device5: ID3D11Device5 = device.cast().map_err(e)?;
        let mut own: Option<ID3D11Fence> = None;
        device5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut own).map_err(e)?;
        let own = own.ok_or("no fence")?;
        let handle = own.CreateSharedHandle(None, GENERIC_ALL.0, PCWSTR::null()).map_err(e)?;
        let mut main: Option<ID3D11Fence> = None;
        device5.OpenSharedFence(HANDLE(main_fence as *mut _), &mut main).map_err(e)?;
        let main = main.ok_or("no main fence")?;
        if let Ok(mut f) = shared.fence.lock() {
            *f = Some(handle.0 as usize);
        }
        Ok(Fences { context: context.cast().map_err(e)?, own, main, value: 0 })
    }
}

/// A stream's targets on another GPU: the tile-sized render target and two
/// CPU-readable copies, read once that GPU finished them.
struct ReadbackTarget {
    tex: ID3D11Texture2D,
    staging: [ID3D11Texture2D; 2],
    size: (u32, u32),
    /// Copies issued and not read yet, oldest first (staging index).
    inflight: VecDeque<usize>,
    next: usize,
    /// The newest copy is a redraw of the same picture.
    redraw: bool,
}

/// A stream's shared textures on the main GPU.
struct RingTarget {
    tex: [ID3D11Texture2D; RING],
    size: (u32, u32),
    generation: u64,
    next: usize,
}

enum Target {
    Readback(ReadbackTarget),
    Ring(RingTarget),
}

impl Target {
    fn size(&self) -> (u32, u32) {
        match self {
            Target::Readback(t) => t.size,
            Target::Ring(t) => t.size,
        }
    }
}

struct Surface {
    render: RenderContext,
    target: Option<Target>,
    pending: bool,
    pending_kind: FrameKind,
    due_ns: Option<i64>,
}

fn texture(device: &ID3D11Device, desc: &D3D11_TEXTURE2D_DESC) -> windows::core::Result<ID3D11Texture2D> {
    let mut t = None;
    // SAFETY: a valid descriptor and out pointer.
    unsafe { device.CreateTexture2D(desc, None, Some(&mut t))? };
    t.ok_or_else(windows::core::Error::empty)
}

fn base_desc((w, h): (u32, u32)) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        ..Default::default()
    }
}

fn readback_target(device: &ID3D11Device, size: (u32, u32)) -> windows::core::Result<ReadbackTarget> {
    let mut desc = base_desc(size);
    let tex = texture(device, &desc)?;
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    let staging = [texture(device, &desc)?, texture(device, &desc)?];
    Ok(ReadbackTarget { tex, staging, size, inflight: VecDeque::new(), next: 0, redraw: false })
}

fn ring_target(device: &ID3D11Device, size: (u32, u32), generation: u64) -> windows::core::Result<(RingTarget, Ring)> {
    let mut desc = base_desc(size);
    desc.BindFlags = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
    desc.MiscFlags = D3D11_RESOURCE_MISC_SHARED.0 as u32;
    let tex = [texture(device, &desc)?, texture(device, &desc)?, texture(device, &desc)?];
    let mut handles = [0usize; RING];
    for (h, t) in handles.iter_mut().zip(&tex) {
        // SAFETY: a texture created with MISC_SHARED.
        *h = unsafe { t.cast::<IDXGIResource>()?.GetSharedHandle()? }.0 as usize;
    }
    Ok((RingTarget { tex, size, generation, next: 0 }, Ring { generation, size, handles }))
}

/// Reads a finished copy: tightly packed RGBA rows, or `None` while that
/// GPU is still working on it.
fn read(context: &ID3D11DeviceContext, staging: &ID3D11Texture2D, (w, h): (u32, u32)) -> Option<Vec<u8>> {
    let row = (w * 4) as usize;
    let mut rgba = vec![0u8; row * h as usize];
    // SAFETY: the staging texture is w x h RGBA8; mapped for read without
    // waiting, and unmapped before the data is used.
    unsafe {
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(staging, 0, D3D11_MAP_READ, D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32, Some(&mut m)).ok()?;
        for y in 0..h as usize {
            let src = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize), row);
            rgba[y * row..(y + 1) * row].copy_from_slice(src);
        }
        context.Unmap(staging, 0);
    }
    Some(rgba)
}

/// While copies are in flight, look for finished ones this often.
const POLL_NS: i64 = 1_000_000;

fn run(
    shared: &Arc<Shared>,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    mut fences: Option<Fences>,
    frame_ready: &(dyn Fn() + Send + Sync),
) {
    let mut surfaces: HashMap<u64, Surface> = HashMap::new();
    let mut generation = 0u64;
    loop {
        // Sleep until a request, an mpv update, the next frame's due time or
        // (with copies in flight) the next look for finished ones.
        let req = {
            let mut r = shared.requests.lock().unwrap_or_else(|p| p.into_inner());
            loop {
                if r.quit || !r.add.is_empty() || !r.remove.is_empty() || shared.dirty.load(Ordering::Acquire) {
                    break;
                }
                let mut next = surfaces
                    .values()
                    .filter(|s| s.pending && s.due_ns.is_some())
                    .map(|s| wait_ns(s.due_ns, s.render.time_ns()))
                    .min();
                let polling =
                    surfaces.values().any(|s| matches!(&s.target, Some(Target::Readback(t)) if !t.inflight.is_empty()));
                if polling {
                    next = Some(next.map_or(POLL_NS, |n| n.min(POLL_NS)));
                }
                match next {
                    Some(ns) if ns <= 0 => break,
                    Some(ns) => {
                        let (g, t) = shared
                            .wake
                            .wait_timeout(r, Duration::from_nanos(ns as u64))
                            .unwrap_or_else(|p| p.into_inner());
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
            return;
        }
        for (id, reply) in req.remove {
            if surfaces.remove(&id).is_some() {
                shared.streams.fetch_sub(1, Ordering::Relaxed);
            }
            if let Ok(mut f) = shared.frames.lock() {
                f.remove(&id);
            }
            if let Ok(mut r) = shared.rings.lock() {
                r.remove(&id);
            }
            if let Ok(mut p) = shared.published.lock() {
                p.remove(&id);
            }
            if let Ok(mut r) = shared.released.lock() {
                r.remove(&id);
            }
            if let Some(r) = reply {
                let _ = r.send(Ok(()));
            }
        }
        for (id, mpv, reply) in req.add {
            // SAFETY: our device was created without SINGLETHREADED.
            match unsafe { RenderContext::new_d3d11(&mpv, device.as_raw()) } {
                Ok(mut render) => {
                    let s = shared.clone();
                    render.set_update_callback(move || s.poke());
                    surfaces.insert(
                        id,
                        Surface { render, target: None, pending: true, pending_kind: FrameKind::None, due_ns: None },
                    );
                    shared.streams.fetch_add(1, Ordering::Relaxed);
                    shared.dirty.store(true, Ordering::Release);
                    let _ = reply.send(Ok(()));
                }
                Err(e) => {
                    let _ = reply.send(Err(format!("render context: {e}")));
                }
            }
        }
        let mut any = false;
        // 1. Readback copies that GPU finished: hand the newest over.
        for (id, s) in surfaces.iter_mut() {
            let Some(Target::Readback(t)) = s.target.as_mut() else { continue };
            let mut newest = None;
            while let Some(&i) = t.inflight.front() {
                let started = Instant::now();
                match read(context, &t.staging[i], t.size) {
                    Some(rgba) => {
                        t.inflight.pop_front();
                        newest = Some((rgba, started.elapsed().as_micros() as u32));
                    }
                    None => break,
                }
            }
            if let Some((rgba, readback_us)) = newest
                && let Ok(mut f) = shared.frames.lock()
            {
                f.insert(*id, Frame { width: t.size.0, height: t.size.1, rgba, readback_us, redraw: t.redraw });
                any = true;
            }
        }
        // 2. Frames that are due: render at tile size and hand over.
        let poked = shared.dirty.swap(false, Ordering::AcqRel);
        let sizes = shared.sizes.lock().map(|s| s.clone()).unwrap_or_default();
        for (id, s) in surfaces.iter_mut() {
            if poked && RenderContext::frame_ready(s.render.update()) {
                let next = s.render.next_frame();
                s.pending = true;
                s.pending_kind = next.kind;
                s.due_ns = next.due_ns;
            }
            if !s.pending || wait_ns(s.due_ns, s.render.time_ns()) > 0 {
                continue;
            }
            s.pending = false;
            s.due_ns = None;
            let want = sizes.get(id).copied().unwrap_or((16, 16));
            if s.target.as_ref().map(Target::size) != Some(want) {
                let made = if fences.is_some() {
                    generation += 1;
                    ring_target(device, want, generation).map(|(t, ring)| {
                        if let Ok(mut r) = shared.rings.lock() {
                            r.insert(*id, ring);
                        }
                        Target::Ring(t)
                    })
                } else {
                    readback_target(device, want).map(Target::Readback)
                };
                match made {
                    Ok(t) => s.target = Some(t),
                    Err(e) => {
                        log::error!("stream texture {want:?} on a render worker: {e}");
                        continue;
                    }
                }
            }
            match s.target.as_mut() {
                Some(Target::Ring(t)) => {
                    let Some(f) = fences.as_mut() else { continue };
                    let i = t.next;
                    let released = shared
                        .released
                        .lock()
                        .ok()
                        .and_then(|r| r.get(id).filter(|(g, _)| *g == t.generation).map(|(_, v)| v[i]))
                        .unwrap_or(0);
                    // SAFETY: fences and textures of this worker's device; the
                    // GPU waits until the main thread no longer samples it.
                    unsafe {
                        if released > 0 {
                            let _ = f.context.Wait(&f.main, released);
                        }
                        if let Err(e) = s.render.render_d3d11(t.tex[i].as_raw(), false) {
                            log::warn!("stream {id} on a render worker: {e}");
                            continue;
                        }
                    }
                    s.render.report_swap();
                    if s.pending_kind == FrameKind::Repeat {
                        if let Ok(mut r) = shared.repeats.lock() {
                            *r.entry(*id).or_default() += 1;
                        }
                        continue;
                    }
                    f.value += 1;
                    // SAFETY: our own fence; Flush starts the GPU work now.
                    unsafe {
                        let _ = f.context.Signal(&f.own, f.value);
                        context.Flush();
                    }
                    if let Ok(mut p) = shared.published.lock() {
                        let redraw = s.pending_kind == FrameKind::Redraw;
                        p.insert(*id, Published { generation: t.generation, index: i, value: f.value, redraw });
                    }
                    t.next = (i + 1) % RING;
                    any = true;
                }
                Some(Target::Readback(t)) => {
                    // SAFETY: a render-target texture on mpv's device.
                    if let Err(e) = unsafe { s.render.render_d3d11(t.tex.as_raw(), false) } {
                        log::warn!("stream {id} on another GPU: {e}");
                        continue;
                    }
                    s.render.report_swap();
                    if s.pending_kind == FrameKind::Repeat {
                        if let Ok(mut r) = shared.repeats.lock() {
                            *r.entry(*id).or_default() += 1;
                        }
                        continue;
                    }
                    t.redraw = s.pending_kind == FrameKind::Redraw;
                    // An unread older copy in this slot is superseded.
                    let i = t.next;
                    t.inflight.retain(|j| *j != i);
                    // SAFETY: same size and format on this device; Flush
                    // starts the GPU work now.
                    unsafe {
                        context.CopyResource(&t.staging[i], &t.tex);
                        context.Flush();
                    }
                    t.inflight.push_back(i);
                    t.next ^= 1;
                }
                None => {}
            }
        }
        if any {
            frame_ready();
        }
    }
}
