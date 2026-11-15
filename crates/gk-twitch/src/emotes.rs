//! Third-party emote sets: 7TV, BTTV and FFZ (FFZ through BTTV's cache).

use crate::http::Http;
use crate::{Error, Result};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Provider {
    SevenTv,
    Bttv,
    Ffz,
    Twitch,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Emote {
    pub name: String,
    pub provider: Provider,
    pub id: String,
    /// The 2x image (about 56 px tall), WebP where the provider has it.
    pub url: String,
    pub animated: bool,
    /// Drawn on top of the previous emote instead of after it.
    pub zero_width: bool,
    /// 1x size in pixels when the provider says it (0 = unknown).
    pub width: u32,
    pub height: u32,
}

/// BTTV's global overlay emotes (its API has no zero-width flag).
const BTTV_ZERO_WIDTH: &[&str] = &["SoSnowy", "IceCold", "SantaHat", "TopHat", "ReinDeer", "CandyCane", "cvMask", "cvHazmat"];
/// 7TV emote flag: zero-width.
const SEVENTV_ZERO_WIDTH: u64 = 1 << 8;

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

fn seventv_emotes(set: &Value) -> Vec<Emote> {
    set["emotes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let data = &e["data"];
            let host = &data["host"];
            let base = host["url"].as_str()?;
            let files = host["files"].as_array()?;
            let file = files
                .iter()
                .find(|f| f["name"] == "2x.webp")
                .or_else(|| files.iter().find(|f| f["name"].as_str().is_some_and(|n| n.starts_with("2x"))))
                .or_else(|| files.first())?;
            let one = files.iter().find(|f| f["name"] == "1x.webp");
            let flags = e["flags"].as_u64().unwrap_or(0) | data["flags"].as_u64().unwrap_or(0);
            Some(Emote {
                name: s(&e["name"]),
                provider: Provider::SevenTv,
                id: s(&e["id"]),
                url: format!("https:{base}/{}", file["name"].as_str()?),
                animated: data["animated"].as_bool().unwrap_or(false),
                zero_width: flags & SEVENTV_ZERO_WIDTH != 0,
                width: one.and_then(|f| f["width"].as_u64()).unwrap_or(0) as u32,
                height: one.and_then(|f| f["height"].as_u64()).unwrap_or(0) as u32,
            })
        })
        .filter(|e| !e.name.is_empty())
        .collect()
}

fn bttv_emotes(list: &Value) -> Vec<Emote> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let id = e["id"].as_str()?;
            let name = s(&e["code"]);
            Some(Emote {
                zero_width: BTTV_ZERO_WIDTH.contains(&name.as_str()),
                name,
                provider: Provider::Bttv,
                id: id.to_owned(),
                url: format!("https://cdn.betterttv.net/emote/{id}/2x.webp"),
                animated: e["animated"].as_bool().unwrap_or(false),
                width: 0,
                height: 0,
            })
        })
        .collect()
}

fn ffz_emotes(list: &Value) -> Vec<Emote> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let images = &e["images"];
            let url = images["2x"].as_str().or_else(|| images["1x"].as_str())?;
            Some(Emote {
                name: s(&e["code"]),
                provider: Provider::Ffz,
                id: e["id"].as_u64().map(|i| i.to_string()).unwrap_or_else(|| s(&e["id"])),
                url: url.to_owned(),
                animated: e["animated"].as_bool().unwrap_or(false),
                zero_width: false,
                width: 0,
                height: 0,
            })
        })
        .collect()
}

fn ok_or_empty(r: Result<Value>) -> Result<Value> {
    // A channel without an account at a provider answers 404: no emotes.
    match r {
        Err(Error::Status(404)) => Ok(Value::Null),
        other => other,
    }
}

/// Global sets, in priority order 7TV, BTTV, FFZ. A failing provider is
/// skipped (its error is returned only if all fail).
pub fn global(http: &Http) -> Result<Vec<Emote>> {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    match http.get_json("https://7tv.io/v3/emote-sets/global", &[]) {
        Ok(v) => out.extend(seventv_emotes(&v)),
        Err(e) => errors.push(e),
    }
    match http.get_json("https://api.betterttv.net/3/cached/emotes/global", &[]) {
        Ok(v) => out.extend(bttv_emotes(&v)),
        Err(e) => errors.push(e),
    }
    match http.get_json("https://api.betterttv.net/3/cached/frankerfacez/emotes/global", &[]) {
        Ok(v) => out.extend(ffz_emotes(&v)),
        Err(e) => errors.push(e),
    }
    if out.is_empty() && !errors.is_empty() { Err(errors.remove(0)) } else { Ok(out) }
}

/// A channel's sets, in priority order 7TV, BTTV, FFZ.
pub fn channel(http: &Http, channel_id: &str) -> Result<Vec<Emote>> {
    if channel_id.is_empty() || !channel_id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::NotFound);
    }
    let mut out = Vec::new();
    let mut errors = Vec::new();
    match ok_or_empty(http.get_json(&format!("https://7tv.io/v3/users/twitch/{channel_id}"), &[])) {
        Ok(v) => out.extend(seventv_emotes(&v["emote_set"])),
        Err(e) => errors.push(e),
    }
    match ok_or_empty(http.get_json(&format!("https://api.betterttv.net/3/cached/users/twitch/{channel_id}"), &[])) {
        Ok(v) => {
            out.extend(bttv_emotes(&v["channelEmotes"]));
            out.extend(bttv_emotes(&v["sharedEmotes"]));
        }
        Err(e) => errors.push(e),
    }
    match ok_or_empty(
        http.get_json(&format!("https://api.betterttv.net/3/cached/frankerfacez/users/twitch/{channel_id}"), &[]),
    ) {
        Ok(v) => out.extend(ffz_emotes(&v)),
        Err(e) => errors.push(e),
    }
    if out.is_empty() && errors.len() == 3 { Err(errors.remove(0)) } else { Ok(out) }
}

/// The CDN URL of a native Twitch emote (2x, dark theme, animated if it is).
pub fn twitch_emote_url(id: &str) -> String {
    format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/default/dark/2.0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn seventv_set_with_zero_width() {
        let set = json!({"emotes":[
            {"id":"a","name":"RainTime","flags":1,"data":{"flags":256,"animated":true,"host":{"url":"//cdn.7tv.app/emote/a",
              "files":[{"name":"1x.webp","width":32,"height":32},{"name":"2x.webp","width":64,"height":64}]}}},
            {"id":"b","name":"Plain","flags":0,"data":{"flags":0,"animated":false,"host":{"url":"//cdn.7tv.app/emote/b",
              "files":[{"name":"1x.webp","width":28,"height":28},{"name":"2x.avif"},{"name":"2x.webp"}]}}},
            {"id":"c","name":"Broken","data":{}}
        ]});
        let e = seventv_emotes(&set);
        assert_eq!(e.len(), 2);
        assert!(e[0].zero_width && e[0].animated);
        assert_eq!(e[0].url, "https://cdn.7tv.app/emote/a/2x.webp");
        assert_eq!((e[1].width, e[1].height), (28, 28));
        assert_eq!(e[1].url, "https://cdn.7tv.app/emote/b/2x.webp");
        assert!(!e[1].zero_width);
    }

    #[test]
    fn bttv_and_ffz() {
        let b = bttv_emotes(&json!([{"id":"x1","code":"IceCold","animated":false},{"id":"x2","code":"catJAM","animated":true}]));
        assert!(b[0].zero_width && !b[1].zero_width && b[1].animated);
        assert_eq!(b[1].url, "https://cdn.betterttv.net/emote/x2/2x.webp");
        let f = ffz_emotes(&json!([{"id":7,"code":"LULW","images":{"1x":"https://f/1","2x":"https://f/2","4x":null}}]));
        assert_eq!((f[0].id.as_str(), f[0].url.as_str()), ("7", "https://f/2"));
    }
}
