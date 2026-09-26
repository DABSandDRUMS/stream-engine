//! Headless GPU tests: render draw lists into a 256×256 texture and check read-back pixels.

mod support;

use std::f32::consts::FRAC_PI_2;
use std::path::Path;
use std::time::{Duration, Instant};

use se_hub::draw::{DrawList, DrawOp, Paint, PathCmd, TextAlign};
use se_vector::{VectorOptions, VectorRenderer};
use support::{Pixels, Target, gpu};

const S: u32 = 256;
const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
const GREEN: [f32; 4] = [0.0, 1.0, 0.0, 1.0];
const WHITE: [f32; 4] = [1.0; 4];
/// Exactly representable in 8 bits: 51, 102, 153.
const SLATE: [f32; 4] = [0.2, 0.4, 0.6, 1.0];
const NONE: [u8; 4] = [0; 4];

fn renderer(assets: &Path) -> VectorRenderer {
    VectorRenderer::new(&gpu().device, VectorOptions { assets_root: assets.to_path_buf(), font_family: None }).expect("renderer")
}

fn draw(r: &mut VectorRenderer, ops: Vec<DrawOp>) -> Pixels {
    let g = gpu();
    let target = Target::new(g, S, S);
    r.render(&g.device, &g.queue, &DrawList { ops, seq: 1 }, &target.view, target.size).expect("render");
    target.read(g)
}

fn rect(xywh: [f32; 4], color: [f32; 4]) -> DrawOp {
    DrawOp::Rect { xywh, radius: 0.0, color, paint: Paint::Fill }
}

fn push(translate: [f32; 2], rotate: f32, scale: [f32; 2], alpha: f32) -> DrawOp {
    DrawOp::Push { translate, rotate, scale, alpha }
}

fn near(px: [u8; 4], want: [u8; 4], tol: u8) -> bool {
    px.iter().zip(want).all(|(a, b)| a.abs_diff(b) <= tol)
}

#[track_caller]
fn assert_px(p: &Pixels, x: u32, y: u32, want: [u8; 4]) {
    assert_eq!(p.at(x, y), want, "pixel ({x}, {y})");
}

#[track_caller]
fn assert_near(p: &Pixels, x: u32, y: u32, want: [u8; 4], tol: u8) {
    assert!(near(p.at(x, y), want, tol), "pixel ({x}, {y}) = {:?}, want {want:?} ±{tol}", p.at(x, y));
}

/// Render until every requested image has decoded (they load on another thread).
fn render_until_loaded(r: &mut VectorRenderer, ops: &[DrawOp]) -> Pixels {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let p = draw(r, ops.to_vec());
        if r.images_pending() == 0 {
            return draw(r, ops.to_vec());
        }
        assert!(Instant::now() < deadline, "images still pending after 10 s");
        std::thread::sleep(Duration::from_millis(5));
        drop(p);
    }
}

#[test]
fn filled_rect_is_exact_inside_and_transparent_outside() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = renderer(dir.path());
    let p = draw(&mut r, vec![rect([0.25, 0.25, 0.5, 0.5], SLATE)]);
    assert_px(&p, 128, 128, [51, 102, 153, 255]);
    assert_px(&p, 65, 65, [51, 102, 153, 255]);
    assert_px(&p, 32, 32, NONE);
    assert_px(&p, 200, 128, NONE);
    // Pixel-aligned edges: 64..192.
    assert_eq!(p.ink_bbox(), Some([64, 64, 191, 191]));
}

#[test]
fn box_width_is_relative_to_layer_width() {
    let g = gpu();
    let dir = tempfile::tempdir().unwrap();
    let mut r = renderer(dir.path());
    let target = Target::new(g, 512, 128);
    let ops = vec![rect([0.0, 0.0, 1.0, 1.0], SLATE), DrawOp::Circle { center: [0.5, 0.5], radius: 0.25, color: RED, paint: Paint::Fill }];
    r.render(&g.device, &g.queue, &DrawList { ops, seq: 1 }, &target.view, target.size).unwrap();
    let p = target.read(g);
    // (0,0,1,1) fills the whole 4:1 layer.
    assert_px(&p, 0, 0, [51, 102, 153, 255]);
    assert_px(&p, 511, 127, [51, 102, 153, 255]);
    // Circle radius is in height units: 0.25 · 128 = 32 px, so it stays round.
    assert_px(&p, 256 + 28, 64, [255, 0, 0, 255]);
    assert_px(&p, 256 + 36, 64, [51, 102, 153, 255]);
    assert_px(&p, 256, 64 - 28, [255, 0, 0, 255]);
}

#[test]
fn rounded_rect_corner_is_transparent() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = renderer(dir.path());
    let p = draw(&mut r, vec![DrawOp::Rect { xywh: [0.25, 0.25, 0.5, 0.5], radius: 0.1, color: RED, paint: Paint::Fill }]);
    assert_px(&p, 64, 64, NONE);
    assert_px(&p, 191, 191, NONE);
    assert_px(&p, 128, 65, [255, 0, 0, 255]);
    assert_px(&p, 65, 128, [255, 0, 0, 255]);
    assert_px(&p, 128, 128, [255, 0, 0, 255]);
}

#[test]
fn stroke_outlines_while_fill_covers() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = renderer(dir.path());
    // 0.04 · 256 = 10.24 px, centred on the edges at x/y = 64 and 192.
    let stroked = draw(&mut r, vec![DrawOp::Rect { xywh: [0.25, 0.25, 0.5, 0.5], radius: 0.0, color: RED, paint: Paint::Stroke(0.04) }]);
    assert_px(&stroked, 128, 128, NONE);
    assert_px(&stroked, 64, 128, [255, 0, 0, 255]);
    assert_px(&stroked, 60, 128, [255, 0, 0, 255]);
    assert_px(&stroked, 128, 191, [255, 0, 0, 255]);
    assert_px(&stroked, 52, 128, NONE);
    assert_px(&stroked, 76, 128, NONE);
    // Miter joins keep the outer corner square.
    assert_px(&stroked, 60, 60, [255, 0, 0, 255]);

    let filled = draw(&mut r, vec![DrawOp::Rect { xywh: [0.25, 0.25, 0.5, 0.5], radius: 0.0, color: RED, paint: Paint::Fill }]);
    assert_px(&filled, 128, 128, [255, 0, 0, 255]);
    assert_px(&filled, 60, 128, NONE);
}

#[test]
fn circle_fill_and_stroke() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = renderer(dir.path());
    // Radius 0.25 · 256 = 64 px around (128, 128).
    let p = draw(&mut r, vec![DrawOp::Circle { center: [0.5, 0.5], radius: 0.25, color: GREEN, paint: Paint::Fill }]);
    assert_px(&p, 128, 128, [0, 255, 0, 255]);
    assert_px(&p, 128 + 60, 128, [0, 255, 0, 255]);
    assert_px(&p, 128, 128 - 60, [0, 255, 0, 255]);
    assert_px(&p, 128 + 70, 128, NONE);
    // Inside the bounding square but outside the disc.
    assert_px(&p, 72, 72, NONE);

    let ring = draw(&mut r, vec![DrawOp::Circle { center: [0.5, 0.5], radius: 0.25, color: GREEN, paint: Paint::Stroke(0.02) }]);
    assert_px(&ring, 128, 128, NONE);
    assert_px(&ring, 128 + 63, 128, [0, 255, 0, 255]);
    assert_px(&ring, 128 + 50, 128, NONE);
}

#[test]
fn line_has_width_and_butt_caps() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = renderer(dir.path());
    // x 25.6..230.4, width 0.05 · 256 = 12.8 px → y 121.6..134.4.
    let p = draw(&mut r, vec![DrawOp::Line { a: [0.1, 0.5], b: [0.9, 0.5], width: 0.05, color: WHITE }]);
    assert_px(&p, 128, 128, [255; 4]);
    assert_px(&p, 128, 123, [255; 4]);
    assert_px(&p, 128, 133, [255; 4]);
    assert_px(&p, 128, 118, NONE);
    assert_px(&p, 128, 138, NONE);
    assert_px(&p, 30, 128, [255; 4]);
    // Round or square caps would reach 6.4 px past the end points.
    assert_px(&p, 21, 128, NONE);
    assert_px(&p, 234, 128, NONE);
}

#[test]
fn path_triangle_fill_and_stroke() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = renderer(dir.path());
    let tri = vec![PathCmd::MoveTo([0.5, 0.1]), PathCmd::LineTo([0.9, 0.9]), PathCmd::LineTo([0.1, 0.9]), PathCmd::Close];
    let p = draw(&mut r, vec![DrawOp::Path { cmds: tri.clone(), color: RED, paint: Paint::Fill }]);
    // Centroid (0.5, 0.633).
    assert_px(&p, 128, 162, [255, 0, 0, 255]);
    assert_px(&p, 128, 220, [255, 0, 0, 255]);
    assert_px(&p, 51, 51, NONE);
    assert_px(&p, 205, 51, NONE);
    assert_px(&p, 128, 240, NONE);

    let outline = draw(&mut r, vec![DrawOp::Path { cmds: tri, color: RED, paint: Paint::Stroke(0.02) }]);
    assert_px(&outline, 128, 162, NONE);
    // Bottom edge y = 230.4.
    assert_px(&outline, 128, 230, [255, 0, 0, 255]);

    // Curves: a quad and a cubic hump filled non-zero.
    let curve =
        vec![PathCmd::MoveTo([0.1, 0.8]), PathCmd::QuadTo([0.3, 0.2], [0.5, 0.8]), PathCmd::CubicTo([0.6, 0.2], [0.8, 0.2], [0.9, 0.8]), PathCmd::Close];
    let c = draw(&mut r, vec![DrawOp::Path { cmds: curve, color: GREEN, paint: Paint::Fill }]);
    assert_px(&c, 77, 180, [0, 255, 0, 255]);
    assert_px(&c, 180, 150, [0, 255, 0, 255]);
    assert_px(&c, 128, 150, NONE);
}

fn text_op(x: f32, align: TextAlign) -> DrawOp {
    DrawOp::Text { pos: [x, 0.6], size: 0.2, color: WHITE, text: "Hello".into(), align }
}

#[test]
fn text_renders_in_its_box_and_aligns() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = renderer(dir.path());
    // size 0.2 → 51.2 px em; baseline at y = 153.6.
    let left = draw(&mut r, vec![text_op(0.1, TextAlign::Left)]);
    let [x0, y0, x1, y1] = left.ink_bbox().expect("text drew something");
    assert!(left.ink_count() > 300, "only {} inked pixels", left.ink_count());
    assert!((24..=34).contains(&x0), "left edge {x0}");
    assert!(x1 > x0 + 80 && x1 < 25 + 5 * 51, "right edge {x1}");
    // Cap height well above the baseline, nothing below it ("Hello" has no descenders).
    assert!(y0 > 153 - 51 && y0 < 153 - 25, "top {y0}");
    assert!((150..=155).contains(&y1), "bottom {y1}");
    let width = x1 - x0;

    let center = draw(&mut r, vec![text_op(0.5, TextAlign::Center)]);
    let [cx0, cy0, cx1, cy1] = center.ink_bbox().expect("centered text");
    assert_eq!((cy0, cy1), (y0, y1), "alignment must not move text vertically");
    assert!(cx1 - cx0 == width, "same glyphs, same width");
    let mid = (cx0 + cx1) / 2;
    assert!(mid.abs_diff(128) <= 6, "centered text midpoint {mid}");

    let right = draw(&mut r, vec![text_op(0.5, TextAlign::Right)]);
    let [rx0, _, rx1, _] = right.ink_bbox().expect("right-aligned text");
    assert!(rx1.abs_diff(128) <= 6, "right edge {rx1}");
    assert!(rx0 < cx0 && cx0 < 128, "right-aligned text sits left of centered text");

    // Transparent text and empty strings draw nothing.
    let none = draw(&mut r, vec![DrawOp::Text { pos: [0.1, 0.6], size: 0.2, color: [1.0, 1.0, 1.0, 0.0], text: "Hello".into(), align: TextAlign::Left }]);
    assert_eq!(none.ink_count(), 0);
    let empty = draw(&mut r, vec![DrawOp::Text { pos: [0.1, 0.6], size: 0.2, color: WHITE, text: String::new(), align: TextAlign::Left }]);
    assert_eq!(empty.ink_count(), 0);
}

#[test]
fn multiline_text_stacks_lines_below_the_first_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = renderer(dir.path());
    let one = draw(&mut r, vec![DrawOp::Text { pos: [0.1, 0.3], size: 0.1, color: WHITE, text: "Hi".into(), align: TextAlign::Left }]);
    let two = draw(&mut r, vec![DrawOp::Text { pos: [0.1, 0.3], size: 0.1, color: WHITE, text: "Hi\nHi".into(), align: TextAlign::Left }]);
    let [_, a_top, _, a_bottom] = one.ink_bbox().unwrap();
    let [_, b_top, _, b_bottom] = two.ink_bbox().unwrap();
    assert_eq!(a_top, b_top, "first line keeps its baseline");
    assert!(b_bottom > a_bottom + 20, "second line below the first ({b_bottom} vs {a_bottom})");
}

#[test]
fn push_translate_scale_rotate() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = renderer(dir.path());
    let square = rect([0.0, 0.0, 0.25, 0.25], RED);

    let moved = draw(&mut r, vec![push([0.5, 0.5], 0.0, [1.0, 1.0], 1.0), square.clone(), DrawOp::Pop]);
    assert_px(&moved, 160, 160, [255, 0, 0, 255]);
    assert_px(&moved, 32, 32, NONE);
    assert_eq!(moved.ink_bbox(), Some([128, 128, 191, 191]));

    // Scale 2 about the origin: 25.6..51.2 → 51.2..102.4.
    let scaled = draw(&mut r, vec![push([0.0, 0.0], 0.0, [2.0, 2.0], 1.0), rect([0.1, 0.1, 0.1, 0.1], RED), DrawOp::Pop]);
    assert_px(&scaled, 90, 90, [255, 0, 0, 255]);
    assert_px(&scaled, 40, 40, NONE);

    // Rotate +90° about the translated origin: (x, y) → (−y, x); 64×25.6 px lands at
    // x 102.4..128, y 128..192.
    let rotated = draw(&mut r, vec![push([0.5, 0.5], FRAC_PI_2, [1.0, 1.0], 1.0), rect([0.0, 0.0, 0.25, 0.1], RED), DrawOp::Pop]);
    assert_px(&rotated, 115, 170, [255, 0, 0, 255]);
    assert_px(&rotated, 160, 140, NONE);

    // Nested pushes compose; ops after the Pop are back in the parent space.
    let nested = draw(
        &mut r,
        vec![
            push([0.25, 0.0], 0.0, [1.0, 1.0], 1.0),
            push([0.0, 0.25], 0.0, [1.0, 1.0], 1.0),
            rect([0.0, 0.0, 0.1, 0.1], RED),
            DrawOp::Pop,
            rect([0.0, 0.0, 0.1, 0.1], GREEN),
            DrawOp::Pop,
            rect([0.0, 0.0, 0.1, 0.1], WHITE),
        ],
    );
    assert_px(&nested, 64 + 12, 64 + 12, [255, 0, 0, 255]);
    assert_px(&nested, 64 + 12, 12, [0, 255, 0, 255]);
    assert_px(&nested, 12, 12, [255; 4]);
}

#[test]
fn push_alpha_multiplies() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = renderer(dir.path());
    let p = draw(
        &mut r,
        vec![
            push([0.0, 0.0], 0.0, [1.0, 1.0], 0.5),
            rect([0.0, 0.0, 0.25, 0.25], WHITE),
            push([0.0, 0.0], 0.0, [1.0, 1.0], 0.5),
            rect([0.5, 0.5, 0.25, 0.25], WHITE),
            DrawOp::Pop,
            DrawOp::Pop,
            rect([0.0, 0.5, 0.25, 0.25], [1.0, 1.0, 1.0, 0.5]),
        ],
    );
    // Straight alpha: color stays white, alpha carries the fade.
    assert_near(&p, 32, 32, [255, 255, 255, 128], 1);
    assert_near(&p, 160, 160, [255, 255, 255, 64], 1);
    assert_near(&p, 32, 160, [255, 255, 255, 128], 1);
}

#[test]
fn unbalanced_pop_is_ignored_and_unclosed_push_does_not_leak() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = renderer(dir.path());
    let small = rect([0.0, 0.0, 0.1, 0.1], RED);
    let p = draw(&mut r, vec![DrawOp::Pop, DrawOp::Pop, small.clone()]);
    assert_px(&p, 12, 12, [255, 0, 0, 255]);

    let open = draw(&mut r, vec![push([0.5, 0.5], 0.0, [1.0, 1.0], 0.5), small.clone()]);
    assert_px(&open, 12, 12, NONE);
    assert_near(&open, 140, 140, [255, 0, 0, 128], 1);
    // The next list starts from identity / full alpha again.
    let next = draw(&mut r, vec![small]);
    assert_px(&next, 12, 12, [255, 0, 0, 255]);
    assert_px(&next, 140, 140, NONE);

    // Non-finite push values keep the parent transform (and still need their Pop).
    let bad = draw(&mut r, vec![push([f32::NAN, 0.0], 0.0, [1.0, 1.0], 1.0), rect([0.0, 0.0, 0.1, 0.1], GREEN), DrawOp::Pop]);
    assert_px(&bad, 12, 12, [0, 255, 0, 255]);
}

#[test]
fn clear_sets_background_and_discards_earlier_ops() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = renderer(dir.path());
    let empty = draw(&mut r, vec![]);
    assert_eq!(empty.ink_count(), 0, "nothing drawn → fully transparent");

    let p = draw(&mut r, vec![rect([0.0, 0.0, 1.0, 1.0], RED), DrawOp::Clear(SLATE), rect([0.5, 0.5, 0.5, 0.5], GREEN)]);
    assert_px(&p, 32, 32, [51, 102, 153, 255]);
    assert_px(&p, 200, 200, [0, 255, 0, 255]);

    // Last clear wins.
    let twice = draw(&mut r, vec![DrawOp::Clear(RED), rect([0.0, 0.0, 0.5, 0.5], GREEN), DrawOp::Clear(SLATE)]);
    assert_px(&twice, 32, 32, [51, 102, 153, 255]);
    assert_px(&twice, 200, 200, [51, 102, 153, 255]);

    // A translucent background comes out as straight alpha.
    let half = draw(&mut r, vec![DrawOp::Clear([1.0, 0.0, 0.0, 0.5])]);
    assert_near(&half, 10, 10, [255, 0, 0, 128], 1);

    // A clear inside a push discards pixels, not the transform.
    let scoped = draw(
        &mut r,
        vec![push([0.5, 0.5], 0.0, [1.0, 1.0], 1.0), rect([0.0, 0.0, 0.1, 0.1], RED), DrawOp::Clear(SLATE), rect([0.0, 0.0, 0.1, 0.1], GREEN), DrawOp::Pop],
    );
    assert_px(&scoped, 140, 140, [0, 255, 0, 255]);
    assert_px(&scoped, 12, 12, [51, 102, 153, 255]);
}

fn write_quad_png(path: &Path) {
    // 2×2: red, blue / green, white.
    let px = [[255, 0, 0, 255], [0, 0, 255, 255], [0, 255, 0, 255], [255, 255, 255, 255]];
    let img = image::RgbaImage::from_fn(2, 2, |x, y| image::Rgba(px[(y * 2 + x) as usize]));
    img.save(path).unwrap();
}

#[test]
fn image_loads_off_thread_then_draws() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("logos")).unwrap();
    write_quad_png(&dir.path().join("logos/quad.png"));
    let mut r = renderer(dir.path());
    let ops = vec![DrawOp::Image { path: "logos/quad.png".into(), xywh: [0.25, 0.25, 0.5, 0.5], opacity: 1.0 }];

    let first = draw(&mut r, ops.clone());
    assert_eq!(first.ink_count(), 0, "nothing until the image is decoded");
    assert!(r.images_pending() > 0);

    let p = render_until_loaded(&mut r, &ops);
    assert_eq!(r.images_pending(), 0);
    // Each texel covers 64×64 px (centres at 96 and 160). Pixels sampled on the outer side
    // of a texel centre only blend with the padded edge, so they are exact.
    assert_near(&p, 95, 95, [255, 0, 0, 255], 1);
    assert_near(&p, 160, 95, [0, 0, 255, 255], 1);
    assert_near(&p, 95, 160, [0, 255, 0, 255], 1);
    assert_near(&p, 160, 160, [255, 255, 255, 255], 1);
    assert_px(&p, 32, 32, NONE);
    assert_px(&p, 220, 220, NONE);

    // Opacity and the alpha stack both fade the image.
    let faded = draw(
        &mut r,
        vec![push([0.0, 0.0], 0.0, [1.0, 1.0], 0.5), DrawOp::Image { path: "logos/quad.png".into(), xywh: [0.25, 0.25, 0.5, 0.5], opacity: 0.5 }, DrawOp::Pop],
    );
    assert_near(&faded, 95, 95, [255, 0, 0, 64], 1);
}

#[test]
fn image_path_escapes_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let assets = dir.path().join("assets");
    std::fs::create_dir(&assets).unwrap();
    // The escape target exists; it must still never be read.
    write_quad_png(&dir.path().join("x.png"));
    let abs = dir.path().join("x.png").to_string_lossy().into_owned();
    let mut r = renderer(&assets);
    let ops = vec![
        DrawOp::Image { path: "../x.png".into(), xywh: [0.0, 0.0, 1.0, 1.0], opacity: 1.0 },
        DrawOp::Image { path: "sub/../../x.png".into(), xywh: [0.0, 0.0, 1.0, 1.0], opacity: 1.0 },
        DrawOp::Image { path: abs, xywh: [0.0, 0.0, 1.0, 1.0], opacity: 1.0 },
        DrawOp::Image { path: "missing.png".into(), xywh: [0.0, 0.0, 1.0, 1.0], opacity: 1.0 },
    ];
    let first = draw(&mut r, ops.clone());
    assert_eq!(first.ink_count(), 0);
    assert_eq!(r.images_pending(), 1, "only the in-root path is requested");
    let p = render_until_loaded(&mut r, &ops);
    assert_eq!(p.ink_count(), 0);
    // Failures are remembered: no re-request on later renders.
    draw(&mut r, ops);
    assert_eq!(r.images_pending(), 0);
}

#[test]
fn set_options_switches_assets_root() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255])).save(a.path().join("i.png")).unwrap();
    image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 255, 255])).save(b.path().join("i.png")).unwrap();
    let mut r = renderer(a.path());
    let ops = vec![DrawOp::Image { path: "i.png".into(), xywh: [0.0, 0.0, 1.0, 1.0], opacity: 1.0 }];
    let p = render_until_loaded(&mut r, &ops);
    assert_near(&p, 128, 128, [255, 0, 0, 255], 1);

    r.set_options(VectorOptions { assets_root: b.path().to_path_buf(), font_family: Some("Liberation Sans".into()) });
    assert_eq!(r.images_pending(), 0);
    let reloading = draw(&mut r, ops.clone());
    assert_eq!(reloading.ink_count(), 0, "cache cleared on set_options");
    let p = render_until_loaded(&mut r, &ops);
    assert_near(&p, 128, 128, [0, 0, 255, 255], 1);
}

#[test]
fn degenerate_ops_draw_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = renderer(dir.path());
    let p = draw(
        &mut r,
        vec![
            rect([f32::NAN, 0.0, 0.5, 0.5], RED),
            rect([0.0, 0.0, f32::INFINITY, 0.5], RED),
            DrawOp::Circle { center: [0.5, 0.5], radius: 0.0, color: RED, paint: Paint::Fill },
            DrawOp::Line { a: [0.1, 0.5], b: [0.9, 0.5], width: 0.0, color: RED },
            DrawOp::Rect { xywh: [0.0, 0.0, 1.0, 1.0], radius: 0.0, color: RED, paint: Paint::Stroke(-1.0) },
            DrawOp::Path {
                cmds: vec![PathCmd::MoveTo([0.0, 0.0]), PathCmd::LineTo([1.0, f32::NAN]), PathCmd::LineTo([1.0, 1.0])],
                color: RED,
                paint: Paint::Fill,
            },
            DrawOp::Path { cmds: vec![], color: RED, paint: Paint::Fill },
            DrawOp::Text { pos: [0.1, 0.5], size: f32::NAN, color: RED, text: "x".into(), align: TextAlign::Left },
            DrawOp::Image { path: "x.png".into(), xywh: [0.0, 0.0, 0.0, 1.0], opacity: 1.0 },
        ],
    );
    assert_eq!(p.ink_count(), 0);
    // Negative box sizes are normalized rather than dropped.
    let flipped = draw(&mut r, vec![rect([0.75, 0.75, -0.5, -0.5], RED)]);
    assert_eq!(flipped.ink_bbox(), Some([64, 64, 191, 191]));
}

#[test]
fn bad_targets_are_errors() {
    let g = gpu();
    let dir = tempfile::tempdir().unwrap();
    let mut r = renderer(dir.path());
    let list = DrawList { ops: vec![rect([0.0, 0.0, 1.0, 1.0], RED)], seq: 1 };
    let target = Target::new(g, 64, 64);
    assert!(r.render(&g.device, &g.queue, &list, &target.view, [128, 64]).is_err(), "size larger than the texture");
    assert!(r.render(&g.device, &g.queue, &list, &target.view, [0, 64]).is_err(), "zero size");
    let srgb = g.device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: 64, height: 64, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = srgb.create_view(&wgpu::TextureViewDescriptor::default());
    assert!(r.render(&g.device, &g.queue, &list, &view, [64, 64]).is_err(), "wrong format");
    // Rendering a sub-rectangle of a larger target is fine.
    r.render(&g.device, &g.queue, &list, &target.view, [32, 32]).unwrap();
    let p = target.read(g);
    assert_px(&p, 16, 16, [255, 0, 0, 255]);
}
