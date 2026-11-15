//! Logging in with the viewer's own browser session.
//!
//! The viewer exports their Twitch cookies (a `cookies.txt` or a JSON cookie
//! export) or pastes the `auth-token` cookie value. That token is what the web
//! player sends as `Authorization: OAuth <token>`; with it the playback token
//! carries the account's Turbo or subscription status, so Turbo accounts get
//! no ads. The token is stored DPAPI-protected for the current Windows user.

use crate::http::Http;
use crate::{Error, Result};
use std::fmt;

/// A validated login. `Debug` never prints the token.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Auth {
    pub token: String,
    pub login: String,
    pub user_id: String,
    /// The viewer's browser identity, so requests look like that browser.
    pub user_agent: Option<String>,
    /// Twitch's `unique_id` cookie of that browser (sent as the device id).
    pub device_id: Option<String>,
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth")
            .field("login", &self.login)
            .field("user_id", &self.user_id)
            .field("user_agent", &self.user_agent)
            .finish_non_exhaustive()
    }
}

/// What a pasted or dropped login contains.
#[derive(Clone, PartialEq, Eq)]
pub struct LoginInput {
    pub token: String,
    pub device_id: Option<String>,
}

impl fmt::Debug for LoginInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginInput").field("device_id", &self.device_id).finish_non_exhaustive()
    }
}

/// The token and, when present (cookie exports), the device id.
pub fn parse_login_input(input: &str) -> Option<LoginInput> {
    let token = parse_token_input(input)?;
    let device_id = find_cookie(input, "unique_id")
        .filter(|v| (8..=64).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_alphanumeric()));
    Some(LoginInput { token, device_id })
}

/// A twitch.tv cookie's value from a Netscape file, a JSON export or a
/// `Cookie:` header line.
fn find_cookie(input: &str, name: &str) -> Option<String> {
    let text = input.trim().trim_start_matches('\u{feff}');
    if text.starts_with('[') || text.starts_with('{') {
        let v = serde_json::from_str::<serde_json::Value>(text).ok()?;
        let list = v.as_array().or_else(|| v.get("cookies").and_then(|c| c.as_array()))?;
        return list.iter().find_map(|c| {
            let domain = c.get("domain").and_then(|n| n.as_str()).unwrap_or("twitch.tv");
            (c.get("name").and_then(|n| n.as_str()) == Some(name) && is_twitch_domain(domain))
                .then(|| c.get("value").and_then(|n| n.as_str()).map(str::to_owned))
                .flatten()
        });
    }
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        let fields: Vec<&str> = line.split('\t').collect();
        if !line.starts_with('#') && fields.len() >= 7 && fields[5] == name && is_twitch_domain(fields[0]) {
            return Some(fields[6].trim().to_owned());
        }
    }
    text.trim_start_matches("Cookie:").split(';').find_map(|part| {
        let (k, v) = part.split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_owned())
    })
}

fn plausible_token(s: &str) -> bool {
    (20..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Finds the `auth-token` for twitch.tv in whatever the viewer gives us:
/// a Netscape `cookies.txt`, a JSON cookie export, a `Cookie:` header line,
/// `oauth:<token>` or the bare token.
pub fn parse_token_input(input: &str) -> Option<String> {
    let text = input.trim().trim_start_matches('\u{feff}');
    if text.is_empty() {
        return None;
    }
    // Bare token or oauth:token.
    let bare = text.strip_prefix("oauth:").unwrap_or(text).trim();
    if plausible_token(bare) {
        return Some(bare.to_owned());
    }
    // JSON export: an array of {domain, name, value}, or {cookies: [...]}.
    if text.starts_with('[') || text.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
            let list = v.as_array().or_else(|| v.get("cookies").and_then(|c| c.as_array()));
            for c in list.into_iter().flatten() {
                let name = c.get("name").and_then(|n| n.as_str()).unwrap_or_default();
                let domain = c.get("domain").and_then(|n| n.as_str()).unwrap_or("twitch.tv");
                let value = c.get("value").and_then(|n| n.as_str()).unwrap_or_default();
                if name == "auth-token" && is_twitch_domain(domain) && plausible_token(value) {
                    return Some(value.to_owned());
                }
            }
        }
        return None;
    }
    // Netscape cookies.txt.
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() >= 7 && fields[5] == "auth-token" && is_twitch_domain(fields[0]) {
            let value = fields[6].trim();
            if plausible_token(value) {
                return Some(value.to_owned());
            }
        }
    }
    // Cookie header: "a=b; auth-token=xyz; c=d".
    for part in text.trim_start_matches("Cookie:").split(';') {
        if let Some((k, v)) = part.split_once('=')
            && k.trim() == "auth-token"
            && plausible_token(v.trim())
        {
            return Some(v.trim().to_owned());
        }
    }
    None
}

fn is_twitch_domain(domain: &str) -> bool {
    let d = domain.trim_start_matches('.');
    d == "twitch.tv" || d.ends_with(".twitch.tv")
}

/// Checks the token with Twitch and returns the account it belongs to.
pub fn validate(http: &Http, token: &str) -> Result<Auth> {
    let header = format!("OAuth {token}");
    let v = match http.get_json("https://id.twitch.tv/oauth2/validate", &[("Authorization", &header)]) {
        Err(Error::Status(401)) => return Err(Error::Auth("the token was rejected or has expired".into())),
        other => other?,
    };
    let login = v.get("login").and_then(|x| x.as_str()).ok_or_else(|| Error::Parse("no login".into()))?;
    let user_id = v.get("user_id").and_then(|x| x.as_str()).ok_or_else(|| Error::Parse("no user id".into()))?;
    Ok(Auth { token: token.to_owned(), login: login.to_owned(), user_id: user_id.to_owned(), ..Default::default() })
}

/// The stored login. It lives in Windows Credential Manager as a generic
/// credential `Glotzkiste/twitch` (encrypted for the Windows user, local to
/// this PC, visible and removable under Control Panel > Credential Manager).
/// A DPAPI-protected file (`%APPDATA%\Glotzkiste\auth.bin`) is only the
/// fallback if Credential Manager refuses a write; an existing file from an
/// older version is moved into Credential Manager on first load.
pub mod store {
    use super::Auth;
    use crate::{Error, Result};

    pub const TARGET: &str = "Glotzkiste/twitch";

    fn path() -> Option<std::path::PathBuf> {
        crate::config_dir().map(|d| d.join("auth.bin"))
    }

    fn encode(auth: &Auth) -> String {
        let clean = |s: &str| s.replace(['\n', '\r'], " ");
        format!(
            "{}\n{}\n{}\n{}\n{}",
            clean(&auth.token),
            clean(&auth.login),
            clean(&auth.user_id),
            clean(auth.user_agent.as_deref().unwrap_or_default()),
            clean(auth.device_id.as_deref().unwrap_or_default())
        )
    }

    fn decode(text: &str) -> Option<Auth> {
        let mut it = text.split('\n');
        let (token, login, user_id) = (it.next()?, it.next()?, it.next()?);
        if token.is_empty() || login.is_empty() {
            return None;
        }
        // Older entries end here; newer ones add the browser identity.
        let opt = |s: Option<&str>| s.map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
        Some(Auth {
            token: token.into(),
            login: login.into(),
            user_id: user_id.into(),
            user_agent: opt(it.next()),
            device_id: opt(it.next()),
        })
    }

    pub fn save(auth: &Auth) -> Result<()> {
        let plain = encode(auth);
        match cred::write(TARGET, &auth.login, plain.as_bytes()) {
            Ok(()) => {
                // Never leave a second copy behind.
                if let Some(p) = path() {
                    let _ = std::fs::remove_file(p);
                }
                Ok(())
            }
            Err(e) => {
                log::warn!("Credential Manager refused the login ({e}); keeping it in a protected file");
                save_file(&plain)
            }
        }
    }

    fn save_file(plain: &str) -> Result<()> {
        let sealed = protect(plain.as_bytes())?;
        let path = path().ok_or_else(|| Error::Io("no settings folder".into()))?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, sealed).map_err(|e| Error::Io(e.to_string()))?;
        std::fs::rename(&tmp, &path).map_err(|e| Error::Io(e.to_string()))
    }

    fn load_file() -> Option<Auth> {
        let sealed = std::fs::read(path()?).ok()?;
        let plain = unprotect(&sealed).ok()?;
        decode(&String::from_utf8(plain).ok()?)
    }

    pub fn load() -> Option<Auth> {
        if let Some(a) = cred::read(TARGET).and_then(|b| String::from_utf8(b).ok()).and_then(|t| decode(&t)) {
            return Some(a);
        }
        // A login from an older version (or the fallback file): move it into
        // Credential Manager; the file is removed only once that worked.
        let a = load_file()?;
        if cred::write(TARGET, &a.login, encode(&a).as_bytes()).is_ok()
            && let Some(p) = path()
        {
            let _ = std::fs::remove_file(p);
        }
        Some(a)
    }

    pub fn clear() {
        cred::delete(TARGET);
        if let Some(p) = path() {
            let _ = std::fs::remove_file(p);
        }
    }

    #[cfg(windows)]
    mod cred {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::Security::Credentials::{
            CRED_FLAGS, CRED_MAX_CREDENTIAL_BLOB_SIZE, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW,
            CredDeleteW, CredFree, CredReadW, CredWriteW,
        };
        use windows::core::{HSTRING, PWSTR};

        pub fn write(target: &str, user: &str, blob: &[u8]) -> Result<(), String> {
            if blob.len() > CRED_MAX_CREDENTIAL_BLOB_SIZE as usize {
                return Err("too large".into());
            }
            let mut target_w: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
            let mut user_w: Vec<u16> = user.encode_utf16().chain(std::iter::once(0)).collect();
            let mut comment_w: Vec<u16> = "Glotzkiste: Twitch login".encode_utf16().chain(std::iter::once(0)).collect();
            let cred = CREDENTIALW {
                Flags: CRED_FLAGS(0),
                Type: CRED_TYPE_GENERIC,
                TargetName: PWSTR(target_w.as_mut_ptr()),
                Comment: PWSTR(comment_w.as_mut_ptr()),
                LastWritten: FILETIME::default(),
                CredentialBlobSize: blob.len() as u32,
                CredentialBlob: blob.as_ptr().cast_mut(),
                Persist: CRED_PERSIST_LOCAL_MACHINE,
                AttributeCount: 0,
                Attributes: std::ptr::null_mut(),
                TargetAlias: PWSTR::null(),
                UserName: PWSTR(user_w.as_mut_ptr()),
            };
            // SAFETY: every pointer refers to a live local buffer for the call;
            // the system copies the data.
            unsafe { CredWriteW(&cred, 0) }.map_err(|e| e.to_string())
        }

        pub fn read(target: &str) -> Option<Vec<u8>> {
            let name = HSTRING::from(target);
            let mut out: *mut CREDENTIALW = std::ptr::null_mut();
            // SAFETY: on success `out` points at a system-allocated credential
            // that we copy from and free with CredFree.
            unsafe {
                CredReadW(&name, CRED_TYPE_GENERIC, None, &mut out).ok()?;
                if out.is_null() {
                    return None;
                }
                let c = &*out;
                let blob = if c.CredentialBlob.is_null() {
                    Vec::new()
                } else {
                    std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec()
                };
                CredFree(out.cast());
                Some(blob)
            }
        }

        pub fn delete(target: &str) {
            let name = HSTRING::from(target);
            // SAFETY: a plain call with a valid string.
            let _ = unsafe { CredDeleteW(&name, CRED_TYPE_GENERIC, None) };
        }
    }

    #[cfg(not(windows))]
    mod cred {
        pub fn write(_: &str, _: &str, _: &[u8]) -> Result<(), String> {
            Err("Windows only".into())
        }
        pub fn read(_: &str) -> Option<Vec<u8>> {
            None
        }
        pub fn delete(_: &str) {}
    }

    #[cfg(windows)]
    fn protect(data: &[u8]) -> Result<Vec<u8>> {
        dpapi(data, true)
    }

    #[cfg(windows)]
    fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
        dpapi(data, false)
    }

    #[cfg(windows)]
    fn dpapi(data: &[u8], seal: bool) -> Result<Vec<u8>> {
        use windows::Win32::Foundation::{HLOCAL, LocalFree};
        use windows::Win32::Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        };
        use windows::core::PCWSTR;
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr().cast_mut() };
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: `input` points at `data` for the call; `out` is filled by
        // the system and freed with LocalFree below.
        let r = unsafe {
            if seal {
                CryptProtectData(&input, PCWSTR::null(), None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            } else {
                CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            }
        };
        r.map_err(|e| Error::Io(format!("DPAPI: {e}")))?;
        // SAFETY: on success `out` holds cbData bytes at pbData.
        let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        // SAFETY: pbData was allocated by the system with LocalAlloc.
        unsafe { LocalFree(Some(HLOCAL(out.pbData.cast()))) };
        Ok(bytes)
    }

    #[cfg(not(windows))]
    fn protect(_: &[u8]) -> Result<Vec<u8>> {
        Err(Error::Io("protected storage is Windows-only".into()))
    }

    #[cfg(not(windows))]
    fn unprotect(_: &[u8]) -> Result<Vec<u8>> {
        Err(Error::Io("protected storage is Windows-only".into()))
    }

    #[cfg(all(test, windows))]
    mod tests {
        use super::*;

        #[test]
        fn dpapi_round_trips() {
            let sealed = protect(b"fake-token-for-test").expect("protect");
            assert_ne!(sealed, b"fake-token-for-test");
            assert_eq!(unprotect(&sealed).expect("unprotect"), b"fake-token-for-test");
        }

        #[test]
        fn credential_manager_round_trips() {
            // A throwaway entry with fake data, removed at the end.
            let target = format!("Glotzkiste/test-{}", crate::random_hex(12));
            let a = Auth {
                token: "faketokenfaketokenfaketoken123".into(),
                login: "someone".into(),
                user_id: "42".into(),
                user_agent: Some("Mozilla/5.0 test".into()),
                device_id: None,
            };
            cred::write(&target, &a.login, encode(&a).as_bytes()).expect("write");
            let back = cred::read(&target).and_then(|b| String::from_utf8(b).ok()).and_then(|t| decode(&t));
            cred::delete(&target);
            assert_eq!(back, Some(a));
            assert!(cred::read(&target).is_none(), "deleted");
        }

        #[test]
        fn old_three_line_entries_still_load() {
            let a = decode("tok\nme\n7").unwrap();
            assert_eq!((a.login.as_str(), a.user_agent.clone(), a.device_id.clone()), ("me", None, None));
            assert!(decode("\nme\n7").is_none());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "abcdefghijklmnopqrstuvwxyz0123";

    #[test]
    fn bare_and_oauth_tokens() {
        assert_eq!(parse_token_input(T).as_deref(), Some(T));
        assert_eq!(parse_token_input(&format!("  oauth:{T}\n")).as_deref(), Some(T));
        assert_eq!(parse_token_input("hello world"), None);
    }

    #[test]
    fn netscape_cookies_txt() {
        let txt = format!(
            "# Netscape HTTP Cookie File\n.example.com\tTRUE\t/\tTRUE\t0\tauth-token\tzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz\n#HttpOnly_.twitch.tv\tTRUE\t/\tTRUE\t1893456000\tauth-token\t{T}\r\n.twitch.tv\tTRUE\t/\tTRUE\t0\tlogin\tsomeone\n"
        );
        assert_eq!(parse_token_input(&txt).as_deref(), Some(T));
    }

    #[test]
    fn json_export_and_header() {
        let json = format!(r#"[{{"domain":".twitch.tv","name":"auth-token","value":"{T}"}}]"#);
        assert_eq!(parse_token_input(&json).as_deref(), Some(T));
        let header = format!("Cookie: unique_id=1; auth-token={T}; x=y");
        assert_eq!(parse_token_input(&header).as_deref(), Some(T));
    }

    #[test]
    fn debug_hides_the_token() {
        let a = Auth { token: T.into(), login: "me".into(), user_id: "1".into(), ..Default::default() };
        assert!(!format!("{a:?}").contains(T));
        let i = parse_login_input(T).unwrap();
        assert!(!format!("{i:?}").contains(T));
    }

    #[test]
    fn device_id_comes_from_the_unique_id_cookie() {
        let txt = format!(
            ".twitch.tv\tTRUE\t/\tTRUE\t0\tunique_id\tAbCdEf0123456789\n.twitch.tv\tTRUE\t/\tTRUE\t0\tauth-token\t{T}\n"
        );
        let i = parse_login_input(&txt).unwrap();
        assert_eq!((i.token.as_str(), i.device_id.as_deref()), (T, Some("AbCdEf0123456789")));
        // A bare token has no device id; a cookie from another site is ignored.
        assert_eq!(parse_login_input(T).unwrap().device_id, None);
        let other = format!(
            ".example.com\tTRUE\t/\tTRUE\t0\tunique_id\tAbCdEf0123456789\n.twitch.tv\tTRUE\t/\tTRUE\t0\tauth-token\t{T}\n"
        );
        assert_eq!(parse_login_input(&other).unwrap().device_id, None);
    }
}
