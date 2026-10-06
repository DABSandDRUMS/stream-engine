//! Runtime behavior on the real GPU: last-good pipelines on broken WGSL, particles, frames.sock
//! export (dmabuf ring + sync_file fences, shm fallback content), effect frame state
//! (`feedback`/`history`), and the no-allocation rule.

mod common;

use common::*;
use se_frames::{ClientMsg, FramesClient, FramesServer, ShmView, wait_fence};
use se_hub::media::PixelFormat;
use se_proto::Value;
use se_render::plan::{ATLAS, PREVIEW, TALL, WIDE};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[global_allocator]
static ALLOC: se_alloc::Counting = se_alloc::Counting;

const PROBE_TOML: &str = "kind = \"shader\"\nlayer = \"source\"\nparams.level = { default = 1.0 }\n";

fn probe(color: &str) -> String {
    format!("@fragment\nfn fs(in: SeVsOut) -> @location(0) vec4<f32> {{\n    return vec4<f32>({color}, 1.0) * p_level();\n}}\n")
}

#[test]
fn broken_wgsl_keeps_the_last_good_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "project.toml", PROJECT);
    write_file(root, "scenes/s.toml", "[canvas.wide]\nnodes = [{ src = \"patch.probe\" }]\n");
    write_file(root, "patches/probe/patch.toml", PROBE_TOML);
    write_file(root, "patches/probe/main.wgsl", &probe("1.0, 0.0, 0.0"));
    let mut h = Harness::new(root);
    assert_eq!(h.reports.lock().patches, vec![("probe".to_string(), Ok(()))]);
    h.set("show.scene.program", "s");
    h.frame();
    assert_eq!(px(&h.read(WIDE), 160, 90), [255, 0, 0, 255]);

    // a type error on line 3 of main.wgsl: reported with the patch line, old pipeline stays
    write_file(
        root,
        "patches/probe/main.wgsl",
        "@fragment\nfn fs(in: SeVsOut) -> @location(0) vec4<f32> {\n    let x: u32 = 1.5;\n    return vec4<f32>(0.0, 1.0, 0.0, 1.0);\n}\n",
    );
    h.loader.send(se_render::loader::LoaderCmd::Recompile).unwrap();
    h.settle();
    let err = h.reports.lock().patches.last().cloned().unwrap();
    let msg = err.1.unwrap_err();
    assert!(msg.starts_with("main.wgsl:3:"), "{msg}");
    h.frame();
    assert_eq!(px(&h.read(WIDE), 160, 90), [255, 0, 0, 255], "old pipeline still renders");

    // a syntax error is reported too, and fixing the file swaps in the new version
    write_file(root, "patches/probe/main.wgsl", "@fragment\nfn fs(in: SeVsOut) -> @location(0) vec4<f32> {\n    return vec4<f32>(0.0, 1.0, 0.0, 1.0)\n}\n");
    h.loader.send(se_render::loader::LoaderCmd::Recompile).unwrap();
    h.settle();
    let msg = h.reports.lock().patches.last().cloned().unwrap().1.unwrap_err();
    assert!(msg.starts_with("main.wgsl:"), "{msg}");
    write_file(root, "patches/probe/main.wgsl", &probe("0.0, 0.0, 1.0"));
    h.loader.send(se_render::loader::LoaderCmd::Recompile).unwrap();
    h.settle();
    assert_eq!(h.reports.lock().patches.last().cloned().unwrap().1, Ok(()));
    h.set("patch.probe.level", Value::Float(0.5));
    h.frame();
    let p = px(&h.read(WIDE), 160, 90);
    assert!(p[2] >= 126 && p[2] <= 129 && p[0] == 0, "new pipeline + live param: {p:?}");
}

#[test]
fn particles_overlay_follows_its_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "project.toml", PROJECT);
    write_file(root, "scenes/s.toml", "[canvas.wide]\nnodes = [{ src = \"color:#000000\" }]\n[canvas.tall]\nnodes = [{ src = \"color:#000000\" }]\n");
    copy_dir(&template("particles", "sparks"), &root.join("patches/sparks"));
    let mut h = Harness::new(root);
    assert!(h.reports.lock().patches.iter().all(|(_, r)| r.is_ok()), "{:?}", h.reports.lock().patches);
    h.set("show.scene.program", "s");
    let lit = |img: &(u32, u32, Vec<u8>)| img.2.chunks(4).filter(|p| p[0] > 40 || p[1] > 40).count();
    for f in 0..30u64 {
        h.frame_at(T0 + f * 16_666_667);
    }
    assert_eq!(lit(&h.read(WIDE)), 0, "no envelope: no sparks");
    h.set("patch.sparks.env", Value::Float(1.0));
    for f in 30..120u64 {
        h.frame_at(T0 + f * 16_666_667);
    }
    let img = h.read(WIDE);
    let n = lit(&img);
    assert!(n > 50, "sparks drawn: {n} lit pixels");
    let tall = h.read(TALL);
    assert!(lit(&tall) > 50, "overlay on every canvas");
}

/// The payload of the trigger that fired a shader patch reaches its header (`se.trigger`) and
/// replaces the previous one on the next trigger.
#[test]
fn trigger_payload_reaches_shader_patches() {
    use se_core::triggers::TriggerPayload;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "project.toml", PROJECT);
    write_file(root, "scenes/s.toml", "[canvas.wide]\nnodes = [{ src = \"patch.probe\" }]\n");
    write_file(root, "patches/probe/patch.toml", "kind = \"shader\"\nlayer = \"source\"\ntrigger = { hold = \"1s\" }\n");
    write_file(
        root,
        "patches/probe/main.wgsl",
        "@fragment\nfn fs(in: SeVsOut) -> @location(0) vec4<f32> {\n    let t = se.trigger;\n    return vec4<f32>(t.bits / 10000.0, t.tier / 4.0, t.user_color.b * f32(se.trigger_count), 1.0);\n}\n",
    );
    let mut h = Harness::new(root);
    assert_eq!(h.reports.lock().patches, vec![("probe".to_string(), Ok(()))]);
    h.set("show.scene.program", "s");
    h.frame();
    assert_eq!(px(&h.read(WIDE), 160, 90), [0, 0, 0, 255], "nothing fired yet");
    let fire = |h: &mut Harness, payload: Value| {
        let p = TriggerPayload::from_event(&payload, None).floats();
        h.r.apply(se_render::renderer::Msg::PatchTrigger { patch: "probe".into(), payload: p });
        h.frame();
        px(&h.read(WIDE), 160, 90)
    };
    let cheer = fire(&mut h, Value::map().with("bits", 5000).with("color", "#0000ff"));
    assert!(cheer[0].abs_diff(128) <= 1 && cheer[1] == 0 && cheer[2] == 255, "bits 5000, blue user, count 1: {cheer:?}");
    let sub = fire(&mut h, Value::map().with("tier", 2));
    assert!(sub[0] == 0 && sub[1].abs_diff(128) <= 1 && sub[2] == 0, "the next trigger replaces the payload: {sub:?}");
}

/// `se.trigger_age` counts seconds from the first frame after a trigger and restarts on the
/// next one; before any trigger it is huge.
#[test]
fn trigger_age_counts_from_each_trigger() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "project.toml", PROJECT);
    write_file(root, "scenes/s.toml", "[canvas.wide]\nnodes = [{ src = \"patch.probe\" }]\n");
    write_file(root, "patches/probe/patch.toml", "kind = \"shader\"\nlayer = \"source\"\ntrigger = { hold = \"10s\" }\n");
    write_file(root, "patches/probe/main.wgsl", "@fragment\nfn fs(in: SeVsOut) -> @location(0) vec4<f32> {\n    return vec4<f32>(min(se.trigger_age, 1.0), 0.0, 0.0, 1.0);\n}\n");
    let mut h = Harness::new(root);
    assert_eq!(h.reports.lock().patches, vec![("probe".to_string(), Ok(()))]);
    h.set("show.scene.program", "s");
    let at = |h: &mut Harness, ms: u64| {
        h.frame_at(T0 + ms * 1_000_000);
        px(&h.read(WIDE), 160, 90)[0]
    };
    assert_eq!(at(&mut h, 0), 255, "never triggered");
    let fire = |h: &mut Harness| h.r.apply(se_render::renderer::Msg::PatchTrigger { patch: "probe".into(), payload: [0.0; se_core::triggers::PAYLOAD_FLOATS] });
    fire(&mut h);
    assert_eq!(at(&mut h, 1000), 0, "the first frame after the trigger");
    assert!(at(&mut h, 1500).abs_diff(128) <= 1, "half a second later");
    fire(&mut h);
    assert_eq!(at(&mut h, 1600), 0, "a new trigger restarts the age");
    assert!(at(&mut h, 1850).abs_diff(64) <= 1, "a quarter second after the second trigger");
}

/// Enum params of a patch drawn without a slot (source/overlay/global effect) reach the shader
/// as the option index, both the manifest default and a live value.
#[test]
fn enum_params_reach_unslotted_patches_as_indices() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "project.toml", PROJECT);
    write_file(root, "scenes/s.toml", "[canvas.wide]\nnodes = [{ src = \"patch.probe\" }]\n");
    write_file(root, "patches/probe/patch.toml", "kind = \"shader\"\nlayer = \"source\"\nparams.mode = { type = \"enum\", options = [\"a\", \"b\", \"c\", \"d\"], default = \"c\" }\n");
    write_file(root, "patches/probe/main.wgsl", "@fragment\nfn fs(in: SeVsOut) -> @location(0) vec4<f32> {\n    return vec4<f32>(f32(p_mode()) / 4.0, 0.0, 0.0, 1.0);\n}\n");
    let mut h = Harness::new(root);
    assert_eq!(h.reports.lock().patches, vec![("probe".to_string(), Ok(()))]);
    h.set("show.scene.program", "s");
    h.frame();
    assert!(px(&h.read(WIDE), 160, 90)[0].abs_diff(128) <= 1, "default `c` = index 2");
    h.set("patch.probe.mode", "b");
    h.frame();
    assert!(px(&h.read(WIDE), 160, 90)[0].abs_diff(64) <= 1, "live `b` = index 1");
}

const HALF_FEEDBACK_WGSL: &str = "@fragment\nfn fs(in: SeVsOut) -> @location(0) vec4<f32> {\n    let c = textureSampleLevel(se_input, se_sampler, in.uv, 0.0);\n    let p = textureSampleLevel(se_prev, se_sampler, in.uv, 0.0);\n    return p * 0.5 + c * 0.5;\n}\n";

fn red_at(h: &mut Harness, x: u32) -> f32 {
    px(&h.read(WIDE), x, 90)[0] as f32 / 255.0
}

fn close(actual: f32, expected: f32, what: &str) {
    assert!((actual - expected).abs() <= 2.5 / 255.0, "{what}: {actual:.3} != {expected:.3}");
}

/// `se_prev` is the attachment instance's own previous output: `prev * 0.5 + input * 0.5` over
/// white converges 0.5 → 0.75 → 0.875, two slots of the same patch keep separate state, a
/// paused slot restarts from transparent, and a patch without `feedback` is unaffected.
#[test]
fn feedback_effects_see_their_own_previous_output() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "project.toml", PROJECT);
    write_file(
        root,
        "scenes/s.toml",
        "[canvas.wide]\nnodes = [{ id = 'l', src = 'color:#ffffff', rect = [0, 0, 0.5, 1], fx = [{ id = 'a', name = 'patch.half' }, { id = 'b', name = 'patch.half' }] }, { id = 'r', src = 'color:#ffffff', rect = [0.5, 0, 0.5, 1], fx = [{ id = 'p', name = 'patch.plain' }] }]\n",
    );
    write_file(root, "patches/half/patch.toml", "kind = \"shader\"\nlayer = \"effect\"\nfeedback = true\n");
    write_file(root, "patches/half/main.wgsl", HALF_FEEDBACK_WGSL);
    write_file(root, "patches/plain/patch.toml", "kind = \"shader\"\nlayer = \"effect\"\n");
    write_file(root, "patches/plain/main.wgsl", "@fragment\nfn fs(in: SeVsOut) -> @location(0) vec4<f32> {\n    return textureSampleLevel(se_input, se_sampler, in.uv, 0.0) * 0.5;\n}\n");
    let mut h = Harness::new(root);
    let mut reports = h.reports.lock().patches.clone();
    reports.sort_by(|x, y| x.0.cmp(&y.0));
    assert_eq!(reports, vec![("half".to_string(), Ok(())), ("plain".to_string(), Ok(()))]);
    h.set("show.scene.program", "s");
    // a_n = (a_{n-1} + 1) / 2; b_n = (b_{n-1} + a_n) / 2 (slot b gets slot a's output as input)
    let (mut a, mut b) = (0.0f32, 0.0f32);
    for n in 1..=4 {
        h.frame();
        a = (a + 1.0) * 0.5;
        b = (b + a) * 0.5;
        close(red_at(&mut h, 80), b, &format!("chained feedback slots, frame {n}"));
        close(red_at(&mut h, 240), 0.5, &format!("plain effect, frame {n}"));
    }
    // bypass slot a for a frame: b keeps converging on its own
    h.set("scene.s.node.l.fx.a.enabled", false);
    h.frame();
    let b_alone = red_at(&mut h, 80);
    close(b_alone, (b + 1.0) * 0.5, "slot b while a is bypassed");
    // re-enabled, slot a restarts from a cleared se_prev (0.5), not from its old 0.94
    h.set("scene.s.node.l.fx.a.enabled", true);
    h.frame();
    close(red_at(&mut h, 80), (b_alone + 0.5) * 0.5, "slot a restarted after the pause");
}

/// The global instance of a triggered feedback effect keeps state per canvas while its
/// envelope is up and starts over after it was off.
#[test]
fn feedback_global_instance_runs_canvas_wide() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "project.toml", PROJECT);
    write_file(root, "scenes/s.toml", "[canvas.wide]\nnodes = [{ id = 'w', src = 'color:#ffffff', fx = [{ id = 'ref', name = 'patch.half', enabled = false }] }]\n");
    write_file(root, "patches/half/patch.toml", "kind = \"shader\"\nlayer = \"effect\"\nfeedback = true\ntrigger = { hold = \"10s\" }\n");
    write_file(root, "patches/half/main.wgsl", HALF_FEEDBACK_WGSL);
    let mut h = Harness::new(root);
    assert_eq!(h.reports.lock().patches, vec![("half".to_string(), Ok(()))]);
    h.set("show.scene.program", "s");
    h.set("patch.half.env", 1.0);
    for expected in [0.5, 0.75, 0.875] {
        h.frame();
        close(red_at(&mut h, 160), expected, "global instance");
    }
    h.set("patch.half.env", 0.0);
    h.frame();
    close(red_at(&mut h, 160), 1.0, "envelope down: no effect");
    h.set("patch.half.env", 1.0);
    h.frame();
    close(red_at(&mut h, 160), 0.5, "next fire starts from a cleared se_prev");
}

/// A triggered patch effect with an explicit `triggered = true` attachment is scoped: it runs
/// in that attachment on its envelope and gets no canvas-wide global instance.
#[test]
fn triggered_attachment_scopes_patch_effect() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "project.toml", PROJECT);
    write_file(
        root,
        "scenes/s.toml",
        "[canvas.wide]\nnodes = [{ id = 'a', src = 'color:#ffffff', rect = [0, 0, 0.5, 1], fx = [{ id = 'x', name = 'patch.dim', triggered = true }] }, { id = 'b', src = 'color:#ffffff', rect = [0.5, 0, 0.5, 1] }]\n",
    );
    write_file(root, "patches/dim/patch.toml", "kind = \"shader\"\nlayer = \"effect\"\ntrigger = { hold = \"10s\" }\n");
    write_file(root, "patches/dim/main.wgsl", "@fragment\nfn fs(in: SeVsOut) -> @location(0) vec4<f32> {\n    return textureSampleLevel(se_input, se_sampler, in.uv, 0.0) * 0.5;\n}\n");
    let mut h = Harness::new(root);
    assert_eq!(h.reports.lock().patches, vec![("dim".to_string(), Ok(()))]);
    h.set("show.scene.program", "s");
    h.frame();
    close(red_at(&mut h, 80), 1.0, "envelope down: node a untouched");
    h.set("patch.dim.env", 1.0);
    h.frame();
    close(red_at(&mut h, 80), 0.5, "triggered attachment on node a");
    close(red_at(&mut h, 240), 1.0, "node b: no global instance");
}

/// `feedback = "state"`: `se_prev` returns what the shader wrote to `@location(1)` last frame,
/// independent of the visible output (here the output shows half the state).
#[test]
fn feedback_state_is_separate_from_the_output() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "project.toml", PROJECT);
    write_file(root, "scenes/s.toml", "[canvas.wide]\nnodes = [{ id = 'w', src = 'color:#ffffff', fx = [{ id = 's', name = 'patch.hold' }] }]\n");
    write_file(root, "patches/hold/patch.toml", "kind = \"shader\"\nlayer = \"effect\"\nfeedback = \"state\"\n");
    write_file(
        root,
        "patches/hold/main.wgsl",
        "@fragment\nfn fs(in: SeVsOut) -> SeOut {\n    let s = textureSampleLevel(se_prev, se_sampler, in.uv, 0.0) * 0.5 + textureSampleLevel(se_input, se_sampler, in.uv, 0.0) * 0.5;\n    return SeOut(vec4<f32>(s.rgb * 0.5, 1.0), s);\n}\n",
    );
    let mut h = Harness::new(root);
    assert_eq!(h.reports.lock().patches, vec![("hold".to_string(), Ok(()))]);
    h.set("show.scene.program", "s");
    // state 0.5, 0.75, 0.875, 0.9375 (a copy of the output would give 0.5, 0.625, 0.656, …)
    for state in [0.5, 0.75, 0.875, 0.9375] {
        h.frame();
        close(red_at(&mut h, 160), state * 0.5, "output = half the state");
    }
}

/// `history = 3` keeps the last three input frames: `se_history_at(uv, 2)` shows the input of
/// two frames ago once recorded (clamped to the recorded frames before that).
#[test]
fn history_ring_returns_older_input_frames() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "project.toml", PROJECT);
    write_file(root, "scenes/s.toml", "[canvas.wide]\nnodes = [{ id = 'n', src = 'patch.probe', fx = [{ id = 'h', name = 'patch.lag' }] }]\n");
    write_file(root, "patches/probe/patch.toml", PROBE_TOML);
    write_file(root, "patches/probe/main.wgsl", &probe("vec3<f32>(1.0, 0.0, 0.0)"));
    write_file(root, "patches/lag/patch.toml", "kind = \"shader\"\nlayer = \"effect\"\nhistory = 3\n");
    write_file(root, "patches/lag/main.wgsl", "@fragment\nfn fs(in: SeVsOut) -> @location(0) vec4<f32> {\n    return se_history_at(in.uv, 2u);\n}\n");
    let mut h = Harness::new(root);
    let mut reports = h.reports.lock().patches.clone();
    reports.sort_by(|x, y| x.0.cmp(&y.0));
    assert_eq!(reports, vec![("lag".to_string(), Ok(())), ("probe".to_string(), Ok(()))]);
    h.set("show.scene.program", "s");
    // input of frame k is k / 10; expected output frame by frame (age 2, clamped to the history)
    for (k, shown) in [(1, 1), (2, 1), (3, 1), (4, 2), (5, 3), (6, 4), (7, 5)] {
        h.set("patch.probe.level", k as f64 / 10.0);
        h.frame();
        close(red_at(&mut h, 160), shown as f32 / 10.0, &format!("frame {k}"));
    }
}

fn wait_demand(server: &FramesServer, canvas: u32, dmabuf: bool, shm: bool) {
    for _ in 0..200 {
        let d = server.demand(canvas);
        if d.dmabuf == dmabuf && d.shm == shm {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("server never saw the clients' hello");
}

/// A single published source stays at native pixel size as only the live node window changes.
/// Exercise both node input precomposition and atomic groups, including half-size preview.
#[test]
fn native_fit_live_windows_clip_pixels_before_node_and_group_fx() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "project.toml", PROJECT);
    for (scene, node_fx, group) in [
        ("direct", "", ""),
        ("node_fx", ",fx=[{name='fade_to_black',amount=0.5}]", ""),
        ("group", "", "\ngroups=[{id='pair',nodes=['chat']}]"),
        ("group_fx", ",fx=[{name='fade_to_black',amount=0.5}]", "\ngroups=[{id='pair',nodes=['chat'],fx=[{name='fade_to_black',amount=0.5}]}]"),
        ("stretch", "", ""),
    ] {
        let fit = if scene == "stretch" { "stretch" } else { "native" };
        write_file(root, &format!("scenes/{scene}.toml"), &format!(
            "[canvas.wide]\nnodes=[{{id='chat',src='cam',fit='{fit}',rect=[0.1,0.1333333333,0.2,0.3555555556]{node_fx}}}]{group}\n"
        ));
    }
    let sock = root.join("frames.sock");
    let server = Arc::new(FramesServer::start(&sock).unwrap());
    let mut client = FramesClient::connect(&sock).unwrap();
    client.hello(se_frames::proto::CLIENT_UI, 1 << PREVIEW, false).unwrap();
    wait_demand(&server, PREVIEW as u32, false, true);
    let mut h = Harness::with_frames(root, Some(server));
    let pattern = |x: u32, y: u32| [if x / 4 % 2 == 0 { 255 } else { 0 }, if y / 4 % 2 == 0 { 255 } else { 0 }, if y >= 48 { 255 } else { 0 }, 255];
    let mut camera = h.video("cam");
    let pixels: Vec<u8> = (0..64).flat_map(|y| (0..64).flat_map(move |x| pattern(x, y))).collect();
    camera.write(64, 64, 256, PixelFormat::Rgba8, 1, &pixels);
    let verify = |h: &mut Harness, scene: &str, left: u32, top: u32, width: u32, height: u32, sx: u32, sy: u32, gain: f32| {
        for canvas in [WIDE, PREVIEW] {
            let divisor = if canvas == PREVIEW { 2 } else { 1 };
            let img = h.read(canvas);
            assert_eq!((img.0, img.1), (320 / divisor, 180 / divisor));
            // Skip antialiased window edges, but inspect every interior source pixel/block.
            for y in 1..height / divisor - 1 {
                for x in 1..width / divisor - 1 {
                    let actual = px(&img, left / divisor + x, top / divisor + y);
                    let p = pattern(sx + x * divisor, sy + y * divisor);
                    let expected = [((p[0] as f32) * gain).round() as u8, ((p[1] as f32) * gain).round() as u8, ((p[2] as f32) * gain).round() as u8, 255];
                    assert!(actual.iter().zip(expected).all(|(a, b)| a.abs_diff(b) <= 2),
                        "{scene} canvas {canvas} content ({x},{y}): {actual:?} != {expected:?}");
                }
            }
        }
    };
    for (scene, gain) in [("direct", 1.0), ("node_fx", 0.5), ("group", 1.0), ("group_fx", 0.25)] {
        h.set("show.scene.program", scene);
        h.set("show.scene.preview", scene);
        h.frame();
        verify(&mut h, scene, 32, 24, 64, 64, 0, 0, gain);

        // No new camera/browser frame or plan reload: rect writes alone clip top/right.
        let address = format!("scene.{scene}.node.chat.rect.wide");
        h.set(&address, Value::from([0.1f32, 56.0 / 180.0, 0.1, 32.0 / 180.0]));
        h.frame();
        verify(&mut h, scene, 32, 56, 32, 32, 0, 32, gain);
        for canvas in [WIDE, PREVIEW] {
            let d = if canvas == PREVIEW { 2 } else { 1 };
            let img = h.read(canvas);
            for (x, y) in [(30, 70), (66, 70), (48, 52), (48, 92)] {
                assert_eq!(px(&img, x / d, y / d), [0, 0, 0, 255], "{scene}: clipped exterior");
            }
        }

        // A bigger window pads above/right instead of repeating the texture's edge.
        h.set(&address, Value::from([0.1f32, 8.0 / 180.0, 0.3, 80.0 / 180.0]));
        h.frame();
        verify(&mut h, scene, 32, 24, 64, 64, 0, 0, gain);
        for canvas in [WIDE, PREVIEW] {
            let d = if canvas == PREVIEW { 2 } else { 1 };
            let img = h.read(canvas);
            assert_eq!(px(&img, 48 / d, 16 / d), [0, 0, 0, 255], "{scene}: transparent top pad");
            assert_eq!(px(&img, 112 / d, 72 / d), [0, 0, 0, 255], "{scene}: transparent right pad");
        }
    }
    h.set("show.scene.program", "stretch");
    h.set("show.scene.preview", "stretch");
    h.set("scene.stretch.node.chat.rect.wide", Value::from([0.1f32, 56.0 / 180.0, 0.1, 32.0 / 180.0]));
    h.frame();
    // Ordinary stretch still scales the whole image into the shrunken window.
    for canvas in [WIDE, PREVIEW] {
        let d = if canvas == PREVIEW { 2 } else { 1 };
        let img = h.read(canvas);
        assert_eq!(px(&img, 34 / d, 58 / d)[2], 0, "stretch includes old top rows");
        assert_eq!(px(&img, 34 / d, 82 / d)[2], 255, "stretch includes bottom rows");
    }

    // Rotation uses the window center, not the smaller native content center.
    h.set("show.scene.program", "direct");
    h.set("show.scene.preview", "direct");
    h.set("scene.direct.node.chat.rotation", 90.0);
    h.frame();
    let img = h.read(WIDE);
    for (x, y) in [(36, 28), (60, 52), (88, 76)] {
        assert_eq!(px(&img, 127 - y, x - 32), pattern(x - 32, y - 24), "native rotation about window center");
    }
    assert_eq!(px(&img, 116, 4), [0, 0, 0, 255], "rotated top padding stays transparent");

    // Explicit crop remains a source-pixel crop, including transparent unused window space.
    h.set("scene.direct.node.chat.rotation", 0.0);
    h.set("scene.direct.node.chat.rect.wide", Value::from([0.1f32, 24.0 / 180.0, 0.2, 64.0 / 180.0]));
    h.set("scene.direct.node.chat.crop.wide", Value::from([0.125f32; 4]));
    h.frame();
    verify(&mut h, "direct crop", 32, 40, 48, 48, 8, 8, 1.0);
    let img = h.read(WIDE);
    assert_eq!(px(&img, 40, 32), [0, 0, 0, 255], "cropped native image pads above");
    assert_eq!(px(&img, 88, 56), [0, 0, 0, 255], "cropped native image pads right");

    // The live scale control sizes the window, not the source's glyph/pattern pixels.
    h.set("scene.direct.node.chat.crop.wide", Value::from([0.0f32; 4]));
    h.set("scene.direct.node.chat.scale", 0.5);
    h.frame();
    verify(&mut h, "direct scale", 48, 40, 32, 32, 0, 32, 1.0);
}

#[test]
fn vertical_gate_skips_tall_scene_sources_and_effects_without_changing_wide() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "project.toml", &format!(
        "{PROJECT}\n[render.canvas_fx]\ntall=[{{name='fade_to_black',amount=0.5}}]\n[render.output_fx]\ntall=[{{name='fade_to_black',amount=0.5}}]\n[overlays.badge]\ncanvases=['tall']\n"
    ));
    write_file(
        root,
        "scenes/s.toml",
        "[canvas.wide]\nnodes=[{src='shared'}]\n[canvas.tall]\nfx=[{name='fade_to_black',amount=0.5}]\nnodes=[{src='shared',rect=[0,0,0.5,1]},{src='exclusive',rect=[0.5,0,0.5,1],fx=[{name='fade_to_black',amount=0.5}]}]\n",
    );
    write_file(root, "sources/exclusive.toml", "kind='camera'\nfx=[{name='fade_to_black',amount=0.5}]\n");
    write_file(root, "patches/badge/patch.toml", "kind='shader'\nlayer='overlay'\nparams.level={default=1.0}\n");
    write_file(root, "patches/badge/main.wgsl", &probe("0.0, 1.0, 0.0"));
    let mut h = Harness::unfused(root);
    let mut shared = h.video("shared");
    let mut exclusive = h.video("exclusive");
    shared.write(16, 16, 64, PixelFormat::Rgba8, 1, &[255, 0, 0, 255].repeat(16 * 16));
    exclusive.write(16, 16, 64, PixelFormat::Rgba8, 1, &vec![255; 16 * 16 * 4]);
    h.set("show.scene.program", "s");
    h.set("render.vertical.enabled", false);
    h.frame();
    assert!(h.r.final_texture(TALL).is_none(), "disabled tall never allocates a render target");
    assert_eq!(h.stats.canvas_seq[TALL].load(Ordering::Relaxed), 0, "no tall export work");
    assert_eq!(h.stats.view().fx_passes, 0, "no tall-only source, node, layout, canvas or output FX");
    let used = |h: &Harness, name: &str| {
        let i = h.plan.source_index[name] as usize;
        h.stats.used()[i / 64] & (1 << (i % 64)) != 0
    };
    assert!(used(&h, "shared"), "shared sources still serve wide");
    assert!(!used(&h, "exclusive"), "tall-only video and its source FX are unused");
    assert!(!used(&h, "patch.badge"), "tall-only overlays are unused");
    assert_eq!(px(&h.read(WIDE), 160, 90), [255, 0, 0, 255]);

    // Missing state defaults to enabled, just as at boot before the OBS owner publishes it.
    h.unset("render.vertical.enabled");
    h.frame();
    assert_eq!(h.stats.canvas_seq[TALL].load(Ordering::Relaxed), 1);
    assert!(h.stats.view().fx_passes >= 5, "all tall-only effect stages return");
    assert!(used(&h, "exclusive") && used(&h, "patch.badge"));
    let tall = h.read(TALL);
    assert_eq!((tall.0, tall.1), (180, 320), "quality and resolution unchanged");
    assert_eq!(px(&h.read(WIDE), 160, 90), [255, 0, 0, 255]);

    h.set("render.vertical.enabled", false);
    shared.write(16, 16, 64, PixelFormat::Rgba8, 2, &[0, 0, 255, 255].repeat(16 * 16));
    h.frame();
    assert_eq!(h.read(TALL), tall, "disabled tall is not rendered even after source changes");
    assert_eq!(h.stats.canvas_seq[TALL].load(Ordering::Relaxed), 1);
    assert_eq!(h.stats.view().fx_passes, 0);
    assert_eq!(px(&h.read(WIDE), 160, 90), [0, 0, 255, 255], "wide keeps consuming fresh frames");
    h.set("render.vertical.enabled", true);
    h.frame();
    assert_eq!(h.stats.canvas_seq[TALL].load(Ordering::Relaxed), 2);
    assert!(h.stats.view().fx_passes >= 5);
    assert_eq!(px(&h.read(WIDE), 160, 90), [0, 0, 255, 255]);
}

#[test]
fn vertical_gate_suppresses_pending_exports_and_preserves_consumer_leases() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "project.toml", PROJECT);
    write_file(
        root,
        "scenes/s.toml",
        "[canvas.wide]\nnodes=[{src='cam'}]\n[canvas.tall]\nnodes=[{src='cam'}]\n",
    );
    let sock = root.join("frames.sock");
    let server = Arc::new(FramesServer::start(&sock).unwrap());
    let mut client = FramesClient::connect(&sock).unwrap();
    client.hello(se_frames::proto::CLIENT_UI, 0b1111, false).unwrap();
    for c in [WIDE, TALL, PREVIEW, ATLAS] {
        wait_demand(&server, c as u32, false, true);
    }
    let mut h = Harness::with_frames(root, Some(server));
    h.set("show.scene.program", "s");
    h.set("show.scene.preview", "s");
    let mut camera = h.video("cam");
    camera.write(16, 16, 64, PixelFormat::Rgba8, 1, &[255, 0, 0, 255].repeat(16 * 16));
    let mut generations = [None; 4];
    let mut maps: [Vec<ShmView>; 4] = std::array::from_fn(|_| Vec::new());
    let mut sizes = [(0u32, 0u32); 4];
    let mut held = None;
    let mut step = |h: &mut Harness, frame: u64, enabled: bool| {
        h.frame_at(T0 + frame * 16_666_667);
        h.r.wait_idle();
        let mut received = [0u32; 4];
        while let Some(m) = client.recv(Duration::from_millis(10)).unwrap() {
            match m {
                ClientMsg::Canvas { msg, fds } => {
                    let c = msg.canvas as usize;
                    assert!(generations[c].is_none(), "toggle must not replace canvas {c}'s ring");
                    generations[c] = Some(msg.generation);
                    sizes[c] = (msg.width, msg.height);
                    maps[c] = fds.iter().map(|fd| ShmView::map(std::os::fd::AsFd::as_fd(fd), msg.min_buffer_len() as usize).unwrap()).collect();
                }
                ClientMsg::Frame { msg, fence } => {
                    let c = msg.canvas as usize;
                    assert!(c != TALL || enabled, "disabled tall exports no frames, including pending readbacks");
                    assert_eq!(Some(msg.generation), generations[c]);
                    assert!(fence.is_none());
                    received[c] += 1;
                    if c == TALL && held.is_none() {
                        held = Some((msg.canvas, msg.buffer, msg.seq));
                    } else {
                        client.release(msg.canvas, msg.buffer, msg.seq).unwrap();
                    }
                }
                ClientMsg::Goodbye(g) => panic!("unexpected goodbye {g:?}"),
            }
        }
        if let Some((_, b, _)) = held {
            let (w, height) = sizes[TALL];
            let i = (((height / 2) * w + w / 2) * 4) as usize;
            assert_eq!(&maps[TALL][b as usize].as_slice()[i..i + 4], &[255, 0, 0, 255], "held tall buffer is never overwritten");
        }
        received
    };
    let mut before = [0u32; 4];
    for f in 0..6 {
        let n = step(&mut h, f, true);
        for c in 0..4 { before[c] += n[c]; }
    }
    assert!(before.iter().all(|n| *n > 0), "all canvases export before disable: {before:?}");
    let tall_seq = h.stats.canvas_seq[TALL].load(Ordering::Relaxed);
    let wide_seq = h.stats.canvas_seq[WIDE].load(Ordering::Relaxed);
    h.set("render.vertical.enabled", false);
    camera.write(16, 16, 64, PixelFormat::Rgba8, 2, &[0, 0, 255, 255].repeat(16 * 16));
    let mut disabled = [0u32; 4];
    for f in 6..12 {
        let n = step(&mut h, f, false);
        for c in 0..4 { disabled[c] += n[c]; }
    }
    assert_eq!(disabled[TALL], 0);
    for c in [WIDE, PREVIEW, ATLAS] {
        assert!(disabled[c] > 0, "canvas {c} exports independently of tall");
    }
    assert_eq!(h.stats.canvas_seq[TALL].load(Ordering::Relaxed), tall_seq);
    assert_eq!(h.stats.canvas_seq[WIDE].load(Ordering::Relaxed), wide_seq + 6);
    assert_eq!(px(&h.read(WIDE), 160, 90), [0, 0, 255, 255]);
    assert_eq!(px(&h.read(PREVIEW), 80, 45), [0, 0, 255, 255]);
    h.set("render.vertical.enabled", true);
    let mut resumed = [0u32; 4];
    for f in 12..18 {
        let n = step(&mut h, f, true);
        for c in 0..4 { resumed[c] += n[c]; }
    }
    assert!(resumed.iter().all(|n| *n > 0), "all canvases resume on existing rings: {resumed:?}");
    assert_eq!(h.stats.canvas_seq[TALL].load(Ordering::Relaxed), tall_seq + 6);
    assert_eq!(px(&h.read(TALL), 90, 160), [0, 0, 255, 255], "tall resumes with fresh content at original quality");
    drop(step);
    let (c, b, seq) = held.expect("a consumer lease survives disable and enable");
    client.release(c, b, seq).unwrap();
}

#[test]
fn frames_sock_exports_dmabuf_and_shm() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "project.toml", PROJECT);
    write_file(root, "scenes/s.toml", "[canvas.wide]\nnodes = [{ src = \"color:#ff8000\" }]\n[canvas.tall]\nnodes = [{ src = \"color:#0080ff\" }]\n");
    let sock = root.join("frames.sock");
    let server = Arc::new(FramesServer::start(&sock).unwrap());
    let mut h = Harness::with_frames(root, Some(server.clone()));
    h.set("show.scene.program", "s");
    h.set("show.scene.preview", "s");

    let mut gpu_client = FramesClient::connect(&sock).unwrap();
    gpu_client.hello(se_frames::proto::CLIENT_UI, 0b1111, true).unwrap();
    let mut shm_client = FramesClient::connect(&sock).unwrap();
    shm_client.hello(se_frames::proto::CLIENT_OBS, 0b0011, false).unwrap();
    wait_demand(&server, WIDE as u32, true, true);
    wait_demand(&server, PREVIEW as u32, true, false);

    let mut dmabuf_frames = [0u32; 4];
    let mut canvases = [None; 4];
    let mut shm_maps: [Vec<ShmView>; 2] = [Vec::new(), Vec::new()];
    let mut shm_sizes = [(0u32, 0u32); 2];
    let mut shm_frames = 0;
    let mut shm_pixel = [[0u8; 4]; 2];
    let mut held: Vec<(u32, u32, u64)> = Vec::new();
    for f in 0..90u64 {
        h.frame_at(T0 + f * 16_666_667);
        h.r.wait_idle();
        while let Some(m) = gpu_client.recv(Duration::from_millis(1)).unwrap() {
            match m {
                ClientMsg::Canvas { msg, fds } => {
                    assert_eq!(msg.drm_fourcc, se_frames::proto::DRM_FORMAT_ABGR8888);
                    assert_eq!(fds.len(), msg.buffer_count as usize);
                    assert_eq!(msg.buffer_count, 4);
                    for fd in &fds {
                        let size = se_frames::sys::fd_size(std::os::fd::AsFd::as_fd(fd)).unwrap();
                        assert!(size >= msg.min_buffer_len(), "dmabuf {size} < {}", msg.min_buffer_len());
                    }
                    canvases[msg.canvas as usize] = Some(msg);
                }
                ClientMsg::Frame { msg, fence } => {
                    assert_eq!(Some(msg.generation), canvases[msg.canvas as usize].map(|c| c.generation));
                    let fence = fence.expect("dmabuf frames carry a sync_file");
                    assert!(wait_fence(&fence, Duration::from_secs(2)).unwrap(), "fence signals");
                    dmabuf_frames[msg.canvas as usize] += 1;
                    // release the previous buffer of this canvas (keep one, like a real client)
                    if let Some(i) = held.iter().position(|(c, _, _)| *c == msg.canvas) {
                        let (c, b, s) = held.remove(i);
                        gpu_client.release(c, b, s).unwrap();
                    }
                    held.push((msg.canvas, msg.buffer, msg.seq));
                }
                ClientMsg::Goodbye(g) => panic!("unexpected goodbye {g:?}"),
            }
        }
        while let Some(m) = shm_client.recv(Duration::from_millis(1)).unwrap() {
            match m {
                ClientMsg::Canvas { msg, fds } => {
                    assert_eq!(msg.drm_fourcc, 0, "shm fallback");
                    let c = msg.canvas as usize;
                    shm_sizes[c] = (msg.width, msg.height);
                    shm_maps[c] = fds.iter().map(|fd| ShmView::map(std::os::fd::AsFd::as_fd(fd), msg.min_buffer_len() as usize).unwrap()).collect();
                }
                ClientMsg::Frame { msg, fence } => {
                    assert!(fence.is_none() && !msg.has_fence);
                    let c = msg.canvas as usize;
                    let (w, hh) = shm_sizes[c];
                    let data = shm_maps[c][msg.buffer as usize].as_slice();
                    let i = (((hh / 2) * w + w / 2) * 4) as usize;
                    shm_pixel[c].copy_from_slice(&data[i..i + 4]);
                    shm_frames += 1;
                    shm_client.release(msg.canvas, msg.buffer, msg.seq).unwrap();
                }
                ClientMsg::Goodbye(g) => panic!("unexpected goodbye {g:?}"),
            }
        }
    }
    let wide = canvases[WIDE].expect("wide canvas announced");
    assert_eq!((wide.width, wide.height), (320, 180));
    assert!(wide.strides[0] >= 320 * 4);
    for c in [WIDE, TALL, PREVIEW] {
        assert!(dmabuf_frames[c] >= 80, "canvas {c}: {} dmabuf frames", dmabuf_frames[c]);
    }
    assert!(dmabuf_frames[ATLAS] >= 20, "atlas at ~30 fps: {}", dmabuf_frames[ATLAS]);
    assert!(shm_frames >= 150, "shm frames {shm_frames}");
    assert_eq!(shm_pixel[WIDE], [255, 128, 0, 255]);
    assert_eq!(shm_pixel[TALL], [0, 128, 255, 255]);

    // no client wants the preview any more → it is not rendered/exported
    gpu_client.hello(se_frames::proto::CLIENT_UI, 0b0011, true).unwrap();
    wait_demand(&server, PREVIEW as u32, false, false);
    let before = server.stats().frames_sent;
    for f in 90..100u64 {
        h.frame_at(T0 + f * 16_666_667);
    }
    h.r.wait_idle();
    std::thread::sleep(Duration::from_millis(50));
    let sent = server.stats().frames_sent - before;
    assert!(sent <= 10 * 3 + 2, "preview/atlas stopped: {sent} frames for 10 renders");
}

#[test]
fn steady_state_frames_do_not_allocate() {
    assert!(se_alloc::installed());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "project.toml", "schema = 1\n[render.canvas_fx]\nwide = [{ name = \"vignette\", amount = 0.5 }]\n");
    write_file(
        root,
        "scenes/a.toml",
        "[canvas.wide]\nnodes = [{ src = \"cam_a\" }, { src = \"cam_b\", rect = [0.6, 0.6, 0.35, 0.35], radius = 24, fx = [{ name = \"grade\", warmth = 0.4 }] }]\ngroups = [{ id = \"pair\", nodes = [\"cam_a\", \"cam_b\"], fx = [{ name = \"grade\", exposure = 0.1 }, { name = \"grade\", warmth = 0.2 }] }]\n[canvas.tall]\nnodes = [{ src = \"cam_a\", rect = [0, 0, 1, 0.5] }, { src = \"cam_b\", rect = [0, 0.5, 1, 0.5] }]\ngroups = [{ id = \"pair\", nodes = [\"cam_a\", \"cam_b\"], fx = [{ name = \"vignette\", amount = 0.2 }] }]\n",
    );
    // a frame-state effect (feedback + history) on a node: its textures are allocated once
    write_file(root, "patches/trail/patch.toml", "kind = \"shader\"\nlayer = \"effect\"\nfeedback = true\nhistory = 2\n");
    write_file(
        root,
        "patches/trail/main.wgsl",
        "@fragment\nfn fs(in: SeVsOut) -> @location(0) vec4<f32> {\n    return max(se_history_at(in.uv, 2u), textureSampleLevel(se_prev, se_sampler, in.uv, 0.0) * 0.9);\n}\n",
    );
    write_file(
        root,
        "scenes/b.toml",
        "[canvas.wide]\nnodes = [{ src = \"cam_b\", fx = [{ name = \"patch.trail\" }] }, { src = \"cam_a\", rect = [0.05, 0.05, 0.3, 0.3], radius = 16 }]\ngroups = [{ id = \"pair\", nodes = [\"cam_a\", \"cam_b\"], fx = [{ name = \"grade\", exposure = 0.1 }] }]\n[canvas.tall]\nnodes = [{ src = \"cam_b\" }]\ngroups = [{ id = \"pair\", nodes = [\"cam_b\"], fx = [{ name = \"grade\", warmth = 0.2 }] }]\n",
    );
    write_file(root, "transitions/morph.toml", "kind = \"morph\"\nms = 1000\n");
    write_file(root, "transitions/glide.toml", "kind = \"glide\"\nms = 1000\n");
    let mut h = Harness::new(root);
    let mut a = h.video("cam_a");
    let mut b = h.video("cam_b");
    let yuyv = vec![128u8; 1920 * 1080 * 2];
    h.set("show.scene.program", "b");
    h.set("show.transition.active", true);
    h.set("show.transition.name", "morph");
    h.set("show.transition.from", "a");
    h.set("show.transition.ms", Value::Int(1_000_000));
    h.set("show.transition.start", Value::Int(T0 as i64));
    h.set("fx.rgb_split.amount", Value::Float(0.4));
    let run = |h: &mut Harness, a: &mut se_hub::media::VideoWriter, b: &mut se_hub::media::VideoWriter, range: std::ops::Range<u64>| {
        for f in range {
            a.write(1920, 1080, 3840, PixelFormat::Yuyv, f, &yuyv);
            b.write(1920, 1080, 3840, PixelFormat::Yuyv, f, &yuyv);
            h.frame_at(T0 + 1_000_000 + f * 16_666_667);
        }
    };
    for kind in ["morph", "glide"] {
        h.set("show.transition.name", kind);
        run(&mut h, &mut a, &mut b, 0..30);
        h.r.wait_idle();
        let before = h.stats.alloc_violations.load(Ordering::Relaxed);
        run(&mut h, &mut a, &mut b, 30..150);
        let after = h.stats.alloc_violations.load(Ordering::Relaxed);
        assert_eq!(after - before, 0, "{kind}: render thread allocated {} times in 120 steady-state frames", after - before);
        assert!(h.stats.view().frame_ms < 50.0);
        for (enabled, range) in [(false, 150..180), (true, 180..210)] {
            h.set("render.vertical.enabled", enabled);
            let before = h.stats.alloc_violations.load(Ordering::Relaxed);
            run(&mut h, &mut a, &mut b, range);
            let after = h.stats.alloc_violations.load(Ordering::Relaxed);
            assert_eq!(after - before, 0, "{kind}: vertical gate {enabled} allocated on the render thread");
        }
    }
}
