//! The Live page (§15.4): what's on air, big, in the middle; what's up next beside it with one
//! obvious button to switch; your scenes; every camera and source; quick effects and what's
//! running; chat and activity on the right; sound along the bottom.

use crate::app::{App, Page, ViewId};
use crate::frames::Canvas;
use crate::views::{mix, monitor, rail, show, show::RailTab};
use egui::{Align, Align2, Color32, CornerRadius, Layout, Rect, RichText, Sense, Stroke, StrokeKind, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::theme::{font_bold, font_medium, font_mono, font_semibold, mix, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, LedState, Size, icon};

/// Which program canvases the On-air monitor shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OnAirView {
    #[default]
    Main,
    Vertical,
    Both,
}

/// Sidebar badge for a page (count of things waiting for the streamer).
pub fn nav_badge(app: &App, p: Page) -> Option<(String, Color32)> {
    let t = &app.t;
    let n = match p {
        Page::Community => rail::badge_count(app, RailTab::Mod) + rail::badge_count(app, RailTab::Queue),
        _ => 0,
    };
    (n > 0).then(|| (n.to_string(), t.accent))
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    egui::Panel::right("rail")
        .exact_size(380.0)
        .resizable(false)
        .frame(egui::Frame::new().fill(t.bg).inner_margin(egui::Margin { left: 18, right: 20, top: 16, bottom: 12 }).stroke(Stroke::new(1.0, t.border)))
        .show(ui, |ui| rail_panel(app, ui));
    egui::Panel::bottom("sound")
        .exact_size(112.0)
        .resizable(false)
        .frame(egui::Frame::new().fill(t.chrome).inner_margin(egui::Margin { left: 24, right: 24, top: 12, bottom: 12 }))
        .show(ui, |ui| sound_bar(app, ui));
    egui::CentralPanel::default().frame(egui::Frame::new().fill(t.bg).inner_margin(egui::Margin { left: 28, right: 24, top: 20, bottom: 16 })).show(ui, |ui| {
        let body_h = ui.available_height();
        egui::ScrollArea::vertical().id_salt("live").auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(ui.available_width());
            let top = ui.cursor().top();
            setup_banner(app, ui);
            // height left for the page once the banner (if any) is drawn
            let geo = Geometry::new(ui.available_width(), body_h - (ui.cursor().top() - top));
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = geo.gap;
                ui.allocate_ui_with_layout(Vec2::new(geo.air_w, 0.0), Layout::top_down(Align::Min), |ui| on_air(app, ui, &geo));
                ui.allocate_ui_with_layout(Vec2::new(geo.next_w, 0.0), Layout::top_down(Align::Min), |ui| up_next(app, ui, geo.next_w));
                if geo.fx_w > 0.0 {
                    ui.allocate_ui_with_layout(Vec2::new(geo.fx_w, 0.0), Layout::top_down(Align::Min), |ui| effects_column(app, ui));
                }
            });
            ui.add_space(spacing::L);
            scenes(app, ui);
            ui.add_space(spacing::L);
            crate::views::auto_sequence::overview_strip(app, ui);
            ui.add_space(spacing::L);
            crate::views::lights::overview_strip(app, ui);
            if geo.fx_w <= 0.0 {
                // narrower windows: the show controls come before the camera strip
                ui.add_space(spacing::L);
                let w = ui.available_width();
                let right = (w * 0.34).clamp(280.0, 420.0);
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = spacing::L;
                    ui.allocate_ui_with_layout(Vec2::new(w - right - spacing::L, 0.0), Layout::top_down(Align::Min), |ui| {
                        widgets::titled(ui, &t, "Buttons", &show::pads_source(app), |_| {}, |ui| show::pads(app, ui));
                    });
                    ui.allocate_ui_with_layout(Vec2::new(right, 0.0), Layout::top_down(Align::Min), |ui| {
                        widgets::titled(ui, &t, "Running now", "", |_| {}, |ui| show::active(app, ui));
                    });
                });
            }
            ui.add_space(spacing::L);
            sources(app, ui);
        });
    });
}

/// Column widths for the top row. The on-air monitor never takes more than about half the
/// page height, so scenes stay in view; on wide screens the quick effects move up beside it.
struct Geometry {
    gap: f32,
    air_w: f32,
    next_w: f32,
    /// Width of the effects column in the top row (0 = effects go below).
    fx_w: f32,
}

impl Geometry {
    fn new(w: f32, body_h: f32) -> Geometry {
        let gap = spacing::L;
        let cap_w = (body_h * 0.44).clamp(240.0, 900.0) * 16.0 / 9.0;
        // Up next is always clearly smaller than On air
        let next_w = (w * 0.3).clamp(300.0, 520.0).min((cap_w * 0.62).max(300.0));
        let rest = w - cap_w - next_w - 2.0 * gap;
        if rest >= 440.0 {
            return Geometry { gap, air_w: cap_w, next_w, fx_w: rest };
        }
        // Narrower windows: the quick effects go under the scenes (the page scrolls; the
        // Emergency stop and Switch stay in view).
        let air_w = (w - next_w - gap).min(cap_w);
        let next_w = (w - air_w - gap).min(640.0).min((air_w * 0.62).max(300.0));
        Geometry { gap, air_w, next_w, fx_w: 0.0 }
    }
}

fn effects_column(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    widgets::titled(ui, &t, "Buttons", &show::pads_source(app), |_| {}, |ui| show::pads(app, ui));
    ui.add_space(spacing::L);
    widgets::titled(ui, &t, "Running now", "", |_| {}, |ui| show::active(app, ui));
}

fn setup_banner(app: &mut App, ui: &mut egui::Ui) {
    let left = crate::views::setup::steps_left(app);
    if left == 0 {
        return;
    }
    let t = app.t.clone();
    let body = format!("{left} step{} left before your first stream.", if left == 1 { "" } else { "s" });
    if widgets::callout(ui, &t, widgets::Tone::Info, icon::ROCKET, "Finish setting up", &body, Some("Continue setup")) {
        app.open_view(ViewId::Setup);
    }
    ui.add_space(spacing::L);
}

// ---- on air / up next --------------------------------------------------------------------------

fn on_air(app: &mut App, ui: &mut egui::Ui, geo: &Geometry) {
    let t = app.t.clone();
    let program = app.m.str("show.scene.program").to_string();
    let air_w = geo.air_w;
    let view = app.show.on_air;
    let live = crate::views::status::on_air(app);
    let (badge, vertical_badge, tally) =
        if live { ("ON AIR", "ON AIR · VERTICAL", LedState::Active) } else { ("PROGRAM", "PROGRAM · VERTICAL", LedState::Idle) };
    ui.horizontal(|ui| {
        ui.label(RichText::new(if live { "On air" } else { "Program" }).font(font_semibold(type_scale::LARGE)).color(t.fg));
        ui.label(RichText::new(if live { "What your viewers see" } else { "Current output · off air" }).color(t.text_dim));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let mut i = match view {
                OnAirView::Main => 0,
                OnAirView::Vertical => 1,
                OnAirView::Both => 2,
            };
            if widgets::segmented(ui, &t, &mut i, &["Main", "Vertical", "Both"]) {
                app.show.on_air = [OnAirView::Main, OnAirView::Vertical, OnAirView::Both][i];
            }
        });
    });
    ui.add_space(spacing::S);
    let hz = app.program_hz();
    let h = air_w * 9.0 / 16.0;
    match app.show.on_air {
        OnAirView::Main => {
            monitor::monitor(app, ui, Canvas::Wide, &program, badge, Vec2::new(air_w, h), tally, hz);
        }
        OnAirView::Vertical => {
            ui.horizontal(|ui| {
                ui.add_space((air_w - h * 9.0 / 16.0) / 2.0);
                monitor::monitor(app, ui, Canvas::Tall, &program, vertical_badge, Vec2::new(h * 9.0 / 16.0, h), tally, hz);
            });
        }
        OnAirView::Both => {
            // wide + tall of the same height, together no wider than the column
            let bh = ((air_w - geo.gap) / (16.0 / 9.0 + 9.0 / 16.0)).min(h);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = geo.gap;
                monitor::monitor(app, ui, Canvas::Wide, &program, badge, Vec2::new(bh * 16.0 / 9.0, bh), tally, hz);
                monitor::monitor(app, ui, Canvas::Tall, &program, "VERTICAL", Vec2::new(bh * 9.0 / 16.0, bh), tally, hz);
            });
        }
    }
}

fn up_next(app: &mut App, ui: &mut egui::Ui, next_w: f32) {
    let t = app.t.clone();
    let program = app.m.str("show.scene.program").to_string();
    let preview = app.m.str("show.scene.preview").to_string();
    ui.horizontal(|ui| {
        ui.label(RichText::new("Up next").font(font_semibold(type_scale::LARGE)).color(t.fg));
        ui.label(RichText::new("Pick a scene below").color(t.text_dim));
    });
    ui.add_space(spacing::S);
    let h = next_w * 9.0 / 16.0;
    let pv = app.preview_hz();
    let same = preview.is_empty() || preview == program;
    if same {
        // nothing picked: an empty frame that says what to do
        let (r, _) = ui.allocate_exact_size(Vec2::new(next_w, h), Sense::hover());
        let p = ui.painter();
        p.rect(r, CornerRadius::same(radius::TILE), t.inset, Stroke::new(1.0, t.border), StrokeKind::Inside);
        p.text(r.center() - Vec2::new(0.0, 12.0), Align2::CENTER_CENTER, icon::LAYERS, se_ui_kit::theme::font(22.0), t.text_faint);
        p.text(r.center() + Vec2::new(0.0, 16.0), Align2::CENTER_CENTER, "Nothing picked yet: click a scene below", font_medium(type_scale::BODY), t.text_dim);
    } else {
        monitor::monitor(app, ui, Canvas::Preview, &preview, "UP NEXT", Vec2::new(next_w, h), LedState::Armed, pv);
    }
    ui.add_space(spacing::M);
    transition_picker(app, ui, next_w);
    ui.add_space(spacing::S);
    let label = if same { "Choose a scene below".to_string() } else { format!("Switch to {}", nice(&preview)) };
    let busy = app.m.b("show.transition.active");
    let r = widgets::button_ex(ui, &t, Some(icon::PLAY), &label, Kind::Primary, Size::Large, next_w, !same && !busy);
    if busy {
        let p = app.m.f("show.transition.progress") as f32;
        let bar = Rect::from_min_size(r.rect.left_bottom() - Vec2::new(0.0, 4.0), Vec2::new(r.rect.width() * p, 4.0));
        ui.painter().rect_filled(bar, CornerRadius::same(2), t.fg);
    }
    if r.on_hover_text(format!("Put \"Up next\" on air ({})", app.keys_label("take"))).clicked() {
        app.take();
    }
}

/// "Random: morph, fade…" or a fixed transition, plus how long it takes.
fn transition_picker(app: &mut App, ui: &mut egui::Ui, width: f32) {
    let t = app.t.clone();
    let preview = app.m.str("show.scene.preview").to_string();
    let pool = pool_names(app, &preview);
    let current = match &app.show.transition {
        Some(n) => nice(n),
        None if pool.is_empty() => "Default".into(),
        None => format!("Random ({})", pool.iter().map(|n| nice(n)).collect::<Vec<_>>().join(", ")),
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon::SHUFFLE).color(t.text_dim));
        ui.label(RichText::new("Transition").color(t.text_dim));
        ui.spacing_mut().combo_width = (width - 110.0).max(120.0);
        egui::ComboBox::from_id_salt("transition").selected_text(current).show_ui(ui, |ui| {
            if ui.selectable_label(app.show.transition.is_none(), "Random (from the scene)").clicked() {
                app.show.transition = None;
            }
            for n in app.m.q_list("transitions").iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>() {
                if ui.selectable_label(app.show.transition.as_deref() == Some(&n), nice(&n)).clicked() {
                    app.show.transition = Some(n);
                }
            }
        });
    });
    ui.add_space(spacing::XS);
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon::CLOCK).color(t.text_dim));
        ui.label(RichText::new("Speed").color(t.text_dim));
        ui.add_space(spacing::S);
        let speeds = [None, Some(300u32), Some(700), Some(1500)];
        let mut cur = speeds.iter().position(|ms| *ms == app.show.take_ms).unwrap_or(0);
        if widgets::segmented(ui, &t, &mut cur, &["Auto", "Fast", "Normal", "Slow"]) {
            app.show.take_ms = speeds[cur];
        }
    });
}

fn pool_names(app: &App, scene: &str) -> Vec<String> {
    app.build
        .scene_config(scene)
        .and_then(|s| s.get_path("transitions.pool"))
        .and_then(Value::as_list)
        .map(|l| l.iter().filter_map(|e| e.get_path("name").and_then(Value::as_str).or(e.as_str()).map(String::from)).collect())
        .unwrap_or_default()
}

/// `zoom_blur` → `Zoom blur`.
/// Display name from an id or label: `zoom_blur` → `Zoom blur`, `brb` → `BRB`,
/// `youtube` → `YouTube`. Acronyms and brand names keep their usual spelling everywhere.
pub fn nice(s: &str) -> String {
    const ACRONYMS: &[&str] =
        &["brb", "obs", "hdmi", "dmx", "tts", "rgb", "lut", "bpm", "fx", "ui", "usb", "dj", "vhs", "ptt", "tv", "fps", "sfx", "eq", "msi", "led", "uv"];
    const BRANDS: &[(&str, &str)] =
        &[("youtube", "YouTube"), ("tiktok", "TikTok"), ("twitch", "Twitch"), ("kofi", "Ko-fi"), ("xtouch", "X-TOUCH"), ("streamdeck", "Stream Deck")];
    s.replace(['_', '-'], " ")
        .split_whitespace()
        .enumerate()
        .map(|(i, w)| {
            let l = w.to_lowercase();
            if let Some((_, b)) = BRANDS.iter().find(|(k, _)| *k == l) {
                return b.to_string();
            }
            if ACRONYMS.contains(&l.as_str()) {
                return l.to_uppercase();
            }
            let mut w = w.to_string();
            if i == 0
                && let Some(f) = w.get_mut(0..1)
            {
                f.make_ascii_uppercase();
            }
            w
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---- scenes ------------------------------------------------------------------------------------

/// Scenes + transition + switch button as one panel (pop-out window).
pub fn scenes_panel(app: &mut App, ui: &mut egui::Ui) {
    scenes(app, ui);
    ui.add_space(spacing::M);
    let w = ui.available_width().min(560.0);
    transition_picker(app, ui, w);
    let preview = app.m.str("show.scene.preview").to_string();
    let same = preview.is_empty() || preview == app.m.str("show.scene.program");
    let t = app.t.clone();
    if widgets::button_ex(ui, &t, Some(icon::PLAY), "Switch", Kind::Primary, Size::Large, w, !same).clicked() {
        app.take();
    }
}

fn scenes(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let program = app.m.str("show.scene.program").to_string();
    let preview = app.m.str("show.scene.preview").to_string();
    let list: Vec<(String, String, i64)> = app
        .m
        .q_list("scenes")
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let name = s.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
            let label = s.get_path("label").and_then(Value::as_str).unwrap_or(&name).to_string();
            let key = s.get_path("key").and_then(Value::as_i64).unwrap_or(i as i64 + 1);
            (name, label, key)
        })
        .collect();
    ui.horizontal(|ui| {
        ui.label(RichText::new("Scenes").font(font_semibold(type_scale::LARGE)).color(t.fg));
        ui.label(RichText::new("Click to put up next · double-click to switch right away").color(t.text_dim));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if widgets::button_ex(ui, &t, Some(icon::EDIT), "Edit scenes", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                app.open_view(ViewId::Composition);
            }
        });
    });
    ui.add_space(spacing::S);
    if list.is_empty() {
        widgets::panel(ui, &t, |ui| {
            ui.set_width(ui.available_width());
            if widgets::empty_state(ui, &t, icon::LAYERS, "No scenes yet", "A scene is one arrangement of your sources as layers.", Some("Make a scene")) {
                app.open_view(ViewId::Composition);
            }
        });
        return;
    }
    let gap = 12.0;
    let tile_w = 300.0_f32.min((ui.available_width() - gap * (list.len() as f32 - 1.0)) / list.len() as f32).max(140.0);
    let thumb_h = tile_w * 9.0 / 16.0;
    egui::ScrollArea::horizontal().id_salt("scenes").show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for (name, label, key) in &list {
                let st = if *name == program && crate::views::status::on_air(app) {
                    LedState::Active
                } else if *name == preview {
                    LedState::Armed
                } else {
                    LedState::Idle
                };
                let (rect, r) = ui.allocate_exact_size(Vec2::new(tile_w, thumb_h + 38.0), Sense::click());
                if ui.is_rect_visible(rect) {
                    let thumb = Rect::from_min_size(rect.min, Vec2::new(tile_w, thumb_h));
                    monitor::scene_thumb(app, ui, thumb, name);
                    let p = ui.painter();
                    let edge = match st {
                        LedState::Idle if r.hovered() => mix(t.border, t.fg, 0.35),
                        LedState::Idle => t.border,
                        s => se_ui_kit::widgets::led_color(&t, s),
                    };
                    p.rect_stroke(
                        thumb,
                        CornerRadius::same(radius::TILE),
                        Stroke::new(if st == LedState::Idle { 1.0 } else { 3.0 }, edge),
                        StrokeKind::Outside,
                    );
                    if st != LedState::Idle {
                        let tag = if st == LedState::Active { "ON AIR" } else { "UP NEXT" };
                        let g = p.layout_no_wrap(tag.into(), font_bold(10.5), Color32::WHITE);
                        let chip = Rect::from_min_size(thumb.left_top() + Vec2::new(8.0, 8.0), Vec2::new(g.size().x + 12.0, 20.0));
                        p.rect_filled(chip, CornerRadius::same(5), edge);
                        p.galley(chip.center() - g.size() / 2.0, g, Color32::WHITE);
                    }
                    let y = thumb.bottom() + 19.0;
                    p.text(egui::pos2(rect.left() + 2.0, y), Align2::LEFT_CENTER, nice(label), font_semibold(type_scale::BODY), t.fg);
                    p.text(egui::pos2(rect.right() - 2.0, y), Align2::RIGHT_CENTER, key.to_string(), font_mono(type_scale::SMALL), t.text_faint);
                }
                let r = r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("Key {key}: up next · Shift+{key}: switch now"));
                if r.double_clicked() || (r.clicked() && app.m.b("show.direct")) {
                    app.m.command(Op::SceneCut { scene: name.clone(), transition: app.show.transition.clone() });
                } else if r.clicked() {
                    app.m.command(Op::SceneGo { scene: name.clone() });
                }
            }
        });
    });
}

// ---- sources ----------------------------------------------------------------------------------

fn sources(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    ui.horizontal(|ui| {
        ui.label(RichText::new("Cameras & sources").font(font_semibold(type_scale::LARGE)).color(t.fg));
        ui.label(RichText::new("Everything the engine can put on screen").color(t.text_dim));
    });
    ui.add_space(spacing::S);
    // tiles share the width (16:9), within sensible heights
    let n = monitor::atlas_tiles(app).len().max(1) as f32;
    let h = ((ui.available_width() - (n - 1.0) * 8.0) / n * 9.0 / 16.0).clamp(96.0, 220.0);
    monitor::multiview(app, ui, h);
}

// ---- right rail ---------------------------------------------------------------------------------

fn rail_panel(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let labels: Vec<String> = RailTab::ALL
        .iter()
        .map(|(tab, label, _)| {
            let n = rail::badge_count(app, *tab);
            if n > 0 { format!("{label} ({n})") } else { label.to_string() }
        })
        .collect();
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let mut idx = RailTab::ALL.iter().position(|(tab, _, _)| *tab == app.show.rail).unwrap_or(0);
    if widgets::tabs(ui, &t, &mut idx, &refs) {
        app.show.rail = RailTab::ALL[idx].0;
    }
    ui.add_space(spacing::S);
    let body = ui.available_rect_before_wrap();
    // Chat stays pinned; tabs select only the lower pane. Both panes scroll independently.
    // Leave room for the search, at least one message, and the composer on short windows.
    let available = (body.height() - spacing::M).max(0.0);
    let chat_h = (available * 0.6).max(220.0).min(available * 0.7);
    let chat_rect = Rect::from_min_size(body.min, Vec2::new(body.width(), chat_h));
    let detail_rect = Rect::from_min_max(egui::pos2(body.left(), chat_rect.bottom() + spacing::M), body.max);
    ui.scope_builder(egui::UiBuilder::new().id_salt("pinned-chat").max_rect(chat_rect), |ui| {
        ui.set_clip_rect(chat_rect.intersect(ui.clip_rect()));
        ui.label(RichText::new("Chat").font(font_semibold(type_scale::LARGE)).color(t.fg));
        ui.add_space(spacing::XS);
        rail::chat(app, ui);
    });
    ui.painter().hline(body.x_range(), chat_rect.bottom() + spacing::M / 2.0, Stroke::new(1.0, t.border));
    ui.scope_builder(egui::UiBuilder::new().id_salt("rail-detail").max_rect(detail_rect), |ui| {
        ui.set_clip_rect(detail_rect.intersect(ui.clip_rect()));
        let heading = match app.show.rail {
            RailTab::Queue => "Song queue",
            RailTab::Events => "Activity",
            RailTab::Mod => "Mod",
        };
        ui.label(RichText::new(heading).font(font_semibold(type_scale::LARGE)).color(t.fg));
        ui.add_space(spacing::XS);
        match app.show.rail {
            RailTab::Queue => rail::queue(app, ui),
            RailTab::Events => rail::events(app, ui),
            RailTab::Mod => rail::moderation(app, ui),
        }
    });
}

// ---- sound ---------------------------------------------------------------------------------------

fn sound_bar(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    mix::refresh_sources(app, ui);
    let sources = mix::source_channels(app);
    // vertically centre the row in the dock
    let chip_h = 66.0;
    ui.add_space(((ui.available_height() - chip_h) / 2.0).max(0.0));
    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            ui.set_height(chip_h);
            ui.label(RichText::new("Sound").font(font_semibold(type_scale::LARGE)).color(t.fg));
            if widgets::button_ex(ui, &t, Some(icon::SLIDERS), "Mixer", Kind::Secondary, Size::Small, 0.0, true).on_hover_text("Open the full mixer").clicked()
            {
                app.open_view(ViewId::Audio);
            }
        });
        ui.add_space(spacing::L);
        if sources.is_empty() {
            widgets::hint(ui, &t, if app.m.connected { "No audio sources. Add an input in Sound." } else { "Waiting for the engine…" });
            return;
        }
        let gap = 10.0;
        let w = ((ui.available_width() - gap * (sources.len() as f32 - 1.0)) / sources.len() as f32).clamp(160.0, 360.0);
        egui::ScrollArea::horizontal().id_salt("sound").max_height(chip_h).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for source in &sources {
                    source_chip(app, ui, source, w);
                }
            });
        });
    });
}

fn source_chip(app: &mut App, ui: &mut egui::Ui, source: &mix::SourceChannel, w: f32) {
    let t = app.t.clone();
    let gain_a = format!("{}.gain", source.address);
    let mute_a = format!("{}.mute", source.address);
    let range = app.m.meta.get(&gain_a).and_then(|m| m.range).unwrap_or([-60.0, 12.0]);
    let db = app.m.f(&gain_a);
    let muted = app.m.b(&mute_a);
    let level = app.m.sig(&format!("{}.level", source.meter)).unwrap_or(0.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 66.0), Sense::hover());
    ui.painter().rect(rect, CornerRadius::same(radius::CONTROL), t.surface, Stroke::new(1.0, t.border), StrokeKind::Inside);
    let inner = rect.shrink2(Vec2::new(10.0, 7.0));
    ui.scope_builder(egui::UiBuilder::new().max_rect(inner).layout(Layout::top_down(Align::Min)), |ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.horizontal(|ui| {
            let label = &source.label;
            ui.add(egui::Label::new(RichText::new(label).font(font_medium(type_scale::SMALL + 0.5)).color(if muted { t.text_faint } else { t.fg })).truncate());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (ic, tip) = if muted { (icon::MUTE, "Unmute") } else { (icon::VOLUME, "Mute") };
                let r = widgets::button_ex(ui, &t, Some(ic), "", if muted { Kind::Danger } else { Kind::Ghost }, Size::Small, 26.0, true).on_hover_text(tip);
                if r.clicked() {
                    app.m.command(Op::Set { address: mute_a.clone(), value: Value::Bool(!muted) });
                }
            });
        });
        let mut pos = mix::db_to_pos(db, range);
        ui.spacing_mut().slider_width = inner.width();
        ui.spacing_mut().interact_size.y = 16.0;
        let r = ui.add(egui::Slider::new(&mut pos, 0.0..=1.0).show_value(false));
        if r.changed() {
            app.m.command(Op::Set { address: gain_a.clone(), value: Value::Float(mix::pos_to_db(pos, range)) });
        }
        if r.double_clicked() {
            app.m.command(Op::Set { address: gain_a.clone(), value: Value::Float(0.0) });
        }
        r.on_hover_text(format!("{db:+.1} dB · double-click resets"));
        widgets::meter_h(ui, &t, Vec2::new(inner.width(), 4.0), if muted { 0.0 } else { level });
    });
    if !app.m.meta.contains_key(&gain_a) && app.m.connected {
        app.fetch_meta_once(&gain_a);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};

    #[test]
    fn compact_rail_keeps_chat_and_composer_above_queue() {
        struct LiveApp(App);
        impl eframe::App for LiveApp {
            fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
                egui::Panel::top("header").exact_size(100.0).show(ui, |_| {});
                super::ui(&mut self.0, ui);
            }
        }
        let mut h = Harness::builder().with_size([1024.0, 600.0]).build_eframe(|cc| {
            let mut app = App::new(
                cc,
                crate::UiOpts { socket: Some("/nonexistent/se-ui-compact-rail-test.sock".into()), layout: Some("single".into()), program_only: false },
            );
            app.show.rail = RailTab::Queue;
            app.m.state.insert("twitch.auth.status".into(), Value::from("authorized"));
            for i in 0..20 {
                app.m.chat_messages.push_back(se_proto::Event::new(
                    "twitch.chat",
                    se_proto::Origin::System,
                    Value::map().with("user", "viewer").with("message", if i == 19 { "Latest visible message".into() } else { format!("Message {i}") }),
                ));
            }
            app.m.queries.insert("queue".into(), Value::map().with("open", true).with("upcoming", Value::List(vec![])));
            LiveApp(app)
        });
        h.run_steps(4);
        let chat = h.get_by_label("Latest visible message").rect();
        let send = h.get_by_label("Send").rect();
        let queue = h.get_by_label("Song queue").rect();
        assert!(chat.bottom() <= send.top(), "the latest message stays above the composer");
        assert!(send.bottom() <= queue.top() - spacing::M, "the whole composer stays inside the chat pane");
        h.get_by_label("No songs yet");
        for tab in [RailTab::Events, RailTab::Mod, RailTab::Queue] {
            h.state_mut().0.show.rail = tab;
            h.run_steps(4);
            h.get_by_label("Latest visible message");
            h.get_by_label("Send");
        }
    }
}
