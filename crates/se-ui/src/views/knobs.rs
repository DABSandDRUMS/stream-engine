//! The premade knobs a quick effect, light look or cue list exposes (se-core `knob`), drawn the
//! same everywhere: a slider for numbers, a colour button for colours, a segmented control or a
//! drop-down for choices. `knob` is one entry of a query's `knobs` list (`Knob::to_value()`:
//! `{label, target, kind, min, max, step, unit, default, options: [{label, value}]}`).
//!
//! Callers send the value live on [`Moved::Dragging`] and save it on [`Moved::Done`] or once
//! it has been quiet for a moment, so a slider drag doesn't write the file every frame.

use egui::{Align, Color32, Layout, RichText};
use se_proto::Value;
use se_ui_kit::Theme;
use se_ui_kit::theme::{font, font_medium, font_mono, type_scale};
use se_ui_kit::widgets;

/// What one frame did to a knob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Moved {
    No,
    /// Changed and may change again right away (a slider held, a colour picker open): preview
    /// it, save once it's quiet.
    Dragging,
    /// Settled (slider let go, a choice picked): save it now.
    Done,
}

fn num(knob: &Value, k: &str) -> Option<f64> {
    knob.get_path(k).and_then(Value::as_f64)
}

fn text<'a>(knob: &'a Value, k: &str) -> &'a str {
    knob.get_path(k).and_then(Value::as_str).unwrap_or("")
}

fn options(knob: &Value) -> &[Value] {
    knob.get_path("options").and_then(Value::as_list).unwrap_or(&[])
}

/// Numbers compare by value (`1` = `1.0`).
fn same(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => (x - y).abs() < 1e-9,
        _ => a == b,
    }
}

/// Decimals a step needs: `0.25` → 2, `0.5` → 1, `1` → 0.
fn decimals(step: Option<f64>) -> usize {
    match step {
        Some(s) if s > 0.0 => (0..4)
            .find(|d| {
                let x = s * 10f64.powi(*d as i32);
                (x - x.round()).abs() < 1e-6
            })
            .unwrap_or(3),
        _ => 2,
    }
}

/// The value as a number and its unit words: `("2.5", "flashes a second")`, `("+30%", "")`.
fn parts(knob: &Value, v: &Value) -> (String, String) {
    match text(knob, "kind") {
        "color" => (v.as_str().unwrap_or("").to_string(), String::new()),
        "choice" => {
            let label = options(knob).iter().find(|o| o.get_path("value").is_some_and(|x| same(x, v))).map(|o| text(o, "label").to_string());
            (String::new(), label.unwrap_or_default())
        }
        _ => {
            let x = v.as_f64().unwrap_or(0.0);
            let unit = text(knob, "unit");
            if unit == "%" {
                let pct = (x * 100.0).round();
                let signed = num(knob, "min").is_some_and(|m| m < 0.0);
                let s = if pct == 0.0 {
                    "0%".to_string()
                } else if signed {
                    format!("{pct:+.0}%")
                } else {
                    format!("{pct:.0}%")
                };
                (s, String::new())
            } else {
                // as many decimals as the step needs, without trailing zeros ("2.5", "3")
                let d = decimals(num(knob, "step"));
                let s = format!("{x:.d$}");
                let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
                (if s == "-0" { "0".into() } else { s }, unit.to_string())
            }
        }
    }
}

/// The value in words: "2.5 flashes a second", "+30%", "#ff8800", "Fast".
pub fn shown(knob: &Value, v: &Value) -> String {
    match parts(knob, v) {
        (n, u) if n.is_empty() => u,
        (n, u) if u.is_empty() => n,
        (n, u) => format!("{n} {u}"),
    }
}

/// The value "Reset knobs" goes back to (`None` when the knob has no default).
pub fn default_of(knob: &Value) -> Option<&Value> {
    knob.get_path("default").filter(|v| !v.is_null())
}

fn label_row(ui: &mut egui::Ui, t: &Theme, knob: &Value, v: &Value) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(text(knob, "label")).font(font_medium(type_scale::SMALL + 0.5)).color(t.text_dim));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let (n, u) = parts(knob, v);
            ui.spacing_mut().item_spacing.x = 4.0;
            if !u.is_empty() {
                ui.label(RichText::new(u).font(font(type_scale::SMALL)).color(t.fg));
            }
            if !n.is_empty() {
                ui.label(RichText::new(n).font(font_mono(type_scale::SMALL)).color(t.fg));
            }
        });
    });
}

/// One knob. `value` is what it shows and is changed in place.
pub fn control(ui: &mut egui::Ui, t: &Theme, salt: impl std::hash::Hash + std::fmt::Debug, knob: &Value, value: &mut Value) -> Moved {
    let id = egui::Id::new(("knob", salt));
    ui.push_id(id, |ui| match text(knob, "kind") {
        "color" => {
            let mut c = value.as_str().and_then(se_ui_kit::theme::hex).unwrap_or(Color32::WHITE);
            let mut moved = Moved::No;
            ui.horizontal(|ui| {
                ui.label(RichText::new(text(knob, "label")).font(font_medium(type_scale::SMALL + 0.5)).color(t.text_dim));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // the picker stays open while it's moved: every change is a preview, saved
                    // once it's quiet
                    if egui::color_picker::color_edit_button_srgba(ui, &mut c, egui::color_picker::Alpha::Opaque).changed() {
                        *value = Value::Str(format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b()));
                        moved = Moved::Dragging;
                    }
                });
            });
            moved
        }
        "choice" => {
            let opts = options(knob);
            let labels: Vec<&str> = opts.iter().map(|o| text(o, "label")).collect();
            let mut i = opts.iter().position(|o| o.get_path("value").is_some_and(|x| same(x, value))).unwrap_or(usize::MAX);
            let before = i;
            ui.label(RichText::new(text(knob, "label")).font(font_medium(type_scale::SMALL + 0.5)).color(t.text_dim));
            if labels.len() <= 4 {
                widgets::segmented(ui, t, &mut i, &labels);
            } else {
                let current = labels.get(i).copied().unwrap_or("Pick one");
                egui::ComboBox::from_id_salt("choice").width(ui.available_width().min(320.0)).selected_text(current).show_ui(ui, |ui| {
                    for (j, l) in labels.iter().enumerate() {
                        ui.selectable_value(&mut i, j, *l);
                    }
                });
            }
            match opts.get(i).and_then(|o| o.get_path("value")) {
                Some(v) if i != before => {
                    *value = v.clone();
                    Moved::Done
                }
                _ => Moved::No,
            }
        }
        _ => {
            let (min, max) = (num(knob, "min").unwrap_or(0.0), num(knob, "max").unwrap_or(1.0));
            let mut x = value.as_f64().unwrap_or(min).clamp(min, max);
            label_row(ui, t, knob, &Value::Float(x));
            ui.spacing_mut().slider_width = ui.available_width();
            ui.spacing_mut().interact_size.y = 20.0;
            let step = num(knob, "step").filter(|s| *s > 0.0);
            let before = x;
            let mut s = egui::Slider::new(&mut x, min..=max).show_value(false);
            if let Some(step) = step {
                s = s.step_by(step);
            }
            let r = ui.add(s);
            // the slider snapping a value that's already on a step isn't a change
            let changed = r.changed() && (x - before).abs() > step.map_or((max - min) * 1e-6, |s| s * 1e-3);
            if changed {
                *value = Value::Float((x * 1e6).round() / 1e6);
            }
            // a drag counts as done when it's let go, if it moved the value at all
            let key = id.with("moved");
            let mut moving = ui.data(|d| d.get_temp::<bool>(key)).unwrap_or(false);
            let held = r.is_pointer_button_down_on() || r.dragged();
            let out = if changed && held {
                moving = true;
                Moved::Dragging
            } else if changed || (moving && !held) {
                moving = false;
                Moved::Done
            } else {
                Moved::No
            };
            ui.data_mut(|d| d.insert_temp(key, moving));
            out
        }
    })
    .inner
}

#[cfg(test)]
mod tests {
    use super::*;

    fn knob(unit: &str, min: f64, step: Option<f64>) -> Value {
        Value::map().with("kind", "number").with("unit", unit).with("min", min).with("max", 3.0).with("step", step.map(Value::Float).unwrap_or_default())
    }

    #[test]
    fn values_read_as_words() {
        assert_eq!(shown(&knob("flashes a second", 0.5, Some(0.25)), &Value::Float(2.5)), "2.5 flashes a second");
        assert_eq!(shown(&knob("pieces", 40.0, Some(20.0)), &Value::Int(160)), "160 pieces");
        assert_eq!(shown(&knob("%", 0.0, Some(0.05)), &Value::Float(0.6)), "60%");
        assert_eq!(shown(&knob("%", -0.5, Some(0.05)), &Value::Float(0.3)), "+30%");
        assert_eq!(shown(&knob("%", -0.5, Some(0.05)), &Value::Float(-0.25)), "-25%");
        assert_eq!(shown(&knob("%", -0.5, Some(0.05)), &Value::Float(0.0)), "0%");
        let choice = Value::map().with("kind", "choice").with("options", Value::List(vec![Value::map().with("label", "Fast").with("value", 4.0)]));
        assert_eq!(shown(&choice, &Value::Int(4)), "Fast");
    }
}
