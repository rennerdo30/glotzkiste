//! The render thread (docs/DESIGN.md §1, §2 gk-render).
//!
//! One D3D11 device for everything: mpv decodes on it (d3d11va), renders each
//! stream into a texture sized to its tile, and our egui renderer composites
//! those textures with the UI. The UI thread hands over tessellated egui
//! output only when the UI changed; a new video or animation frame just
//! replays the last UI with fresh texture contents. At most one present per
//! display refresh (waitable swap chain, frame latency 1).

#[cfg(windows)]
mod d3d;
#[cfg(windows)]
mod egui_d3d;

use std::sync::atomic::AtomicU64;

/// User texture ids at or above this bit are animations; below are streams.
pub const ANIM_BIT: u64 = 1 << 40;

/// Where a stream's video goes this frame, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoTile {
    pub id: u64,
    /// x0, y0, x1, y1.
    pub rect_px: [i32; 4],
}

impl VideoTile {
    pub fn size(&self) -> (u32, u32) {
        ((self.rect_px[2] - self.rect_px[0]).max(1) as u32, (self.rect_px[3] - self.rect_px[1]).max(1) as u32)
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
}

/// What the render thread reports once it is up.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub adapter: String,
    pub software: bool,
}

#[cfg(windows)]
pub use d3d::Presenter;

#[cfg(not(windows))]
pub struct Presenter;
