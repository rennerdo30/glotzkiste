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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Pane {
    Stream(StreamPane),
    Chat { channel: String },
    Empty,
}

impl Pane {
    pub fn stream(channel: &str) -> Self {
        Pane::Stream(StreamPane { channel: channel.to_owned(), quality: Quality::Auto, volume: 80.0, id: 0 })
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Tab {
    pub title: String,
    pub tree: Tree<Pane>,
    /// The stream that has the sound in this tab.
    #[serde(default)]
    pub audio: Option<String>,
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
        Self { title: title.to_owned(), tree: Tree::new_grid(new_id(), panes), audio: None }
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

    #[cfg_attr(not(test), allow(dead_code))]
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

    /// Rebuilds the layout; streams keep their players (same panes),
    /// `chat` adds a chat column for that channel.
    pub fn apply(&mut self, preset: Preset, chat: Option<String>) {
        let streams = self.stream_panes();
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
        let root = match chat {
            Some(c) => {
                let chat = tiles.insert_pane(Pane::Chat { channel: c });
                let lin = Linear::new_binary(LinearDir::Horizontal, [body, chat], 0.78);
                tiles.insert_container(Container::Linear(lin))
            }
            None => body,
        };
        self.tree = Tree::new(new_id(), root, tiles);
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
        if let Some((_, cid)) = best {
            if let Some(Tile::Container(c)) = tiles.get_mut(cid) {
                if c.kind() == ContainerKind::Horizontal && c.num_children() >= 2 {
                    c.set_kind(ContainerKind::Grid);
                }
                c.add_child(new);
                return;
            }
        }
        // A single stream (maybe with a chat): group it with the new one.
        let first_stream = tiles.iter().find_map(|(id, t)| (matches!(t, Tile::Pane(Pane::Stream(_))) && *id != new).then_some(*id));
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
        let chat = self.tree.tiles.insert_pane(Pane::Chat { channel: channel.to_owned() });
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

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Saved {
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub latency: Latency,
}

fn path() -> Option<std::path::PathBuf> {
    gk_twitch::config_dir().map(|d| d.join("layout.json"))
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
pub fn to_json(tabs: &[Tab], active: usize, latency: Latency) -> Option<String> {
    #[derive(serde::Serialize)]
    struct Ref<'a> {
        tabs: &'a [Tab],
        active: usize,
        latency: Latency,
    }
    serde_json::to_string(&Ref { tabs, active, latency }).ok()
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
    fn presets_keep_every_stream() {
        let mut t = Tab::new("x", vec![Pane::stream("a"), Pane::stream("b"), Pane::stream("c")]);
        for p in Preset::ALL {
            t.apply(p, Some("a".into()));
            assert_eq!(streams(&t), ["a", "b", "c"], "{p:?}");
            assert!(t.has_chat());
        }
        t.apply(Preset::Grid, None);
        assert!(!t.has_chat());
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
        let json = to_json(std::slice::from_ref(&t), 0, Latency::Low).unwrap();
        let back: Saved = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tabs[0].stream_channels(), ["a", "b"]);
        assert_eq!(back.latency, Latency::Low);
    }
}
