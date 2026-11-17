//! The viewer's settings (GK-15), saved as `settings.json` beside
//! `layout.json`. The `GK_*` and `GLOTZKISTE_*` environment switches still
//! win over them.

use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The highest variant automatic quality picks, by height (0: source).
    pub max_auto_height: u32,
    /// Decode new streams on the other GPUs first (else on the main GPU).
    pub other_gpus: bool,
    /// Copy frames from other GPUs GPU to GPU (else read back).
    pub gpu_copy: bool,
    /// Render threads on the main GPU (0: automatic; after a restart).
    pub render_threads: u32,
    /// Look for updates once a day.
    pub auto_update: bool,
    /// "" (this version's channel), "stable" or "beta".
    pub update_channel: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_auto_height: 0,
            other_gpus: true,
            gpu_copy: true,
            render_threads: 0,
            auto_update: true,
            update_channel: String::new(),
        }
    }
}

/// Choices for `max_auto_height`.
pub const HEIGHTS: [(u32, &str); 5] = [(0, "Source"), (1080, "1080p"), (720, "720p"), (480, "480p"), (360, "360p")];

static MAX_AUTO_HEIGHT: AtomicU32 = AtomicU32::new(0);

/// The cap for automatic quality (0: none), read by the stream threads.
pub fn max_auto_height() -> u32 {
    MAX_AUTO_HEIGHT.load(Ordering::Relaxed)
}

fn path() -> Option<std::path::PathBuf> {
    crate::layout::dir().map(|d| d.join("settings.json"))
}

impl Settings {
    pub fn load() -> Self {
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return Self::default();
        };
        serde_json::from_str(&text).map_err(|e| log::warn!("settings.json: {e}")).unwrap_or_default()
    }

    /// Writes the file (call it off the UI thread).
    pub fn save(&self) {
        let (Some(p), Ok(json)) = (path(), serde_json::to_string_pretty(self)) else { return };
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }

    /// Hands the settings to the parts that read them themselves.
    pub fn apply(&self) {
        MAX_AUTO_HEIGHT.store(self.max_auto_height, Ordering::Relaxed);
        gk_render::set_gpu_copy(self.gpu_copy);
        gk_render::set_render_workers(self.render_threads as usize);
    }

    /// The update channel to ask for (`None`: this version's).
    pub fn channel(&self) -> Option<String> {
        matches!(self.update_channel.as_str(), "stable" | "beta").then(|| self.update_channel.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_take_defaults() {
        let s: Settings = serde_json::from_str(r#"{"max_auto_height":720}"#).unwrap();
        assert_eq!(s.max_auto_height, 720);
        assert!(s.other_gpus && s.gpu_copy && s.auto_update);
        assert_eq!(s.channel(), None);
        let s = Settings { update_channel: "beta".into(), ..Default::default() };
        assert_eq!(s.channel().as_deref(), Some("beta"));
        let s = Settings { update_channel: "nightly".into(), ..Default::default() };
        assert_eq!(s.channel(), None);
    }
}
