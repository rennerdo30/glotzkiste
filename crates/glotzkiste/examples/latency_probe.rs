//! Headless low-latency check: mpv (vo=null, ao=null) reads the live feeder
//! through the stream callback API; prints buffer and speed once a second.
//! `cargo run -p glotzkiste --example latency_probe [login] [secs]`
use gk_mpv::{Format, Libmpv, Mpv, StreamReader, Value};
use gk_twitch::{Client, Http, live::{FeedReader, LiveFeed}};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct R(FeedReader);
impl StreamReader for R {
    fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> { self.0.read(buf) }
    fn cancel(&self) { self.0.cancel() }
}

fn main() {
    let path = std::env::var("GK_LIBMPV").expect("GK_LIBMPV");
    let lib = Arc::new(Libmpv::load_from(&[path.into()]).expect("libmpv"));
    let http = Http::new();
    let client = Client::new(http.clone(), None);
    let login = std::env::args().nth(1).filter(|s| !s.is_empty()).unwrap_or_else(|| {
        let v = http.post_json("https://gql.twitch.tv/gql", &[("Client-ID", gk_twitch::api::CLIENT_ID)],
            &serde_json::json!({"query": "query { streams(first: 5) { edges { node { broadcaster { login } } } } }"}), &[]).expect("top");
        v["data"]["streams"]["edges"][std::env::var("GK_PICK").ok().and_then(|s| s.parse::<usize>().ok()).unwrap_or(0)]["node"]["broadcaster"]["login"].as_str().unwrap_or_default().to_owned()
    });
    let secs: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(45);
    let pb = client.playback(&login).expect("playback");
    let idx = pb.variants.iter().position(|v| v.height == 720).unwrap_or(0);
    let feed = Arc::new(LiveFeed::start(http, pb.variants, idx, Arc::new(|| {})));

    let mpv = Mpv::create(lib).expect("create");
    for (k, v) in [("vo", "null"), ("ao", "null"), ("profile", "low-latency"), ("demuxer-lavf-format", "mpegts"),
                   ("cache", "yes"), ("demuxer-max-bytes", "32MiB"), ("audio-buffer", "0.1"), ("terminal", "no")] {
        if let Err(e) = mpv.set_option(k, v) { println!("option {k}: {e}"); }
    }
    mpv.initialize().expect("init");
    let f2 = feed.clone();
    mpv.add_stream_protocol("gkfeed", Box::new(move |_uri| Some(Arc::new(R(f2.reader())) as Arc<dyn StreamReader>))).expect("proto");
    mpv.observe(1, "demuxer-cache-duration", Format::Double).ok();
    mpv.observe(2, "frame-drop-count", Format::Int64).ok();
    mpv.observe(3, "video-params/h", Format::Int64).ok();
    mpv.command(&["loadfile", "gkfeed://probe"]).expect("load");
    let t0 = Instant::now();
    let (mut cache, mut drops, mut h, mut speed, mut first_frame) = (0.0f64, 0i64, 0i64, 1.0f64, None);
    let mut last_print = Instant::now();
    let target = 0.8;
    while t0.elapsed() < Duration::from_secs(secs) {
        while let Some(ev) = mpv.wait_event(0.05) {
            match ev {
                gk_mpv::Event::PropertyChange { id: 1, value: Value::Double(d), .. } => cache = d,
                gk_mpv::Event::PropertyChange { id: 2, value: Value::Int64(d), .. } => drops = d,
                gk_mpv::Event::PropertyChange { id: 3, value: Value::Int64(d), .. } => { h = d; if first_frame.is_none() { first_frame = Some(t0.elapsed()); } }
                gk_mpv::Event::EndFile { reason, error } => { println!("end {reason:?} {error:?}"); return; }
                _ => {}
            }
        }
        let total = cache + feed.buffered_secs();
        let want = if total > target + 1.5 { 1.25 } else if total > target + 0.4 { 1.08 } else if total < target - 0.4 { 0.97 } else { 1.0 };
        if (want - speed) .abs() > 1e-3 { speed = want; mpv.command(&["set", "speed", &format!("{speed}")]).ok(); }
        if last_print.elapsed() >= Duration::from_secs(2) {
            last_print = Instant::now();
            let st = feed.status();
            println!("t={:>5.1}s h={h} demux={cache:.2}s pipe={:.2}s speed={speed} drops={drops} ll={} ad={} seg={}/{}", t0.elapsed().as_secs_f64(), feed.buffered_secs(), st.low_latency, st.in_ad, st.segments_done, st.segments_skipped);
        }
    }
    println!("first video params after {:?}", first_frame);
}
