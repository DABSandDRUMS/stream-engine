//! Scenes → Overlays & effects (§6, §15.5): every overlay, animated background, particle effect
//! and video/audio effect as a card: what it is, whether it's working, a Test button, on/off,
//! and its settings as real controls (colors, sliders, switches). "New" makes one from the
//! shipped templates. File locations, script CPU and errors with line numbers live under Details.

use crate::app::App;
use crate::views::live::nice;
use egui::{Align, Color32, Layout, RichText, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::theme::{font_mono, font_semibold, mix, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};

/// Kinds a user can create: (kind, friendly name, one-line explanation).
const NEW_KINDS: &[(&str, &str, &str)] = &[
    ("web", "Web overlay", "A web page drawn over the video: alerts, chat, labels, goals."),
    ("script", "Animated graphics", "Shapes and text that react to events, drawn by a small script."),
    ("particles", "Particles", "Confetti, sparks and snow that burst on cue."),
    ("shader", "Background or video effect", "A GPU effect: animated backgrounds or a look applied to the video."),
    ("dsp", "Audio effect", "Processes sound on a sound channel."),
];

#[derive(Clone, Default)]
struct NewForm {
    open: bool,
    name: String,
    kind: usize,
    template: String,
}

fn action(app: &mut App, name: &str, args: Value) {
    app.m.command(Op::Action { name: name.into(), args });
}

/// What a patch is, in words.
fn kind_name(kind: &str, layer: &str) -> &'static str {
    match (kind, layer) {
        ("web", _) => "Web overlay",
        ("shader", "source") => "Animated background",
        ("shader", "transition") => "Transition",
        ("shader", _) => "Video effect",
        ("particles", _) => "Particles",
        ("script", _) => "Animated graphics",
        ("dsp", _) => "Audio effect",
        _ => "Overlay",
    }
}

/// Plain descriptions for the overlays that ship with Stream Engine (their manifests are
/// written for developers). The owner's own overlays show their own description.
fn plain_description(id: &str) -> Option<&'static str> {
    Some(match id {
        "alertbox" => "Pops up when someone follows, subscribes, cheers, raids or tips.",
        "chatbox" => "Shows your chat on stream. Deleted messages disappear from it too.",
        "goals" => "Progress bars for your sub, bits and follower goals.",
        "labels" => "Your latest follower, subscriber and biggest cheer, as text on screen.",
        "eventlist" => "A running list of recent follows, subs, cheers, raids and tips.",
        "credits" => "Rolls the credits at the end of the stream with everyone who joined in.",
        "countdown" => "A \"starting soon\" countdown that also shows the song playing.",
        "confetti" => "A burst of confetti in your colors.",
        "sparks" => "Sparks that fly on cue.",
        "sub_meteors" => "Meteors rain down when someone subscribes.",
        "hype_detector" => "Watches chat, cheers and the music for big moments and marks them for clips.",
        "hype_meter" => "A meter that fills up as the stream gets more exciting.",
        "aurora" => "A calm, flowing light background that moves with the bass.",
        "ringmod" => "A robot-voice sound effect.",
        _ => return None,
    })
}

fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "web" => icon::IMAGE,
        "shader" => icon::PALETTE,
        "particles" => icon::STAR,
        "script" => icon::BOLT,
        "dsp" => icon::VOLUME,
        _ => icon::SPARKLE,
    }
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let now = ui.input(|i| i.time);
    let poll = egui::Id::new("patches-view-poll");
    let last: f64 = ui.data_mut(|d| d.get_temp(poll)).unwrap_or(-10.0);
    if now - last > 1.0 {
        ui.data_mut(|d| d.insert_temp(poll, now));
        app.m.query("patches", Value::Null);
        if app.m.q("patch.templates").is_none() || now - last > 30.0 {
            app.m.query("patch.templates", Value::Null);
        }
    }
    let form_id = egui::Id::new("patches-view-new");
    let mut form: NewForm = ui.data_mut(|d| d.get_temp(form_id)).unwrap_or_default();
    let mut patches = app.m.q_list("patches").to_vec();
    patches.sort_by_key(|p| {
        let kind = p.get_path("kind").and_then(Value::as_str).unwrap_or("");
        (
            ["web", "script", "particles", "shader", "dsp"].iter().position(|k| *k == kind).unwrap_or(9),
            p.get_path("label").and_then(Value::as_str).unwrap_or("").to_lowercase(),
        )
    });

    ui.horizontal(|ui| {
        widgets::hint(ui, &t, "Everything you can put on top of your video, plus effects. Settings change live.");
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if widgets::button_ex(ui, &t, Some(icon::PLUS), "New overlay or effect", Kind::Primary, Size::Medium, 0.0, true).clicked() {
                form.open = !form.open;
            }
        });
    });
    ui.add_space(spacing::M);
    if form.open {
        new_form(app, ui, &mut form);
        ui.add_space(spacing::M);
    }
    ui.data_mut(|d| d.insert_temp(form_id, form));

    if patches.is_empty() {
        widgets::panel(ui, &t, |ui| {
            ui.set_width(ui.available_width());
            let msg =
                if app.m.query_errors.contains_key("patches") { "The overlay loader isn't running." } else { "Make your first overlay with the button above." };
            widgets::empty_state(ui, &t, icon::SPARKLE, "No overlays yet", msg, None);
        });
        return;
    }
    let gap = spacing::L;
    egui::ScrollArea::vertical().id_salt("patches").auto_shrink([false, false]).show(ui, |ui| {
        let w = ui.available_width();
        let cols = ((w + gap) / (460.0 + gap)).floor().clamp(1.0, 4.0) as usize;
        let cw = (w - gap * (cols as f32 - 1.0)) / cols as f32;
        for (ri, row) in patches.chunks(cols).enumerate() {
            // cards in a row share the height of the tallest one (measured last frame)
            let hid = egui::Id::new(("patch-row-h", ri, cols));
            let row_h: f32 = ui.data(|d| d.get_temp(hid)).unwrap_or(0.0);
            let mut tallest = 0.0_f32;
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for p in row {
                    let natural = ui.allocate_ui_with_layout(Vec2::new(cw, 0.0), Layout::top_down(Align::Min), |ui| patch_card(app, ui, p, row_h)).inner;
                    tallest = tallest.max(natural);
                }
            });
            if (tallest - row_h).abs() > 0.5 {
                ui.data_mut(|d| d.insert_temp(hid, tallest));
                ui.ctx().request_repaint();
            }
            ui.add_space(gap);
        }
    });
}

fn new_form(app: &mut App, ui: &mut egui::Ui, form: &mut NewForm) {
    let t = app.t.clone();
    widgets::titled(
        ui,
        &t,
        "New overlay or effect",
        "Starts from a ready-made template you can change.",
        |_| {},
        |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Name").color(t.text_dim));
                ui.add(se_ui_kit::widgets::field(&mut form.name).hint_text("e.g. Sub goal bar").desired_width(260.0));
            });
            ui.add_space(spacing::S);
            ui.horizontal_wrapped(|ui| {
                for (i, (_, name, blurb)) in NEW_KINDS.iter().enumerate() {
                    let r = widgets::button_ex(
                        ui,
                        &t,
                        Some(kind_icon(NEW_KINDS[i].0)),
                        name,
                        if form.kind == i { Kind::Primary } else { Kind::Secondary },
                        Size::Small,
                        0.0,
                        true,
                    );
                    if r.on_hover_text(*blurb).clicked() {
                        form.kind = i;
                    }
                }
            });
            let (kind, _, blurb) = NEW_KINDS[form.kind];
            widgets::hint(ui, &t, blurb);
            let templates: Vec<(String, String)> = app
                .m
                .q_list("patch.templates")
                .iter()
                .filter(|v| v.get_path("kind").and_then(Value::as_str) == Some(kind))
                .map(|v| {
                    (
                        v.get_path("name").and_then(Value::as_str).unwrap_or("").to_string(),
                        v.get_path("description").and_then(Value::as_str).unwrap_or("").to_string(),
                    )
                })
                .collect();
            if !templates.iter().any(|(n, _)| *n == form.template) {
                form.template = templates.first().map(|(n, _)| n.clone()).unwrap_or_else(|| "default".into());
            }
            if templates.len() > 1 {
                ui.add_space(spacing::S);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Start from").color(t.text_dim));
                    egui::ComboBox::from_id_salt("patch-new-template").selected_text(nice(&form.template)).show_ui(ui, |ui| {
                        for (n, d) in &templates {
                            ui.selectable_value(&mut form.template, n.clone(), nice(n)).on_hover_text(d);
                        }
                    });
                });
            }
            ui.add_space(spacing::M);
            let id = crate::views::scene_edit::slug(&form.name);
            let taken = app.m.q_list("patches").iter().any(|p| p.get_path("id").and_then(Value::as_str) == Some(id.as_str()));
            ui.horizontal(|ui| {
                if widgets::button_ex(ui, &t, Some(icon::CHECK), "Create", Kind::Primary, Size::Medium, 0.0, !id.is_empty() && !taken).clicked() {
                    action(app, "patch.new", Value::map().with("id", id.clone()).with("kind", kind).with("template", form.template.clone()).with("open", true));
                    app.m.toast(format!("Made \"{}\". Its files open in your editor.", form.name.trim()), false);
                    form.name.clear();
                    form.open = false;
                    app.m.refresh_soon();
                }
                if widgets::button_ex(ui, &t, None, "Cancel", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
                    form.open = false;
                }
                if taken {
                    widgets::hint(ui, &t, "That name is taken.");
                }
            });
        },
    );
}

/// One overlay card. `min_h` = the row's height (tallest card's natural height); returns this
/// card's natural height (without the stretch) so rows can also shrink.
fn patch_card(app: &mut App, ui: &mut egui::Ui, p: &Value, min_h: f32) -> f32 {
    let t = app.t.clone();
    let s = |k: &str| p.get_path(k).and_then(Value::as_str).unwrap_or("").to_string();
    let id = s("id");
    let kind = s("kind");
    let st = app.m.get(&format!("patch.{id}.state")).and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| s("state"));
    let err = {
        let live = app.m.get(&format!("patch.{id}.error")).and_then(Value::as_str).unwrap_or("").to_string();
        if live.is_empty() { s("error") } else { live }
    };
    let enabled = p.get_path("enabled").is_none_or(Value::truthy) && st != "disabled";
    let title = if s("label").is_empty() || s("label") == id { nice(&id) } else { s("label") };
    let (status, color) = match st.as_str() {
        _ if !enabled => ("Off", t.text_dim),
        "error" => ("Has a problem", t.bright_red),
        "suspended" => ("Paused: too slow", t.yellow),
        _ => ("Working", t.green),
    };
    let border = if st == "error" { mix(t.border, t.bright_red, 0.6) } else { t.border };
    egui::Frame::new()
        .fill(t.surface)
        .stroke(egui::Stroke::new(1.0, border))
        .corner_radius(radius::CARD)
        .inner_margin(egui::Margin::same(spacing::L as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // the frame adds its margins and stroke around this; a card may grow (settings
            // open), which raises the row height for its neighbours next frame
            ui.set_min_height((min_h - 2.0 * spacing::L - 2.0).max(0.0));
            let top = ui.cursor().top();
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(40.0), egui::Sense::hover());
                ui.painter().rect_filled(r, radius::CONTROL, mix(t.surface, t.accent, 0.16));
                ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, kind_icon(&kind), se_ui_kit::theme::font(18.0), t.accent);
                ui.vertical(|ui| {
                    ui.label(RichText::new(&title).font(font_semibold(type_scale::LARGE)).color(t.fg));
                    ui.label(RichText::new(kind_name(&kind, &s("layer"))).color(t.text_dim));
                });
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    let mut on = enabled || st == "suspended";
                    if widgets::toggle(ui, &t, &mut on).on_hover_text(if on { "Turn off" } else { "Turn on" }).changed() {
                        action(app, if on { "patch.enable" } else { "patch.disable" }, Value::map().with("id", id.clone()));
                    }
                });
            });
            let desc = plain_description(&id).map(String::from).unwrap_or_else(|| s("description"));
            if !desc.is_empty() {
                ui.add_space(spacing::S);
                ui.label(RichText::new(desc).color(t.text_dim));
            }
            ui.add_space(spacing::S);
            ui.horizontal(|ui| {
                widgets::badge(ui, &t, status, color);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if st == "suspended" && widgets::button_ex(ui, &t, Some(icon::PLAY), "Resume", Kind::Secondary, Size::Small, 0.0, true).clicked() {
                        action(app, "patch.enable", Value::map().with("id", id.clone()));
                    }
                    if p.get_path("trigger").is_some_and(Value::truthy)
                        && widgets::button_ex(ui, &t, Some(icon::BOLT), "Test", Kind::Secondary, Size::Small, 0.0, enabled)
                            .on_hover_text("Play it once now")
                            .clicked()
                    {
                        app.m.command(Op::Trigger { address: format!("patch.{id}"), payload: Value::Null });
                    }
                });
            });
            if st == "error" && !err.is_empty() {
                ui.add_space(spacing::S);
                let still = p.get_path("live").is_some_and(Value::truthy);
                widgets::hint(
                    ui,
                    &t,
                    if still {
                        "Its last change has a mistake, so the previous version is still showing."
                    } else {
                        "It has a mistake and can't show until it's fixed."
                    },
                );
            }
            let params = p.get_path("params").and_then(Value::as_list).unwrap_or(&[]).to_vec();
            if !params.is_empty() {
                ui.add_space(spacing::S);
                widgets::details(ui, &t, ("patch-settings", &id), "Settings", |ui| {
                    for q in &params {
                        param_row(app, ui, q);
                    }
                });
            }
            widgets::details(ui, &t, ("patch-details", &id), "Details", |ui| {
                widgets::fact(ui, &t, "Folder", &format!("patches/{id}/"));
                if !err.is_empty() {
                    let loc = match (p.get_path("error_file").and_then(Value::as_str), p.get_path("error_line").and_then(Value::as_i64)) {
                        (Some(f), Some(l)) if l > 0 => format!("{f}:{l}"),
                        (Some(f), _) => f.to_string(),
                        _ => String::new(),
                    };
                    ui.label(RichText::new(format!("{loc} {err}")).font(font_mono(type_scale::SMALL)).color(t.bright_red));
                }
                if let Some(sc) = p.get_path("script") {
                    let n = |k: &str| sc.get_path(k).and_then(Value::as_f64).unwrap_or(0.0);
                    let budget = p.get_path("budget.cpu_ms").and_then(Value::as_f64).unwrap_or(2.0);
                    widgets::fact(ui, &t, "Script time", &format!("{:.2} ms (max {:.2}, allowed {budget})", n("cpu_ms_avg"), n("cpu_ms_peak")));
                    widgets::fact(ui, &t, "Memory", &format!("{} KB", n("memory_kb") as u64));
                }
                ui.horizontal(|ui| {
                    if widgets::button_ex(ui, &t, Some(icon::EDIT), "Edit files", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                        action(app, "patch.open", Value::map().with("id", id.clone()));
                    }
                    if widgets::button_ex(ui, &t, Some(icon::UNDO), "Reload", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                        action(app, "patch.reload", Value::map().with("id", id.clone()));
                    }
                });
            });
            ui.cursor().top() - top + 2.0 * spacing::L + 2.0
        })
        .inner
}

fn rgba(v: &Value) -> Option<Color32> {
    match v {
        Value::Str(s) => se_ui_kit::theme::hex(s),
        Value::List(l) if l.len() >= 3 => {
            let c = |i: usize| (l.get(i).and_then(Value::as_f64).unwrap_or(1.0).clamp(0.0, 1.0) * 255.0).round() as u8;
            Some(Color32::from_rgba_unmultiplied(c(0), c(1), c(2), if l.len() > 3 { c(3) } else { 255 }))
        }
        _ => None,
    }
}

/// A color in the same form as the parameter's default (`"#rrggbb"` or `[r, g, b, a]`).
fn color_value(c: Color32, like: &Value) -> Value {
    let [r, g, b, a] = c.to_srgba_unmultiplied();
    match like {
        Value::List(l) => {
            let f = |x: u8| Value::Float(((x as f64 / 255.0) * 1000.0).round() / 1000.0);
            let mut v = vec![f(r), f(g), f(b)];
            if l.len() > 3 {
                v.push(f(a));
            }
            Value::List(v)
        }
        _ if a < 255 => Value::Str(format!("#{r:02x}{g:02x}{b:02x}{a:02x}")),
        _ => Value::Str(format!("#{r:02x}{g:02x}{b:02x}")),
    }
}

/// One setting as a real control, saved with `set_base`.
fn param_row(app: &mut App, ui: &mut egui::Ui, q: &Value) {
    let t = app.t.clone();
    let s = |k: &str| q.get_path(k).and_then(Value::as_str).unwrap_or("").to_string();
    let addr = s("address");
    let ty = s("type");
    let default = q.get_path("default").cloned().unwrap_or(Value::Null);
    let cur = app.m.get(&addr).cloned().or_else(|| q.get_path("value").cloned()).unwrap_or_else(|| default.clone());
    let label = if s("description").is_empty() { nice(&s("name")) } else { s("description") };
    let mut set = None;
    ui.horizontal(|ui| {
        ui.add_sized([150.0, 24.0], egui::Label::new(RichText::new(&label).color(t.text_dim)).truncate());
        match ty.as_str() {
            "color" => {
                let mut c = rgba(&cur).unwrap_or(Color32::WHITE);
                if egui::color_picker::color_edit_button_srgba(ui, &mut c, egui::color_picker::Alpha::OnlyBlend).changed() {
                    set = Some(color_value(c, &default));
                }
            }
            "bool" => {
                let mut b = cur.truthy();
                if widgets::toggle(ui, &t, &mut b).changed() {
                    set = Some(Value::Bool(b));
                }
            }
            "enum" => {
                let opts: Vec<String> =
                    q.get_path("options").and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(|v| v.as_str().map(String::from)).collect();
                let c = cur.as_str().unwrap_or("").to_string();
                if opts.len() <= 4 {
                    let labels: Vec<String> = opts.iter().map(|o| nice(o)).collect();
                    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                    let mut i = opts.iter().position(|o| *o == c).unwrap_or(0);
                    if widgets::segmented(ui, &t, &mut i, &refs) {
                        set = Some(Value::Str(opts[i].clone()));
                    }
                } else {
                    egui::ComboBox::from_id_salt(("param", &addr)).selected_text(nice(&c)).show_ui(ui, |ui| {
                        for o in &opts {
                            if ui.selectable_label(*o == c, nice(o)).clicked() {
                                set = Some(Value::Str(o.clone()));
                            }
                        }
                    });
                }
            }
            "float" | "int" => {
                let (lo, hi) = match q.get_path("range").and_then(Value::as_list) {
                    Some([a, b, ..]) => (a.as_f64().unwrap_or(0.0), b.as_f64().unwrap_or(1.0)),
                    _ => (0.0, cur.as_f64().unwrap_or(1.0).abs().max(1.0) * 2.0),
                };
                // while dragging, the slider keeps its own value; it's saved on release
                let key = egui::Id::new(("param-drag", &addr));
                let mut x = ui.data(|d| d.get_temp::<f64>(key)).unwrap_or_else(|| cur.as_f64().unwrap_or(0.0));
                ui.spacing_mut().slider_width = (ui.available_width() - 70.0).max(80.0);
                ui.spacing_mut().interact_size.y = 18.0;
                let slider = egui::Slider::new(&mut x, lo..=hi);
                let slider = if ty == "int" { slider.integer() } else { slider.max_decimals(2) };
                let r = ui.add(slider);
                if r.dragged() {
                    ui.data_mut(|d| d.insert_temp(key, x));
                } else {
                    ui.data_mut(|d| d.remove::<f64>(key));
                }
                if r.drag_stopped() || (r.changed() && !r.dragged()) {
                    set = Some(if ty == "int" { Value::Int(x.round() as i64) } else { Value::Float((x * 1000.0).round() / 1000.0) });
                }
            }
            _ => {
                let key = egui::Id::new(("param-text", &addr));
                let mut buf: String = ui.data_mut(|d| d.get_temp(key)).unwrap_or_else(|| cur.as_str().map(String::from).unwrap_or_else(|| cur.to_string()));
                let r = ui.add(se_ui_kit::widgets::field(&mut buf).desired_width(ui.available_width()));
                if r.lost_focus() && buf != cur.as_str().unwrap_or("") {
                    set = Some(Value::Str(buf.clone()));
                }
                ui.data_mut(|d| d.insert_temp(key, buf));
            }
        }
    });
    if let Some(v) = set {
        app.m.command(Op::SetBase { address: addr, value: v });
    }
}
