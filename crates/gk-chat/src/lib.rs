//! Twitch chat for Glotzkiste (docs/DESIGN.md §2 gk-chat): one background
//! connection, the message model, an emote tokenizer and bounded buffers.
//! Nothing here touches the UI or blocks a caller.

pub mod client;
pub mod irc;
pub mod model;
pub mod tokenize;

pub use client::{ChatClient, Credentials};
pub use model::{ChatEvent, ChatMessage};
pub use tokenize::{EmoteLookup, EmoteRef, Span, tokenize};

use std::collections::VecDeque;

/// A message ready to draw.
#[derive(Debug, Clone)]
pub struct ChatLine {
    pub msg: ChatMessage,
    pub spans: Vec<Span>,
    /// Twitch's system text for subs, raids and the like.
    pub system: Option<String>,
    /// Removed by a moderator (shown struck out, not dropped).
    pub deleted: bool,
    pub mention: bool,
    /// Increases with every line; widgets key on it.
    pub seq: u64,
}

/// A channel's recent messages, capped (AGENTS.md performance rule 6).
#[derive(Debug, Default)]
pub struct ChatBuffer {
    lines: VecDeque<ChatLine>,
    cap: usize,
    next_seq: u64,
    /// Bumped on every change, so a view knows when to lay out again.
    pub revision: u64,
}

impl ChatBuffer {
    pub fn new(cap: usize) -> Self {
        Self { lines: VecDeque::with_capacity(cap.min(4096)), cap: cap.max(1), next_seq: 0, revision: 0 }
    }

    pub fn push(&mut self, msg: ChatMessage, spans: Vec<Span>, system: Option<String>) {
        let mention = tokenize::mentions(&spans);
        self.lines.push_back(ChatLine { msg, spans, system, deleted: false, mention, seq: self.next_seq });
        self.next_seq += 1;
        while self.lines.len() > self.cap {
            self.lines.pop_front();
        }
        self.revision += 1;
    }

    /// A ban/timeout (`Some(user)`) or a full clear (`None`).
    pub fn clear_chat(&mut self, user: Option<&str>) {
        for l in self.lines.iter_mut() {
            if user.is_none_or(|u| l.msg.login.eq_ignore_ascii_case(u)) {
                l.deleted = true;
            }
        }
        self.revision += 1;
    }

    pub fn clear_msg(&mut self, id: &str) {
        if let Some(l) = self.lines.iter_mut().rev().find(|l| l.msg.id == id) {
            l.deleted = true;
            self.revision += 1;
        }
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn get(&self, i: usize) -> Option<&ChatLine> {
        self.lines.get(i)
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &ChatLine> + ExactSizeIterator {
        self.lines.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(login: &str, id: &str) -> ChatMessage {
        ChatMessage { login: login.into(), id: id.into(), text: "x".into(), ..Default::default() }
    }

    #[test]
    fn buffer_is_capped_and_moderated() {
        let mut b = ChatBuffer::new(3);
        for i in 0..5 {
            b.push(m(if i % 2 == 0 { "a" } else { "b" }, &i.to_string()), vec![], None);
        }
        assert_eq!(b.len(), 3);
        assert_eq!(b.get(0).unwrap().msg.id, "2");
        b.clear_chat(Some("A"));
        assert!(b.get(0).unwrap().deleted && !b.get(1).unwrap().deleted && b.get(2).unwrap().deleted);
        b.clear_msg("3");
        assert!(b.get(1).unwrap().deleted);
    }
}
