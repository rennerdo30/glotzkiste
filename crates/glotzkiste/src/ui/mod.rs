//! Drawing: the top bar (tabs, add channel, layout, latency, account), the
//! tile area and the login dialog. Runs only inside an egui pass.

pub mod chat;
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
    egui::Panel::top("bar").exact_size(42.0).frame(egui::Frame::NONE.fill(PANEL).inner_margin(egui::Margin::symmetric(8, 6))).show(ui, |ui| {
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
        let digits = [egui::Key::Num1, egui::Key::Num2, egui::Key::Num3, egui::Key::Num4, egui::Key::Num5, egui::Key::Num6, egui::Key::Num7, egui::Key::Num8, egui::Key::Num9];
        for (n, k) in digits.into_iter().enumerate() {
            if i.consume_key(egui::Modifiers::CTRL, k) && n < app.tabs.len() {
                app.active = n;
                app.maximized = None;
            }
        }
        if !typing && i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) {
            app.maximized = None;
        }
    });
}

fn pill(ui: &mut egui::Ui, text: &str, fill: Color32, fg: Color32) {
    egui::Frame::NONE.fill(fill).corner_radius(CornerRadius::same(4)).inner_margin(egui::Margin::symmetric(6, 2)).show(ui, |ui| {
        ui.label(RichText::new(text).color(fg).size(12.0));
    });
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
            let b = egui::Button::new(text).fill(if selected { RAISED } else { Color32::TRANSPARENT }).stroke(if selected { Stroke::new(1.0, ACCENT) } else { Stroke::NONE });
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
        if ui.add(egui::Button::new(RichText::new("+").size(16.0)).frame(false)).on_hover_text("New tab (Ctrl+T)").clicked() {
            app.tabs.push(Tab::new(&format!("Tab {}", app.tabs.len() + 1), vec![]));
            app.active = app.tabs.len() - 1;
            app.maximized = None;
        }
        ui.separator();
        add_channel_box(app, ui, 220.0, "bar");

        // Right side, laid out right to left.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            account_button(app, ui);
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
    let te = egui::TextEdit::singleline(&mut app.add_text).hint_text("Add channel…").desired_width(width).id_salt(("add", id));
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
                ui.label(RichText::new(if app.followed_loading { "Loading…" } else { "No followed channel is live." }).color(MUTED));
            }
            egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                let list = app.followed.clone();
                for c in list {
                    let viewers = if c.viewers >= 1000 { format!("{:.1}k", c.viewers as f64 / 1000.0) } else { c.viewers.to_string() };
                    let r = ui.add(
                        egui::Button::new(
                            egui::text::LayoutJob::simple_singleline(format!("● {}   {}   {viewers}", c.display_name, c.category), FontId::proportional(14.0), TEXT),
                        )
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
        ui.label(RichText::new(format!("Arrange {} stream{}", streams.len(), if streams.len() == 1 { "" } else { "s" })).color(MUTED));
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
        ui.label(RichText::new("Drag a tile by its top bar to move it; drag the gaps to resize.").color(MUTED).size(12.0));
    });
}

fn account_button(app: &mut App, ui: &mut egui::Ui) {
    let (text, color) = match &app.auth {
        Some(a) => (format!("● {}", a.login), GOOD),
        None => ("Log in".to_owned(), TEXT),
    };
    let fill = if app.auth.is_some() { RAISED } else { ACCENT };
    if ui.add(egui::Button::new(RichText::new(text).color(color)).fill(fill)).on_hover_text("Account, Turbo and chat").clicked() {
        app.login.open = true;
        app.login.message = None;
    }
    let turbo = app.streams.values().any(|s| s.status().turbo);
    if app.auth.is_some() && turbo {
        pill(ui, "Turbo · no ads", Color32::from_rgb(40, 70, 50), GOOD);
    }
}

fn login_dialog(app: &mut App, ctx: &egui::Context) {
    let mut open = true;
    egui::Window::new("Your Twitch account")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
        .default_width(460.0)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.set_max_width(460.0);
            if let Some(a) = app.auth.clone() {
                ui.label(RichText::new(format!("Logged in as {}", a.login)).size(18.0).color(TEXT));
                let turbo = app.streams.values().any(|s| s.status().turbo);
                let ads = app.streams.values().any(|s| s.status().ads_expected);
                ui.label(if turbo {
                    RichText::new("Turbo is active: streams play without ads.").color(GOOD)
                } else if app.streams.is_empty() {
                    RichText::new("Open a stream to see whether this account plays without ads.").color(MUTED)
                } else if ads {
                    RichText::new("This account has no Turbo: Twitch sends ads, and Glotzkiste skips them (the picture holds during a break).").color(WARN)
                } else {
                    RichText::new("No ads on the open streams.").color(GOOD)
                });
                ui.add_space(8.0);
                if ui.button("Log out").clicked() {
                    app.logout();
                }
            } else {
                ui.label(RichText::new("Log in with your browser session").size(18.0).color(TEXT));
                ui.label(RichText::new(
                    "With Turbo, streams play without ads; logged in, you can also chat. Glotzkiste uses the login your browser already has:",
                ).color(MUTED));
                ui.add_space(4.0);
                ui.label("1. Open twitch.tv in your browser, logged in.");
                ui.label("2. Export its cookies as cookies.txt (any \"cookies.txt\" browser extension), or copy the value of the auth-token cookie.");
                ui.label("3. Choose the file below, or paste the file's text or the token.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(!app.login.busy, egui::Button::new("Choose cookies.txt…").fill(ACCENT)).clicked() {
                        app.pick_cookie_file();
                    }
                    if app.login.busy {
                        ui.spinner();
                    }
                });
                ui.add_space(4.0);
                ui.add(egui::TextEdit::multiline(&mut app.login.paste).hint_text("…or paste here").password(true).desired_rows(2).desired_width(f32::INFINITY));
                if ui.add_enabled(!app.login.busy && !app.login.paste.trim().is_empty(), egui::Button::new("Log in")).clicked() {
                    let text = app.login.paste.clone();
                    app.start_login(text);
                }
                ui.add_space(6.0);
                ui.label(RichText::new("The login stays on this PC, encrypted for your Windows user, and is only sent to Twitch.").color(MUTED).size(12.0));
            }
            if let Some((ok, m)) = &app.login.message {
                ui.add_space(6.0);
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
