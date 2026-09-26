//! Headless test harness: a real renderer on the GPU with synthetic sources, a hand-built
//! snapshot, the real loader thread, and readback of canvas textures.

#![allow(dead_code)]

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use se_core::config::{Config, SourceFile};
use se_hub::Snapshot;
use se_hub::media::{PixelFormat, VideoSlots, VideoWriter};
use se_proto::Value;
use se_render::gpu::{Gpu, GpuOptions};
use se_render::loader::{Loader, LoaderCmd, Report};
use se_render::perf::Stats;
use se_render::plan::Plan;
use se_render::renderer::{AssetRequest, Inputs, Msg, Renderer};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub const T0: u64 = 10_000_000_000;

#[derive(Default)]
pub struct Reports {
    pub patches: Vec<(String, Result<(), String>)>,
    pub transitions: Vec<(String, Result<(), String>)>,
    pub styles: Vec<(String, Result<(), String>)>,
}

pub struct TestReport(pub Arc<Mutex<Reports>>);

impl Report for TestReport {
    fn patch(&self, id: &str, result: Result<(), String>) {
        self.0.lock().patches.push((id.to_string(), result));
    }
    fn transition(&self, name: &str, result: Result<(), String>) {
        self.0.lock().transitions.push((name.to_string(), result));
    }
    fn style(&self, path: &str, result: Result<(), String>) {
        self.0.lock().styles.push((path.to_string(), result));
    }
}

pub struct Harness {
    pub r: Renderer,
    pub inputs: Inputs,
    pub plan: Arc<Plan>,
    pub loader: Sender<LoaderCmd>,
    pub msgs: Receiver<Msg>,
    pub assets: Receiver<AssetRequest>,
    pub reports: Arc<Mutex<Reports>>,
    pub state: BTreeMap<String, Value>,
    pub signals: BTreeMap<String, f32>,
    pub slots: VideoSlots,
    pub root: PathBuf,
    pub stats: Arc<Stats>,
    /// Apply fused effect pipelines from the loader (false = every effect runs its own pass).
    pub fuse: bool,
    loader_thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        // the loader holds a device reference: release it before the process tears down the
        // driver
        let _ = self.loader.send(LoaderCmd::Shutdown);
        if let Some(t) = self.loader_thread.take() {
            let _ = t.join();
        }
    }
}

/// Read the project files under `root` (like the engine's loader does, kinds by directory).
pub fn load_config(root: &Path) -> Config {
    let mut files = Vec::new();
    let mut add = |kind: &str, path: &Path| {
        let src = std::fs::read_to_string(path).unwrap();
        files.push(SourceFile {
            kind: kind.to_string(),
            name: path.file_stem().unwrap().to_string_lossy().to_string(),
            path: path.strip_prefix(root).unwrap().to_string_lossy().to_string(),
            table: toml::from_str(&src).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
        });
    };
    add("project", &root.join("project.toml"));
    for kind in ["scenes", "transitions", "presets", "sources"] {
        let Ok(rd) = std::fs::read_dir(root.join(kind)) else { continue };
        let mut paths: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "toml")).collect();
        paths.sort();
        for p in paths {
            add(kind, &p);
        }
    }
    let c = Config::build(&files);
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    c
}

pub fn write_file(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

pub fn template(kind: &str, name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates/patches").join(kind).join(name)
}

pub fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            copy_dir(&p, &to.join(e.file_name()));
        } else {
            std::fs::copy(&p, to.join(e.file_name())).unwrap();
        }
    }
}

impl Harness {
    pub fn new(root: &Path) -> Harness {
        Self::with_frames(root, None)
    }

    /// A harness whose renderer never receives fused effect pipelines (the reference path).
    pub fn unfused(root: &Path) -> Harness {
        Self::build(root, None, false)
    }

    pub fn with_frames(root: &Path, frames: Option<Arc<se_frames::FramesServer>>) -> Harness {
        Self::build(root, frames, true)
    }

    fn build(root: &Path, frames: Option<Arc<se_frames::FramesServer>>, fuse: bool) -> Harness {
        let cfg = load_config(root);
        let manifests: Vec<Arc<se_patch::Manifest>> = se_patch::scan(root).into_iter().map(|m| Arc::new(m.unwrap())).collect();
        let plan = Arc::new(Plan::build(&cfg, &manifests, root.to_path_buf()));
        assert!(plan.errors.is_empty(), "{:?}", plan.errors);
        let gpu = Gpu::new(&GpuOptions::default()).expect("GPU");
        let (asset_tx, asset_rx) = crossbeam_channel::unbounded();
        let stats = Arc::new(Stats::default());
        let mut r = Renderer::new(gpu, 1, plan.clone(), asset_tx, frames, stats.clone()).expect("renderer");
        r.set_time_origin(T0);
        let (loader_tx, loader_rx) = crossbeam_channel::unbounded();
        let (msg_tx, msg_rx) = crossbeam_channel::unbounded();
        let reports = Arc::new(Mutex::new(Reports::default()));
        let loader_thread = Some(Loader::spawn(loader_rx, msg_tx, Box::new(TestReport(reports.clone()))).unwrap());
        loader_tx.send(LoaderCmd::Device { device: r.gpu.device.clone(), layouts: r.layouts(), generation: 1 }).unwrap();
        loader_tx.send(LoaderCmd::Plan(plan.clone())).unwrap();
        let mut h = Harness {
            r,
            inputs: Inputs::default(),
            plan,
            loader: loader_tx,
            msgs: msg_rx,
            assets: asset_rx,
            reports,
            state: BTreeMap::new(),
            signals: BTreeMap::new(),
            slots: VideoSlots::default(),
            root: root.to_path_buf(),
            stats,
            fuse,
            loader_thread,
        };
        h.settle();
        h
    }

    /// Register a video slot; its reader goes to the renderer.
    pub fn video(&mut self, name: &str) -> VideoWriter {
        let w = self.slots.register(name);
        let r = self.slots.take_reader(name).unwrap();
        self.inputs.video.insert(name.to_string(), r);
        w
    }

    pub fn set(&mut self, addr: &str, v: impl Into<Value>) {
        self.state.insert(addr.to_string(), v.into());
    }

    pub fn unset(&mut self, addr: &str) {
        self.state.remove(addr);
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            index: Arc::new(self.state.keys().enumerate().map(|(i, k)| (k.clone(), i)).collect()),
            values: self.state.values().cloned().collect(),
            signal_names: Arc::new(self.signals.keys().cloned().collect()),
            signal_index: Arc::new(self.signals.keys().enumerate().map(|(i, k)| (k.clone(), i)).collect()),
            signals: self.signals.values().copied().collect(),
            ..Default::default()
        }
    }

    /// Forward asset requests to the loader and apply everything it sent, waiting up to 10 s
    /// for outstanding compiles/loads.
    pub fn settle(&mut self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut idle = 0;
        while std::time::Instant::now() < deadline {
            let mut any = false;
            while let Ok(a) = self.assets.try_recv() {
                self.loader.send(LoaderCmd::Asset(a)).unwrap();
                any = true;
            }
            if let Ok(m) = self.msgs.recv_timeout(Duration::from_millis(150)) {
                if self.fuse || !matches!(m, Msg::Fused { .. }) {
                    self.r.apply(m);
                }
                any = true;
            }
            if any {
                idle = 0;
            } else {
                idle += 1;
                if idle >= 2 {
                    return;
                }
            }
        }
    }

    pub fn frame_at(&mut self, now: u64) {
        let snap = self.snapshot();
        self.r.frame(&snap, now, &mut self.inputs);
    }

    pub fn frame(&mut self) {
        self.frame_at(T0 + 1_000_000_000);
    }

    /// Straight RGBA8 pixels of a canvas's final texture.
    pub fn read(&mut self, canvas: usize) -> (u32, u32, Vec<u8>) {
        let tex = self.r.final_texture(canvas).expect("canvas rendered").clone();
        read_texture(&self.r.gpu.device, &self.r.gpu.queue, &tex)
    }
}

pub fn read_texture(device: &wgpu::Device, queue: &wgpu::Queue, tex: &wgpu::Texture) -> (u32, u32, Vec<u8>) {
    let (w, h) = (tex.width(), tex.height());
    let row = (w * 4).div_ceil(256) * 256;
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (row * h) as u64,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut enc = device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(
        tex.as_image_copy(),
        wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) } },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    queue.submit([enc.finish()]);
    buf.slice(..).map_async(wgpu::MapMode::Read, |r| r.unwrap());
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    let view = buf.slice(..).get_mapped_range().unwrap();
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h as usize {
        out.extend_from_slice(&view[y * row as usize..y * row as usize + w as usize * 4]);
    }
    (w, h, out)
}

pub fn px(img: &(u32, u32, Vec<u8>), x: u32, y: u32) -> [u8; 4] {
    let i = ((y * img.0 + x) * 4) as usize;
    [img.2[i], img.2[i + 1], img.2[i + 2], img.2[i + 3]]
}

/// RGB test pattern: 8 vertical color bars over the top 2/3, a horizontal grey ramp below.
pub fn pattern_rgb(w: u32, h: u32) -> Vec<[u8; 3]> {
    const BARS: [[u8; 3]; 8] = [[235, 235, 235], [235, 235, 16], [16, 235, 235], [16, 235, 16], [235, 16, 235], [235, 16, 16], [16, 16, 235], [16, 16, 16]];
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            if y < h * 2 / 3 {
                out.push(BARS[(x * 8 / w) as usize]);
            } else {
                let v = (16 + x * 219 / (w - 1)) as u8;
                out.push([v, v, v]);
            }
        }
    }
    out
}

/// BT.709 limited-range YUYV of an RGB image.
pub fn to_yuyv_709_limited(rgb: &[[u8; 3]], w: u32, h: u32) -> Vec<u8> {
    let mut out = vec![0u8; (w * h * 2) as usize];
    let yuv = |c: [u8; 3]| {
        let (r, g, b) = (c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0);
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let cb = (b - y) / 1.8556;
        let cr = (r - y) / 1.5748;
        ((16.0 + 219.0 * y).round(), (128.0 + 224.0 * cb).round(), (128.0 + 224.0 * cr).round())
    };
    for y in 0..h {
        for x in (0..w).step_by(2) {
            let a = yuv(rgb[(y * w + x) as usize]);
            let b = yuv(rgb[(y * w + x + 1) as usize]);
            let o = ((y * w + x) * 2) as usize;
            out[o] = a.0 as u8;
            out[o + 1] = ((a.1 + b.1) / 2.0).round() as u8;
            out[o + 2] = b.0 as u8;
            out[o + 3] = ((a.2 + b.2) / 2.0).round() as u8;
        }
    }
    out
}

/// Straight-alpha RGBA test image: a diagonal gradient, a green (key color) block, a
/// half-transparent white bar.
pub fn pattern_rgba(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let px: [u8; 4] = if x > w / 2 && y > h / 2 {
                [0, 255, 0, 255]
            } else if y < h / 8 {
                [255, 255, 255, 128]
            } else {
                [(x * 255 / w) as u8, (y * 255 / h) as u8, 160, 255]
            };
            out.extend_from_slice(&px);
        }
    }
    out
}

pub fn publish_sources(h: &mut Harness) -> (VideoWriter, VideoWriter) {
    let mut a = h.video("cam_a");
    let mut b = h.video("cam_b");
    let (w, hh) = (320, 180);
    let yuyv = to_yuyv_709_limited(&pattern_rgb(w, hh), w, hh);
    a.write(w, hh, w * 2, PixelFormat::Yuyv, 1, &yuyv);
    b.write(160, 90, 160 * 4, PixelFormat::Rgba8, 1, &pattern_rgba(160, 90));
    h.set("source.cam_a.matrix", "bt709");
    h.set("source.cam_a.range", "limited");
    (a, b)
}

pub const PROJECT: &str = r##"
schema = 1
[canvas.wide]
width = 320
height = 180
fps = 60
[canvas.tall]
width = 180
height = 320
fps = 60
[safety]
video_flash_limit = false
[render]
no_signal = "#202028"
"##;

/// Compare against `tests/golden/<name>.png` (write it with `SE_UPDATE_GOLDEN=1`).
/// Tolerance: ≤ 4/255 per channel on 99.5 % of pixels, mean abs error < 0.6.
pub fn golden(name: &str, img: &(u32, u32, Vec<u8>)) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(format!("{name}.png"));
    if std::env::var_os("SE_UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        image::save_buffer(&path, &img.2, img.0, img.1, image::ExtendedColorType::Rgba8).unwrap();
        eprintln!("wrote golden {}", path.display());
        return;
    }
    let want =
        image::open(&path).unwrap_or_else(|e| panic!("{}: {e} (create goldens with SE_UPDATE_GOLDEN=1 after checking the output)", path.display())).to_rgba8();
    assert_eq!((want.width(), want.height()), (img.0, img.1), "{name}: size");
    let (mut bad, mut sum) = (0usize, 0u64);
    for (a, b) in want.as_raw().iter().zip(&img.2) {
        let d = a.abs_diff(*b);
        sum += d as u64;
        if d > 4 {
            bad += 1;
        }
    }
    let n = img.2.len();
    let mean = sum as f64 / n as f64;
    if bad as f64 > n as f64 * 0.005 || mean >= 0.6 {
        let out = std::env::temp_dir().join(format!("se-render-{name}-actual.png"));
        image::save_buffer(&out, &img.2, img.0, img.1, image::ExtendedColorType::Rgba8).unwrap();
        panic!("{name}: {bad} of {n} channels differ by > 4, mean {mean:.3} (actual written to {})", out.display());
    }
}
