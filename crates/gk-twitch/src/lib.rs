//! Twitch access for Glotzkiste: login, channel info, playback URLs, badge
//! and third-party emote sets, image download and decode.
//!
//! Everything here blocks; the app calls it from worker threads only
//! (AGENTS.md performance rule 1). Nothing panics on network data, and the
//! login token never reaches a log line or a `Debug` print.

pub mod api;
pub mod auth;
pub mod emotes;
pub mod http;
pub mod image;
pub mod live;
pub mod playlist;

pub use api::{Badge, ChannelInfo, Client, Playback};
pub use auth::Auth;
pub use emotes::{Emote, Provider};
pub use http::Http;
pub use playlist::Variant;

use std::fmt;

/// Every failure in this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Connection, TLS or timeout problem.
    Network(String),
    /// The server answered with this HTTP status.
    Status(u16),
    /// The answer was not in the expected shape.
    Parse(String),
    /// The channel exists but is not live.
    Offline,
    /// No such channel or user.
    NotFound,
    /// Login token missing, expired or rejected.
    Auth(String),
    /// Twitch refused the request (for example a GQL error message).
    Refused(String),
    /// Local file problem (cache, stored login).
    Io(String),
    /// An image could not be decoded or broke a size cap.
    Image(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Network(e) => write!(f, "network error: {e}"),
            Error::Status(s) => write!(f, "server answered {s}"),
            Error::Parse(e) => write!(f, "unexpected answer: {e}"),
            Error::Offline => f.write_str("offline"),
            Error::NotFound => f.write_str("no such channel"),
            Error::Auth(e) => write!(f, "login problem: {e}"),
            Error::Refused(e) => write!(f, "refused: {e}"),
            Error::Io(e) => write!(f, "file error: {e}"),
            Error::Image(e) => write!(f, "image error: {e}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// `%APPDATA%\Glotzkiste` (settings, login), created on demand.
pub fn config_dir() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var_os("APPDATA")?).join("Glotzkiste");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// `%LOCALAPPDATA%\Glotzkiste` (caches, extracted libraries), created on demand.
pub fn local_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("APPDATA"))?;
    let dir = std::path::PathBuf::from(base).join("Glotzkiste");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// A 64-bit FNV-1a hash: stable cache file names without a crypto dependency.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Random hex from the OS-seeded std hasher (device ids, IRC nick numbers).
pub fn random_hex(len: usize) -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::with_capacity(len);
    let mut counter = 0u64;
    while out.len() < len {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(counter);
        h.write_u128(
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0),
        );
        out.push_str(&format!("{:016x}", h.finish()));
        counter += 1;
    }
    out.truncate(len);
    out
}
