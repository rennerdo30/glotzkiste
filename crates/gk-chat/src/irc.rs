//! IRCv3 line parsing with Twitch tags.

/// One parsed IRC line.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Line {
    pub tags: Vec<(String, String)>,
    /// The nick part of the prefix (`login` in `login!login@...`).
    pub nick: String,
    pub command: String,
    pub params: Vec<String>,
}

impl Line {
    pub fn tag(&self, key: &str) -> Option<&str> {
        self.tags.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty())
    }
    /// The channel (first parameter without `#`).
    pub fn channel(&self) -> &str {
        self.params.first().map(|p| p.trim_start_matches('#')).unwrap_or_default()
    }
    /// The trailing text (last parameter).
    pub fn text(&self) -> &str {
        if self.params.len() >= 2 { self.params.last().map(String::as_str).unwrap_or_default() } else { "" }
    }
}

fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut it = v.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some(':') => out.push(';'),
                Some('s') => out.push(' '),
                Some('\\') => out.push('\\'),
                Some('r') => out.push('\r'),
                Some('n') => out.push('\n'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parses one line (without CR LF). Malformed input gives `None`.
pub fn parse(line: &str) -> Option<Line> {
    let mut rest = line.trim_end_matches(['\r', '\n']);
    let mut out = Line::default();
    if let Some(r) = rest.strip_prefix('@') {
        let (tags, r) = r.split_once(' ')?;
        for t in tags.split(';') {
            let (k, v) = t.split_once('=').unwrap_or((t, ""));
            out.tags.push((k.to_owned(), unescape(v)));
        }
        rest = r.trim_start();
    }
    if let Some(r) = rest.strip_prefix(':') {
        let (prefix, r) = r.split_once(' ')?;
        out.nick = prefix.split('!').next().unwrap_or_default().to_owned();
        rest = r.trim_start();
    }
    let (command, mut r) = rest.split_once(' ').unwrap_or((rest, ""));
    if command.is_empty() {
        return None;
    }
    out.command = command.to_owned();
    loop {
        r = r.trim_start_matches(' ');
        if r.is_empty() {
            break;
        }
        if let Some(trailing) = r.strip_prefix(':') {
            out.params.push(trailing.to_owned());
            break;
        }
        let (p, next) = r.split_once(' ').unwrap_or((r, ""));
        out.params.push(p.to_owned());
        r = next;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privmsg_with_tags() {
        let l = parse("@badge-info=;badges=broadcaster/1;color=#FF0000;display-name=Some\\sOne;emotes=25:0-4;id=abc;room-id=7;tmi-sent-ts=1700000000000 :someone!someone@someone.tmi.twitch.tv PRIVMSG #chan :Kappa hello there\r\n").unwrap();
        assert_eq!(l.command, "PRIVMSG");
        assert_eq!(l.nick, "someone");
        assert_eq!(l.channel(), "chan");
        assert_eq!(l.text(), "Kappa hello there");
        assert_eq!(l.tag("display-name"), Some("Some One"));
        assert_eq!(l.tag("badge-info"), None);
        assert_eq!(l.tag("emotes"), Some("25:0-4"));
    }

    #[test]
    fn ping_and_numerics() {
        let l = parse("PING :tmi.twitch.tv").unwrap();
        assert_eq!((l.command.as_str(), l.params[0].as_str()), ("PING", "tmi.twitch.tv"));
        let l = parse(":tmi.twitch.tv 001 justinfan1 :Welcome, GLHF!").unwrap();
        assert_eq!(l.command, "001");
        let l = parse(":tmi.twitch.tv CLEARCHAT #chan").unwrap();
        assert_eq!((l.channel(), l.text()), ("chan", ""));
        assert!(parse("").is_none());
        assert!(parse("@novalue").is_none());
    }
}
