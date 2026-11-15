//! Finds libmpv: the copy embedded in the executable (extracted once to
//! `%LOCALAPPDATA%\Glotzkiste`), else one next to the executable.

use gk_mpv::Libmpv;
use std::path::PathBuf;
use std::sync::Arc;

static PACKED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/libmpv.zst"));
static META: &str = include_str!(concat!(env!("OUT_DIR"), "/libmpv.meta"));

fn extract() -> Result<Option<PathBuf>, String> {
    if PACKED.is_empty() {
        return Ok(None);
    }
    let mut it = META.split(' ');
    let (hash, len) = (it.next().unwrap_or("0"), it.next().and_then(|l| l.parse::<u64>().ok()).unwrap_or(0));
    let dir = gk_twitch::local_dir().ok_or("no local app data folder")?;
    let path = dir.join(format!("libmpv-{hash}.dll"));
    if std::fs::metadata(&path).map(|m| m.len() == len).unwrap_or(false) {
        return Ok(Some(path));
    }
    let raw = zstd::stream::decode_all(PACKED).map_err(|e| format!("unpacking libmpv: {e}"))?;
    let tmp = dir.join(format!("libmpv-{hash}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &raw).map_err(|e| format!("writing libmpv: {e}"))?;
    match std::fs::rename(&tmp, &path) {
        Ok(()) => {}
        // Another instance won the race: use its file.
        Err(_) if path.is_file() => {
            let _ = std::fs::remove_file(&tmp);
        }
        Err(e) => return Err(format!("placing libmpv: {e}")),
    }
    // Remove extractions of older builds.
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("libmpv-") && name != format!("libmpv-{hash}.dll") && name.ends_with(".dll") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(Some(path))
}

/// A libmpv-2.dll next to the executable replaces the embedded copy (the
/// LGPL lets the viewer swap the library; licenses/libmpv-NOTICE.txt).
fn replacement() -> Option<PathBuf> {
    let p = std::env::current_exe().ok()?.parent()?.join("libmpv-2.dll");
    p.is_file().then_some(p)
}

pub fn load() -> Result<Arc<Libmpv>, String> {
    if let Some(p) = replacement() {
        log::info!("using the libmpv next to the executable: {}", p.display());
        return Libmpv::load_from(&[p]).map(Arc::new).map_err(|e| e.to_string());
    }
    match extract()? {
        Some(path) => Libmpv::load_from(&[path]).map(Arc::new).map_err(|e| e.to_string()),
        None => Libmpv::load().map(Arc::new).map_err(|e| e.to_string()),
    }
}
