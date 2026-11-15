//! About: version, links, the update check and the licences.

use super::{ACCENT, BAD, GOOD, MUTED, TEXT};
use crate::app::App;
use egui::{RichText, vec2};

const REPO: &str = "https://github.com/rennerdo30/glotzkiste";
const LICENSE: &str = include_str!("../../../../LICENSE");
const LIBMPV_NOTICE: &str = include_str!("../../../../licenses/libmpv-NOTICE.txt");
static LOGO: &[u8] = include_bytes!("../../../../assets/logo/glotzkiste-256.png");

fn logo(app: &mut App, ctx: &egui::Context) -> Option<egui::TextureHandle> {
    if app.logo.is_none() {
        let img = gk_twitch::image::decode(LOGO).ok()?;
        let f = img.frames.into_iter().next()?;
        let ci = egui::ColorImage::from_rgba_unmultiplied([f.width as usize, f.height as usize], &f.rgba);
        app.logo = Some(ctx.load_texture("glotzkiste-logo", ci, egui::TextureOptions::LINEAR));
    }
    app.logo.clone()
}

pub fn window(app: &mut App, ctx: &egui::Context) {
    let mut open = true;
    let tex = logo(app, ctx);
    egui::Window::new("About Glotzkiste")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
        .default_width(520.0)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.set_width(520.0);
            ui.horizontal(|ui| {
                if let Some(t) = &tex {
                    ui.add(egui::Image::new(t).fit_to_exact_size(vec2(88.0, 88.0)));
                }
                ui.vertical(|ui| {
                    ui.label(RichText::new("Glotzkiste").font(crate::fonts::bold(26.0)).color(TEXT));
                    ui.label(RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).color(MUTED));
                    ui.label(RichText::new("Many live streams and chats at once. Fast, low latency, free and open source.").color(TEXT));
                    ui.horizontal(|ui| {
                        if ui.link("Website and source").clicked() {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(REPO));
                        }
                        if ui.link("Releases").clicked() {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(crate::update::RELEASE_PAGE));
                        }
                        if ui.link("Report a problem").clicked() {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(format!("{REPO}/issues")));
                        }
                    });
                });
            });
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new("Updates").font(crate::fonts::bold(14.0)).color(TEXT));
                if let Some(c) = app.update.found.clone() {
                    let label = if crate::update::installed() { format!("Install {}", c.version) } else { format!("Download {}", c.version) };
                    if ui.add_enabled(!app.update.installing, egui::Button::new(label).fill(ACCENT)).clicked() {
                        app.install_update();
                    }
                } else if ui.add_enabled(!app.update.checking, egui::Button::new("Check now")).clicked() {
                    app.check_updates(true);
                }
                if app.update.checking || app.update.installing {
                    ui.spinner();
                }
            });
            let how = if crate::update::installed() {
                "This copy was installed by the setup: it checks once a day and installs an update when you click it."
            } else {
                "This is a portable copy: it tells you about new versions; download them from the releases page."
            };
            ui.label(RichText::new(how).color(MUTED).size(12.0));
            if let Some((ok, m)) = &app.update.message {
                ui.label(RichText::new(m).color(if *ok { GOOD } else { BAD }));
            }
            ui.separator();
            ui.label(RichText::new("Licences").font(crate::fonts::bold(14.0)).color(TEXT));
            ui.label(RichText::new("Glotzkiste is MIT-licensed. It contains libmpv (LGPL 2.1 or later), which you can replace; details below. The Rust crates it uses are listed in THIRD-PARTY-NOTICES.txt in every release package.").color(MUTED).size(12.0));
            egui::CollapsingHeader::new("Glotzkiste licence (MIT)").show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| ui.label(RichText::new(LICENSE).monospace().size(11.0)));
            });
            egui::CollapsingHeader::new("libmpv (LGPL) and its source").show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| ui.label(RichText::new(LIBMPV_NOTICE).monospace().size(11.0)));
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new("Glotzkiste is an independent project. It is not affiliated with or endorsed by Twitch, 7TV, BetterTTV or FrankerFaceZ; their names refer only to the services it connects to.")
                    .color(MUTED)
                    .size(11.0),
            );
        });
    if !open {
        app.about_open = false;
    }
}
