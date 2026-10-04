//! Lights (§9, §15.6): the lights console. Looks and cue lists are made for the streamer in the
//! project files (docs/lights.md); this page uses them:
//! - **Master**: brightness, blackout, panic, output arming and layout verification.
//! - **Musical layers**: base/rhythm/accent status and generation-owned controls.
//! - **Looks**: authored palettes select/release the base slot; palette knobs remain editable.
//! - **Manual cue lists (advanced)**: existing Go / Back / Stop and list knobs.
//! - **Stage**: rendered preview, not confirmation of the fixtures' physical state.
//!
//! Starting something while on air asks first ("Your viewers will see this"). Pure client of
//! the se-dmx queries, actions and state addresses; knobs go through `lights.knob` (sent live
//! while moving, saved once they settle).

use crate::app::App;
use crate::model::Model;
use crate::views::knobs::{self, Moved};
use crate::views::live::nice;
use egui::text::{LayoutJob, TextWrapping};
use egui::{Align, Align2, Color32, CornerRadius, Galley, Layout, Pos2, Rect, RichText, Sense, Shape, Stroke, StrokeKind, Ui, UiBuilder, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font, font_bold, font_medium, font_mono, font_semibold, mix, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, LedState, Size, Tone, icon};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Structural queries, refreshed about once a second.
const SLOW_QUERIES: [&str; 5] = ["lights.rig", "lights.cuelists", "lights.palettes", "lights.layers", "audio.mix"];
const SLOW_EVERY: Duration = Duration::from_secs(1);
/// Live output (stage picture, flash limiter), refreshed while the page is visible.
const FAST_EVERY: Duration = Duration::from_millis(70);
/// After an action, re-query structure this soon (the engine applies actions asynchronously).
const AFTER_ACTION: Duration = Duration::from_millis(150);
/// Locally set values win over the (lagging) engine echo for this long.
const EDIT_HOLD: Duration = Duration::from_millis(1500);
/// A knob being moved is sent live at most this often…
const KNOB_LIVE_EVERY: Duration = Duration::from_millis(60);
/// …and saved into its file once it has been still this long (sliders save on release).
const KNOB_QUIET: Duration = Duration::from_millis(600);
/// Pan travel assumed by the stage picture (typical mover: 540°); pan 0.5 = centre.
const PAN_TRAVEL_DEG: f32 = 540.0;
const CROSSHAIRS: &str = "\u{f05b}";
/// Tiles in the look grid: at least this wide, grown to fill the row (up to the max).
const TILE_MIN_W: f32 = 132.0;
const TILE_MAX_W: f32 = 260.0;
/// Height of the name + description lines under a tile's face.
const TILE_TEXT_H: f32 = 50.0;

/// Something that starts on the lights (asked first while on air).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Start {
    Default,
    Look(String),
    List(String),
}

/// A knob being moved: sent live while it moves, saved once it settles.
#[derive(Clone, Debug)]
struct KnobMove {
    /// `look` or `cuelist`.
    kind: &'static str,
    item: String,
    target: String,
    value: Value,
    moved: Instant,
    sent: Option<Instant>,
    saved: bool,
}

impl KnobMove {
    fn op(&self, save: bool) -> Op {
        act(
            "lights.knob",
            Value::map().with(self.kind, self.item.as_str()).with("target", self.target.as_str()).with("value", self.value.clone()).with("save", save),
        )
    }
}

/// Per-view UI state, kept in egui memory.
#[derive(Clone, Default)]
struct LightsState {
    slow_at: Option<Instant>,
    fast_at: Option<Instant>,
    /// Look whose knobs are shown (palette name); the first one on (else the first) when unset.
    look: Option<String>,
    /// On air: waiting for "Your viewers will see this" before starting this.
    confirm: Option<Start>,
    /// Recently set values (`@<address>`, `look:<name>`).
    edits: HashMap<String, (Value, Instant)>,
    /// Knobs being moved, by `<kind>/<item>/<target>`.
    knobs: HashMap<String, KnobMove>,
    /// A quick effect just made here: open it in Scenes → Quick effects.
    open_quick: Option<String>,
    /// Explicitly open the rig file for fixture, output and safety setup.
    open_setup: bool,
}

impl LightsState {
    /// Structure about once a second; the live output too when `live` (the stage picture).
    fn refresh(&mut self, m: &mut Model, now: Instant, live: bool) {
        if !m.connected {
            return;
        }
        if self.slow_at.is_none_or(|t| now.saturating_duration_since(t) >= SLOW_EVERY) {
            self.slow_at = Some(now);
            for q in SLOW_QUERIES {
                m.query(q, Value::Null);
            }
        }
        if live && self.fast_at.is_none_or(|t| now.saturating_duration_since(t) >= FAST_EVERY) {
            self.fast_at = Some(now);
            m.query("lights.output", Value::Null);
        }
    }

    /// Schedule the structural refresh `AFTER_ACTION` from now.
    fn refresh_soon(&mut self, now: Instant) {
        self.slow_at = now.checked_sub(SLOW_EVERY.saturating_sub(AFTER_ACTION));
    }

    fn edit(&mut self, key: &str, v: Value, now: Instant) {
        self.edits.insert(key.to_string(), (v, now));
    }

    fn edited(&self, key: &str, now: Instant) -> Option<&Value> {
        self.edits.get(key).filter(|(_, at)| now.saturating_duration_since(*at) < EDIT_HOLD).map(|(v, _)| v)
    }

    /// The value a knob shows while it's being moved (and a moment after).
    fn knob_value(&self, key: &str, now: Instant) -> Option<&Value> {
        self.knobs.get(key).filter(|k| !k.saved || now.saturating_duration_since(k.moved) < EDIT_HOLD).map(|k| &k.value)
    }

    /// A knob moved this frame: live while dragging (throttled), saved when done.
    #[allow(clippy::too_many_arguments)]
    fn knob_moved(&mut self, out: &mut Vec<Op>, kind: &'static str, item: &str, target: &str, value: Value, moved: Moved, now: Instant) {
        if moved == Moved::No {
            return;
        }
        let k = self.knobs.entry(knob_key(kind, item, target)).or_insert_with(|| KnobMove {
            kind,
            item: item.to_string(),
            target: target.to_string(),
            value: Value::Null,
            moved: now,
            sent: None,
            saved: true,
        });
        k.value = value;
        k.moved = now;
        if moved == Moved::Done {
            k.saved = true;
            k.sent = Some(now);
            out.push(k.op(true));
        } else {
            k.saved = false;
            if k.sent.is_none_or(|s| now.saturating_duration_since(s) >= KNOB_LIVE_EVERY) {
                k.sent = Some(now);
                out.push(k.op(false));
            }
        }
    }

    /// Save knobs that have been still for a moment; forget settled ones.
    fn flush_knobs(&mut self, out: &mut Vec<Op>, now: Instant) -> bool {
        for k in self.knobs.values_mut() {
            if !k.saved && now.saturating_duration_since(k.moved) >= KNOB_QUIET {
                k.saved = true;
                k.sent = Some(now);
                out.push(k.op(true));
            }
        }
        self.knobs.retain(|_, k| !k.saved || now.saturating_duration_since(k.moved) < EDIT_HOLD);
        self.knobs.values().any(|k| !k.saved)
    }
}

fn knob_key(kind: &str, item: &str, target: &str) -> String {
    format!("{kind}/{item}/{target}")
}

/// Everything a card reads this frame.
struct Cx<'a> {
    m: &'a Model,
    t: &'a Theme,
    rig: &'a Rig,
    output: Option<&'a Value>,
    on_air: bool,
    now: Instant,
}

/// Full-area Lights view.
pub fn ui(app: &mut App, ui: &mut Ui) {
    let id = ui.make_persistent_id("lights_view");
    let now = Instant::now();
    let mut st = ui.data_mut(|d| std::mem::take(d.get_temp_mut_or_default::<LightsState>(id)));
    st.refresh(&mut app.m, now, true);
    let t = app.t.clone();
    let on_air = crate::views::status::on_air(app);
    let mut out = Vec::new();
    view(&app.m, &t, &mut st, ui, &mut out, now, on_air);
    // live knob moves don't need the structure re-read; everything else does
    if out.iter().any(|op| !matches!(op, Op::Action { name, args } if name == "lights.knob" && args.get_path("save") == Some(&Value::Bool(false)))) {
        st.refresh_soon(now);
    }
    let open_quick = st.open_quick.take();
    let open_setup = std::mem::take(&mut st.open_setup);
    ui.data_mut(|d| d.insert_temp(id, st));
    for op in out {
        app.m.command(op);
    }
    if let Some(preset) = open_quick {
        let ctx = ui.ctx().clone();
        crate::views::actions::open(app, &ctx, &preset);
    }
    if open_setup {
        app.open_in_editor("lights/rig.toml", None);
    }
}

/// Compact lights control for the Overview: master brightness and Blackout, the running cue
/// list with Go / Stop, and the looks as on/off tiles — one card about 140 px tall that fills
/// the width. Without patched lights it's the one-line "not set up yet".
pub fn overview_strip(app: &mut App, ui: &mut Ui) {
    let id = ui.make_persistent_id("lights_overview");
    let now = Instant::now();
    let mut st = ui.data_mut(|d| std::mem::take(d.get_temp_mut_or_default::<LightsState>(id)));
    st.refresh(&mut app.m, now, false);
    let t = app.t.clone();
    let on_air = crate::views::status::on_air(app);
    let mut out = Vec::new();
    strip(&app.m, &t, &mut st, ui, &mut out, now, on_air);
    if !out.is_empty() {
        st.refresh_soon(now);
    }
    let open_setup = std::mem::take(&mut st.open_setup);
    ui.data_mut(|d| d.insert_temp(id, st));
    for op in out {
        app.m.command(op);
    }
    if open_setup {
        app.open_in_editor("lights/rig.toml", None);
    }
}

fn strip(m: &Model, t: &Theme, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>, now: Instant, on_air: bool) {
    st.edits.retain(|_, (_, at)| now.saturating_duration_since(*at) < EDIT_HOLD);
    if !on_air {
        st.confirm = None;
    }
    let rig = Rig::parse(m.q("lights.rig"));
    widgets::panel(ui, t, |ui| {
        ui.set_width(ui.available_width());
        let one_liner = |ui: &mut Ui, text: &str| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::LIGHT).size(type_scale::BODY + 1.0).color(t.text_dim));
                ui.label(RichText::new("Lights").font(font_semibold(type_scale::BODY)).color(t.fg));
                ui.add_space(spacing::S);
                widgets::hint(ui, t, text);
            });
        };
        if !rig.present {
            one_liner(ui, "Waiting for your lights…");
            return;
        }
        if rig.fixtures.is_empty() {
            one_liner(ui, "No lights are patched yet.");
            st.open_setup = widgets::button_ex(ui, t, Some(icon::SETTINGS), "Set up lights", Kind::Secondary, Size::Small, 0.0, m.connected).clicked();
            return;
        }
        let cx = Cx { m, t, rig: &rig, output: None, on_air, now };
        let lists = m.q_list("lights.cuelists");
        let playing = |cl: &Value| {
            let n = s(cl, "name");
            st.edited(&format!("@lights.cuelist.{n}.playing"), now)
                .map(Value::truthy)
                .or_else(|| m.get(&format!("lights.cuelist.{n}.playing")).map(Value::truthy))
                .unwrap_or_else(|| cl.get_path("playing").is_some_and(Value::truthy))
        };
        // the running cue list (steps first), else the first one to step through
        let list = lists
            .iter()
            .filter(|cl| playing(cl))
            .min_by_key(|cl| list(cl, "cues").len() <= 1)
            .map(|cl| (cl, true))
            .or_else(|| lists.iter().find(|cl| list(cl, "cues").len() > 1).or_else(|| lists.first()).map(|cl| (cl, false)));
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = spacing::S;
            ui.label(RichText::new(icon::LIGHT).size(type_scale::BODY + 1.0).color(t.text_dim));
            ui.label(RichText::new("Lights").font(font_semibold(type_scale::BODY)).color(t.fg));
            ui.add_space(spacing::L);
            master_controls(&cx, st, ui, out, (ui.available_width() * 0.2).clamp(140.0, 420.0));
            if let Some((cl, running)) = list {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| strip_cuelist(&cx, st, ui, out, cl, running));
            }
        });
        ui.add_space(spacing::S);
        default_controls(&cx, st, ui, out);
        ui.add_space(spacing::S);
        match st.confirm.clone() {
            Some(Start::Default) => {}
            Some(start) => {
                let (what, action) = match &start {
                    Start::Look(n) => {
                        (m.q_list("lights.palettes").iter().find(|p| s(p, "name") == n.as_str()).map(look_label).unwrap_or_else(|| nice(n)), "Turn it on")
                    }
                    Start::List(n) => (lists.iter().find(|c| s(c, "name") == n.as_str()).map(list_label).unwrap_or_else(|| nice(n)), "Start it"),
                    Start::Default => ("Configured idle room lighting".to_string(), "Default lights"),
                };
                ui.horizontal(|ui| {
                    ui.set_min_height(34.0);
                    ui.label(RichText::new(icon::LIVE).color(t.bright_red));
                    ui.label(RichText::new(format!("You're on air: your viewers will see “{what}”.")).font(font_medium(type_scale::BODY)).color(t.fg));
                    if widgets::button_ex(ui, t, None, action, Kind::Live, Size::Small, 0.0, m.connected && !panic_latched(m) && !layer_busy(m, st, "base", now)).clicked() {
                        match &start {
                            Start::Default => restore_default(st, out),
                            Start::Look(n) => {
                                select_look(st, out, n, now);
                            }
                            Start::List(n) => {
                                out.push(act("lights.go", Value::map().with("cuelist", n.as_str())));
                                st.edit(&format!("@lights.cuelist.{n}.playing"), Value::Bool(true), now);
                            }
                        }
                        st.confirm = None;
                    }
                    if widgets::button_ex(ui, t, None, "Cancel", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                        st.confirm = None;
                    }
                });
            }
            None => strip_looks(&cx, st, ui, out),
        }
    });
}

/// The strip's cue list: name, where it is, Go and Stop (drawn right to left).
fn strip_cuelist(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>, cl: &Value, running: bool) {
    let (m, t, now) = (cx.m, cx.t, cx.now);
    let name = s(cl, "name");
    let args = || Value::map().with("cuelist", name);
    ui.spacing_mut().item_spacing.x = spacing::XS + 2.0;
    if widgets::button_ex(ui, t, Some(icon::STOP), "Stop", Kind::Secondary, Size::Small, 0.0, running).on_hover_text("Stop this cue list").clicked() {
        out.push(act("lights.release", args()));
        st.edit(&format!("@lights.cuelist.{name}.playing"), Value::Bool(false), now);
    }
    let cues = list(cl, "cues");
    let multi = cues.len() > 1;
    let tip = if running && multi { "Go to the next step" } else { "Start this cue list" };
    if widgets::button_ex(ui, t, Some(icon::PLAY), "Go", if running && multi { Kind::Primary } else { Kind::Secondary }, Size::Small, 56.0, true)
        .on_hover_text(tip)
        .clicked()
    {
        if !running && cx.on_air {
            st.confirm = Some(Start::List(name.to_string()));
        } else {
            out.push(act("lights.go", args()));
            st.edit(&format!("@lights.cuelist.{name}.playing"), Value::Bool(true), now);
        }
    }
    let current =
        m.get(&format!("lights.cuelist.{name}.cue")).map(text).filter(|c| !c.is_empty()).or_else(|| cl.get_path("current").filter(|v| !v.is_null()).map(text));
    let where_ = match (&current, running) {
        (Some(c), true) if multi => {
            let i = cues.iter().position(|x| x.get_path("id").map(text).as_deref() == Some(c.as_str()));
            let step = i.map(|i| step_label(&cues[i])).unwrap_or_else(|| format!("Step {c}"));
            format!("{step} ({} of {})", i.map(|i| i + 1).unwrap_or(0), cues.len())
        }
        (_, true) => "Running".into(),
        _ => "Stopped".into(),
    };
    line_text(ui, &where_, font(type_scale::SMALL + 0.5), if running { t.fg } else { t.text_dim }, 260.0);
    line_text(ui, &list_label(cl), font_semibold(type_scale::BODY), t.fg, 220.0);
    let (r, _) = ui.allocate_exact_size(Vec2::new(10.0, 20.0), Sense::hover());
    ui.painter().circle_filled(r.center(), 4.5, if running { t.green } else { t.text_faint });
}

/// The strip's looks: one row of on/off tiles filling the width ("+N more" when they don't fit).
fn strip_looks(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>) {
    let t = cx.t;
    let pals = cx.m.q_list("lights.palettes");
    if pals.is_empty() {
        widgets::hint(ui, t, "No looks yet. Add your own under lights/palettes/.");
        return;
    }
    const MIN_W: f32 = 112.0;
    const MAX_W: f32 = 380.0;
    let gap = spacing::S;
    let w = ui.available_width();
    let fit = (((w + gap) / (MIN_W + gap)).floor() as usize).max(1);
    let shown = if pals.len() > fit { fit.saturating_sub(1).max(1) } else { pals.len() };
    let cols = if pals.len() > shown { shown + 1 } else { shown };
    let cw = ((w - gap * (cols as f32 - 1.0)) / cols as f32).floor().min(MAX_W);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        for pal in &pals[..shown] {
            let name = s(pal, "name");
            let on = look_on(cx, st, pal);
            let c = look_color(cx, st, pal).unwrap_or_else(|| {
                let i = look_attr(pal, "intensity").and_then(Value::as_f32).unwrap_or(1.0);
                mix(t.inset, t.fg_bright, i.clamp(0.0, 1.0) * 0.9)
            });
            let tip = match (on, cx.on_air) {
                (true, _) => "On · tap to turn it off",
                (false, true) => "Tap to try it on air (asks first)",
                (false, false) => "Tap to turn it on",
            };
            let label = look_label(pal);
            if ui.add_enabled_ui(cx.m.connected && !panic_latched(cx.m) && !layer_busy(cx.m, st, "base", cx.now), |ui| look_chip(ui, t, cw, c, &label, on)).inner.on_hover_text(format!("{label}\n{tip}")).clicked() {
                toggle_look(cx, st, out, name, on);
            }
        }
        if pals.len() > shown {
            let rest: Vec<String> = pals[shown..].iter().map(look_label).collect();
            let (r, resp) = ui.allocate_exact_size(Vec2::new(cw, 34.0), Sense::hover());
            ui.painter().rect(r, CornerRadius::same(radius::CONTROL), t.surface_hi, Stroke::new(1.0, t.border), StrokeKind::Inside);
            ui.painter().text(r.center(), Align2::CENTER_CENTER, format!("+{} more", rest.len()), font_medium(type_scale::SMALL + 0.5), t.text_dim);
            resp.on_hover_text(format!("On the Lights page: {}", rest.join(", ")));
        }
    });
}

/// A compact look toggle: colour swatch and name; accent border and tint when on.
fn look_chip(ui: &mut Ui, t: &Theme, w: f32, color: Color32, label: &str, on: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 34.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, on, format!("{label} look")));
    if ui.is_rect_visible(rect) {
        let fill = if on {
            mix(t.surface_hi, t.accent, 0.14)
        } else if resp.hovered() {
            mix(t.surface_hi, t.fg, 0.05)
        } else {
            t.surface_hi
        };
        let p = ui.painter();
        p.rect(
            rect,
            CornerRadius::same(radius::CONTROL),
            fill,
            Stroke::new(if on { 2.0 } else { 1.0 }, if on { t.accent } else { t.border }),
            StrokeKind::Inside,
        );
        let sw = Rect::from_min_size(Pos2::new(rect.left() + 8.0, rect.center().y - 9.0), Vec2::splat(18.0));
        p.rect_filled(sw, CornerRadius::same(4), if on { color } else { mix(t.surface_hi, color, 0.8) });
        let right = if on { 30.0 } else { 8.0 };
        let name = one_line(ui, label, font_medium(type_scale::SMALL + 1.0), t.fg, rect.width() - 34.0 - right);
        ui.painter().galley_with_override_text_color(Pos2::new(sw.right() + 8.0, rect.center().y - name.size().y / 2.0), name, t.fg);
        if on {
            ui.painter().text(Pos2::new(rect.right() - 10.0, rect.center().y), Align2::RIGHT_CENTER, "ON", font_bold(10.0), t.accent);
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn view(m: &Model, t: &Theme, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>, now: Instant, on_air: bool) {
    st.edits.retain(|_, (_, at)| now.saturating_duration_since(*at) < EDIT_HOLD);
    if st.flush_knobs(out, now) {
        ui.ctx().request_repaint_after(KNOB_QUIET);
    }
    if !on_air {
        st.confirm = None;
    }
    let rig = Rig::parse(m.q("lights.rig"));
    if !rig.present {
        widgets::panel(ui, t, |ui| {
            ui.set_width(ui.available_width());
            widgets::empty_state(
                ui,
                t,
                icon::LIGHT,
                "Waiting for your lights",
                "Your looks, cue lists and lights show up here once Stream Engine is running.",
                None,
            );
        });
        return;
    }
    // Empty libraries and unpatched rigs still expose safety and the layer foundation.
    let output = m.q("lights.output");
    let cx = Cx { m, t, rig: &rig, output, on_air, now };
    egui::ScrollArea::vertical().id_salt("lights_console").auto_shrink([false, false]).show(ui, |ui| {
        if !rig.errors.is_empty() {
            widgets::callout(
                ui,
                t,
                Tone::Warn,
                icon::WARN,
                "Part of your lights setup couldn't load",
                "Everything else keeps working. Send us what's under Details and we'll fix it.",
                None,
            );
            widgets::details(ui, t, "lights_errors", "Details", |ui| {
                for e in &rig.errors {
                    ui.add(egui::Label::new(RichText::new(e).size(type_scale::SMALL + 0.5).color(t.text_dim)).wrap());
                }
            });
            ui.add_space(spacing::L);
        }
        master_card(&cx, st, ui, out);
        ui.add_space(spacing::L);
        foundation_card(&cx, st, ui, out);
        ui.add_space(spacing::L);
        main_light_card(&cx, ui);
        ui.add_space(spacing::L);
        capabilities_card(&cx, ui);
        ui.add_space(spacing::L);
        let w = ui.available_width();
        let right = (w * 0.42).clamp(400.0, 1100.0);
        columns(
            ui,
            w - right - spacing::L,
            &mut (st, out),
            |ui, (st, out)| {
                looks_card(&cx, st, ui, out);
                ui.add_space(spacing::L);
                adjust_card(&cx, st, ui, out);
                ui.add_space(spacing::L);
                stage_card(&cx, ui);
                ui.add_space(spacing::L);
            },
            |ui, (st, out)| {
                cuelists_card(&cx, st, ui, out);
                ui.add_space(spacing::L);
            },
        );
    });
}

// ---------------------------------------------------------------- data

fn act(name: &str, args: Value) -> Op {
    Op::Action { name: name.into(), args }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

fn num(v: &Value, k: &str) -> Option<f64> {
    v.get_path(k).and_then(Value::as_f64)
}

fn list<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get_path(k).and_then(Value::as_list).unwrap_or(&[])
}

fn strs(v: &Value, k: &str) -> Vec<String> {
    list(v, k).iter().map(text).collect()
}

/// Display form of a scalar-ish value (strings unquoted).
fn text(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        other => other.to_string(),
    }
}

fn vec2_of(v: &Value, k: &str) -> [f32; 2] {
    v.get_path(k).and_then(Value::as_vec2).unwrap_or([0.5, 0.5])
}

#[derive(Clone, Debug, Default)]
struct Fixture {
    id: String,
    label: String,
    position: [f32; 2],
    heads: Vec<String>,
}

impl Fixture {
    fn name(&self) -> String {
        if self.label.is_empty() { nice(&self.id) } else { self.label.clone() }
    }
}

#[derive(Clone, Debug, Default)]
struct HeadInfo {
    id: String,
    fixture: String,
    position: [f32; 2],
    rotation: f32,
    kind: String,
    beam: f32,
    moving: bool,
}

#[derive(Clone, Debug, Default)]
struct Rig {
    fixtures: Vec<Fixture>,
    heads: Vec<HeadInfo>,
    outputs: Vec<Value>,
    errors: Vec<String>,
    safety: Option<Value>,
    armed: Option<bool>,
    unverified: Vec<String>,
    targets: Vec<String>,
    present: bool,
}

impl Rig {
    fn parse(v: Option<&Value>) -> Rig {
        let Some(v) = v.filter(|v| v.as_map().is_some()) else {
            return Rig::default();
        };
        let fixtures = list(v, "fixtures")
            .iter()
            .map(|f| Fixture { id: s(f, "id").to_string(), label: s(f, "label").to_string(), position: vec2_of(f, "position"), heads: strs(f, "heads") })
            .collect();
        let heads = list(v, "heads")
            .iter()
            .map(|h| HeadInfo {
                id: s(h, "id").to_string(),
                fixture: s(h, "fixture").to_string(),
                position: vec2_of(h, "position"),
                rotation: num(h, "rotation").unwrap_or(0.0) as f32,
                kind: s(h, "kind").to_string(),
                beam: num(h, "beam").unwrap_or(25.0) as f32,
                moving: h.get_path("moving").is_some_and(Value::truthy),
            })
            .collect();
        Rig {
            fixtures,
            heads,
            outputs: list(v, "outputs").to_vec(),
            errors: strs(v, "errors"),
            safety: v.get_path("safety").filter(|s| s.as_map().is_some()).cloned(),
            armed: v.get_path("output_armed").and_then(|v| match v { Value::Bool(b) => Some(*b), _ => None }),
            unverified: list(v, "fixtures").iter().filter(|f| !f.get_path("layout_verified").is_some_and(Value::truthy)).map(|f| s(f, "id").to_string()).collect(),
            targets: {
                let mut targets = vec!["all".to_string()];
                targets.extend(list(v, "fixtures").iter().map(|f| s(f, "id").to_string()));
                if let Some(groups) = v.get_path("groups").and_then(Value::as_map) {
                    targets.extend(groups.keys().cloned());
                }
                targets.sort();
                targets.dedup();
                targets
            },
            present: true,
        }
    }

    fn fixture(&self, id: &str) -> Option<&Fixture> {
        self.fixtures.iter().find(|f| f.id == id)
    }

    fn head(&self, id: &str) -> Option<&HeadInfo> {
        self.heads.iter().find(|h| h.id == id)
    }

    /// Heads of a fixture (a fixture without explicit heads is its own single head).
    fn fixture_heads(&self, f: &Fixture) -> Vec<String> {
        if f.heads.is_empty() { vec![f.id.clone()] } else { f.heads.clone() }
    }

    /// Friendly name of a light.
    fn head_name(&self, id: &str) -> String {
        match self.head(id).and_then(|h| self.fixture(&h.fixture)) {
            Some(f) if f.id != id => format!("{} · {}", f.name(), nice(id.strip_prefix(&format!("{}_", f.id)).unwrap_or(id))),
            Some(f) => f.name(),
            None => nice(id),
        }
    }
}

/// A palette/cue attribute entry: plain value or `{ value, fade, … }`.
fn plain(v: &Value) -> &Value {
    v.get_path("value").filter(|_| v.as_map().is_some()).unwrap_or(v)
}

fn color_of(v: &Value) -> Option<Color32> {
    let c = plain(v).as_color()?;
    Some(rgb(c[0], c[1], c[2]))
}

fn rgb(r: f32, g: f32, b: f32) -> Color32 {
    let q = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color32::from_rgb(q(r), q(g), q(b))
}

fn pct(v: f32) -> String {
    format!("{:.0}%", v.clamp(0.0, 1.0) * 100.0)
}

/// A number with at most two decimals and no trailing zeros: 4 → "4", 1.50 → "1.5".
fn plain_num(x: f64) -> String {
    let mut txt = format!("{x:.2}");
    while txt.ends_with('0') {
        txt.pop();
    }
    if txt.ends_with('.') {
        txt.pop();
    }
    txt
}

/// Plain duration: "instant", "0.25 s", "2 s", "1.5 s".
fn dur(ms: f64) -> String {
    if ms <= 0.0 { "instant".into() } else { format!("{} s", plain_num(ms / 1000.0)) }
}

fn health_led(status: &str) -> LedState {
    match status {
        "pass" | "ok" => LedState::Healthy,
        "warn" => LedState::Armed,
        "fail" | "error" => LedState::Error,
        _ => LedState::Idle,
    }
}

fn look_label(pal: &Value) -> String {
    let (name, label) = (s(pal, "name"), s(pal, "label"));
    if label.is_empty() || label == name { nice(name) } else { label.to_string() }
}

fn list_label(cl: &Value) -> String {
    if s(cl, "label").is_empty() { nice(s(cl, "name")) } else { s(cl, "label").to_string() }
}

/// First value a look sets for `attr` (the `all` entry first).
fn look_attr<'a>(pal: &'a Value, attr: &str) -> Option<&'a Value> {
    let set = pal.get_path("set").and_then(Value::as_map)?;
    set.get("all").into_iter().chain(set.values()).find_map(|a| a.get_path(attr).map(plain))
}

/// Only the current base palette owns a look tile; rhythm/accent aren't base toggles.
fn look_on(cx: &Cx, _st: &LightsState, pal: &Value) -> bool {
    layer_state(cx.m, "base").is_some_and(|v| v.get_path("active").is_some_and(Value::truthy) && s(v, "selection.palette") == s(pal, "name"))
}

/// A knob's shown value: the one being moved, else what the engine has.
fn knob_now(st: &LightsState, kind: &str, item: &str, k: &Value, now: Instant) -> Value {
    st.knob_value(&knob_key(kind, item, s(k, "target")), now).cloned().or_else(|| k.get_path("value").cloned()).unwrap_or_default()
}

/// Colour of a look's face: its colour knob as moved, else its colour (stream colours follow the
/// live stream palette).
fn look_color(cx: &Cx, st: &LightsState, pal: &Value) -> Option<Color32> {
    let name = s(pal, "name");
    if let Some(k) = list(pal, "knobs").iter().find(|k| s(k, "kind") == "color" && s(k, "target").ends_with(".color")) {
        let v = knob_now(st, "look", name, k, cx.now);
        if let Some(c) = v.as_str().and_then(se_ui_kit::theme::hex) {
            return Some(c);
        }
    }
    let v = look_attr(pal, "color")?;
    if let Some(slot) = v.as_str().and_then(|s| s.strip_prefix("stream:")) {
        return Some(cx.m.get(&format!("palette.{slot}")).and_then(color_of).unwrap_or(cx.t.accent));
    }
    color_of(v)
}

/// What a look does, in a few words ("Stream colors · 80% bright").
fn look_words(pal: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if look_attr(pal, "color").and_then(Value::as_str).is_some_and(|c| c.starts_with("stream:")) {
        parts.push("Stream colors".into());
    }
    if let Some(i) = look_attr(pal, "intensity").and_then(Value::as_f64) {
        parts.push(format!("{} bright", pct(i as f32)));
    }
    if look_attr(pal, "pan").is_some() || look_attr(pal, "tilt").is_some() {
        parts.push("Points the moving lights".into());
    }
    parts.join(" · ")
}

// ---------------------------------------------------------------- local widgets

/// One line of text, cut with "…" when wider than `max_w`.
fn one_line(ui: &Ui, text: &str, font: egui::FontId, color: Color32, max_w: f32) -> Arc<Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = TextWrapping::truncate_at_width(max_w.max(8.0));
    ui.painter().layout_job(job)
}

/// Horizontal 0..1 slider: track, accent fill, round knob.
fn level(ui: &mut Ui, t: &Theme, width: f32, value: &mut f32) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(width.max(40.0), 24.0), Sense::click_and_drag());
    let track = Rect::from_center_size(rect.center(), Vec2::new(rect.width() - 18.0, 6.0));
    if let Some(p) = resp.interact_pointer_pos()
        && (resp.dragged() || resp.clicked())
    {
        let v = ((p.x - track.left()) / track.width()).clamp(0.0, 1.0);
        if (v - *value).abs() > f32::EPSILON {
            *value = v;
            resp.mark_changed();
        }
    }
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        p.rect_filled(track, CornerRadius::same(3), t.inset);
        let x = track.left() + track.width() * value.clamp(0.0, 1.0);
        p.rect_filled(Rect::from_min_max(track.min, Pos2::new(x, track.bottom())), CornerRadius::same(3), t.accent);
        let active = resp.hovered() || resp.dragged();
        p.circle(Pos2::new(x, rect.center().y), if active { 9.0 } else { 8.0 }, t.fg, Stroke::new(2.0, if active { t.accent } else { t.border }));
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// One line of text as wide as it needs (at most `max_w`, then "…" with the full text on hover).
fn line_text(ui: &mut Ui, text: &str, font: egui::FontId, color: Color32, max_w: f32) -> egui::Response {
    let g = one_line(ui, text, font, color, max_w);
    let elided = g.elided;
    let (r, resp) = ui.allocate_exact_size(g.size(), Sense::hover());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
    ui.painter().galley_with_override_text_color(r.min, g, color);
    if elided { resp.on_hover_text(text) } else { resp }
}

/// A look tile: its colour on top (or an icon), name and a few words below. `on` = running
/// (accent border and an "ON" chip), `selected` = its knobs are shown (accent tint).
#[allow(clippy::too_many_arguments)]
fn tile(ui: &mut Ui, t: &Theme, size: Vec2, face: Option<Color32>, label: &str, sub: &str, on: bool, selected: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, on, format!("{label} look")));
    if ui.is_rect_visible(rect) {
        let hovered = resp.hovered();
        let fill = if resp.is_pointer_button_down_on() {
            mix(t.surface_hi, Color32::BLACK, 0.12)
        } else if selected {
            mix(t.surface_hi, t.accent, 0.12)
        } else if hovered {
            mix(t.surface_hi, t.fg, 0.05)
        } else {
            t.surface_hi
        };
        let edge = if on {
            t.accent
        } else if hovered {
            mix(t.border, t.fg, 0.3)
        } else {
            t.border
        };
        let p = ui.painter();
        p.rect(rect, CornerRadius::same(radius::TILE), fill, Stroke::new(if on { 2.0 } else { 1.0 }, edge), StrokeKind::Inside);
        let face_rect = Rect::from_min_size(rect.min + Vec2::splat(6.0), Vec2::new(rect.width() - 12.0, rect.height() - 6.0 - TILE_TEXT_H));
        let cr = CornerRadius::same(radius::TILE - 3);
        match face {
            Some(c) => {
                p.rect_filled(face_rect, cr, if on { c } else { mix(t.surface_hi, c, 0.8) });
            }
            None => {
                p.rect_filled(face_rect, cr, mix(t.surface, t.cyan, 0.14));
                p.text(face_rect.center(), Align2::CENTER_CENTER, CROSSHAIRS, font((face_rect.height() * 0.42).clamp(20.0, 34.0)), t.cyan);
            }
        }
        if on {
            let g = p.layout_no_wrap("ON".into(), font_bold(10.5), t.fg);
            let chip = Rect::from_min_size(Pos2::new(face_rect.right() - g.size().x - 20.0, face_rect.top() + 6.0), Vec2::new(g.size().x + 14.0, 18.0));
            p.rect_filled(chip, CornerRadius::same(radius::PILL), t.bg.gamma_multiply(0.85));
            p.galley_with_override_text_color(chip.center() - g.size() / 2.0, g, t.fg);
        }
        let max_w = rect.width() - 20.0;
        let x = rect.left() + 10.0;
        let name = one_line(ui, label, font_semibold(type_scale::BODY), t.fg, max_w);
        let name_y = if sub.is_empty() { face_rect.bottom() + (TILE_TEXT_H - name.size().y) / 2.0 } else { face_rect.bottom() + 8.0 };
        ui.painter().galley_with_override_text_color(Pos2::new(x, name_y), name, t.fg);
        if !sub.is_empty() {
            let sub = one_line(ui, sub, font(type_scale::SMALL), t.text_dim, max_w);
            ui.painter().galley_with_override_text_color(Pos2::new(x, face_rect.bottom() + 28.0), sub, t.text_dim);
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Tile size for `n` tiles in a row of `width`: as many columns as fit (never more than there
/// are tiles), widened to fill the row up to `TILE_MAX_W`; the face grows with the width.
fn tile_size(width: f32, n: usize) -> Vec2 {
    let gap = spacing::M;
    let fit = ((width + gap) / (TILE_MIN_W + gap)).floor().max(2.0);
    let cols = fit.min(n.max(1) as f32);
    let w = ((width - gap * (cols - 1.0)) / cols).floor().min(TILE_MAX_W);
    Vec2::new(w, 6.0 + (w * 0.34).clamp(44.0, 96.0) + TILE_TEXT_H)
}

/// A clickable row inside a cue list: step number, name, timing.
fn step_row(ui: &mut Ui, t: &Theme, id: &str, label: &str, timing: &str, current: bool) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 34.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if current {
            p.rect_filled(rect, CornerRadius::same(6), mix(t.surface_hi, t.accent, 0.2));
        } else if resp.hovered() {
            p.rect_filled(rect, CornerRadius::same(6), mix(t.surface_hi, t.fg, 0.06));
        }
        p.text(
            Pos2::new(rect.left() + 10.0, rect.center().y),
            Align2::LEFT_CENTER,
            id,
            font_mono(type_scale::SMALL),
            if current { t.accent } else { t.text_faint },
        );
        let timing = one_line(ui, timing, font(type_scale::SMALL), t.text_dim, (rect.width() * 0.5).max(60.0));
        let tx = rect.right() - 10.0 - timing.size().x;
        let name = one_line(ui, label, font_medium(type_scale::SMALL + 1.0), t.fg, (tx - rect.left() - 52.0).max(40.0));
        let p = ui.painter();
        p.galley_with_override_text_color(Pos2::new(rect.left() + 40.0, rect.center().y - name.size().y / 2.0), name, t.fg);
        p.galley_with_override_text_color(Pos2::new(tx, rect.center().y - timing.size().y / 2.0), timing, t.text_dim);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn progress_bar(ui: &mut Ui, t: &Theme, width: f32, frac: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 4.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(2), t.inset);
    p.rect_filled(Rect::from_min_size(rect.min, Vec2::new(rect.width() * frac.clamp(0.0, 1.0), rect.height())), CornerRadius::same(2), t.accent);
}

/// Two columns side by side (`left` wide); both closures get a top-down Ui of their width and
/// the shared mutable state `s` in turn.
fn columns<S>(ui: &mut Ui, left: f32, s: &mut S, body_l: impl FnOnce(&mut Ui, &mut S), body_r: impl FnOnce(&mut Ui, &mut S)) {
    let w = ui.available_width();
    let right = (w - left - spacing::L).max(200.0);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = spacing::L;
        ui.allocate_ui_with_layout(Vec2::new(left, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_width(left);
            body_l(ui, s);
        });
        ui.allocate_ui_with_layout(Vec2::new(right, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_width(right);
            body_r(ui, s);
        });
    });
}

/// Knob controls of one look / cue list: stacked, or side by side in cells of at least
/// `cell_min` wide when there's room (wide screens).
#[allow(clippy::too_many_arguments)]
fn knob_rows(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>, kind: &'static str, item: &str, knob_list: &[Value], cell_min: f32) {
    let w = ui.available_width();
    let cols = (((w + spacing::XL) / (cell_min + spacing::XL)).floor() as usize).clamp(1, knob_list.len().max(1));
    let cw = ((w - spacing::XL * (cols as f32 - 1.0)) / cols as f32).floor();
    for (row, chunk) in knob_list.chunks(cols).enumerate() {
        if row > 0 {
            ui.add_space(spacing::S);
        }
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = spacing::XL;
            for k in chunk {
                ui.allocate_ui_with_layout(Vec2::new(cw, 0.0), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(cw);
                    let target = s(k, "target");
                    let mut v = knob_now(st, kind, item, k, cx.now);
                    let moved = knobs::control(ui, cx.t, knob_key(kind, item, target), k, &mut v);
                    st.knob_moved(out, kind, item, target, v, moved, cx.now);
                });
            }
        });
    }
}

/// On air: "Your viewers will see this" with the button that really starts it. True = go.
fn confirm_on_air(ui: &mut Ui, t: &Theme, st: &mut LightsState, what: &str, action: &str) -> bool {
    let go = widgets::callout(ui, t, Tone::Danger, icon::LIVE, "Your viewers will see this", &format!("You're on air, so {what} right now."), Some(action));
    if go || widgets::button_ex(ui, t, None, "Cancel", Kind::Ghost, Size::Small, 0.0, true).clicked() {
        st.confirm = None;
    }
    go
}

// ---------------------------------------------------------------- master

fn default_controls(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>) {
    let t = cx.t;
    ui.horizontal_wrapped(|ui| {
        if widgets::button_ex(ui, t, Some(icon::LIGHT), "Default lights", Kind::Primary, Size::Small, 0.0, cx.m.connected).clicked() {
            if cx.on_air {
                st.confirm = Some(Start::Default);
            } else {
                restore_default(st, out);
            }
        }
        widgets::hint(ui, t, "Restore configured idle room lighting and pause AUTO LIGHTS.");
        let (label, color) = match cx.m.get("lights.auto") {
            Some(Value::Bool(true)) => ("AUTO LIGHTS on", t.green),
            Some(Value::Bool(false)) => ("AUTO LIGHTS paused", t.yellow),
            _ => ("AUTO LIGHTS status unknown", t.text_dim),
        };
        widgets::badge(ui, t, label, color);
    });
    if st.confirm == Some(Start::Default) {
        ui.add_enabled_ui(cx.m.connected, |ui| {
            if confirm_on_air(ui, t, st, "restoring configured idle room lighting pauses AUTO LIGHTS and clears active lighting overrides", "Default lights") {
                restore_default(st, out);
            }
        });
    }
}

fn master_card(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>) {
    let t = cx.t;
    let mut safe = false;
    widgets::titled(
        ui,
        t,
        "Master",
        "Works on top of every look and cue list.",
        |ui| {
            safe = widgets::hold_button(ui, t, "Panic / safe look", t.bright_red, 0.8);
        },
        |ui| {
            ui.set_width(ui.available_width());
            default_controls(cx, st, ui, out);
            ui.add_space(spacing::S);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = spacing::S;
                master_controls(cx, st, ui, out, (ui.available_width() * 0.32).clamp(180.0, 560.0));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| health_pill(cx, ui));
            });
            ui.add_space(spacing::S);
            flash_safety(cx, ui);
            output_safety(cx, ui);
            if panic_latched(cx.m) {
                widgets::badge(ui, t, "Panic latched · layer starts blocked", t.bright_red);
                if widgets::button_ex(ui, t, None, "Release safe look / reset panic", Kind::Secondary, Size::Small, 0.0, cx.m.connected).clicked() {
                    out.push(act("lights.release", Value::map().with("cuelist", "safe")));
                }
            }
        },
    );
    if safe {
        out.push(act("lights.panic", Value::Null));
    }
}

/// Overall brightness (`lights.master`) and the Blackout switch (`lights.blackout`), on one row.
fn master_controls(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>, slider_w: f32) {
    let (m, t, now) = (cx.m, cx.t, cx.now);
    ui.label(RichText::new("Brightness").font(font_medium(type_scale::BODY)).color(t.fg));
    let key = "@lights.master";
    let mut gm = st.edited(key, now).and_then(Value::as_f32).or_else(|| m.get("lights.master").and_then(Value::as_f32)).unwrap_or(1.0);
    if level(ui, t, slider_w, &mut gm).on_hover_text("How bright all your lights are overall").changed() {
        st.edit(key, Value::from(gm), now);
        out.push(Op::Set { address: "lights.master".into(), value: Value::from(gm) });
    }
    ui.add_sized([48.0, 24.0], egui::Label::new(RichText::new(pct(gm)).font(font_mono(type_scale::BODY)).color(t.fg)));
    ui.add_space(spacing::L);
    let bkey = "@lights.blackout";
    let mut blackout = st.edited(bkey, now).map(Value::truthy).unwrap_or_else(|| m.b("lights.blackout"));
    if widgets::toggle(ui, t, &mut blackout).on_hover_text("Black out the rendered output. Hardware receives blackout only while armed and connected; disarmed fixtures may retain their last frame.").changed() {
        st.edit(bkey, Value::Bool(blackout), now);
        out.push(Op::Set { address: "lights.blackout".into(), value: Value::Bool(blackout) });
    }
    ui.label(RichText::new("Blackout").font(font_medium(type_scale::BODY)).color(t.fg));
    if blackout {
        widgets::badge(ui, t, "Preview blacked out", t.bright_red);
    }
}

/// Transport health is not proof of the fixtures' physical state.
fn health_pill(cx: &Cx, ui: &mut Ui) {
    let (status, detail) = cx.m.get("health.dmx").map(|h| (s(h, "status"), s(h, "detail"))).unwrap_or(("", ""));
    let text = if cx.rig.armed == Some(false) { "Preview only" } else {
        match health_led(status) {
            LedState::Healthy => "Transport ready",
            LedState::Armed => "Check lighting setup",
            LedState::Error => "Transport unavailable",
            _ => "Transport status unknown",
        }
    };
    let mut tip: Vec<String> = cx
        .rig
        .outputs
        .iter()
        .filter(|o| o.get_path("enabled").is_none_or(Value::truthy))
        .map(|o| {
            let word = match health_led(s(o, "status")) {
                LedState::Healthy => "working",
                LedState::Error => "not connected",
                _ => "needs a look",
            };
            let kind = match s(o, "kind") {
                "enttec_pro" | "enttec" | "enttec_open" => "USB lighting box".to_string(),
                "sacn" | "artnet" => "Network lights".to_string(),
                other => nice(other),
            };
            format!("{kind}: {word}")
        })
        .collect();
    if !detail.is_empty() {
        tip.push(format!("\nDetails: {detail}"));
    }
    let r = widgets::pill(ui, cx.t, icon::DEVICE, text, health_led(status));
    if !tip.is_empty() {
        r.on_hover_text(tip.join("\n"));
    }
}

/// The flash limiter in plain words (read-only; set in the lights setup).
fn flash_safety(cx: &Cx, ui: &mut Ui) {
    let t = cx.t;
    let safety = cx.rig.safety.as_ref();
    let hz = safety.and_then(|s| num(s, "max_flash_hz")).unwrap_or(3.0);
    let cap = safety.and_then(|s| num(s, "max_intensity")).unwrap_or(1.0);
    let mut line = if hz <= 0.0 {
        "DMX flash limiter: detected flashes are held.".to_string()
    } else {
        format!("DMX flash limiter: at most {} detected flash onset{} per second.", plain_num(hz), if hz == 1.0 { "" } else { "s" })
    };
    if cap < 0.999 {
        line.push_str(&format!(" Output intensity cap: {}.", pct(cap as f32)));
    }
    let strobe = if safety.is_some_and(|s| s.get_path("strobe").and_then(Value::as_str) == Some("block")) {
        "Built-in strobes on your lights are kept off."
    } else {
        "Built-in strobes on your lights are slowed down to match."
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon::CHECK).size(type_scale::SMALL + 1.0).color(t.green));
        widgets::hint(ui, t, &line).on_hover_text(format!("{strobe}\nThe limiter measures commanded DMX levels, not physical light. Autonomous fixture programs and shutter behavior still require hardware commissioning."));
        let limiting = cx.output.and_then(|o| o.get_path("limiter.active")).is_some_and(Value::truthy);
        if limiting {
            widgets::badge(ui, t, "Softening flashes now", t.yellow).on_hover_text("Some flashes are too fast right now, so they're being softened.");
        }
    });
}

// ---------------------------------------------------------------- looks

fn layer_state<'a>(m: &'a Model, slot: &str) -> Option<&'a Value> {
    m.get(&format!("lights.layer.{slot}.state")).filter(|v| v.get_path("active").is_some())
        .or_else(|| m.q("lights.layers").and_then(|v| v.get_path(slot)))
}

fn panic_latched(m: &Model) -> bool {
    m.get("lights.panic_latched").or_else(|| m.q("lights.layers").and_then(|v| v.get_path("panic_latched"))).is_some_and(Value::truthy)
}

fn layer_busy(m: &Model, st: &LightsState, slot: &str, now: Instant) -> bool {
    st.edited(&format!("layer:{slot}:busy"), now).is_some_and(Value::truthy)
        || layer_state(m, slot).and_then(|v| v.get_path("pending")).is_some_and(|v| !v.is_null())
}

fn select_look(st: &mut LightsState, out: &mut Vec<Op>, name: &str, now: Instant) {
    out.push(act("lights.layer.select", Value::map().with("layer", "base").with("palette", name).with("owner", format!("ui:palette:{name}"))));
    st.edit("layer:base:busy", Value::Bool(true), now);
}

fn restore_default(st: &mut LightsState, out: &mut Vec<Op>) {
    // Discard pending knob sends and optimistic lighting values; show the engine echo.
    st.knobs.clear();
    st.edits.clear();
    st.confirm = None;
    out.push(act("lights.default", Value::Null));
}

fn layer_args(slot: &str, state: &Value) -> Value {
    let mut args = Value::map().with("layer", slot);
    if let Some(owner) = state.get_path("owner").and_then(Value::as_str) {
        args = args.with("owner", owner);
    }
    args
}

fn release_layer(st: &mut LightsState, out: &mut Vec<Op>, slot: &str, state: &Value, now: Instant) {
    out.push(act("lights.layer.release", layer_args(slot, state)));
    st.edit(&format!("layer:{slot}:busy"), Value::Bool(true), now);
}

fn output_safety(cx: &Cx, ui: &mut Ui) {
    let t = cx.t;
    let (label, color) = match cx.rig.armed {
        Some(true) => ("Output armed in rig configuration", t.yellow),
        Some(false) => ("Output disarmed · preview only", t.text_dim),
        None => ("Output arming unknown", t.yellow),
    };
    ui.horizontal_wrapped(|ui| {
        widgets::badge(ui, t, label, color).on_hover_text("Hardware sends require [output] armed=true and an enabled transport. Disarming stops transport; hardware may retain its last frame. This is not blackout.");
        let enabled = cx.rig.outputs.iter().filter(|v| v.get_path("enabled").is_some_and(Value::truthy)).count();
        widgets::hint(ui, t, &format!("{enabled} transport(s) enabled"));
        if cx.rig.fixtures.is_empty() {
            widgets::hint(ui, t, "No fixtures patched");
        } else if cx.rig.unverified.is_empty() {
            widgets::badge(ui, t, "Fixture layout verified", t.green);
        } else {
            widgets::badge(ui, t, "Fixture layout unverified", t.yellow).on_hover_text(format!("Unverified fixture positions/orientation: {}. Stage preview is approximate; verify the real layout before arming.", cx.rig.unverified.join(", ")));
        }
    });
    if cx.rig.armed == Some(false) {
        widgets::hint(ui, t, "Preview continues while disarmed. Fixtures may hold their last hardware frame.");
    }
}

fn foundation_card(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>) {
    widgets::titled(ui, cx.t, "Musical layers", "Base → rhythm → accent. Content is authored; no looks are generated automatically.", |_| {}, |ui| {
        if let Some(clock) = cx.m.q("audio.mix").and_then(|v| v.get_path("analysis.beat")) {
            widgets::hint(ui, cx.t, &format!("Clock: {} · {} BPM · {}", s(clock, "source"), num(clock, "bpm").map(plain_num).unwrap_or_else(|| "—".into()), s(clock, "status")));
        } else {
            widgets::hint(ui, cx.t, "Clock status unavailable");
        }
        for slot in ["base", "rhythm", "accent"] {
            ui.push_id(slot, |ui| {
                let state = layer_state(cx.m, slot);
                let active = state.is_some_and(|v| v.get_path("active").is_some_and(Value::truthy));
                let pending = layer_busy(cx.m, st, slot, cx.now);
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(nice(slot)).font(font_semibold(type_scale::BODY)).color(cx.t.fg));
                    widgets::badge(ui, cx.t, if pending { "Pending" } else if active { "Active" } else { "Empty" }, if active { cx.t.green } else { cx.t.text_dim });
                    if let Some(state) = state.filter(|_| active) {
                        let refs: Vec<String> = ["selection.palette", "selection.cuelist"].iter().map(|k| s(state, k)).filter(|v| !v.is_empty()).map(str::to_string).chain(list(state, "selection.effects").iter().map(text)).collect();
                        widgets::hint(ui, cx.t, &refs.join(" · "));
                        if let Some(ms) = num(state, "remaining_ms") {
                            widgets::hint(ui, cx.t, &format!("{} remaining", dur(ms)));
                        }
                        let vibe = s(state, "controls.vibe");
                        if !vibe.is_empty() {
                            widgets::hint(ui, cx.t, &format!("Vibe (authored): {vibe}"));
                        }
                    }
                });
                let empty = Value::Null;
                let state = state.unwrap_or(&empty);
                ui.add_enabled_ui(cx.m.connected && active && !pending && !panic_latched(cx.m), |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for (field, label, range) in [
                            ("energy", "Energy", 0.0..=1.0),
                            ("rhythm", "Beat period ×", 0.125..=8.0),
                            ("brightness", "Brightness", 0.0..=1.0),
                        ] {
                            let key = format!("layer:{slot}:{}:{field}", s(state, "owner"));
                            let mut value = st.edited(&key, cx.now).and_then(Value::as_f32).or_else(|| num(state, &format!("controls.{field}")).map(|v| v as f32)).unwrap_or(1.0);
                            let response = ui.add(egui::Slider::new(&mut value, range).text(label));
                            if field == "rhythm" {
                                response.clone().on_hover_text("Beats per effect cycle: 2 doubles the period, 0.5 halves it. This does not change tempo.");
                            }
                            if response.changed() {
                                st.edit(&key, Value::from(value), cx.now);
                                out.push(act("lights.layer.set", layer_args(slot, state).with(field, value)));
                            }
                        }
                        if widgets::button_ex(ui, cx.t, Some(icon::STOP), "Release", Kind::Secondary, Size::Small, 0.0, true).clicked() {
                            release_layer(st, out, slot, state, cx.now);
                        }
                    });
                    let key = format!("layer:{slot}:{}:coverage", s(state, "owner"));
                    let current = st.edited(&key, cx.now).or_else(|| state.get_path("controls.coverage"));
                    let mut selected: Vec<String> = current.and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(Value::as_str).map(str::to_string).collect();
                    // Keep the main controls at fixture/group granularity. Retain
                    // any explicitly selected pixel targets from advanced authoring.
                    let mut targets = cx.rig.targets.clone();
                    for target in &selected {
                        if !targets.contains(target) { targets.push(target.clone()); }
                    }
                    ui.horizontal_wrapped(|ui| {
                        widgets::hint(ui, cx.t, "Coverage");
                        for target in &targets {
                            let mut checked = selected.contains(target);
                            if widgets::toggle(ui, cx.t, &mut checked).on_hover_text(target).changed() {
                                if checked {
                                    if target == "all" { selected.clear(); } else { selected.retain(|v| v != "all"); }
                                    selected.push(target.clone());
                                } else {
                                    selected.retain(|v| v != target);
                                }
                                let coverage = Value::from(selected.clone());
                                st.edit(&key, coverage.clone(), cx.now);
                                out.push(act("lights.layer.set", layer_args(slot, state).with("coverage", coverage)));
                            }
                            ui.label(RichText::new(target).color(cx.t.text_dim));
                        }
                    });
                });
                ui.add_space(spacing::S);
            });
        }
    });
}

fn main_light_card(cx: &Cx, ui: &mut Ui) {
    let Some(state) = cx.m.get("lights.main_light.state") else { return };
    let phase = s(state, "phase");
    if !state.get_path("configured").is_some_and(Value::truthy) && phase != "error" { return; }
    widgets::titled(ui, cx.t, "Main light", "Automatic handoff follows sent DMX, never preview-only lighting.", |_| {}, |ui| {
        ui.horizontal_wrapped(|ui| {
            widgets::badge(ui, cx.t, phase, if phase == "error" { cx.t.yellow } else { cx.t.text_dim });
            let power = match state.get_path("on") {
                Some(Value::Bool(true)) => "Last verified: ON",
                Some(Value::Bool(false)) => "Last verified: OFF",
                _ => "Relay state unknown",
            };
            ui.label(power);
            ui.label(format!("{} · {}", s(state, "host"), s(state, "mac")));
        });
        widgets::hint(ui, cx.t, s(state, "detail"));
        widgets::hint(ui, cx.t, "Global blackout, zero intended brightness, disarm or output stop restore the main light after takeover. Intentional waveform darkness keeps it off.");
    });
}

fn capabilities_card(cx: &Cx, ui: &mut Ui) {
    let Some(rig) = cx.m.q("lights.rig") else { return };
    widgets::titled(ui, cx.t, "Fixture capabilities", "Independent fixture/pixel targets and the complete physical channel map.", |_| {}, |ui| {
        widgets::hint(ui, cx.t, "Operator = direct attribute; managed = encoder-owned gate; fixed = intentional mode byte; blocked = unavailable under current safety/calibration.");
        for fixture in list(rig, "fixtures") {
            let id = s(fixture, "id");
            let address = num(fixture, "address").unwrap_or(0.0);
            let footprint = num(fixture, "footprint").unwrap_or(0.0);
            let label = format!("{id} · {} · U{}:{}–{}", s(fixture, "mode"), plain_num(num(fixture, "universe").unwrap_or(0.0)), plain_num(address), plain_num(address + footprint - 1.0));
            ui.collapsing(label, |ui| {
                widgets::hint(ui, cx.t, s(fixture, "notes"));
                let heads = strs(fixture, "heads");
                if heads.len() > 1 {
                    widgets::hint(ui, cx.t, &format!("{} independently targeted pixels: {}", heads.len(), heads.join(", ")));
                }
                egui::Grid::new(("fixture_channel_map", id)).striped(true).show(ui, |ui| {
                    for heading in ["Channel / DMX", "Target", "Function", "Ownership", "State attribute"] {
                        ui.strong(heading);
                    }
                    ui.end_row();
                    for channel in list(fixture, "channel_map") {
                        ui.label(format!("{} / {}", plain_num(num(channel, "channel").unwrap_or(0.0)), plain_num(num(channel, "address").unwrap_or(0.0))));
                        ui.label(s(channel, "head"));
                        let function = if s(channel, "control") == "fixed" {
                            format!("{} · byte {}", s(channel, "label"), plain_num(num(channel, "default").unwrap_or(0.0)))
                        } else { s(channel, "label").to_string() };
                        let slots = list(channel, "slots").iter().map(|slot| format!("Index {} · {} → DMX byte {}", plain_num(num(slot, "index").unwrap_or(0.0)), s(slot, "name"), plain_num(num(slot, "value").unwrap_or(0.0)))).collect::<Vec<_>>().join("\n");
                        ui.label(function).on_hover_text(slots);
                        let control = s(channel, "control");
                        widgets::badge(ui, cx.t, control, if control == "blocked" { cx.t.yellow } else { cx.t.text_dim }).on_hover_text(s(channel, "restriction"));
                        ui.small(s(channel, "state_address")).on_hover_text(s(channel, "restriction"));
                        ui.end_row();
                    }
                });
            });
        }
    });
}

/// Turn a base look on (asking first on air) or release exactly its current generation.
fn toggle_look(cx: &Cx, st: &mut LightsState, out: &mut Vec<Op>, name: &str, on: bool) {
    if !cx.m.connected || layer_busy(cx.m, st, "base", cx.now) || panic_latched(cx.m) {
        return;
    }
    if on {
        if let Some(current) = layer_state(cx.m, "base")
            && s(current, "selection.palette") == name
        {
            release_layer(st, out, "base", current, cx.now);
        }
    } else if cx.on_air {
        st.confirm = Some(Start::Look(name.to_string()));
    } else {
        select_look(st, out, name, cx.now);
    }
}

fn looks_card(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>) {
    let t = cx.t;
    let pals = cx.m.q_list("lights.palettes");
    let on: Vec<&Value> = pals.iter().filter(|p| look_on(cx, st, p)).collect();
    let sub = match on.len() {
        0 => "Tap a look to turn it on. Tap it again to turn it off.".to_string(),
        1 => format!("{} is on · tap a look to turn it on or off", look_label(on[0])),
        n => format!("{n} looks on · tap a look to turn it on or off"),
    };
    let mut all_off = false;
    let release_ready = cx.m.connected && !layer_busy(cx.m, st, "base", cx.now);
    widgets::titled(
        ui,
        t,
        "Looks",
        &sub,
        |ui| {
            if !on.is_empty() {
                all_off = widgets::button_ex(ui, t, Some(icon::POWER), "Release base", Kind::Secondary, Size::Small, 0.0, release_ready).clicked();
            }
        },
        |ui| {
            ui.set_width(ui.available_width());
            if pals.is_empty() {
                widgets::empty_state(
                    ui,
                    t,
                    icon::PALETTE,
                    "No looks yet",
                    "Add a look file under lights/palettes/ with the colors and brightness you want. Looks only run when you turn them on.",
                    None,
                );
                return;
            }
            if let Some(Start::Look(name)) = st.confirm.clone()
                && let Some(pal) = pals.iter().find(|p| s(p, "name") == name)
            {
                let ready = !panic_latched(cx.m) && !layer_busy(cx.m, st, "base", cx.now) && cx.m.connected;
                if ui.add_enabled_ui(ready, |ui| confirm_on_air(ui, t, st, &format!("“{}” becomes your base look", look_label(pal)), "Turn it on")).inner {
                    select_look(st, out, &name, cx.now);
                }
                ui.add_space(spacing::M);
            }
            let size = tile_size(ui.available_width(), pals.len());
            let shown = shown_look(cx, st, pals).map(|p| s(p, "name").to_string());
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::splat(spacing::M);
                for pal in pals {
                    let name = s(pal, "name");
                    let is_on = look_on(cx, st, pal);
                    let face = if look_attr(pal, "color").is_none() && (look_attr(pal, "pan").is_some() || look_attr(pal, "tilt").is_some()) {
                        None
                    } else {
                        Some(look_color(cx, st, pal).unwrap_or_else(|| {
                            let i = look_attr(pal, "intensity").and_then(Value::as_f32).unwrap_or(1.0);
                            mix(t.inset, t.fg_bright, i.clamp(0.0, 1.0) * 0.9)
                        }))
                    };
                    let label = look_label(pal);
                    let resp = ui.add_enabled_ui(cx.m.connected && !panic_latched(cx.m) && !layer_busy(cx.m, st, "base", cx.now), |ui| tile(ui, t, size, face, &label, &look_words(pal), is_on, shown.as_deref() == Some(name))).inner;
                    // a corner button shows the look's knobs without turning it on
                    let adj = Rect::from_min_size(resp.rect.min + Vec2::splat(10.0), Vec2::splat(28.0));
                    let mut child = ui.new_child(UiBuilder::new().max_rect(adj).id_salt(("look-adjust", name)));
                    let adjust = widgets::icon_button(&mut child, t, icon::SLIDERS, "Adjust it (without turning it on)").clicked();
                    let tip = match (is_on, cx.on_air) {
                        (true, _) => "On · tap to turn it off",
                        (false, true) => "Tap to try it on air (asks first)",
                        (false, false) => "Tap to turn it on",
                    };
                    let words = look_words(pal);
                    let tip = if words.is_empty() { format!("{label}\n{tip}") } else { format!("{label}: {words}\n{tip}") };
                    if adjust {
                        st.look = Some(name.to_string());
                    } else if resp.on_hover_text(tip).clicked() {
                        st.look = Some(name.to_string());
                        toggle_look(cx, st, out, name, is_on);
                    }
                }
            });
        },
    );
    if all_off && let Some(current) = layer_state(cx.m, "base") {
        release_layer(st, out, "base", current, cx.now);
    }
}

/// The look whose knobs are shown: the chosen one, else the first one on, else the first.
fn shown_look<'a>(cx: &Cx, st: &LightsState, pals: &'a [Value]) -> Option<&'a Value> {
    st.look.as_deref().and_then(|n| pals.iter().find(|p| s(p, "name") == n)).or_else(|| pals.iter().find(|p| look_on(cx, st, p))).or_else(|| pals.first())
}

fn adjust_card(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>) {
    let (m, t) = (cx.m, cx.t);
    let pals = m.q_list("lights.palettes");
    let Some(pal) = shown_look(cx, st, pals) else { return };
    let name = s(pal, "name").to_string();
    let label = look_label(pal);
    let is_on = look_on(cx, st, pal);
    let knob_list = list(pal, "knobs");
    let sub = match (is_on, knob_list.is_empty()) {
        (true, false) => "On now · changes show on your lights as you make them",
        (true, true) => "On now",
        (false, false) => "Off · adjust it now, it looks like this when it goes on",
        (false, true) => "Off",
    };
    let (mut toggle, mut quick, mut reset) = (false, false, false);
    let defaults: Vec<(&str, &Value)> = knob_list.iter().filter_map(|k| knobs::default_of(k).map(|d| (s(k, "target"), d))).collect();
    let changed = knob_list.iter().any(|k| knobs::default_of(k).is_some_and(|d| !same(&knob_now(st, "look", &name, k, cx.now), d)));
    let toggle_ready = cx.m.connected && !panic_latched(cx.m) && !layer_busy(cx.m, st, "base", cx.now);
    widgets::titled(
        ui,
        t,
        &label,
        sub,
        |ui| {
            let (icon_, lbl, kind, tip) = match (is_on, cx.on_air) {
                (true, _) => (icon::POWER, "Turn off", Kind::Secondary, "Take this look off your lights"),
                (false, true) => (icon::PLAY, "Try it on air", Kind::Live, "You're on air: your viewers will see it"),
                (false, false) => (icon::PLAY, "Turn on", Kind::Primary, "You're off air: only you see it"),
            };
            toggle = widgets::button_ex(ui, t, Some(icon_), lbl, kind, Size::Small, 0.0, toggle_ready).on_hover_text(tip).clicked();
            quick = widgets::button_ex(ui, t, Some(icon::BOLT), "Make a quick effect", Kind::Secondary, Size::Small, 0.0, true)
                .on_hover_text("Puts this look on a pad: one tap on Overview or your Stream Deck turns it on and off.")
                .clicked();
        },
        |ui| {
            ui.set_width(ui.available_width());
            if knob_list.is_empty() {
                widgets::hint(ui, t, "This look has nothing to adjust. Ask us if you'd like a knob for it (its color or brightness, say).");
                return;
            }
            knob_rows(cx, st, ui, out, "look", &name, knob_list, 360.0);
            if changed {
                ui.add_space(spacing::S);
                reset = widgets::button_ex(ui, t, Some(icon::UNDO), "Reset knobs", Kind::Ghost, Size::Small, 0.0, true)
                    .on_hover_text("Back to how the look was made")
                    .clicked();
            }
        },
    );
    if toggle {
        toggle_look(cx, st, out, &name, is_on);
    }
    if reset {
        for (target, d) in defaults {
            st.knob_moved(out, "look", &name, target, d.clone(), Moved::Done, cx.now);
        }
    }
    if quick {
        let taken: Vec<String> = m
            .q_list("presets")
            .iter()
            .map(|p| s(p, "name").to_string())
            .chain(m.q_list("project.files").iter().filter(|f| s(f, "kind") == "presets").map(|f| s(f, "name").to_string()))
            .collect();
        let id = crate::views::quick_edit::free_id(&label, &taken);
        let text = crate::views::quick_edit::look_preset(&label, &name);
        out.push(act("project.write", Value::map().with("path", format!("presets/{id}.toml")).with("text", text)));
        st.open_quick = Some(id);
    }
}

/// Numbers compare by value (`1` = `1.0`), colours case-blind.
fn same(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64(), a.as_str(), b.as_str()) {
        (Some(x), Some(y), _, _) => (x - y).abs() < 1e-6,
        (_, _, Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        _ => a == b,
    }
}

// ---------------------------------------------------------------- cue lists

fn cuelists_card(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>) {
    let t = cx.t;
    let lists = cx.m.q_list("lights.cuelists");
    widgets::titled(
        ui,
        t,
        "Manual cue lists (advanced)",
        "Go / Back / Locate use the manual console, separate from musical layer selections.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if lists.is_empty() {
                widgets::empty_state(
                    ui,
                    t,
                    icon::LIST,
                    "No cue lists yet",
                    "Add a cue list under lights/cuelists/ to define the steps you want. Nothing runs until you start it.",
                    None,
                );
                return;
            }
            // lists you step through first, then the one-step ones (engine order within each)
            let mut lists: Vec<&Value> = lists.iter().collect();
            lists.sort_by_key(|cl| list(cl, "cues").len() <= 1);
            let w = ui.available_width();
            if w >= 720.0 {
                let cw = ((w - spacing::M) / 2.0).floor();
                for pair in lists.chunks(2) {
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = spacing::M;
                        for cl in pair {
                            ui.allocate_ui_with_layout(Vec2::new(cw, 0.0), Layout::top_down(Align::Min), |ui| {
                                ui.set_width(cw);
                                cuelist_row(cx, st, ui, out, cl);
                            });
                        }
                    });
                    ui.add_space(spacing::M);
                }
            } else {
                for cl in &lists {
                    cuelist_row(cx, st, ui, out, cl);
                    ui.add_space(spacing::S);
                }
            }
        },
    );
}

fn step_label(c: &Value) -> String {
    let label = s(c, "label");
    if label.is_empty() { format!("Step {}", text(c.get_path("id").unwrap_or(&Value::Null))) } else { label.to_string() }
}

/// "Fades in 3 s, out 1 s · next step 2 s after".
fn cue_timing(c: &Value) -> String {
    let ms = |k: &str| c.get_path(k).filter(|v| !v.is_null()).and_then(Value::as_f64);
    let fade = ms("fade_ms").unwrap_or(0.0);
    let mut s = if fade > 0.0 { format!("Fades in {}", dur(fade)) } else { "Snaps on".to_string() };
    if let Some(o) = ms("fade_out_ms")
        && (o - fade).abs() > 0.5
    {
        s.push_str(&format!(", out {}", dur(o)));
    }
    if let Some(d) = ms("delay_ms").filter(|d| *d > 0.0) {
        s.push_str(&format!(" · waits {} first", dur(d)));
    }
    if let Some(f) = ms("follow_ms") {
        s.push_str(&format!(" · next step {} after", dur(f)));
    }
    if let Some(w) = ms("wait_ms") {
        s.push_str(&format!(" · next step {} after it starts", dur(w)));
    }
    s
}

fn cuelist_row(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>, cl: &Value) {
    let (m, t, now) = (cx.m, cx.t, cx.now);
    let name = s(cl, "name");
    let base = format!("lights.cuelist.{name}");
    let pkey = format!("@{base}.playing");
    let playing = st
        .edited(&pkey, now)
        .map(Value::truthy)
        .or_else(|| m.get(&format!("{base}.playing")).map(Value::truthy))
        .unwrap_or_else(|| cl.get_path("playing").is_some_and(Value::truthy));
    let current = m.get(&format!("{base}.cue")).map(text).filter(|c| !c.is_empty()).or_else(|| cl.get_path("current").filter(|v| !v.is_null()).map(text));
    let next = m.get(&format!("{base}.next")).map(text).filter(|c| !c.is_empty()).or_else(|| cl.get_path("next").filter(|v| !v.is_null()).map(text));
    let cues = list(cl, "cues");
    let multi = cues.len() > 1;
    let cue_name = |id: &str| cues.iter().find(|c| c.get_path("id").map(text).as_deref() == Some(id)).map(step_label).unwrap_or_else(|| format!("Step {id}"));
    let title = list_label(cl);
    let frame = egui::Frame::new()
        .fill(if playing { mix(t.surface, t.accent, 0.07) } else { t.surface })
        .stroke(Stroke::new(1.0, if playing { mix(t.border, t.accent, 0.7) } else { t.border }))
        .corner_radius(radius::TILE)
        .inner_margin(egui::Margin::same(spacing::M as i8));
    // one-step lists are simple looks: their step name only matters when it adds something
    let aside = cues.first().map(step_label).filter(|l| !multi && !l.eq_ignore_ascii_case(&title));
    let args = || Value::map().with("cuelist", name);
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(Vec2::new(10.0, 20.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 4.5, if playing { t.green } else { t.text_faint });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = spacing::XS + 2.0;
                if widgets::button_ex(ui, t, Some(icon::STOP), "Stop", Kind::Secondary, Size::Small, 0.0, playing)
                    .on_hover_text("Stop this cue list; the lights it controls fade back")
                    .clicked()
                {
                    out.push(act("lights.release", args()));
                    st.edit(&pkey, Value::Bool(false), now);
                }
                if multi
                    && widgets::button_ex(ui, t, Some(icon::LEFT), "Back", Kind::Secondary, Size::Small, 0.0, playing)
                        .on_hover_text("Go back one step")
                        .clicked()
                {
                    out.push(act("lights.back", args()));
                }
                let (go_tip, go_kind) = match (playing, multi, cx.on_air) {
                    (false, _, true) => ("You're on air: starting it asks first", Kind::Secondary),
                    (false, _, false) => ("Start this cue list", Kind::Secondary),
                    (true, true, _) => ("Go to the next step", Kind::Primary),
                    (true, false, _) => ("Start it again", Kind::Secondary),
                };
                if widgets::button_ex(ui, t, Some(icon::PLAY), "Go", go_kind, Size::Small, 64.0, true).on_hover_text(go_tip).clicked() {
                    if !playing && cx.on_air {
                        st.confirm = Some(Start::List(name.to_string()));
                    } else {
                        out.push(act("lights.go", args()));
                        st.edit(&pkey, Value::Bool(true), now);
                    }
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    line_text(ui, &title, font_semibold(type_scale::BODY + 1.0), t.fg, ui.available_width());
                    if let Some(a) = &aside {
                        line_text(ui, &format!("·  {a}"), font(type_scale::SMALL + 0.5), t.text_dim, ui.available_width());
                    }
                });
            });
        });
        if st.confirm.as_ref() == Some(&Start::List(name.to_string())) {
            ui.add_space(spacing::S);
            if confirm_on_air(ui, t, st, &format!("“{title}” starts on your lights"), "Start it") {
                out.push(act("lights.go", args()));
                st.edit(&pkey, Value::Bool(true), now);
            }
        }
        let line = match (&current, playing) {
            (Some(c), true) if multi => {
                let n = cues.iter().position(|x| x.get_path("id").map(text).as_deref() == Some(c.as_str())).map(|i| i + 1).unwrap_or(0);
                let nx = next.as_deref().map(|x| format!("  ·  next: {}", cue_name(x))).unwrap_or_default();
                Some(format!("Now: {} ({n} of {}){nx}", cue_name(c), cues.len()))
            }
            (_, true) => Some("Running".to_string()),
            _ if multi => Some(format!("{} steps · starts with {}", cues.len(), cues.first().map(step_label).unwrap_or_default())),
            _ if cues.is_empty() => Some("Empty: no steps yet".into()),
            _ => None,
        };
        if let Some(line) = line {
            ui.add_space(2.0);
            line_text(ui, &line, font(type_scale::SMALL + 0.5), if playing { t.fg } else { t.text_dim }, ui.available_width());
        }
        let (progress, total) = (num(cl, "progress").unwrap_or(0.0) as f32, num(cl, "total_ms").unwrap_or(0.0));
        if playing && total > 0.0 && progress < 1.0 {
            ui.add_space(spacing::XS);
            progress_bar(ui, t, ui.available_width(), progress);
        }
        let knob_list = list(cl, "knobs");
        if !knob_list.is_empty() {
            ui.add_space(spacing::S);
            knob_rows(cx, st, ui, out, "cuelist", name, knob_list, 10_000.0);
        }
        if multi {
            ui.add_space(spacing::XS);
            widgets::details(ui, t, ("lights_steps", name), &format!("All {} steps", cues.len()), |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for c in cues {
                    let id = text(c.get_path("id").unwrap_or(&Value::Null));
                    let is_cur = playing && current.as_deref() == Some(id.as_str());
                    if step_row(ui, t, &id, &step_label(c), &cue_timing(c), is_cur).on_hover_text("Click to jump to this step").clicked() {
                        out.push(act("lights.goto", Value::map().with("cuelist", name).with("cue", id)));
                    }
                }
            });
        }
    });
}

// ---------------------------------------------------------------- stage

/// A head as drawn on the stage: rig layout merged with live output.
#[derive(Clone, Debug)]
struct StageHead {
    id: String,
    pos: [f32; 2],
    rotation: f32,
    kind: String,
    beam: f32,
    moving: bool,
    intensity: f32,
    color: [f32; 3],
    pan: f32,
    tilt: f32,
    limited: bool,
}

fn stage_heads(rig: &Rig, output: Option<&Value>) -> Vec<StageHead> {
    let live: HashMap<&str, &Value> =
        output.map(|o| list(o, "heads")).unwrap_or(&[]).iter().map(|h| (s(h, "id"), h)).filter(|(id, _)| !id.is_empty()).collect();
    rig.heads
        .iter()
        .map(|h| {
            let base = StageHead {
                id: h.id.clone(),
                pos: h.position,
                rotation: h.rotation,
                kind: h.kind.clone(),
                beam: h.beam,
                moving: h.moving,
                intensity: 0.0,
                color: [1.0; 3],
                pan: 0.5,
                tilt: 0.5,
                limited: false,
            };
            let Some(o) = live.get(h.id.as_str()) else { return base };
            let f = |k: &str, d: f32| num(o, k).map(|x| x as f32).unwrap_or(d);
            let mut c = o.get_path("color").and_then(Value::as_color).map(|c| [c[0], c[1], c[2]]).unwrap_or([1.0; 3]);
            if c.iter().any(|x| *x > 1.0) {
                c = c.map(|x| x / 255.0);
            }
            StageHead {
                pos: [f("x", base.pos[0]), f("y", base.pos[1])],
                rotation: f("rotation", base.rotation),
                beam: f("beam", base.beam),
                intensity: f("intensity", 0.0).clamp(0.0, 1.0),
                color: c,
                pan: f("pan", 0.5),
                tilt: f("tilt", 0.5),
                limited: o.get_path("limited").is_some_and(Value::truthy),
                ..base
            }
        })
        .collect()
}

/// Stage coordinates (x 0 = stage left … 1 = stage right; y 0 = downstage … 1 = upstage) → screen.
fn to_screen(stage: Rect, p: [f32; 2]) -> Pos2 {
    let r = stage.shrink(28.0);
    Pos2::new(r.left() + r.width() * p[0].clamp(0.0, 1.0), r.bottom() - r.height() * p[1].clamp(0.0, 1.0))
}

/// Screen-space beam direction and reach (0..1) of a mover. Rotation 0° faces downstage
/// (screen down), 90° faces stage right; pan 0.5 = rotation, tilt 0.5 = straight down.
fn beam_dir(rotation_deg: f32, pan: f32, tilt: f32) -> (Vec2, f32) {
    let mut a = (rotation_deg + (pan - 0.5) * PAN_TRAVEL_DEG).to_radians();
    let off = tilt - 0.5;
    if off < 0.0 {
        a += std::f32::consts::PI;
    }
    (Vec2::new(a.sin(), a.cos()), 0.12 + 0.88 * (off.abs() * 2.0).min(1.0))
}

fn stage_card(cx: &Cx, ui: &mut Ui) {
    let t = cx.t;
    let heads = stage_heads(cx.rig, cx.output);
    let schematic = !cx.rig.unverified.is_empty();
    let sub = if schematic {
        "Logical fixture / pixel order — not a surveyed room layout or physical confirmation."
    } else {
        "Rendered DMX preview — not physical confirmation."
    };
    widgets::titled(
        ui,
        t,
        if schematic { "Fixture / pixel test" } else { "Stage" },
        sub,
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if schematic {
                patch_picture(t, ui, cx.rig, &heads);
                return;
            }
            let w = ui.available_width();
            let (stage, resp) = ui.allocate_exact_size(Vec2::new(w, (w * 0.42).clamp(160.0, 340.0)), Sense::hover());
            stage_floor(t, ui, cx.rig, &heads, stage);
            if let Some(pos) = resp.hover_pos()
                && let Some(h) = heads
                    .iter()
                    .map(|h| (h, to_screen(stage, h.pos).distance(pos)))
                    .filter(|(_, d)| *d <= 18.0)
                    .min_by(|a, b| a.1.total_cmp(&b.1))
                    .map(|(h, _)| h)
            {
                let mut tip = format!("{}\nBrightness {}", cx.rig.head_name(&h.id), pct(h.intensity));
                if h.limited {
                    tip.push_str("\nSoftened by the flash safety");
                }
                resp.on_hover_text_at_pointer(tip);
            }
        },
    );
}

/// Keep unsurveyed heads visible without assigning fictitious stage coordinates.
fn patch_picture(t: &Theme, ui: &mut Ui, rig: &Rig, heads: &[StageHead]) {
    let label_width = 180.0_f32.min(ui.available_width() * 0.4);
    let pixel_width = (ui.available_width() - label_width - spacing::L).max(18.0);
    egui::Grid::new("lights_patch_picture").spacing(Vec2::new(spacing::L, spacing::S)).show(ui, |ui| {
        for fixture in &rig.fixtures {
            ui.add_sized([label_width, 18.0], egui::Label::new(RichText::new(fixture.name()).font(font_medium(type_scale::SMALL)).color(t.text_dim)).truncate());
            ui.horizontal_wrapped(|ui| {
                ui.set_width(pixel_width);
                ui.spacing_mut().item_spacing = Vec2::splat(3.0);
                for (head, info) in heads.iter().zip(&rig.heads).filter(|(_, info)| info.fixture == fixture.id) {
                    let (r, response) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                    let fill = if head.intensity > 0.01 {
                        rgb(head.color[0] * head.intensity, head.color[1] * head.intensity, head.color[2] * head.intensity)
                    } else {
                        mix(t.inset, t.fg, 0.1)
                    };
                    ui.painter().rect_filled(r, CornerRadius::same(3), fill);
                    let ring = if head.limited { t.yellow } else { mix(t.inset, t.fg, 0.35) };
                    ui.painter().rect_stroke(r, CornerRadius::same(3), Stroke::new(1.0, ring), StrokeKind::Inside);
                    response.on_hover_ui(|ui| {
                        ui.label(rig.head_name(&info.id));
                        ui.label(format!("Brightness {}", pct(head.intensity)));
                        if head.limited {
                            ui.label("Softened by flash safety");
                        }
                    });
                }
            });
            ui.end_row();
        }
    });
}

/// The stage seen from above: lights glow in their live color, movers throw a beam cone.
fn stage_floor(t: &Theme, ui: &Ui, rig: &Rig, heads: &[StageHead], stage: Rect) {
    let p = ui.painter_at(stage);
    let floor = t.inset;
    p.rect_filled(stage, CornerRadius::same(radius::CONTROL), floor);
    let grid = Stroke::new(1.0, mix(floor, t.fg, 0.05));
    let inner = stage.shrink(28.0);
    for i in 0..=8 {
        let x = inner.left() + inner.width() * i as f32 / 8.0;
        p.line_segment([Pos2::new(x, inner.top()), Pos2::new(x, inner.bottom())], grid);
    }
    for i in 0..=4 {
        let y = inner.top() + inner.height() * i as f32 / 4.0;
        p.line_segment([Pos2::new(inner.left(), y), Pos2::new(inner.right(), y)], grid);
    }
    let edge = Stroke::new(2.0, mix(floor, t.fg, 0.18));
    p.line_segment([Pos2::new(inner.left(), stage.bottom() - 18.0), Pos2::new(inner.right(), stage.bottom() - 18.0)], edge);
    let small = font_medium(type_scale::SMALL - 1.0);
    p.text(stage.center_top() + Vec2::new(0.0, 8.0), Align2::CENTER_TOP, "Back of the stage", small.clone(), t.text_faint);
    p.text(stage.center_bottom() - Vec2::new(0.0, 3.0), Align2::CENTER_BOTTOM, "Front (camera side)", small, t.text_faint);
    // beams first, glyphs on top
    for h in heads.iter().filter(|h| h.moving) {
        let c = to_screen(stage, h.pos);
        let (dir, reach) = beam_dir(h.rotation, h.pan, h.tilt);
        let len = reach * stage.height() * 0.45;
        let half = (h.beam * 0.5).clamp(1.5, 45.0).to_radians();
        let base = dir.y.atan2(dir.x);
        let mut pts = vec![c];
        for i in 0..=8 {
            let a = base - half + 2.0 * half * i as f32 / 8.0;
            pts.push(c + Vec2::new(a.cos(), a.sin()) * len);
        }
        let col = rgb(h.color[0], h.color[1], h.color[2]);
        let alpha = 0.08 + 0.4 * h.intensity;
        p.add(Shape::convex_polygon(pts, col.gamma_multiply(alpha), Stroke::new(1.0, col.gamma_multiply(alpha + 0.1))));
    }
    for h in heads {
        let c = to_screen(stage, h.pos);
        let lit = rgb(h.color[0] * h.intensity, h.color[1] * h.intensity, h.color[2] * h.intensity);
        if h.intensity > 0.01 {
            let glow = rgb(h.color[0], h.color[1], h.color[2]);
            p.circle_filled(c, 10.0 + 22.0 * h.intensity, glow.gamma_multiply(0.10 * h.intensity));
            p.circle_filled(c, 10.0 + 10.0 * h.intensity, glow.gamma_multiply(0.25 * h.intensity));
        }
        let fill = if h.intensity > 0.01 { lit } else { mix(floor, t.fg, 0.1) };
        let ring = Stroke::new(1.5, mix(floor, t.fg, 0.35));
        if h.kind == "bar" {
            let r = Rect::from_center_size(c, Vec2::splat(16.0));
            p.rect_filled(r, CornerRadius::same(3), fill);
            p.rect_stroke(r, CornerRadius::same(3), ring, StrokeKind::Outside);
        } else {
            p.circle_filled(c, 9.0, fill);
            p.circle_stroke(c, 9.0, ring);
            if h.moving {
                p.circle_filled(c, 2.5, t.fg);
            }
        }
        if h.limited {
            p.circle_stroke(c, 15.0, Stroke::new(2.0, t.yellow));
        }
    }
    if rig.fixtures.len() <= 48 {
        for f in &rig.fixtures {
            let pos = rig.fixture_heads(f).first().and_then(|h| heads.iter().find(|x| &x.id == h)).map(|h| h.pos).unwrap_or(f.position);
            let g = one_line(ui, &f.name(), font_medium(type_scale::SMALL), t.text_dim, 180.0);
            // names go under the light, or above it near the front edge (the edge has its own label)
            let under = to_screen(stage, pos) + Vec2::new(-g.size().x / 2.0, 18.0);
            let at = if under.y + g.size().y > stage.bottom() - 22.0 { to_screen(stage, pos) - Vec2::new(g.size().x / 2.0, 18.0 + g.size().y) } else { under };
            p.galley_with_override_text_color(at, g, t.text_dim);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_tile_cannot_release_another_base_selection() {
        let mut m = Model::new(Some(std::env::temp_dir().join(format!("se-lights-stale-{}.sock", std::process::id()))));
        m.connected = true;
        m.queries.insert("lights.layers".into(), Value::map().with("base", Value::map()
            .with("active", true).with("owner", "layer:base:2").with("pending", Value::Null)
            .with("selection", Value::map().with("palette", "blue"))));
        let t = Theme::default();
        let rig = Rig::default();
        let now = Instant::now();
        let cx = Cx { m: &m, t: &t, rig: &rig, output: None, on_air: false, now };
        let mut st = LightsState::default();
        let mut out = Vec::new();
        toggle_look(&cx, &mut st, &mut out, "warm", true);
        assert!(out.is_empty(), "a stale Warm stop must not affect the replacement Blue base");
        assert!(!layer_busy(&m, &st, "base", now), "ignored stale stop must not lock the controls");
    }


    /// A knob sends its value live while moving and saves it once it's still.
    #[test]
    fn knob_moves_are_live_then_saved_once() {
        let mut st = LightsState::default();
        let mut out = Vec::new();
        let t0 = Instant::now();
        let op =
            |v: f64, save: bool| act("lights.knob", Value::map().with("look", "warm").with("target", "set.all.intensity").with("value", v).with("save", save));
        st.knob_moved(&mut out, "look", "warm", "set.all.intensity", Value::Float(0.5), Moved::Dragging, t0);
        st.knob_moved(&mut out, "look", "warm", "set.all.intensity", Value::Float(0.55), Moved::Dragging, t0 + Duration::from_millis(10));
        st.knob_moved(&mut out, "look", "warm", "set.all.intensity", Value::Float(0.6), Moved::Dragging, t0 + Duration::from_millis(80));
        assert_eq!(out, [op(0.5, false), op(0.6, false)], "throttled while dragging");
        assert_eq!(st.knob_value("look/warm/set.all.intensity", t0 + Duration::from_millis(90)), Some(&Value::Float(0.6)));
        out.clear();
        assert!(st.flush_knobs(&mut out, t0 + Duration::from_millis(300)), "still moving: not saved yet");
        assert!(out.is_empty());
        assert!(!st.flush_knobs(&mut out, t0 + Duration::from_millis(700)));
        assert_eq!(out, [op(0.6, true)], "saved once it has been still");
        out.clear();
        st.flush_knobs(&mut out, t0 + Duration::from_millis(900));
        assert!(out.is_empty(), "saved only once");
        st.knob_moved(&mut out, "look", "warm", "set.all.intensity", Value::Float(0.7), Moved::Done, t0 + Duration::from_millis(1000));
        assert_eq!(out, [op(0.7, true)], "a released slider saves at once");
    }

    #[test]
    fn beam_points_downstage_at_rest_and_follows_rotation() {
        let (d, reach) = beam_dir(0.0, 0.5, 1.0);
        assert!(d.y > 0.99 && d.x.abs() < 1e-3, "0° faces downstage (screen down): {d:?}");
        assert!((reach - 1.0).abs() < 1e-6);
        let (d, _) = beam_dir(90.0, 0.5, 1.0);
        assert!(d.x > 0.99, "90° faces stage right: {d:?}");
        let (d, _) = beam_dir(0.0, 0.5, 0.0);
        assert!(d.y < -0.99, "tilt below centre throws the other way: {d:?}");
        let (_, reach) = beam_dir(0.0, 0.5, 0.5);
        assert!(reach > 0.0 && reach < 0.2, "tilt centre = short stub");
    }

}
