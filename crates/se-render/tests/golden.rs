//! Golden-image tests (§23): headless wgpu rendering of synthetic sources, transitions at fixed
//! progress, and effects at fixed params, compared with tolerance against
//! `tests/golden/*.png`. Regenerate (after checking the output) with `SE_UPDATE_GOLDEN=1`.

mod common;

use common::*;
use se_proto::Value;
use se_render::plan::{TALL, WIDE};

#[global_allocator]
static ALLOC: se_alloc::Counting = se_alloc::Counting;

fn project() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    write_file(root, "project.toml", PROJECT);
    write_file(
        root,
        "scenes/one.toml",
        r#"
[canvas.wide]
nodes = [
  { src = "cam_a", rect = [0, 0, 1, 1] },
  { src = "cam_b", rect = [0.6, 0.55, 0.35, 0.35], radius = 12 },
]
[canvas.tall]
nodes = [
  { src = "cam_a", rect = [0, 0, 1, 0.5] },
  { src = "cam_b", rect = [0, 0.5, 1, 0.5], crop = [0.25, 0, 0.25, 0] },
]
"#,
    );
    write_file(
        root,
        "scenes/two.toml",
        r##"
[canvas.wide]
nodes = [
  { src = "cam_b", rect = [0, 0, 1, 1] },
  { src = "color:#ff8000", rect = [0.05, 0.08, 0.2, 0.3], radius = 20, rotation = 15 },
  { src = "cam_a", rect = [0.05, 0.55, 0.4, 0.4], radius = 8, fx = [{ name = "grade", saturation = 0.0 }] },
  { src = "cam_missing", rect = [0.55, 0.55, 0.4, 0.4], when = "mode == 'live'" },
]
[canvas.tall]
nodes = [{ src = "cam_b", rect = [0, 0, 1, 1] }]
"##,
    );
    write_file(
        root,
        "scenes/yuv.toml",
        "[canvas.wide]\nnodes = [{ src = \"cam_a\", rect = [0, 0, 1, 1] }]\n[canvas.tall]\nnodes = [{ src = \"cam_a\", rect = [0, 0, 1, 1] }]\n",
    );
    write_file(root, "scenes/key.toml", "[canvas.wide]\nnodes = [{ src = \"color:#2040ff\" }, { src = \"cam_b\" }]\n");
    write_file(root, "scenes/aurora.toml", "[canvas.wide]\nnodes = [{ src = \"patch.aurora\" }]\n");
    write_file(root, "sources/cam_b.toml", "fx = [{ name = \"chroma_key\", amount = 1.0, when = \"scene == 'key'\" }]\n");
    write_file(root, "transitions/morph.toml", "kind = \"morph\"\nms = 1000\nease = \"linear\"\nenter = \"scale\"\nexit = \"fade\"\n");
    write_file(root, "transitions/zoomblur.toml", "kind = \"shader\"\nshader = \"transitions/zoomblur.wgsl\"\nms = 1000\nstrength = 0.4\n");
    write_file(
        root,
        "transitions/glitchy.toml",
        "kind = \"combined\"\nshader = \"transitions/glitch.wgsl\"\nms = 1000\nease = \"linear\"\nstrength = 1.5\nblock = 24.0\n",
    );
    std::fs::copy(example_dir().join("transitions/zoomblur.wgsl"), root.join("transitions/zoomblur.wgsl")).unwrap();
    std::fs::copy(example_dir().join("transitions/glitch.wgsl"), root.join("transitions/glitch.wgsl")).unwrap();
    copy_dir(&example_dir().join("patches/aurora"), &root.join("patches/aurora"));
    d
}

fn at_progress(h: &mut Harness, from: &str, to: &str, transition: &str, p: f64) {
    h.set("show.scene.program", to);
    h.set("show.transition.active", true);
    h.set("show.transition.name", transition);
    h.set("show.transition.from", from);
    h.set("show.transition.ms", Value::Int(1000));
    // frame time is T0 + 1 s
    h.set("show.transition.start", Value::Int((T0 + 1_000_000_000) as i64 - (p * 1e9) as i64));
}

fn no_transition(h: &mut Harness, scene: &str) {
    h.set("show.scene.program", scene);
    h.set("show.transition.active", false);
}

#[test]
fn scenes_transitions_and_effects_match_goldens() {
    let dir = project();
    let mut h = Harness::new(dir.path());
    {
        let r = h.reports.lock();
        assert!(r.transitions.iter().all(|(_, res)| res.is_ok()), "{:?}", r.transitions);
        assert!(r.patches.iter().all(|(_, res)| res.is_ok()), "{:?}", r.patches);
    }
    let _w = publish_sources(&mut h);
    h.set("palette.accent", Value::from([0.9f32, 0.3, 0.2, 1.0]));

    no_transition(&mut h, "one");
    h.frame();
    golden("one_wide", &h.read(WIDE));
    golden("one_tall", &h.read(TALL));

    no_transition(&mut h, "two");
    h.set("show.mode", "offline");
    h.frame();
    let two = h.read(WIDE);
    golden("two_wide", &two);
    // the `when` node is hidden offline and shows the no-signal slate when live
    h.set("show.mode", "live");
    h.frame();
    let live = h.read(WIDE);
    assert_eq!(px(&live, 240, 130), [0x20, 0x20, 0x28, 255], "no-signal slate");
    assert_ne!(px(&two, 240, 130), [0x20, 0x20, 0x28, 255]);

    for (name, tr, p) in [("morph", "morph", 0.5), ("fade", "fade", 0.5), ("zoomblur", "zoomblur", 0.5), ("glitchy", "glitchy", 0.5)] {
        at_progress(&mut h, "one", "two", tr, p);
        h.frame();
        golden(&format!("{name}_half_wide"), &h.read(WIDE));
    }
    // progress 0 / 1 of a crossfade equal the endpoints
    at_progress(&mut h, "one", "two", "fade", 0.0);
    h.frame();
    let start = h.read(WIDE);
    no_transition(&mut h, "one");
    h.frame();
    let one = h.read(WIDE);
    let diff = start.2.iter().zip(&one.2).map(|(a, b)| a.abs_diff(*b) as u32).max().unwrap();
    assert!(diff <= 2, "fade at 0 == outgoing scene (max diff {diff})");

    let effects: &[(&str, &[(&str, f64)])] = &[
        ("grade", &[("fx.grade.warmth", 0.6), ("fx.grade.contrast", 1.3), ("fx.grade.saturation", 1.4)]),
        ("vignette", &[("fx.vignette.amount", 1.0)]),
        ("pixelate", &[("fx.pixelate.amount", 1.0), ("fx.pixelate.size", 16.0)]),
        ("rgb_split", &[("fx.rgb_split.amount", 1.0), ("fx.rgb_split.spread", 0.03)]),
        ("blur", &[("fx.blur.amount", 1.0), ("fx.blur.radius", 12.0)]),
        ("vhs", &[("fx.vhs.amount", 0.8)]),
        ("glitch", &[("fx.glitch.amount", 1.0)]),
        ("zoom_pulse", &[("fx.zoom_pulse.amount", 1.0), ("fx.zoom_pulse.beat", 0.0), ("fx.zoom_pulse.zoom", 0.3)]),
        ("fade_to_black", &[("fx.fade_to_black.amount", 0.5)]),
    ];
    for (name, params) in effects {
        no_transition(&mut h, "yuv");
        for (a, v) in *params {
            h.set(a, Value::Float(*v));
        }
        h.frame();
        golden(&format!("fx_{name}"), &h.read(WIDE));
        for (a, _) in *params {
            h.unset(a);
        }
    }
    // triggered effect: level × envelope
    no_transition(&mut h, "yuv");
    h.set("fx.vignette.env", Value::Float(1.0));
    h.frame();
    let triggered = h.read(WIDE);
    h.unset("fx.vignette.env");
    h.set("fx.vignette.amount", Value::Float(0.6));
    h.frame();
    let latched = h.read(WIDE);
    assert_eq!(triggered.2, latched.2, "level 0.6 × env 1 == amount 0.6");
    h.unset("fx.vignette.amount");

    // source effect (chroma key on cam_b, conditional on the scene): keyed green shows blue
    no_transition(&mut h, "key");
    h.frame();
    let key = h.read(WIDE);
    golden("key_wide", &key);
    let k = px(&key, 280, 160);
    assert!(k[2] > 200 && k[1] < 120, "keyed area shows the blue background: {k:?}");

    // shader patch source compiled by the loader
    no_transition(&mut h, "aurora");
    h.frame();
    golden("aurora_wide", &h.read(WIDE));
}

#[test]
fn yuyv_conversion_is_accurate() {
    let dir = project();
    let mut h = Harness::new(dir.path());
    let _w = publish_sources(&mut h);
    no_transition(&mut h, "yuv");
    h.frame();
    let img = h.read(WIDE);
    let want = pattern_rgb(320, 180);
    let mut worst = 0u8;
    for y in 0..180u32 {
        for x in 0..320u32 {
            // skip pixels next to a bar edge (4:2:2 chroma is shared by pixel pairs)
            if (x % 40) < 2 || (x % 40) > 37 {
                continue;
            }
            let got = px(&img, x, y);
            let w = want[(y * 320 + x) as usize];
            for c in 0..3 {
                worst = worst.max(got[c].abs_diff(w[c]));
            }
            assert_eq!(got[3], 255);
        }
    }
    assert!(worst <= 3, "YUYV BT.709 limited → RGB error {worst}");
    // full-range data interpreted as limited is visibly wrong (the address matters)
    h.set("source.cam_a.range", "full");
    h.frame();
    let full = h.read(WIDE);
    assert!(px(&full, 20, 20)[0] < 235 - 10, "range switch re-converts: {:?}", px(&full, 20, 20));
}

#[test]
fn source_lut_and_color_correction_apply() {
    let dir = project();
    // inverting 2³ LUT
    let mut cube = String::from("LUT_3D_SIZE 2\n");
    for b in 0..2 {
        for g in 0..2 {
            for r in 0..2 {
                cube += &format!("{} {} {}\n", 1 - r, 1 - g, 1 - b);
            }
        }
    }
    write_file(dir.path(), "assets/invert.cube", &cube);
    let mut h = Harness::new(dir.path());
    let _w = publish_sources(&mut h);
    no_transition(&mut h, "yuv");
    h.frame();
    let plain = h.read(WIDE);
    h.set("source.cam_a.lut", "assets/invert.cube");
    h.frame(); // requests the LUT
    h.settle(); // loader parses + renderer uploads
    h.frame();
    let inv = h.read(WIDE);
    let (a, b) = (px(&plain, 20, 20), px(&inv, 20, 20));
    for c in 0..3 {
        assert!((a[c] as i32 + b[c] as i32 - 255).abs() <= 3, "inverted: {a:?} vs {b:?}");
    }
    h.set("source.cam_a.lut_amount", Value::Float(0.0));
    h.set("source.cam_a.color.brightness", Value::Float(-1.0));
    h.frame();
    assert_eq!(&px(&h.read(WIDE), 20, 20)[..3], &[0, 0, 0], "brightness −1 → black");
}
