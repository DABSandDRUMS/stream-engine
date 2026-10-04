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
    /// Atomic compositor membership, including the originating scene during a morph.
    pub group: Option<(u32, u32)>,
    pub group_z: i64,
    /// Custom enter/exit style during a morph: (`Plan::styles` index, presence 0–1).
    pub style: Option<(u32, f32)>,
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
    // Offsets and corner radius are in pixels of the layout's own canvas (1920 wide for Main); a
    // canvas drawn at another size (the half-size preview) scales them with its picture.
    let px = size[0] / plan.canvases[layout].width.max(1) as f32;
    for (ni, n) in s.layouts[layout].nodes.iter().enumerate() {
        let group = n.group.map(|gi| &s.layouts[layout].groups[gi as usize]);
        if group.is_some_and(|g| !g.visible || g.opacity <= 0.002
            || g.when.is_some_and(|w| !whens.eval(w as usize, &plan.whens[w as usize], snap, res)))
        {
            continue;
        }
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
        let off = [res.f32(snap, n.offset_x, n.def.offset[0]) * px, res.f32(snap, n.offset_y, n.def.offset[1]) * px];
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
            radius: res.f32(snap, n.radius, n.def.radius).max(0.0) * scale * px,
            opacity: res.f32(snap, n.opacity, n.def.opacity).clamp(0.0, 1.0),
            rotation: res.f32(snap, n.rotation, n.def.rotation).to_radians(),
            z: res.i64(snap, n.z, n.def.z),
            order: ni as u32,
            blend: n.blend,
            group: n.group.map(|gi| (scene as u32, gi)),
            group_z: group.map_or(0, |g| g.z),
            style: None,
        });
    }
}

/// Cull off-canvas/transparent items and sort by (z, order) without allocating.
pub fn finish(items: &mut Vec<Item>, size: [f32; 2]) {
    items.retain(|i| i.on_canvas(size[0], size[1]));
    items.sort_unstable_by(|a, b| {
        let az = if a.group.is_some() { a.group_z } else { a.z };
        let bz = if b.group.is_some() { b.group_z } else { b.z };
        az.cmp(&bz).then_with(|| a.group.cmp(&b.group)).then(a.z.cmp(&b.z)).then(a.order.cmp(&b.order))
    });
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
        Style::Slide => {
            let [x, y, w, h] = it.rect;
            let (cx, cy) = (x + w * 0.5, y + h * 0.5);
            // Use the same rotation-safe circle as culling so even rotated corners clear.
            let (rx, ry) = if it.rotation == 0.0 { (w * 0.5, h * 0.5) } else {
                let r = w.hypot(h) * 0.5;
                (r, r)
            };
            let distances = [cx + rx + 2.0, size[0] - cx + rx + 2.0, cy + ry + 2.0, size[1] - cy + ry + 2.0];
            let mut edge = 0;
            for i in 1..4 {
                if distances[i] < distances[edge] { edge = i; }
            }
            let travel = distances[edge].max(0.0) * (1.0 - p);
            match edge {
                0 => it.rect[0] -= travel,
                1 => it.rect[0] += travel,
                2 => it.rect[1] -= travel,
                _ => it.rect[1] += travel,
            }
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
        // drawn in place through the style's shader (the renderer fades it while the shader
        // is not compiled)
        Style::Custom(i) => it.style = Some((i, p)),
    }
}

/// Scratch buffers reused every frame.
#[derive(Default)]
pub struct MorphScratch {
    a: Vec<Item>,
    b: Vec<Item>,
    matched: Vec<bool>,
    /// Glide: the `b` index each `a` item is matched with (`u32::MAX`: none).
    pair: Vec<u32>,
}

impl MorphScratch {
    /// Pre-sized for `nodes` per scene so the frame loop never grows it.
    pub fn with_capacity(nodes: usize) -> MorphScratch {
        MorphScratch { a: Vec::with_capacity(nodes), b: Vec::with_capacity(nodes), matched: Vec::with_capacity(nodes), pair: Vec::with_capacity(nodes) }
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
    let MorphScratch { a, b, matched, .. } = scratch;
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
                if t < 0.5 {
                    m.group = s.group;
                    m.group_z = s.group_z;
                }
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

/// Interpolated geometry of a matched pair (`a` outgoing, `b` incoming) onto `m`.
fn lerp_geometry(m: &mut Item, a: &Item, b: &Item, t: f32) {
    m.rect = lerp4(a.rect, b.rect, t);
    m.crop = lerp4(a.crop, b.crop, t);
    m.radius = lerp(a.radius, b.radius, t);
    m.opacity = lerp(a.opacity, b.opacity, t);
    m.rotation = lerp(a.rotation, b.rotation, t);
}

/// Glide styles receive role-local presence. Exit fade/scale retain the outgoing image as a
/// fallback; the layer crossfade handles their disappearance without exposing black.
fn apply_glide_style(it: &mut Item, style: Style, p: f32, entering: bool, size: [f32; 2]) {
    match style {
        Style::Fade if !entering => {}
        Style::Scale if !entering => {
            let opacity = it.opacity;
            apply_style(it, style, p, entering, size);
            it.opacity = opacity;
        }
        Style::Custom(i) if entering => {
            it.style = Some((i, p));
            it.opacity *= p;
        }
        _ => apply_style(it, style, p, entering, size),
    }
    if entering && p <= 0.0 {
        it.opacity = 0.0;
    }
}

/// Clamp linear transition time to a role's configured interval.
pub fn window(u: f32, [start, end]: [f32; 2]) -> f32 {
    ((u - start) / (end - start)).clamp(0.0, 1.0)
}

pub fn glide_fade(tr: &TransitionPlan, u: f32) -> f32 {
    let p = window(u, tr.fade_window);
    p * p * (3.0 - 2.0 * p)
}

/// Glide between scene `from` and `to` at LINEAR progress `u`: the same matching and geometry as
/// [`eval_morph`], but drawn as two sides that each keep their own scene's node identity (fx,
/// mask, group) and stacking for the whole transition. `out_a` gets every outgoing item (matched
/// ones at the interpolated geometry, the rest leaving with their exit style), `out_b` every
/// incoming one (matched at the same geometry, the rest arriving with their enter style). The
/// renderer crossfades the two sides (`glide_pass`).
#[allow(clippy::too_many_arguments)]
pub fn eval_glide(
    plan: &Plan,
    tr: &TransitionPlan,
    from: usize,
    to: usize,
    layout: usize,
    size: [f32; 2],
    u: f32,
    snap: &Snapshot,
    res: &Resolved,
    whens: &mut Whens,
    scratch: &mut MorphScratch,
    out_a: &mut Vec<Item>,
    out_b: &mut Vec<Item>,
) {
    let t = ease(tr.ease, u);
    let enter = ease(Ease::Decelerate, window(u, tr.enter_window));
    let exit = 1.0 - ease(Ease::Accelerate, window(u, tr.exit_window));
    let MorphScratch { a, b, pair, .. } = scratch;
    a.clear();
    b.clear();
    eval_layout(plan, from, layout, size, snap, res, whens, a);
    eval_layout(plan, to, layout, size, snap, res, whens, b);
    pair.clear();
    pair.resize(a.len(), u32::MAX);
    for (bi, it) in b.iter().enumerate() {
        let mut m = *it;
        match a.iter().enumerate().position(|(ai, x)| pair[ai] == u32::MAX && x.source == it.source) {
            Some(ai) => {
                pair[ai] = bi as u32;
                lerp_geometry(&mut m, &a[ai], it, t);
            }
            None => apply_glide_style(&mut m, it.node(plan).enter.unwrap_or(tr.enter), enter, true, size),
        }
        out_b.push(m);
    }
    for (ai, it) in a.iter().enumerate() {
        let mut m = *it;
        match pair[ai] {
            u32::MAX => apply_glide_style(&mut m, it.node(plan).exit.unwrap_or(tr.exit), exit, false, size),
            bi => lerp_geometry(&mut m, it, &b[bi as usize], t),
        }
        out_a.push(m);
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

    #[test]
    fn offsets_and_radius_scale_with_a_smaller_canvas() {
        let p = plan();
        let snap = snapshot(&[("scene.b.node.cam2.offset_x", Value::Float(200.0)), ("scene.b.node.cam2.scale", Value::Float(1.25))], &[]);
        let at = |size: [f32; 2]| {
            let mut res = Resolved::default();
            res.update(&snap, &p.state, &p.signals);
            let mut whens = Whens::default();
            whens.reset(p.whens.len());
            let mut out = Vec::new();
            eval_layout(&p, p.scene_index["b"], WIDE, size, &snap, &res, &mut whens, &mut out);
            out.into_iter().find(|i| p.sources[i.source as usize].name == "cam2").unwrap()
        };
        let full = at([1920.0, 1080.0]);
        let half = at([960.0, 540.0]);
        // the half-size preview is the full picture scaled down: a 1.25x picture panned 200 px keeps
        // 40 px past the left edge at 1920 wide, and 20 px at 960 wide
        assert_eq!(full.rect, [-40.0, -135.0, 2400.0, 1350.0]);
        assert_eq!(half.rect, [-20.0, -67.5, 1200.0, 675.0]);
        assert_eq!((full.radius, half.radius), (12.5, 6.25));
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
    fn nearest_edge_slide_clears_canvas_and_returns_to_exact_geometry() {
        let p = plan();
        let snap = snapshot(&[], &[]);
        let base = eval(&p, "a", &snap)[0];
        for (rect, axis, sign) in [
            ([10.0, 400.0, 100.0, 80.0], 0, -1.0),
            ([1800.0, 400.0, 100.0, 80.0], 0, 1.0),
            ([800.0, 10.0, 100.0, 80.0], 1, -1.0),
            ([800.0, 990.0, 100.0, 80.0], 1, 1.0),
        ] {
            for rotation in [0.0, 0.8] {
                let mut original = base;
                original.rect = rect;
                original.rotation = rotation;
                for entering in [true, false] {
                    let mut gone = original;
                    apply_style(&mut gone, Style::Slide, 0.0, entering, [1920.0, 1080.0]);
                    assert!(!gone.on_canvas(1920.0, 1080.0), "{gone:?}");
                    assert!((gone.rect[axis] - rect[axis]) * sign > 0.0);
                    assert_eq!(gone.rect[1 - axis], rect[1 - axis]);
                    let mut present = original;
                    apply_style(&mut present, Style::Slide, 1.0, entering, [1920.0, 1080.0]);
                    assert_eq!(present, original);
                }
            }
        }
    }

    #[test]
    fn glide_roles_overlap_and_keep_scene_identity() {
        let mut p = plan();
        let tr = p.transition_index["morph"];
        p.transitions[tr].ease = Ease::Standard;
        p.transitions[tr].exit = Style::Slide;
        p.scenes[p.scene_index["b"]].layouts[WIDE].nodes[1].enter = Some(Style::Fade);
        let snap = snapshot(&[("show.mode", Value::Str("live".into()))], &[]);
        let a = eval(&p, "a", &snap);
        let b = eval(&p, "b", &snap);
        let mut res = Resolved::default();
        res.update(&snap, &p.state, &p.signals);
        let mut whens = Whens::default();
        whens.reset(p.whens.len());
        let mut scratch = MorphScratch::default();
        let mut outgoing = Vec::new();
        let mut incoming = Vec::new();
        for u in [0.0, 0.2, 0.3, 0.4, 0.5, 0.9, 1.0] {
            outgoing.clear();
            incoming.clear();
            eval_glide(&p, &p.transitions[tr], p.scene_index["a"], p.scene_index["b"], WIDE, [1920.0, 1080.0], u, &snap, &res, &mut whens, &mut scratch, &mut outgoing, &mut incoming);
            let oa = outgoing.iter().find(|it| it.source == b[0].source).unwrap();
            let ib = &incoming[0];
            assert_eq!((oa.scene, oa.node, oa.z, oa.group), (a[1].scene, a[1].node, a[1].z, a[1].group));
            assert_eq!((ib.scene, ib.node, ib.z, ib.group), (b[0].scene, b[0].node, b[0].z, b[0].group));
            assert_eq!(oa.rect, ib.rect);
            let expected = lerp4(a[1].rect, b[0].rect, ease(Ease::Standard, u));
            assert_eq!(ib.rect, expected);
            if u <= 0.3 { assert_eq!(incoming[1].opacity, 0.0); }
            if u == 0.4 {
                assert!(incoming[1].opacity > 0.0);
                assert!(outgoing[0].on_canvas(1920.0, 1080.0), "roles overlap");
            }
            if u >= 0.5 { assert!(!outgoing[0].on_canvas(1920.0, 1080.0)); }
            if u == 1.0 { assert_eq!(incoming[1], b[1]); }
        }
    }

    #[test]
    fn glide_custom_enter_presence_is_transparent_before_phase_and_continuous_after() {
        let p = plan();
        let snap = snapshot(&[], &[]);
        let original = eval(&p, "a", &snap)[0];
        for u in [0.0, 0.2, 0.3, 0.3001, 0.7, 1.0] {
            let presence = ease(Ease::Decelerate, window(u, [0.3, 1.0]));
            let mut entering = original;
            apply_glide_style(&mut entering, Style::Custom(0), presence, true, [1920.0, 1080.0]);
            assert_eq!(entering.style, Some((0, presence)));
            assert_eq!(entering.opacity, original.opacity * presence);
            if u <= 0.3 { assert!(!entering.on_canvas(1920.0, 1080.0)); }
            if u == 0.3001 { assert!(entering.opacity < 0.01); }
            if u == 1.0 { assert_eq!(entering.opacity, original.opacity); }
        }
        let mut leaving = original;
        apply_glide_style(&mut leaving, Style::Custom(0), 0.0, false, [1920.0, 1080.0]);
        assert_eq!(leaving.style, Some((0, 0.0)));
    }


    #[test]
    fn invalid_glide_windows_report_error_and_use_safe_defaults() {
        let c = config(&[
            ("project", "project", "schema = 1"),
            ("transitions", "bad", "kind = \"glide\"\nenter_window = [0.8, 0.2]\nexit_window = [-0.1, 0.5]\nfade_window = [0.4, 0.4]"),
            ("transitions", "valid", "kind = \"glide\"\nenter_window = [0, 0.8]\nexit_window = [0.1, 0.9]\nfade_window = [0, 1]"),
        ]);
        let p = Plan::build(&c, &[], PathBuf::from("/tmp"));
        assert_eq!(p.errors.len(), 3, "{:?}", p.errors);
        let tr = p.transition("bad");
        assert_eq!((tr.enter_window, tr.exit_window, tr.fade_window), ([0.3, 1.0], [0.0, 0.5], [0.15, 0.75]));
        let valid = p.transition("valid");
        assert_eq!((valid.enter_window, valid.exit_window, valid.fade_window), ([0.0, 0.8], [0.1, 0.9], [0.0, 1.0]));
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
        let mut out_b = Vec::with_capacity(64);
        let mut scratch = MorphScratch::default();
        let tr = p.transition("morph");
        let (a, b) = (p.scene_index["a"], p.scene_index["b"]);
        eval_morph(&p, tr, a, b, WIDE, [1920.0, 1080.0], 0.3, &snap, &res, &mut whens, &mut scratch, &mut out);
        out.clear();
        eval_glide(&p, tr, a, b, WIDE, [1920.0, 1080.0], 0.3, &snap, &res, &mut whens, &mut scratch, &mut out, &mut out_b);
        let s = se_alloc::Scope::begin();
        for i in 0..100 {
            out.clear();
            eval_morph(&p, tr, a, b, WIDE, [1920.0, 1080.0], i as f32 / 100.0, &snap, &res, &mut whens, &mut scratch, &mut out);
            finish(&mut out, [1920.0, 1080.0]);
            eval_layout(&p, b, WIDE, [1920.0, 1080.0], &snap, &res, &mut whens, &mut out);
            out.clear();
            out_b.clear();
            eval_glide(&p, tr, a, b, WIDE, [1920.0, 1080.0], i as f32 / 100.0, &snap, &res, &mut whens, &mut scratch, &mut out, &mut out_b);
        }
        assert_eq!(s.allocs(), 0);
    }
}
