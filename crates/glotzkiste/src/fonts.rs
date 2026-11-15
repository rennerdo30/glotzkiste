//! Windows system fonts: Segoe UI for the interface, then CJK, Korean and
//! symbol fallbacks so chat in any language renders. Read on a worker; the
//! UI applies them once when ready.

use std::sync::Arc;

const DIR: &str = "C:\\Windows\\Fonts";

/// (name, file, collection index), in fallback order.
const CANDIDATES: &[(&str, &str, u32)] = &[
    ("segoe", "segoeui.ttf", 0),
    ("segoe-bold", "segoeuib.ttf", 0),
    ("yugothic", "YuGothM.ttc", 0),
    ("msyahei", "msyh.ttc", 0),
    ("malgun", "malgun.ttf", 0),
    ("symbol", "seguisym.ttf", 0),
    ("nirmala", "Nirmala.ttc", 0),
];

pub fn load() -> Option<egui::FontDefinitions> {
    let dir = std::env::var_os("WINDIR").map(|w| std::path::PathBuf::from(w).join("Fonts")).unwrap_or_else(|| DIR.into());
    let mut defs = egui::FontDefinitions::default();
    let mut regular = Vec::new();
    let mut bold = None;
    for (name, file, index) in CANDIDATES {
        let Ok(bytes) = std::fs::read(dir.join(file)) else { continue };
        let mut data = egui::FontData::from_owned(bytes);
        data.index = *index;
        defs.font_data.insert((*name).to_owned(), Arc::new(data));
        if *name == "segoe-bold" {
            bold = Some(*name);
        } else {
            regular.push(*name);
        }
    }
    if regular.is_empty() {
        return None;
    }
    // Ours first, egui's own fonts (with its emoji) after them.
    let prop = defs.families.entry(egui::FontFamily::Proportional).or_default();
    for (i, n) in regular.iter().enumerate() {
        prop.insert(i, (*n).to_owned());
    }
    let fallback: Vec<String> = prop.clone();
    if let Some(b) = bold {
        let mut list = vec![b.to_owned()];
        list.extend(fallback.iter().cloned());
        defs.families.insert(egui::FontFamily::Name("bold".into()), list);
    } else {
        defs.families.insert(egui::FontFamily::Name("bold".into()), fallback.clone());
    }
    let mono = defs.families.entry(egui::FontFamily::Monospace).or_default();
    mono.extend(regular.iter().skip(1).map(|n| (*n).to_owned()));
    Some(defs)
}

/// A bold font id (Segoe UI Bold when present).
pub fn bold(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name("bold".into()))
}
