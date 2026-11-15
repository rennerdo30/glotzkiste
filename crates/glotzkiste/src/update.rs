//! In-app updates (the contract is docs/RELEASING.md). The check runs on a
//! worker once after start, at most once a day; installing downloads the
//! setup, verifies it against the release's SHA256SUMS.txt and starts it
//! with /S /UPDATE, then Glotzkiste closes so the setup can replace it and
//! start it again. Nothing here runs on the UI thread.

use gk_update::{Candidate, Channel, Version};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::time::SystemTime;

pub use gk_update::RELEASE_PAGE;

/// Whether this copy was installed by the setup (and so can update itself).
/// Portable copies and development builds only report updates.
pub fn installed() -> bool {
    let (Ok(exe), Some(local)) = (std::env::current_exe(), std::env::var_os("LOCALAPPDATA")) else {
        return false;
    };
    let dir = PathBuf::from(local).join("Programs").join("Glotzkiste");
    exe.parent().is_some_and(|p| p.to_string_lossy().eq_ignore_ascii_case(&dir.to_string_lossy()))
}

pub fn current() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| Version::parse("0.0.0").unwrap_or_else(|_| unreachable!()))
}

pub enum Outcome {
    Found(Candidate),
    UpToDate,
    Skipped,
    /// Updates are off for this copy; the reason, for an explicit check.
    Off(String),
}

/// `force`: a check the viewer asked for (ignores the daily limit).
fn check(force: bool) -> Result<Outcome, String> {
    let off = if cfg!(debug_assertions) {
        Some("This is a development build; updates are off.")
    } else if std::env::var_os("GLOTZKISTE_NO_UPDATE").is_some() {
        Some("Updates are off (GLOTZKISTE_NO_UPDATE is set).")
    } else {
        None
    };
    if let Some(why) = off {
        return Ok(if force { Outcome::Off(why.into()) } else { Outcome::Skipped });
    }
    let current = current();
    let Some(dir) = gk_twitch::local_dir() else { return Ok(Outcome::Skipped) };
    let stamp = dir.join("update-check");
    if !force && !gk_update::due(&stamp, SystemTime::now()) {
        return Ok(Outcome::Skipped);
    }
    let setting = std::env::var("GLOTZKISTE_UPDATE_CHANNEL").ok();
    let channel = Channel::from_setting(setting.as_deref(), &current);
    let result = gk_update::check(&current, channel).map_err(|e| e.to_string());
    // A failed check still waits a day: no retry storm on a flaky network.
    let _ = gk_update::write_stamp(&stamp, SystemTime::now());
    Ok(match result? {
        Some(c) => Outcome::Found(c),
        None => Outcome::UpToDate,
    })
}

pub fn spawn_check(force: bool, done: impl FnOnce(Result<Outcome, String>) + Send + 'static) {
    let _ = std::thread::Builder::new().name("gk-update-check".into()).spawn(move || {
        let r = catch_unwind(AssertUnwindSafe(|| check(force))).unwrap_or_else(|_| Err("update check crashed".into()));
        done(r);
    });
}

/// Downloads, verifies and starts the setup. On `Ok` the caller closes the
/// app; the setup waits for it, installs and starts it again.
fn install(c: &Candidate) -> Result<(), String> {
    if !installed() {
        return Err(format!("This copy was not installed by the setup; get Glotzkiste {} from {RELEASE_PAGE}", c.version));
    }
    let setup = gk_update::download_verified(c).map_err(|e| e.to_string())?;
    // A fresh folder per attempt: never runs a file someone else placed.
    let nonce = SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let dir = std::env::temp_dir().join(format!("glotzkiste-update-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&dir).map_err(|e| format!("could not create the download folder: {e}"))?;
    let path = dir.join(gk_update::setup_name(&c.version));
    std::fs::write(&path, &setup).map_err(|e| format!("could not save the setup: {e}"))?;
    // Re-check what is on disk right before running it.
    let on_disk = std::fs::read(&path).map_err(|e| e.to_string())?;
    if gk_update::sha256_hex(&on_disk) != gk_update::sha256_hex(&setup) {
        return Err("the saved setup changed on disk, so it was not run".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        std::process::Command::new(&path)
            .args(["/S", "/UPDATE"])
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .map_err(|e| format!("could not start the setup: {e}"))?;
    }
    Ok(())
}

pub fn spawn_install(c: Candidate, done: impl FnOnce(Result<(), String>) + Send + 'static) {
    let _ = std::thread::Builder::new().name("gk-update-install".into()).spawn(move || {
        let r = catch_unwind(AssertUnwindSafe(|| install(&c))).unwrap_or_else(|_| Err("update crashed".into()));
        done(r);
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_running_version_parses() {
        assert_eq!(super::current().to_string(), env!("CARGO_PKG_VERSION"));
    }
}
