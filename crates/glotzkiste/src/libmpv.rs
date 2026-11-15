//! Finds libmpv: `libmpv-2.dll` next to the executable. The setup and the
//! portable zip put it there, and the LGPL lets the viewer replace it with
//! any compatible build (licenses/libmpv-NOTICE.txt). Development builds get
//! the pinned copy next to the exe from build.rs (`GK_LIBMPV`).

use gk_mpv::Libmpv;
use std::path::PathBuf;
use std::sync::Arc;

fn next_to_exe() -> Option<PathBuf> {
    let p = std::env::current_exe().ok()?.parent()?.join("libmpv-2.dll");
    p.is_file().then_some(p)
}

/// Removes the copies of libmpv that pre-release builds extracted from the
/// executable into `%LOCALAPPDATA%\Glotzkiste` (about 100 MB each).
fn remove_old_extractions() {
    let Some(dir) = gk_twitch::local_dir() else { return };
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("libmpv-") && (name.ends_with(".dll") || name.ends_with(".tmp")) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

pub fn load() -> Result<Arc<Libmpv>, String> {
    remove_old_extractions();
    match next_to_exe() {
        Some(p) => Libmpv::load_from(&[p]).map(Arc::new).map_err(|e| e.to_string()),
        None => Libmpv::load()
            .map(Arc::new)
            .map_err(|e| format!("libmpv-2.dll is missing next to glotzkiste.exe (reinstall Glotzkiste): {e}")),
    }
}
