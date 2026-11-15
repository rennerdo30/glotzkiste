//! Other GPUs: their video decoders take streams before the main GPU's
//! (which also serves the browser, games and the desktop).
//!
//! mpv cannot send copy-back decoding to a chosen adapter, so Glotzkiste
//! gives each other GPU its own D3D11 device and render thread: a stream's
//! mpv render context lives on that device, so the stream decodes (zero-copy
//! there) and scales to its tile size on that GPU. Each finished tile-sized
//! frame is read back and handed to the main render thread, which uploads
//! it: one copy per frame at tile size (a 495x328 tile is 0.65 MB), the price
//! of using another decoder.

use crate::d3d::wait_ns;
use gk_mpv::{FrameKind, Mpv, RenderContext};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

/// A finished frame of a moved stream (RGBA8 rows, tightly packed).
pub(crate) struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Microseconds spent reading it back.
    pub readback_us: u32,
}

type Reply = mpsc::Sender<Result<(), String>>;

#[derive(Default)]
struct Requests {
    add: Vec<(u64, Mpv, Reply)>,
    remove: Vec<(u64, Option<Reply>)>,
    quit: bool,
}

pub(crate) struct Shared {
    requests: Mutex<Requests>,
    wake: Condvar,
    dirty: AtomicBool,
    /// Newest frame per stream, taken by the main render thread.
    pub frames: Mutex<HashMap<u64, Frame>>,
    /// Tile size per stream, set by the main render thread.
    pub sizes: Mutex<HashMap<u64, (u32, u32)>>,
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

/// The second GPU's render thread.
pub(crate) struct Aux {
    pub shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Aux {
    /// Starts a render thread on the adapter with `luid`. `frame_ready`
    /// wakes the main render thread.
    pub fn start(luid: (u32, i32), frame_ready: Arc<dyn Fn() + Send + Sync>) -> Result<Self, String> {
        let shared = Arc::new(Shared {
            requests: Mutex::new(Requests::default()),
            wake: Condvar::new(),
            dirty: AtomicBool::new(false),
            frames: Mutex::new(HashMap::new()),
            sizes: Mutex::new(HashMap::new()),
        });
        let (tx, rx) = mpsc::channel();
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name("gk-render-aux".into())
            .spawn(move || match device_on(luid) {
                Ok((device, context)) => {
                    let _ = tx.send(Ok(()));
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run(&s, &device, &context, &*frame_ready)
                    }));
                    if r.is_err() {
                        log::error!("second-GPU render thread panicked");
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            })
            .map_err(|e| e.to_string())?;
        rx.recv().map_err(|_| "the second-GPU thread stopped during start".to_owned())??;
        Ok(Self { shared, thread: Some(thread) })
    }

    pub fn add(&self, id: u64, mpv: Mpv, reply: Reply) {
        self.shared.update(|r| r.add.push((id, mpv, reply)));
    }

    pub fn remove(&self, id: u64, reply: Option<Reply>) {
        self.shared.update(|r| r.remove.push((id, reply)));
    }
}

impl Drop for Aux {
    fn drop(&mut self) {
        self.shared.update(|r| r.quit = true);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
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

/// A stream's tile-sized render target and two CPU-readable copies of it:
/// a copy is read once that GPU finished it (never waiting for it), while
/// the next frame can already go into the other.
struct Target {
    tex: ID3D11Texture2D,
    staging: [ID3D11Texture2D; 2],
    size: (u32, u32),
    /// Copies issued and not read yet, oldest first (staging index).
    inflight: VecDeque<usize>,
    next: usize,
}

struct Surface {
    render: RenderContext,
    target: Option<Target>,
    pending: bool,
    pending_kind: FrameKind,
    due_ns: Option<i64>,
}

fn target(device: &ID3D11Device, (w, h): (u32, u32)) -> windows::core::Result<Target> {
    let mut desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        ..Default::default()
    };
    let make = |desc: &D3D11_TEXTURE2D_DESC| -> windows::core::Result<ID3D11Texture2D> {
        let mut t = None;
        // SAFETY: a valid descriptor and out pointer.
        unsafe { device.CreateTexture2D(desc, None, Some(&mut t))? };
        t.ok_or_else(windows::core::Error::empty)
    };
    let tex = make(&desc)?;
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    let staging = [make(&desc)?, make(&desc)?];
    Ok(Target { tex, staging, size: (w, h), inflight: VecDeque::new(), next: 0 })
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
    frame_ready: &(dyn Fn() + Send + Sync),
) {
    let mut surfaces: HashMap<u64, Surface> = HashMap::new();
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
                if surfaces.values().any(|s| s.target.as_ref().is_some_and(|t| !t.inflight.is_empty())) {
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
            surfaces.remove(&id);
            if let Ok(mut f) = shared.frames.lock() {
                f.remove(&id);
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
                    shared.dirty.store(true, Ordering::Release);
                    let _ = reply.send(Ok(()));
                }
                Err(e) => {
                    let _ = reply.send(Err(format!("render context on that GPU: {e}")));
                }
            }
        }
        let mut any = false;
        // 1. Copies that GPU finished: hand the newest of each stream over.
        for (id, s) in surfaces.iter_mut() {
            let Some(t) = s.target.as_mut() else { continue };
            let mut newest = None;
            while let Some(&i) = t.inflight.front() {
                let started = std::time::Instant::now();
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
                f.insert(*id, Frame { width: t.size.0, height: t.size.1, rgba, readback_us });
                any = true;
            }
        }
        // 2. Frames that are due: render at tile size, start the copy.
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
            if s.target.as_ref().map(|t| t.size) != Some(want) {
                match target(device, want) {
                    Ok(t) => s.target = Some(t),
                    Err(e) => {
                        log::error!("stream texture {want:?} on another GPU: {e}");
                        continue;
                    }
                }
            }
            let Some(t) = s.target.as_mut() else { continue };
            // SAFETY: a render-target texture on mpv's device.
            if let Err(e) = unsafe { s.render.render_d3d11(t.tex.as_raw(), false) } {
                log::warn!("stream {id} on another GPU: {e}");
                continue;
            }
            s.render.report_swap();
            if s.pending_kind == FrameKind::Repeat {
                continue;
            }
            // An unread older copy in this slot is superseded by this frame.
            let i = t.next;
            t.inflight.retain(|j| *j != i);
            // SAFETY: both textures are on this device with the same size and
            // format; Flush starts the GPU work now.
            unsafe {
                context.CopyResource(&t.staging[i], &t.tex);
                context.Flush();
            }
            t.inflight.push_back(i);
            t.next ^= 1;
        }
        if any {
            frame_ready();
        }
    }
}
