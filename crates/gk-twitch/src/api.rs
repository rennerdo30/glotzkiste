//! Twitch's web GQL endpoint and the usher playlist service.

use crate::auth::Auth;
use crate::http::Http;
use crate::playlist::{self, Variant};
use crate::{Error, Result};
use serde_json::{Value, json};

/// The web player's client id (as used by the maintained open-source players).
pub const CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";
const GQL: &str = "https://gql.twitch.tv/gql";
const PLAYBACK_TOKEN_HASH: &str = "ed230aa1e33e07eebb8928504583da78a5173989fadfb1ac94be06a04f3cdbe9";

/// What the viewer needs to know about a channel.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChannelInfo {
    pub id: String,
    pub login: String,
    pub display_name: String,
    pub live: bool,
    pub title: String,
    pub category: String,
    pub viewers: u64,
    pub avatar_url: String,
}

/// A live stream's playable variants and the account's ad status.
#[derive(Debug, Clone, PartialEq)]
pub struct Playback {
    pub variants: Vec<Variant>,
    /// The playback token says this account has Turbo.
    pub turbo: bool,
    /// The playback token says ads may be shown.
    pub ads_expected: bool,
}

/// A chat badge image.
#[derive(Debug, Clone, PartialEq)]
pub struct Badge {
    pub set: String,
    pub version: String,
    pub url: String,
    pub title: String,
}

/// Twitch access, anonymous or with the viewer's login. Cheap to clone.
#[derive(Clone)]
pub struct Client {
    http: Http,
    auth: Option<Auth>,
    device_id: String,
}

const USER_FIELDS: &str =
    "id login displayName profileImageURL(width: 70) stream { id title viewersCount game { displayName } }";

fn parse_user(u: &Value) -> Option<ChannelInfo> {
    let s = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    let stream = u.get("stream").filter(|s| !s.is_null());
    Some(ChannelInfo {
        id: s(u.get("id")?),
        login: s(u.get("login")?),
        display_name: s(&u["displayName"]),
        live: stream.is_some(),
        title: stream.map(|st| s(&st["title"])).unwrap_or_default(),
        category: stream.map(|st| s(&st["game"]["displayName"])).unwrap_or_default(),
        viewers: stream.and_then(|st| st["viewersCount"].as_u64()).unwrap_or(0),
        avatar_url: s(&u["profileImageURL"]),
    })
}

/// A channel login is 1 to 25 letters, digits or underscores; anything else
/// never reaches a query or a URL.
pub fn valid_login(login: &str) -> bool {
    (1..=25).contains(&login.len()) && login.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

impl Client {
    pub fn new(http: Http, auth: Option<Auth>) -> Self {
        let device_id = auth.as_ref().and_then(|a| a.device_id.clone()).unwrap_or_else(|| crate::random_hex(32));
        Self { http, auth, device_id }
    }

    pub fn http(&self) -> &Http {
        &self.http
    }

    pub fn auth(&self) -> Option<&Auth> {
        self.auth.as_ref()
    }

    fn gql(&self, body: &Value) -> Result<Value> {
        let auth_header = self.auth.as_ref().map(|a| format!("OAuth {}", a.token));
        let mut headers: Vec<(&str, &str)> = vec![("Client-ID", CLIENT_ID), ("X-Device-Id", &self.device_id)];
        if let Some(h) = &auth_header {
            headers.push(("Authorization", h));
        }
        let v = self.http.post_json(GQL, &headers, body, &[400, 401, 403])?;
        if let Some(msg) = v.get("error").and_then(Value::as_str) {
            let detail = v.get("message").and_then(Value::as_str).unwrap_or_default();
            return Err(if detail.contains("token") || msg == "Unauthorized" {
                Error::Auth(format!("{msg}: {detail}"))
            } else {
                Error::Refused(format!("{msg}: {detail}"))
            });
        }
        if v.get("data").is_none()
            && let Some(m) = v["errors"][0]["message"].as_str()
        {
            return Err(Error::Refused(m.to_owned()));
        }
        Ok(v)
    }

    fn query(&self, q: &str) -> Result<Value> {
        let v = self.gql(&json!({ "query": q }))?;
        if let Some(m) = v["errors"][0]["message"].as_str()
            && v["data"].is_null()
        {
            return Err(Error::Refused(m.to_owned()));
        }
        Ok(v)
    }

    /// One channel.
    pub fn channel(&self, login: &str) -> Result<ChannelInfo> {
        self.channels(&[login])?.into_iter().next().ok_or(Error::NotFound)
    }

    /// Several channels in one request (unknown logins are left out).
    pub fn channels(&self, logins: &[&str]) -> Result<Vec<ChannelInfo>> {
        let list: Vec<String> = logins
            .iter()
            .map(|l| l.to_ascii_lowercase())
            .filter(|l| valid_login(l))
            .map(|l| format!("\"{l}\""))
            .collect();
        if list.is_empty() {
            return Ok(Vec::new());
        }
        let v = self.query(&format!("query {{ users(logins: [{}]) {{ {USER_FIELDS} }} }}", list.join(",")))?;
        Ok(v["data"]["users"].as_array().into_iter().flatten().filter_map(parse_user).collect())
    }

    /// Live channels the logged-in viewer follows, most viewers first.
    pub fn followed_live(&self) -> Result<Vec<ChannelInfo>> {
        if self.auth.is_none() {
            return Err(Error::Auth("not logged in".into()));
        }
        let v = self.query(&format!(
            "query {{ currentUser {{ followedLiveUsers(first: 100) {{ edges {{ node {{ {USER_FIELDS} }} }} }} }} }}"
        ))?;
        let mut out: Vec<ChannelInfo> = v["data"]["currentUser"]["followedLiveUsers"]["edges"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| parse_user(&e["node"]))
            .collect();
        out.sort_by_key(|c| std::cmp::Reverse(c.viewers));
        Ok(out)
    }

    /// The playable variants of a live channel. Logged in, the playback token
    /// carries the account's Turbo status, which removes the ads.
    pub fn playback(&self, login: &str) -> Result<Playback> {
        let login = login.to_ascii_lowercase();
        if !valid_login(&login) {
            return Err(Error::NotFound);
        }
        let body = json!({
            "operationName": "PlaybackAccessToken",
            "extensions": { "persistedQuery": { "version": 1, "sha256Hash": PLAYBACK_TOKEN_HASH } },
            "variables": {
                "isLive": true, "login": login, "isVod": false, "vodID": "",
                "playerType": "embed", "platform": "site"
            }
        });
        let v = self.gql(&body)?;
        let t = &v["data"]["streamPlaybackAccessToken"];
        if t.is_null() {
            if let Some(m) = v["errors"][0]["message"].as_str() {
                return Err(Error::Refused(m.to_owned()));
            }
            return Err(Error::Offline);
        }
        let (Some(value), Some(sig)) = (t["value"].as_str(), t["signature"].as_str()) else {
            return Err(Error::Parse("playback token without value or signature".into()));
        };
        let token: Value = serde_json::from_str(value).unwrap_or(Value::Null);
        let turbo = token["turbo"].as_bool().unwrap_or(false);
        let ads_expected = token["show_ads"].as_bool().unwrap_or(true) && !token["hide_ads"].as_bool().unwrap_or(false);

        let url = usher_url(&login, sig, value);
        let headers = [("Referer", "https://player.twitch.tv"), ("Origin", "https://player.twitch.tv")];
        let text = match self.http.get_text(&url, &headers) {
            Err(Error::Status(404)) => return Err(Error::Offline),
            Err(Error::Status(403)) => return Err(Error::Refused("this stream is not available here".into())),
            other => other?,
        };
        let variants = playlist::parse_master(&text);
        if variants.is_empty() {
            return Err(Error::Offline);
        }
        Ok(Playback { variants, turbo, ads_expected })
    }

    /// Global and channel chat badges, keyed by set and version.
    pub fn badges(&self, channel_id: Option<&str>) -> Result<Vec<Badge>> {
        let fields = "setID version imageURL(size: DOUBLE) title";
        let channel_part = match channel_id {
            Some(id) if id.bytes().all(|b| b.is_ascii_digit()) && !id.is_empty() => {
                format!("user(id: \"{id}\") {{ broadcastBadges {{ {fields} }} }}")
            }
            _ => String::new(),
        };
        let v = self.query(&format!("query {{ badges {{ {fields} }} {channel_part} }}"))?;
        let parse = |b: &Value| {
            Some(Badge {
                set: b["setID"].as_str()?.to_owned(),
                version: b["version"].as_str()?.to_owned(),
                url: b["imageURL"].as_str()?.to_owned(),
                title: b["title"].as_str().unwrap_or_default().to_owned(),
            })
        };
        let mut out: Vec<Badge> = v["data"]["badges"].as_array().into_iter().flatten().filter_map(parse).collect();
        // Channel badges (subscriber, bits) override the global ones.
        out.extend(v["data"]["user"]["broadcastBadges"].as_array().into_iter().flatten().filter_map(parse));
        Ok(out)
    }
}

fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn usher_url(login: &str, sig: &str, token: &str) -> String {
    let p = u32::from_str_radix(&crate::random_hex(5), 16).unwrap_or(0) % 999_999;
    format!(
        "https://usher.ttvnw.net/api/v2/channel/hls/{login}.m3u8?platform=web&p={p}&allow_source=true&allow_audio_only=true\
         &playlist_include_framerate=true&multigroup_video=true&supported_codecs=h264&fast_bread=true&sig={}&token={}",
        enc(sig),
        enc(token)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logins_are_checked_before_use() {
        assert!(valid_login("some_streamer42"));
        assert!(!valid_login("a\" } evil { \""));
        assert!(!valid_login(""));
        assert!(!valid_login(&"x".repeat(26)));
    }

    #[test]
    fn user_parsing_handles_offline_and_live() {
        let live = json!({"id":"1","login":"a","displayName":"A","profileImageURL":"u",
            "stream":{"title":"t","viewersCount":5,"game":{"displayName":"g"}}});
        let c = parse_user(&live).unwrap();
        assert!(c.live && c.viewers == 5 && c.category == "g");
        let off = json!({"id":"2","login":"b","displayName":"B","profileImageURL":"u","stream":null});
        assert!(!parse_user(&off).unwrap().live);
    }

    #[test]
    fn usher_url_encodes_the_token() {
        let u = usher_url("abc", "s1g", r#"{"a":"b c"}"#);
        assert!(u.contains("token=%7B%22a%22%3A%22b%20c%22%7D"));
        assert!(u.starts_with("https://usher.ttvnw.net/api/v2/channel/hls/abc.m3u8?"));
    }
}
