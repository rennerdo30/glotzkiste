//! Tabs of tile layouts: presets, adding channels and chats, saving.

use crate::stream::{Latency, Quality};
use egui_tiles::{Container, ContainerKind, Linear, LinearDir, Tile, TileId, Tiles, Tree};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StreamPane {
    pub channel: String,
    pub quality: Quality,
    pub volume: f64,
    #[serde(skip)]
    pub id: u64,
}

/// Which channels a chat pane shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ChatMode {
    /// Only `channel`.
    #[default]
    One,
    /// These channels merged into one list.
    Several(Vec<String>),
    /// Every stream in the tab, merged (follows the tab).
    AllStreams,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Pane {
    Stream(StreamPane),
    Chat {
        /// The single channel (`ChatMode::One`), and the fallback otherwise.
        channel: String,
        /// Layouts saved before combined chats load as `One`.
        #[serde(default)]
        mode: ChatMode,
    },
    Empty,
}

impl Pane {
    pub fn stream(channel: &str) -> Self {
        Pane::Stream(StreamPane { channel: channel.to_owned(), quality: Quality::Auto, volume: 80.0, id: 0 })
    }

    pub fn chat(channel: &str) -> Self {
        Pane::Chat { channel: channel.to_owned(), mode: ChatMode::One }
    }
}

/// The channels a chat pane shows, given the tab's streams (deduplicated,
/// in order).
pub fn chat_channels(channel: &str, mode: &ChatMode, tab_streams: &[String]) -> Vec<String> {
    let list: Vec<String> = match mode {
        ChatMode::One => vec![channel.to_owned()],
        ChatMode::Several(list) => list.clone(),
        ChatMode::AllStreams => tab_streams.to_vec(),
    };
    let mut out: Vec<String> = Vec::new();
    for c in list {
        if !c.is_empty() && !out.contains(&c) {
            out.push(c);
        }
    }
    if out.is_empty() && !channel.is_empty() {
        out.push(channel.to_owned());
    }
    out
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Tab {
    pub title: String,
    pub tree: Tree<Pane>,
    /// The stream that has the sound in this tab.
    #[serde(default)]
    pub audio: Option<String>,
    /// Grid tiles bigger than one cell (GK-16); empty: the plain grid.
    #[serde(default)]
    pub spans: Vec<Span>,
}

/// A grid tile spanning `w` x `h` cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Span {
    pub tile: TileId,
    pub w: u8,
    pub h: u8,
}

/// The largest span of a tile, in cells.
pub const MAX_SPAN: u32 = 4;

/// A tab that can be drawn as a packed grid: the grid's tiles in order and,
/// if the tab has a chat column, the body's share and the chat panes.
pub struct Packable {
    pub grid: Vec<TileId>,
    pub chat: Option<(f32, Vec<TileId>)>,
}

/// Places tiles of `spans` (w, h cells) in a packed grid for an area of
/// `size`: each tile takes the first free spot in reading order, so a
/// bigger tile pushes the others along. Picks the column count that gives
/// the videos (16:9) the most room. Returns columns, rows and positions.
pub fn pack(spans: &[(u32, u32)], size: (f32, f32)) -> (u32, u32, Vec<(u32, u32)>) {
    let max_w = spans.iter().map(|s| s.0).max().unwrap_or(1).max(1);
    let area: u32 = spans.iter().map(|s| s.0 * s.1).sum::<u32>().max(1);
    let mut best = (1, 1, Vec::new());
    let mut best_w = -1.0f32;
    for cols in max_w..=area.max(max_w).min(16) {
        let (pos, rows) = first_fit(spans, cols);
        let (cw, ch) = (size.0 / cols as f32, size.1 / rows.max(1) as f32);
        let video_w = cw.min(ch * 16.0 / 9.0);
        if video_w > best_w + 0.5 {
            best_w = video_w;
            best = (cols, rows, pos);
        }
    }
    best
}

/// Like [`pack`], but keeps `prev` columns unless another count gives the
/// videos clearly (15 %) more room: a resize does not reshuffle the grid.
pub fn pack_near(spans: &[(u32, u32)], size: (f32, f32), prev: Option<u32>) -> (u32, u32, Vec<(u32, u32)>) {
    let best = pack(spans, size);
    let max_w = spans.iter().map(|s| s.0).max().unwrap_or(1).max(1);
    let Some(prev) = prev.filter(|p| *p >= max_w && *p != best.0) else { return best };
    let score = |cols: u32, rows: u32| (size.0 / cols as f32).min(size.1 / rows.max(1) as f32 * 16.0 / 9.0);
    let (pos, rows) = first_fit(spans, prev);
    if score(prev, rows) * 1.15 >= score(best.0, best.1) { (prev, rows, pos) } else { best }
}

fn first_fit(spans: &[(u32, u32)], cols: u32) -> (Vec<(u32, u32)>, u32) {
    let mut used: Vec<Vec<bool>> = Vec::new();
    let mut out = Vec::with_capacity(spans.len());
    for &(w, h) in spans {
        let (w, h) = (w.clamp(1, cols), h.max(1));
        let mut y = 0u32;
        'search: loop {
            for x in 0..=cols - w {
                let free =
                    (y..y + h).all(|yy| (x..x + w).all(|xx| used.get(yy as usize).is_none_or(|row| !row[xx as usize])));
                if free {
                    for yy in y..y + h {
                        while used.len() <= yy as usize {
                            used.push(vec![false; cols as usize]);
                        }
                        for xx in x..x + w {
                            used[yy as usize][xx as usize] = true;
                        }
                    }
                    out.push((x, y));
                    break 'search;
                }
            }
            y += 1;
        }
    }
    (out, used.len() as u32)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// Every stream in a grid.
    Grid,
    /// Side by side.
    Columns,
    /// One big stream, the others stacked beside it.
    Spotlight,
    /// One at a time, switched with tabs.
    Stack,
}

impl Preset {
    pub const ALL: [Preset; 4] = [Preset::Grid, Preset::Columns, Preset::Spotlight, Preset::Stack];
    pub fn label(self) -> &'static str {
        match self {
            Preset::Grid => "Grid",
            Preset::Columns => "Side by side",
            Preset::Spotlight => "Spotlight: one big, others small",
            Preset::Stack => "One at a time (tabs)",
        }
    }
    pub fn icon(self) -> &'static str {
        match self {
            Preset::Grid => "▦",
            Preset::Columns => "▥",
            Preset::Spotlight => "◧",
            Preset::Stack => "▭",
        }
    }
}

fn new_id() -> egui::Id {
    egui::Id::unique(crate::random_u64())
}

impl Tab {
    pub fn new(title: &str, panes: Vec<Pane>) -> Self {
        let panes = if panes.is_empty() { vec![Pane::Empty] } else { panes };
        Self { title: title.to_owned(), tree: Tree::new_grid(new_id(), panes), audio: None, spans: Vec::new() }
    }

    /// Panes in reading order (depth first from the root).
    pub fn panes_in_order(&self) -> Vec<TileId> {
        fn walk(tiles: &Tiles<Pane>, id: TileId, out: &mut Vec<TileId>) {
            match tiles.get(id) {
                Some(Tile::Pane(_)) => out.push(id),
                Some(Tile::Container(c)) => {
                    for ch in c.children_vec() {
                        walk(tiles, ch, out);
                    }
                }
                None => {}
            }
        }
        let mut out = Vec::new();
        if let Some(root) = self.tree.root() {
            walk(&self.tree.tiles, root, &mut out);
        }
        out
    }

    pub fn stream_channels(&self) -> Vec<String> {
        self.panes_in_order()
            .into_iter()
            .filter_map(|id| match self.tree.tiles.get(id) {
                Some(Tile::Pane(Pane::Stream(s))) => Some(s.channel.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn has_chat(&self) -> bool {
        self.tree.tiles.tiles().any(|t| matches!(t, Tile::Pane(Pane::Chat { .. })))
    }

    fn stream_panes(&self) -> Vec<Pane> {
        self.panes_in_order()
            .into_iter()
            .filter_map(|id| match self.tree.tiles.get(id) {
                Some(Tile::Pane(p @ Pane::Stream(_))) => Some(p.clone()),
                _ => None,
            })
            .collect()
    }

    /// The tile's span in a packed grid (1 x 1 unless set).
    pub fn span_of(&self, tile: TileId) -> (u32, u32) {
        self.spans.iter().find(|s| s.tile == tile).map_or((1, 1), |s| (u32::from(s.w), u32::from(s.h)))
    }

    /// Sets a tile's span; 1 x 1 removes it.
    pub fn set_span(&mut self, tile: TileId, w: u32, h: u32) {
        self.spans.retain(|s| s.tile != tile && self.tree.tiles.get(s.tile).is_some());
        let (w, h) = (w.clamp(1, MAX_SPAN), h.clamp(1, MAX_SPAN));
        if (w, h) != (1, 1) {
            self.spans.push(Span { tile, w: w as u8, h: h as u8 });
        }
    }

    /// The grid this tab can draw packed: a grid of streams as the root,
    /// or beside a chat column.
    pub fn packable(&self) -> Option<Packable> {
        let tiles = &self.tree.tiles;
        let is_stream_grid = |id: TileId| match tiles.get(id) {
            Some(Tile::Container(c @ Container::Grid(_))) => {
                let kids: Vec<TileId> = c.children().copied().collect();
                (!kids.is_empty() && kids.iter().all(|k| matches!(tiles.get(*k), Some(Tile::Pane(Pane::Stream(_))))))
                    .then_some(kids)
            }
            _ => None,
        };
        let root = self.tree.root()?;
        if let Some(grid) = is_stream_grid(root) {
            return Some(Packable { grid, chat: None });
        }
        let Some(Tile::Container(Container::Linear(l))) = tiles.get(root) else { return None };
        if l.dir != LinearDir::Horizontal || l.children.len() != 2 {
            return None;
        }
        let (body, side) = (l.children[0], l.children[1]);
        let grid = is_stream_grid(body)?;
        let chats: Vec<TileId> = match tiles.get(side) {
            Some(Tile::Pane(Pane::Chat { .. })) => vec![side],
            Some(Tile::Container(Container::Linear(v)))
                if v.dir == LinearDir::Vertical
                    && v.children.iter().all(|c| matches!(tiles.get(*c), Some(Tile::Pane(Pane::Chat { .. })))) =>
            {
                v.children.clone()
            }
            _ => return None,
        };
        let total = l.shares[body] + l.shares[side];
        let share = if total > 0.0 { l.shares[body] / total } else { 0.78 };
        Some(Packable { grid, chat: Some((share, chats)) })
    }

    fn chat_panes(&self) -> Vec<Pane> {
        self.panes_in_order()
            .into_iter()
            .filter_map(|id| match self.tree.tiles.get(id) {
                Some(Tile::Pane(p @ Pane::Chat { .. })) => Some(p.clone()),
                _ => None,
            })
            .collect()
    }

    /// Adds a chat panel to the chat column on the right (GK-17): stacked
    /// under the chats already there, or as a new column.
    pub fn add_chat_panel(&mut self, pane: Pane) {
        let tiles = &mut self.tree.tiles;
        // The column: the parent of the last chat pane, when that is a
        // vertical stack of chats, or the chat pane itself.
        let last_chat = self.tree.root.and_then(|_| {
            tiles.iter().filter(|(_, t)| matches!(t, Tile::Pane(Pane::Chat { .. }))).map(|(id, _)| *id).last()
        });
        let new = tiles.insert_pane(pane);
        if let Some(chat) = last_chat {
            let parent = tiles.parent_of(chat);
            let stack = parent.filter(|p| {
                matches!(tiles.get(*p), Some(Tile::Container(Container::Linear(l))) if l.dir == LinearDir::Vertical
                    && l.children.iter().all(|c| matches!(tiles.get(*c), Some(Tile::Pane(Pane::Chat { .. })))))
            });
            if let Some(s) = stack
                && let Some(Tile::Container(c)) = tiles.get_mut(s)
            {
                c.add_child(new);
                return;
            }
            let lin = Linear::new_binary(LinearDir::Vertical, [chat, new], 0.5);
            let group = tiles.insert_container(Container::Linear(lin));
            match parent {
                Some(p) => {
                    if let Some(Tile::Container(c)) = tiles.get_mut(p) {
                        let _ = c.replace_child(chat, group);
                    }
                }
                None => self.tree.root = Some(group),
            }
            return;
        }
        match self.tree.root {
            Some(root) => {
                let lin = Linear::new_binary(LinearDir::Horizontal, [root, new], 0.78);
                self.tree.root = Some(tiles.insert_container(Container::Linear(lin)));
            }
            None => self.tree.root = Some(new),
        }
    }

    /// Rebuilds the layout; streams keep their players (same panes) and
    /// the chat panels stay, stacked in a column on the right; `chat` adds
    /// one for that channel (unless it is already shown on its own).
    pub fn apply(&mut self, preset: Preset, chat: Option<String>) {
        let streams = self.stream_panes();
        let mut chats = self.chat_panes();
        if let Some(c) = chat
            && !chats.iter().any(|p| matches!(p, Pane::Chat { channel, mode: ChatMode::One } if *channel == c))
        {
            chats.push(Pane::chat(&c));
        }
        let mut tiles: Tiles<Pane> = Tiles::default();
        let ids: Vec<TileId> = streams.into_iter().map(|p| tiles.insert_pane(p)).collect();
        let body = if ids.is_empty() {
            tiles.insert_pane(Pane::Empty)
        } else if ids.len() == 1 {
            ids[0]
        } else {
            match preset {
                Preset::Grid => tiles.insert_grid_tile(ids.clone()),
                Preset::Columns => tiles.insert_horizontal_tile(ids.clone()),
                Preset::Stack => tiles.insert_tab_tile(ids.clone()),
                Preset::Spotlight => {
                    let side = if ids.len() == 2 { ids[1] } else { tiles.insert_vertical_tile(ids[1..].to_vec()) };
                    let lin = Linear::new_binary(LinearDir::Horizontal, [ids[0], side], 0.72);
                    tiles.insert_container(Container::Linear(lin))
                }
            }
        };
        let chat_ids: Vec<TileId> = chats.into_iter().map(|p| tiles.insert_pane(p)).collect();
        let root = match chat_ids.len() {
            0 => body,
            n => {
                let column = if n == 1 { chat_ids[0] } else { tiles.insert_vertical_tile(chat_ids) };
                let lin = Linear::new_binary(LinearDir::Horizontal, [body, column], 0.78);
                tiles.insert_container(Container::Linear(lin))
            }
        };
        self.tree = Tree::new(new_id(), root, tiles);
        self.spans.clear();
    }

    /// Adds a stream: fills an empty pane, else joins the main stream group.
    pub fn add_stream(&mut self, channel: &str) {
        let tiles = &mut self.tree.tiles;
        let empty = tiles.iter().find_map(|(id, t)| matches!(t, Tile::Pane(Pane::Empty)).then_some(*id));
        if let Some(id) = empty {
            if let Some(Tile::Pane(p)) = tiles.get_mut(id) {
                *p = Pane::stream(channel);
            }
            return;
        }
        let new = tiles.insert_pane(Pane::stream(channel));
        // The container holding the most streams is the main group.
        let mut best: Option<(usize, TileId)> = None;
        for (id, t) in tiles.iter() {
            if let Tile::Container(c) = t {
                let n = c.children().filter(|ch| matches!(tiles.get(**ch), Some(Tile::Pane(Pane::Stream(_))))).count();
                if n > 0 && best.is_none_or(|(b, _)| n > b) {
                    best = Some((n, *id));
                }
            }
        }
        if let Some((_, cid)) = best
            && let Some(Tile::Container(c)) = tiles.get_mut(cid)
        {
            if c.kind() == ContainerKind::Horizontal && c.num_children() >= 2 {
                c.set_kind(ContainerKind::Grid);
            }
            c.add_child(new);
            return;
        }
        // A single stream (maybe with a chat): group it with the new one.
        let first_stream =
            tiles.iter().find_map(|(id, t)| (matches!(t, Tile::Pane(Pane::Stream(_))) && *id != new).then_some(*id));
        match first_stream {
            Some(s) => self.split(s, new, ContainerKind::Grid),
            None => {
                let root = self.tree.root();
                let grid = match root {
                    Some(r) => self.tree.tiles.insert_grid_tile(vec![r, new]),
                    None => new,
                };
                self.tree.root = Some(grid);
            }
        }
    }

    /// Puts `new` next to `target` in a new container of `kind`.
    fn split(&mut self, target: TileId, new: TileId, kind: ContainerKind) {
        let parent = self.tree.tiles.parent_of(target);
        let group = self.tree.tiles.insert_container(Container::new(kind, vec![target, new]));
        match parent {
            Some(p) => {
                if let Some(Tile::Container(c)) = self.tree.tiles.get_mut(p) {
                    let _ = c.replace_child(target, group);
                }
            }
            None => self.tree.root = Some(group),
        }
    }

    /// Adds a chat for `channel` to the right of its stream tile.
    pub fn add_chat_beside(&mut self, stream_tile: TileId, channel: &str) {
        let chat = self.tree.tiles.insert_pane(Pane::chat(channel));
        let parent = self.tree.tiles.parent_of(stream_tile);
        let lin = Linear::new_binary(LinearDir::Horizontal, [stream_tile, chat], 0.75);
        let group = self.tree.tiles.insert_container(Container::Linear(lin));
        match parent {
            Some(p) => {
                if let Some(Tile::Container(c)) = self.tree.tiles.get_mut(p) {
                    let _ = c.replace_child(stream_tile, group);
                }
            }
            None => self.tree.root = Some(group),
        }
    }

    pub fn remove_tile(&mut self, id: TileId) {
        self.tree.remove_recursively(id);
        if self.tree.root().is_none() || self.tree.tiles.tiles().all(|t| matches!(t, Tile::Container(_))) {
            self.tree = Tree::new_grid(new_id(), vec![Pane::Empty]);
        }
    }
}

/// The window's place on screen, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WindowGeom {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub maximized: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Saved {
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub latency: Latency,
    #[serde(default)]
    pub window: Option<WindowGeom>,
}

static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Keeps the state somewhere else (tests: never the viewer's own state).
pub fn set_dir(dir: std::path::PathBuf) {
    let _ = std::fs::create_dir_all(&dir);
    let _ = DIR.set(dir);
}

/// Where the layout and the settings are kept.
pub fn dir() -> Option<std::path::PathBuf> {
    match DIR.get() {
        Some(d) => Some(d.clone()),
        None => gk_twitch::config_dir(),
    }
}

fn path() -> Option<std::path::PathBuf> {
    dir().map(|d| d.join("layout.json"))
}

pub fn load() -> Option<Saved> {
    let text = std::fs::read_to_string(path()?).ok()?;
    let mut s: Saved = serde_json::from_str(&text).map_err(|e| log::warn!("layout.json: {e}")).ok()?;
    s.tabs.retain(|t| t.tree.root().is_some());
    if s.tabs.is_empty() {
        return None;
    }
    s.active = s.active.min(s.tabs.len() - 1);
    Some(s)
}

/// Serializes the state; the caller writes it off the UI thread.
pub fn to_json(tabs: &[Tab], active: usize, latency: Latency, window: Option<WindowGeom>) -> Option<String> {
    #[derive(serde::Serialize)]
    struct Ref<'a> {
        tabs: &'a [Tab],
        active: usize,
        latency: Latency,
        window: Option<WindowGeom>,
    }
    serde_json::to_string(&Ref { tabs, active, latency, window }).ok()
}

/// A layout file (GK-18): one tab, or every tab of a `layout.json`.
#[derive(serde::Serialize)]
struct ExportRef<'a> {
    glotzkiste_layout: u32,
    tabs: &'a [Tab],
}

/// The tab as a layout file's text.
pub fn export(tab: &Tab) -> Option<String> {
    serde_json::to_string_pretty(&ExportRef { glotzkiste_layout: 1, tabs: std::slice::from_ref(tab) }).ok()
}

/// The tabs in a layout file: our export, a `layout.json`, or a single tab.
pub fn import(text: &str) -> Result<Vec<Tab>, String> {
    #[derive(serde::Deserialize)]
    struct Tabs {
        tabs: Vec<Tab>,
    }
    let mut tabs = match serde_json::from_str::<Tabs>(text) {
        Ok(t) => t.tabs,
        Err(e) => {
            serde_json::from_str::<Tab>(text).map(|t| vec![t]).map_err(|_| format!("Not a layout file ({e})."))?
        }
    };
    tabs.retain(|t| t.tree.root().is_some());
    if tabs.is_empty() {
        return Err("The file has no layout in it.".into());
    }
    Ok(tabs)
}

pub fn write(json: String) {
    let Some(p) = path() else { return };
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn streams(t: &Tab) -> Vec<String> {
        t.stream_channels()
    }

    #[test]
    fn packing_pushes_tiles_along() {
        // Four 1x1 in a wide area: one row or 2x2, never a column.
        let (c, r, p) = pack(&[(1, 1); 4], (1600.0, 900.0));
        assert_eq!((c, r), (2, 2));
        assert_eq!(p, vec![(0, 0), (1, 0), (0, 1), (1, 1)]);
        // The first tile 2x2: the others flow around it.
        let (c, r, p) = pack(&[(2, 2), (1, 1), (1, 1), (1, 1)], (1600.0, 900.0));
        assert_eq!((c, r), (3, 3), "{p:?}");
        assert_eq!(p, vec![(0, 0), (2, 0), (2, 1), (0, 2)]);
        // Every tile placed, none overlapping.
        let spans = [(2, 1), (1, 2), (1, 1), (3, 1), (1, 1), (2, 2)];
        let (c, _, p) = pack(&spans, (1920.0, 1080.0));
        let mut cells = std::collections::HashSet::new();
        for (s, (x, y)) in spans.iter().zip(&p) {
            assert!(x + s.0.min(c) <= c);
            for yy in *y..y + s.1 {
                for xx in *x..x + s.0.min(c) {
                    assert!(cells.insert((xx, yy)), "overlap at {xx},{yy}");
                }
            }
        }
    }

    #[test]
    fn packing_keeps_its_columns_when_close() {
        let four = [(1, 1); 4];
        let (c, _, _) = pack(&four, (1600.0, 900.0));
        assert_eq!(c, 2);
        // A tile grows: three columns would be best, but two are kept while
        // they give nearly as much room.
        let grown = [(2, 1), (1, 1), (1, 1), (1, 1)];
        let (best, _, _) = pack(&grown, (1600.0, 900.0));
        let (kept, rows, pos) = pack_near(&grown, (1600.0, 900.0), Some(2));
        assert!(kept == 2 || kept == best, "{kept} {best}");
        assert_eq!(pos.len(), 4);
        assert!(rows >= 2);
        // A previous count narrower than the widest tile is ignored.
        assert_eq!(pack_near(&[(3, 1)], (1600.0, 900.0), Some(2)).0, 3);
    }

    #[test]
    fn spans_and_packable_tabs() {
        let mut tab = Tab::new("t", (0..4).map(|i| Pane::stream(&format!("s{i}"))).collect());
        tab.apply(Preset::Grid, None);
        let p = tab.packable().expect("a grid of streams");
        assert_eq!(p.grid.len(), 4);
        assert!(p.chat.is_none());
        tab.set_span(p.grid[0], 2, 9);
        assert_eq!(tab.span_of(p.grid[0]), (2, MAX_SPAN));
        tab.set_span(p.grid[0], 1, 1);
        assert!(tab.spans.is_empty());
        tab.add_chat_panel(Pane::chat("s0"));
        let p = tab.packable().expect("grid beside a chat column");
        assert_eq!(p.chat.as_ref().map(|c| c.1.len()), Some(1));
        tab.set_span(p.grid[1], 2, 2);
        tab.apply(Preset::Columns, None);
        assert!(tab.spans.is_empty(), "a preset resets spans");
        assert!(tab.packable().is_none(), "columns are not a grid");
    }

    #[test]
    fn several_chat_panels_survive_presets() {
        let mut tab = Tab::new("t", vec![Pane::stream("a"), Pane::stream("b")]);
        tab.apply(Preset::Grid, Some("a".into()));
        tab.add_chat_panel(Pane::chat("b"));
        tab.add_chat_panel(Pane::Chat { channel: "a".into(), mode: ChatMode::AllStreams });
        assert_eq!(tab.chat_panes().len(), 3);
        tab.apply(Preset::Spotlight, None);
        assert_eq!(tab.chat_panes().len(), 3);
        assert_eq!(streams(&tab), vec!["a".to_string(), "b".to_string()]);
        // Asking for a chat that is already shown adds none.
        tab.apply(Preset::Grid, Some("b".into()));
        assert_eq!(tab.chat_panes().len(), 3);
        // A tab without chats gets a column.
        let mut solo = Tab::new("s", vec![Pane::stream("x")]);
        solo.add_chat_panel(Pane::chat("x"));
        assert_eq!(solo.chat_panes().len(), 1);
    }

    #[test]
    fn layout_files_round_trip() {
        let mut tab = Tab::new("Esports", vec![Pane::stream("a"), Pane::stream("b"), Pane::stream("c")]);
        tab.apply(Preset::Spotlight, Some("a".into()));
        let text = export(&tab).unwrap();
        let back = import(&text).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].title, "Esports");
        assert_eq!(streams(&back[0]), streams(&tab));
        // A bare tab and a whole layout.json import too.
        let bare = serde_json::to_string(&tab).unwrap();
        assert_eq!(import(&bare).unwrap().len(), 1);
        let twin = import(&bare).unwrap().remove(0);
        let whole = to_json(&[tab, twin], 0, Latency::Low, None).unwrap();
        assert_eq!(import(&whole).unwrap().len(), 2);
        assert!(import("{\"hello\": 1}").is_err());
        assert!(import("not json").is_err());
    }

    #[test]
    fn presets_keep_every_stream() {
        let mut t = Tab::new("x", vec![Pane::stream("a"), Pane::stream("b"), Pane::stream("c")]);
        for p in Preset::ALL {
            t.apply(p, Some("a".into()));
            assert_eq!(streams(&t), ["a", "b", "c"], "{p:?}");
            assert!(t.has_chat());
        }
        // A preset keeps the chat panels (GK-17); closing one removes it.
        t.apply(Preset::Grid, None);
        assert!(t.has_chat());
        assert_eq!(t.chat_panes().len(), 1);
    }

    #[test]
    fn adding_fills_empty_then_groups() {
        let mut t = Tab::new("x", vec![]);
        t.add_stream("a");
        assert_eq!(streams(&t), ["a"]);
        t.add_stream("b");
        t.add_stream("c");
        assert_eq!(streams(&t).len(), 3);
        // With a chat beside a single stream, the new stream joins the stream.
        let mut t = Tab::new("y", vec![Pane::stream("a")]);
        t.apply(Preset::Grid, Some("a".into()));
        t.add_stream("b");
        assert_eq!(streams(&t), ["a", "b"]);
        assert!(t.has_chat());
    }

    #[test]
    fn chat_modes_resolve_channels() {
        let tab = vec!["a".to_owned(), "b".to_owned(), "a".to_owned()];
        assert_eq!(chat_channels("x", &ChatMode::One, &tab), ["x"]);
        assert_eq!(chat_channels("x", &ChatMode::AllStreams, &tab), ["a", "b"]);
        assert_eq!(chat_channels("x", &ChatMode::Several(vec!["b".into(), "c".into()]), &tab), ["b", "c"]);
        // An empty selection falls back to the pane's channel.
        assert_eq!(chat_channels("x", &ChatMode::Several(vec![]), &tab), ["x"]);
        assert_eq!(chat_channels("x", &ChatMode::AllStreams, &[]), ["x"]);
    }

    #[test]
    fn old_chat_panes_still_load() {
        let old = r#"{"Chat":{"channel":"somebody"}}"#;
        let p: Pane = serde_json::from_str(old).unwrap();
        assert!(matches!(p, Pane::Chat { ref channel, mode: ChatMode::One } if channel == "somebody"));
    }

    #[test]
    fn removing_the_last_tile_leaves_an_empty_pane() {
        let mut t = Tab::new("x", vec![Pane::stream("a")]);
        let id = t.panes_in_order()[0];
        t.remove_tile(id);
        assert!(t.tree.tiles.tiles().any(|t| matches!(t, Tile::Pane(Pane::Empty))));
    }

    #[test]
    fn layout_round_trips_through_json() {
        let mut t = Tab::new("x", vec![Pane::stream("a"), Pane::stream("b")]);
        t.apply(Preset::Spotlight, Some("b".into()));
        let geom = WindowGeom { x: 10, y: 20, w: 1280, h: 720, maximized: true };
        let json = to_json(std::slice::from_ref(&t), 0, Latency::Low, Some(geom)).unwrap();
        let back: Saved = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tabs[0].stream_channels(), ["a", "b"]);
        assert!(back.tabs[0].has_chat());
        assert_eq!(back.latency, Latency::Low);
        assert_eq!(back.window, Some(geom));
        // Files from before the window was saved still load.
        let old = json.replace(",\"window\":{\"x\":10,\"y\":20,\"w\":1280,\"h\":720,\"maximized\":true}", "");
        assert!(!old.contains("window"));
        let back: Saved = serde_json::from_str(&old).unwrap();
        assert_eq!(back.window, None);
    }
}
