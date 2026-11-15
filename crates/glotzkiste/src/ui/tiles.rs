//! The tile area: stream tiles (video, hover bar, quality, audio focus),
//! chat panes and empty panes, arranged by egui_tiles.

use super::{ACCENT, BAD, GOOD, MUTED, PANEL, RAISED, TEXT, WARN, pill};
use crate::app::App;
use crate::chat_state::{BadgeMap, ChatState};
use crate::images::Images;
use crate::layout::{Pane, StreamPane};
use crate::stream::{Cmd, Quality, State, Status, StreamHandle};
use egui::{Color32, CornerRadius, FontId, Pos2, Rect, RichText, Sense, pos2, vec2};
use egui_tiles::{TileId, UiResponse};
use gk_render::VideoTile;
use gk_twitch::ChannelInfo;
use std::collections::HashMap;

enum Act {
    Remove(TileId),
    AddChat(TileId, String),
    Audio(String),
    MuteAll,
    Maximize(TileId),
    Send(String, String),
    Login,
    Add(String),
}

struct Behavior<'a> {
    streams: &'a HashMap<u64, StreamHandle>,
    chat: &'a mut ChatState,
    images: &'a mut Images,
    info: &'a HashMap<String, ChannelInfo>,
    audio: Option<String>,
    logged_in: bool,
    maximized: Option<TileId>,
    videos: &'a mut Vec<VideoTile>,
    heights: &'a mut Vec<(u64, u32)>,
    ppp: f32,
    acts: Vec<Act>,
    add_text: &'a mut String,
}

fn latency_color(tenths: u32) -> Color32 {
    match tenths {
        0..=12 => GOOD,
        13..=30 => WARN,
        _ => BAD,
    }
}

fn quality_label(q: &Quality, st: &Status) -> String {
    match q {
        Quality::Auto if st.current.is_empty() => "Auto".into(),
        Quality::Auto => format!("Auto · {}", st.current),
        Quality::Named(n) => n.clone(),
    }
}

impl Behavior<'_> {
    fn stream_ui(&mut self, ui: &mut egui::Ui, tile: TileId, p: &mut StreamPane) -> UiResponse {
        let rect = ui.max_rect();
        let ppp = self.ppp;
        // Whole pixels: the stream texture is exactly this size (1:1).
        let px = [
            (rect.min.x * ppp).round() as i32,
            (rect.min.y * ppp).round() as i32,
            (rect.max.x * ppp).round() as i32,
            (rect.max.y * ppp).round() as i32,
        ];
        let aligned = Rect::from_min_max(pos2(px[0] as f32 / ppp, px[1] as f32 / ppp), pos2(px[2] as f32 / ppp, px[3] as f32 / ppp));
        let handle = self.streams.get(&p.id);
        let st = handle.map(|s| s.status()).unwrap_or_default();
        let painter = ui.painter().clone();
        painter.rect_filled(aligned, 0.0, Color32::BLACK);
        if p.id != 0 && px[2] > px[0] && px[3] > px[1] {
            painter.image(egui::TextureId::User(p.id), aligned, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
            self.videos.push(VideoTile { id: p.id, rect_px: px });
            self.heights.push((p.id, (px[3] - px[1]).max(1) as u32));
        }
        let audible = self.audio.as_deref() == Some(p.channel.as_str());
        let info = self.info.get(&p.channel);
        let name = info.map(|i| i.display_name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| p.channel.clone());

        // Centre message for anything but normal playback.
        let msg = match &st.state {
            _ if st.in_ad => Some((format!("{name}: ad break"), "Skipping it. With Turbo there are none.".to_owned())),
            State::Starting => Some((name.clone(), "Connecting…".into())),
            State::Offline => Some((name.clone(), "Offline. Glotzkiste starts it when it goes live.".into())),
            State::Error(e) => Some((name.clone(), e.clone())),
            State::Playing => None,
        };
        if let Some((title, sub)) = msg {
            let c = rect.center();
            painter.text(c - vec2(0.0, 12.0), egui::Align2::CENTER_CENTER, title, crate::fonts::bold(18.0), TEXT);
            painter.text(c + vec2(0.0, 12.0), egui::Align2::CENTER_CENTER, sub, FontId::proportional(14.0), MUTED);
        }

        // Clicks on the video: audio focus; double click: maximize.
        let body = ui.interact(rect, ui.scope_id().with(("video", tile)), Sense::click());
        if body.double_clicked() {
            self.acts.push(Act::Maximize(tile));
        } else if body.clicked() && !audible {
            self.acts.push(Act::Audio(p.channel.clone()));
        }

        // The tile with the sound has an accent frame: visible at a glance.
        if audible {
            painter.rect_stroke(aligned.shrink(1.0), 0.0, egui::Stroke::new(2.0, ACCENT), egui::StrokeKind::Inside);
        }
        let hovered = ui.rect_contains_pointer(rect);
        let mut response = UiResponse::None;
        if !hovered {
            if audible {
                painter.text(rect.left_top() + vec2(10.0, 8.0), egui::Align2::LEFT_TOP, "🔊", FontId::proportional(14.0), TEXT);
            }
            return response;
        }
        // Hover bar: drag handle, name, title, latency, quality, volume,
        // chat, maximize, close.
        let bar = Rect::from_min_max(rect.min, pos2(rect.max.x, rect.min.y + 34.0));
        painter.rect_filled(bar, 0.0, Color32::from_black_alpha(185));
        let drag = ui.interact(bar, ui.scope_id().with(("drag", tile)), Sense::click_and_drag());
        if drag.drag_started() {
            response = UiResponse::DragStarted;
        }
        if drag.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
        if drag.double_clicked() {
            self.acts.push(Act::Maximize(tile));
        }
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(bar.shrink2(vec2(8.0, 4.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
        let ui = &mut child;
        ui.label(RichText::new(if audible { "🔊" } else { "🔈" }).color(if audible { TEXT } else { MUTED }));
        ui.label(RichText::new(&name).font(crate::fonts::bold(14.0)).color(TEXT));
        if let Some(i) = info.filter(|i| i.live) {
            let viewers = if i.viewers >= 1000 { format!("{:.1}k", i.viewers as f64 / 1000.0) } else { i.viewers.to_string() };
            ui.label(RichText::new(format!("{viewers} · {}", i.category)).color(MUTED).size(12.0)).on_hover_text(&i.title);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add(egui::Button::new("✕").frame(false)).on_hover_text("Remove").clicked() {
                self.acts.push(Act::Remove(tile));
            }
            let max_label = if self.maximized == Some(tile) { "🗗" } else { "⛶" };
            if ui.add(egui::Button::new(max_label).frame(false)).on_hover_text("Maximize in this tab (double-click, Esc)").clicked() {
                self.acts.push(Act::Maximize(tile));
            }
            if ui.add(egui::Button::new("💬").frame(false)).on_hover_text("Open the chat beside it").clicked() {
                self.acts.push(Act::AddChat(tile, p.channel.clone()));
            }
            // Volume (only for the stream with the sound).
            if audible {
                let mut v = p.volume as f32;
                let r = ui.add_sized([80.0, 18.0], egui::Slider::new(&mut v, 0.0..=100.0).show_value(false)).on_hover_text(format!("Volume {v:.0}"));
                if r.changed() {
                    p.volume = f64::from(v);
                    if let Some(h) = handle {
                        h.send(Cmd::Volume(p.volume));
                    }
                }
                if ui.add(egui::Button::new("🔇").frame(false)).on_hover_text("Mute").clicked() {
                    self.acts.push(Act::MuteAll);
                }
            } else if ui.add(egui::Button::new("🔈 Listen").frame(false)).clicked() {
                self.acts.push(Act::Audio(p.channel.clone()));
            }
            // Quality.
            let label = quality_label(&p.quality, &st);
            egui::ComboBox::from_id_salt(("quality", tile)).selected_text(label).width(120.0).show_ui(ui, |ui| {
                let auto_hint = if st.current.is_empty() { "Auto".to_owned() } else { format!("Auto ({})", st.current) };
                if ui.selectable_label(p.quality == Quality::Auto, auto_hint).on_hover_text("Matches the tile size").clicked() {
                    p.quality = Quality::Auto;
                    if let Some(h) = handle {
                        h.send(Cmd::Quality(Quality::Auto));
                    }
                }
                for v in &st.variants {
                    let label = if v == "audio_only" { "Audio only".to_owned() } else { v.clone() };
                    let q = Quality::Named(v.clone());
                    if ui.selectable_label(p.quality == q, label).clicked() {
                        p.quality = q.clone();
                        if let Some(h) = handle {
                            h.send(Cmd::Quality(q));
                        }
                    }
                }
            });
            // Latency, coloured.
            if matches!(st.state, State::Playing) {
                let t = st.buffer_tenths;
                let text = format!("{}{:.1}s", if st.speed_up { "» " } else { "" }, t as f64 / 10.0);
                pill(ui, &text, RAISED, latency_color(t));
                ui.label("").on_hover_text("Delay behind the live edge we receive (Twitch adds its own ~1-2 s). » = catching up.");
                if st.drops > 0 {
                    ui.label(RichText::new(format!("{} dropped", st.drops)).color(MUTED).size(11.0));
                }
            }
        });
        response
    }

    fn chat_ui(&mut self, ui: &mut egui::Ui, tile: TileId, channel: &str) -> UiResponse {
        let rect = ui.max_rect();
        let painter = ui.painter().clone();
        painter.rect_filled(rect, 0.0, PANEL);
        let header = Rect::from_min_max(rect.min, pos2(rect.max.x, rect.min.y + 30.0));
        painter.rect_filled(header, 0.0, Color32::from_rgb(31, 31, 35));
        let mut response = UiResponse::None;
        let drag = ui.interact(header, ui.scope_id().with(("chatdrag", tile)), Sense::click_and_drag());
        if drag.drag_started() {
            response = UiResponse::DragStarted;
        }
        let dot = if self.chat.connected { GOOD } else { WARN };
        let mut hui = ui.new_child(egui::UiBuilder::new().max_rect(header.shrink2(vec2(10.0, 4.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
        hui.label(RichText::new("●").color(dot).size(10.0)).on_hover_text(if self.chat.connected { "Connected" } else { "Connecting…" });
        hui.label(RichText::new(format!("{channel} chat")).font(crate::fonts::bold(14.0)).color(TEXT));
        hui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add(egui::Button::new("✕").frame(false)).on_hover_text("Close chat").clicked() {
                self.acts.push(Act::Remove(tile));
            }
        });
        let body = Rect::from_min_max(pos2(rect.min.x, header.max.y), rect.max);
        let global: BadgeMap = self.chat.global_badges().clone();
        let can_send = self.logged_in;
        if let Some(ch) = self.chat.channels.get_mut(channel) {
            let badges = ch.badges.clone();
            let mut paint = super::chat::Paint { images: self.images, badges: &badges, global_badges: &global };
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(body));
            match ch.view.show(&mut child, body, &ch.buffer, can_send, &mut paint) {
                super::chat::Action::Send(t) => self.acts.push(Act::Send(channel.to_owned(), t)),
                super::chat::Action::Login => self.acts.push(Act::Login),
                super::chat::Action::None => {}
            }
        } else {
            painter.text(body.center(), egui::Align2::CENTER_CENTER, "Joining…", FontId::proportional(14.0), MUTED);
        }
        response
    }

    fn empty_ui(&mut self, ui: &mut egui::Ui, tile: TileId) -> UiResponse {
        let rect = ui.max_rect();
        ui.painter().rect_stroke(rect.shrink(6.0), CornerRadius::same(10), egui::Stroke::new(1.0, RAISED), egui::StrokeKind::Inside);
        let inner = Rect::from_center_size(rect.center(), vec2(300.0_f32.min(rect.width() - 20.0), 90.0));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::top_down(egui::Align::Center)));
        child.label(RichText::new("Watch a channel").font(crate::fonts::bold(18.0)).color(TEXT));
        let r = child.add(egui::TextEdit::singleline(self.add_text).hint_text("channel name or twitch.tv link").desired_width(inner.width()).id_salt(("empty-add", tile)));
        if r.lost_focus() && child.input(|i| i.key_pressed(egui::Key::Enter)) {
            self.acts.push(Act::Add(std::mem::take(self.add_text)));
        }
        child.label(RichText::new("Enter adds it here. Ctrl+T opens a new tab.").color(MUTED).size(12.0));
        UiResponse::None
    }
}

impl egui_tiles::Behavior<Pane> for Behavior<'_> {
    fn pane_ui(&mut self, ui: &mut egui::Ui, tile: TileId, pane: &mut Pane) -> UiResponse {
        match pane {
            Pane::Stream(p) => self.stream_ui(ui, tile, p),
            Pane::Chat { channel } => {
                let c = channel.clone();
                self.chat_ui(ui, tile, &c)
            }
            Pane::Empty => self.empty_ui(ui, tile),
        }
    }

    fn tab_title_for_pane(&mut self, pane: &Pane) -> egui::WidgetText {
        match pane {
            Pane::Stream(p) => self.info.get(&p.channel).map(|i| i.display_name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| p.channel.clone()).into(),
            Pane::Chat { channel } => format!("{channel} chat").into(),
            Pane::Empty => "empty".into(),
        }
    }

    fn gap_width(&self, _style: &egui::Style) -> f32 {
        3.0
    }

    fn tab_bar_height(&self, _style: &egui::Style) -> f32 {
        28.0
    }

    fn tab_bar_color(&self, _visuals: &egui::Visuals) -> Color32 {
        PANEL
    }

    fn simplification_options(&self) -> egui_tiles::SimplificationOptions {
        egui_tiles::SimplificationOptions { all_panes_must_have_tabs: false, ..Default::default() }
    }

    fn drag_preview_color(&self, _visuals: &egui::Visuals) -> Color32 {
        ACCENT.gamma_multiply(0.35)
    }
}

pub fn content(app: &mut App, ui: &mut egui::Ui, videos: &mut Vec<VideoTile>, heights: &mut Vec<(u64, u32)>) {
    let ppp = ui.ctx().pixels_per_point();
    let active = app.active.min(app.tabs.len().saturating_sub(1));
    let audio = app.tabs.get(active).and_then(|t| t.audio.clone());
    let mut b = Behavior {
        streams: &app.streams,
        chat: &mut app.chat,
        images: &mut app.images,
        info: &app.info,
        audio,
        logged_in: app.auth.is_some(),
        maximized: app.maximized,
        videos,
        heights,
        ppp,
        acts: Vec::new(),
        add_text: &mut app.add_text,
    };
    let Some(tab) = app.tabs.get_mut(active) else { return };
    // A maximized tile fills the tab; the others keep running.
    let max_pane = app.maximized.filter(|id| matches!(tab.tree.tiles.get(*id), Some(egui_tiles::Tile::Pane(_))));
    if let Some(id) = max_pane {
        if let Some(egui_tiles::Tile::Pane(pane)) = tab.tree.tiles.get_mut(id) {
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(ui.max_rect()));
            let _ = egui_tiles::Behavior::pane_ui(&mut b, &mut child, id, pane);
        }
    } else {
        tab.tree.ui(&mut b, ui);
    }
    let acts = std::mem::take(&mut b.acts);
    drop(b);
    for a in acts {
        match a {
            Act::Remove(id) => {
                if app.maximized == Some(id) {
                    app.maximized = None;
                }
                if let Some(tab) = app.tabs.get_mut(active) {
                    tab.remove_tile(id);
                    let streams = tab.stream_channels();
                    if tab.audio.as_ref().is_some_and(|a| !streams.contains(a)) {
                        tab.audio = streams.first().cloned();
                    }
                }
            }
            Act::AddChat(id, channel) => {
                if let Some(tab) = app.tabs.get_mut(active) {
                    tab.add_chat_beside(id, &channel);
                }
                app.maximized = None;
            }
            Act::Audio(c) => {
                if let Some(tab) = app.tabs.get_mut(active) {
                    tab.audio = Some(c);
                }
            }
            Act::MuteAll => {
                if let Some(tab) = app.tabs.get_mut(active) {
                    tab.audio = Some(String::new());
                }
            }
            Act::Maximize(id) => app.maximized = if app.maximized == Some(id) { None } else { Some(id) },
            Act::Send(c, t) => app.chat.send(&c, &t),
            Act::Login => app.login.open = true,
            Act::Add(c) => app.add_channel(&c),
        }
    }
}
