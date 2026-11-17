//! The render thread (docs/DESIGN.md §1, §2 gk-render).
//!
//! One D3D11 device for everything: mpv decodes on it (d3d11va), renders each
//! stream into a texture sized to its tile, and our egui renderer composites
//! those textures with the UI. The UI thread hands over tessellated egui
//! output only when the UI changed; a new video or animation frame just
//! replays the last UI with fresh texture contents. At most one present per
//! display refresh (waitable swap chain, frame latency 1).

#[cfg(windows)]
mod bridge;
mod d3d;
#[cfg(windows)]
mod egui_d3d;
#[cfg(windows)]
mod worker;

use std::sync::atomic::AtomicU64;

/// User texture ids at or above this bit are animations; below are streams.
pub const ANIM_BIT: u64 = 1 << 40;

/// Where a stream's video goes this frame, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoTile {
    pub id: u64,
    /// x0, y0, x1, y1.
    pub rect_px: [i32; 4],
    /// Render at this size instead of the rect's (the tile is moving: the
    /// stream keeps its texture and is scaled until the tile settles).
    pub render: Option<(u32, u32)>,
}

impl VideoTile {
    pub fn size(&self) -> (u32, u32) {
        self.render.unwrap_or((
            (self.rect_px[2] - self.rect_px[0]).max(1) as u32,
            (self.rect_px[3] - self.rect_px[1]).max(1) as u32,
        ))
    }
}

/// One egui pass's output.
pub struct UiFrame {
    pub primitives: Vec<egui::ClippedPrimitive>,
    pub textures_delta: egui::TexturesDelta,
    pub pixels_per_point: f32,
    pub videos: Vec<VideoTile>,
}

/// Frames of an animated image (straight RGBA8, all the same size).
pub struct AnimFrames {
    pub width: u32,
    pub height: u32,
    pub frames: Vec<(Vec<u8>, u32)>,
}

/// Counters for `--smoke` and the debug overlay.
#[derive(Default)]
pub struct Stats {
    pub presents: AtomicU64,
    pub ui_frames: AtomicU64,
    pub video_frames: AtomicU64,
    pub anim_redraws: AtomicU64,
    /// Total render thread busy time in microseconds (excluding waits).
    pub busy_us: AtomicU64,
    /// Presents that waited longer than 50 ms on the swap chain.
    pub slow_waits: AtomicU64,
    /// Frames of streams on other GPUs copied over, their bytes and the
    /// time spent (readback there plus upload here), in microseconds.
    pub copied_frames: AtomicU64,
    /// Of those, copied GPU to GPU (no CPU time; not in `copy_us`).
    pub gpu_copied_frames: AtomicU64,
    pub copied_bytes: AtomicU64,
    pub copy_us: AtomicU64,
    /// Per stream: frames drawn into its tile and the tile texture size.
    pub tiles: std::sync::Mutex<std::collections::HashMap<u64, TileStats>>,
}

/// What the render thread did for one stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TileStats {
    /// Frames rendered into the stream's tile texture.
    pub frames: u64,
    pub width: u32,
    pub height: u32,
    /// The texture is shown in the current UI (else a 16 px placeholder).
    pub visible: bool,
    /// Renders mpv asked for that were not a new picture.
    pub repeats: u64,
    pub redraws: u64,
    /// Frames copied over from another GPU and the time that took (us).
    pub copied: u64,
    /// Of those, copied GPU to GPU.
    pub gpu_copied: u64,
    pub copy_us: u64,
}

/// What the render thread reports once it is up.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub adapter: String,
    pub software: bool,
    /// The adapter's LUID (low, high): matches GPU performance counters.
    pub luid: (u32, i32),
}

#[cfg(windows)]
pub use d3d::{Presenter, set_gpu_copy, set_render_workers};

/// Registers the calling thread with the Windows multimedia scheduler as a
/// playback thread (MMCSS "Playback"): it keeps its slice of CPU time while
/// the window is in the background. For the render thread and workers.
#[cfg(windows)]
pub(crate) fn playback_thread() {
    use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
    let mut index = 0u32;
    // SAFETY: a static task name and a valid out value; the registration
    // ends with the thread.
    if let Err(e) = unsafe { AvSetMmThreadCharacteristicsW(windows::core::w!("Playback"), &mut index) } {
        log::info!("multimedia scheduler: {e}");
    }
}

#[cfg(not(windows))]
pub struct Presenter;
