//! Joins a channel anonymously for a few seconds and counts what arrives.
//! `cargo run -p gk-chat --example chat_probe <channel> [secs]`
use gk_chat::{ChatClient, ChatEvent};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn main() {
    let ch = std::env::args().nth(1).expect("channel");
    let secs: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(8);
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let s2 = seen.clone();
    let t0 = Instant::now();
    let client = ChatClient::start(None, Box::new(move |ev| {
        let line = match &ev {
            ChatEvent::Connected { .. } => format!("{:?} connected", t0.elapsed()),
            ChatEvent::Joined { channel } => format!("{:?} joined {channel}", t0.elapsed()),
            ChatEvent::RoomState { room_id, .. } => format!("roomstate room_id={room_id}"),
            ChatEvent::Message(m) => format!("msg emotes={} badges={} len={}", m.twitch_emotes.len(), m.badges.len(), m.text.len()),
            other => format!("{other:?}").chars().take(80).collect(),
        };
        if let Ok(mut s) = s2.lock() { s.push(line); }
    }));
    client.join(&ch);
    std::thread::sleep(Duration::from_secs(secs));
    drop(client);
    let s = seen.lock().unwrap();
    let msgs = s.iter().filter(|l| l.starts_with("msg")).count();
    for l in s.iter().filter(|l| !l.starts_with("msg")).take(6) { println!("{l}"); }
    println!("{msgs} messages in {secs}s; sample: {:?}", s.iter().find(|l| l.starts_with("msg")));
}
