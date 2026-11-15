//! The app side of chat: the one connection, a buffer per channel, emote
//! sets (7TV, BTTV, FFZ) and badges, loaded on workers.

use crate::workers::Pool;
use gk_chat::{ChatBuffer, ChatClient, ChatEvent, ChatMessage, Credentials, EmoteLookup, EmoteRef, Span, tokenize};
use gk_twitch::{Client, Emote};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub const HISTORY: usize = 600;

pub type EmoteMap = HashMap<String, EmoteRef>;
pub type BadgeMap = HashMap<(String, String), String>;

pub struct ChannelChat {
    pub buffer: ChatBuffer,
    pub room_id: String,
    pub emotes: Arc<EmoteMap>,
    pub badges: Arc<BadgeMap>,
    pub emotes_requested: bool,
    pub joined: bool,
}

impl ChannelChat {
    fn new() -> Self {
        Self {
            buffer: ChatBuffer::new(HISTORY),
            room_id: String::new(),
            emotes: Arc::default(),
            badges: Arc::default(),
            emotes_requested: false,
            joined: false,
        }
    }
}

struct Lookup<'a> {
    channel: &'a EmoteMap,
    global: &'a EmoteMap,
}

impl EmoteLookup for Lookup<'_> {
    fn get(&self, name: &str) -> Option<EmoteRef> {
        self.channel.get(name).or_else(|| self.global.get(name)).cloned()
    }
}

enum Loaded {
    GlobalEmotes(EmoteMap),
    GlobalBadges(BadgeMap),
    Channel { channel: String, emotes: EmoteMap, badges: BadgeMap },
}

fn to_map(list: Vec<Emote>) -> EmoteMap {
    // Lists come in priority order; the first of a name wins.
    let mut m = EmoteMap::new();
    for e in list {
        m.entry(e.name).or_insert(EmoteRef { url: e.url, animated: e.animated, zero_width: e.zero_width });
    }
    m
}

pub struct ChatState {
    client: ChatClient,
    queue: Arc<Mutex<Vec<ChatEvent>>>,
    loaded: Arc<Mutex<Vec<Loaded>>>,
    pub channels: HashMap<String, ChannelChat>,
    global_emotes: Arc<EmoteMap>,
    global_badges: Arc<BadgeMap>,
    pool: Pool,
    api: Arc<Mutex<Client>>,
    wake: Arc<dyn Fn() + Send + Sync>,
    pub login: Option<String>,
    pub connected: bool,
    pub last_error: Option<String>,
    /// Bumped when every chat layout must be redone (new emote sets).
    pub generation: u64,
    /// Sample conversations of demo channels (`--demo`), by channel.
    demo_feeds: HashMap<String, Arc<AtomicBool>>,
}

/// A gentle sample conversation for a demo channel: made-up names, plain
/// text, no emote or badge art from anyone else.
fn demo_chat(
    channel: String,
    queue: Arc<Mutex<Vec<ChatEvent>>>,
    wake: Arc<dyn Fn() + Send + Sync>,
    alive: Arc<AtomicBool>,
) {
    const NAMES: [(&str, [u8; 3]); 10] = [
        ("sunny_otter", [251, 191, 36]),
        ("pixelpilot", [34, 211, 238]),
        ("mossbyte", [52, 211, 153]),
        ("velvetfox", [244, 114, 182]),
        ("quietkettle", [167, 139, 250]),
        ("nightowl_42", [96, 165, 250]),
        ("tinyrobot", [248, 113, 113]),
        ("lumen", [163, 230, 53]),
        ("paperplane", [251, 146, 60]),
        ("cloudberry", [196, 181, 253]),
    ];
    const LINES: [&str; 18] = [
        "this latency is unreal, I see it before my friend on the phone",
        "four streams and my fans are not even spinning",
        "the fractal one is hypnotic",
        "can we get the life one bigger? it looks like a city at night",
        "how is the chat this smooth with so much going on",
        "just merged three chats into one, love it",
        "anyone else watching on two monitors with the stats open?",
        "gg",
        "that colour shift on the aurora stream",
        "first time here, what is this?",
        "multiview is the only way now",
        "the spotlight layout is perfect for tournaments",
        "zero dropped frames, the stats say so",
        "hello from the couch",
        "tiles snap so nicely when you drag them",
        "is that a test pattern? nostalgic",
        "@pixelpilot same here",
        "brb getting snacks",
    ];
    let _ = std::thread::Builder::new().name(format!("gk-demo-chat-{channel}")).spawn(move || {
        let mut n: u64 = channel.bytes().map(u64::from).sum();
        while alive.load(Ordering::Relaxed) {
            n = n.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let (login, color) = NAMES[(n >> 33) as usize % NAMES.len()];
            let text = LINES[(n >> 17) as usize % LINES.len()];
            let msg = ChatMessage {
                id: format!("demo-{n}"),
                channel: channel.clone(),
                login: login.into(),
                display_name: login.into(),
                color: Some(color),
                text: text.into(),
                ..Default::default()
            };
            if let Ok(mut q) = queue.lock() {
                let was_empty = q.is_empty();
                q.push(ChatEvent::Message(msg));
                if was_empty {
                    wake();
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(500 + (n >> 50) % 1400));
        }
    });
}

impl ChatState {
    pub fn new(
        credentials: Option<Credentials>,
        pool: Pool,
        api: Arc<Mutex<Client>>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let queue: Arc<Mutex<Vec<ChatEvent>>> = Arc::default();
        let client = {
            let (queue, wake) = (queue.clone(), wake.clone());
            ChatClient::start(
                credentials.clone(),
                Box::new(move |ev| {
                    if let Ok(mut q) = queue.lock() {
                        let was_empty = q.is_empty();
                        q.push(ev);
                        // Edge-triggered: one wake per batch, however busy.
                        if was_empty {
                            wake();
                        }
                    }
                }),
            )
        };
        let loaded: Arc<Mutex<Vec<Loaded>>> = Arc::default();
        {
            let (loaded, api, wake) = (loaded.clone(), api.clone(), wake.clone());
            pool.spawn(move || {
                let client = api.lock().map(|c| c.clone()).ok();
                let Some(client) = client else { return };
                if let Ok(list) = gk_twitch::emotes::global(client.http())
                    && let Ok(mut l) = loaded.lock()
                {
                    l.push(Loaded::GlobalEmotes(to_map(list)));
                }
                if let Ok(b) = client.badges(None) {
                    let map: BadgeMap = b.into_iter().map(|b| ((b.set, b.version), b.url)).collect();
                    if let Ok(mut l) = loaded.lock() {
                        l.push(Loaded::GlobalBadges(map));
                    }
                }
                wake();
            });
        }
        Self {
            client,
            queue,
            loaded,
            channels: HashMap::new(),
            global_emotes: Arc::default(),
            global_badges: Arc::default(),
            pool,
            api,
            wake,
            login: credentials.map(|c| c.login),
            connected: false,
            last_error: None,
            generation: 0,
            demo_feeds: HashMap::new(),
        }
    }

    pub fn set_credentials(&mut self, c: Option<Credentials>) {
        self.login = c.as_ref().map(|c| c.login.clone());
        self.client.set_credentials(c);
    }

    pub fn global_badges(&self) -> &BadgeMap {
        &self.global_badges
    }

    /// Joins the channels shown in chat panes, leaves the others.
    pub fn sync_channels(&mut self, wanted: &HashSet<String>) {
        for c in wanted {
            let ch = self.channels.entry(c.clone()).or_insert_with(ChannelChat::new);
            if !ch.joined {
                ch.joined = true;
                if crate::stream::is_demo(c) {
                    let alive = Arc::new(AtomicBool::new(true));
                    self.demo_feeds.insert(c.clone(), alive.clone());
                    demo_chat(c.clone(), self.queue.clone(), self.wake.clone(), alive);
                } else {
                    self.client.join(c);
                }
            }
        }
        let gone: Vec<String> = self.channels.keys().filter(|c| !wanted.contains(*c)).cloned().collect();
        for c in gone {
            if let Some(alive) = self.demo_feeds.remove(&c) {
                alive.store(false, Ordering::Relaxed);
            } else {
                self.client.part(&c);
            }
            self.channels.remove(&c);
        }
    }

    pub fn send(&self, channel: &str, text: &str) {
        self.client.send(channel, text);
    }

    fn request_channel(&mut self, channel: &str) {
        let Some(ch) = self.channels.get_mut(channel) else { return };
        if ch.emotes_requested || ch.room_id.is_empty() {
            return;
        }
        ch.emotes_requested = true;
        let (room, channel, loaded, api, wake) =
            (ch.room_id.clone(), channel.to_owned(), self.loaded.clone(), self.api.clone(), self.wake.clone());
        self.pool.spawn(move || {
            let Some(client) = api.lock().map(|c| c.clone()).ok() else { return };
            let emotes = gk_twitch::emotes::channel(client.http(), &room).map(to_map).unwrap_or_default();
            let badges = client
                .badges(Some(&room))
                .map(|b| b.into_iter().map(|b| ((b.set, b.version), b.url)).collect())
                .unwrap_or_default();
            if let Ok(mut l) = loaded.lock() {
                l.push(Loaded::Channel { channel, emotes, badges });
            }
            wake();
        });
    }

    fn spans(&self, channel: &str, msg: &ChatMessage) -> Vec<Span> {
        let empty = EmoteMap::new();
        let ch = self.channels.get(channel).map(|c| &*c.emotes).unwrap_or(&empty);
        tokenize(msg, &Lookup { channel: ch, global: &self.global_emotes }, self.login.as_deref().unwrap_or_default())
    }

    fn retokenize(&mut self, channel: Option<&str>) {
        let names: Vec<String> = match channel {
            Some(c) => vec![c.to_owned()],
            None => self.channels.keys().cloned().collect(),
        };
        for name in names {
            let Some(mut ch) = self.channels.remove(&name) else { continue };
            let global = self.global_emotes.clone();
            let me = self.login.clone().unwrap_or_default();
            let emotes = ch.emotes.clone();
            ch.buffer.retokenize(|m| tokenize(m, &Lookup { channel: &emotes, global: &global }, &me));
            self.generation += 1;
            self.channels.insert(name, ch);
        }
    }

    /// Takes everything that arrived; true if anything visible changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        let loaded = std::mem::take(&mut *self.loaded.lock().unwrap_or_else(|p| p.into_inner()));
        for l in loaded {
            changed = true;
            match l {
                Loaded::GlobalEmotes(m) => {
                    self.global_emotes = Arc::new(m);
                    self.retokenize(None);
                }
                Loaded::GlobalBadges(m) => self.global_badges = Arc::new(m),
                Loaded::Channel { channel, emotes, badges } => {
                    if let Some(ch) = self.channels.get_mut(&channel) {
                        ch.emotes = Arc::new(emotes);
                        ch.badges = Arc::new(badges);
                    }
                    self.retokenize(Some(&channel));
                }
            }
        }
        let events = std::mem::take(&mut *self.queue.lock().unwrap_or_else(|p| p.into_inner()));
        for ev in events {
            changed = true;
            match ev {
                ChatEvent::Connected { .. } => {
                    self.connected = true;
                    self.last_error = None;
                }
                ChatEvent::Disconnected { reason } => {
                    self.connected = false;
                    self.last_error = Some(reason);
                    for ch in self.channels.values_mut() {
                        ch.joined = true; // the client rejoins by itself
                    }
                }
                ChatEvent::Joined { .. } => {}
                ChatEvent::RoomState { channel, room_id } => {
                    if let Some(ch) = self.channels.get_mut(&channel) {
                        ch.room_id = room_id;
                    }
                    self.request_channel(&channel);
                }
                ChatEvent::Message(m) => {
                    let spans = self.spans(&m.channel.clone(), &m);
                    if let Some(ch) = self.channels.get_mut(&m.channel) {
                        ch.buffer.push(m, spans, None);
                    }
                }
                ChatEvent::UserNotice { message, system_text } => {
                    let spans = self.spans(&message.channel.clone(), &message);
                    if let Some(ch) = self.channels.get_mut(&message.channel) {
                        ch.buffer.push(message, spans, Some(system_text));
                    }
                }
                ChatEvent::ClearChat { channel, user } => {
                    if let Some(ch) = self.channels.get_mut(&channel) {
                        ch.buffer.clear_chat(user.as_deref());
                    }
                }
                ChatEvent::ClearMsg { channel, id } => {
                    if let Some(ch) = self.channels.get_mut(&channel) {
                        ch.buffer.clear_msg(&id);
                    }
                }
                ChatEvent::Notice { channel, text } => {
                    let targets: Vec<String> =
                        if channel.is_empty() { self.channels.keys().cloned().collect() } else { vec![channel] };
                    for c in targets {
                        if let Some(ch) = self.channels.get_mut(&c) {
                            let msg = ChatMessage { channel: c.clone(), ..Default::default() };
                            ch.buffer.push(msg, Vec::new(), Some(text.clone()));
                        }
                    }
                }
            }
        }
        changed
    }
}
