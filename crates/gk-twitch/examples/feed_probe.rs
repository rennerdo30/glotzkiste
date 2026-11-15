//! Resolves a live channel anonymously and runs the low-latency feeder for a
//! few seconds: `cargo run -p gk-twitch --example feed_probe [login] [secs]`.
//! Prints only counts and timings (never tokens or URLs).
use gk_twitch::{Client, Http, live::LiveFeed};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    let http = Http::new();
    let client = Client::new(http.clone(), None);
    let login = std::env::args().nth(1).filter(|s| !s.is_empty()).unwrap_or_else(|| {
        let v = http
            .post_json("https://gql.twitch.tv/gql", &[("Client-ID", gk_twitch::api::CLIENT_ID)],
                &serde_json::json!({"query": "query { streams(first: 1) { edges { node { broadcaster { login } } } } }"}), &[])
            .expect("top streams");
        v["data"]["streams"]["edges"][0]["node"]["broadcaster"]["login"].as_str().unwrap_or_default().to_owned()
    });
    let secs: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let t0 = Instant::now();
    let pb = match client.playback(&login) { Ok(p) => p, Err(e) => { println!("playback error: {e}"); return; } };
    println!("{login}: {} variants in {:?}, turbo={} ads_expected={}", pb.variants.len(), t0.elapsed(), pb.turbo, pb.ads_expected);
    for v in &pb.variants { println!("  {:10} {:4}x{:<4} {:>4.0}fps {:>8} bps source={}", v.name, v.width, v.height, v.fps, v.bandwidth, v.source); }
    let idx = pb.variants.iter().position(|v| v.height == 720).unwrap_or(0);
    let t1 = Instant::now();
    let feed = LiveFeed::start(http, pb.variants, idx, Arc::new(|| {}));
    let r = feed.reader();
    let mut buf = vec![0u8; 64 * 1024];
    let (mut total, mut first) = (0usize, None);
    let mut sync_ok = true;
    let mut switched = false;
    while t1.elapsed() < Duration::from_secs(secs) {
        match r.read(&mut buf) {
            Ok(0) => { println!("stream ended"); break; }
            Ok(n) => {
                if let Some(e) = feed.status().last_error { eprintln!("  err at {:?}: {e}", t1.elapsed()); }
                if first.is_none() { first = Some(t1.elapsed()); sync_ok = buf[0] == 0x47; println!("first bytes: {:02x?} {:?}", &buf[..8.min(n)], String::from_utf8_lossy(&buf[4..8.min(n)])); }
                total += n;
            }
            Err(e) => { println!("read error {e}"); break; }
        }
        if !switched && t1.elapsed() > Duration::from_secs(secs / 2) { feed.set_variant(feed.variants().len().saturating_sub(2)); switched = true; println!("switched quality at {:?}", t1.elapsed()); }
    }
    let st = feed.status();
    println!("first byte after {:?} (TS sync byte ok: {sync_ok}); {} KiB in {:?}", first.unwrap_or_default(), total / 1024, t1.elapsed());
    println!("status: variant={} low_latency={} in_ad={} done={} skipped={} err={:?}", st.variant, st.low_latency, st.in_ad, st.segments_done, st.segments_skipped, st.last_error);
}
