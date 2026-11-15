//! The stats window: a miniature of the current tab, laid out exactly like
//! the tiles, with every stream's numbers in its own cell, and a global row
//! (display, UI, render thread, process). Optionally the same table on the
//! tiles themselves. Sampled twice a second, only while shown.

use super::{BAD, GOOD, MUTED, PANEL, RAISED, TEXT, WARN};
use crate::stream::{State, Status, StreamStats};
use egui::{Color32, Rect, RichText, vec2};
use gk_render::{Stats, TileStats};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Instant;

/// Where a tile was drawn in this pass.
#[derive(Clone)]
pub enum TileKind {
    Stream { id: u64, channel: String },
    Chat { channel: String },
    Empty,
}

#[derive(Clone)]
pub struct TileRect {
    pub rect: Rect,
    pub kind: TileKind,
}

#[derive(Default, Clone)]
pub struct Rates {
    pub presents_s: f64,
    pub passes_s: f64,
    pub pass_ms: f64,
    pub render_busy_pct: f64,
    pub anim_s: f64,
    pub slow_waits: u64,
    pub cpu_pct: f64,
    pub ram_mb: f64,
    /// Frames drawn into each stream's tile per second.
    pub tile_fps: HashMap<u64, f64>,
    /// Repeat and redraw renders per second (no new picture).
    pub tile_extra: HashMap<u64, (f64, f64)>,
    pub tiles: HashMap<u64, TileStats>,
}

struct Snap {
    at: Instant,
    presents: u64,
    passes: u64,
    pass_us: u64,
    busy_us: u64,
    anim: u64,
    cpu_100ns: u64,
    tiles: HashMap<u64, u64>,
    extra: HashMap<u64, (u64, u64)>,
}

#[derive(Default)]
pub struct Sampler {
    last: Option<Snap>,
    pub rates: Rates,
}

/// Process CPU time (100 ns units) and working set (bytes).
fn process_usage() -> (u64, u64) {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
        let (mut c, mut e, mut k, mut u) =
            (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let mut mem =
            PROCESS_MEMORY_COUNTERS { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
        // SAFETY: out pointers are valid locals; the pseudo handle needs no close.
        unsafe {
            let p = GetCurrentProcess();
            let cpu = if GetProcessTimes(p, &mut c, &mut e, &mut k, &mut u).is_ok() {
                let t = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
                t(k) + t(u)
            } else {
                0
            };
            let ram = if GetProcessMemoryInfo(p, &mut mem, mem.cb).is_ok() { mem.WorkingSetSize as u64 } else { 0 };
            (cpu, ram)
        }
    }
    #[cfg(not(windows))]
    {
        (0, 0)
    }
}

impl Sampler {
    /// Updates the rates at most once a second.
    pub fn sample(&mut self, st: &Stats, passes: u64, pass_us: u64) {
        if self.last.as_ref().is_some_and(|l| l.at.elapsed().as_millis() < 900) {
            return;
        }
        let ld = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
        let tiles: HashMap<u64, TileStats> = st.tiles.lock().map(|t| t.clone()).unwrap_or_default();
        let (cpu, ram) = process_usage();
        let now = Snap {
            at: Instant::now(),
            presents: ld(&st.presents),
            passes,
            pass_us,
            busy_us: ld(&st.busy_us),
            anim: ld(&st.anim_redraws),
            cpu_100ns: cpu,
            tiles: tiles.iter().map(|(k, v)| (*k, v.frames)).collect(),
            extra: tiles.iter().map(|(k, v)| (*k, (v.repeats, v.redraws))).collect(),
        };
        if let Some(l) = &self.last {
            let dt = now.at.duration_since(l.at).as_secs_f64().max(0.001);
            let per = |a: u64, b: u64| a.saturating_sub(b) as f64 / dt;
            let passes_d = now.passes.saturating_sub(l.passes);
            let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
            self.rates = Rates {
                presents_s: per(now.presents, l.presents),
                passes_s: per(now.passes, l.passes),
                pass_ms: if passes_d > 0 {
                    now.pass_us.saturating_sub(l.pass_us) as f64 / 1000.0 / passes_d as f64
                } else {
                    0.0
                },
                render_busy_pct: now.busy_us.saturating_sub(l.busy_us) as f64 / 10_000.0 / dt,
                anim_s: per(now.anim, l.anim),
                slow_waits: ld(&st.slow_waits),
                // Share of one core, like Task Manager's per-process view x cores.
                cpu_pct: now.cpu_100ns.saturating_sub(l.cpu_100ns) as f64 / 100_000.0 / dt / cores,
                ram_mb: ram as f64 / (1024.0 * 1024.0),
                // Smoothed: one-second samples swing by a frame or two.
                tile_fps: now
                    .tiles
                    .iter()
                    .map(|(k, f)| {
                        let fresh = per(*f, l.tiles.get(k).copied().unwrap_or(*f));
                        let old = self.rates.tile_fps.get(k).copied().unwrap_or(fresh);
                        (*k, old * 0.5 + fresh * 0.5)
                    })
                    .collect(),
                tile_extra: now
                    .extra
                    .iter()
                    .map(|(k, (rp, rd))| {
                        let (lrp, lrd) = l.extra.get(k).copied().unwrap_or((*rp, *rd));
                        (*k, (per(*rp, lrp), per(*rd, lrd)))
                    })
                    .collect(),
                tiles,
            };
        }
        self.last = Some(now);
    }
}

fn row(ui: &mut egui::Ui, key: &str, value: RichText) {
    ui.label(RichText::new(key).color(MUTED).size(12.0));
    ui.label(value.size(12.0));
    ui.end_row();
}

fn plain(s: impl Into<String>) -> RichText {
    RichText::new(s.into()).color(TEXT)
}

fn codec_name(raw: &str) -> &str {
    raw.split([' ', '(']).next().unwrap_or(raw)
}

/// The table for one stream.
pub fn stream_table(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    status: &Status,
    s: &StreamStats,
    rates: &Rates,
    stream_id: u64,
) {
    egui::Grid::new(id_salt).num_columns(2).spacing(vec2(10.0, 2.0)).show(ui, |ui| {
        let state = match &status.state {
            _ if status.in_ad => RichText::new("ad break, skipping").color(WARN),
            State::Playing => RichText::new("playing").color(GOOD),
            State::Starting => RichText::new("connecting").color(WARN),
            State::Offline => RichText::new("offline").color(MUTED),
            State::Error(e) => RichText::new(e.clone()).color(BAD),
        };
        row(ui, "State", state);
        if s.sampled.is_none() {
            row(ui, "", RichText::new("measuring…").color(MUTED));
            return;
        }
        let container = match s.container.as_str() {
            "Fmp4" => "fMP4",
            "Ts" => "TS",
            other => other,
        };
        let res = if s.height > 0 { format!("{}×{}", s.width, s.height) } else { "—".into() };
        row(ui, "Stream", plain(format!("{} · {res} · {} · {container}", status.current, codec_name(&s.codec))));
        // Frames that reached the screen, against the stream's own rate.
        let tile = rates.tiles.get(&stream_id).copied().unwrap_or_default();
        let shown = rates.tile_fps.get(&stream_id).copied().unwrap_or(0.0);
        // While catching up (speed above 1) more frames per second are right.
        let expected = s.source_fps * s.speed.max(0.5);
        let fps_color = if s.source_fps <= 0.0 || !tile.visible {
            TEXT
        } else if shown >= expected * 0.9 && shown <= expected * 1.1 {
            GOOD
        } else {
            WARN
        };
        let decoder = if s.decoded_fps > 0.0 { format!(" · decoder {:.1}", s.decoded_fps) } else { String::new() };
        row(
            ui,
            "Frames",
            RichText::new(format!("{shown:.1} of {:.0} fps shown{decoder}", s.source_fps)).color(fps_color),
        );
        let (repeats, redraws) = rates.tile_extra.get(&stream_id).copied().unwrap_or((0.0, 0.0));
        if repeats + redraws >= 0.5 {
            row(
                ui,
                "Extra renders",
                RichText::new(format!("{repeats:.1} repeats/s · {redraws:.1} redraws/s")).color(MUTED),
            );
        }
        let viewport = if tile.visible {
            format!("{}×{} px", tile.width, tile.height)
        } else {
            "hidden (not decoding video)".into()
        };
        row(ui, "Viewport", plain(viewport));
        let drops = s.drops_vo + s.drops_decoder;
        let mut parts = vec![format!("output {}", s.drops_vo), format!("decoder {}", s.drops_decoder)];
        if s.delayed > 0 {
            parts.push(format!("late {}", s.delayed));
        }
        if s.mistimed > 0 {
            parts.push(format!("mistimed {}", s.mistimed));
        }
        row(
            ui,
            "Dropped",
            RichText::new(format!("{drops}  ({})", parts.join(", "))).color(if drops == 0 { GOOD } else { WARN }),
        );
        let (dec, dec_col) = decoder_label(status, s);
        row(ui, "Decoder", RichText::new(dec).color(dec_col));
        let behind = s.cache_s + s.pipe_s;
        let lat_color = if behind <= 1.2 {
            GOOD
        } else if behind <= 3.0 {
            WARN
        } else {
            BAD
        };
        let speed = if s.speed > 1.001 {
            format!(" · catching up ×{:.2}", s.speed)
        } else if s.speed < 0.999 {
            format!(" · building a cushion ×{:.2}", s.speed)
        } else {
            String::new()
        };
        row(ui, "Buffered", RichText::new(format!("{behind:.2} s{speed}")).color(lat_color));
        row(
            ui,
            "Network",
            plain(format!("{:.0} kbit/s in · stream {:.0} kbit/s", s.download_kbps, s.video_kbps + s.audio_kbps)),
        );
        row(
            ui,
            "Segments",
            plain(format!(
                "{} ok · {} skipped{}{}",
                s.segments_done,
                s.segments_skipped,
                if status.low_latency { " · low-latency" } else { "" },
                if s.reopens > 0 { format!(" · {} reopen(s)", s.reopens) } else { String::new() }
            )),
        );
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    /// Table when there are many streams, layout otherwise.
    #[default]
    Auto,
    Layout,
    Table,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortBy {
    #[default]
    Layout,
    Name,
    Fps,
    Dropped,
    Buffered,
    Network,
}

/// What the stats window remembers between passes.
#[derive(Default)]
pub struct Ui {
    pub view: View,
    pub sort: SortBy,
    pub descending: bool,
}

pub struct Ctx<'a> {
    pub rects: &'a [TileRect],
    pub statuses: HashMap<u64, Status>,
    pub stats: HashMap<u64, StreamStats>,
    pub chat_lines: HashMap<String, usize>,
    pub rates: &'a Rates,
    pub adapter: &'a str,
    pub gpus: &'a [crate::gpu::Gpu],
    pub render_luid: (u32, i32),
    pub on_tiles: &'a mut bool,
    pub state: &'a mut Ui,
}

fn decoder_label(status: &Status, s: &StreamStats) -> (String, Color32) {
    use crate::stream::DecodeOn;
    if !matches!(status.state, State::Playing) || status.in_ad {
        return ("—".into(), MUTED);
    }
    match status.decode_on {
        DecodeOn::Cpu => ("CPU (GPU decoders full)".into(), WARN),
        DecodeOn::Second(_) if s.hwdec.contains("d3d11") => (format!("{} · copied", status.decoder), GOOD),
        DecodeOn::Gpu if s.hwdec.contains("d3d11") => (format!("{} · zero-copy", status.decoder), GOOD),
        _ if s.hwdec.is_empty() || s.hwdec == "no" => ("software".into(), WARN),
        _ => (s.hwdec.clone(), TEXT),
    }
}

/// The whole stats view (drawn in its own window).
pub fn contents(ui: &mut egui::Ui, mut c: Ctx<'_>) {
    let r = c.rates;
    ui.horizontal(|ui| {
        let pill = |ui: &mut egui::Ui, k: &str, v: String, col: Color32| {
            egui::Frame::NONE.fill(RAISED).corner_radius(4).inner_margin(egui::Margin::symmetric(8, 3)).show(
                ui,
                |ui| {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::Label::new(RichText::new(k).color(MUTED).size(12.0))
                                .wrap_mode(egui::TextWrapMode::Extend),
                        );
                        ui.add(
                            egui::Label::new(RichText::new(v).color(col).size(12.0))
                                .wrap_mode(egui::TextWrapMode::Extend),
                        );
                    });
                },
            );
        };
        pill(ui, "Display", format!("{:.0} presents/s", r.presents_s), TEXT);
        pill(
            ui,
            "UI",
            format!("{:.1} passes/s · {:.2} ms each", r.passes_s, r.pass_ms),
            if r.pass_ms < 4.0 { GOOD } else { WARN },
        );
        pill(
            ui,
            "Render thread",
            format!("{:.1} % of a core", r.render_busy_pct),
            if r.render_busy_pct < 40.0 { GOOD } else { WARN },
        );
        pill(ui, "Emote animation", format!("{:.0} redraws/s", r.anim_s), TEXT);
        pill(ui, "Slow presents", r.slow_waits.to_string(), if r.slow_waits == 0 { GOOD } else { WARN });
    });
    ui.horizontal(|ui| {
        let pill = |ui: &mut egui::Ui, k: &str, v: String, col: Color32| {
            egui::Frame::NONE.fill(RAISED).corner_radius(4).inner_margin(egui::Margin::symmetric(8, 3)).show(
                ui,
                |ui| {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::Label::new(RichText::new(k).color(MUTED).size(12.0))
                                .wrap_mode(egui::TextWrapMode::Extend),
                        );
                        ui.add(
                            egui::Label::new(RichText::new(v).color(col).size(12.0))
                                .wrap_mode(egui::TextWrapMode::Extend),
                        );
                    });
                },
            );
        };
        pill(ui, "Process", format!("{:.1} % CPU · {:.0} MB", r.cpu_pct, r.ram_mb), TEXT);
        // Video decoders, ours first.
        let mut gpus: Vec<&crate::gpu::Gpu> = c.gpus.iter().collect();
        gpus.sort_by_key(|g| g.luid != c.render_luid);
        for g in gpus {
            let mine = g.luid == c.render_luid;
            let col = if g.decode_pct >= 90.0 {
                BAD
            } else if g.decode_pct >= 70.0 {
                WARN
            } else {
                GOOD
            };
            let label = if mine { "Video decoder" } else { "Other GPU decoder" };
            pill(ui, label, format!("{:.0} % · {}", g.decode_pct, g.name), col);
        }
    });
    let streams: Vec<&TileRect> = c.rects.iter().filter(|t| matches!(t.kind, TileKind::Stream { .. })).collect();
    let table = match c.state.view {
        View::Auto => streams.len() > 9,
        View::Layout => false,
        View::Table => true,
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new("View").color(MUTED));
        let auto_label =
            if c.state.view == View::Auto { if table { "Auto (table)" } else { "Auto (layout)" } } else { "Auto" };
        ui.selectable_value(&mut c.state.view, View::Auto, auto_label)
            .on_hover_text("A table above 9 streams, the tile layout otherwise");
        ui.selectable_value(&mut c.state.view, View::Layout, "Layout");
        ui.selectable_value(&mut c.state.view, View::Table, "Table");
        ui.separator();
        ui.checkbox(c.on_tiles, "Numbers on the tiles too");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(format!("Drawn by {}", c.adapter)).color(MUTED).size(12.0));
        });
    });
    // Chats: one line, they do not take space from the streams.
    let chats: Vec<String> = c
        .rects
        .iter()
        .filter_map(|t| match &t.kind {
            TileKind::Chat { channel } => Some(format!(
                "{} ({} kept)",
                channel.replace('~', ""),
                channel.split(" + ").map(|ch| c.chat_lines.get(ch).copied().unwrap_or(0)).sum::<usize>()
            )),
            _ => None,
        })
        .collect();
    if !chats.is_empty() {
        ui.label(RichText::new(format!("Chats: {}", chats.join(" · "))).color(MUTED).size(12.0));
    }
    ui.add_space(4.0);
    if streams.is_empty() {
        ui.label(RichText::new("No streams in this tab.").color(MUTED));
        return;
    }
    if table {
        stream_rows(ui, &mut c, &streams);
    } else {
        stream_map(ui, &c, &streams);
    }
}

/// The streams of the tab, placed like the tiles (chats left out).
fn stream_map(ui: &mut egui::Ui, c: &Ctx<'_>, streams: &[&TileRect]) {
    let src = streams.iter().map(|t| t.rect).reduce(|a, b| a.union(b)).unwrap_or(Rect::NOTHING);
    let avail = ui.available_size();
    if src.width() < 1.0 || src.height() < 1.0 {
        return;
    }
    let scale = (avail.x / src.width()).min(avail.y / src.height()).max(0.2);
    let (map, _) = ui.allocate_exact_size(src.size() * scale, egui::Sense::hover());
    let to_map = |r: Rect| Rect::from_min_max(map.min + (r.min - src.min) * scale, map.min + (r.max - src.min) * scale);
    for t in streams {
        let TileKind::Stream { id, channel } = &t.kind else { continue };
        let channel = channel.trim_start_matches('~');
        let cell = to_map(t.rect).shrink(2.0);
        ui.painter().rect_filled(cell, 6.0, PANEL);
        ui.painter().rect_stroke(cell, 6.0, egui::Stroke::new(1.0, RAISED), egui::StrokeKind::Inside);
        let mut child = ui.new_child(
            egui::UiBuilder::new().max_rect(cell.shrink(8.0)).layout(egui::Layout::top_down(egui::Align::Min)),
        );
        child.set_clip_rect(cell.shrink(2.0));
        child.label(RichText::new(channel).font(crate::fonts::bold(14.0)).color(TEXT));
        let status = c.statuses.get(id).cloned().unwrap_or_default();
        let stats = c.stats.get(id).cloned().unwrap_or_default();
        egui::ScrollArea::both().id_salt(("stats-scroll", *id)).auto_shrink([false, false]).show(&mut child, |ui| {
            stream_table(ui, ("stats", *id), &status, &stats, c.rates, *id);
        });
    }
}

/// One row per stream, sortable.
fn stream_rows(ui: &mut egui::Ui, c: &mut Ctx<'_>, streams: &[&TileRect]) {
    struct Row {
        order: usize,
        id: u64,
        name: String,
        status: Status,
        s: StreamStats,
        shown: f64,
    }
    let mut rows: Vec<Row> = streams
        .iter()
        .enumerate()
        .filter_map(|(order, t)| match &t.kind {
            TileKind::Stream { id, channel } => Some(Row {
                order,
                id: *id,
                name: channel.trim_start_matches('~').to_owned(),
                status: c.statuses.get(id).cloned().unwrap_or_default(),
                s: c.stats.get(id).cloned().unwrap_or_default(),
                shown: c.rates.tile_fps.get(id).copied().unwrap_or(0.0),
            }),
            _ => None,
        })
        .collect();
    let key = |r: &Row| -> f64 {
        match c.state.sort {
            SortBy::Layout | SortBy::Name => 0.0,
            SortBy::Fps => r.shown - r.s.source_fps * r.s.speed.max(0.5),
            SortBy::Dropped => (r.s.drops_vo + r.s.drops_decoder) as f64,
            SortBy::Buffered => r.s.cache_s + r.s.pipe_s,
            SortBy::Network => r.s.download_kbps,
        }
    };
    match c.state.sort {
        SortBy::Layout => rows.sort_by_key(|r| r.order),
        SortBy::Name => rows.sort_by(|a, b| a.name.cmp(&b.name)),
        _ => rows.sort_by(|a, b| key(a).total_cmp(&key(b))),
    }
    if c.state.descending {
        rows.reverse();
    }
    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("stats-rows").striped(true).num_columns(9).spacing(vec2(16.0, 4.0)).show(ui, |ui| {
            let header = |ui: &mut egui::Ui, label: &str, by: SortBy| {
                let on = c.state.sort == by;
                let arrow = if on { if c.state.descending { " ▾" } else { " ▴" } } else { "" };
                // `c.state` is borrowed mutably below through a raw toggle.
                let r = ui.add(
                    egui::Label::new(
                        RichText::new(format!("{label}{arrow}")).color(if on { TEXT } else { MUTED }).strong(),
                    )
                    .sense(egui::Sense::click()),
                );
                r.clicked()
            };
            let mut clicked: Option<SortBy> = None;
            for (label, by) in [("#", SortBy::Layout), ("Stream", SortBy::Name)] {
                if header(ui, label, by) {
                    clicked = Some(by);
                }
            }
            ui.label(RichText::new("State").color(MUTED).strong());
            for (label, by) in [
                ("Frames shown", SortBy::Fps),
                ("Dropped", SortBy::Dropped),
                ("Buffered", SortBy::Buffered),
                ("Network", SortBy::Network),
            ] {
                if header(ui, label, by) {
                    clicked = Some(by);
                }
            }
            ui.label(RichText::new("Stream · viewport").color(MUTED).strong());
            ui.label(RichText::new("Decoder").color(MUTED).strong());
            ui.end_row();
            if let Some(by) = clicked {
                if c.state.sort == by {
                    c.state.descending = !c.state.descending;
                } else {
                    c.state.sort = by;
                    c.state.descending = matches!(by, SortBy::Dropped | SortBy::Buffered);
                }
            }
            for r in &rows {
                let st = &r.status;
                let s = &r.s;
                ui.label(RichText::new((r.order + 1).to_string()).color(MUTED));
                ui.label(RichText::new(&r.name).color(TEXT).strong());
                let state = match &st.state {
                    _ if st.in_ad => RichText::new("ad break").color(WARN),
                    State::Playing => RichText::new("playing").color(GOOD),
                    State::Starting => RichText::new("connecting").color(WARN),
                    State::Offline => RichText::new("offline").color(MUTED),
                    State::Error(e) => RichText::new(e.clone()).color(BAD),
                };
                ui.label(state);
                let expected = s.source_fps * s.speed.max(0.5);
                let fps_col = if s.source_fps <= 0.0 {
                    TEXT
                } else if r.shown >= expected * 0.9 && r.shown <= expected * 1.1 {
                    GOOD
                } else {
                    WARN
                };
                ui.label(RichText::new(format!("{:.1} / {:.0}", r.shown, s.source_fps)).color(fps_col));
                let drops = s.drops_vo + s.drops_decoder;
                ui.label(RichText::new(drops.to_string()).color(if drops == 0 { GOOD } else { WARN }));
                let behind = s.cache_s + s.pipe_s;
                let speed = if s.speed > 1.001 {
                    " »"
                } else if s.speed < 0.999 {
                    " «"
                } else {
                    ""
                };
                ui.label(RichText::new(format!("{behind:.2} s{speed}")).color(if behind <= 1.2 {
                    GOOD
                } else if behind <= 3.0 {
                    WARN
                } else {
                    BAD
                }));
                ui.label(RichText::new(format!("{:.0} kbit/s", s.download_kbps)).color(TEXT));
                let tile = c.rates.tiles.get(&r.id).copied().unwrap_or_default();
                ui.label(RichText::new(format!("{} · {}×{}", st.current, tile.width, tile.height)).color(TEXT));
                let (dec, col) = decoder_label(st, s);
                ui.label(RichText::new(dec).color(col));
                ui.end_row();
            }
        });
    });
}
