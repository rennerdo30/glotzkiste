//! Drawing: the top bar (tabs, add channel, layout, latency, account), the
//! tile area and the login dialog. Runs only inside an egui pass.

mod about;
pub mod chat;
pub mod stats;
mod tiles;

use crate::app::App;
use crate::layout::{Preset, Tab};
use crate::stream::Latency;
use egui::{Color32, CornerRadius, FontId, RichText, Stroke, vec2};
use gk_render::VideoTile;

pub const BG: Color32 = Color32::from_rgb(14, 14, 16);
pub const PANEL: Color32 = Color32::from_rgb(24, 24, 27);
pub const RAISED: Color32 = Color32::from_rgb(38, 38, 44);
pub const ACCENT: Color32 = Color32::from_rgb(145, 70, 255);
pub const TEXT: Color32 = Color32::from_rgb(239, 239, 241);
pub const MUTED: Color32 = Color32::from_rgb(173, 173, 184);
pub const GOOD: Color32 = Color32::from_rgb(70, 210, 120);
pub const WARN: Color32 = Color32::from_rgb(240, 190, 60);
pub const BAD: Color32 = Color32::from_rgb(240, 80, 80);
/// The render thread's clear colour (gamma-encoded like egui's colours).
pub const BG_LINEAR: [f32; 4] = [14.0 / 255.0, 14.0 / 255.0, 16.0 / 255.0, 1.0];

pub fn style(ctx: &egui::Context) {
    // Until the system fonts are read, "bold" maps to the default font.
    let mut defs = egui::FontDefinitions::default();
    let prop = defs.families.get(&egui::FontFamily::Proportional).cloned().unwrap_or_default();
    defs.families.insert(egui::FontFamily::Name("bold".into()), prop);
    ctx.set_fonts(defs);

    let mut v = egui::Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.extreme_bg_color = Color32::from_rgb(10, 10, 12);
    v.faint_bg_color = RAISED;
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0, TEXT);
    v.hyperlink_color = Color32::from_rgb(160, 120, 255);
    v.window_corner_radius = CornerRadius::same(8);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.inactive.weak_bg_fill = RAISED;
    v.widgets.inactive.bg_fill = RAISED;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(54, 54, 62);
    v.widgets.hovered.bg_fill = Color32::from_rgb(54, 54, 62);
    v.widgets.active.weak_bg_fill = ACCENT;
    v.widgets.active.bg_fill = ACCENT;
    for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = CornerRadius::same(6);
    }
    ctx.set_visuals(v);
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = vec2(6.0, 6.0);
        s.spacing.button_padding = vec2(8.0, 4.0);
        s.interaction.tooltip_delay = 0.25;
        s.animation_time = 0.08;
    });
}

pub fn draw(app: &mut App, ui: &mut egui::Ui, videos: &mut Vec<VideoTile>, heights: &mut Vec<(u64, u32)>) {
    shortcuts(app, ui);
    egui::Panel::top("bar")
        .exact_size(42.0)
        .frame(egui::Frame::NONE.fill(PANEL).inner_margin(egui::Margin::symmetric(8, 6)))
        .show(ui, |ui| {
            top_bar(app, ui);
        });
    egui::CentralPanel::default().frame(egui::Frame::NONE.fill(BG)).show(ui, |ui| {
        if let Some(e) = app.error.clone() {
            ui.centered_and_justified(|ui| ui.label(RichText::new(e).color(BAD).size(16.0)));
            return;
        }
        tiles::content(app, ui, videos, heights);
    });
    if app.login.open {
        login_dialog(app, ui.ctx());
    }
    if app.about_open {
        about::window(app, ui.ctx());
    }
    if app.random.open {
        random_dialog(app, ui.ctx());
    }
    if app.settings_open {
        settings_window(app, ui.ctx());
    }
    if app.stats_open || app.stats_on_tiles {
        // Numbers refresh twice a second while shown; closed, nothing ticks.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    }
}

fn shortcuts(app: &mut App, ui: &egui::Ui) {
    let typing = ui.ctx().memory(|m| m.focused().is_some());
    ui.input_mut(|i| {
        if i.consume_key(egui::Modifiers::CTRL, egui::Key::T) {
            app.tabs.push(Tab::new(&format!("Tab {}", app.tabs.len() + 1), vec![]));
            app.active = app.tabs.len() - 1;
            app.maximized = None;
        }
        if i.consume_key(egui::Modifiers::CTRL, egui::Key::W) && app.tabs.len() > 1 {
            app.tabs.remove(app.active);
            app.active = app.active.min(app.tabs.len() - 1);
            app.maximized = None;
        }
        if i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab) && !app.tabs.is_empty() {
            app.active = (app.active + 1) % app.tabs.len();
            app.maximized = None;
        }
        let digits = [
            egui::Key::Num1,
            egui::Key::Num2,
            egui::Key::Num3,
            egui::Key::Num4,
            egui::Key::Num5,
            egui::Key::Num6,
            egui::Key::Num7,
            egui::Key::Num8,
            egui::Key::Num9,
        ];
        for (n, k) in digits.into_iter().enumerate() {
            if i.consume_key(egui::Modifiers::CTRL, k) && n < app.tabs.len() {
                app.active = n;
                app.maximized = None;
            }
        }
        if !typing && i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) {
            app.maximized = None;
        }
        if i.consume_key(egui::Modifiers::NONE, egui::Key::F3) {
            app.stats_open = !app.stats_open;
        }
        if i.consume_key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, egui::Key::U) {
            app.install_update();
        }
    });
}

fn pill(ui: &mut egui::Ui, text: &str, fill: Color32, fg: Color32) {
    egui::Frame::NONE.fill(fill).corner_radius(CornerRadius::same(4)).inner_margin(egui::Margin::symmetric(6, 2)).show(
        ui,
        |ui| {
            ui.label(RichText::new(text).color(fg).size(12.0));
        },
    );
}

fn top_bar(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal_centered(|ui| {
        // Tabs.
        let mut close: Option<usize> = None;
        for i in 0..app.tabs.len() {
            if app.renaming == Some(i) {
                let r = ui.add(egui::TextEdit::singleline(&mut app.tabs[i].title).desired_width(110.0));
                if r.lost_focus() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    app.renaming = None;
                } else {
                    r.request_focus();
                }
                continue;
            }
            let selected = i == app.active;
            let n = app.tabs[i].stream_channels().len();
            let label = if n > 0 { format!("{}  {n}", app.tabs[i].title) } else { app.tabs[i].title.clone() };
            let text = RichText::new(label).color(if selected { TEXT } else { MUTED });
            let b = egui::Button::new(text)
                .fill(if selected { RAISED } else { Color32::TRANSPARENT })
                .stroke(if selected { Stroke::new(1.0, ACCENT) } else { Stroke::NONE });
            let r = ui.add(b).on_hover_text("Double-click to rename, middle-click to close");
            if r.clicked() {
                app.active = i;
                app.maximized = None;
            }
            if r.double_clicked() {
                app.renaming = Some(i);
            }
            if r.middle_clicked() && app.tabs.len() > 1 {
                close = Some(i);
            }
            r.context_menu(|ui| {
                if ui.button("Rename").clicked() {
                    app.renaming = Some(i);
                    ui.close();
                }
                if app.tabs.len() > 1 && ui.button("Close tab").clicked() {
                    close = Some(i);
                    ui.close();
                }
            });
        }
        if let Some(i) = close {
            app.tabs.remove(i);
            app.active = app.active.min(app.tabs.len() - 1);
            app.maximized = None;
        }
        if ui
            .add(egui::Button::new(RichText::new("+").size(16.0)).frame(false))
            .on_hover_text("New tab (Ctrl+T)")
            .clicked()
        {
            app.tabs.push(Tab::new(&format!("Tab {}", app.tabs.len() + 1), vec![]));
            app.active = app.tabs.len() - 1;
            app.maximized = None;
        }
        ui.separator();
        add_channel_box(app, ui, 220.0, "bar");
        if ui
            .add(egui::Button::new(RichText::new("🎲").size(15.0)).frame(false))
            .on_hover_text("Add random live streams")
            .clicked()
        {
            app.random.open = !app.random.open;
        }

        // Right side, laid out right to left.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add(egui::Button::new(RichText::new("⋯").size(16.0)).frame(false))
                .on_hover_text("About, updates, licences")
                .clicked()
            {
                app.about_open = !app.about_open;
            }
            if ui.add(egui::Button::new(RichText::new("⚙").size(16.0)).frame(false)).on_hover_text("Settings").clicked()
            {
                app.settings_open = !app.settings_open;
            }
            account_button(app, ui);
            if let Some(c) = app.update.found.clone() {
                let label = if app.update.installing {
                    format!("Updating to {}…", c.version)
                } else {
                    format!("⬆ Update to {}", c.version)
                };
                let hint = if crate::update::installed() {
                    "Downloads, checks and installs it, then restarts (Ctrl+Shift+U)"
                } else {
                    "Opens the download page (this copy was not installed by the setup)"
                };
                if ui
                    .add_enabled(
                        !app.update.installing,
                        egui::Button::new(RichText::new(label).color(Color32::BLACK)).fill(GOOD),
                    )
                    .on_hover_text(hint)
                    .clicked()
                {
                    app.install_update();
                }
            }
            let stats_text = RichText::new("📊 Stats").color(if app.stats_open { TEXT } else { MUTED });
            if ui
                .add(egui::Button::new(stats_text).fill(if app.stats_open { RAISED } else { Color32::TRANSPARENT }))
                .on_hover_text("Stream stats (F3)")
                .clicked()
            {
                app.stats_open = !app.stats_open;
            }
            ui.menu_button(RichText::new(format!("⚡ {}", app.latency.label())), |ui| {
                ui.label(RichText::new("How close to live").color(MUTED));
                for l in [Latency::Ultra, Latency::Low, Latency::Smooth] {
                    let hint = match l {
                        Latency::Ultra => "As close as possible. Best on a stable connection.",
                        Latency::Low => "About a second of cushion.",
                        Latency::Smooth => "Rides out shaky connections.",
                    };
                    if ui.selectable_label(app.latency == l, l.label()).on_hover_text(hint).clicked() {
                        app.set_latency(l);
                        ui.close();
                    }
                }
            })
            .response
            .on_hover_text("Latency mode for all streams");
            layout_menu(app, ui);
        });
    });
}

pub fn add_channel_box(app: &mut App, ui: &mut egui::Ui, width: f32, id: &str) {
    let te = egui::TextEdit::singleline(&mut app.add_text)
        .hint_text("Add channel…")
        .desired_width(width)
        .id_salt(("add", id));
    let r = ui.add(te);
    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        let text = std::mem::take(&mut app.add_text);
        app.add_channel(&text);
    }
    if app.auth.is_some() {
        let mr = ui.menu_button("Following ▾", |ui| {
            app.refresh_followed(false);
            ui.set_min_width(280.0);
            if app.followed.is_empty() {
                ui.label(
                    RichText::new(if app.followed_loading { "Loading…" } else { "No followed channel is live." })
                        .color(MUTED),
                );
            }
            egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                let list = app.followed.clone();
                for c in list {
                    let viewers = if c.viewers >= 1000 {
                        format!("{:.1}k", c.viewers as f64 / 1000.0)
                    } else {
                        c.viewers.to_string()
                    };
                    let r = ui.add(
                        egui::Button::new(egui::text::LayoutJob::simple_singleline(
                            format!("● {}   {}   {viewers}", c.display_name, c.category),
                            FontId::proportional(14.0),
                            TEXT,
                        ))
                        .frame(false),
                    );
                    let r = r.on_hover_text(&c.title);
                    if r.clicked() {
                        app.add_channel(&c.login);
                        ui.close();
                    }
                }
            });
        });
        mr.response.on_hover_text("Live channels you follow");
    }
}

fn layout_menu(app: &mut App, ui: &mut egui::Ui) {
    ui.menu_button("▦ Layout", |ui| {
        ui.set_min_width(250.0);
        let Some(tab) = app.tabs.get_mut(app.active) else { return };
        let streams = tab.stream_channels();
        let chat_channel = tab.audio.clone().or_else(|| streams.first().cloned());
        ui.label(
            RichText::new(format!("Arrange {} stream{}", streams.len(), if streams.len() == 1 { "" } else { "s" }))
                .color(MUTED),
        );
        for p in Preset::ALL {
            ui.horizontal(|ui| {
                if ui.button(format!("{}  {}", p.icon(), p.label())).clicked() {
                    tab.apply(p, None);
                    app.maximized = None;
                    ui.close();
                }
                if let Some(c) = &chat_channel
                    && ui.button("+ chat").on_hover_text(format!("With the chat of {c}")).clicked()
                {
                    tab.apply(p, Some(c.clone()));
                    app.maximized = None;
                    ui.close();
                }
            });
        }
        ui.separator();
        ui.horizontal(|ui| {
            if let Some(c) = &chat_channel
                && ui.button(format!("💬 + Chat of {c}")).on_hover_text("Another chat panel on the right").clicked()
            {
                tab.add_chat_panel(crate::layout::Pane::chat(c));
                app.maximized = None;
                ui.close();
            }
            if ui.button("💬 + Combined chat").on_hover_text("One panel with every stream's chat in this tab").clicked()
            {
                tab.add_chat_panel(crate::layout::Pane::Chat {
                    channel: chat_channel.clone().unwrap_or_default(),
                    mode: crate::layout::ChatMode::AllStreams,
                });
                app.maximized = None;
                ui.close();
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("⬇ Export…").on_hover_text("Save this tab's layout as a file").clicked() {
                app.export_layout();
                ui.close();
            }
            if ui.button("⬆ Import…").on_hover_text("Open a layout file as a new tab").clicked() {
                app.import_layout();
                ui.close();
            }
        });
        if let Some((ok, m)) = &app.layout_message {
            ui.label(RichText::new(m).color(if *ok { GOOD } else { BAD }).size(12.0));
        }
        ui.separator();
        ui.label(
            RichText::new("Drag a tile by its top bar to move it; drag the gaps to resize.").color(MUTED).size(12.0),
        );
    });
}

fn account_button(app: &mut App, ui: &mut egui::Ui) {
    let (text, color) = match &app.auth {
        Some(a) => (format!("● {}", a.login), GOOD),
        None => ("Log in".to_owned(), TEXT),
    };
    let fill = if app.auth.is_some() { RAISED } else { ACCENT };
    if ui
        .add(egui::Button::new(RichText::new(text).color(color)).fill(fill))
        .on_hover_text("Account, Turbo and chat")
        .clicked()
    {
        app.login.open = true;
        app.login.message = None;
    }
    let turbo = app.streams.values().any(|s| s.status().turbo);
    if app.auth.is_some() && turbo {
        pill(ui, "Turbo · no ads", Color32::from_rgb(40, 70, 50), GOOD);
    }
}

fn step(ui: &mut egui::Ui, n: usize, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(22.0, 22.0), egui::Sense::hover());
        ui.painter().circle_filled(r.center(), 11.0, ACCENT);
        ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, n.to_string(), crate::fonts::bold(13.0), TEXT);
        ui.vertical(|ui| {
            ui.label(RichText::new(title).font(crate::fonts::bold(14.0)).color(TEXT));
            add(ui);
        });
    });
    ui.add_space(6.0);
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(title).font(crate::fonts::bold(15.0)).color(TEXT));
    ui.add_space(2.0);
}

fn note(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).color(MUTED).size(12.0));
}

/// Settings (GK-15): changes apply at once (some to new streams or after a
/// restart, as noted) and are saved.
fn settings_window(app: &mut App, ctx: &egui::Context) {
    use crate::settings::{HEIGHTS, Settings};
    let mut open = true;
    let before = app.settings.clone();
    let mut latency = app.latency;
    let mut check_now = false;
    egui::Window::new("Settings").collapsible(false).resizable(false).default_width(460.0).open(&mut open).show(
        ctx,
        |ui| {
            ui.set_width(460.0);
            let s = &mut app.settings;
            section(ui, "Playback");
            egui::Grid::new("settings-playback").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Latency");
                ui.horizontal(|ui| {
                    for l in [Latency::Ultra, Latency::Low, Latency::Smooth] {
                        ui.selectable_value(&mut latency, l, l.label());
                    }
                });
                ui.end_row();
                ui.label("Highest auto quality");
                let current = HEIGHTS.iter().find(|(h, _)| *h == s.max_auto_height).map_or("Source", |h| h.1);
                egui::ComboBox::from_id_salt("settings-height").selected_text(current).width(120.0).show_ui(ui, |ui| {
                    for (h, label) in HEIGHTS {
                        ui.selectable_value(&mut s.max_auto_height, h, label);
                    }
                });
                ui.end_row();
            });
            note(ui, "Auto quality follows each tile's size; this caps it (saves bandwidth and decoder load).");

            section(ui, "GPUs");
            ui.checkbox(&mut s.other_gpus, "Decode on my other GPUs first (integrated ones too)");
            note(ui, "Off: new streams decode on the GPU that draws the window, then the CPU.");
            ui.checkbox(&mut s.gpu_copy, "Copy frames GPU to GPU");
            note(ui, "Off: frames from other GPUs are read back by the CPU. For GPUs used from now on.");
            ui.horizontal(|ui| {
                ui.label("Render threads");
                let label = if s.render_threads == 0 { "Automatic".to_owned() } else { s.render_threads.to_string() };
                egui::ComboBox::from_id_salt("settings-threads").selected_text(label).width(110.0).show_ui(ui, |ui| {
                    ui.selectable_value(&mut s.render_threads, 0, "Automatic");
                    for n in 1..=8u32 {
                        ui.selectable_value(&mut s.render_threads, n, n.to_string());
                    }
                });
                note(ui, "after a restart");
            });

            section(ui, "Updates");
            ui.checkbox(&mut s.auto_update, "Look for updates once a day");
            ui.horizontal(|ui| {
                ui.label("Channel");
                let label = match s.update_channel.as_str() {
                    "stable" => "Stable",
                    "beta" => "Beta",
                    _ => "This version's",
                };
                egui::ComboBox::from_id_salt("settings-channel").selected_text(label).width(130.0).show_ui(ui, |ui| {
                    ui.selectable_value(&mut s.update_channel, String::new(), "This version's");
                    ui.selectable_value(&mut s.update_channel, "stable".to_owned(), "Stable");
                    ui.selectable_value(&mut s.update_channel, "beta".to_owned(), "Beta");
                });
                if ui.button("Check now").clicked() {
                    check_now = true;
                }
            });

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                note(ui, "Saved automatically.");
                if ui.small_button("Reset to defaults").clicked() {
                    *s = Settings::default();
                    latency = Latency::Ultra;
                }
            });
        },
    );
    if latency != app.latency {
        app.set_latency(latency);
    }
    if app.settings != before {
        app.settings_changed();
    }
    if check_now {
        app.check_updates(true);
        app.about_open = true;
    }
    if !open {
        app.settings_open = false;
    }
}

/// Broadcaster languages for the random-streams dialog: label and Twitch
/// language code (`None`: any).
pub const LANGUAGES: [(&str, Option<&str>); 14] = [
    ("Any language", None),
    ("English", Some("EN")),
    ("German", Some("DE")),
    ("Spanish", Some("ES")),
    ("French", Some("FR")),
    ("Portuguese", Some("PT")),
    ("Italian", Some("IT")),
    ("Polish", Some("PL")),
    ("Russian", Some("RU")),
    ("Turkish", Some("TR")),
    ("Japanese", Some("JA")),
    ("Korean", Some("KO")),
    ("Chinese", Some("ZH")),
    ("Arabic", Some("AR")),
];

/// "Add random streams": how many, from which category, in which language.
fn random_dialog(app: &mut App, ctx: &egui::Context) {
    app.fetch_categories();
    let mut open = true;
    egui::Window::new("Add random streams")
        .collapsible(false)
        .resizable(false)
        .default_width(380.0)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.set_width(380.0);
            ui.label(RichText::new("Fills the current tab with random live channels.").color(MUTED));
            ui.add_space(6.0);
            egui::Grid::new("random-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("How many");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut app.random.count).range(1..=100).speed(0.3));
                    for n in [5u32, 10, 25, 50] {
                        if ui.small_button(n.to_string()).clicked() {
                            app.random.count = n;
                        }
                    }
                });
                ui.end_row();
                ui.label("Category");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut app.random.category)
                            .hint_text("Any category")
                            .desired_width(200.0),
                    );
                    let categories = app.random.categories.clone();
                    ui.menu_button("▾", |ui| {
                        ui.set_min_width(220.0);
                        if ui.button("Any category").clicked() {
                            app.random.category.clear();
                            ui.close();
                        }
                        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                            if categories.is_empty() {
                                ui.label(RichText::new("Loading…").color(MUTED));
                            }
                            for c in categories {
                                if ui.button(&c).clicked() {
                                    app.random.category = c;
                                    ui.close();
                                }
                            }
                        });
                    })
                    .response
                    .on_hover_text("Most watched categories right now");
                });
                ui.end_row();
                ui.label("Language");
                let current = LANGUAGES.get(app.random.language).map_or("Any language", |l| l.0);
                egui::ComboBox::from_id_salt("random-language").selected_text(current).width(200.0).show_ui(ui, |ui| {
                    for (i, (label, _)) in LANGUAGES.iter().enumerate() {
                        ui.selectable_value(&mut app.random.language, i, *label);
                    }
                });
                ui.end_row();
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let label = if app.random.busy { "Looking…" } else { "🎲 Add streams" };
                if ui.add_enabled(!app.random.busy, egui::Button::new(label)).clicked() {
                    app.add_random();
                }
                if let Some((ok, m)) = &app.random.message {
                    ui.label(RichText::new(m).color(if *ok { GOOD } else { BAD }));
                }
            });
            if app.random.count > 30 && !app.random.category.trim().is_empty() {
                ui.label(
                    RichText::new("Twitch lists the top 30 live streams of a category to apps; more come from other categories only.")
                        .color(MUTED)
                        .size(12.0),
                );
            }
            if app.random.count > 30 {
                ui.label(
                    RichText::new("Many streams load your GPUs' video decoders and your connection.")
                        .color(MUTED)
                        .size(12.0),
                );
            }
        });
    if !open {
        app.random.open = false;
    }
}

fn login_dialog(app: &mut App, ctx: &egui::Context) {
    use crate::app::LoginMode;
    app.detect_browsers();
    let mut open = true;
    egui::Window::new("Log in with Twitch")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
        .default_width(560.0)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.set_width(560.0);
            if let Some(a) = app.auth.clone() {
                logged_in_view(app, ui, &a);
                return;
            }
            ui.horizontal_wrapped(|ui| {
                for b in ["✔ No ads with Turbo", "✔ Chat as yourself", "✔ Your live channels, one click away"] {
                    ui.label(RichText::new(b).color(GOOD));
                    ui.add_space(8.0);
                }
            });
            ui.label(RichText::new("Glotzkiste borrows the login your browser already has. Your password never touches it.").color(MUTED));
            ui.add_space(8.0);

            // Which browser: installed ones only, default first.
            let browsers = app.login.browsers.clone().unwrap_or_default();
            if !browsers.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Your browser:").color(MUTED));
                    for (i, b) in browsers.iter().enumerate() {
                        let label = if b.default { format!("{} ★", b.kind.name()) } else { b.kind.name().to_owned() };
                        let sel = app.login.browser == i;
                        let r = ui.add(egui::Button::new(RichText::new(label).color(if sel { TEXT } else { MUTED })).fill(if sel { RAISED } else { Color32::TRANSPARENT }).stroke(if sel { Stroke::new(1.0, ACCENT) } else { Stroke::NONE }));
                        let r = if b.default { r.on_hover_text("Your default browser") } else { r };
                        if r.clicked() {
                            app.login.browser = i;
                        }
                    }
                });
            }
            let browser = browsers.get(app.login.browser).cloned();
            let kind = browser.as_ref().map(|b| b.kind).unwrap_or(crate::browsers::Kind::Chrome);
            let bname = browser.as_ref().map(|b| b.kind.name()).unwrap_or("your browser");

            // Two ways in.
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut app.login.mode, LoginMode::Extension, "With an extension (easiest)");
                ui.selectable_value(&mut app.login.mode, LoginMode::DevTools, "Without installing anything");
            });
            ui.separator();
            match app.login.mode {
                LoginMode::Extension => {
                    step(ui, 1, "Install \"Get cookies.txt LOCALLY\"", |ui| {
                        ui.label(RichText::new("Open source (MIT), exports only when you click, never sends anything anywhere.").color(MUTED).size(12.0));
                        ui.horizontal(|ui| {
                            if ui.add(egui::Button::new(format!("Open the store in {bname}")).fill(ACCENT)).clicked() {
                                crate::browsers::open(browser.as_ref(), kind.store_url(), ui.ctx());
                            }
                            if ui.link("Source code").clicked() {
                                crate::browsers::open(browser.as_ref(), crate::browsers::SOURCE, ui.ctx());
                            }
                        });
                        if let Some(note) = kind.store_note() {
                            ui.label(RichText::new(note).color(WARN).size(12.0));
                        }
                    });
                    step(ui, 2, "Open twitch.tv, logged in", |ui| {
                        if ui.button(format!("Open twitch.tv in {bname}")).clicked() {
                            crate::browsers::open(browser.as_ref(), "https://www.twitch.tv/", ui.ctx());
                        }
                    });
                    step(ui, 3, "Copy your cookies", |ui| {
                        ui.label("Click the extensions button (the puzzle piece) next to the address bar, then Get cookies.txt LOCALLY.");
                        ui.label("Keep the format on Netscape and press Copy.");
                        ui.label(RichText::new("(Or Export, then drop the file on this window.)").color(MUTED).size(12.0));
                    });
                    step(ui, 4, "Paste them here", |_| {});
                }
                LoginMode::DevTools => {
                    step(ui, 1, "Open twitch.tv, logged in", |ui| {
                        if ui.button(format!("Open twitch.tv in {bname}")).clicked() {
                            crate::browsers::open(browser.as_ref(), "https://www.twitch.tv/", ui.ctx());
                        }
                    });
                    step(ui, 2, "Press F12 to open the developer tools", |ui| {
                        ui.label(format!("Go to {}.", kind.devtools_path()));
                    });
                    step(ui, 3, "Copy the auth-token value", |ui| {
                        ui.label("Find the row auth-token, double-click its Value and press Ctrl+C.");
                        ui.label(RichText::new("It is about 30 letters and digits. Treat it like a password.").color(MUTED).size(12.0));
                    });
                    step(ui, 4, "Paste it here", |_| {});
                }
            }
            ui.horizontal(|ui| {
                let te = egui::TextEdit::singleline(&mut app.login.paste).hint_text("Paste here (hidden)").password(true).desired_width(330.0);
                let r = ui.add(te);
                let go = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let can = !app.login.busy && !app.login.paste.trim().is_empty();
                if (ui.add_enabled(can, egui::Button::new("Log in").fill(ACCENT)).clicked() || (go && can)) && !app.login.busy {
                    let text = std::mem::take(&mut app.login.paste);
                    app.start_login(text);
                }
                if ui.add_enabled(!app.login.busy, egui::Button::new("Choose file…")).on_hover_text("An exported cookies.txt").clicked() {
                    app.pick_cookie_file();
                }
                if app.login.busy {
                    ui.spinner();
                }
            });
            ui.add_space(6.0);
            ui.label(
                RichText::new("Only the twitch.tv login is used. It is kept in Windows Credential Manager (encrypted for your Windows user, on this PC only) and only ever sent to Twitch.")
                    .color(MUTED)
                    .size(12.0),
            );
            if let Some((ok, m)) = &app.login.message {
                ui.add_space(4.0);
                ui.label(RichText::new(m).color(if *ok { GOOD } else { BAD }));
            }
        });
    // Files dropped on the window go to the login dialog while it is open.
    let dropped = ctx.input(|i| i.raw.dropped_files.clone());
    if let Some(path) = dropped.iter().map(|f| f.path().to_path_buf()).find(|p| !p.as_os_str().is_empty()) {
        app.login_from_file(Some(path));
    }
    if !open {
        app.login.open = false;
    }
}

fn logged_in_view(app: &mut App, ui: &mut egui::Ui, a: &gk_twitch::Auth) {
    ui.label(RichText::new(format!("Logged in as {}", a.login)).size(18.0).color(TEXT));
    let turbo = app.streams.values().any(|s| s.status().turbo);
    let ads = app.streams.values().any(|s| s.status().ads_expected);
    ui.label(if turbo {
        RichText::new("✔ Turbo is active: streams play without ads.").color(GOOD)
    } else if app.streams.is_empty() {
        RichText::new("Open a stream to see whether this account plays without ads.").color(MUTED)
    } else if ads {
        RichText::new(
            "This account has no Turbo, so Twitch sends ads. Glotzkiste skips them; the picture holds during a break.",
        )
        .color(WARN)
    } else {
        RichText::new("✔ No ads on the open streams.").color(GOOD)
    });
    let identity = a
        .user_agent
        .as_deref()
        .and_then(crate::app::ua_label)
        .map(|l| format!("✔ Requests look like your browser ({l})"))
        .unwrap_or_else(|| "Requests use a standard browser identity".into());
    ui.label(RichText::new(identity).color(if a.user_agent.is_some() { GOOD } else { MUTED }));
    if a.device_id.is_some() {
        ui.label(RichText::new("✔ Same device as your browser session").color(GOOD));
    }
    ui.label(RichText::new("Stored in Windows Credential Manager as Glotzkiste/twitch.").color(MUTED).size(12.0));
    if let Some(p) = app.login.imported_file.clone() {
        ui.add_space(6.0);
        egui::Frame::NONE.fill(RAISED).corner_radius(6).inner_margin(8).show(ui, |ui| {
            ui.label(RichText::new("The exported cookies file still holds your whole browser session:").color(WARN));
            ui.label(RichText::new(p.display().to_string()).color(MUTED).size(12.0));
            if ui.add(egui::Button::new("Delete that file now").fill(ACCENT)).clicked() {
                app.delete_imported_file();
            }
        });
    }
    ui.add_space(8.0);
    if ui.button("Log out").clicked() {
        app.logout();
    }
    if let Some((ok, m)) = &app.login.message {
        ui.add_space(4.0);
        ui.label(RichText::new(m).color(if *ok { GOOD } else { BAD }));
    }
}
