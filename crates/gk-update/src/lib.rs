//! Update checks for Glotzkiste: release discovery on GitHub, version order,
//! channel choice, checksum verification and bounded HTTPS downloads.
//! (Ported from the owner's Nitrate player, same contract.)
//!
//! The parsing and selection logic has no I/O and is unit-tested. Network
//! access goes through the HTTPS client the app already uses (ureq, rustls),
//! HTTPS only, size-capped; only this repository's GitHub API and release
//! downloads are ever fetched.

use serde_json::Value;
use std::cmp::Ordering;
use std::fmt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const REPO: &str = "rennerdo30/glotzkiste";
pub const RELEASE_PAGE: &str = "https://github.com/rennerdo30/glotzkiste/releases";
const API_URL: &str = "https://api.github.com/repos/rennerdo30/glotzkiste/releases?per_page=30";
const DOWNLOAD_PREFIX: &str = "https://github.com/rennerdo30/glotzkiste/releases/download/";
pub const SUMS: &str = "SHA256SUMS.txt";
pub const MAX_METADATA: usize = 8 * 1024 * 1024;
pub const MAX_SETUP: usize = 200 * 1024 * 1024;
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn err(s: impl Into<String>) -> Error {
    Error(s.into())
}

/// A SemVer version, `MAJOR.MINOR.PATCH[-PRE]`, no build metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Vec<Ident>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Ident {
    Num(u64),
    Text(String),
}

impl Version {
    /// Strict parse; a leading `v` (tags) is accepted.
    pub fn parse(input: &str) -> Result<Version, Error> {
        let input = input.strip_prefix('v').unwrap_or(input);
        if input.len() > 64 {
            return Err(err("version is too long"));
        }
        let (core, pre) = match input.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (input, None),
        };
        let mut parts = core.split('.');
        let mut number = || -> Result<u64, Error> {
            let s = parts.next().ok_or_else(|| err("version needs major.minor.patch"))?;
            if s.is_empty() || (s.len() > 1 && s.starts_with('0')) || !s.bytes().all(|c| c.is_ascii_digit()) {
                return Err(err("invalid numeric version component"));
            }
            s.parse().map_err(|_| err("version component is too large"))
        };
        let (major, minor, patch) = (number()?, number()?, number()?);
        if parts.next().is_some() {
            return Err(err("version needs exactly three numeric components"));
        }
        let mut ids = Vec::new();
        if let Some(pre) = pre {
            for part in pre.split('.') {
                if part.is_empty() || !part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
                    return Err(err("invalid prerelease identifier"));
                }
                if part.bytes().all(|c| c.is_ascii_digit()) {
                    if part.len() > 1 && part.starts_with('0') {
                        return Err(err("numeric prerelease identifier has a leading zero"));
                    }
                    ids.push(Ident::Num(part.parse().map_err(|_| err("prerelease number is too large"))?));
                } else {
                    ids.push(Ident::Text(part.to_owned()));
                }
            }
        }
        Ok(Version { major, minor, patch, pre: ids })
    }

    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

impl Ord for Version {
    /// SemVer precedence: a release ranks above its prereleases; numeric
    /// identifiers compare numerically and below text identifiers.
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch)).then_with(|| {
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        let c = match (a, b) {
                            (Ident::Num(x), Ident::Num(y)) => x.cmp(y),
                            (Ident::Num(_), Ident::Text(_)) => Ordering::Less,
                            (Ident::Text(_), Ident::Num(_)) => Ordering::Greater,
                            (Ident::Text(x), Ident::Text(y)) => x.cmp(y),
                        };
                        if c != Ordering::Equal {
                            return c;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            }
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        for (i, p) in self.pre.iter().enumerate() {
            f.write_str(if i == 0 { "-" } else { "." })?;
            match p {
                Ident::Num(n) => write!(f, "{n}")?,
                Ident::Text(s) => f.write_str(s)?,
            }
        }
        Ok(())
    }
}

/// Which releases an install is offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Published stable releases only.
    Stable,
    /// The newest published release, prerelease or stable.
    Beta,
}

impl Channel {
    /// A prerelease build follows Beta, a stable build follows Stable.
    pub fn for_version(v: &Version) -> Channel {
        if v.is_prerelease() { Channel::Beta } else { Channel::Stable }
    }

    pub fn from_setting(s: Option<&str>, current: &Version) -> Channel {
        match s {
            Some("stable") => Channel::Stable,
            Some("beta") => Channel::Beta,
            _ => Channel::for_version(current),
        }
    }
}

pub fn setup_name(v: &Version) -> String {
    format!("glotzkiste-{v}-setup.exe")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub version: Version,
    pub setup_url: String,
    pub sums_url: String,
}

/// The newest release newer than `current` that `channel` offers, from the
/// GitHub releases list. Drafts, unparseable tags, downgrades and releases
/// without the setup and checksum assets are skipped; Stable never takes a
/// prerelease.
pub fn pick(current: &Version, channel: Channel, body: &[u8]) -> Result<Option<Candidate>, Error> {
    if body.len() > MAX_METADATA {
        return Err(err("release metadata exceeds 8 MiB"));
    }
    let v: Value = serde_json::from_slice(body).map_err(|e| err(format!("unexpected answer from GitHub: {e}")))?;
    let list = v.as_array().ok_or_else(|| err("unexpected answer from GitHub: not a list"))?;
    let mut best: Option<Candidate> = None;
    for rel in list {
        if rel.get("draft").and_then(Value::as_bool) != Some(false) {
            continue;
        }
        let Some(version) = rel.get("tag_name").and_then(Value::as_str).and_then(|t| Version::parse(t).ok()) else {
            continue;
        };
        let pre = rel.get("prerelease").and_then(Value::as_bool).unwrap_or(false);
        if version <= *current || (channel == Channel::Stable && (pre || version.is_prerelease())) {
            continue;
        }
        let Some(assets) = rel.get("assets").and_then(Value::as_array) else { continue };
        let url = |name: &str| {
            assets
                .iter()
                .find(|a| a.get("name").and_then(Value::as_str) == Some(name))
                .and_then(|a| a.get("browser_download_url"))
                .and_then(Value::as_str)
                .filter(|u| u.starts_with(DOWNLOAD_PREFIX))
                .map(str::to_owned)
        };
        let (Some(setup_url), Some(sums_url)) = (url(&setup_name(&version)), url(SUMS)) else { continue };
        if best.as_ref().is_none_or(|b| version > b.version) {
            best = Some(Candidate { version, setup_url, sums_url });
        }
    }
    Ok(best)
}

/// The expected SHA-256 (lowercase hex) of `name` in a sha256sum file.
/// Exactly one well-formed line for the name is required.
pub fn expected_hash(sums: &str, name: &str) -> Result<String, Error> {
    let mut found = Vec::new();
    for line in sums.lines() {
        let line = line.trim_end_matches('\r');
        let Some((hash, file)) = line.split_once(' ') else { continue };
        let file = file.trim_start_matches([' ', '*']);
        if file == name {
            if hash.len() != 64 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
                return Err(err(format!("{SUMS} has a malformed entry for {name}")));
            }
            found.push(hash.to_ascii_lowercase());
        }
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(err(format!("{SUMS} has no checksum for {name}"))),
        _ => Err(err(format!("{SUMS} lists {name} more than once"))),
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in digest.iter() {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Whether a daily check is due, from the timestamp file of the last one.
pub fn due(stamp: &Path, now: SystemTime) -> bool {
    let last = std::fs::read_to_string(stamp).ok().and_then(|s| s.trim().parse::<u64>().ok());
    let now = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    last.is_none_or(|l| now.saturating_sub(l) >= CHECK_INTERVAL.as_secs() || l > now)
}

pub fn write_stamp(stamp: &Path, now: SystemTime) -> Result<(), Error> {
    if let Some(dir) = stamp.parent() {
        std::fs::create_dir_all(dir).map_err(|e| err(e.to_string()))?;
    }
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    std::fs::write(stamp, secs.to_string()).map_err(|e| err(e.to_string()))
}

/// Asks GitHub for the release list and picks a candidate.
pub fn check(current: &Version, channel: Channel) -> Result<Option<Candidate>, Error> {
    let body = fetch(API_URL, MAX_METADATA)?;
    pick(current, channel, &body)
}

/// Downloads `SHA256SUMS.txt` and the setup of `c`, and returns the setup
/// bytes only when they match their published checksum.
pub fn download_verified(c: &Candidate) -> Result<Vec<u8>, Error> {
    let sums = fetch(&c.sums_url, 1024 * 1024)?;
    let sums = String::from_utf8(sums).map_err(|_| err(format!("{SUMS} is not text")))?;
    let expected = expected_hash(&sums, &setup_name(&c.version))?;
    let setup = fetch(&c.setup_url, MAX_SETUP)?;
    if sha256_hex(&setup) != expected {
        return Err(err("the downloaded setup does not match its published checksum, so it was not run"));
    }
    Ok(setup)
}

fn allowed(url: &str) -> bool {
    url.starts_with("https://api.github.com/repos/rennerdo30/glotzkiste/") || url.starts_with(DOWNLOAD_PREFIX)
}

fn fetch(url: &str, limit: usize) -> Result<Vec<u8>, Error> {
    if !allowed(url) {
        return Err(err("refusing a URL outside Glotzkiste's GitHub releases"));
    }
    let config = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(Duration::from_secs(60)))
        .user_agent(concat!("glotzkiste/", env!("CARGO_PKG_VERSION")))
        .max_redirects(5)
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let mut resp = agent
        .get(url)
        .header("Accept", "application/vnd.github+json, application/octet-stream")
        .call()
        .map_err(|e| err(format!("download failed: {e}")))?;
    resp.body_mut().with_config().limit(limit as u64).read_to_vec().map_err(|e| err(format!("download failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn semver_order() {
        let order =
            ["0.1.0-alpha", "0.1.0-beta.2", "0.1.0-beta.10", "0.1.0-poc", "0.1.0-rc.1", "0.1.0", "0.1.1", "1.0.0"];
        for p in order.windows(2) {
            assert!(v(p[0]) < v(p[1]), "{} < {}", p[0], p[1]);
        }
        assert_eq!(v("v0.1.0-beta.12").to_string(), "0.1.0-beta.12");
        for bad in ["1.0", "01.0.0", "1.0.0-01", "1.0.0-", "1.0.0-a..b", "1.0.0.0", "x.y.z", ""] {
            assert!(Version::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(Channel::for_version(&v("0.1.0-poc")), Channel::Beta);
        assert_eq!(Channel::from_setting(Some("stable"), &v("0.1.0-poc")), Channel::Stable);
    }

    fn release(tag: &str, pre: bool, draft: bool, assets: bool) -> String {
        let ver = tag.trim_start_matches('v');
        let a = if assets {
            format!(
                r#"[{{"name":"glotzkiste-{ver}-setup.exe","browser_download_url":"{DOWNLOAD_PREFIX}{tag}/glotzkiste-{ver}-setup.exe"}},{{"name":"SHA256SUMS.txt","browser_download_url":"{DOWNLOAD_PREFIX}{tag}/SHA256SUMS.txt"}}]"#
            )
        } else {
            "[]".into()
        };
        format!(r#"{{"tag_name":"{tag}","prerelease":{pre},"draft":{draft},"assets":{a}}}"#)
    }

    #[test]
    fn channels_pick_the_right_release() {
        let list = format!(
            "[{},{},{},{},{}]",
            release("v0.3.0-beta.1", true, false, true),
            release("v0.2.0", false, false, true),
            release("v0.9.0", false, true, true),
            release("v0.4.0", false, false, false),
            release("not-a-version", false, false, true),
        );
        let cur = v("0.1.0-poc");
        let beta = pick(&cur, Channel::Beta, list.as_bytes()).unwrap().unwrap();
        assert_eq!(beta.version, v("0.3.0-beta.1"));
        let stable = pick(&cur, Channel::Stable, list.as_bytes()).unwrap().unwrap();
        assert_eq!(stable.version, v("0.2.0"), "drafts, prereleases and releases without assets are skipped");
        assert!(pick(&v("0.3.0"), Channel::Beta, list.as_bytes()).unwrap().is_none(), "never a downgrade");
        assert!(pick(&cur, Channel::Beta, b"{oops").is_err());
        let foreign = r#"[{"tag_name":"v1.0.0","prerelease":false,"draft":false,"assets":[{"name":"glotzkiste-1.0.0-setup.exe","browser_download_url":"https://evil.example/x"},{"name":"SHA256SUMS.txt","browser_download_url":"https://evil.example/y"}]}]"#;
        assert!(
            pick(&cur, Channel::Stable, foreign.as_bytes()).unwrap().is_none(),
            "URLs outside Glotzkiste's releases are refused"
        );
    }

    #[test]
    fn checksum_file() {
        let h = "a".repeat(64);
        let sums = format!("{h}  glotzkiste-1.0.0-setup.exe\r\n{}  other.zip\n", "b".repeat(64));
        assert_eq!(expected_hash(&sums, "glotzkiste-1.0.0-setup.exe").unwrap(), h);
        assert!(expected_hash(&sums, "missing.exe").is_err());
        assert!(expected_hash(&format!("{h}  x\n{h}  x\n"), "x").is_err());
        assert!(expected_hash("short  x", "x").is_err());
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    /// Talks to GitHub; run by hand with `cargo test -p glotzkiste-update -- --ignored`.
    #[test]
    #[ignore]
    fn live_release_list() {
        let r = check(&v("0.0.1"), Channel::Beta);
        assert!(r.is_ok(), "{r:?}");
        assert!(fetch("https://example.com/", 10).is_err(), "foreign hosts are refused");
    }

    #[test]
    fn daily_stamp() {
        let dir = std::env::temp_dir().join(format!("glotzkiste-update-test-{}", std::process::id()));
        let stamp = dir.join("update-check");
        let t0 = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert!(due(&stamp, t0));
        write_stamp(&stamp, t0).unwrap();
        assert!(!due(&stamp, t0 + Duration::from_secs(3600)));
        assert!(due(&stamp, t0 + Duration::from_secs(25 * 3600)));
        assert!(due(&stamp, t0 - Duration::from_secs(10)), "a stamp from the future does not block checks");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
