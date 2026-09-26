//! Per-frame scene evaluation (§4.3, §4.4): scene nodes + live addresses → a z-sorted list of
//! placements in canvas pixels, with `when` clauses, culling, and morph interpolation between
//! two scenes. Pure CPU; allocation-free once the output vectors reached their capacity.

use crate::addr::{Resolved, Whens};
use crate::plan::{Blend, NodePlan, Plan, Style, TrKind, TransitionPlan};
use se_hub::Snapshot;
use se_proto::Ease;

/// One placement of a source on a canvas.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Item {
    pub source: u32,
    /// Node the placement comes from (fx chain, mask).
    pub scene: u32,
    pub layout: u8,
    pub node: u32,
    /// Canvas pixels: x, y, w, h (before rotation, which is about the center).
    pub rect: [f32; 4],
    /// Source uv sub-rect x0, y0, x1, y1.
    pub crop: [f32; 4],
    pub radius: f32,
    pub opacity: f32,
    /// Radians, clockwise.
    pub rotation: f32,
    pub z: i64,
    pub order: u32,
    pub blend: Blend,
}

impl Item {
    pub fn node<'p>(&self, plan: &'p Plan) -> &'p NodePlan {
        &plan.scenes[self.scene as usize].layouts[self.layout as usize].nodes[self.node as usize]
    }

    /// Visible on a `w × h` canvas (bounding circle test covers rotation).
    pub fn on_canvas(&self, w: f32, h: f32) -> bool {
        let [x, y, rw, rh] = self.rect;
        if rw <= 0.5 || rh <= 0.5 || self.opacity <= 0.002 {
            return false;
        }
        if self.rotation == 0.0 {
            return x < w && y < h && x + rw > 0.0 && y + rh > 0.0;
        }
        let r = 0.5 * (rw * rw + rh * rh).sqrt();
        let (cx, cy) = (x + rw * 0.5, y + rh * 0.5);
        cx + r > 0.0 && cy + r > 0.0 && cx - r < w && cy - r < h
    }
}

/// Transition state at the frame's time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrState {
    pub active: bool,
    /// Linear progress 0..1.
    pub progress: f32,
    pub from: Option<usize>,
    pub to: Option<usize>,
    pub transition: usize,
}

impl TrState {
    pub fn eased(&self, plan: &Plan) -> f32 {
        ease(plan.transitions[self.transition].ease, self.progress)
    }
    pub fn kind(&self, plan: &Plan) -> TrKind {
        plan.transitions[self.transition].kind
    }
}

pub fn ease(e: Ease, t: f32) -> f32 {
    e.apply(t as f64) as f32
}

/// Program/preview/transition from the core's show state, interpolated by master-clock time
/// (the core's `show.transition.progress` only updates at its tick; this is exact per frame).
pub fn transition_state(plan: &Plan, snap: &Snapshot, res: &Resolved, now: u64) -> TrState {
    let a = &plan.show;
    let to = res.str(snap, a.program).and_then(|s| plan.scene_index.get(s).copied());
    let active = res.bool(snap, a.tr_active, false);
    let name = res.str(snap, a.tr_name).unwrap_or("");
    let transition = plan.transition_index.get(name).or_else(|| plan.transition_index.get("fade")).copied().unwrap_or(0);
    if !active {
        return TrState { active: false, progress: 1.0, from: None, to, transition };
    }
    let from = res.str(snap, a.tr_from).and_then(|s| plan.scene_index.get(s).copied());
    let start = res.i64(snap, a.tr_start, 0).max(0) as u64;
    let ms = res.i64(snap, a.tr_ms, 0).max(0) as u64;
    let progress = if ms == 0 { 1.0 } else { (now.saturating_sub(start) as f64 / (ms as f64 * 1e6)).clamp(0.0, 1.0) as f32 };
    let active = progress < 1.0 && from.is_some() && from != to;
    TrState { active, progress, from, to, transition }
}

/// Evaluate one scene layout into `out` (appends, unsorted, unculled).
#[allow(clippy::too_many_arguments)]
pub fn eval_layout(plan: &Plan, scene: usize, layout: usize, size: [f32; 2], snap: &Snapshot, res: &Resolved, whens: &mut Whens, out: &mut Vec<Item>) {
    let s = &plan.scenes[scene];
    for (ni, n) in s.layouts[layout].nodes.iter().enumerate() {
        if !res.bool(snap, n.visible, n.def.visible) {
            continue;
        }
        if let Some(w) = n.when
            && !whens.eval(w as usize, &plan.whens[w as usize], snap, res)
        {
            continue;
        }
        let r = res.vec4(snap, n.rect, n.def.rect);
        let c = res.vec4(snap, n.crop, n.def.crop);
        let scale = res.f32(snap, n.scale, n.def.scale).max(0.0);
        let off = [res.f32(snap, n.offset_x, n.def.offset[0]), res.f32(snap, n.offset_y, n.def.offset[1])];
        let base = [r[0] * size[0], r[1] * size[1], r[2] * size[0], r[3] * size[1]];
        let center = [base[0] + base[2] * 0.5 + off[0], base[1] + base[3] * 0.5 + off[1]];
        let wh = [base[2] * scale, base[3] * scale];
        let x0 = c[0].clamp(0.0, 1.0);
        let y0 = c[1].clamp(0.0, 1.0);
        let x1 = (1.0 - c[2]).clamp(x0 + 1e-4, 1.0);
        let y1 = (1.0 - c[3]).clamp(y0 + 1e-4, 1.0);
        out.push(Item {
            source: n.source,
            scene: scene as u32,
            layout: layout as u8,
            node: ni as u32,
            rect: [center[0] - wh[0] * 0.5, center[1] - wh[1] * 0.5, wh[0], wh[1]],
            crop: [x0, y0, x1, y1],
            radius: res.f32(snap, n.radius, n.def.radius).max(0.0) * scale,
            opacity: res.f32(snap, n.opacity, n.def.opacity).clamp(0.0, 1.0),
            rotation: res.f32(snap, n.rotation, n.def.rotation).to_radians(),
            z: res.i64(snap, n.z, n.def.z),
            order: ni as u32,
            blend: n.blend,
        });
    }
}

/// Cull off-canvas/transparent items and sort by (z, order) without allocating.
pub fn finish(items: &mut Vec<Item>, size: [f32; 2]) {
    items.retain(|i| i.on_canvas(size[0], size[1]));
    items.sort_unstable_by(|a, b| a.z.cmp(&b.z).then(a.order.cmp(&b.order)));
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn lerp4(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    [lerp(a[0], b[0], t), lerp(a[1], b[1], t), lerp(a[2], b[2], t), lerp(a[3], b[3], t)]
}

/// Apply an enter/exit style at presence `p` (0 = gone, 1 = in place).
pub fn apply_style(it: &mut Item, style: Style, p: f32, entering: bool, size: [f32; 2]) {
    let p = p.clamp(0.0, 1.0);
    match style {
        Style::None => {
            if p < 1.0 && !entering || p <= 0.0 {
                it.opacity = 0.0;
            }
        }
        Style::Fade => it.opacity *= p,
        Style::Scale => {
            let s = lerp(0.85, 1.0, p);
            let (cx, cy) = (it.rect[0] + it.rect[2] * 0.5, it.rect[1] + it.rect[3] * 0.5);
            it.rect[2] *= s;
            it.rect[3] *= s;
            it.rect[0] = cx - it.rect[2] * 0.5;
            it.rect[1] = cy - it.rect[3] * 0.5;
            it.radius *= s;
            it.opacity *= p;
        }
        Style::SlideLeft | Style::SlideRight | Style::SlideUp | Style::SlideDown => {
            // entering: arrive moving in the style's direction; exiting: leave in it
            let dir = match style {
                Style::SlideLeft => [-1.0, 0.0],
                Style::SlideRight => [1.0, 0.0],
                Style::SlideUp => [0.0, -1.0],
                _ => [0.0, 1.0],
            };
            let k = if entering { -(1.0 - p) } else { 1.0 - p };
            it.rect[0] += dir[0] * k * size[0];
            it.rect[1] += dir[1] * k * size[1];
        }
    }
}

/// Scratch buffers reused every frame.
#[derive(Default)]
pub struct MorphScratch {
    a: Vec<Item>,
    b: Vec<Item>,
    matched: Vec<bool>,
}

impl MorphScratch {
    /// Pre-sized for `nodes` per scene so the frame loop never grows it.
    pub fn with_capacity(nodes: usize) -> MorphScratch {
        MorphScratch { a: Vec::with_capacity(nodes), b: Vec::with_capacity(nodes), matched: Vec::with_capacity(nodes) }
    }
}

/// Morph between scene `from` and `to` at eased progress `t` (§4.4): nodes showing the same
/// source animate rect/crop/radius/opacity/rotation; the rest enter/exit with their style.
#[allow(clippy::too_many_arguments)]
pub fn eval_morph(
    plan: &Plan,
    tr: &TransitionPlan,
    from: usize,
    to: usize,
    layout: usize,
    size: [f32; 2],
    t: f32,
    snap: &Snapshot,
    res: &Resolved,
    whens: &mut Whens,
    scratch: &mut MorphScratch,
    out: &mut Vec<Item>,
) {
    let MorphScratch { a, b, matched } = scratch;
    a.clear();
    b.clear();
    eval_layout(plan, from, layout, size, snap, res, whens, a);
    eval_layout(plan, to, layout, size, snap, res, whens, b);
    matched.clear();
    matched.resize(a.len(), false);
    let base = out.len() as u32;
    for (bi, it) in b.iter().enumerate() {
        let mut m = *it;
        m.order = base + a.len() as u32 + bi as u32;
        match a.iter().enumerate().position(|(ai, x)| !matched[ai] && x.source == it.source) {
            Some(ai) => {
                matched[ai] = true;
                let s = &a[ai];
                m.rect = lerp4(s.rect, it.rect, t);
                m.crop = lerp4(s.crop, it.crop, t);
                m.radius = lerp(s.radius, it.radius, t);
                m.opacity = lerp(s.opacity, it.opacity, t);
                m.rotation = lerp(s.rotation, it.rotation, t);
                // keep the incoming stacking, but let the outgoing order lead until halfway
                m.z = if t < 0.5 { s.z } else { it.z };
                m.order = if t < 0.5 { base + ai as u32 } else { m.order };
            }
            None => {
                let style = it.node(plan).enter.unwrap_or(tr.enter);
                apply_style(&mut m, style, t, true, size);
            }
        }
        out.push(m);
    }
    for (ai, it) in a.iter().enumerate() {
        if matched[ai] {
            continue;
        }
        let mut m = *it;
        m.order = base + ai as u32;
        let style = it.node(plan).exit.unwrap_or(tr.exit);
        apply_style(&mut m, style, 1.0 - t, false, size);
        out.push(m);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::tests::snapshot;
    use crate::plan::tests::config;
    use crate::plan::{Plan, WIDE};
    use se_proto::Value;
    use std::path::PathBuf;

    fn plan() -> Plan {
        let c = config(&[
            ("project", "project", "schema = 1"),
            (
                "scenes",
                "a",
                "[canvas.wide]\nnodes = [{ src = \"cam1\", rect = [0, 0, 0.5, 1] }, { src = \"cam2\", rect = [0.5, 0, 0.5, 1], z = 1 }, { src = \"cam3\", rect = [2, 2, 0.1, 0.1] }]",
            ),
            (
                "scenes",
                "b",
                "[canvas.wide]\nnodes = [{ src = \"cam2\", rect = [0, 0, 1, 1], radius = 10 }, { src = \"cam4\", rect = [0.7, 0.7, 0.2, 0.2], when = \"mode == 'live'\", enter = \"slide_up\" }]",
            ),
            ("transitions", "morph", "kind = \"morph\"\nenter = \"fade\"\nexit = \"fade\""),
        ]);
        let p = Plan::build(&c, &[], PathBuf::from("/tmp"));
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        p
    }

    fn eval(p: &Plan, scene: &str, snap: &Snapshot) -> Vec<Item> {
        let mut res = Resolved::default();
        res.update(snap, &p.state, &p.signals);
        let mut whens = Whens::default();
        whens.reset(p.whens.len());
        let mut out = Vec::new();
        eval_layout(p, p.scene_index[scene], WIDE, [1920.0, 1080.0], snap, &res, &mut whens, &mut out);
        finish(&mut out, [1920.0, 1080.0]);
        out
    }

    #[test]
    fn layout_uses_live_addresses_culls_and_sorts() {
        let p = plan();
        let snap = snapshot(
            &[
                ("scene.a.node.cam1.rect.wide", Value::from([0.25f32, 0.0, 0.5, 1.0])),
                ("scene.a.node.cam1.z.wide", Value::Int(5)),
                ("scene.a.node.cam1.offset_x", Value::Float(10.0)),
                ("scene.a.node.cam1.scale", Value::Float(0.5)),
                ("scene.a.node.cam1.crop.wide", Value::from([0.1f32, 0.0, 0.2, 0.0])),
            ],
            &[],
        );
        let items = eval(&p, "a", &snap);
        assert_eq!(items.len(), 2, "cam3 is off-canvas");
        assert_eq!(p.sources[items[0].source as usize].name, "cam2", "z sorted");
        let cam1 = items[1];
        // 0.25..0.75 of 1920 → center 960 + 10, scaled to half size
        assert_eq!(cam1.rect, [970.0 - 240.0, 270.0, 480.0, 540.0]);
        assert!((cam1.crop[0] - 0.1).abs() < 1e-6 && (cam1.crop[2] - 0.8).abs() < 1e-6);
        let hidden = snapshot(&[("scene.a.node.cam1.visible", Value::Bool(false)), ("scene.a.node.cam2.opacity", Value::Float(0.0))], &[]);
        assert!(eval(&p, "a", &hidden).is_empty());
    }

    #[test]
    fn when_clause_hides_node() {
        let p = plan();
        assert_eq!(eval(&p, "b", &snapshot(&[("show.mode", Value::Str("offline".into()))], &[])).len(), 1);
        assert_eq!(eval(&p, "b", &snapshot(&[("show.mode", Value::Str("live".into()))], &[])).len(), 2);
    }

    fn morph_at(p: &Plan, t: f32, snap: &Snapshot) -> Vec<Item> {
        let mut res = Resolved::default();
        res.update(snap, &p.state, &p.signals);
        let mut whens = Whens::default();
        whens.reset(p.whens.len());
        let mut out = Vec::new();
        let tr = p.transition("morph");
        eval_morph(p, tr, p.scene_index["a"], p.scene_index["b"], WIDE, [1920.0, 1080.0], t, snap, &res, &mut whens, &mut MorphScratch::default(), &mut out);
        finish(&mut out, [1920.0, 1080.0]);
        out
    }

    #[test]
    fn morph_matches_by_source_and_styles_the_rest() {
        let p = plan();
        let snap = snapshot(&[("show.mode", Value::Str("live".into()))], &[]);
        let start = morph_at(&p, 0.0, &snap);
        let names = |v: &[Item]| v.iter().map(|i| p.sources[i.source as usize].name.clone()).collect::<Vec<_>>();
        // t = 0 looks like scene a (cam4 slides in from below: off canvas → culled)
        assert_eq!(names(&start), ["cam1", "cam2"]);
        let cam2 = start.iter().find(|i| p.sources[i.source as usize].name == "cam2").unwrap();
        assert_eq!(cam2.rect, [960.0, 0.0, 960.0, 1080.0]);
        let mid = morph_at(&p, 0.5, &snap);
        let cam2 = mid.iter().find(|i| p.sources[i.source as usize].name == "cam2").unwrap();
        assert_eq!(cam2.rect, [480.0, 0.0, 1440.0, 1080.0]);
        assert_eq!(cam2.radius, 5.0);
        let cam1 = mid.iter().find(|i| p.sources[i.source as usize].name == "cam1").unwrap();
        assert!((cam1.opacity - 0.5).abs() < 1e-6, "exit fade");
        assert!(!mid.iter().any(|i| p.sources[i.source as usize].name == "cam4"), "still below the canvas at t = 0.5");
        let late = morph_at(&p, 0.75, &snap);
        let cam4 = late.iter().find(|i| p.sources[i.source as usize].name == "cam4").unwrap();
        assert!((cam4.rect[1] - (0.7 * 1080.0 + 0.25 * 1080.0)).abs() < 1e-3, "slide_up enters from below");
        let end = morph_at(&p, 1.0, &snap);
        assert_eq!(names(&end), ["cam2", "cam4"]);
        assert_eq!(end[0].rect, [0.0, 0.0, 1920.0, 1080.0]);
    }

    #[test]
    fn transition_progress_from_master_clock() {
        let p = plan();
        let snap = snapshot(
            &[
                ("show.scene.program", Value::Str("b".into())),
                ("show.transition.active", Value::Bool(true)),
                ("show.transition.name", Value::Str("morph".into())),
                ("show.transition.from", Value::Str("a".into())),
                ("show.transition.start", Value::Int(1_000_000_000)),
                ("show.transition.ms", Value::Int(500)),
            ],
            &[],
        );
        let mut res = Resolved::default();
        res.update(&snap, &p.state, &p.signals);
        let s = transition_state(&p, &snap, &res, 1_250_000_000);
        assert!(s.active);
        assert!((s.progress - 0.5).abs() < 1e-6);
        assert_eq!((s.from, s.to), (Some(p.scene_index["a"]), Some(p.scene_index["b"])));
        assert_eq!(s.kind(&p), TrKind::Morph);
        let done = transition_state(&p, &snap, &res, 2_000_000_000);
        assert!(!done.active && done.progress == 1.0);
    }

    #[test]
    fn steady_state_is_allocation_free() {
        let p = plan();
        let snap = snapshot(&[("show.mode", Value::Str("live".into()))], &[]);
        let mut res = Resolved::default();
        res.update(&snap, &p.state, &p.signals);
        let mut whens = Whens::default();
        whens.reset(p.whens.len());
        let mut out = Vec::with_capacity(64);
        let mut scratch = MorphScratch::default();
        let tr = p.transition("morph");
        let (a, b) = (p.scene_index["a"], p.scene_index["b"]);
        eval_morph(&p, tr, a, b, WIDE, [1920.0, 1080.0], 0.3, &snap, &res, &mut whens, &mut scratch, &mut out);
        let s = se_alloc::Scope::begin();
        for i in 0..100 {
            out.clear();
            eval_morph(&p, tr, a, b, WIDE, [1920.0, 1080.0], i as f32 / 100.0, &snap, &res, &mut whens, &mut scratch, &mut out);
            finish(&mut out, [1920.0, 1080.0]);
            eval_layout(&p, b, WIDE, [1920.0, 1080.0], &snap, &res, &mut whens, &mut out);
        }
        assert_eq!(s.allocs(), 0);
    }
}
