//! Canvas editor widget: drag/resize/crop/radius handles, snapping, safe areas (§15.5).
//!
//! Node geometry is normalized to the canvas: `rect = [x, y, w, h]`, `crop = [left, top, right,
//! bottom]` insets as fractions of the source (0–1), `radius` in canvas pixels. Crop is OBS-style:
//! the source sub-rect `[l, t, 1-r, 1-b]` is stretched into `rect`, so `rect` is the visible box and
//! cropping an edge moves that edge while the source keeps its on-canvas scale.
//!
//! Everything except [`canvas_editor`] is pure geometry. The widget keeps its drag state in egui
//! temp memory keyed by the caller's id and reports edits as [`CanvasEdit`]s: one per frame while
//! dragging (`finished = false`, for live preview) and one with `finished = true` on release.

use crate::theme::Theme;
use crate::widgets::{LedState, icon, led_color};
use egui::{
    Align2, Color32, CornerRadius, CursorIcon, Event, EventFilter, FontId, Id, InputState, Key, Modifiers, Painter, PointerButton, Pos2, Rect, Response, Sense,
    Shape, Stroke, StrokeKind, TextureId, Ui, Vec2, pos2, vec2,
};

/// Smallest size a resize or crop can produce, as a fraction of the canvas on each axis.
pub const MIN_SIZE: f32 = 0.01;
/// Action-safe frame (fraction of the canvas, centered).
pub const ACTION_SAFE: f32 = 0.93;
/// Title-safe frame (fraction of the canvas, centered).
pub const TITLE_SAFE: f32 = 0.90;

/// Screen px around the canvas inside the widget, so handles on the canvas edge stay visible.
const PAD: f32 = 8.0;
/// Drawn handle size (screen px).
const HANDLE: f32 = 7.0;
/// Handle hit tolerance (screen px).
const HIT: f32 = 6.0;
/// Offset of the radius handle from the node's top-left corner (screen px).
const RADIUS_INSET: f32 = 12.0;
const LABEL_FONT: f32 = 11.0;
const SMALL_FONT: f32 = 9.5;
const LOCK: &str = "\u{f023}";
const HIDDEN: &str = "\u{f070}";
const CROP: &str = "\u{f125}";

/// One node on one canvas.
#[derive(Clone, Debug, PartialEq)]
pub struct CanvasNode {
    pub id: String,
    pub label: String,
    /// `[x, y, w, h]` normalized to the canvas.
    pub rect: [f32; 4],
    /// `[left, top, right, bottom]` insets as fractions of the source (0–1).
    pub crop: [f32; 4],
    /// Corner radius in canvas pixels.
    pub radius: f32,
    pub z: i32,
    /// Hidden nodes are drawn dashed, stay editable, and are not snap targets.
    pub visible: bool,
    /// Selectable but not editable.
    pub locked: bool,
    /// A binding modulates this node (magenta outline).
    pub modulated: bool,
}

/// Display and interaction options for one canvas.
#[derive(Clone, Debug)]
pub struct CanvasOpts {
    /// Canvas size in pixels (`wide` 1920×1080, `tall` 1080×1920).
    pub canvas_px: [f32; 2],
    /// Grid step normalized to the canvas (e.g. `1/48`): drawn, and snapped to when `snap`.
    pub grid: Option<f32>,
    pub snap: bool,
    /// Snap distance in screen pixels.
    pub snap_px: f32,
    /// Action-safe (93%) and title-safe (90%) frames.
    pub safe_area: bool,
    /// TikTok UI overlay zones ([`TikTokGuides::DEFAULT`]), for the `tall` canvas.
    pub tiktok_guides: bool,
    pub show_labels: bool,
    /// Tally border around the canvas.
    pub tally: Option<LedState>,
    /// Rendered canvas texture and its uv rect; `None` = dark fill.
    pub background: Option<(TextureId, Rect)>,
    /// Draw no background at all (neither fill nor texture): the caller paints the canvas
    /// contents itself, into [`canvas_rect`] of the widget rect.
    pub transparent: bool,
    /// The scene is on program: red live edge, edits reach the stream.
    pub live: bool,
}

impl CanvasOpts {
    pub fn new(canvas_px: [f32; 2]) -> Self {
        CanvasOpts {
            canvas_px,
            grid: None,
            snap: true,
            snap_px: 8.0,
            safe_area: false,
            tiktok_guides: false,
            show_labels: true,
            tally: None,
            background: None,
            transparent: false,
            live: false,
        }
    }
}

/// A grab point on a node: its body or one of the 8 resize handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Handle {
    Move,
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
}

impl Handle {
    /// The 8 resize handles, clockwise from the top-left corner.
    pub const RESIZE: [Handle; 8] =
        [Handle::TopLeft, Handle::Top, Handle::TopRight, Handle::Right, Handle::BottomRight, Handle::Bottom, Handle::BottomLeft, Handle::Left];

    pub fn is_corner(self) -> bool {
        matches!(self, Handle::TopLeft | Handle::TopRight | Handle::BottomRight | Handle::BottomLeft)
    }

    /// The edge an edge handle sits on.
    pub fn edge(self) -> Option<Edge> {
        match self {
            Handle::Left => Some(Edge::Left),
            Handle::Top => Some(Edge::Top),
            Handle::Right => Some(Edge::Right),
            Handle::Bottom => Some(Edge::Bottom),
            _ => None,
        }
    }

    /// Which edge moves on each axis: -1 = left/top, 1 = right/bottom, 0 = none.
    fn dirs(self) -> (i8, i8) {
        match self {
            Handle::Move => (0, 0),
            Handle::TopLeft => (-1, -1),
            Handle::Top => (0, -1),
            Handle::TopRight => (1, -1),
            Handle::Right => (1, 0),
            Handle::BottomRight => (1, 1),
            Handle::Bottom => (0, 1),
            Handle::BottomLeft => (-1, 1),
            Handle::Left => (-1, 0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Edge {
    Left,
    Top,
    Right,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EditKind {
    Move,
    Resize(Handle),
    /// Alt + edge handle: crop that edge.
    Crop(Edge),
    /// Horizontal drag of the radius handle.
    Radius,
}

/// A geometry edit of one node: the full draft geometry, not a delta.
#[derive(Clone, Debug, PartialEq)]
pub struct CanvasEdit {
    pub node: String,
    pub kind: EditKind,
    pub rect: [f32; 4],
    pub crop: [f32; 4],
    pub radius: f32,
    /// `false` while dragging (preview), `true` once on release or per arrow-key nudge (commit).
    /// Escape cancels a drag by finishing with the start geometry.
    pub finished: bool,
}

pub struct CanvasResponse {
    pub response: Response,
    pub edit: Option<CanvasEdit>,
    /// The selection after this frame (store it and pass it back next frame).
    pub selected: Option<String>,
    /// The node under the pointer (or being dragged).
    pub hovered: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    X,
    Y,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    HCenter,
    Right,
    Top,
    VCenter,
    Bottom,
}

/// Snap sources besides the canvas edges and center lines (always included).
#[derive(Clone, Copy, Debug, Default)]
pub struct SnapTargets<'a> {
    /// Other visible nodes' rects (the dragged node excluded): edges and centers.
    pub others: &'a [[f32; 4]],
    /// Grid step (normalized).
    pub grid: Option<f32>,
}

/// A snapped rect and the guide lines it snapped to (normalized canvas coordinates).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Snapped {
    pub rect: [f32; 4],
    /// Vertical guide at this x.
    pub guide_x: Option<f32>,
    /// Horizontal guide at this y.
    pub guide_y: Option<f32>,
}

/// A labeled overlay zone, normalized `[x, y, w, h]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GuideZone {
    pub label: &'static str,
    pub rect: [f32; 4],
}

/// Where the TikTok app draws its UI over a 9:16 stream: keep text and faces out of these.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TikTokGuides {
    /// Status bar and LIVE / Following / For You tabs.
    pub top_bar: GuideZone,
    /// Like / comment / share / gift buttons.
    pub action_rail: GuideZone,
    /// Username, caption, music ticker, comment input.
    pub caption: GuideZone,
    /// Left caption margin.
    pub caption_margin: GuideZone,
}

impl TikTokGuides {
    pub const DEFAULT: TikTokGuides = TikTokGuides {
        top_bar: GuideZone { label: "top bar", rect: [0.0, 0.0, 1.0, 0.085] },
        action_rail: GuideZone { label: "action rail", rect: [0.86, 0.36, 0.14, 0.48] },
        caption: GuideZone { label: "caption / music", rect: [0.0, 0.78, 1.0, 0.22] },
        caption_margin: GuideZone { label: "caption margin", rect: [0.0, 0.0, 0.045, 1.0] },
    };

    pub fn zones(&self) -> [&GuideZone; 4] {
        [&self.top_bar, &self.action_rail, &self.caption, &self.caption_margin]
    }
}

impl Default for TikTokGuides {
    fn default() -> Self {
        TikTokGuides::DEFAULT
    }
}

// ---------------------------------------------------------------------------------------------
// Coordinates

/// The largest rect with the canvas aspect ratio, centered in `avail`.
pub fn fit_canvas(avail: Rect, canvas_px: [f32; 2]) -> Rect {
    let aspect = canvas_px[0] / canvas_px[1];
    if !aspect.is_finite() || aspect <= 0.0 || avail.width() <= 0.0 || avail.height() <= 0.0 {
        return avail;
    }
    let size =
        if avail.width() / avail.height() > aspect { vec2(avail.height() * aspect, avail.height()) } else { vec2(avail.width(), avail.width() / aspect) };
    Rect::from_center_size(avail.center(), size)
}

/// Where [`canvas_editor`] puts the canvas inside its widget rect (`CanvasResponse::response.rect`).
pub fn canvas_rect(widget: Rect, canvas_px: [f32; 2]) -> Rect {
    fit_canvas(widget.shrink(PAD), canvas_px)
}

pub fn to_screen(rect: [f32; 4], canvas: Rect) -> Rect {
    Rect::from_min_size(
        pos2(canvas.min.x + rect[0] * canvas.width(), canvas.min.y + rect[1] * canvas.height()),
        vec2(rect[2] * canvas.width(), rect[3] * canvas.height()),
    )
}

pub fn to_norm(rect: Rect, canvas: Rect) -> [f32; 4] {
    let (w, h) = (canvas.width(), canvas.height());
    [(rect.min.x - canvas.min.x) / w, (rect.min.y - canvas.min.y) / h, rect.width() / w, rect.height() / h]
}

pub fn pos_to_norm(pos: Pos2, canvas: Rect) -> [f32; 2] {
    [(pos.x - canvas.min.x) / canvas.width(), (pos.y - canvas.min.y) / canvas.height()]
}

/// `[x, y, w, h]` in canvas pixels (inspector display).
pub fn rect_px(rect: [f32; 4], canvas_px: [f32; 2]) -> [f32; 4] {
    [rect[0] * canvas_px[0], rect[1] * canvas_px[1], rect[2] * canvas_px[0], rect[3] * canvas_px[1]]
}

/// Inverse of [`rect_px`] (inspector input).
pub fn rect_from_px(px: [f32; 4], canvas_px: [f32; 2]) -> [f32; 4] {
    [px[0] / canvas_px[0], px[1] / canvas_px[1], px[2] / canvas_px[0], px[3] / canvas_px[1]]
}

/// The uncropped source's extent on the canvas (normalized): `rect` grown by the crop insets.
pub fn source_rect(rect: [f32; 4], crop: [f32; 4]) -> [f32; 4] {
    let fw = rect[2] / (1.0 - crop[0] - crop[2]).max(1e-4);
    let fh = rect[3] / (1.0 - crop[1] - crop[3]).max(1e-4);
    [rect[0] - crop[0] * fw, rect[1] - crop[1] * fh, fw, fh]
}

/// A centered frame covering `frac` of the canvas on each axis.
pub fn safe_rect(frac: f32) -> [f32; 4] {
    let m = (1.0 - frac) * 0.5;
    [m, m, frac, frac]
}

// ---------------------------------------------------------------------------------------------
// Hit testing

pub fn handle_pos(rect: Rect, handle: Handle) -> Pos2 {
    match handle {
        Handle::Move => rect.center(),
        Handle::TopLeft => rect.left_top(),
        Handle::Top => rect.center_top(),
        Handle::TopRight => rect.right_top(),
        Handle::Right => rect.right_center(),
        Handle::BottomRight => rect.right_bottom(),
        Handle::Bottom => rect.center_bottom(),
        Handle::BottomLeft => rect.left_bottom(),
        Handle::Left => rect.left_center(),
    }
}

/// The grab point of a node's screen `rect` at `pos`: a corner within `tol` beats an edge within
/// `tol` (anywhere along it), which beats the body ([`Handle::Move`]).
pub fn hit_handle(rect: Rect, pos: Pos2, tol: f32) -> Option<Handle> {
    if !rect.expand(tol).contains(pos) {
        return None;
    }
    let cheb = |p: Pos2| (p.x - pos.x).abs().max((p.y - pos.y).abs());
    let nearest = |hs: &[Handle], dist: &dyn Fn(Handle) -> f32| {
        hs.iter().map(|&h| (dist(h), h)).filter(|(d, _)| *d <= tol).min_by(|a, b| a.0.total_cmp(&b.0)).map(|(_, h)| h)
    };
    let corners = [Handle::TopLeft, Handle::TopRight, Handle::BottomRight, Handle::BottomLeft];
    if let Some(h) = nearest(&corners, &|h| cheb(handle_pos(rect, h))) {
        return Some(h);
    }
    let edge_dist = |h: Handle| match h {
        Handle::Left => (pos.x - rect.left()).abs(),
        Handle::Right => (pos.x - rect.right()).abs(),
        Handle::Top => (pos.y - rect.top()).abs(),
        _ => (pos.y - rect.bottom()).abs(),
    };
    if let Some(h) = nearest(&[Handle::Left, Handle::Top, Handle::Right, Handle::Bottom], &edge_dist) {
        return Some(h);
    }
    rect.contains(pos).then_some(Handle::Move)
}

/// The radius handle: inside the top-left corner, shifted right by the on-screen radius.
/// `None` when the node is too small on screen to fit it.
fn radius_handle(rect: Rect, radius_screen: f32) -> Option<Pos2> {
    if rect.width() < 4.0 * RADIUS_INSET || rect.height() < 3.0 * RADIUS_INSET {
        return None;
    }
    let x = (RADIUS_INSET + radius_screen.max(0.0)).min(rect.width() * 0.5);
    Some(rect.left_top() + vec2(x, RADIUS_INSET))
}

fn rect_contains(r: [f32; 4], p: [f32; 2]) -> bool {
    let (x0, x1) = (r[0].min(r[0] + r[2]), r[0].max(r[0] + r[2]));
    let (y0, y1) = (r[1].min(r[1] + r[3]), r[1].max(r[1] + r[3]));
    (x0..=x1).contains(&p[0]) && (y0..=y1).contains(&p[1])
}

/// The topmost node (highest `z`, later index on ties) containing `pos` (normalized); visible
/// nodes win over hidden ones so a hidden node never blocks clicks on visible content.
pub fn pick_node(nodes: &[CanvasNode], pos: [f32; 2]) -> Option<usize> {
    let top = |visible: bool| {
        nodes.iter().enumerate().filter(|(_, n)| n.visible == visible && rect_contains(n.rect, pos)).max_by_key(|(i, n)| (n.z, *i)).map(|(i, _)| i)
    };
    top(true).or_else(|| top(false))
}

// ---------------------------------------------------------------------------------------------
// Edits

fn offset(rect: [f32; 4], d: [f32; 2]) -> [f32; 4] {
    [rect[0] + d[0], rect[1] + d[1], rect[2], rect[3]]
}

fn min_scale(w: f32, h: f32) -> f32 {
    (MIN_SIZE / w).max(MIN_SIZE / h)
}

/// `start` scaled by `s` (at least to the min size) about the handle's opposite anchor; axes the
/// handle doesn't move stay centered.
fn scaled(start: [f32; 4], dirs: (i8, i8), s: f32) -> [f32; 4] {
    let [x, y, w, h] = start;
    let s = s.max(min_scale(w, h));
    let (nw, nh) = (w * s, h * s);
    let place = |p: f32, size: f32, nsize: f32, dir: i8| match dir {
        -1 => p + size - nsize,
        1 => p,
        _ => p + (size - nsize) * 0.5,
    };
    [place(x, w, nw, dirs.0), place(y, h, nh, dirs.1), nw, nh]
}

fn resize_span(lo: f32, hi: f32, dir: i8, d: f32) -> (f32, f32) {
    match dir {
        -1 => ((lo + d).min(hi - MIN_SIZE), hi),
        1 => (lo, (hi + d).max(lo + MIN_SIZE)),
        _ => (lo, hi),
    }
}

/// Drag `handle` of `start` by `delta` (normalized). Never inverts: each side stays at least
/// [`MIN_SIZE`]. `keep_aspect` preserves `w / h` (corners follow the dominant axis, edges scale
/// the other axis about its center). Positions are not clamped: nodes may leave the canvas.
pub fn resize(start: [f32; 4], handle: Handle, delta: [f32; 2], keep_aspect: bool) -> [f32; 4] {
    let [x, y, w, h] = start;
    let (hx, hy) = handle.dirs();
    if (hx, hy) == (0, 0) {
        return offset(start, delta);
    }
    if keep_aspect && w > 0.0 && h > 0.0 {
        let sx = (w + f32::from(hx) * delta[0]) / w;
        let sy = (h + f32::from(hy) * delta[1]) / h;
        let s = match (hx, hy) {
            (0, _) => sy,
            (_, 0) => sx,
            _ => sx.max(sy),
        };
        return scaled(start, (hx, hy), s);
    }
    let (x0, x1) = resize_span(x, x + w, hx, delta[0]);
    let (y0, y1) = resize_span(y, y + h, hy, delta[1]);
    [x0, y0, x1 - x0, y1 - y0]
}

/// New inset for one crop edge: `cur + d`, within `0..=1 - other`, keeping at least `min_frac`
/// of the source visible (an already smaller visible part may only grow).
fn crop_inset(cur: f32, other: f32, d: f32, min_frac: f32) -> f32 {
    let hi = (1.0 - other - min_frac).max(cur.min(1.0 - other)).max(0.0);
    (cur + d).max(0.0).min(hi)
}

/// Crop one edge by dragging it `delta` (normalized canvas units): the edge follows the pointer
/// and the source keeps its on-canvas scale. Insets stay within `0..=1 - opposite inset` and the
/// visible box never shrinks below [`MIN_SIZE`]. Returns `(rect, crop)`.
pub fn crop_edge(rect: [f32; 4], crop: [f32; 4], edge: Edge, delta: [f32; 2]) -> ([f32; 4], [f32; 4]) {
    let [x, y, w, h] = rect;
    let [l, t, r, b] = crop;
    let full_w = w / (1.0 - l - r).max(1e-4);
    let full_h = h / (1.0 - t - b).max(1e-4);
    let (mut rect, mut crop) = (rect, crop);
    match edge {
        Edge::Left => {
            let v = crop_inset(l, r, delta[0] / full_w, MIN_SIZE / full_w);
            let moved = (v - l) * full_w;
            crop[0] = v;
            rect[0] = x + moved;
            rect[2] = w - moved;
        }
        Edge::Right => {
            let v = crop_inset(r, l, -delta[0] / full_w, MIN_SIZE / full_w);
            crop[2] = v;
            rect[2] = w - (v - r) * full_w;
        }
        Edge::Top => {
            let v = crop_inset(t, b, delta[1] / full_h, MIN_SIZE / full_h);
            let moved = (v - t) * full_h;
            crop[1] = v;
            rect[1] = y + moved;
            rect[3] = h - moved;
        }
        Edge::Bottom => {
            let v = crop_inset(b, t, -delta[1] / full_h, MIN_SIZE / full_h);
            crop[3] = v;
            rect[3] = h - (v - b) * full_h;
        }
    }
    (rect, crop)
}

/// Radius after a horizontal drag of `dx_px` canvas pixels: at least 0, at most half the node's
/// shorter side (larger radii look identical).
pub fn drag_radius(start: f32, dx_px: f32, rect: [f32; 4], canvas_px: [f32; 2]) -> f32 {
    let max = 0.5 * (rect[2] * canvas_px[0]).abs().min((rect[3] * canvas_px[1]).abs());
    (start + dx_px).max(0.0).min(max)
}

/// Move `rect` by `px` canvas pixels (arrow-key nudge).
pub fn nudge(rect: [f32; 4], px: [f32; 2], canvas_px: [f32; 2]) -> [f32; 4] {
    offset(rect, [px[0] / canvas_px[0], px[1] / canvas_px[1]])
}

// ---------------------------------------------------------------------------------------------
// Snapping

/// Snap distance `snap_px` (screen) in normalized units per axis for a canvas shown at `canvas`.
pub fn snap_threshold(snap_px: f32, canvas: Rect) -> [f32; 2] {
    if canvas.width() <= 0.0 || canvas.height() <= 0.0 {
        return [0.0; 2];
    }
    [snap_px / canvas.width(), snap_px / canvas.height()]
}

/// The closest target to `v` within `thr`: `(distance, target)`. Ties keep the earlier source
/// (canvas, then nodes, then grid).
fn nearest(v: f32, axis: Axis, targets: &SnapTargets, thr: f32) -> Option<(f32, f32)> {
    if thr.is_nan() || thr <= 0.0 || !v.is_finite() {
        return None;
    }
    let mut best: Option<(f32, f32)> = None;
    let mut consider = |target: f32| {
        let d = (target - v).abs();
        if d <= thr && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, target));
        }
    };
    for c in [0.0, 0.5, 1.0] {
        consider(c);
    }
    for r in targets.others {
        let (lo, size) = match axis {
            Axis::X => (r[0], r[2]),
            Axis::Y => (r[1], r[3]),
        };
        consider(lo);
        consider(lo + size * 0.5);
        consider(lo + size);
    }
    if let Some(step) = targets.grid.filter(|s| s.is_finite() && *s > 0.0) {
        consider((v / step).round() * step);
    }
    best
}

/// Best snap of any of `features` (positions on one axis): `(offset to apply, target)`.
fn snap_features(features: [f32; 3], axis: Axis, targets: &SnapTargets, thr: f32) -> Option<(f32, f32)> {
    let mut best: Option<(f32, f32, f32)> = None;
    for f in features {
        if let Some((d, target)) = nearest(f, axis, targets, thr)
            && best.is_none_or(|b| d < b.0)
        {
            best = Some((d, target - f, target));
        }
    }
    best.map(|(_, off, target)| (off, target))
}

/// Snap a moved rect: its left/center/right to vertical targets and top/center/bottom to
/// horizontal ones, each axis independently, within `threshold` (normalized, per axis).
pub fn snap_move(rect: [f32; 4], targets: &SnapTargets, threshold: [f32; 2]) -> Snapped {
    let mut out = Snapped { rect, guide_x: None, guide_y: None };
    let [x, y, w, h] = rect;
    if let Some((off, target)) = snap_features([x, x + w * 0.5, x + w], Axis::X, targets, threshold[0]) {
        out.rect[0] += off;
        out.guide_x = Some(target);
    }
    if let Some((off, target)) = snap_features([y, y + h * 0.5, y + h], Axis::Y, targets, threshold[1]) {
        out.rect[1] += off;
        out.guide_y = Some(target);
    }
    out
}

/// [`resize`], then snap the moving edges. Free resize snaps each moving edge on its own (never
/// below [`MIN_SIZE`]); with `keep_aspect` the closest edge snap drives the scale.
pub fn snap_resize(start: [f32; 4], handle: Handle, delta: [f32; 2], keep_aspect: bool, targets: &SnapTargets, threshold: [f32; 2]) -> Snapped {
    let (hx, hy) = handle.dirs();
    if (hx, hy) == (0, 0) {
        return snap_move(offset(start, delta), targets, threshold);
    }
    let raw = resize(start, handle, delta, keep_aspect);
    let mut out = Snapped { rect: raw, guide_x: None, guide_y: None };
    let moving_edge = |r: [f32; 4], pos: usize, dir: i8| if dir < 0 { r[pos] } else { r[pos] + r[pos + 2] };
    if keep_aspect && start[2] > 0.0 && start[3] > 0.0 {
        // (distance relative to threshold, scale, axis, target)
        let mut best: Option<(f32, f32, Axis, f32)> = None;
        for (axis, pos, dir) in [(Axis::X, 0, hx), (Axis::Y, 1, hy)] {
            if dir == 0 {
                continue;
            }
            let thr = threshold[pos];
            let Some((d, target)) = nearest(moving_edge(raw, pos, dir), axis, targets, thr) else { continue };
            let size = if dir < 0 { start[pos] + start[pos + 2] - target } else { target - start[pos] };
            let s = size / start[pos + 2];
            if s >= min_scale(start[2], start[3]) && best.is_none_or(|b| d / thr < b.0) {
                best = Some((d / thr, s, axis, target));
            }
        }
        if let Some((_, s, axis, target)) = best {
            out.rect = scaled(start, (hx, hy), s);
            match axis {
                Axis::X => out.guide_x = Some(target),
                Axis::Y => out.guide_y = Some(target),
            }
        }
        return out;
    }
    for (axis, pos, dir) in [(Axis::X, 0, hx), (Axis::Y, 1, hy)] {
        if dir == 0 {
            continue;
        }
        let (lo, hi) = (raw[pos], raw[pos] + raw[pos + 2]);
        let Some((_, target)) = nearest(moving_edge(raw, pos, dir), axis, targets, threshold[pos]) else { continue };
        let (lo, hi) = if dir < 0 { (target, hi) } else { (lo, target) };
        if hi - lo >= MIN_SIZE {
            out.rect[pos] = lo;
            out.rect[pos + 2] = hi - lo;
            match axis {
                Axis::X => out.guide_x = Some(target),
                Axis::Y => out.guide_y = Some(target),
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Alignment tools

fn bounds(rects: &[[f32; 4]]) -> [f32; 4] {
    let (mut x0, mut y0, mut x1, mut y1) = (f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY);
    for r in rects {
        x0 = x0.min(r[0]);
        y0 = y0.min(r[1]);
        x1 = x1.max(r[0] + r[2]);
        y1 = y1.max(r[1] + r[3]);
    }
    [x0, y0, x1 - x0, y1 - y0]
}

/// Align rects: a single rect aligns to the canvas, several align to their bounding box.
pub fn align(rects: &[[f32; 4]], how: Align) -> Vec<[f32; 4]> {
    let b = if rects.len() == 1 { fill_canvas() } else { bounds(rects) };
    rects
        .iter()
        .map(|r| {
            let mut o = *r;
            match how {
                Align::Left => o[0] = b[0],
                Align::HCenter => o[0] = b[0] + (b[2] - r[2]) * 0.5,
                Align::Right => o[0] = b[0] + b[2] - r[2],
                Align::Top => o[1] = b[1],
                Align::VCenter => o[1] = b[1] + (b[3] - r[3]) * 0.5,
                Align::Bottom => o[1] = b[1] + b[3] - r[3],
            }
            o
        })
        .collect()
}

/// Equal gaps between rects along `axis`; the first and last (by position) stay put. Fewer than
/// 3 rects are returned unchanged. Output order matches input order.
pub fn distribute(rects: &[[f32; 4]], axis: Axis) -> Vec<[f32; 4]> {
    let mut out = rects.to_vec();
    if rects.len() < 3 {
        return out;
    }
    let (p, s) = match axis {
        Axis::X => (0, 2),
        Axis::Y => (1, 3),
    };
    let mut order: Vec<usize> = (0..rects.len()).collect();
    order.sort_by(|&a, &b| rects[a][p].total_cmp(&rects[b][p]));
    let start = rects[order[0]][p];
    let last = rects[order[order.len() - 1]];
    let total: f32 = rects.iter().map(|r| r[s]).sum();
    let gap = (last[p] + last[s] - start - total) / (rects.len() - 1) as f32;
    let mut cur = start;
    for &i in &order {
        out[i][p] = cur;
        cur += rects[i][s] + gap;
    }
    out
}

/// The whole canvas.
pub fn fill_canvas() -> [f32; 4] {
    [0.0, 0.0, 1.0, 1.0]
}

/// Shrink `rect` about its center so its pixel aspect (w/h) equals `aspect` (e.g. a 16:9 source
/// on `tall`). Invalid aspects or empty rects are returned unchanged.
pub fn fit_aspect(rect: [f32; 4], aspect: f32, canvas_px: [f32; 2]) -> [f32; 4] {
    let [x, y, w, h] = rect;
    let (wp, hp) = (w * canvas_px[0], h * canvas_px[1]);
    if !aspect.is_finite() || aspect <= 0.0 || wp <= 0.0 || hp <= 0.0 {
        return rect;
    }
    let (nwp, nhp) = if wp / hp > aspect { (hp * aspect, hp) } else { (wp, wp / aspect) };
    let (nw, nh) = (nwp / canvas_px[0], nhp / canvas_px[1]);
    [x + (w - nw) * 0.5, y + (h - nh) * 0.5, nw, nh]
}

// ---------------------------------------------------------------------------------------------
// Widget

/// Drag state in egui temp memory (keyed by the widget id).
#[derive(Clone, Debug)]
struct DragState {
    node: String,
    kind: EditKind,
    origin: Pos2,
    last: Pos2,
    rect: [f32; 4],
    crop: [f32; 4],
    radius: f32,
}

struct Draft {
    rect: [f32; 4],
    crop: [f32; 4],
    radius: f32,
    guide_x: Option<f32>,
    guide_y: Option<f32>,
}

/// What a press at `pos` grabs: `(node index, edit)`; `edit` is `None` for locked nodes (select
/// only). Handles of the (unlocked) selected node come first: corner > radius > edge; then the
/// topmost node body.
fn grab(nodes: &[CanvasNode], sel: Option<usize>, canvas: Rect, scale: f32, pos: Pos2, alt: bool) -> Option<(usize, Option<EditKind>)> {
    if let Some(i) = sel
        && let Some(n) = nodes.get(i)
        && !n.locked
    {
        let sr = to_screen(n.rect, canvas);
        let hit = hit_handle(sr, pos, HIT);
        if let Some(h) = hit.filter(|h| h.is_corner()) {
            return Some((i, Some(EditKind::Resize(h))));
        }
        if radius_handle(sr, n.radius * scale).is_some_and(|p| p.distance(pos) <= HIT) {
            return Some((i, Some(EditKind::Radius)));
        }
        if let Some(h) = hit
            && let Some(e) = h.edge()
        {
            return Some((i, Some(if alt { EditKind::Crop(e) } else { EditKind::Resize(h) })));
        }
    }
    let j = pick_node(nodes, pos_to_norm(pos, canvas))?;
    Some((j, (!nodes[j].locked).then_some(EditKind::Move)))
}

fn draft(st: &DragState, pointer: Pos2, canvas: Rect, scale: f32, mods: Modifiers, nodes: &[CanvasNode], opts: &CanvasOpts) -> Draft {
    let d = pointer - st.origin;
    let dn = [d.x / canvas.width(), d.y / canvas.height()];
    let snapping = opts.snap && !mods.ctrl && matches!(st.kind, EditKind::Move | EditKind::Resize(_));
    let others: Vec<[f32; 4]> = if snapping { nodes.iter().filter(|n| n.visible && n.id != st.node).map(|n| n.rect).collect() } else { Vec::new() };
    let targets = SnapTargets { others: &others, grid: opts.grid };
    let thr = if snapping { snap_threshold(opts.snap_px, canvas) } else { [0.0; 2] };
    let mut out = Draft { rect: st.rect, crop: st.crop, radius: st.radius, guide_x: None, guide_y: None };
    let snapped = match st.kind {
        EditKind::Move => Some(snap_move(offset(st.rect, dn), &targets, thr)),
        EditKind::Resize(h) => Some(snap_resize(st.rect, h, dn, mods.shift, &targets, thr)),
        EditKind::Crop(e) => {
            (out.rect, out.crop) = crop_edge(st.rect, st.crop, e, dn);
            None
        }
        EditKind::Radius => {
            out.radius = drag_radius(st.radius, d.x / scale, st.rect, opts.canvas_px);
            None
        }
    };
    if let Some(s) = snapped {
        out.rect = s.rect;
        out.guide_x = s.guide_x;
        out.guide_y = s.guide_y;
    }
    out
}

/// Remove arrow-key presses from the input; returns the nudge in canvas px (Shift = 10 px).
fn take_nudge(i: &mut InputState) -> [f32; 2] {
    let mut d = [0.0f32; 2];
    i.events.retain(|e| {
        let Event::Key { key, pressed: true, modifiers, .. } = e else { return true };
        let step = if modifiers.shift { 10.0 } else { 1.0 };
        match key {
            Key::ArrowLeft => d[0] -= step,
            Key::ArrowRight => d[0] += step,
            Key::ArrowUp => d[1] -= step,
            Key::ArrowDown => d[1] += step,
            _ => return true,
        }
        false
    });
    d
}

fn cursor_for(kind: EditKind, dragging: bool) -> CursorIcon {
    match kind {
        EditKind::Move if dragging => CursorIcon::Grabbing,
        EditKind::Move => CursorIcon::Grab,
        EditKind::Resize(Handle::TopLeft | Handle::BottomRight) => CursorIcon::ResizeNwSe,
        EditKind::Resize(Handle::TopRight | Handle::BottomLeft) => CursorIcon::ResizeNeSw,
        EditKind::Resize(Handle::Left | Handle::Right) => CursorIcon::ResizeHorizontal,
        EditKind::Resize(_) => CursorIcon::ResizeVertical,
        EditKind::Crop(Edge::Left | Edge::Right) => CursorIcon::ResizeColumn,
        EditKind::Crop(_) => CursorIcon::ResizeRow,
        EditKind::Radius => CursorIcon::ResizeHorizontal,
    }
}

/// The canvas editor for one canvas (call once per canvas for side-by-side `wide` + `tall`, with
/// distinct ids). Allocates exactly `size`; the canvas is letterboxed inside ([`canvas_rect`]).
///
/// Mouse: click selects the topmost node (empty space deselects); drag a body to move; drag a
/// handle of the selected node to resize (Shift keeps aspect); Alt + edge handle crops that edge;
/// the circle inside the top-left corner sets the radius (drag horizontally); Ctrl disables
/// snapping; Escape cancels. Keyboard (widget focused): arrows nudge 1 px, Shift+arrows 10 px.
/// Locked nodes are selectable only. Apply finished edits to `nodes` right away so consecutive
/// nudges accumulate.
pub fn canvas_editor(ui: &mut Ui, t: &Theme, id: Id, size: Vec2, nodes: &[CanvasNode], selected: Option<&str>, opts: &CanvasOpts) -> CanvasResponse {
    let (outer, _) = ui.allocate_exact_size(size, Sense::hover());
    let mut response = ui.interact(outer, id, Sense::click_and_drag());
    let canvas = canvas_rect(outer, opts.canvas_px);
    let mut out = CanvasResponse { response: response.clone(), edit: None, selected: selected.map(str::to_owned), hovered: None };
    if canvas.width() < 1.0 || canvas.height() < 1.0 || opts.canvas_px[0] <= 0.0 || opts.canvas_px[1] <= 0.0 {
        return out;
    }
    let scale = canvas.width() / opts.canvas_px[0];
    let state_id = id.with("canvas_drag");
    let (mods, hover, press_origin, escape) = ui.input(|i| (i.modifiers, i.pointer.hover_pos(), i.pointer.press_origin(), i.key_pressed(Key::Escape)));
    let index_of = |s: &str| nodes.iter().position(|n| n.id == s);
    let mut sel = selected.and_then(index_of);
    let mut changed = false;
    let mut state: Option<DragState> = ui.data(|d| d.get_temp(state_id));
    let mut guides = (None, None);

    if response.drag_started_by(PointerButton::Primary) {
        state = None;
        if let Some(at) = press_origin.or(response.interact_pointer_pos()) {
            match grab(nodes, sel, canvas, scale, at, mods.alt) {
                Some((i, kind)) => {
                    if sel != Some(i) {
                        sel = Some(i);
                        out.selected = Some(nodes[i].id.clone());
                        changed = true;
                    }
                    if let Some(kind) = kind {
                        let n = &nodes[i];
                        state = Some(DragState { node: n.id.clone(), kind, origin: at, last: at, rect: n.rect, crop: n.crop, radius: n.radius });
                    }
                }
                None if out.selected.is_some() => {
                    sel = None;
                    out.selected = None;
                    changed = true;
                }
                None => {}
            }
        }
        response.request_focus();
    }

    let mut active: Option<EditKind> = None;
    if let Some(mut st) = state.take() {
        // Escape (or the pointer leaving the window) stops a drag without a button release.
        let dragging = response.dragged();
        let stopped = response.drag_stopped();
        if (dragging || stopped) && nodes.iter().any(|n| n.id == st.node) {
            st.last = response.interact_pointer_pos().unwrap_or(st.last);
            let (rect, crop, radius) = if stopped && escape {
                (st.rect, st.crop, st.radius)
            } else {
                let d = draft(&st, st.last, canvas, scale, mods, nodes, opts);
                if !stopped {
                    guides = (d.guide_x, d.guide_y);
                }
                (d.rect, d.crop, d.radius)
            };
            out.edit = Some(CanvasEdit { node: st.node.clone(), kind: st.kind, rect, crop, radius, finished: stopped });
            changed = true;
            if stopped {
                ui.data_mut(|d| d.remove::<DragState>(state_id));
            } else {
                active = Some(st.kind);
                ui.ctx().set_cursor_icon(cursor_for(st.kind, true));
                ui.data_mut(|d| d.insert_temp(state_id, st.clone()));
                state = Some(st);
            }
        } else {
            // The drag ended while the widget was not shown, or its node is gone.
            ui.data_mut(|d| d.remove::<DragState>(state_id));
        }
    }

    if response.clicked() {
        if let Some(p) = response.interact_pointer_pos() {
            let hit = grab(nodes, sel, canvas, scale, p, mods.alt).map(|(i, _)| i);
            if hit != sel {
                sel = hit;
                out.selected = hit.map(|i| nodes[i].id.clone());
                changed = true;
            }
        }
        response.request_focus();
    }

    if response.has_focus() {
        ui.memory_mut(|m| m.set_focus_lock_filter(id, EventFilter { horizontal_arrows: true, vertical_arrows: true, ..Default::default() }));
        if state.is_none()
            && out.edit.is_none()
            && let Some(n) = sel.map(|i| &nodes[i]).filter(|n| !n.locked)
        {
            let d = ui.input_mut(take_nudge);
            if d != [0.0; 2] {
                out.edit = Some(CanvasEdit {
                    node: n.id.clone(),
                    kind: EditKind::Move,
                    rect: nudge(n.rect, d, opts.canvas_px),
                    crop: n.crop,
                    radius: n.radius,
                    finished: true,
                });
                changed = true;
            }
        }
    }

    // Hover target (node + handle), for highlight, cursor, and the response.
    let mut hover_kind = None;
    let hovered_idx = if let Some(st) = &state {
        index_of(&st.node)
    } else if response.hovered()
        && let Some(p) = hover
    {
        grab(nodes, sel, canvas, scale, p, mods.alt).map(|(i, kind)| {
            if let Some(k) = kind {
                hover_kind = (Some(i) == sel || k == EditKind::Move).then_some(k);
                ui.ctx().set_cursor_icon(cursor_for(k, false));
            }
            i
        })
    } else {
        None
    };
    out.hovered = hovered_idx.map(|i| nodes[i].id.clone());
    if changed {
        response.mark_changed();
    }

    if ui.is_rect_visible(outer) {
        let view = View { t, canvas, scale, opts, alt: mods.alt };
        view.paint(&ui.painter_at(outer), outer, nodes, sel, hovered_idx, out.edit.as_ref(), active, hover_kind, guides);
    }
    out.response = response;
    out
}

// ---------------------------------------------------------------------------------------------
// Painting

struct View<'a> {
    t: &'a Theme,
    canvas: Rect,
    scale: f32,
    opts: &'a CanvasOpts,
    alt: bool,
}

fn dashed_rect(p: &Painter, r: Rect, stroke: Stroke) {
    let pts = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
    p.extend(Shape::dashed_line(&pts, stroke, 5.0, 4.0));
}

/// Text on a dark chip; returns the chip rect.
fn chip(p: &Painter, t: &Theme, pos: Pos2, anchor: Align2, text: String, size: f32, color: Color32) -> Rect {
    let galley = p.layout_no_wrap(text, FontId::proportional(size), color);
    let r = anchor.anchor_size(pos, galley.size());
    p.rect_filled(r.expand(2.0), CornerRadius::same(2), t.bg_darker.gamma_multiply(0.85));
    p.galley(r.min, galley, color);
    r
}

fn readout(kind: EditKind, rect: [f32; 4], crop: [f32; 4], radius: f32, canvas_px: [f32; 2]) -> String {
    match kind {
        EditKind::Move | EditKind::Resize(_) => {
            let [x, y, w, h] = rect_px(rect, canvas_px);
            format!("{x:.0}, {y:.0}  {w:.0}×{h:.0}")
        }
        EditKind::Crop(_) => {
            let [l, t, r, b] = crop.map(|v| v * 100.0);
            format!("crop {l:.1}% {t:.1}% {r:.1}% {b:.1}%")
        }
        EditKind::Radius => format!("radius {radius:.0} px"),
    }
}

impl View<'_> {
    fn radius_screen(&self, radius: f32, sr: Rect) -> f32 {
        (radius * self.scale).clamp(0.0, 0.5 * sr.width().abs().min(sr.height().abs()))
    }

    fn paint(
        &self,
        p: &Painter,
        outer: Rect,
        nodes: &[CanvasNode],
        sel: Option<usize>,
        hovered: Option<usize>,
        edit: Option<&CanvasEdit>,
        active: Option<EditKind>,
        hover_kind: Option<EditKind>,
        guides: (Option<f32>, Option<f32>),
    ) {
        let (t, canvas, opts) = (self.t, self.canvas, self.opts);
        if !opts.transparent {
            p.rect_filled(outer, CornerRadius::same(4), t.bg_darker);
            match opts.background {
                Some((tex, uv)) => p.image(tex, canvas, uv, Color32::WHITE),
                None => p.rect_filled(canvas, CornerRadius::ZERO, t.bg_dark),
            };
        }
        self.grid(p);

        let geom = |i: usize| {
            let n = &nodes[i];
            match edit {
                Some(e) if e.node == n.id => (e.rect, e.crop, e.radius),
                _ => (n.rect, n.crop, n.radius),
            }
        };
        let mut order: Vec<usize> = (0..nodes.len()).collect();
        order.sort_by_key(|&i| (nodes[i].z, i));
        for &i in &order {
            let (rect, crop, radius) = geom(i);
            self.node(p, &nodes[i], rect, crop, radius, Some(i) == sel, Some(i) == hovered);
        }

        // Dim whatever lies outside the canvas.
        let dim = t.bg_darker.gamma_multiply(0.75);
        for r in [
            Rect::from_min_max(outer.min, pos2(outer.max.x, canvas.min.y)),
            Rect::from_min_max(pos2(outer.min.x, canvas.max.y), outer.max),
            Rect::from_min_max(pos2(outer.min.x, canvas.min.y), pos2(canvas.min.x, canvas.max.y)),
            Rect::from_min_max(pos2(canvas.max.x, canvas.min.y), pos2(outer.max.x, canvas.max.y)),
        ] {
            if r.is_positive() {
                p.rect_filled(r, CornerRadius::ZERO, dim);
            }
        }
        p.rect_stroke(canvas, CornerRadius::ZERO, Stroke::new(1.0, t.muted), StrokeKind::Outside);
        self.overlays(p);

        if let Some(i) = sel {
            let (rect, crop, radius) = geom(i);
            self.selection(p, &nodes[i], rect, crop, radius, active, hover_kind);
        }
        let guide = Stroke::new(1.0, t.accent);
        if let Some(x) = guides.0 {
            p.vline(canvas.min.x + x * canvas.width(), outer.y_range(), guide);
        }
        if let Some(y) = guides.1 {
            p.hline(outer.x_range(), canvas.min.y + y * canvas.height(), guide);
        }
        if let (Some(e), Some(kind)) = (edit, active) {
            let sr = to_screen(e.rect, canvas);
            let text = readout(kind, e.rect, e.crop, e.radius, opts.canvas_px);
            chip(p, t, sr.left_bottom() + vec2(0.0, 6.0), Align2::LEFT_TOP, text, SMALL_FONT, t.accent);
        }

        if let Some(s) = opts.tally {
            p.rect_stroke(canvas, CornerRadius::ZERO, Stroke::new(3.0, led_color(t, s)), StrokeKind::Outside);
        }
        if opts.live {
            let red = t.tally_program();
            p.rect_stroke(outer, CornerRadius::same(4), Stroke::new(2.0, red), StrokeKind::Inside);
            let r = chip(p, t, canvas.right_top() + vec2(-6.0, 6.0), Align2::RIGHT_TOP, "LIVE".into(), SMALL_FONT, red);
            p.circle_filled(r.left_center() - vec2(8.0, 0.0), 3.5, red);
        }
    }

    fn grid(&self, p: &Painter) {
        let (t, canvas) = (self.t, self.canvas);
        let Some(step) = self.opts.grid.filter(|s| s.is_finite() && *s > 0.0) else { return };
        let stroke = Stroke::new(1.0, t.muted.gamma_multiply(0.3));
        let lines = (1.0 / step).floor() as usize;
        if step * canvas.width() >= 4.0 {
            for k in 1..=lines {
                let x = canvas.min.x + k as f32 * step * canvas.width();
                if x < canvas.max.x - 0.5 {
                    p.vline(x, canvas.y_range(), stroke);
                }
            }
        }
        if step * canvas.height() >= 4.0 {
            for k in 1..=lines {
                let y = canvas.min.y + k as f32 * step * canvas.height();
                if y < canvas.max.y - 0.5 {
                    p.hline(canvas.x_range(), y, stroke);
                }
            }
        }
        let center = Stroke::new(1.0, t.muted.gamma_multiply(0.6));
        p.vline(canvas.center().x, canvas.y_range(), center);
        p.hline(canvas.x_range(), canvas.center().y, center);
    }

    fn node(&self, p: &Painter, n: &CanvasNode, rect: [f32; 4], crop: [f32; 4], radius: f32, selected: bool, hovered: bool) {
        let t = self.t;
        let sr = to_screen(rect, self.canvas);
        let corner = CornerRadius::from(self.radius_screen(radius, sr));
        let textured = self.opts.background.is_some() || self.opts.transparent;
        if n.visible && !textured {
            p.rect_filled(sr, corner, t.bg_light.gamma_multiply(0.55));
        }
        if selected || hovered {
            p.rect_filled(sr, corner, t.selection.gamma_multiply(if selected { 0.35 } else { 0.2 }));
        }
        let color = if n.modulated {
            t.modulated()
        } else if selected {
            t.accent
        } else if hovered {
            t.fg
        } else {
            t.fg_dim
        };
        let stroke = Stroke::new(
            if selected {
                2.0
            } else if hovered || n.modulated {
                1.5
            } else {
                1.0
            },
            color,
        );
        if n.visible {
            p.rect_stroke(sr, corner, stroke, StrokeKind::Inside);
        } else {
            dashed_rect(p, sr, stroke);
        }
        if !self.opts.show_labels || sr.height() < 14.0 || sr.width() < 16.0 {
            return;
        }
        let cropped = crop.iter().any(|c| *c != 0.0);
        let mut text = String::with_capacity(n.label.len() + 16);
        for (on, glyph) in [(n.locked, LOCK), (!n.visible, HIDDEN), (n.modulated, icon::MOD), (cropped, CROP)] {
            if on {
                text.push_str(glyph);
                text.push(' ');
            }
        }
        text.push_str(&n.label);
        let clip = p.with_clip_rect(sr.intersect(p.clip_rect()));
        chip(&clip, t, sr.left_top() + vec2(5.0, 4.0), Align2::LEFT_TOP, text, LABEL_FONT, if selected { t.accent } else { t.fg });
    }

    fn overlays(&self, p: &Painter) {
        let (t, canvas) = (self.t, self.canvas);
        if self.opts.safe_area {
            for (frac, label, color, anchor) in
                [(ACTION_SAFE, "action safe", t.muted, Align2::LEFT_TOP), (TITLE_SAFE, "title safe", t.fg_dim, Align2::LEFT_BOTTOM)]
            {
                let r = to_screen(safe_rect(frac), canvas);
                dashed_rect(p, r, Stroke::new(1.0, color));
                let at = if anchor == Align2::LEFT_TOP { r.left_top() + vec2(3.0, 2.0) } else { r.left_bottom() + vec2(3.0, -2.0) };
                p.text(at, anchor, label, FontId::proportional(SMALL_FONT), color);
            }
        }
        if self.opts.tiktok_guides {
            let (fill, edge) = (t.cyan.gamma_multiply(0.12), Stroke::new(1.0, t.cyan.gamma_multiply(0.7)));
            for z in TikTokGuides::DEFAULT.zones() {
                let r = to_screen(z.rect, canvas);
                p.rect_filled(r, CornerRadius::ZERO, fill);
                p.rect_stroke(r, CornerRadius::ZERO, edge, StrokeKind::Inside);
                let galley = p.layout_no_wrap(z.label.to_owned(), FontId::proportional(SMALL_FONT), t.cyan);
                if galley.size().x + 6.0 <= r.width() && galley.size().y + 4.0 <= r.height() {
                    p.galley(r.left_top() + vec2(3.0, 2.0), galley, t.cyan);
                }
            }
        }
    }

    fn selection(&self, p: &Painter, n: &CanvasNode, rect: [f32; 4], crop: [f32; 4], radius: f32, active: Option<EditKind>, hover: Option<EditKind>) {
        let t = self.t;
        let sr = to_screen(rect, self.canvas);
        // The cropped-away parts of the source, dimmed around the visible box.
        if crop.iter().any(|c| *c != 0.0) || matches!(active, Some(EditKind::Crop(_))) {
            let src = to_screen(source_rect(rect, crop), self.canvas);
            let dim = t.bg_darker.gamma_multiply(0.5);
            for r in [
                Rect::from_min_max(src.min, pos2(src.max.x, sr.min.y)),
                Rect::from_min_max(pos2(src.min.x, sr.max.y), src.max),
                Rect::from_min_max(pos2(src.min.x, sr.min.y), pos2(sr.min.x, sr.max.y)),
                Rect::from_min_max(pos2(sr.max.x, sr.min.y), pos2(src.max.x, sr.max.y)),
            ] {
                if r.is_positive() {
                    p.rect_filled(r, CornerRadius::ZERO, dim);
                }
            }
            dashed_rect(p, src, Stroke::new(1.0, t.orange.gamma_multiply(0.8)));
        }
        if n.locked {
            return;
        }
        let lit = |k: EditKind| active == Some(k) || (active.is_none() && hover == Some(k));
        for h in Handle::RESIZE {
            let at = handle_pos(sr, h);
            if let (true, Some(e)) = (self.alt, h.edge()) {
                let len = 16.0f32.min(0.3 * if matches!(e, Edge::Left | Edge::Right) { sr.height() } else { sr.width() }).max(4.0);
                let size = if matches!(e, Edge::Left | Edge::Right) { vec2(3.0, len) } else { vec2(len, 3.0) };
                let c = if lit(EditKind::Crop(e)) { t.fg_bright } else { t.orange };
                p.rect_filled(Rect::from_center_size(at, size), CornerRadius::same(1), c);
                continue;
            }
            let r = Rect::from_center_size(at, Vec2::splat(HANDLE));
            let fill = if lit(EditKind::Resize(h)) || h.edge().is_some_and(|e| lit(EditKind::Crop(e))) { t.accent } else { t.bg };
            p.rect_filled(r, CornerRadius::same(1), fill);
            p.rect_stroke(r, CornerRadius::same(1), Stroke::new(1.5, t.accent), StrokeKind::Middle);
        }
        if let Some(at) = radius_handle(sr, self.radius_screen(radius, sr)) {
            let fill = if lit(EditKind::Radius) { t.accent } else { t.bg };
            p.circle_filled(at, 4.0, fill);
            p.circle_stroke(at, 4.0, Stroke::new(1.5, t.accent));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Context, RawInput};

    const WIDE: [f32; 2] = [1920.0, 1080.0];
    const TALL: [f32; 2] = [1080.0, 1920.0];

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    fn close4(a: [f32; 4], b: [f32; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| close(*x, y))
    }

    #[track_caller]
    fn assert_rect(a: [f32; 4], b: [f32; 4]) {
        assert!(close4(a, b), "{a:?} != {b:?}");
    }

    fn node(id: &str, rect: [f32; 4]) -> CanvasNode {
        CanvasNode { id: id.into(), label: id.to_uppercase(), rect, crop: [0.0; 4], radius: 0.0, z: 0, visible: true, locked: false, modulated: false }
    }

    #[test]
    fn fit_and_round_trip_on_both_canvases() {
        let avail = Rect::from_min_size(pos2(10.0, 20.0), vec2(800.0, 600.0));
        for px in [WIDE, TALL] {
            let c = fit_canvas(avail, px);
            assert!(close(c.width() / c.height(), px[0] / px[1]));
            assert!(avail.contains_rect(c.shrink(0.01)));
            assert!(close(c.center().x, avail.center().x) && close(c.center().y, avail.center().y));
            for r in [[0.0, 0.0, 1.0, 1.0], [0.25, 0.1, 0.5, 0.3], [-0.2, 0.9, 0.4, 0.6]] {
                let s = to_screen(r, c);
                assert_rect(to_norm(s, c), r);
            }
            let p = pos2(c.min.x + 0.3 * c.width(), c.min.y + 0.7 * c.height());
            let n = pos_to_norm(p, c);
            assert!(close(n[0], 0.3) && close(n[1], 0.7));
        }
        // Wide in a tall area is width-limited, tall in a wide area height-limited.
        assert!(close(fit_canvas(avail, WIDE).width(), 800.0));
        assert!(close(fit_canvas(avail, TALL).height(), 600.0));
    }

    #[test]
    fn pixel_rects_round_trip() {
        assert_rect(rect_px([0.5, 0.5, 0.25, 0.25], WIDE), [960.0, 540.0, 480.0, 270.0]);
        assert_rect(rect_px([0.5, 0.5, 0.25, 0.25], TALL), [540.0, 960.0, 270.0, 480.0]);
        for px in [WIDE, TALL] {
            let r = [0.123, 0.456, 0.2, 0.05];
            assert_rect(rect_from_px(rect_px(r, px), px), r);
        }
        assert_rect(nudge([0.1, 0.1, 0.2, 0.2], [1.0, -10.0], WIDE), [0.1 + 1.0 / 1920.0, 0.1 - 10.0 / 1080.0, 0.2, 0.2]);
    }

    #[test]
    fn handle_hit_priority() {
        let r = Rect::from_min_max(pos2(100.0, 100.0), pos2(300.0, 200.0));
        // Near the corner: inside the body and within reach of two edges, the corner wins.
        assert_eq!(hit_handle(r, pos2(103.0, 104.0), HIT), Some(Handle::TopLeft));
        assert_eq!(hit_handle(r, pos2(305.0, 195.0), HIT), Some(Handle::BottomRight));
        assert_eq!(hit_handle(r, pos2(297.0, 98.0), HIT), Some(Handle::TopRight));
        assert_eq!(hit_handle(r, pos2(102.0, 203.0), HIT), Some(Handle::BottomLeft));
        // Anywhere along an edge (inside or just outside) beats the body.
        assert_eq!(hit_handle(r, pos2(150.0, 104.0), HIT), Some(Handle::Top));
        assert_eq!(hit_handle(r, pos2(96.0, 150.0), HIT), Some(Handle::Left));
        assert_eq!(hit_handle(r, pos2(299.0, 130.0), HIT), Some(Handle::Right));
        assert_eq!(hit_handle(r, pos2(250.0, 205.0), HIT), Some(Handle::Bottom));
        assert_eq!(hit_handle(r, pos2(200.0, 150.0), HIT), Some(Handle::Move));
        assert_eq!(hit_handle(r, pos2(90.0, 150.0), HIT), None);
        assert_eq!(hit_handle(r, pos2(200.0, 250.0), HIT), None);
        // A node smaller than the tolerance: the nearest corner wins.
        let tiny = Rect::from_min_size(pos2(0.0, 0.0), vec2(8.0, 8.0));
        assert_eq!(hit_handle(tiny, pos2(6.0, 7.0), HIT), Some(Handle::BottomRight));
        assert_eq!(hit_handle(tiny, pos2(1.0, 2.0), HIT), Some(Handle::TopLeft));
    }

    #[test]
    fn radius_handle_tracks_radius_and_hides_on_small_nodes() {
        let r = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 100.0));
        assert_eq!(radius_handle(r, 0.0), Some(pos2(RADIUS_INSET, RADIUS_INSET)));
        assert_eq!(radius_handle(r, 20.0), Some(pos2(RADIUS_INSET + 20.0, RADIUS_INSET)));
        assert_eq!(radius_handle(r, 500.0), Some(pos2(100.0, RADIUS_INSET)));
        assert_eq!(radius_handle(Rect::from_min_size(Pos2::ZERO, vec2(30.0, 30.0)), 0.0), None);
    }

    #[test]
    fn resize_every_handle() {
        let s = [0.2, 0.2, 0.4, 0.4];
        let d = [0.1, 0.05];
        let expect = [
            (Handle::TopLeft, [0.3, 0.25, 0.3, 0.35]),
            (Handle::Top, [0.2, 0.25, 0.4, 0.35]),
            (Handle::TopRight, [0.2, 0.25, 0.5, 0.35]),
            (Handle::Right, [0.2, 0.2, 0.5, 0.4]),
            (Handle::BottomRight, [0.2, 0.2, 0.5, 0.45]),
            (Handle::Bottom, [0.2, 0.2, 0.4, 0.45]),
            (Handle::BottomLeft, [0.3, 0.2, 0.3, 0.45]),
            (Handle::Left, [0.3, 0.2, 0.3, 0.4]),
            (Handle::Move, [0.3, 0.25, 0.4, 0.4]),
        ];
        for (h, want) in expect {
            assert_rect(resize(s, h, d, false), want);
        }
    }

    #[test]
    fn resize_keeps_aspect() {
        let s = [0.2, 0.2, 0.4, 0.2];
        // Corner: the dominant axis drives, the opposite corner stays.
        assert_rect(resize(s, Handle::BottomRight, [0.2, 0.0], true), [0.2, 0.2, 0.6, 0.3]);
        assert_rect(resize(s, Handle::TopLeft, [0.0, -0.1], true), [0.0, 0.1, 0.6, 0.3]);
        // Edge: the other axis scales about its center.
        assert_rect(resize(s, Handle::Right, [0.2, 0.9], true), [0.2, 0.15, 0.6, 0.3]);
        assert_rect(resize(s, Handle::Top, [0.5, -0.2], true), [0.0, 0.0, 0.8, 0.4]);
        for h in Handle::RESIZE {
            for d in [[0.13, -0.07], [-0.3, 0.4], [5.0, 5.0], [-5.0, -5.0]] {
                let r = resize(s, h, d, true);
                assert!(close(r[2] / r[3], 2.0), "{h:?} {d:?} -> {r:?}");
                assert!(r[2] >= MIN_SIZE - 1e-6 && r[3] >= MIN_SIZE - 1e-6);
            }
        }
    }

    #[test]
    fn resize_never_inverts() {
        let s = [0.2, 0.2, 0.4, 0.2];
        for h in Handle::RESIZE {
            for d in [[5.0, 5.0], [-5.0, -5.0], [5.0, -5.0], [-5.0, 5.0]] {
                let r = resize(s, h, d, false);
                assert!(r[2] >= MIN_SIZE - 1e-6 && r[3] >= MIN_SIZE - 1e-6, "{h:?} {d:?} -> {r:?}");
            }
        }
        // The fixed side stays where it was.
        assert_rect(resize(s, Handle::Left, [1.0, 0.0], false), [0.6 - MIN_SIZE, 0.2, MIN_SIZE, 0.2]);
        assert_rect(resize(s, Handle::Right, [-1.0, 0.0], false), [0.2, 0.2, MIN_SIZE, 0.2]);
        // Aspect lock stops at the min size on the shorter side.
        assert_rect(resize(s, Handle::BottomRight, [-1.0, -1.0], true), [0.2, 0.2, 2.0 * MIN_SIZE, MIN_SIZE]);
        // Nothing clamps positions: nodes may leave the canvas.
        assert_rect(resize(s, Handle::Right, [2.0, 0.0], false), [0.2, 0.2, 2.4, 0.2]);
    }

    #[test]
    fn crop_edges_keep_source_scale() {
        let rect = [0.0, 0.0, 0.5, 0.5];
        let (r, c) = crop_edge(rect, [0.0; 4], Edge::Left, [0.1, 0.3]);
        assert_rect(c, [0.2, 0.0, 0.0, 0.0]);
        assert_rect(r, [0.1, 0.0, 0.4, 0.5]);
        let (r, c) = crop_edge(rect, [0.0; 4], Edge::Right, [-0.1, 0.0]);
        assert_rect(c, [0.0, 0.0, 0.2, 0.0]);
        assert_rect(r, [0.0, 0.0, 0.4, 0.5]);
        let (r, c) = crop_edge(rect, [0.0; 4], Edge::Top, [0.0, 0.05]);
        assert_rect(c, [0.0, 0.1, 0.0, 0.0]);
        assert_rect(r, [0.0, 0.05, 0.5, 0.45]);
        let (r, c) = crop_edge(rect, [0.0; 4], Edge::Bottom, [0.0, -0.25]);
        assert_rect(c, [0.0, 0.0, 0.0, 0.5]);
        assert_rect(r, [0.0, 0.0, 0.5, 0.25]);
        // The uncropped source is where it was.
        let before = source_rect(rect, [0.0; 4]);
        for e in [Edge::Left, Edge::Top, Edge::Right, Edge::Bottom] {
            let (r, c) = crop_edge(rect, [0.0; 4], e, [0.07, 0.07]);
            assert_rect(source_rect(r, c), before);
        }
    }

    #[test]
    fn crop_clamps_to_zero_and_opposite_side() {
        // Already cropped 10% left: dragging outward stops at the source edge.
        let rect = [0.2, 0.0, 0.45, 0.5];
        let (r, c) = crop_edge(rect, [0.1, 0.0, 0.0, 0.0], Edge::Left, [-1.0, 0.0]);
        assert!(close(c[0], 0.0));
        assert_rect(r, [0.15, 0.0, 0.5, 0.5]);
        // Dragging inward stops before the opposite inset, keeping the min visible size.
        let rect = [0.0, 0.0, 0.35, 0.5];
        let (r, c) = crop_edge(rect, [0.0, 0.0, 0.3, 0.0], Edge::Left, [5.0, 0.0]);
        let full_w = 0.5;
        assert!(c[0] <= 1.0 - 0.3);
        assert!(close(c[0], 1.0 - 0.3 - MIN_SIZE / full_w));
        assert!(close(r[2], MIN_SIZE));
        let (_, c) = crop_edge(rect, [0.0, 0.0, 0.3, 0.0], Edge::Right, [-5.0, 0.0]);
        assert!(close(c[2], 1.0 - MIN_SIZE / full_w));
        let (_, c) = crop_edge(rect, [0.0, 0.0, 0.3, 0.0], Edge::Right, [5.0, 0.0]);
        assert!(close(c[2], 0.0));
    }

    #[test]
    fn radius_is_clamped() {
        let rect = [0.0, 0.0, 0.25, 0.5];
        assert!(close(drag_radius(10.0, 5.0, rect, WIDE), 15.0));
        assert!(close(drag_radius(10.0, -50.0, rect, WIDE), 0.0));
        // Half the shorter side: min(480, 540) / 2.
        assert!(close(drag_radius(10.0, 1e6, rect, WIDE), 240.0));
    }

    #[test]
    fn snap_move_to_canvas_edges_and_center() {
        let none = SnapTargets::default();
        let thr = [0.01, 0.01];
        let s = snap_move([0.004, 0.33, 0.2, 0.2], &none, thr);
        assert_rect(s.rect, [0.0, 0.33, 0.2, 0.2]);
        assert_eq!((s.guide_x, s.guide_y), (Some(0.0), None));
        let s = snap_move([0.4, 0.795, 0.2, 0.2], &none, thr);
        assert_rect(s.rect, [0.4, 0.8, 0.2, 0.2]);
        assert_eq!((s.guide_x, s.guide_y), (Some(0.5), Some(1.0)));
        // Outside the threshold nothing moves.
        let s = snap_move([0.02, 0.33, 0.2, 0.2], &none, thr);
        assert_rect(s.rect, [0.02, 0.33, 0.2, 0.2]);
        assert_eq!((s.guide_x, s.guide_y), (None, None));
        // A zero threshold (snapping off) never snaps.
        assert_eq!(snap_move([0.001, 0.3, 0.2, 0.2], &none, [0.0; 2]).guide_x, None);
    }

    #[test]
    fn snap_move_to_other_nodes_and_grid() {
        let others = [[0.3, 0.3, 0.2, 0.2]];
        let t = SnapTargets { others: &others, grid: None };
        let thr = [0.01, 0.01];
        // Right edge onto the other's left edge.
        let s = snap_move([0.093, 0.62, 0.2, 0.1], &t, thr);
        assert_rect(s.rect, [0.1, 0.62, 0.2, 0.1]);
        assert_eq!(s.guide_x, Some(0.3));
        // Center onto the other's center.
        let s = snap_move([0.702, 0.3485, 0.1, 0.1], &t, thr);
        assert_rect(s.rect, [0.702, 0.35, 0.1, 0.1]);
        assert_eq!(s.guide_y, Some(0.4));
        // Grid: only when a grid is set; the closest target wins over a farther node edge.
        let g = SnapTargets { others: &others, grid: Some(0.1) };
        let s = snap_move([0.205, 0.62, 0.03, 0.03], &g, thr);
        assert_rect(s.rect, [0.2, 0.62, 0.03, 0.03]);
        assert_eq!(s.guide_x, Some(0.2));
        let s = snap_move([0.205, 0.62, 0.03, 0.03], &t, thr);
        assert_eq!(s.guide_x, None);
        let s = snap_move([0.2915, 0.62, 0.03, 0.03], &g, thr);
        assert_eq!(s.guide_x, Some(0.3), "node edge at 0.3 beats grid 0.3 tie and far grid");
    }

    #[test]
    fn snap_threshold_converts_screen_px() {
        let canvas = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 450.0));
        let thr = snap_threshold(8.0, canvas);
        assert!(close(thr[0], 0.01) && close(thr[1], 8.0 / 450.0));
        // The same 5 px miss snaps at this zoom but not at 4× zoom.
        let none = SnapTargets::default();
        let r = [5.0 / 800.0, 0.3, 0.2, 0.2];
        assert_eq!(snap_move(r, &none, thr).guide_x, Some(0.0));
        let zoomed = snap_threshold(8.0, Rect::from_min_size(Pos2::ZERO, vec2(3200.0, 1800.0)));
        assert_eq!(snap_move(r, &none, zoomed).guide_x, None);
    }

    #[test]
    fn snap_resize_moves_only_dragged_edges() {
        let others = [[0.5, 0.0, 0.2, 0.2]];
        let t = SnapTargets { others: &others, grid: None };
        let thr = [0.01, 0.01];
        let s = snap_resize([0.1, 0.3, 0.3, 0.2], Handle::Right, [0.095, 0.0], false, &t, thr);
        assert_rect(s.rect, [0.1, 0.3, 0.4, 0.2]);
        assert_eq!(s.guide_x, Some(0.5));
        // A snap that would go below the min size is skipped.
        let s = snap_resize([0.1, 0.3, 0.3, 0.2], Handle::Left, [0.395, 0.0], false, &t, thr);
        assert_eq!(s.guide_x, None);
        assert!(close(s.rect[2], MIN_SIZE));
        // Aspect lock: the snapped edge drives the scale, the other axis follows.
        let s = snap_resize([0.5, 0.5, 0.2, 0.1], Handle::BottomRight, [0.296, 0.0], true, &SnapTargets::default(), thr);
        assert_rect(s.rect, [0.5, 0.5, 0.5, 0.25]);
        assert_eq!(s.guide_x, Some(1.0));
    }

    #[test]
    fn align_single_to_canvas_and_many_to_bounds() {
        let one = [[0.1, 0.2, 0.3, 0.4]];
        assert_rect(align(&one, Align::Right)[0], [0.7, 0.2, 0.3, 0.4]);
        assert_rect(align(&one, Align::VCenter)[0], [0.1, 0.3, 0.3, 0.4]);
        let many = [[0.1, 0.2, 0.2, 0.2], [0.5, 0.1, 0.1, 0.5]];
        let l = align(&many, Align::Left);
        assert!(close(l[0][0], 0.1) && close(l[1][0], 0.1));
        let c = align(&many, Align::HCenter);
        assert!(close(c[0][0] + 0.1, 0.35) && close(c[1][0] + 0.05, 0.35));
        let b = align(&many, Align::Bottom);
        assert!(close(b[0][1] + 0.2, 0.6) && close(b[1][1] + 0.5, 0.6));
        let top = align(&many, Align::Top);
        assert!(close(top[0][1], 0.1) && close(top[1][1], 0.1));
        assert!(align(&[], Align::Left).is_empty());
    }

    #[test]
    fn distribute_equal_gaps() {
        let rects = [[0.8, 0.0, 0.1, 0.1], [0.0, 0.0, 0.1, 0.1], [0.15, 0.0, 0.2, 0.1], [0.2, 0.0, 0.1, 0.1]];
        let out = distribute(&rects, Axis::X);
        // Order by x: 1, 2, 3, 0. Span 0..0.9, sizes 0.5 → gaps 0.4 / 3.
        let gap = 0.4 / 3.0;
        assert!(close(out[1][0], 0.0));
        assert!(close(out[2][0], 0.1 + gap));
        assert!(close(out[3][0], 0.3 + 2.0 * gap));
        assert!(close(out[0][0], 0.8));
        assert!(out.iter().all(|r| close(r[1], 0.0)));
        let two = [[0.3, 0.1, 0.1, 0.1], [0.0, 0.5, 0.1, 0.1]];
        assert_eq!(distribute(&two, Axis::Y), two.to_vec());
    }

    #[test]
    fn fit_aspect_contains_in_rect() {
        let r = fit_aspect(fill_canvas(), 16.0 / 9.0, TALL);
        assert_rect(r, [0.0, (1.0 - 607.5 / 1920.0) / 2.0, 1.0, 607.5 / 1920.0]);
        let px = rect_px(r, TALL);
        assert!(close(px[2] / px[3], 16.0 / 9.0));
        let r = fit_aspect(fill_canvas(), 9.0 / 16.0, WIDE);
        assert_rect(r, [(1.0 - 607.5 / 1920.0) / 2.0, 0.0, 607.5 / 1920.0, 1.0]);
        assert_rect(fit_aspect([0.1, 0.1, 0.2, 0.2], f32::NAN, WIDE), [0.1, 0.1, 0.2, 0.2]);
        assert_rect(fit_aspect([0.1, 0.1, 0.0, 0.2], 1.0, WIDE), [0.1, 0.1, 0.0, 0.2]);
    }

    #[test]
    fn pick_prefers_top_z_then_visible() {
        let mut nodes = vec![node("a", [0.0, 0.0, 0.5, 0.5]), node("b", [0.2, 0.2, 0.5, 0.5]), node("c", [0.2, 0.2, 0.1, 0.1])];
        assert_eq!(pick_node(&nodes, [0.25, 0.25]), Some(2));
        nodes[0].z = 5;
        assert_eq!(pick_node(&nodes, [0.25, 0.25]), Some(0));
        nodes[0].visible = false;
        assert_eq!(pick_node(&nodes, [0.25, 0.25]), Some(2));
        assert_eq!(pick_node(&nodes, [0.1, 0.1]), Some(0), "hidden nodes are still pickable alone");
        assert_eq!(pick_node(&nodes, [0.9, 0.9]), None);
    }

    #[test]
    fn tiktok_zones_are_on_canvas() {
        for z in TikTokGuides::default().zones() {
            let [x, y, w, h] = z.rect;
            assert!(x >= 0.0 && y >= 0.0 && x + w <= 1.0 + 1e-6 && y + h <= 1.0 + 1e-6, "{}", z.label);
        }
        assert_rect(safe_rect(TITLE_SAFE), [0.05, 0.05, 0.9, 0.9]);
    }

    // -- Headless interaction ------------------------------------------------------------------

    struct Rig {
        ctx: Context,
        time: f64,
        theme: Theme,
        nodes: Vec<CanvasNode>,
        selected: Option<String>,
        opts: CanvasOpts,
        canvas: Rect,
    }

    impl Rig {
        fn new(nodes: Vec<CanvasNode>) -> Rig {
            let mut opts = CanvasOpts::new(WIDE);
            opts.snap = false;
            let mut rig = Rig { ctx: Context::default(), time: 0.0, theme: Theme::default(), nodes, selected: None, opts, canvas: Rect::NOTHING };
            let r = rig.frame(vec![], Modifiers::NONE);
            rig.canvas = canvas_rect(r.response.rect, WIDE);
            rig
        }

        fn frame(&mut self, mut events: Vec<Event>, modifiers: Modifiers) -> CanvasResponse {
            self.time += 1.0 / 60.0;
            events.insert(0, Event::ModifiersChanged(modifiers));
            let input =
                RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(820.0, 480.0))), time: Some(self.time), events, ..Default::default() };
            let mut out = None;
            let full = self.ctx.run_ui(input, |ui| {
                out = Some(canvas_editor(ui, &self.theme, Id::new("canvas-test"), vec2(800.0, 450.0), &self.nodes, self.selected.as_deref(), &self.opts));
            });
            full.drop_without_applying_deltas();
            let r = out.expect("widget ran");
            assert_eq!(r.response.rect.size(), vec2(800.0, 450.0));
            self.selected = r.selected.clone();
            if let Some(e) = r.edit.as_ref().filter(|e| e.finished)
                && let Some(n) = self.nodes.iter_mut().find(|n| n.id == e.node)
            {
                (n.rect, n.crop, n.radius) = (e.rect, e.crop, e.radius);
            }
            r
        }

        fn screen(&self, id: &str) -> Rect {
            to_screen(self.nodes.iter().find(|n| n.id == id).unwrap().rect, self.canvas)
        }

        /// Press at `from`, move halfway, move to `to` (returns that in-drag frame), release.
        fn drag(&mut self, from: Pos2, to: Pos2, m: Modifiers) -> (CanvasResponse, CanvasResponse) {
            let press = self.frame(vec![Event::PointerMoved(from), button(from, true, m)], m);
            assert!(press.edit.is_none(), "a press alone is not an edit yet");
            self.frame(vec![Event::PointerMoved(from + (to - from) * 0.5)], m);
            let during = self.frame(vec![Event::PointerMoved(to)], m);
            let done = self.frame(vec![button(to, false, m)], m);
            (during, done)
        }

        fn click(&mut self, at: Pos2) -> CanvasResponse {
            self.frame(vec![Event::PointerMoved(at), button(at, true, Modifiers::NONE)], Modifiers::NONE);
            self.frame(vec![button(at, false, Modifiers::NONE)], Modifiers::NONE)
        }
    }

    fn button(pos: Pos2, pressed: bool, modifiers: Modifiers) -> Event {
        Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers }
    }

    fn key(key: Key, modifiers: Modifiers) -> Event {
        Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    #[test]
    fn drag_body_previews_then_commits_move() {
        let mut rig = Rig::new(vec![node("a", [0.1, 0.1, 0.3, 0.3]), node("b", [0.6, 0.6, 0.2, 0.2])]);
        let (cw, ch) = (rig.canvas.width(), rig.canvas.height());
        let from = rig.screen("a").center();
        let (during, done) = rig.drag(from, from + vec2(40.0, 20.0), Modifiers::NONE);
        let e = during.edit.expect("in-progress edit");
        assert!(!e.finished);
        assert_eq!((e.node.as_str(), e.kind), ("a", EditKind::Move));
        assert_rect(e.rect, [0.1 + 40.0 / cw, 0.1 + 20.0 / ch, 0.3, 0.3]);
        assert_eq!(during.selected.as_deref(), Some("a"));
        assert_eq!(during.hovered.as_deref(), Some("a"));
        assert!(during.response.changed());
        let e = done.edit.expect("finished edit");
        assert!(e.finished);
        assert_rect(e.rect, [0.1 + 40.0 / cw, 0.1 + 20.0 / ch, 0.3, 0.3]);
        // Nothing is emitted after the release.
        assert!(rig.frame(vec![], Modifiers::NONE).edit.is_none());
    }

    #[test]
    fn drag_handles_resize_crop_and_radius() {
        let mut rig = Rig::new(vec![node("a", [0.1, 0.1, 0.3, 0.3])]);
        rig.selected = Some("a".into());
        let (cw, ch) = (rig.canvas.width(), rig.canvas.height());

        let corner = rig.screen("a").right_bottom();
        let (_, done) = rig.drag(corner, corner + vec2(40.0, 20.0), Modifiers::NONE);
        let e = done.edit.unwrap();
        assert_eq!(e.kind, EditKind::Resize(Handle::BottomRight));
        assert_rect(e.rect, [0.1, 0.1, 0.3 + 40.0 / cw, 0.3 + 20.0 / ch]);

        let before = rig.nodes[0].rect;
        let corner = rig.screen("a").right_bottom();
        let (_, done) = rig.drag(corner, corner + vec2(60.0, 0.0), Modifiers::SHIFT);
        let r = done.edit.unwrap().rect;
        assert!(close(r[2] / r[3], before[2] / before[3]), "shift keeps aspect");
        assert!(close(r[2], before[2] + 60.0 / cw));

        rig.nodes[0].rect = [0.1, 0.1, 0.3, 0.3];
        let left = rig.screen("a").left_center();
        let (_, done) = rig.drag(left, left + vec2(30.0, 0.0), Modifiers::ALT);
        let e = done.edit.unwrap();
        assert_eq!(e.kind, EditKind::Crop(Edge::Left));
        assert!(close(e.crop[0], 30.0 / cw / 0.3));
        assert_rect(e.rect, [0.1 + 30.0 / cw, 0.1, 0.3 - 30.0 / cw, 0.3]);

        let grip = radius_handle(rig.screen("a"), 0.0).unwrap();
        let (_, done) = rig.drag(grip, grip + vec2(20.0, 3.0), Modifiers::NONE);
        let e = done.edit.unwrap();
        assert_eq!(e.kind, EditKind::Radius);
        assert!(close(e.radius, 20.0 * 1920.0 / cw));
    }

    #[test]
    fn drag_snaps_unless_ctrl() {
        let mut rig = Rig::new(vec![node("a", [0.1, 0.1, 0.3, 0.3])]);
        rig.opts.snap = true;
        let cw = rig.canvas.width();
        let from = rig.screen("a").center();
        let to = from - vec2(0.1 * cw - 3.0, 0.0);
        let (during, done) = rig.drag(from, to, Modifiers::NONE);
        assert!(close(during.edit.unwrap().rect[0], 0.0));
        assert!(close(done.edit.unwrap().rect[0], 0.0));

        rig.nodes[0].rect = [0.1, 0.1, 0.3, 0.3];
        let (_, done) = rig.drag(from, to, Modifiers::CTRL);
        assert!(close(done.edit.unwrap().rect[0], 3.0 / cw));
    }

    #[test]
    fn escape_cancels_to_start_geometry() {
        let mut rig = Rig::new(vec![node("a", [0.1, 0.1, 0.3, 0.3])]);
        let from = rig.screen("a").center();
        rig.frame(vec![Event::PointerMoved(from), button(from, true, Modifiers::NONE)], Modifiers::NONE);
        let during = rig.frame(vec![Event::PointerMoved(from + vec2(50.0, 0.0))], Modifiers::NONE);
        assert!(!during.edit.unwrap().finished);
        let esc = rig.frame(vec![key(Key::Escape, Modifiers::NONE)], Modifiers::NONE);
        let e = esc.edit.expect("cancel finishes the drag");
        assert!(e.finished);
        assert_rect(e.rect, [0.1, 0.1, 0.3, 0.3]);
        assert!(rig.frame(vec![button(from + vec2(50.0, 0.0), false, Modifiers::NONE)], Modifiers::NONE).edit.is_none());
    }

    #[test]
    fn click_selects_topmost_and_empty_deselects() {
        let mut nodes = vec![node("a", [0.1, 0.1, 0.5, 0.5]), node("b", [0.3, 0.3, 0.2, 0.2])];
        nodes[1].z = 1;
        let mut rig = Rig::new(nodes);
        let r = rig.click(rig.screen("b").center());
        assert_eq!(r.selected.as_deref(), Some("b"));
        assert!(r.edit.is_none());
        let r = rig.click(rig.screen("a").left_top() + vec2(10.0, 10.0));
        assert_eq!(r.selected.as_deref(), Some("a"));
        let empty = rig.canvas.right_bottom() - vec2(10.0, 10.0);
        let r = rig.click(empty);
        assert_eq!(r.selected, None);
    }

    #[test]
    fn locked_nodes_select_but_do_not_move() {
        let mut nodes = vec![node("a", [0.1, 0.1, 0.3, 0.3])];
        nodes[0].locked = true;
        let mut rig = Rig::new(nodes);
        let from = rig.screen("a").center();
        let (during, done) = rig.drag(from, from + vec2(40.0, 0.0), Modifiers::NONE);
        assert!(during.edit.is_none() && done.edit.is_none());
        assert_eq!(done.selected.as_deref(), Some("a"));
        rig.frame(vec![], Modifiers::NONE);
        assert!(rig.frame(vec![key(Key::ArrowRight, Modifiers::NONE)], Modifiers::NONE).edit.is_none());
    }

    #[test]
    fn arrow_keys_nudge_focused_selection() {
        let mut rig = Rig::new(vec![node("a", [0.1, 0.1, 0.3, 0.3])]);
        // Selected but never focused: arrows are left to the rest of the UI.
        rig.selected = Some("a".into());
        assert!(rig.frame(vec![key(Key::ArrowRight, Modifiers::NONE)], Modifiers::NONE).edit.is_none());
        rig.click(rig.screen("a").center());
        rig.frame(vec![], Modifiers::NONE);
        let r = rig.frame(vec![key(Key::ArrowRight, Modifiers::NONE)], Modifiers::NONE);
        let e = r.edit.expect("nudge edit");
        assert!(e.finished);
        assert_eq!(e.kind, EditKind::Move);
        assert_rect(e.rect, [0.1 + 1.0 / 1920.0, 0.1, 0.3, 0.3]);
        let r = rig.frame(vec![key(Key::ArrowUp, Modifiers::SHIFT), key(Key::ArrowUp, Modifiers::SHIFT)], Modifiers::SHIFT);
        assert_rect(r.edit.unwrap().rect, [0.1 + 1.0 / 1920.0, 0.1 - 20.0 / 1080.0, 0.3, 0.3]);
    }
}
