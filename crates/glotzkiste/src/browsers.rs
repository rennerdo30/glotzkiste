//! Installed browsers and the default one, for the login guide: each
//! button opens the extension store in that browser.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Chrome,
    Edge,
    Brave,
    Firefox,
    Opera,
    OperaGx,
    Vivaldi,
}

/// The recommended cookie exporter (open source, MIT, exports only on click).
pub const CHROME_STORE: &str =
    "https://chromewebstore.google.com/detail/get-cookiestxt-locally/cclelndahbckbenkjhflpdbgdldlbecc";
pub const FIREFOX_STORE: &str = "https://addons.mozilla.org/firefox/addon/get-cookies-txt-locally/";
pub const SOURCE: &str = "https://github.com/kairi003/Get-cookies.txt-LOCALLY";

impl Kind {
    pub const ALL: [Kind; 7] =
        [Kind::Chrome, Kind::Edge, Kind::Brave, Kind::Firefox, Kind::Opera, Kind::OperaGx, Kind::Vivaldi];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Chrome => "Chrome",
            Kind::Edge => "Edge",
            Kind::Brave => "Brave",
            Kind::Firefox => "Firefox",
            Kind::Opera => "Opera",
            Kind::OperaGx => "Opera GX",
            Kind::Vivaldi => "Vivaldi",
        }
    }

    fn exe(self) -> &'static str {
        match self {
            Kind::Chrome => "chrome.exe",
            Kind::Edge => "msedge.exe",
            Kind::Brave => "brave.exe",
            Kind::Firefox => "firefox.exe",
            Kind::Opera | Kind::OperaGx => "opera.exe",
            Kind::Vivaldi => "vivaldi.exe",
        }
    }

    /// Matches the `ProgId` of the default https handler.
    fn from_prog_id(p: &str) -> Option<Kind> {
        let p = p.to_ascii_lowercase();
        Some(if p.starts_with("chromehtml") {
            Kind::Chrome
        } else if p.starts_with("msedgehtm") {
            Kind::Edge
        } else if p.starts_with("bravehtml") {
            Kind::Brave
        } else if p.starts_with("firefoxurl") {
            Kind::Firefox
        } else if p.starts_with("operagx") {
            Kind::OperaGx
        } else if p.starts_with("opera") {
            Kind::Opera
        } else if p.starts_with("vivaldi") {
            Kind::Vivaldi
        } else {
            return None;
        })
    }

    pub fn store_url(self) -> &'static str {
        if self == Kind::Firefox { FIREFOX_STORE } else { CHROME_STORE }
    }

    /// What is different about installing in this browser.
    pub fn store_note(self) -> Option<&'static str> {
        match self {
            Kind::Edge => {
                Some("Edge asks once to \"Allow extensions from other stores\": choose Allow, then \"Add to Chrome\".")
            }
            Kind::Opera | Kind::OperaGx => {
                Some("Opera first offers its \"Install Chrome Extensions\" helper; add it, then the extension.")
            }
            _ => None,
        }
    }

    /// Where the cookie list is in the developer tools.
    pub fn devtools_path(self) -> &'static str {
        if self == Kind::Firefox {
            "Storage › Cookies › https://www.twitch.tv"
        } else {
            "Application › Storage › Cookies › https://www.twitch.tv"
        }
    }
}

#[derive(Debug, Clone)]
pub struct Browser {
    pub kind: Kind,
    pub exe: std::path::PathBuf,
    pub default: bool,
}

#[cfg(windows)]
fn reg_string(root: windows::Win32::System::Registry::HKEY, key: &str, value: Option<&str>) -> Option<String> {
    use windows::Win32::System::Registry::{RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::{HSTRING, PCWSTR};
    let key = HSTRING::from(key);
    let value = value.map(HSTRING::from);
    let vptr = value.as_ref().map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr()));
    let mut buf = vec![0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: buffer and length describe `buf`; strings are NUL-terminated.
    let r =
        unsafe { RegGetValueW(root, &key, vptr, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr().cast()), Some(&mut len)) };
    if r.is_err() {
        return None;
    }
    let n = (len as usize / 2).saturating_sub(1).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]).trim_end_matches('\0').trim_matches('"').to_owned())
}

/// Installed browsers, the default one first. Reads the registry: call it
/// on a worker.
pub fn detect() -> Vec<Browser> {
    #[cfg(windows)]
    {
        use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
        let default = reg_string(
            HKEY_CURRENT_USER,
            "Software\\Microsoft\\Windows\\Shell\\Associations\\UrlAssociations\\https\\UserChoice",
            Some("ProgId"),
        )
        .and_then(|p| Kind::from_prog_id(&p));
        let mut out = Vec::new();
        for kind in Kind::ALL {
            let key = format!("Software\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{}", kind.exe());
            let exe = reg_string(HKEY_CURRENT_USER, &key, None).or_else(|| reg_string(HKEY_LOCAL_MACHINE, &key, None));
            let Some(exe) = exe.map(std::path::PathBuf::from).filter(|p| p.is_file()) else { continue };
            // Opera and Opera GX share opera.exe: tell them apart by folder.
            let gx = exe.to_string_lossy().to_ascii_lowercase().contains("opera gx");
            if (kind == Kind::OperaGx) != gx && matches!(kind, Kind::Opera | Kind::OperaGx) {
                continue;
            }
            out.push(Browser { kind, exe, default: default == Some(kind) });
        }
        // Edge always exists on Windows even without an App Paths entry.
        if !out.iter().any(|b| b.kind == Kind::Edge) {
            for base in ["ProgramFiles(x86)", "ProgramFiles"] {
                if let Some(p) = std::env::var_os(base)
                    .map(|b| std::path::PathBuf::from(b).join("Microsoft\\Edge\\Application\\msedge.exe"))
                    .filter(|p| p.is_file())
                {
                    out.push(Browser { kind: Kind::Edge, exe: p, default: default == Some(Kind::Edge) });
                    break;
                }
            }
        }
        out.sort_by_key(|b| !b.default);
        out
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// The major version from the exe's version resource.
#[cfg(windows)]
fn major_version(exe: &std::path::Path) -> Option<u32> {
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW,
    };
    use windows::core::{HSTRING, w};
    let path = HSTRING::from(exe.as_os_str());
    // SAFETY: the buffer is sized by the first call; VerQueryValueW returns a
    // pointer into it, read before the buffer is dropped.
    unsafe {
        let size = GetFileVersionInfoSizeW(&path, None);
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        GetFileVersionInfoW(&path, None, size, buf.as_mut_ptr().cast()).ok()?;
        let mut info: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        if !VerQueryValueW(buf.as_ptr().cast(), w!("\\"), &mut info, &mut len).as_bool()
            || info.is_null()
            || (len as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>()
        {
            return None;
        }
        let fixed = &*(info as *const VS_FIXEDFILEINFO);
        Some(fixed.dwFileVersionMS >> 16).filter(|v| *v > 0)
    }
}

#[cfg(not(windows))]
fn major_version(_: &std::path::Path) -> Option<u32> {
    None
}

/// The user agent this browser sends, built the way the browser builds it
/// (since user-agent reduction only the major version varies). `None` for
/// browsers whose exe version is not their engine version (Opera, Vivaldi).
pub fn user_agent_for(kind: Kind, major: u32) -> Option<String> {
    const WIN: &str = "Windows NT 10.0; Win64; x64";
    match kind {
        Kind::Chrome | Kind::Brave => Some(format!(
            "Mozilla/5.0 ({WIN}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36"
        )),
        Kind::Edge => Some(format!(
            "Mozilla/5.0 ({WIN}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36 Edg/{major}.0.0.0"
        )),
        Kind::Firefox => Some(format!("Mozilla/5.0 ({WIN}; rv:{major}.0) Gecko/20100101 Firefox/{major}.0")),
        Kind::Opera | Kind::OperaGx | Kind::Vivaldi => None,
    }
}

/// The browser's user agent and a short label ("Chrome 142"). Reads the
/// exe's version: call it on a worker.
pub fn user_agent(b: &Browser) -> Option<(String, String)> {
    let major = major_version(&b.exe)?;
    let ua = user_agent_for(b.kind, major)?;
    Some((ua, format!("{} {major}", b.kind.name())))
}

/// Opens `url` in this browser (falls back to the default handler).
pub fn open(browser: Option<&Browser>, url: &str, ctx: &egui::Context) {
    if let Some(b) = browser {
        let spawned = std::process::Command::new(&b.exe).arg(url).spawn();
        match spawned {
            // The browser runs on its own; we do not keep or wait for it.
            Ok(child) => drop(child),
            Err(e) => {
                log::warn!("starting {}: {e}", b.kind.name());
                ctx.open_url(egui::OpenUrl::new_tab(url));
            }
        }
    } else {
        ctx.open_url(egui::OpenUrl::new_tab(url));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prog_ids_map_to_browsers() {
        assert_eq!(Kind::from_prog_id("ChromeHTML"), Some(Kind::Chrome));
        assert_eq!(Kind::from_prog_id("MSEdgeHTM"), Some(Kind::Edge));
        assert_eq!(Kind::from_prog_id("FirefoxURL-308046B0AF4A39CB"), Some(Kind::Firefox));
        assert_eq!(Kind::from_prog_id("OperaGXStable"), Some(Kind::OperaGx));
        assert_eq!(Kind::from_prog_id("OperaStable"), Some(Kind::Opera));
        assert_eq!(Kind::from_prog_id("SomethingElse"), None);
        assert_eq!(Kind::Firefox.store_url(), FIREFOX_STORE);
        assert_eq!(Kind::Brave.store_url(), CHROME_STORE);
    }

    #[test]
    fn user_agents_match_the_browsers_own() {
        assert_eq!(
            user_agent_for(Kind::Chrome, 142).as_deref(),
            Some(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36"
            )
        );
        assert!(user_agent_for(Kind::Edge, 142).unwrap().ends_with("Safari/537.36 Edg/142.0.0.0"));
        assert_eq!(
            user_agent_for(Kind::Firefox, 145).as_deref(),
            Some("Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:145.0) Gecko/20100101 Firefox/145.0")
        );
        assert_eq!(user_agent_for(Kind::Vivaldi, 7), None);
    }

    #[test]
    fn reads_a_real_exe_version() {
        // Every Windows has notepad with a version resource.
        let windir = std::env::var_os("WINDIR").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Windows".into());
        assert!(major_version(&windir.join("System32").join("notepad.exe")).is_some_and(|v| v >= 6));
    }
}
