//! Splits a message into text, emotes, mentions and links.

use crate::model::ChatMessage;

/// A third-party emote known by name.
#[derive(Debug, Clone, PartialEq)]
pub struct EmoteRef {
    pub url: String,
    pub animated: bool,
    pub zero_width: bool,
}

/// Looks up third-party emotes by exact name (channel sets before global).
pub trait EmoteLookup {
    fn get(&self, name: &str) -> Option<EmoteRef>;
}

impl EmoteLookup for () {
    fn get(&self, _: &str) -> Option<EmoteRef> {
        None
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Span {
    Text(String),
    Emote { name: String, url: String, animated: bool, zero_width: bool },
    Mention { name: String, me: bool },
    Link(String),
}

/// The CDN URL of a native Twitch emote (2x, dark, animated when it is).
pub fn twitch_emote_url(id: &str) -> String {
    format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/default/dark/2.0")
}

fn is_link(w: &str) -> bool {
    let w = w.trim_end_matches(['.', ',', ')', '!', '?']);
    if w.starts_with("http://") || w.starts_with("https://") {
        return w.len() > 10;
    }
    // bare domains like "example.com/x": a dot, a letters-only TLD of 2-10
    // chars, no spaces.
    let host = w.split('/').next().unwrap_or_default();
    match host.rsplit_once('.') {
        Some((name, tld)) => {
            !name.is_empty()
                && (2..=10).contains(&tld.len())
                && tld.chars().all(|c| c.is_ascii_alphabetic())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        }
        None => false,
    }
}

fn push_text(out: &mut Vec<Span>, s: &str) {
    if s.is_empty() {
        return;
    }
    if let Some(Span::Text(t)) = out.last_mut() {
        t.push_str(s);
    } else {
        out.push(Span::Text(s.to_owned()));
    }
}

fn words(out: &mut Vec<Span>, text: &str, lookup: &dyn EmoteLookup, me: &str) {
    let mut start = 0;
    let bytes = text.as_bytes();
    while start < text.len() {
        // Whitespace run.
        let ws_end = text[start..].find(|c: char| !c.is_whitespace()).map_or(text.len(), |i| start + i);
        push_text(out, &text[start..ws_end]);
        if ws_end >= text.len() {
            break;
        }
        let end = text[ws_end..].find(char::is_whitespace).map_or(text.len(), |i| ws_end + i);
        let w = &text[ws_end..end];
        if let Some(e) = lookup.get(w) {
            out.push(Span::Emote { name: w.to_owned(), url: e.url, animated: e.animated, zero_width: e.zero_width });
        } else if w.len() > 1 && bytes[ws_end] == b'@' {
            let name = w[1..].trim_end_matches([',', ':', '.', '!', '?']);
            out.push(Span::Mention { name: name.to_owned(), me: !me.is_empty() && name.eq_ignore_ascii_case(me) });
            push_text(out, &w[1 + name.len()..]);
        } else if is_link(w) {
            out.push(Span::Link(w.to_owned()));
        } else {
            push_text(out, w);
        }
        start = end;
    }
}

/// Tokenizes `msg`; `me` is the viewer's login (for highlighted mentions).
pub fn tokenize(msg: &ChatMessage, lookup: &dyn EmoteLookup, me: &str) -> Vec<Span> {
    let mut out = Vec::new();
    let mut pos = 0;
    for (id, r) in &msg.twitch_emotes {
        if r.start < pos
            || r.end > msg.text.len()
            || !msg.text.is_char_boundary(r.start)
            || !msg.text.is_char_boundary(r.end)
        {
            continue;
        }
        words(&mut out, &msg.text[pos..r.start], lookup, me);
        out.push(Span::Emote {
            name: msg.text[r.clone()].to_owned(),
            url: twitch_emote_url(id),
            animated: false,
            zero_width: false,
        });
        pos = r.end;
    }
    words(&mut out, &msg.text[pos..], lookup, me);
    out
}

/// True if the message mentions `me`.
pub fn mentions(spans: &[Span]) -> bool {
    spans.iter().any(|s| matches!(s, Span::Mention { me: true, .. }))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct L;
    impl EmoteLookup for L {
        fn get(&self, name: &str) -> Option<EmoteRef> {
            match name {
                "catJAM" => Some(EmoteRef { url: "u/cat".into(), animated: true, zero_width: false }),
                "RainTime" => Some(EmoteRef { url: "u/rain".into(), animated: true, zero_width: true }),
                _ => None,
            }
        }
    }

    fn msg(text: &str, emotes: Vec<(String, std::ops::Range<usize>)>) -> ChatMessage {
        ChatMessage { text: text.into(), twitch_emotes: emotes, ..Default::default() }
    }

    #[test]
    fn mixes_native_and_third_party_emotes() {
        let m = msg("Kappa hi catJAM RainTime @Me, see example.com", vec![("25".into(), 0..5)]);
        let s = tokenize(&m, &L, "me");
        assert!(matches!(&s[0], Span::Emote { name, url, .. } if name == "Kappa" && url.contains("/25/")));
        assert_eq!(s[1], Span::Text(" hi ".into()));
        assert!(matches!(&s[2], Span::Emote { name, .. } if name == "catJAM"));
        assert!(matches!(&s[4], Span::Emote { zero_width: true, .. }));
        assert!(matches!(&s[6], Span::Mention { name, me: true } if name == "Me"));
        assert_eq!(s[7], Span::Text(", see ".into()));
        assert_eq!(s[8], Span::Link("example.com".into()));
        assert!(mentions(&s));
    }

    #[test]
    fn plain_text_stays_one_span() {
        let s = tokenize(&msg("just some words. ok", vec![]), &(), "");
        assert_eq!(s, vec![Span::Text("just some words. ok".into())]);
    }

    #[test]
    fn bad_ranges_do_not_panic() {
        let s = tokenize(&msg("ä", vec![("1".into(), 1..2)]), &(), "");
        assert_eq!(s, vec![Span::Text("ä".into())]);
    }
}
