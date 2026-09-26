//! Curve widgets for Modulate (§15.5): a binding's transfer curve with a live input marker, and a
//! dual scope overlaying a raw signal and its shaped output. Both allocate exactly `size`, format
//! only a handful of labels per frame (never per point), and draw non-finite samples as gaps.

use crate::theme::Theme;
use egui::{Align2, CornerRadius, FontId, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, Vec2, pos2, vec2};

const PAD: f32 = 4.0;
const TICK_FONT: f32 = 9.5;
const MAX_SAMPLES: usize = 512;

/// Plot output vs input (0–1) through the shaping function `f`, with the y axis spanning
/// `range_out` (sorted; a degenerate or non-finite range falls back to 0–1; values outside are
/// drawn on the border). The dashed diagonal is the linear mapping; `marker_in` puts a live dot on
/// the curve at the current input.
pub fn transfer_curve(ui: &mut Ui, t: &Theme, size: Vec2, f: &dyn Fn(f64) -> f64, range_out: [f64; 2], marker_in: Option<f64>) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(3), t.bg_darker);
    let plot = rect.shrink(PAD);
    if plot.width() < 2.0 || plot.height() < 2.0 {
        return resp;
    }
    grid(&p, t, plot, 4);
    p.extend(Shape::dashed_line(&[plot.left_bottom(), plot.right_top()], Stroke::new(1.0, t.muted), 4.0, 4.0));

    let (lo, hi) = out_range(range_out);
    let at = |x: f64, y: f64| unit(y, lo, hi).map(|v| pos2(plot.left() + plot.width() * x as f32, plot.bottom() - plot.height() * v));
    let n = (plot.width().round() as usize).clamp(2, MAX_SAMPLES);
    let step = 1.0 / (n - 1) as f64;
    paint_series(&p, (0..n).map(|i| at(i as f64 * step, f(i as f64 * step))), Stroke::new(2.0, t.accent));

    let font = FontId::proportional(TICK_FONT);
    p.text(plot.left_top() + vec2(2.0, 1.0), Align2::LEFT_TOP, fmt_num(hi), font.clone(), t.fg_dim);
    p.text(plot.left_bottom() + vec2(2.0, -1.0), Align2::LEFT_BOTTOM, fmt_num(lo), font.clone(), t.fg_dim);
    p.text(plot.center_bottom() + vec2(0.0, -1.0), Align2::CENTER_BOTTOM, ".5", font.clone(), t.fg_dim);
    p.text(plot.right_bottom() + vec2(-2.0, -1.0), Align2::RIGHT_BOTTOM, "1", font.clone(), t.fg_dim);

    if let Some(x) = marker_in.filter(|m| m.is_finite()).map(|m| m.clamp(0.0, 1.0)) {
        let c = t.modulated();
        let y = f(x);
        let sx = plot.left() + plot.width() * x as f32;
        let guide = Stroke::new(1.0, c.gamma_multiply(0.6));
        p.extend(Shape::dashed_line(&[pos2(sx, plot.top()), pos2(sx, plot.bottom())], guide, 3.0, 3.0));
        if let Some(dot) = at(x, y) {
            p.extend(Shape::dashed_line(&[pos2(plot.left(), dot.y), dot], guide, 3.0, 3.0));
            p.circle_filled(dot, 4.0, c);
            p.circle_stroke(dot, 4.0, Stroke::new(1.0, t.bg_darker));
        }
        p.text(plot.right_top() + vec2(-2.0, 1.0), Align2::RIGHT_TOP, format!("{x:.2} → {}", fmt_num(y)), font, c);
    }
    p.rect_stroke(plot, CornerRadius::ZERO, Stroke::new(1.0, t.muted), StrokeKind::Outside);
    resp
}

/// Overlay a raw signal (dim) and its shaped output (magenta), newest sample at the right edge.
/// Each has its own scale: the input spans its finite min/max joined with 0–1; the output spans
/// `out_range` (sorted; falls back to the input rule when degenerate or non-finite).
pub fn dual_scope(ui: &mut Ui, t: &Theme, size: Vec2, input: &[f32], output: &[f32], out_range: [f32; 2]) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(3), t.bg_darker);
    let plot = rect.shrink(PAD);
    if plot.width() < 2.0 || plot.height() < 2.0 {
        return resp;
    }
    grid(&p, t, plot, 2);
    let font = FontId::proportional(TICK_FONT);
    let (out_c, in_c) = (t.modulated(), t.fg_dim);
    let n = input.len().max(output.len());
    if n == 0 {
        p.text(plot.center(), Align2::CENTER_CENTER, "no data", font, in_c);
        return resp;
    }
    let in_r = auto_range(input);
    let out_r = if out_range[0].is_finite() && out_range[1].is_finite() && out_range[0] != out_range[1] {
        (out_range[0].min(out_range[1]), out_range[0].max(out_range[1]))
    } else {
        auto_range(output)
    };
    let dx = if n > 1 { plot.width() / (n - 1) as f32 } else { 0.0 };
    let series = |values: &[f32], (lo, hi): (f32, f32), stroke: Stroke| {
        let len = values.len();
        let pts = values.iter().enumerate().map(|(i, v)| {
            unit(f64::from(*v), f64::from(lo), f64::from(hi)).map(|u| pos2(plot.right() - (len - 1 - i) as f32 * dx, plot.bottom() - plot.height() * u))
        });
        paint_series(&p, pts, stroke);
    };
    series(input, in_r, Stroke::new(1.5, in_c));
    series(output, out_r, Stroke::new(2.0, out_c));

    p.text(plot.left_top() + vec2(2.0, 1.0), Align2::LEFT_TOP, fmt_num(f64::from(in_r.1)), font.clone(), in_c);
    p.text(plot.left_bottom() + vec2(2.0, -1.0), Align2::LEFT_BOTTOM, fmt_num(f64::from(in_r.0)), font.clone(), in_c);
    p.text(plot.right_top() + vec2(-2.0, 1.0), Align2::RIGHT_TOP, fmt_num(f64::from(out_r.1)), font.clone(), out_c);
    p.text(plot.right_bottom() + vec2(-2.0, -1.0), Align2::RIGHT_BOTTOM, fmt_num(f64::from(out_r.0)), font.clone(), out_c);
    // Legend, top center: swatch + name per trace.
    let y = plot.top() + 7.0;
    let mut x = plot.center().x - 34.0;
    for (name, c) in [("in", in_c), ("out", out_c)] {
        p.line_segment([pos2(x, y), pos2(x + 10.0, y)], Stroke::new(2.0, c));
        let r = p.text(pos2(x + 13.0, y), Align2::LEFT_CENTER, name, font.clone(), c);
        x = r.right() + 10.0;
    }
    resp
}

fn grid(p: &Painter, t: &Theme, plot: Rect, divisions: usize) {
    let stroke = Stroke::new(1.0, t.muted.gamma_multiply(0.35));
    for k in 1..divisions {
        let f = k as f32 / divisions as f32;
        p.vline(plot.left() + plot.width() * f, plot.y_range(), stroke);
        p.hline(plot.x_range(), plot.bottom() - plot.height() * f, stroke);
    }
}

/// `(lo, hi)` sorted, or 0–1 when not a usable range.
fn out_range(r: [f64; 2]) -> (f64, f64) {
    if r[0].is_finite() && r[1].is_finite() && r[0] != r[1] { (r[0].min(r[1]), r[0].max(r[1])) } else { (0.0, 1.0) }
}

/// Finite min/max of `values` joined with 0–1 (so the span is never empty).
fn auto_range(values: &[f32]) -> (f32, f32) {
    values.iter().filter(|v| v.is_finite()).fold((0.0f32, 1.0f32), |(lo, hi), v| (lo.min(*v), hi.max(*v)))
}

/// `v` as a 0–1 fraction of `lo..hi`, clamped; `None` for non-finite values.
fn unit(v: f64, lo: f64, hi: f64) -> Option<f32> {
    v.is_finite().then(|| ((v - lo) / (hi - lo)).clamp(0.0, 1.0) as f32)
}

fn fmt_num(v: f64) -> String {
    let a = v.abs();
    if !v.is_finite() {
        "—".to_owned()
    } else if a >= 1000.0 {
        format!("{v:.0}")
    } else if a >= 100.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    }
}

/// Calls `f` with each maximal run of consecutive `Some` points (runs split at `None`). `f` may
/// take the buffer; it is cleared afterwards either way.
fn for_each_run(points: impl Iterator<Item = Option<Pos2>>, mut f: impl FnMut(&mut Vec<Pos2>)) {
    let mut run = Vec::new();
    for pt in points {
        match pt {
            Some(q) => run.push(q),
            None if !run.is_empty() => {
                f(&mut run);
                run.clear();
            }
            None => {}
        }
    }
    if !run.is_empty() {
        f(&mut run);
    }
}

/// Polylines through the finite points; an isolated sample becomes a dot.
fn paint_series(p: &Painter, points: impl Iterator<Item = Option<Pos2>>, stroke: Stroke) {
    for_each_run(points, |run| {
        if run.len() == 1 {
            p.circle_filled(run[0], stroke.width, stroke.color);
        } else {
            p.add(Shape::line(std::mem::take(run), stroke));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::epaint::{ClippedPrimitive, Primitive};
    use egui::{Context, RawInput};

    #[test]
    fn runs_split_at_gaps() {
        let p = |x: f32| Some(pos2(x, 0.0));
        let pts = [p(0.0), p(1.0), None, p(2.0), None, None, p(3.0), p(4.0), p(5.0)];
        let mut lens = Vec::new();
        for_each_run(pts.into_iter(), |r| lens.push(r.len()));
        assert_eq!(lens, [2, 1, 3]);
        let mut calls = 0;
        for_each_run([None, None].into_iter(), |_| calls += 1);
        for_each_run(std::iter::empty(), |_| calls += 1);
        assert_eq!(calls, 0);
        // A consumer that takes the buffer does not leak points into the next run.
        let mut firsts = Vec::new();
        for_each_run(pts.into_iter(), |r| firsts.push(std::mem::take(r)[0].x));
        assert_eq!(firsts, [0.0, 2.0, 3.0]);
    }

    #[test]
    fn ranges_and_mapping() {
        assert_eq!(out_range([2.0, -2.0]), (-2.0, 2.0));
        assert_eq!(out_range([3.0, 3.0]), (0.0, 1.0));
        assert_eq!(out_range([f64::NAN, 1.0]), (0.0, 1.0));
        assert_eq!(auto_range(&[]), (0.0, 1.0));
        assert_eq!(auto_range(&[f32::NAN, f32::INFINITY]), (0.0, 1.0));
        assert_eq!(auto_range(&[0.5, f32::NAN, 140.0, -3.0]), (-3.0, 140.0));
        assert_eq!(unit(0.5, 0.0, 2.0), Some(0.25));
        assert_eq!(unit(9.0, 0.0, 2.0), Some(1.0));
        assert_eq!(unit(-9.0, 0.0, 2.0), Some(0.0));
        assert_eq!(unit(f64::NAN, 0.0, 1.0), None);
        assert_eq!(unit(f64::NEG_INFINITY, 0.0, 1.0), None);
        assert_eq!(fmt_num(1234.5), "1234");
        assert_eq!(fmt_num(-150.25), "-150.2");
        assert_eq!(fmt_num(0.125), "0.12");
        assert_eq!(fmt_num(f64::NAN), "—");
    }

    /// Run one frame of `add` and return its response plus the tessellated output.
    fn render(add: impl Fn(&mut Ui) -> Response) -> (Response, Vec<ClippedPrimitive>) {
        let ctx = Context::default();
        let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0))), ..Default::default() };
        let mut out = None;
        let mut full = ctx.run_ui(input, |ui| out = Some(add(ui)));
        full.textures_delta.clear();
        (out.expect("ran"), ctx.tessellate(full.shapes, full.pixels_per_point))
    }

    #[track_caller]
    fn assert_finite(prims: &[ClippedPrimitive]) {
        for cp in prims {
            if let Primitive::Mesh(m) = &cp.primitive {
                assert!(m.vertices.iter().all(|v| v.pos.x.is_finite() && v.pos.y.is_finite()), "non-finite vertex");
            }
        }
    }

    #[test]
    fn transfer_curve_handles_bad_input() {
        let t = Theme::default();
        let size = vec2(180.0, 120.0);
        let cases: [(&dyn Fn(f64) -> f64, [f64; 2], Option<f64>); 5] = [
            (&|x| x * x, [0.0, 1.0], Some(0.3)),
            (&|x| if x > 0.5 { f64::NAN } else { x }, [0.0, 1.0], Some(0.9)),
            (&|_| f64::INFINITY, [5.0, 5.0], Some(f64::NAN)),
            (&|x| 200.0 * x - 50.0, [100.0, -100.0], Some(7.0)),
            (&|x| x, [f64::NAN, f64::INFINITY], None),
        ];
        for (f, range, marker) in cases {
            let (resp, prims) = render(|ui| transfer_curve(ui, &t, size, f, range, marker));
            assert_eq!(resp.rect.size(), size);
            assert_finite(&prims);
        }
        // Too small to plot still allocates exactly.
        let (resp, prims) = render(|ui| transfer_curve(ui, &t, vec2(6.0, 3.0), &|x| x, [0.0, 1.0], Some(0.5)));
        assert_eq!(resp.rect.size(), vec2(6.0, 3.0));
        assert_finite(&prims);
    }

    #[test]
    fn dual_scope_handles_bad_input() {
        let t = Theme::default();
        let size = vec2(220.0, 90.0);
        let ramp: Vec<f32> = (0..64).map(|i| i as f32 / 63.0).collect();
        let holes: Vec<f32> = (0..64).map(|i| if i % 5 == 0 { f32::NAN } else { (i as f32).sin() * 40.0 }).collect();
        let cases: [(&[f32], &[f32], [f32; 2]); 6] = [
            (&[], &[], [0.0, 1.0]),
            (&[0.4], &[], [0.0, 1.0]),
            (&[], &[0.7], [0.0, 1.0]),
            (&[f32::NAN; 8], &[f32::INFINITY; 8], [f32::NAN, 1.0]),
            (&holes, &ramp[..10], [2.0, 2.0]),
            (&ramp, &holes, [-40.0, 40.0]),
        ];
        for (input, output, range) in cases {
            let (resp, prims) = render(|ui| dual_scope(ui, &t, size, input, output, range));
            assert_eq!(resp.rect.size(), size);
            assert_finite(&prims);
        }
    }
}
