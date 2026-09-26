//! Runtime behavior on the real GPU: last-good pipelines on broken WGSL, particles, frames.sock
//! export (dmabuf ring + sync_file fences, shm fallback content), and the no-allocation rule.

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
    copy_dir(&example_dir().join("patches/sparks"), &root.join("patches/sparks"));
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
        "[canvas.wide]\nnodes = [{ src = \"cam_a\" }, { src = \"cam_b\", rect = [0.6, 0.6, 0.35, 0.35], radius = 24, fx = [{ name = \"grade\", warmth = 0.4 }] }]\n[canvas.tall]\nnodes = [{ src = \"cam_a\", rect = [0, 0, 1, 0.5] }, { src = \"cam_b\", rect = [0, 0.5, 1, 0.5] }]\n",
    );
    write_file(
        root,
        "scenes/b.toml",
        "[canvas.wide]\nnodes = [{ src = \"cam_b\" }, { src = \"cam_a\", rect = [0.05, 0.05, 0.3, 0.3], radius = 16 }]\n[canvas.tall]\nnodes = [{ src = \"cam_b\" }]\n",
    );
    write_file(root, "transitions/morph.toml", "kind = \"morph\"\nms = 1000\n");
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
    run(&mut h, &mut a, &mut b, 0..30);
    h.r.wait_idle();
    let before = h.stats.alloc_violations.load(Ordering::Relaxed);
    run(&mut h, &mut a, &mut b, 30..150);
    let after = h.stats.alloc_violations.load(Ordering::Relaxed);
    assert_eq!(after - before, 0, "render thread allocated {} times in 120 steady-state frames", after - before);
    assert!(h.stats.view().frame_ms < 50.0);
}
