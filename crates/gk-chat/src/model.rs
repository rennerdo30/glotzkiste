//! Chat messages and events, built from IRC lines.

use crate::irc::Line;
use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChatMessage {
    pub id: String,
    pub channel: String,
    pub room_id: String,
    pub login: String,
    pub display_name: String,
    pub color: Option<[u8; 3]>,
    /// (set, version), e.g. ("subscriber", "12").
    pub badges: Vec<(String, String)>,
    pub text: String,
    /// Native emotes: (emote id, byte range in `text`).
    pub twitch_emotes: Vec<(String, Range<usize>)>,
    /// `/me` message.
    pub action: bool,
    pub highlighted: bool,
    pub first_message: bool,
    pub bits: u32,
    pub timestamp_ms: u64,
    /// (display name, text) of the message this one replies to.
    pub reply_to: Option<(String, String)>,
    /// Sent by this app (local echo).
    pub own: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChatEvent {
    Connected { login: String },
    Joined { channel: String },
    Message(ChatMessage),
    /// Subscriptions, raids and similar, with Twitch's own system text.
    UserNotice { message: ChatMessage, system_text: String },
    /// A ban or timeout (`user`), or a full clear (`None`).
    ClearChat { channel: String, user: Option<String> },
    ClearMsg { channel: String, id: String },
    RoomState { channel: String, room_id: String },
    Notice { channel: String, text: String },
    Disconnected { reason: String },
}

fn parse_color(c: &str) -> Option<[u8; 3]> {
    let h = c.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

/// `25:0-4,12-16/1902:6-10` with char (code point) positions, into byte
/// ranges of `text`. Out-of-range or overlapping entries are dropped.
pub fn parse_emotes(tag: &str, text: &str) -> Vec<(String, Range<usize>)> {
    // Char index -> byte offset table (one extra entry for the end).
    let offsets: Vec<usize> = text.char_indices().map(|(b, _)| b).chain(std::iter::once(text.len())).collect();
    let mut out = Vec::new();
    for group in tag.split('/') {
        let Some((id, ranges)) = group.split_once(':') else { continue };
        for r in ranges.split(',') {
            let Some((a, b)) = r.split_once('-') else { continue };
            let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) else { continue };
            if a > b || b + 1 >= offsets.len() {
                continue;
            }
            out.push((id.to_owned(), offsets[a]..offsets[b + 1]));
        }
    }
    out.sort_by_key(|(_, r)| r.start);
    let mut last_end = 0;
    out.retain(|(_, r)| {
        let ok = r.start >= last_end;
        if ok {
            last_end = r.end;
        }
        ok
    });
    out
}

pub(crate) fn message_from(line: &Line) -> ChatMessage {
    let mut text = line.text().to_owned();
    let mut action = false;
    if let Some(inner) = text.strip_prefix("\u{1}ACTION ") {
        text = inner.trim_end_matches('\u{1}').to_owned();
        action = true;
    }
    let badges = line
        .tag("badges")
        .map(|b| b.split(',').filter_map(|p| p.split_once('/')).map(|(s, v)| (s.to_owned(), v.to_owned())).collect())
        .unwrap_or_default();
    let login = if line.nick.is_empty() { line.tag("login").unwrap_or_default().to_owned() } else { line.nick.clone() };
    let twitch_emotes = line.tag("emotes").map(|e| parse_emotes(e, &text)).unwrap_or_default();
    ChatMessage {
        id: line.tag("id").unwrap_or_default().to_owned(),
        channel: line.channel().to_owned(),
        room_id: line.tag("room-id").unwrap_or_default().to_owned(),
        display_name: line.tag("display-name").map(str::to_owned).unwrap_or_else(|| login.clone()),
        login,
        color: line.tag("color").and_then(parse_color),
        badges,
        twitch_emotes,
        text,
        action,
        highlighted: line.tag("msg-id") == Some("highlighted-message"),
        first_message: line.tag("first-msg") == Some("1"),
        bits: line.tag("bits").and_then(|b| b.parse().ok()).unwrap_or(0),
        timestamp_ms: line.tag("tmi-sent-ts").and_then(|t| t.parse().ok()).unwrap_or(0),
        reply_to: match (line.tag("reply-parent-display-name"), line.tag("reply-parent-msg-body")) {
            (Some(n), Some(b)) => Some((n.to_owned(), b.to_owned())),
            _ => None,
        },
        own: false,
    }
}

/// Turns an IRC line into a chat event, if it is one we show.
pub fn event_from(line: &Line) -> Option<ChatEvent> {
    Some(match line.command.as_str() {
        "PRIVMSG" => ChatEvent::Message(message_from(line)),
        "USERNOTICE" => ChatEvent::UserNotice {
            message: message_from(line),
            system_text: line.tag("system-msg").unwrap_or_default().to_owned(),
        },
        "CLEARCHAT" => ChatEvent::ClearChat {
            channel: line.channel().to_owned(),
            user: Some(line.text().to_owned()).filter(|u| !u.is_empty()),
        },
        "CLEARMSG" => ChatEvent::ClearMsg { channel: line.channel().to_owned(), id: line.tag("target-msg-id")?.to_owned() },
        "ROOMSTATE" => ChatEvent::RoomState { channel: line.channel().to_owned(), room_id: line.tag("room-id")?.to_owned() },
        "NOTICE" => ChatEvent::Notice { channel: line.channel().to_owned(), text: line.text().to_owned() },
        "JOIN" => ChatEvent::Joined { channel: line.channel().to_owned() },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irc::parse;

    #[test]
    fn emote_positions_are_chars_not_bytes() {
        // "ä" is two bytes; Kappa sits at chars 2..=6.
        let text = "ä Kappa x";
        let e = parse_emotes("25:2-6", text);
        assert_eq!(&text[e[0].1.clone()], "Kappa");
        // Out of range and overlapping ranges are dropped.
        assert!(parse_emotes("1:5-99", text).is_empty());
        assert_eq!(parse_emotes("1:2-6/2:4-5", text).len(), 1);
    }

    #[test]
    fn action_reply_and_badges() {
        let l = parse("@badges=subscriber/12,premium/1;color=#1E90FF;display-name=Bob;emotes=;id=m1;reply-parent-display-name=Al;reply-parent-msg-body=hi\\sthere :bob!bob@bob.tmi.twitch.tv PRIVMSG #c :\u{1}ACTION waves\u{1}").unwrap();
        let ChatEvent::Message(m) = event_from(&l).unwrap() else { panic!() };
        assert!(m.action);
        assert_eq!(m.text, "waves");
        assert_eq!(m.color, Some([0x1e, 0x90, 0xff]));
        assert_eq!(m.badges, [("subscriber".to_owned(), "12".to_owned()), ("premium".to_owned(), "1".to_owned())]);
        assert_eq!(m.reply_to, Some(("Al".to_owned(), "hi there".to_owned())));
    }

    #[test]
    fn moderation_events() {
        let l = parse("@target-msg-id=xyz :tmi.twitch.tv CLEARMSG #c :bad words").unwrap();
        assert_eq!(event_from(&l), Some(ChatEvent::ClearMsg { channel: "c".into(), id: "xyz".into() }));
        let l = parse(":tmi.twitch.tv CLEARCHAT #c :troll").unwrap();
        assert_eq!(event_from(&l), Some(ChatEvent::ClearChat { channel: "c".into(), user: Some("troll".into()) }));
    }
}
