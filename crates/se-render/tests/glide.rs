//! `kind = "glide"` transitions: morph geometry, but each side keeps its own scene's identity
//! and stacking, and the sides crossfade without a dip — so the picture changes continuously
//! from the first to the last frame (no pop at the start, halfway, or the end).

mod common;

use common::*;
use se_proto::Value;
use se_render::plan::WIDE;

#[global_allocator]
static ALLOC: se_alloc::Counting = se_alloc::Counting;

type Img = (u32, u32, Vec<u8>);

/// Canvas 320 × 180 (`PROJECT`); progress steps of one 60 fps frame of a 1 s transition.
const W: f32 = 320.0;
const H: f32 = 180.0;
const STEPS: usize = 60;

fn project() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    write_file(root, "project.toml", PROJECT);
    // (a) one node in both scenes; only the incoming one has a node effect
    write_file(root, "scenes/plain.toml", "[canvas.wide]\nnodes = [{ src = \"color:#d04020\", rect = [0.1, 0.1, 0.4, 0.4] }]\n");
    write_file(
        root,
        "scenes/framed.toml",
        "[canvas.wide]\nnodes = [{ src = \"color:#d04020\", rect = [0.4, 0.3, 0.5, 0.6], fx = [{ name = \"grade\", saturation = 0.0 }] }]\n",
    );
    // (b) a full-screen camera with a small window of another on top (explicit z) → the
    // window's camera grows to full screen (default z, below the leaving camera's z) and the
    // first one leaves
    write_file(
        root,
        "scenes/two_cams.toml",
        "[canvas.wide]\nnodes = [{ src = \"color:#c03030\", z = 5 }, { src = \"color:#3070c0\", rect = [0.6, 0.55, 0.35, 0.35], radius = 12, z = 10 }]\n",
    );
    write_file(root, "scenes/cam2.toml", "[canvas.wide]\nnodes = [{ src = \"color:#3070c0\" }]\n");
    // (c) an incoming scene with its own background and an entering node
    write_file(
        root,
        "scenes/on_bg.toml",
        "[canvas.wide]\nbackground = \"#284060\"\nnodes = [{ src = \"color:#3070c0\", rect = [0.1, 0.1, 0.5, 0.5] }, { src = \"color:#20c040\", rect = [0.65, 0.2, 0.3, 0.3], enter = \"scale\" }]\n",
    );
    write_file(root, "transitions/glide.toml", "kind = \"glide\"\nms = 1000\nease = \"linear\"\nenter_window = [0, 1]\nexit_window = [0, 1]\nfade_window = [0, 1]\n");
    d
}

fn at_progress(h: &mut Harness, from: &str, to: &str, p: f64) {
    h.set("show.scene.program", to);
    h.set("show.transition.active", true);
    h.set("show.transition.name", "glide");
    h.set("show.transition.from", from);
    h.set("show.transition.ms", Value::Int(1000));
    // frame time is T0 + 1 s
    h.set("show.transition.start", Value::Int((T0 + 1_000_000_000) as i64 - (p * 1e9).round() as i64));
}

fn still(h: &mut Harness, scene: &str) -> Img {
    h.set("show.scene.program", scene);
    h.set("show.transition.active", false);
    h.frame();
    h.read(WIDE)
}

/// Frames at progress 0, 1/60, …, 59/60 and (last) 0.9999.
fn sweep(h: &mut Harness, from: &str, to: &str) -> Vec<(f32, Img)> {
    let mut out = Vec::new();
    for i in 0..=STEPS {
        let p = if i == STEPS { 0.9999 } else { i as f64 / STEPS as f64 };
        at_progress(h, from, to, p);
        h.frame();
        out.push((p as f32, h.read(WIDE)));
    }
    out
}

fn luma(img: &Img) -> f32 {
    let s: f64 = img.2.chunks(4).map(|p| 0.2126 * p[0] as f64 + 0.7152 * p[1] as f64 + 0.0722 * p[2] as f64).sum();
    (s / (img.0 * img.1) as f64) as f32
}

fn diff(a: [u8; 4], b: [u8; 4]) -> u8 {
    a.iter().zip(&b).take(3).map(|(x, y)| x.abs_diff(*y)).max().unwrap()
}

fn lerp5(a: [f32; 5], b: [f32; 5], t: f32) -> [f32; 5] {
    std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
}

/// Whether pixel (x, y) is near a moving edge between two frames: inside one rect but not the
/// other, or within 2.5 px of either's border (edge motion is not a pop). Rects are
/// `x, y, w, h, corner radius` in pixels.
fn near_edge(x: u32, y: u32, r0: [f32; 5], r1: [f32; 5]) -> bool {
    let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
    // signed distance to the rounded rect (negative inside)
    let sd = |r: [f32; 5]| {
        let (hw, hh) = (r[2] * 0.5, r[3] * 0.5);
        let rad = r[4].min(hw).min(hh);
        let qx = (px - r[0] - hw).abs() - (hw - rad);
        let qy = (py - r[1] - hh).abs() - (hh - rad);
        (qx.max(0.0).hypot(qy.max(0.0))) + qx.max(qy).min(0.0) - rad
    };
    let (d0, d1) = (sd(r0), sd(r1));
    (d0 < 0.0) != (d1 < 0.0) || d0.abs() < 2.5 || d1.abs() < 2.5
}

/// Largest per-pixel change (max channel) between consecutive frames, over every 2nd pixel not
/// near the moving rect; returns (step, progress, x, y).
fn max_step(frames: &[(f32, Img)], rect: impl Fn(f32) -> [f32; 5]) -> (u8, f32, u32, u32) {
    let mut worst = (0, 0.0, 0, 0);
    for w in frames.windows(2) {
        let ((t0, a), (t1, b)) = (&w[0], &w[1]);
        let (r0, r1) = (rect(*t0), rect(*t1));
        for y in (0..a.1).step_by(2) {
            for x in (0..a.0).step_by(2) {
                if near_edge(x, y, r0, r1) {
                    continue;
                }
                let d = diff(px(a, x, y), px(b, x, y));
                if d > worst.0 {
                    worst = (d, *t1, x, y);
                }
            }
        }
    }
    worst
}

fn close(a: [u8; 4], b: [u8; 4], tol: u8) -> bool {
    diff(a, b) <= tol
}

/// Channels differing by more than 4 must stay under 0.5 % (edge antialiasing).
fn assert_matches(name: &str, got: &Img, want: &Img) {
    let bad = got.2.iter().zip(&want.2).filter(|(a, b)| a.abs_diff(**b) > 4).count();
    assert!(bad as f64 <= got.2.len() as f64 * 0.005, "{name}: {bad} of {} channels differ by > 4", got.2.len());
}

/// (a) The incoming node's effect fades in with the crossfade instead of popping in on the
/// first frame: pixels inside the node all the way through change continuously from A's look to
/// B's.
#[test]
fn matched_node_effect_changes_continuously() {
    let dir = project();
    let mut h = Harness::new(dir.path());
    let a = still(&mut h, "plain");
    let b = still(&mut h, "framed");
    let frames = sweep(&mut h, "plain", "framed");
    // inside the moving node at every t: x 0.4–0.5, y 0.3–0.5 of the canvas
    let samples: Vec<(u32, u32)> = (132..=156).step_by(6).flat_map(|x| (58..=86).step_by(7).map(move |y| (x, y))).collect();
    assert!(!close(px(&a, 144, 72), px(&b, 144, 72), 20), "the effect must change the look");
    for &(x, y) in &samples {
        assert!(close(px(&frames[0].1, x, y), px(&a, x, y), 3), "t=0 at ({x},{y}): {:?} vs A {:?}", px(&frames[0].1, x, y), px(&a, x, y));
        let end = &frames[STEPS].1;
        assert!(close(px(end, x, y), px(&b, x, y), 3), "t=1 at ({x},{y}): {:?} vs B {:?}", px(end, x, y), px(&b, x, y));
        for w in frames.windows(2) {
            let d = diff(px(&w[0].1, x, y), px(&w[1].1, x, y));
            assert!(d <= 8, "({x},{y}) jumps by {d} at t={}", w[1].0);
        }
    }
}

/// (b) A camera growing from a window to full screen stays on top of the leaving full-screen
/// camera all the way (no reorder at halfway), and the frame never darkens below either scene.
#[test]
fn growing_window_keeps_stacking_without_pop_or_dip() {
    let dir = project();
    write_file(dir.path(), "transitions/glide.toml", "kind = \"glide\"\nms = 1000\n");
    let mut h = Harness::new(dir.path());
    let a = still(&mut h, "two_cams");
    let b = still(&mut h, "cam2");
    let frames = sweep(&mut h, "two_cams", "cam2");
    let rect = |u: f32| lerp5([0.6 * W, 0.55 * H, 0.35 * W, 0.35 * H, 12.0], [0.0, 0.0, W, H, 0.0], se_proto::Ease::Standard.apply(u as f64) as f32);
    let (step, t, x, y) = max_step(&frames, rect);
    assert!(step <= 24, "largest frame-to-frame change {step} at t={t} ({x},{y})");
    // the window's centre shows camera 2 through halfway (morph drops it under camera 1 there)
    let blue = px(&b, 160, 90);
    for (t, f) in &frames {
        let r = rect(*t);
        let (cx, cy) = ((r[0] + r[2] * 0.5) as u32, (r[1] + r[3] * 0.5) as u32);
        assert!(close(px(f, cx, cy), blue, 3), "t={t}: window centre {:?}, camera 2 {blue:?}", px(f, cx, cy));
    }
    let floor = luma(&a).min(luma(&b)) - 1.0;
    let (t_min, l_min) = frames.iter().map(|(t, f)| (*t, luma(f))).fold((0.0, f32::MAX), |m, (t, l)| if l < m.1 { (t, l) } else { m });
    eprintln!("mean luma A {} B {}; lowest during glide {l_min} at t={t_min}", luma(&a), luma(&b));
    assert!(l_min >= floor, "t={t_min}: mean luma {l_min} below min(A {}, B {})", luma(&a), luma(&b));
    assert_matches("end frame", &frames[STEPS].1, &b);
}

/// (c) The last frame is the incoming scene as it renders on its own, background included.
#[test]
fn end_frame_is_the_incoming_scene() {
    let dir = project();
    let mut h = Harness::new(dir.path());
    let b = still(&mut h, "on_bg");
    let frames = sweep(&mut h, "two_cams", "on_bg");
    assert_matches("end frame", &frames[STEPS].1, &b);
    // where B ends on its background, A stays (no dip) and eases into that background over the
    // last quarter, one small step per frame
    let (x, y) = (20, 160);
    assert!(close(px(&frames[0].1, x, y), [0xc0, 0x30, 0x30, 255], 3), "t=0 shows A: {:?}", px(&frames[0].1, x, y));
    assert!(close(px(&frames[STEPS * 3 / 4].1, x, y), [0xc0, 0x30, 0x30, 255], 3), "A stays until 3/4");
    assert!(close(px(&frames[STEPS].1, x, y), [0x28, 0x40, 0x60, 255], 3), "t=1 shows B's background: {:?}", px(&frames[STEPS].1, x, y));
    for w in frames.windows(2) {
        let d = diff(px(&w[0].1, x, y), px(&w[1].1, x, y));
        assert!(d <= 24, "({x},{y}) jumps by {d} at t={}", w[1].0);
    }
}

/// A translucent incoming node must not retain any outgoing contribution at the endpoint;
/// the alpha fallback cleanup must finish with linear time, even if fade finishes earlier.
#[test]
fn translucent_incoming_has_no_early_flash_or_endpoint_residue() {
    let dir = project();
    write_file(dir.path(), "transitions/glide.toml", "kind = \"glide\"\nms = 1000\n");
    write_file(dir.path(), "scenes/translucent.toml", "[canvas.wide]\nbackground = \"#284060\"\nnodes = [{ src = \"color:#20c040\", rect = [0.65, 0.2, 0.3, 0.3], opacity = 0.5, enter = \"fade\" }]\n");
    let mut h = Harness::new(dir.path());
    let a = still(&mut h, "two_cams");
    let b = still(&mut h, "translucent");
    let frames = sweep(&mut h, "two_cams", "translucent");
    // This point belongs only to the unmatched incoming node: no content before enter starts.
    let (x, y) = (240, 55);
    for (u, frame) in &frames {
        if *u <= 0.3 { assert!(close(px(frame, x, y), px(&a, x, y), 3), "early flash at {u}"); }
    }
    assert_matches("translucent endpoint", &frames[STEPS].1, &b);
    for w in frames.windows(2) {
        assert!(diff(px(&w[0].1, x, y), px(&w[1].1, x, y)) <= 24, "translucent node discontinuity at {}", w[1].0);
    }
    at_progress(&mut h, "two_cams", "translucent", 1.0);
    h.frame();
    assert_eq!(h.read(WIDE), b, "exact incoming at completion");
}

/// Default choreography must blend scene-FX-processed backgrounds, not append raw backgrounds.
#[test]
fn default_glide_preserves_scene_effects_and_background_endpoints() {
    let dir = project();
    write_file(dir.path(), "transitions/glide.toml", "kind = \"glide\"\nms = 1000\n");
    write_file(dir.path(), "scenes/fx_a.toml", "[canvas.wide]\nbackground = \"#804020\"\nfx = [{ name = \"grade\", saturation = 0.0 }]\nnodes = [{ src = \"color:#3070c0\", rect = [0.6, 0.55, 0.35, 0.35] }]\n");
    write_file(dir.path(), "scenes/fx_b.toml", "[canvas.wide]\nbackground = \"#284060\"\nfx = [{ name = \"grade\", warmth = 0.4 }]\nnodes = [{ src = \"color:#3070c0\", rect = [0.1, 0.1, 0.4, 0.4] }]\n");
    let mut h = Harness::new(dir.path());
    let a = still(&mut h, "fx_a");
    let b = still(&mut h, "fx_b");
    let frames = sweep(&mut h, "fx_a", "fx_b");
    assert_matches("default initial", &frames[0].1, &a);
    assert_matches("default endpoint", &frames[STEPS].1, &b);
    let (x, y) = (20, 160);
    let (pa, pb) = (px(&a, x, y), px(&b, x, y));
    assert!(!close(pa, pb, 20), "processed backgrounds must differ");
    for (u, frame) in &frames {
        let pixel = px(frame, x, y);
        for c in 0..3 {
            assert!(pixel[c] >= pa[c].min(pb[c]).saturating_sub(3) && pixel[c] <= pa[c].max(pb[c]).saturating_add(3), "background dip/overshoot at {u}: {pixel:?}");
        }
        let t = se_proto::Ease::Standard.apply(*u as f64) as f32;
        let r = lerp5([0.6 * W, 0.55 * H, 0.35 * W, 0.35 * H, 0.0], [0.1 * W, 0.1 * H, 0.4 * W, 0.4 * H, 0.0], t);
        let (cx, cy) = ((r[0] + r[2] * 0.5) as u32, (r[1] + r[3] * 0.5) as u32);
        assert!(!close(px(frame, cx, cy), pixel, 15), "matched camera vanished at {u}");
    }
    for w in frames.windows(2) {
        assert!(diff(px(&w[0].1, x, y), px(&w[1].1, x, y)) <= 8, "scene FX background pop at {}", w[1].0);
    }
}
