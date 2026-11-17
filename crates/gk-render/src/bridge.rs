//! GPU-to-GPU frame copies for streams on another GPU (GK-3).
//!
//! D3D11 cannot open the memory two GPUs share, so a small D3D12 bridge does
//! the copies, per frame:
//!
//! 1. The worker's D3D11 device (on the other GPU) renders the stream into a
//!    texture both of its APIs open, then signals `rendered`.
//! 2. A copy queue on the other GPU waits for that and copies the texture
//!    into a buffer in a cross-adapter heap (memory both GPUs can reach),
//!    then signals `copied_out` (the texture is free again) and `crossed`
//!    (the buffer is filled; a cross-adapter fence).
//! 3. A copy queue on the main GPU waits for `crossed`, and for the main
//!    thread to stop sampling the target texture, copies the buffer into one
//!    of the stream's shared textures there and signals `done`.
//! 4. The main thread treats it like a frame drawn on the main GPU: its GPU
//!    waits for `done` before sampling.
//!
//! No CPU waits in the frame path and no CPU copies. Every fence counts the
//! same per-worker frame number. A command list per ring slot is recorded
//! once and submitted again only after `done` showed the slot's last copy
//! finished. Making a stream's resources takes 10-30 ms, so a builder
//! thread does it while the worker keeps drawing the other streams.

use crate::worker::{RING, Ring};
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{Interface, PCWSTR};

type R<T> = Result<T, String>;

fn err(what: &'static str) -> impl Fn(windows::core::Error) -> String {
    move |e| format!("{what}: {e}")
}

/// Closes an NT handle when dropped.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: a handle we created and nobody else closes.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// A finished build: stream, generation, the targets or why not.
pub(crate) type Made = (u64, u64, R<(Target, Ring)>);

/// A build request: stream, size, generation.
type Job = (u64, (u32, u32), u64);

/// Makes stream targets (on the builder thread; the devices are free
/// threaded).
#[derive(Clone)]
struct Maker {
    other: ID3D12Device,
    main: ID3D12Device,
    device11: ID3D11Device,
    /// A D3D11 device on the main GPU, for the textures the main thread
    /// opens.
    main11: ID3D11Device,
    /// The heap goes on the GPU where creating it worked last time.
    heap_on_main: Arc<AtomicBool>,
}

pub(crate) struct Bridge {
    queue_other: ID3D12CommandQueue,
    queue_main: ID3D12CommandQueue,
    jobs: Option<mpsc::Sender<Job>>,
    made: mpsc::Receiver<Made>,
    builder: Option<JoinHandle<()>>,
    context: ID3D11DeviceContext4,
    /// D3D11 → other copy queue: the frame is drawn.
    rendered11: ID3D11Fence,
    rendered12: ID3D12Fence,
    /// Other copy queue → D3D11: the render texture was read.
    copied_out12: ID3D12Fence,
    copied_out11: ID3D11Fence,
    /// Other copy queue → main copy queue (cross-adapter).
    crossed_other: ID3D12Fence,
    crossed_main: ID3D12Fence,
    /// Main copy queue → main thread: the frame is in the shared texture.
    pub done: ID3D12Fence,
    /// The main thread's fence (opened here): a texture is no longer sampled.
    main_fence: ID3D12Fence,
    pub value: u64,
    event: Owned,
    /// Targets replaced or removed, kept until `done` passes their last use.
    retired: Vec<(u64, Target)>,
}

/// A stream's resources on both GPUs.
pub(crate) struct Target {
    /// The render target on the other GPU (D3D11) and its D3D12 view.
    pub tex: ID3D11Texture2D,
    _tex12: ID3D12Resource,
    _heaps: (ID3D12Heap, ID3D12Heap),
    _buffers: (ID3D12Resource, ID3D12Resource),
    _main: [ID3D12Resource; RING],
    _main11: Vec<ID3D11Texture2D>,
    lists_other: [ID3D12GraphicsCommandList; RING],
    lists_main: [ID3D12GraphicsCommandList; RING],
    _allocators: Vec<ID3D12CommandAllocator>,
    pub size: (u32, u32),
    pub generation: u64,
    pub next: usize,
    /// Frame number of each slot's last copy.
    slot_value: [u64; RING],
    /// Frame number of the last copy out of `tex`.
    last: u64,
}

/// Adapter with `luid`.
pub(crate) fn adapter(luid: (u32, i32)) -> R<IDXGIAdapter1> {
    // SAFETY: COM calls with valid out values.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(err("DXGI"))?;
        let mut i = 0;
        loop {
            let a = factory.EnumAdapters1(i).map_err(|_| "that GPU is not present".to_owned())?;
            i += 1;
            let d = a.GetDesc1().map_err(err("adapter"))?;
            if (d.AdapterLuid.LowPart, d.AdapterLuid.HighPart) == luid {
                return Ok(a);
            }
        }
    }
}

fn device12(luid: (u32, i32)) -> R<ID3D12Device> {
    let a = adapter(luid)?;
    let mut d: Option<ID3D12Device> = None;
    // SAFETY: a valid adapter and out value.
    unsafe { D3D12CreateDevice(&a, D3D_FEATURE_LEVEL_11_0, &mut d).map_err(err("D3D12 device"))? };
    d.ok_or_else(|| "no D3D12 device".into())
}

fn copy_queue(d: &ID3D12Device) -> R<ID3D12CommandQueue> {
    let desc = D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_COPY, ..Default::default() };
    // SAFETY: a valid descriptor.
    unsafe { d.CreateCommandQueue(&desc).map_err(err("copy queue")) }
}

/// A D3D12 fence on `d`, shared (also across adapters when `cross`).
fn fence12(d: &ID3D12Device, cross: bool) -> R<(ID3D12Fence, Owned)> {
    let flags =
        if cross { D3D12_FENCE_FLAG_SHARED | D3D12_FENCE_FLAG_SHARED_CROSS_ADAPTER } else { D3D12_FENCE_FLAG_SHARED };
    // SAFETY: COM calls on a valid device.
    unsafe {
        let f: ID3D12Fence = d.CreateFence(0, flags).map_err(err("fence"))?;
        let h = d.CreateSharedHandle(&f, None, GENERIC_ALL.0, PCWSTR::null()).map_err(err("fence handle"))?;
        Ok((f, Owned(h)))
    }
}

fn open12<T: Interface>(d: &ID3D12Device, h: HANDLE, what: &'static str) -> R<T> {
    let mut o: Option<T> = None;
    // SAFETY: an NT handle of a shared object.
    unsafe { d.OpenSharedHandle(h, &mut o).map_err(err(what))? };
    o.ok_or_else(|| format!("{what}: none"))
}

fn open11_fence(d: &ID3D11Device5, h: HANDLE) -> R<ID3D11Fence> {
    let mut o: Option<ID3D11Fence> = None;
    // SAFETY: an NT handle of a shared fence.
    unsafe { d.OpenSharedFence(h, &mut o).map_err(err("D3D11 fence"))? };
    o.ok_or_else(|| "D3D11 fence: none".into())
}

impl Bridge {
    /// Sets up the bridge from the worker's D3D11 device (on `other_luid`)
    /// to the main GPU. `main_fence` is the main thread's fence (NT handle,
    /// left open). Also returns `done` as an NT handle for the main thread.
    /// `wake` wakes the worker when a build is finished.
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        main_luid: (u32, i32),
        main_fence: usize,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> R<(Self, usize)> {
        let device5: ID3D11Device5 = device.cast().map_err(err("D3D11.5"))?;
        let context: ID3D11DeviceContext4 = context.cast().map_err(err("D3D11.4"))?;
        // The worker's adapter, from its device.
        // SAFETY: COM calls on valid objects.
        let other_luid = unsafe {
            let dxgi: IDXGIDevice = device.cast().map_err(err("DXGI device"))?;
            let d = dxgi.GetAdapter().map_err(err("adapter"))?.GetDesc().map_err(err("adapter"))?;
            (d.AdapterLuid.LowPart, d.AdapterLuid.HighPart)
        };
        let other = device12(other_luid)?;
        let main = device12(main_luid)?;
        let (main11, _) = crate::worker::device_on(main_luid)?;
        let queue_other = copy_queue(&other)?;
        let queue_main = copy_queue(&main)?;
        // SAFETY: COM calls on valid devices; every handle is closed by Owned.
        unsafe {
            let mut rendered11: Option<ID3D11Fence> = None;
            device5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut rendered11).map_err(err("D3D11 fence"))?;
            let rendered11 = rendered11.ok_or("no D3D11 fence")?;
            let h =
                Owned(rendered11.CreateSharedHandle(None, GENERIC_ALL.0, PCWSTR::null()).map_err(err("fence handle"))?);
            let rendered12: ID3D12Fence = open12(&other, h.0, "rendered fence")?;

            let (copied_out12, h) = fence12(&other, false)?;
            let copied_out11 = open11_fence(&device5, h.0)?;

            let (crossed_other, h) = fence12(&other, true)?;
            let crossed_main: ID3D12Fence = open12(&main, h.0, "cross-adapter fence")?;

            let (done, done_handle) = fence12(&main, false)?;
            let main_fence: ID3D12Fence = open12(&main, HANDLE(main_fence as *mut _), "main fence")?;

            let event = Owned(CreateEventW(None, false, false, PCWSTR::null()).map_err(err("event"))?);
            let maker =
                Maker { other, main, device11: device.clone(), main11, heap_on_main: Arc::new(AtomicBool::new(false)) };
            // Try a small target now: a GPU pair without cross-adapter
            // support fails here, not on the first frame.
            drop(maker.target((64, 64), 0)?);
            let (jobs, rx) = mpsc::channel::<Job>();
            let (tx, made) = mpsc::channel();
            let builder = std::thread::Builder::new()
                .name("gk-bridge".into())
                .spawn(move || {
                    for (id, size, generation) in rx {
                        let r =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| maker.target(size, generation)))
                                .unwrap_or_else(|_| Err("building panicked".into()));
                        if tx.send((id, generation, r)).is_err() {
                            return;
                        }
                        wake();
                    }
                })
                .map_err(|e| e.to_string())?;
            let handle = done_handle.0.0 as usize;
            std::mem::forget(done_handle);
            let bridge = Self {
                queue_other,
                queue_main,
                jobs: Some(jobs),
                made,
                builder: Some(builder),
                context,
                rendered11,
                rendered12,
                copied_out12,
                copied_out11,
                crossed_other,
                crossed_main,
                done,
                main_fence,
                value: 0,
                event,
                retired: Vec::new(),
            };
            Ok((bridge, handle))
        }
    }

    /// Asks the builder for a stream's targets at `size`; they come back
    /// through `take_made`.
    pub fn request(&self, id: u64, size: (u32, u32), generation: u64) {
        if let Some(j) = &self.jobs {
            let _ = j.send((id, size, generation));
        }
    }

    /// Finished builds.
    pub fn take_made(&self) -> Vec<Made> {
        self.made.try_iter().collect()
    }
}

impl Maker {
    /// A stream's targets at `size`; the ring's handles are for the main
    /// thread. Puts the shared heap on the other GPU, else on the main one.
    fn target(&self, size: (u32, u32), generation: u64) -> R<(Target, Ring)> {
        let on_main = self.heap_on_main.load(Ordering::Relaxed);
        match self.target_inner(size, generation, on_main) {
            Ok(t) => Ok(t),
            Err(first) => {
                let t = self.target_inner(size, generation, !on_main).map_err(|_| first)?;
                self.heap_on_main.store(!on_main, Ordering::Relaxed);
                Ok(t)
            }
        }
    }

    fn target_inner(&self, (w, h): (u32, u32), generation: u64, heap_on_main: bool) -> R<(Target, Ring)> {
        let format = DXGI_FORMAT_R8G8B8A8_UNORM;
        let tex_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 0,
            Width: u64::from(w),
            Height: h,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_NONE,
        };
        // SAFETY: COM calls on valid devices with valid descriptors; NT
        // handles are closed by Owned or by the Ring.
        unsafe {
            // 1. The render target on the other GPU, opened in D3D12.
            let d11 = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
            };
            let mut tex = None;
            self.device11.CreateTexture2D(&d11, None, Some(&mut tex)).map_err(err("render texture"))?;
            let tex: ID3D11Texture2D = tex.ok_or("no render texture")?;
            let h11 = Owned(
                tex.cast::<IDXGIResource1>()
                    .map_err(err("DXGI resource"))?
                    .CreateSharedHandle(None, GENERIC_ALL.0, PCWSTR::null())
                    .map_err(err("texture handle"))?,
            );
            let tex12: ID3D12Resource = open12(&self.other, h11.0, "render texture in D3D12")?;

            // 2. The cross-adapter buffer: one slot per ring texture.
            let mut fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
            let mut total = 0u64;
            self.other.GetCopyableFootprints(&tex_desc, 0, 1, 0, Some(&mut fp), None, None, Some(&mut total));
            let slot = total.next_multiple_of(u64::from(D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT));
            let size = (slot * RING as u64).next_multiple_of(64 * 1024);
            let (a, b) = if heap_on_main { (&self.main, &self.other) } else { (&self.other, &self.main) };
            let heap_desc = D3D12_HEAP_DESC {
                SizeInBytes: size,
                Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                Alignment: 0,
                Flags: D3D12_HEAP_FLAG_SHARED | D3D12_HEAP_FLAG_SHARED_CROSS_ADAPTER,
            };
            let mut heap_a: Option<ID3D12Heap> = None;
            a.CreateHeap(&heap_desc, &mut heap_a).map_err(err("cross-adapter heap"))?;
            let heap_a = heap_a.ok_or("no heap")?;
            let hh =
                Owned(a.CreateSharedHandle(&heap_a, None, GENERIC_ALL.0, PCWSTR::null()).map_err(err("heap handle"))?);
            let heap_b: ID3D12Heap = open12(b, hh.0, "cross-adapter heap on the other GPU")?;
            let buf_desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Alignment: 0,
                Width: size,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_UNKNOWN,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                Flags: D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER,
            };
            let placed = |d: &ID3D12Device, heap: &ID3D12Heap| -> R<ID3D12Resource> {
                let mut r: Option<ID3D12Resource> = None;
                d.CreatePlacedResource(heap, 0, &buf_desc, D3D12_RESOURCE_STATE_COMMON, None, &mut r)
                    .map_err(err("cross-adapter buffer"))?;
                r.ok_or_else(|| "no buffer".into())
            };
            let (buf_a, buf_b) = (placed(a, &heap_a)?, placed(b, &heap_b)?);
            let (buf_other, buf_main) =
                if heap_on_main { (buf_b.clone(), buf_a.clone()) } else { (buf_a.clone(), buf_b.clone()) };

            // 3. The shared textures on the main GPU.
            // Made by D3D11 (the main thread's D3D11 cannot open textures
            // D3D12 made), opened in D3D12 for the copy; the ring's NT
            // handles go to the main thread, which opens them too.
            let mut main = Vec::with_capacity(RING);
            let mut main11 = Vec::with_capacity(RING);
            let mut ring = Ring { generation, size: (w, h), handles: [0; RING], nt: true };
            for h in ring.handles.iter_mut() {
                let mut t = None;
                self.main11.CreateTexture2D(&d11, None, Some(&mut t)).map_err(err("main-GPU texture"))?;
                let t: ID3D11Texture2D = t.ok_or("no main-GPU texture")?;
                let handle = t
                    .cast::<IDXGIResource1>()
                    .map_err(err("DXGI resource"))?
                    .CreateSharedHandle(None, GENERIC_ALL.0, PCWSTR::null())
                    .map_err(err("texture handle"))?;
                *h = handle.0 as usize;
                main.push(open12::<ID3D12Resource>(&self.main, handle, "main-GPU texture in D3D12")?);
                main11.push(t);
            }

            // 4. A recorded copy per slot on each GPU.
            let mut allocators = Vec::new();
            let mut record = |d: &ID3D12Device, dst: D3D12_TEXTURE_COPY_LOCATION, src: D3D12_TEXTURE_COPY_LOCATION| {
                let r = (|| -> R<ID3D12GraphicsCommandList> {
                    let alloc: ID3D12CommandAllocator =
                        d.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY).map_err(err("allocator"))?;
                    let list: ID3D12GraphicsCommandList = d
                        .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &alloc, None)
                        .map_err(err("command list"))?;
                    list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
                    list.Close().map_err(err("command list"))?;
                    allocators.push(alloc);
                    Ok(list)
                })();
                drop_location(dst);
                drop_location(src);
                r
            };
            let footprint =
                |i: usize| D3D12_PLACED_SUBRESOURCE_FOOTPRINT { Offset: slot * i as u64, Footprint: fp.Footprint };
            let mut lists_other = Vec::with_capacity(RING);
            let mut lists_main = Vec::with_capacity(RING);
            for (i, m) in main.iter().enumerate() {
                lists_other.push(record(&self.other, placed_location(&buf_other, footprint(i)), subresource(&tex12))?);
                lists_main.push(record(&self.main, subresource(m), placed_location(&buf_main, footprint(i)))?);
            }
            let three = |v: Vec<ID3D12GraphicsCommandList>| -> [ID3D12GraphicsCommandList; RING] {
                v.try_into().unwrap_or_else(|_| unreachable!("RING lists"))
            };
            let main: [ID3D12Resource; RING] = main.try_into().unwrap_or_else(|_| unreachable!("RING textures"));
            Ok((
                Target {
                    tex,
                    _tex12: tex12,
                    _heaps: (heap_a, heap_b),
                    _buffers: (buf_a, buf_b),
                    _main: main,
                    _main11: main11,
                    lists_other: three(lists_other),
                    lists_main: three(lists_main),
                    _allocators: allocators,
                    size: (w, h),
                    generation,
                    next: 0,
                    slot_value: [0; RING],
                    last: 0,
                },
                ring,
            ))
        }
    }
}

impl Bridge {
    /// Before drawing into `t.tex`: makes the D3D11 GPU wait until the last
    /// copy out of it is done. False if the next slot's previous copy has
    /// not finished (skip this frame's copy then).
    /// The stream's last frame is not through both copies yet.
    pub fn behind(&self, t: &Target) -> bool {
        // SAFETY: reading a fence value.
        t.last > 0 && unsafe { self.done.GetCompletedValue() } < t.last
    }

    pub fn begin(&self, t: &Target) -> bool {
        // SAFETY: fences of this bridge.
        unsafe {
            if t.last > 0 {
                let _ = self.context.Wait(&self.copied_out11, t.last);
            }
            // Never waits: with three slots the one three frames back is
            // long done unless the main thread stopped presenting.
            self.done.GetCompletedValue() >= t.slot_value[t.next]
        }
    }

    /// After drawing a new frame into `t.tex`: queues both copies. Returns
    /// the slot and the `done` value the main thread waits for.
    /// `released`: the main fence value after which the slot's texture is
    /// no longer sampled (0: never sampled).
    pub fn submit(&mut self, t: &mut Target, released: u64, d3d11: &ID3D11DeviceContext) -> (usize, u64) {
        self.value += 1;
        let v = self.value;
        let i = t.next;
        // SAFETY: fences, queues and recorded lists of this bridge; the
        // lists' previous runs finished (`begin` saw `done` pass them).
        unsafe {
            let _ = self.context.Signal(&self.rendered11, v);
            d3d11.Flush();
            let _ = self.queue_other.Wait(&self.rendered12, v);
            self.queue_other.ExecuteCommandLists(&[Some(t.lists_other[i].cast().unwrap_or_else(|_| unreachable!()))]);
            let _ = self.queue_other.Signal(&self.copied_out12, v);
            let _ = self.queue_other.Signal(&self.crossed_other, v);
            let _ = self.queue_main.Wait(&self.crossed_main, v);
            if released > 0 {
                let _ = self.queue_main.Wait(&self.main_fence, released);
            }
            self.queue_main.ExecuteCommandLists(&[Some(t.lists_main[i].cast().unwrap_or_else(|_| unreachable!()))]);
            let _ = self.queue_main.Signal(&self.done, v);
        }
        t.slot_value[i] = v;
        t.last = v;
        t.next = (i + 1) % RING;
        (i, v)
    }

    /// Keeps a target alive until the GPUs are done with it.
    pub fn retire(&mut self, t: Target) {
        let last = t.slot_value.iter().copied().max().unwrap_or(0).max(t.last);
        self.retired.push((last, t));
        self.collect();
    }

    /// Frees retired targets the GPUs finished with.
    pub fn collect(&mut self) {
        if self.retired.is_empty() {
            return;
        }
        // SAFETY: reading a fence value.
        let done = unsafe { self.done.GetCompletedValue() };
        self.retired.retain(|(v, _)| *v > done);
    }

    /// Waits (bounded) for every queued copy, before the devices go away.
    pub fn finish(&mut self) {
        // SAFETY: fence and event of this bridge.
        unsafe {
            if self.done.GetCompletedValue() < self.value
                && self.done.SetEventOnCompletion(self.value, self.event.0).is_ok()
            {
                WaitForSingleObject(self.event.0, 500);
            }
        }
        self.collect();
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        // The builder ends when its job channel closes.
        self.jobs = None;
        if let Some(t) = self.builder.take() {
            let _ = t.join();
        }
    }
}

fn subresource(r: &ID3D12Resource) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        pResource: ManuallyDrop::new(Some(r.clone())),
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
    }
}

fn placed_location(r: &ID3D12Resource, fp: D3D12_PLACED_SUBRESOURCE_FOOTPRINT) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        pResource: ManuallyDrop::new(Some(r.clone())),
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: fp },
    }
}

fn drop_location(l: D3D12_TEXTURE_COPY_LOCATION) {
    drop(ManuallyDrop::into_inner(l.pResource));
}
