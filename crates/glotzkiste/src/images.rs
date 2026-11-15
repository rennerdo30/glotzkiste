//! Emote, badge and avatar images by URL. Downloads and decodes run on the
//! worker pool; static images become egui textures, animated ones live on
//! the render thread (their frames change without an egui pass). Bounded by
//! count and by animation bytes, least recently used first out.

use crate::workers::Pool;
use gk_render::{ANIM_BIT, AnimFrames, Presenter};
use gk_twitch::Http;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const MAX_IMAGES: usize = 3000;
const MAX_ANIM_BYTES: usize = 384 * 1024 * 1024;

#[derive(Clone)]
pub enum Img {
    Static(egui::TextureHandle),
    Anim { id: u64, size: egui::Vec2 },
}

impl Img {
    pub fn texture_id(&self) -> egui::TextureId {
        match self {
            Img::Static(t) => t.id(),
            Img::Anim { id, .. } => egui::TextureId::User(*id | ANIM_BIT),
        }
    }
    /// Pixel size of the decoded image.
    pub fn size(&self) -> egui::Vec2 {
        match self {
            Img::Static(t) => t.size_vec2(),
            Img::Anim { size, .. } => *size,
        }
    }
}

enum Slot {
    Loading,
    Ready { img: Img, used: u64, bytes: usize },
    Failed,
}

type Done = Arc<Mutex<Vec<(String, Result<gk_twitch::image::DecodedImage, String>)>>>;

pub struct Images {
    slots: HashMap<String, Slot>,
    done: Done,
    pool: Pool,
    http: Http,
    wake: Arc<dyn Fn() + Send + Sync>,
    next_anim: u64,
    anim_bytes: usize,
    /// The current pass number (for least-recently-used eviction).
    pub frame: u64,
    /// Bumped whenever an image becomes ready (layouts depending on image
    /// sizes are rebuilt).
    pub generation: u64,
}

impl Images {
    pub fn new(pool: Pool, http: Http, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let p2 = pool.clone();
        p2.spawn(gk_twitch::image::trim_cache);
        Self {
            slots: HashMap::new(),
            done: Arc::default(),
            pool,
            http,
            wake,
            next_anim: 1,
            anim_bytes: 0,
            frame: 0,
            generation: 0,
        }
    }

    /// The image if ready; starts loading it otherwise.
    pub fn get(&mut self, url: &str) -> Option<Img> {
        if url.is_empty() {
            return None;
        }
        match self.slots.get_mut(url) {
            Some(Slot::Ready { img, used, .. }) => {
                *used = self.frame;
                Some(img.clone())
            }
            Some(_) => None,
            None => {
                self.slots.insert(url.to_owned(), Slot::Loading);
                let (url, http, done, wake) = (url.to_owned(), self.http.clone(), self.done.clone(), self.wake.clone());
                self.pool.spawn(move || {
                    let r = gk_twitch::image::fetch_decoded(&http, &url).map_err(|e| e.to_string());
                    if let Ok(mut d) = done.lock() {
                        let was_empty = d.is_empty();
                        d.push((url, r));
                        if was_empty {
                            wake();
                        }
                    }
                });
                None
            }
        }
    }

    /// Takes finished downloads; true if any image became ready.
    pub fn poll(&mut self, ctx: &egui::Context, presenter: Option<&Presenter>) -> bool {
        let done = std::mem::take(&mut *self.done.lock().unwrap_or_else(|p| p.into_inner()));
        if done.is_empty() {
            return false;
        }
        for (url, r) in done {
            let slot = match r {
                Ok(img) if img.animated() && presenter.is_some() => {
                    let (w, h) = img.size();
                    let bytes = img.frames.iter().map(|f| f.rgba.len()).sum::<usize>();
                    let id = self.next_anim;
                    self.next_anim += 1;
                    let frames = AnimFrames {
                        width: w,
                        height: h,
                        frames: img.frames.into_iter().map(|f| (f.rgba, f.delay_ms)).collect(),
                    };
                    if let Some(p) = presenter {
                        p.set_animation(id, Arc::new(frames));
                    }
                    self.anim_bytes += bytes;
                    Slot::Ready { img: Img::Anim { id, size: egui::vec2(w as f32, h as f32) }, used: self.frame, bytes }
                }
                Ok(img) => match img.frames.into_iter().next() {
                    Some(f) => {
                        let ci =
                            egui::ColorImage::from_rgba_unmultiplied([f.width as usize, f.height as usize], &f.rgba);
                        let tex = ctx.load_texture(&url, ci, egui::TextureOptions::LINEAR);
                        Slot::Ready { img: Img::Static(tex), used: self.frame, bytes: 0 }
                    }
                    None => Slot::Failed,
                },
                Err(e) => {
                    log::info!("image {url}: {e}");
                    Slot::Failed
                }
            };
            self.slots.insert(url, slot);
        }
        self.generation += 1;
        self.evict(presenter);
        true
    }

    fn evict(&mut self, presenter: Option<&Presenter>) {
        if self.slots.len() <= MAX_IMAGES && self.anim_bytes <= MAX_ANIM_BYTES {
            return;
        }
        let mut ready: Vec<(u64, String)> = self
            .slots
            .iter()
            .filter_map(|(u, s)| match s {
                Slot::Ready { used, .. } if *used + 2 < self.frame => Some((*used, u.clone())),
                _ => None,
            })
            .collect();
        ready.sort();
        for (_, url) in ready {
            if self.slots.len() <= MAX_IMAGES * 9 / 10 && self.anim_bytes <= MAX_ANIM_BYTES * 9 / 10 {
                break;
            }
            if let Some(Slot::Ready { img, bytes, .. }) = self.slots.remove(&url) {
                if let Img::Anim { id, .. } = img
                    && let Some(p) = presenter
                {
                    p.remove_animation(id);
                }
                self.anim_bytes = self.anim_bytes.saturating_sub(bytes);
            }
        }
    }
}
