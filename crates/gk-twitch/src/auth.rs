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
#[derive(Clone, PartialEq, Eq)]
pub struct Auth {
    pub token: String,
    pub login: String,
    pub user_id: String,
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth").field("login", &self.login).field("user_id", &self.user_id).finish_non_exhaustive()
    }
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
        if let Some((k, v)) = part.split_once('=') {
            if k.trim() == "auth-token" && plausible_token(v.trim()) {
                return Some(v.trim().to_owned());
            }
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
    Ok(Auth { token: token.to_owned(), login: login.to_owned(), user_id: user_id.to_owned() })
}

/// The stored login, DPAPI-protected (`%APPDATA%\Glotzkiste\auth.bin`).
pub mod store {
    use super::Auth;
    use crate::{Error, Result};

    fn path() -> Option<std::path::PathBuf> {
        crate::config_dir().map(|d| d.join("auth.bin"))
    }

    pub fn save(auth: &Auth) -> Result<()> {
        let plain = format!("{}\n{}\n{}", auth.token, auth.login, auth.user_id);
        let sealed = protect(plain.as_bytes())?;
        let path = path().ok_or_else(|| Error::Io("no settings folder".into()))?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, sealed).map_err(|e| Error::Io(e.to_string()))?;
        std::fs::rename(&tmp, &path).map_err(|e| Error::Io(e.to_string()))
    }

    pub fn load() -> Option<Auth> {
        let sealed = std::fs::read(path()?).ok()?;
        let plain = unprotect(&sealed).ok()?;
        let text = String::from_utf8(plain).ok()?;
        let mut it = text.split('\n');
        let (token, login, user_id) = (it.next()?, it.next()?, it.next()?);
        if token.is_empty() || login.is_empty() {
            return None;
        }
        Some(Auth { token: token.into(), login: login.into(), user_id: user_id.into() })
    }

    pub fn clear() {
        if let Some(p) = path() {
            let _ = std::fs::remove_file(p);
        }
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
        #[test]
        fn dpapi_round_trips() {
            let sealed = super::protect(b"fake-token-for-test").expect("protect");
            assert_ne!(sealed, b"fake-token-for-test");
            assert_eq!(super::unprotect(&sealed).expect("unprotect"), b"fake-token-for-test");
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
        let a = Auth { token: T.into(), login: "me".into(), user_id: "1".into() };
        assert!(!format!("{a:?}").contains(T));
    }
}
