//! The chat view: bottom-anchored and virtualized. Only lines on screen are
//! laid out; each line's layout (word wrap with inline emotes and badges) is
//! cached until the width or an image it waits for changes.

use crate::chat_state::BadgeMap;
use crate::images::Images;
use egui::{Color32, FontId, Pos2, Rect, Vec2, pos2, vec2};
use gk_chat::{ChatBuffer, ChatLine, Span};
use std::collections::HashMap;
use std::sync::Arc;

const FONT: f32 = 14.0;
const EMOTE_H: f32 = 28.0;
const BADGE_H: f32 = 18.0;
const LINE_GAP: f32 = 3.0;
const PAD: f32 = 8.0;

enum Item {
    Text { pos: Vec2, galley: Arc<egui::Galley>, link: Option<String> },
    Image { rect: Rect, tex: egui::TextureId, tip: String },
}

struct Laid {
    height: f32,
    items: Vec<Item>,
    /// Laid out with every image known (else redone when images arrive).
    complete: bool,
    images_gen: u64,
    highlight: Option<Color32>,
    bar: Option<Color32>,
}

#[derive(Default)]
pub struct ChatView {
    /// Pixels scrolled up from the newest line (0 = following).
    scroll_up: f32,
    last_seq: Option<u64>,
    new_below: usize,
    cache: HashMap<u64, Laid>,
    width: f32,
    pub input: String,
}

pub struct Paint<'a> {
    pub images: &'a mut Images,
    pub badges: &'a BadgeMap,
    pub global_badges: &'a BadgeMap,
}

pub enum Action {
    None,
    Send(String),
    Login,
}

/// Readable name colours on a dark background.
fn name_color(login: &str, color: Option<[u8; 3]>) -> Color32 {
    let [r, g, b] = color.unwrap_or_else(|| {
        const PALETTE: [[u8; 3]; 10] = [
            [255, 99, 99], [255, 160, 70], [240, 210, 80], [120, 220, 110], [80, 210, 200],
            [100, 170, 255], [170, 140, 255], [240, 120, 210], [200, 200, 200], [255, 140, 140],
        ];
        let h = login.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
        PALETTE[(h % PALETTE.len() as u32) as usize]
    });
    // Lift dark colours toward white until they read on the background.
    let lum = 0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b);
    if lum < 110.0 {
        let t = ((110.0 - lum) / 110.0).clamp(0.0, 0.7);
        let lift = |c: u8| (f32::from(c) + (255.0 - f32::from(c)) * t) as u8;
        Color32::from_rgb(lift(r), lift(g), lift(b))
    } else {
        Color32::from_rgb(r, g, b)
    }
}

struct Flow<'a> {
    width: f32,
    x: f32,
    row_top: f32,
    row: Vec<(Item, f32)>, // item and its height
    out: Vec<Item>,
    ctx: &'a egui::Context,
}

impl Flow<'_> {
    fn line_height(&self) -> f32 {
        FONT + 6.0
    }
    fn break_row(&mut self) {
        let h = self.row.iter().map(|(_, h)| *h).fold(self.line_height(), f32::max);
        for (mut it, ih) in self.row.drain(..) {
            let dy = self.row_top + (h - ih) / 2.0;
            match &mut it {
                Item::Text { pos, .. } => pos.y += dy,
                Item::Image { rect, .. } => *rect = rect.translate(vec2(0.0, dy)),
            }
            self.out.push(it);
        }
        self.row_top += h;
        self.x = 0.0;
    }
    fn place(&mut self, w: f32, h: f32, make: impl FnOnce(f32) -> Item) {
        if self.x > 0.0 && self.x + w > self.width {
            self.break_row();
        }
        let it = make(self.x);
        self.row.push((it, h));
        self.x += w;
    }
    fn text(&mut self, s: &str, font: FontId, color: Color32, link: Option<String>) {
        // Break into words keeping their trailing spaces, so rows wrap at
        // spaces; a word wider than the view is wrapped by egui itself.
        let mut start = 0;
        let b = s.as_bytes();
        while start < s.len() {
            let mut end = s[start..].find(' ').map_or(s.len(), |i| start + i);
            while end < s.len() && b[end] == b' ' {
                end += 1;
            }
            let piece = s[start..end].to_owned();
            let galley = self.ctx.fonts_mut(|f| f.layout_no_wrap(piece, font.clone(), color));
            if galley.size().x > self.width {
                if self.x > 0.0 {
                    self.break_row();
                }
                let wrapped = self.ctx.fonts_mut(|f| f.layout(s[start..end].to_owned(), font.clone(), color, self.width));
                let sz = wrapped.size();
                self.place(sz.x, sz.y, |x| Item::Text { pos: vec2(x, 0.0), galley: wrapped, link: link.clone() });
                self.break_row();
            } else {
                let sz = galley.size();
                self.place(sz.x, sz.y, |x| Item::Text { pos: vec2(x, 0.0), galley, link: link.clone() });
            }
            start = end;
        }
    }
    fn image(&mut self, tex: egui::TextureId, size: Vec2, h: f32, tip: String) {
        let w = if size.y > 0.0 { size.x / size.y * h } else { h };
        self.place(w + 2.0, h, |x| Item::Image { rect: Rect::from_min_size(pos2(x + 1.0, 0.0), vec2(w, h)), tex, tip });
    }
    /// A zero-width emote stacks on the previous image in this row.
    fn overlay(&mut self, tex: egui::TextureId, size: Vec2, tip: String) -> bool {
        let Some(base) = self.row.iter().rev().find_map(|(it, _)| if let Item::Image { rect, .. } = it { Some(*rect) } else { None }) else {
            return false;
        };
        let w = if size.y > 0.0 { size.x / size.y * EMOTE_H } else { EMOTE_H };
        let rect = Rect::from_center_size(base.center(), vec2(w, EMOTE_H));
        self.row.push((Item::Image { rect, tex, tip }, base.height()));
        true
    }
}

fn layout(line: &ChatLine, width: f32, ctx: &egui::Context, p: &mut Paint<'_>) -> Laid {
    let mut f = Flow { width, x: 0.0, row_top: 0.0, row: Vec::new(), out: Vec::new(), ctx };
    let mut complete = true;
    let m = &line.msg;
    let dim = |c: Color32| if line.deleted { c.gamma_multiply(0.35) } else { c };
    let text_color = dim(Color32::from_gray(225));
    let font = FontId::proportional(FONT);
    let mut bar = None;
    if let Some(sys) = &line.system {
        bar = Some(Color32::from_rgb(145, 70, 255));
        f.text(sys, font.clone(), Color32::from_rgb(190, 170, 255), None);
        if !m.text.is_empty() {
            f.break_row();
        }
    }
    if let Some((who, body)) = &m.reply_to {
        let short: String = body.chars().take(60).collect();
        f.text(&format!("↪ {who}: {short}"), FontId::proportional(12.0), Color32::from_gray(140), None);
        f.break_row();
    }
    if !m.login.is_empty() {
        for (set, ver) in &m.badges {
            let url = p.badges.get(&(set.clone(), ver.clone())).or_else(|| p.global_badges.get(&(set.clone(), ver.clone())));
            if let Some(url) = url {
                match p.images.get(url) {
                    Some(img) => f.image(img.texture_id(), img.size(), BADGE_H, set.clone()),
                    None => complete = false,
                }
            }
        }
        let name = if m.display_name.eq_ignore_ascii_case(&m.login) { m.display_name.clone() } else { format!("{} ({})", m.display_name, m.login) };
        let nc = dim(name_color(&m.login, m.color));
        f.text(&name, crate::fonts::bold(FONT), nc, None);
        f.text(if m.action { " " } else { ": " }, font.clone(), text_color, None);
        let body_color = if m.action { nc } else { text_color };
        for s in &line.spans {
            match s {
                Span::Text(t) => f.text(t, font.clone(), body_color, None),
                Span::Mention { name, me } => {
                    let c = if *me { Color32::from_rgb(255, 120, 120) } else { Color32::from_rgb(180, 190, 255) };
                    f.text(&format!("@{name}"), crate::fonts::bold(FONT), dim(c), None);
                }
                Span::Link(u) => {
                    let url = if u.starts_with("http") { u.clone() } else { format!("https://{u}") };
                    f.text(u, font.clone(), dim(Color32::from_rgb(110, 170, 255)), Some(url));
                }
                Span::Emote { name, url, zero_width, .. } => match p.images.get(url) {
                    Some(img) => {
                        if !(*zero_width && f.overlay(img.texture_id(), img.size(), name.clone())) {
                            f.image(img.texture_id(), img.size(), EMOTE_H, name.clone());
                        }
                    }
                    None => {
                        complete = false;
                        f.text(name, font.clone(), Color32::from_gray(120), None);
                    }
                },
            }
        }
    }
    f.break_row();
    let highlight = if line.mention {
        Some(Color32::from_rgba_unmultiplied(255, 60, 60, 38))
    } else if m.highlighted || m.first_message {
        Some(Color32::from_rgba_unmultiplied(145, 70, 255, 34))
    } else {
        None
    };
    Laid { height: f.row_top, items: f.out, complete, images_gen: p.images.generation, highlight, bar }
}

impl ChatView {
    pub fn invalidate(&mut self) {
        self.cache.clear();
    }

    fn laid<'a>(&'a mut self, line: &ChatLine, width: f32, ctx: &egui::Context, p: &mut Paint<'_>) -> &'a Laid {
        let stale = match self.cache.get(&line.seq) {
            None => true,
            Some(l) => !l.complete && l.images_gen != p.images.generation,
        };
        if stale {
            let l = layout(line, width, ctx, p);
            self.cache.insert(line.seq, l);
        }
        &self.cache[&line.seq]
    }

    /// Draws the messages and the input line inside `rect`.
    pub fn show(&mut self, ui: &mut egui::Ui, rect: Rect, buffer: &ChatBuffer, can_send: bool, p: &mut Paint<'_>) -> Action {
        let mut action = Action::None;
        let ctx = ui.ctx().clone();
        let input_h = 34.0;
        let list = Rect::from_min_max(rect.min, pos2(rect.max.x, rect.max.y - input_h));
        let width = (list.width() - 2.0 * PAD).max(40.0);
        if (width - self.width).abs() > 0.5 {
            self.width = width;
            self.cache.clear();
        }
        // Drop layouts of lines that left the buffer.
        if let Some(first) = buffer.get(0) {
            let oldest = first.seq;
            if self.cache.len() > buffer.len() + 64 {
                self.cache.retain(|seq, _| *seq >= oldest);
            }
        }
        // New lines while scrolled up keep the view still.
        let newest = buffer.iter().next_back().map(|l| l.seq);
        if let (Some(prev), Some(_)) = (self.last_seq, newest) {
            if self.scroll_up > 0.0 {
                let new: Vec<&ChatLine> = buffer.iter().rev().take_while(|l| l.seq > prev).collect();
                for l in new {
                    let h = self.laid(l, width, &ctx, p).height + LINE_GAP;
                    self.scroll_up += h;
                    self.new_below += 1;
                }
            }
        }
        self.last_seq = newest;

        let resp = ui.interact(list, ui.scope_id().with("chat-list"), egui::Sense::click());
        if resp.hovered() {
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            if dy != 0.0 {
                self.scroll_up = (self.scroll_up + dy).max(0.0);
            }
        }
        if self.scroll_up <= 0.0 {
            self.new_below = 0;
        }
        let painter = ui.painter().with_clip_rect(list);
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let mut hover_tip: Option<String> = None;
        let mut hover_link: Option<String> = None;
        let mut y = list.max.y - PAD + self.scroll_up;
        let mut reached_top = true;
        for line in buffer.iter().rev() {
            let laid = self.laid(line, width, &ctx, p);
            let top = y - laid.height;
            if top > list.max.y {
                y = top - LINE_GAP;
                continue;
            }
            let origin = pos2(list.min.x + PAD, top);
            let full = Rect::from_min_max(pos2(list.min.x, top - 1.0), pos2(list.max.x, y + 1.0));
            if let Some(h) = laid.highlight {
                painter.rect_filled(full, 0.0, h);
            }
            if let Some(b) = laid.bar {
                painter.rect_filled(Rect::from_min_max(full.min, pos2(full.min.x + 3.0, full.max.y)), 0.0, b);
            }
            for it in &laid.items {
                match it {
                    Item::Text { pos, galley, link } => {
                        let at = origin + *pos;
                        painter.galley(at, galley.clone(), Color32::WHITE);
                        if let (Some(u), Some(pt)) = (link, pointer) {
                            if Rect::from_min_size(at, galley.size()).contains(pt) {
                                hover_link = Some(u.clone());
                            }
                        }
                    }
                    Item::Image { rect, tex, tip } => {
                        let r = rect.translate(origin.to_vec2());
                        painter.image(*tex, r, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
                        if pointer.is_some_and(|pt| r.contains(pt)) {
                            hover_tip = Some(tip.clone());
                        }
                    }
                }
            }
            y = top - LINE_GAP;
            if y < list.min.y {
                reached_top = false;
                break;
            }
        }
        // Clamp scrolling at the oldest line.
        if reached_top && y > list.min.y + PAD && self.scroll_up > 0.0 {
            self.scroll_up = (self.scroll_up - (y - list.min.y - PAD)).max(0.0);
        }
        if resp.hovered() {
            if let Some(t) = hover_tip {
                egui::Tooltip::always_open(ctx.clone(), ui.layer_id(), ui.scope_id().with("emote-tip"), egui::PopupAnchor::Pointer).show(|ui| {
                    ui.label(t);
                });
            }
            if let Some(u) = &hover_link {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                if resp.clicked() {
                    ctx.open_url(egui::OpenUrl::new_tab(u));
                }
            }
        }
        if self.new_below > 0 {
            let r = Rect::from_center_size(pos2(list.center().x, list.max.y - 20.0), vec2(170.0, 26.0));
            let label = format!("{} new message{} ↓", self.new_below, if self.new_below == 1 { "" } else { "s" });
            if ui.put(r, egui::Button::new(label).fill(Color32::from_rgb(145, 70, 255))).clicked() {
                self.scroll_up = 0.0;
                self.new_below = 0;
            }
        } else if self.scroll_up > 0.0 {
            let r = Rect::from_center_size(pos2(list.center().x, list.max.y - 20.0), vec2(150.0, 26.0));
            if ui.put(r, egui::Button::new("Back to live ↓")).clicked() {
                self.scroll_up = 0.0;
            }
        }
        // Input line.
        let input = Rect::from_min_max(pos2(rect.min.x + 6.0, list.max.y + 4.0), pos2(rect.max.x - 6.0, rect.max.y - 4.0));
        if can_send {
            let te = egui::TextEdit::singleline(&mut self.input).hint_text("Send a message").char_limit(500).desired_width(input.width());
            let r = ui.put(input, te);
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let text = std::mem::take(&mut self.input);
                if !text.trim().is_empty() {
                    action = Action::Send(text);
                }
                r.request_focus();
            }
        } else if ui.put(input, egui::Button::new("Log in to chat").frame(false)).clicked() {
            action = Action::Login;
        }
        action
    }
}
