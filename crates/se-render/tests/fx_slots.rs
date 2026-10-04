//! Pixel-level FX contract regressions, using synthetic sources and the actual GPU loader.
mod common;

use common::*;
use se_proto::Value;
use se_render::plan::{PREVIEW, TALL, WIDE};

fn setup(scene: &str, project: &str) -> (tempfile::TempDir, Harness) {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "project.toml", &format!("{PROJECT}\n{project}"));
    write_file(dir.path(), "scenes/s.toml", scene);
    let mut h = Harness::new(dir.path());
    h.set("show.scene.program", "s");
    h.set("show.scene.preview", "s");
    (dir, h)
}

fn center(h: &mut Harness, canvas: usize) -> [u8; 4] {
    let image = h.read(canvas);
    px(&image, image.0 / 2, image.1 / 2)
}

fn near(actual: [u8; 4], expected: [u8; 4]) {
    assert!(actual.iter().zip(expected).all(|(a, b)| a.abs_diff(b) <= 2), "{actual:?} != {expected:?}");
}

#[test]
fn repeated_slots_have_independent_settings_and_real_trigger_bypass() {
    let (_dir, mut h) = setup(
        "[canvas.wide]\nnodes = [{ id = 'white', src = 'color:#ffffff', fx = [{id='a',name='fade_to_black',amount=0.5},{id='b',name='fade_to_black',amount=0.5}] }]",
        "",
    );
    h.frame();
    near(center(&mut h, WIDE), [64, 64, 64, 255]);
    h.set("scene.s.node.white.fx.a.amount", 0.0);
    h.frame();
    near(center(&mut h, WIDE), [128, 128, 128, 255]);
    h.set("scene.s.node.white.fx.b.triggered", true);
    h.set("scene.s.node.white.fx.b.level", 0.8);
    h.set("fx.fade_to_black.env", 1.0);
    // Suppress only the implicit master instance; the node still receives its shared envelope.
    h.set("render.output.wide.fx_enabled", false);
    h.frame();
    near(center(&mut h, WIDE), [51, 51, 51, 255]);
    h.set("scene.s.node.white.fx.b.enabled", false);
    h.frame();
    near(center(&mut h, WIDE), [255, 255, 255, 255]);
    h.set("scene.s.node.white.fx.b.enabled", true);
    h.set("scene.s.node.white.fx_enabled", false);
    h.frame();
    near(center(&mut h, WIDE), [255, 255, 255, 255]);
}

#[test]
fn repeated_pointwise_slots_keep_authored_chain_order_when_fused() {
    let (_dir, mut h) = setup(
        "[canvas.wide]\nnodes=[{id='gray',src='color:#404040',fx=[{id='lift',name='grade',lift=0.5},{id='contrast',name='grade',contrast=2.0}]}]",
        "",
    );
    h.frame();
    // .25 -> lift .625 -> contrast .75. Reversing the chain would instead give .50.
    near(center(&mut h, WIDE), [192, 192, 192, 255]);
    h.set("scene.s.node.gray.fx.lift.enabled", false);
    h.frame();
    near(center(&mut h, WIDE), [1, 1, 1, 255]);
    h.set("scene.s.node.gray.fx.lift.enabled", true);
    h.set("scene.s.node.gray.fx.contrast.enabled", false);
    h.frame();
    near(center(&mut h, WIDE), [160, 160, 160, 255]);
}

#[test]
fn group_is_one_atomic_layer_with_transparent_gaps_on_every_canvas() {
    let layout = "nodes=[{id='left',src='color:#ff0000',rect=[0,0,0.25,1],z=-10},{id='right',src='color:#00ff00',rect=[0.75,0,0.25,1],z=10},{id='blue',src='color:#0000ff',z=0}]\ngroups=[{id='pair',nodes=['left','right'],z=1,opacity=0.5,fx=[{id='dark',name='fade_to_black',amount=0.5}]}]";
    let scene = format!("[canvas.wide]\n{layout}\n[canvas.tall]\n{layout}\n");
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "project.toml", PROJECT);
    write_file(dir.path(), "scenes/s.toml", &scene);
    let socket = dir.path().join("frames.sock");
    let server = std::sync::Arc::new(se_frames::FramesServer::start(&socket).unwrap());
    let mut client = se_frames::client::FramesClient::connect(&socket).unwrap();
    client.hello(se_frames::proto::CLIENT_UI, 1 << PREVIEW, true).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !server.demand(PREVIEW as u32).dmabuf {
        assert!(std::time::Instant::now() < deadline, "preview demand not received");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let mut h = Harness::with_frames(dir.path(), Some(server));
    h.set("show.scene.program", "s");
    h.set("show.scene.preview", "s");
    h.frame();
    for canvas in [WIDE, TALL, PREVIEW] {
        let image = h.read(canvas);
        near(px(&image, image.0 / 8, image.1 / 2), [64, 0, 128, 255]);
        near(px(&image, image.0 * 7 / 8, image.1 / 2), [0, 64, 128, 255]);
        near(px(&image, image.0 / 2, image.1 / 2), [0, 0, 255, 255]);
    }
    h.set("scene.s.canvas.wide.group.pair.fx_enabled", false);
    h.frame();
    let image = h.read(WIDE);
    near(px(&image, image.0 / 8, image.1 / 2), [128, 0, 128, 255]);
    // Group bypass never restores members' individual parent z ordering.
    near(px(&image, image.0 * 7 / 8, image.1 / 2), [0, 128, 128, 255]);
}

#[test]
fn group_effect_sees_composited_pixels_not_individual_members() {
    let (_dir, mut h) = setup(
        "[canvas.wide]\nnodes=[{id='red',src='color:#ff0000'},{id='green',src='color:#00ff00',opacity=0.5,z=1}]\ngroups=[{id='pair',nodes=['red','green'],fx=[{name='chroma_key',similarity=0.05,spill=0.0}]}]\n[canvas.tall]\nnodes=[{id='red',src='color:#ff0000'},{id='green',src='color:#00ff00',opacity=0.5,z=1,fx=[{name='chroma_key',similarity=0.05,spill=0.0}]}]",
        "",
    );
    h.frame();
    near(center(&mut h, WIDE), [128, 128, 0, 255]);
    near(center(&mut h, TALL), [255, 0, 0, 255]);
}

#[test]
fn source_scene_layout_canvas_and_output_slots_stack_and_master_survives_scene_switch() {
    let scene = "fx=[{id='scene',name='fade_to_black',amount=0.5}]\n[canvas.wide]\nfx=[{id='layout',name='fade_to_black',amount=0.5}]\nnodes=[{id='video',src='cam',fx=[{id='node',name='fade_to_black',amount=0.5}]}]";
    let project = "\n[render.canvas_fx]\nwide=[{id='canvas',name='fade_to_black',amount=0.5}]\n[render.output_fx]\nwide=[{id='output',name='fade_to_black',amount=0.5}]";
    let (dir, mut h) = setup(scene, project);
    write_file(dir.path(), "sources/cam.toml", "kind='camera'\nfx=[{id='source',name='fade_to_black',amount=0.5}]\n");
    write_file(dir.path(), "scenes/other.toml", "[canvas.wide]\nnodes=[{src='color:#ffffff'}]");
    let plan = std::sync::Arc::new(se_render::plan::Plan::build(&load_config(dir.path()), &[], dir.path().to_path_buf()));
    h.r.set_plan(plan.clone());
    h.plan = plan.clone();
    h.loader.send(se_render::loader::LoaderCmd::Plan(plan)).unwrap();
    h.settle();
    let mut camera = h.video("cam");
    camera.write(16, 16, 64, se_hub::media::PixelFormat::Rgba8, 1, &vec![255; 16 * 16 * 4]);
    h.frame();
    near(center(&mut h, WIDE), [4, 4, 4, 255]);
    for (address, expected) in [
        ("source.cam.fx.source.amount", 8),
        ("scene.s.node.video.fx.node.amount", 16),
        ("scene.s.fx.scene.amount", 32),
        ("scene.s.canvas.wide.fx.layout.amount", 64),
        ("render.canvas.wide.fx.canvas.amount", 128),
        ("render.output.wide.fx.output.amount", 255),
    ] {
        h.set(address, 0.0);
        h.frame();
        near(center(&mut h, WIDE), [expected, expected, expected, 255]);
    }
    h.set("render.output.wide.fx.output.amount", 0.5);
    h.set("show.scene.program", "other");
    h.frame();
    near(center(&mut h, WIDE), [128, 128, 128, 255]);
}

#[test]
fn outgoing_and_incoming_groups_remain_isolated_during_morph() {
    let (dir, mut h) = setup(
        "[canvas.wide]\nnodes=[{id='red',src='color:#ff0000',rect=[0,0,0.25,1]}]\ngroups=[{id='pair',nodes=['red'],fx=[{name='fade_to_black',amount=0.5}]}]",
        "",
    );
    write_file(dir.path(), "scenes/other.toml", "[canvas.wide]\nnodes=[{id='red',src='color:#ff0000',rect=[0.75,0,0.25,1]}]\ngroups=[{id='pair',nodes=['red'],fx=[{name='fade_to_black',amount=0.75}]}]");
    let plan = std::sync::Arc::new(se_render::plan::Plan::build(&load_config(dir.path()), &[], dir.path().to_path_buf()));
    h.r.set_plan(plan.clone());
    h.plan = plan;
    h.set("show.scene.program", "other");
    h.set("show.transition.active", true);
    h.set("show.transition.from", "s");
    h.set("show.transition.name", "morph");
    h.set("show.transition.start", Value::Int(T0 as i64));
    h.set("show.transition.ms", 1000);
    h.frame_at(T0 + 250_000_000);
    let image = h.read(WIDE);
    let lit = image.2.chunks_exact(4).max_by_key(|p| p[0]).unwrap();
    assert!(lit[0].abs_diff(128) <= 2 && lit[1] == 0 && lit[2] == 0, "outgoing group FX: {lit:?}");
    h.frame_at(T0 + 750_000_000);
    let image = h.read(WIDE);
    let lit = image.2.chunks_exact(4).max_by_key(|p| p[0]).unwrap();
    assert!(lit[0].abs_diff(64) <= 2 && lit[1] == 0 && lit[2] == 0, "incoming group FX: {lit:?}");
}

#[test]
fn manifest_effect_slots_use_independent_full_typed_uniforms() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "project.toml", PROJECT);
    write_file(dir.path(), "patches/tint/patch.toml", "kind='shader'\nlayer='effect'\nparams.tint={type='color',default='#ffffff'}\nparams.gain={default=1.0}\n");
    write_file(dir.path(), "patches/tint/main.wgsl", "@fragment\nfn fs(in: SeVsOut) -> @location(0) vec4<f32> {\nlet c=textureSample(se_input,se_sampler,in.uv);\nreturn vec4<f32>(c.rgb*p_tint().rgb*p_gain(),c.a);\n}\n");
    write_file(dir.path(), "scenes/s.toml", "[canvas.wide]\nnodes=[{id='left',src='color:#ffffff',rect=[0,0,0.5,1],fx=[{id='tint',name='patch.tint',tint='#ff0000',gain=0.5}]},{id='right',src='color:#ffffff',rect=[0.5,0,0.5,1],fx=[{id='tint',name='patch.tint',tint='#00ff00',gain=0.25}]}]");
    let mut h = Harness::new(dir.path());
    h.set("show.scene.program", "s");
    h.frame();
    let image = h.read(WIDE);
    near(px(&image, image.0 / 4, image.1 / 2), [128, 0, 0, 255]);
    near(px(&image, image.0 * 3 / 4, image.1 / 2), [0, 64, 0, 255]);
    h.set("scene.s.node.right.fx.tint.gain", 1.0);
    h.set("scene.s.node.right.fx.tint.tint", "#0000ff");
    h.frame();
    let image = h.read(WIDE);
    near(px(&image, image.0 / 4, image.1 / 2), [128, 0, 0, 255]);
    near(px(&image, image.0 * 3 / 4, image.1 / 2), [0, 0, 255, 255]);
}

#[test]
fn local_lut_file_is_live_at_every_attachment_host() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "project.toml", &format!("{PROJECT}\n[render.canvas_fx]\nwide=[{{id='lut',name='lut',amount=0.0}}]\n[render.output_fx]\nwide=[{{id='lut',name='lut',amount=0.0}}]"));
    write_file(dir.path(), "sources/cam.toml", "kind='camera'\nfx=[{id='lut',name='lut',amount=0.0}]\n");
    write_file(dir.path(), "scenes/s.toml", "fx=[{id='lut',name='lut',amount=0.0}]\n[canvas.wide]\nfx=[{id='lut',name='lut',amount=0.0}]\nnodes=[{id='cam',src='cam',fx=[{id='lut',name='lut',amount=0.0}]}]\ngroups=[{id='pair',nodes=['cam'],fx=[{id='lut',name='lut',amount=0.0}]}]");
    write_file(dir.path(), "assets/blue.cube", &format!("LUT_3D_SIZE 2\n{}", "0 0 1\n".repeat(8)));
    write_file(dir.path(), "assets/white.cube", &format!("LUT_3D_SIZE 2\n{}", "1 1 1\n".repeat(8)));
    let mut h = Harness::new(dir.path());
    h.set("show.scene.program", "s");
    let mut camera = h.video("cam");
    camera.write(16, 16, 64, se_hub::media::PixelFormat::Rgba8, 1, &vec![255; 16 * 16 * 4]);
    for host in ["source.cam", "scene.s.node.cam", "scene.s", "scene.s.canvas.wide", "scene.s.canvas.wide.group.pair", "render.canvas.wide", "render.output.wide"] {
        h.set(&format!("{host}.fx.lut.amount"), 1.0);
        h.set(&format!("{host}.fx.lut.file"), "assets/blue.cube");
        h.frame();
        h.settle();
        h.frame();
        near(center(&mut h, WIDE), [0, 0, 255, 255]);
        h.set(&format!("{host}.fx.lut.file"), "assets/white.cube");
        h.frame();
        h.settle();
        h.frame();
        near(center(&mut h, WIDE), [255, 255, 255, 255]);
        h.set(&format!("{host}.fx.lut.amount"), 0.0);
    }
}

fn palette_effect(root: &std::path::Path, name: &str, aliases: &str) {
    write_file(root, &format!("patches/{name}/patch.toml"), &format!(
        "kind='shader'\nlayer='effect'\ntrigger={{attack='2s',hold='24s',release='3s'}}\nparams.slot={{type='int',default=0,range=[0,7]}}\n{aliases}\n"
    ));
    write_file(root, &format!("patches/{name}/main.wgsl"), r#"
@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let src = textureSample(se_input, se_sampler, in.uv);
    let tint = palette(u32(p_slot()));
    return mix(src, vec4<f32>(tint.rgb * src.a, src.a), clamp(se.env, 0.0, 1.0));
}
"#);
}

#[test]
fn live_palette_aliases_recolor_every_effect_attachment_host() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "project.toml", &format!("{PROJECT}\n[render.canvas_fx]\nwide=[{{id='tint',name='patch.tint',triggered=true}}]\n[render.output_fx]\nwide=[{{id='tint',name='patch.tint',triggered=true}}]"));
    write_file(dir.path(), "sources/cam.toml", "kind='camera'\nfx=[{id='tint',name='patch.tint',triggered=true}]\n");
    write_file(dir.path(), "scenes/s.toml", "fx=[{id='tint',name='patch.tint',triggered=true}]\n[canvas.wide]\nfx=[{id='tint',name='patch.tint',triggered=true}]\nnodes=[{id='cam',src='cam',fx=[{id='tint',name='patch.tint',triggered=true}]}]\ngroups=[{id='pair',nodes=['cam'],fx=[{id='tint',name='patch.tint',triggered=true}]}]");
    palette_effect(dir.path(), "tint", "palette={accent='lx.color.a',background='lx.color.b',foreground='lx.color.e'}");
    let mut h = Harness::new(dir.path());
    h.set("show.scene.program", "s");
    h.set("palette.accent", "#00ff00");
    let hosts = ["source.cam", "scene.s.node.cam", "scene.s", "scene.s.canvas.wide", "scene.s.canvas.wide.group.pair", "render.canvas.wide", "render.output.wide"];
    // Author trigger scope before plan compilation, then select one live host at a time.
    for host in hosts {
        h.set(&format!("{host}.fx.tint.enabled"), false);
    }
    let mut camera = h.video("cam");
    camera.write(16, 16, 64, se_hub::media::PixelFormat::Rgba8, 1, &vec![255; 16 * 16 * 4]);
    for host in hosts {
        h.set("patch.tint.env", 1.0);
        h.set(&format!("{host}.fx.tint.enabled"), true);
        for (slot, address) in [(0, "lx.color.a"), (1, "lx.color.b"), (2, "lx.color.e")] {
            h.set(&format!("{host}.fx.tint.slot"), slot);
            h.set(address, "#ff0000");
            h.frame();
            near(center(&mut h, WIDE), [255, 0, 0, 255]);
            h.set(address, "#0000ff");
            h.frame();
            near(center(&mut h, WIDE), [0, 0, 255, 255]);
        }
        h.set("patch.tint.env", 0.0);
        h.frame();
        near(center(&mut h, WIDE), [255, 255, 255, 255]);
        h.set("patch.tint.env", 1.0);
        h.frame();
        near(center(&mut h, WIDE), [0, 0, 255, 255]);
        h.set("patch.tint.env", 0.3);
        h.frame();
        near(center(&mut h, WIDE), [179, 179, 255, 255]);
        h.set(&format!("{host}.fx.tint.enabled"), false);
    }
}

#[test]
fn palette_alias_fallbacks_and_normal_patch_palette_stay_independent() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "project.toml", PROJECT);
    write_file(dir.path(), "scenes/s.toml", "[canvas.wide]\nnodes=[{id='alias',src='color:#ffffff',rect=[0,0,0.5,1],fx=[{id='tint',name='patch.alias'}]},{id='normal',src='color:#ffffff',rect=[0.5,0,0.5,1],fx=[{id='tint',name='patch.normal'}]}]");
    palette_effect(dir.path(), "alias", "palette={accent='lx.color.a'}");
    palette_effect(dir.path(), "normal", "");
    let mut h = Harness::new(dir.path());
    h.set("show.scene.program", "s");
    h.set("palette.accent", "#00ff00");
    let sample = |h: &mut Harness, left, right| {
        h.frame();
        let image = h.read(WIDE);
        near(px(&image, image.0 / 4, image.1 / 2), left);
        near(px(&image, image.0 * 3 / 4, image.1 / 2), right);
    };
    sample(&mut h, [0, 255, 0, 255], [0, 255, 0, 255]);
    for (color, expected) in [("#ff0000", [255, 0, 0, 255]), ("#0000ff", [0, 0, 255, 255])] {
        h.set("lx.color.a", color);
        sample(&mut h, expected, [0, 255, 0, 255]);
    }
    for invalid in [
        Value::from("not a color"),
        Value::Float(1.0),
        Value::from([f32::NAN, 0.0, 1.0, 1.0]),
        Value::from([1.2f32, 0.0, 0.0, 1.0]),
        Value::from([1.0f32, 0.0, 0.0, 0.0]),
        Value::from([0.0f32, 0.0]),
    ] {
        h.set("lx.color.a", invalid);
        sample(&mut h, [0, 255, 0, 255], [0, 255, 0, 255]);
    }
    h.unset("lx.color.a");
    h.set("palette.accent", "#ffff00");
    sample(&mut h, [255, 255, 0, 255], [255, 255, 0, 255]);
    // Unaliased slots keep their own live stream colors, not an aliased accent.
    h.set("lx.color.a", "#ff0000");
    h.set("palette.red", "#00ffff");
    h.set("scene.s.node.alias.fx.tint.slot", 3);
    h.set("scene.s.node.normal.fx.tint.slot", 3);
    sample(&mut h, [0, 255, 255, 255], [0, 255, 255, 255]);
}

#[test]
fn global_palette_alias_preserves_coverage_and_zero_strength_bypass() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "project.toml", PROJECT);
    write_file(dir.path(), "scenes/s.toml", "[canvas.wide]\nbackground='#00000000'\nnodes=[{src='color:#ffffff80',rect=[0,0,0.5,1]}]");
    // Register the patch's effect library entry without putting an active attachment on program.
    write_file(dir.path(), "scenes/library.toml", "[canvas.wide]\nnodes=[{src='color:#ffffff',fx=[{name='patch.tint',enabled=false}]}]");
    palette_effect(dir.path(), "tint", "palette={accent='lx.color.a'}");
    // No active explicit attachment: exercise the implicit global trigger instance.
    let mut h = Harness::new(dir.path());
    h.set("show.scene.program", "s");
    h.set("lx.color.a", "#ff0000");
    h.set("patch.tint.env", 1.0);
    h.frame();
    let image = h.read(WIDE);
    near(px(&image, image.0 / 4, image.1 / 2), [128, 0, 0, 255]);
    near(px(&image, image.0 * 3 / 4, image.1 / 2), [0, 0, 0, 255]);
    h.set("lx.color.a", "#0000ff");
    h.frame();
    let image = h.read(WIDE);
    near(px(&image, image.0 / 4, image.1 / 2), [0, 0, 128, 255]);
    h.set("patch.tint.env", 0.0);
    h.frame();
    let image = h.read(WIDE);
    near(px(&image, image.0 / 4, image.1 / 2), [128, 128, 128, 255]);
    near(px(&image, image.0 * 3 / 4, image.1 / 2), [0, 0, 0, 255]);
}
