//! Offline preview of shader/particles patches on a synthetic drum stage with a synthetic
//! 120 BPM groove (kick/snare/hat/level signals, a fill every fourth bar) and a patch trigger
//! every 4 s. Renders with the real renderer and loader on the GPU, no engine, no cameras.
//!
//! cargo run --release -p se-render --example fx_preview -- [--out DIR] [--seconds 8]
//!     [--size 1280x720] [--image backdrop.png | --video clip.mp4] [--baseline] <patch dir>...
//!
//! `--video` loops real footage (scaled/cropped to the canvas) under the patch instead of the
//! synthetic stage; the groove signals and triggers stay synthetic. `--trigger-every S`
//! changes the trigger interval (e.g. 0.4 to simulate chat messages, each from a different
//! viewer colour); `--talk` stops the band from 5 s to 8 s with the mic up.
//! Triggers follow the patch's manifest `trigger` timing (attack/hold/release, the speed it
//! has on the show), spaced so each burst finishes; `--seconds auto` renders two full bursts
//! (or 13 s for patches without a trigger).
//!
//! Writes `<id>.mp4`, `<id>.png` (a still while triggered), `<id>.txt` (label, description,
//! layer, GPU time) and refreshes `index.html` (every preview in the folder) in DIR
//! (default `target/fx-previews`). Effect patches are attached to the stage layer, source
//! patches are placed full-canvas above it, overlays draw on their own.

#[path = "../tests/common/mod.rs"]
mod common;

use common::*;
use se_core::triggers::TriggerPayload;
use se_hub::media::PixelFormat;
use se_proto::Value;
use se_render::plan::WIDE;
use se_render::renderer::Msg;
use std::f32::consts::TAU;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const FPS: u32 = 30;
const STAGE: (u32, u32) = (640, 360);
const BPM: f32 = 120.0;
/// Hit-envelope decay rate (1/s): the live analysis falls to 5 % in 150 ms
/// (`se-analysis` default `envelope_decay_ms`), about exp(-20 t).
const DECAY: f32 = 20.0;
/// Gap between preview triggers when the patch has no manifest trigger (accents only).
const TRIGGER_EVERY: f32 = 4.0;
const TRIGGER_FIRST: f32 = 1.0;
/// Envelope for patches without a manifest trigger (their accents use `se.trigger_age`).
const ACCENT_ENV: Env = Env { attack: 0.1, hold: 1.2, release: 0.8 };
/// Hold used for a manifest trigger without `hold` (held until released on the show).
const OPEN_HOLD: f32 = 3.0;

#[derive(Clone, Copy)]
struct Env {
    attack: f32,
    hold: f32,
    release: f32,
}

impl Env {
    fn len(&self) -> f32 {
        self.attack + self.hold + self.release
    }
}

struct Args {
    out: PathBuf,
    /// `None` = auto: two full plays of a burst, or 13 s (three accents) otherwise.
    seconds: Option<f32>,
    size: (u32, u32),
    image: Option<PathBuf>,
    video: Option<PathBuf>,
    baseline: bool,
    trigger_every: Option<f32>,
    talk: bool,
    patches: Vec<PathBuf>,
}

fn args() -> Args {
    let mut a = Args {
        out: Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/fx-previews"),
        seconds: Some(8.0),
        size: (1280, 720),
        image: None,
        video: None,
        baseline: false,
        trigger_every: None,
        talk: false,
        patches: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| panic!("{arg} needs a value"));
        match arg.as_str() {
            "--out" => a.out = val().into(),
            "--seconds" => {
                let v = val();
                a.seconds = if v == "auto" { None } else { Some(v.parse().expect("--seconds")) };
            }
            "--size" => {
                let v = val();
                let (w, h) = v.split_once('x').expect("--size WxH");
                a.size = (w.parse().expect("width"), h.parse().expect("height"));
            }
            "--image" => a.image = Some(val().into()),
            "--video" => a.video = Some(val().into()),
            "--baseline" => a.baseline = true,
            "--trigger-every" => a.trigger_every = Some(val().parse().expect("--trigger-every")),
            "--talk" => a.talk = true,
            _ => a.patches.push(arg.into()),
        }
    }
    a
}

/// The synthetic groove: 4/4 at 120 BPM, kicks on 1, 3 and the "and" of 3, snares on 2 and 4,
/// eighth-note hats, a crash on every bar's downbeat, and a sixteenth-note snare fill over
/// beats 3-4 of every fourth bar. Values are decaying hit envelopes, like the analysis signals.
/// Chat speeds up over the clip (`twitch.chat_rate`, messages per minute, 10 → 120). With
/// `talk`, the band stops from 5 s to 8 s while the mic is up (talking between songs).
struct Groove {
    t: f32,
    len: f32,
    talk: bool,
}

impl Groove {
    fn beat(&self) -> f32 {
        self.t * BPM / 60.0
    }
    fn since(&self, hits: &dyn Fn(f32) -> bool, step: f32) -> f32 {
        // seconds since the most recent hit on a `step`-beat grid
        let b = self.beat();
        let mut k = (b / step).floor();
        for _ in 0..64 {
            if k < 0.0 {
                break;
            }
            if hits(k * step) {
                return (b - k * step) * 60.0 / BPM;
            }
            k -= 1.0;
        }
        99.0
    }
    fn fill(b: f32) -> bool {
        (b / 4.0).floor() as i64 % 4 == 3 && b % 4.0 >= 2.0
    }
    fn kick(&self) -> f32 {
        let s = self.since(&|b| matches!(((b % 4.0) * 2.0).round() as i32, 0 | 4 | 5) && !Self::fill(b), 0.5);
        (-s * DECAY).exp()
    }
    fn snare(&self) -> f32 {
        let s = self.since(&|b| Self::fill(b) || matches!((b % 4.0).round() as i32, 1 | 3) && (b % 1.0) == 0.0, 0.25);
        (-s * DECAY).exp()
    }
    fn hat(&self) -> f32 {
        (-self.since(&|_| true, 0.5) * DECAY).exp()
    }
    fn crash(&self) -> f32 {
        (-self.since(&|b| b % 4.0 == 0.0, 4.0) * 2.5).exp()
    }
    fn phrase(&self) -> f32 {
        (self.beat() / 16.0).fract()
    }
    fn talking(&self) -> bool {
        self.talk && (5.0..8.0).contains(&self.t)
    }
    fn signals(&self, out: &mut std::collections::BTreeMap<String, f32>) {
        let quiet = if self.talking() { 0.0 } else { 1.0 };
        let (k, s, h, c) = (self.kick() * quiet, self.snare() * quiet, self.hat() * quiet, self.crash() * quiet);
        let b = self.beat();
        let swell = self.phrase();
        let level = ((0.45 + 0.25 * k + 0.12 * s + 0.2 * swell) * quiet).clamp(0.04, 1.0);
        let bass = ((0.25 + 0.65 * k) * quiet).clamp(0.03, 1.0);
        let mid = ((0.3 + 0.5 * s) * quiet).clamp(0.05, 1.0);
        let high = ((0.2 + 0.4 * h + 0.4 * c) * quiet).clamp(0.03, 1.0);
        let mic = if self.talking() { 0.35 + 0.3 * (self.t * 7.0).sin().abs() } else { 0.02 };
        let chat = 10.0 + 110.0 * (self.t / self.len.max(0.1)).min(1.0);
        let rnd = {
            let n = (b.floor() as u32).wrapping_mul(2_654_435_761);
            (n >> 8) as f32 / (1u32 << 24) as f32
        };
        for (name, v) in [
            ("band.level", level),
            ("band.bass", bass),
            ("band.mid", mid),
            ("band.high", high),
            ("band.kick", k),
            ("band.snare", s),
            ("band.hat", h.max(c)),
            ("band.centroid", 0.35 + 0.3 * h + 0.2 * c),
            ("music.level", level),
            ("music.bass", bass),
            ("music.mid", mid),
            ("music.high", high),
            ("beat.phase", b.fract()),
            ("beat.bpm", BPM),
            ("mic.level", mic),
            ("twitch.chat_rate", chat),
            ("twitch.viewers", 42.0),
            ("lfo.slow", 0.5 + 0.5 * (self.t * TAU / 8.0).sin()),
            ("lfo.mid", 0.5 + 0.5 * (self.t * TAU / 2.0).sin()),
            ("lfo.fast", 0.5 + 0.5 * (self.t * TAU * 2.0).sin()),
            ("lfo.beat", 1.0 - b.fract()),
            ("lfo.bar", (b / 4.0).fract()),
            ("lfo.random", rnd),
        ] {
            out.insert(name.to_string(), v);
        }
    }
}

/// Trigger envelope at `t` (fires every `every` seconds from TRIGGER_FIRST).
fn envelope(t: f32, every: f32, e: Env) -> (f32, Option<u32>) {
    if t < TRIGGER_FIRST {
        return (0.0, None);
    }
    let k = ((t - TRIGGER_FIRST) / every).floor();
    let s = t - TRIGGER_FIRST - k * every;
    let env = if s < e.attack {
        s / e.attack.max(1e-3)
    } else if s < e.attack + e.hold {
        1.0
    } else {
        (1.0 - (s - e.attack - e.hold) / e.release.max(1e-3)).max(0.0)
    };
    (env, Some(k as u32))
}

/// `"350ms"`, `"2.5s"`, `"1m"` or a number of milliseconds, in seconds.
fn seconds(v: &toml::Value) -> Option<f32> {
    if let Some(n) = v.as_integer() {
        return Some(n as f32 / 1000.0);
    }
    if let Some(n) = v.as_float() {
        return Some(n as f32 / 1000.0);
    }
    let s = v.as_str()?.trim();
    let (num, scale) = if let Some(n) = s.strip_suffix("ms") {
        (n, 0.001)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60.0)
    } else {
        (s, 0.001)
    };
    num.trim().parse::<f32>().ok().map(|n| n * scale)
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

/// A stylised drum stage (dark room, two spotlights, kit, gold cymbals that flare on hits, a
/// stick swinging with the snare, small bright rig lights) so glow/glint/trail effects have
/// highlights and motion to work with.
fn stage(g: &Groove, buf: &mut [u8]) {
    let (w, h) = STAGE;
    let asp = w as f32 / h as f32;
    let (k, s, hat, crash) = (g.kick(), g.snare(), g.hat(), g.crash());
    let swing = -0.6 + 0.9 * (1.0 - s);
    let stick_a = [0.36 * asp, 0.47];
    let stick_b = [stick_a[0] + 0.16 * swing.cos(), stick_a[1] - 0.16 * swing.sin()];
    let drums: [([f32; 2], f32, [f32; 3], f32); 5] = [
        ([0.50, 0.70], 0.17, [0.55, 0.08, 0.10], k),
        ([0.41, 0.50], 0.07, [0.55, 0.08, 0.10], 0.0),
        ([0.58, 0.49], 0.075, [0.55, 0.08, 0.10], 0.0),
        ([0.70, 0.64], 0.095, [0.55, 0.08, 0.10], 0.0),
        ([0.33, 0.61], 0.07, [0.75, 0.75, 0.78], s),
    ];
    let cymbals: [([f32; 2], f32, f32); 3] = [([0.25, 0.36], 0.11, crash), ([0.78, 0.33], 0.12, 0.6 * crash), ([0.21, 0.53], 0.07, hat)];
    for y in 0..h {
        for x in 0..w {
            let (u, v) = ((x as f32 + 0.5) / h as f32, (y as f32 + 0.5) / h as f32);
            let ux = u / asp;
            let mut c = mix([0.05, 0.05, 0.11], [0.12, 0.07, 0.14], v);
            for (cx, col, spread) in [(0.3, [1.0, 0.75, 0.45], 0.22), (0.72, [0.45, 0.6, 1.0], 0.2)] {
                let dx = (ux - cx).abs() - v * spread * 0.5;
                let cone = (1.0 - dx.max(0.0) / 0.08).clamp(0.0, 1.0) * (0.15 + 0.15 * v);
                c = [c[0] + col[0] * cone, c[1] + col[1] * cone, c[2] + col[2] * cone];
            }
            if v > 0.78 {
                c = mix(c, [0.06, 0.04, 0.05], 0.6);
            }
            for i in 0..6 {
                let lx = (0.12 + i as f32 * 0.152) * asp;
                let d = ((u - lx).powi(2) + (v - 0.06).powi(2)).sqrt();
                let glow = (1.0 - d / 0.012).clamp(0.0, 1.0);
                let on = if i % 2 == 0 { 0.6 + 0.4 * k } else { 0.6 + 0.4 * s };
                c = mix(c, [1.0, 0.97, 0.9], glow * on);
            }
            for (p, r, col, hit) in drums {
                let d = ((u - p[0] * asp).powi(2) + (v - p[1]).powi(2)).sqrt() / r;
                if d < 1.0 {
                    let head = mix([0.82, 0.78, 0.7], [1.0, 0.98, 0.92], hit);
                    c = if d > 0.82 { col } else { mix(head, col, (d / 0.82).powi(6) * 0.4) };
                }
            }
            for (p, r, hit) in cymbals {
                let (dx, dy) = ((u - p[0] * asp) / r, (v - p[1]) / (r * 0.28));
                let d = (dx * dx + dy * dy).sqrt();
                if d < 1.0 {
                    let spec = (1.0 - ((dx - 0.25).powi(2) + (dy + 0.2).powi(2)).sqrt() / 0.35).clamp(0.0, 1.0);
                    c = mix([0.72, 0.55, 0.16], [1.0, 0.95, 0.75], (spec * (0.5 + 0.5 * hit) + 0.35 * hit).min(1.0));
                }
            }
            {
                let (ax, ay, bx, by) = (stick_a[0], stick_a[1], stick_b[0], stick_b[1]);
                let (px, py, dx, dy) = (u - ax, v - ay, bx - ax, by - ay);
                let t = ((px * dx + py * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
                let d = ((px - dx * t).powi(2) + (py - dy * t).powi(2)).sqrt();
                if d < 0.006 {
                    c = [0.95, 0.88, 0.7];
                }
            }
            let i = ((y * w + x) * 4) as usize;
            buf[i] = (c[0].clamp(0.0, 1.0) * 255.0) as u8;
            buf[i + 1] = (c[1].clamp(0.0, 1.0) * 255.0) as u8;
            buf[i + 2] = (c[2].clamp(0.0, 1.0) * 255.0) as u8;
            buf[i + 3] = 255;
        }
    }
}

struct Patch {
    id: String,
    label: String,
    description: String,
    layer: String,
    kind: String,
    /// The manifest trigger's envelope (attack/hold/release), if it declares one.
    trigger: Option<Env>,
}

fn read_patch(dir: &Path) -> Patch {
    let id = dir.file_name().expect("patch dir name").to_string_lossy().to_string();
    let text = std::fs::read_to_string(dir.join("patch.toml")).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    let t: toml::Table = toml::from_str(&text).unwrap_or_else(|e| panic!("{}/patch.toml: {e}", dir.display()));
    let s = |k: &str, d: &str| t.get(k).and_then(|v| v.as_str()).unwrap_or(d).to_string();
    let trigger = t.get("trigger").and_then(|v| v.as_table()).map(|tr| {
        let get = |k: &str| tr.get(k).and_then(seconds);
        Env { attack: get("attack").unwrap_or(0.0), hold: get("hold").unwrap_or(OPEN_HOLD), release: get("release").unwrap_or(0.0) }
    });
    Patch { label: s("label", &id), description: s("description", ""), layer: s("layer", "source"), kind: s("kind", "shader"), trigger, id }
}

fn project(size: (u32, u32)) -> String {
    format!(
        "schema = 1\n[canvas.wide]\nwidth = {}\nheight = {}\nfps = {FPS}\n[canvas.tall]\nwidth = 180\nheight = 320\nfps = {FPS}\n[safety]\nvideo_flash_limit = false\n[render]\nno_signal = \"#202028\"\n",
        size.0, size.1
    )
}

fn render(a: &Args, p: Option<&Patch>, src: Option<&Path>, backdrop: &Option<(u32, u32, Vec<u8>)>) -> String {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "project.toml", &project(a.size));
    let id = p.map_or("_baseline".to_string(), |p| p.id.clone());
    let mut nodes = String::from("{ id = \"stage\", src = \"stage\", rect = [0, 0, 1, 1], z = 0");
    if let Some(p) = p {
        copy_dir(src.unwrap(), &root.join("patches").join(&p.id));
        match p.layer.as_str() {
            // a triggered effect runs canvas-wide on its own while its envelope is up (like
            // firing it on the show); the renderer only plans patch effects that something
            // references, so it is referenced by a bypassed slot. An always-on effect is
            // attached to the stage layer.
            "effect" if p.trigger.is_some() => nodes += &format!(", fx = [{{ id = \"ref\", name = \"patch.{}\", enabled = false }}] }}", p.id),
            "effect" => nodes += &format!(", fx = [{{ id = \"preview\", name = \"patch.{}\" }}] }}", p.id),
            "source" => nodes += &format!(" }}, {{ id = \"patch\", src = \"patch.{}\", rect = [0, 0, 1, 1], z = 10 }}", p.id),
            "overlay" => nodes += " }",
            other => panic!("{}: layer `{other}` has no preview (effect, source and overlay do)", p.id),
        }
    } else {
        nodes += " }";
    }
    write_file(root, "scenes/stage.toml", &format!("[canvas.wide]\nnodes = [{nodes}]\n[canvas.tall]\nnodes = [{{ src = \"color:#000000\" }}]\n"));
    let mut h = Harness::new(root);
    let errors: Vec<String> = h.reports.lock().patches.iter().filter_map(|(id, r)| r.as_ref().err().map(|e| format!("{id}: {e}"))).collect();
    if !errors.is_empty() {
        panic!("patch failed to load:\n{}", errors.join("\n"));
    }
    h.set("show.scene.program", "stage");
    let mut cam = h.video("stage");
    let (w, hh) = a.size;
    let mp4 = a.out.join(format!("{id}.mp4"));
    let mut ff = Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgba", "-s", &format!("{w}x{hh}"), "-r", &FPS.to_string(), "-i", "-"])
        .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p", "-movflags", "+faststart"])
        .arg(&mp4)
        .stdin(Stdio::piped())
        .spawn()
        .expect("ffmpeg");
    let mut stdin = ff.stdin.take().unwrap();
    let mut buf = vec![0u8; (STAGE.0 * STAGE.1 * 4) as usize];
    let mut clip = a.video.as_ref().map(|path| {
        let vf = format!("scale={w}:{hh}:force_original_aspect_ratio=increase,crop={w}:{hh},fps={FPS}");
        let mut dec = Command::new("ffmpeg")
            .args(["-loglevel", "error", "-stream_loop", "-1", "-i"])
            .arg(path)
            .args(["-an", "-vf", &vf, "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("ffmpeg decoder");
        let out = dec.stdout.take().unwrap();
        (dec, out, vec![0u8; (w * hh * 4) as usize])
    });
    // a burst plays at its own manifest timing, with a breather before the next one
    let env_shape = p.and_then(|p| p.trigger).unwrap_or(ACCENT_ENV);
    let every = a.trigger_every.unwrap_or(match p.and_then(|p| p.trigger) {
        Some(e) => (e.len() + 1.5).max(TRIGGER_EVERY),
        None => TRIGGER_EVERY,
    });
    let seconds = a.seconds.unwrap_or(match p.and_then(|p| p.trigger) {
        Some(e) => TRIGGER_FIRST + every + e.len() + 1.0,
        None => 13.0,
    });
    let frames = (seconds * FPS as f32).round() as u64;
    let still = ((TRIGGER_FIRST + env_shape.attack + env_shape.hold * 0.5) * FPS as f32) as u64;
    let mut last_trigger = None;
    let (mut gpu_sum, mut gpu_n, mut gpu_max) = (0f32, 0u32, 0f32);
    for f in 0..frames {
        let t = f as f32 / FPS as f32;
        let now = T0 + f * 1_000_000_000 / FPS as u64;
        let g = Groove { t, len: seconds, talk: a.talk };
        match (&mut clip, backdrop) {
            (Some((_, out, vbuf)), _) => {
                std::io::Read::read_exact(out, vbuf).expect("read footage frame");
                cam.write(w, hh, w * 4, PixelFormat::Rgba8, now, vbuf);
            }
            (None, Some((bw, bh, px))) => cam.write(*bw, *bh, bw * 4, PixelFormat::Rgba8, now, px),
            (None, None) => {
                stage(&g, &mut buf);
                cam.write(STAGE.0, STAGE.1, STAGE.0 * 4, PixelFormat::Rgba8, now, &buf);
            }
        }
        g.signals(&mut h.signals);
        if let Some(p) = p {
            let (env, k) = envelope(t, every, env_shape);
            if k.is_some() && k != last_trigger {
                last_trigger = k;
                let n = k.unwrap();
                let colors = ["#ff4fa3", "#4fc3ff", "#ffd84f", "#7dff6a", "#b77dff", "#ff8a3d"];
                let user = format!("viewer{}", n % 7);
                let payload = Value::map().with("amount", 100 * (n % 8 + 1) as i64).with("tier", (n % 3 + 1) as i64).with("count", (n + 1) as i64).with("color", colors[n as usize % colors.len()]);
                let actor = se_proto::Actor { platform: "twitch".into(), id: user.clone(), name: user, ..Default::default() };
                h.r.apply(Msg::PatchTrigger { patch: p.id.clone(), payload: TriggerPayload::from_event(&payload, Some(&actor)).floats() });
            }
            h.set(&format!("patch.{}.env", p.id), Value::Float(env as f64));
            h.set(&format!("patch.{}.active", p.id), Value::Bool(env > 0.0));
        }
        while let Ok(req) = h.assets.try_recv() {
            h.loader.send(se_render::loader::LoaderCmd::Asset(req)).unwrap();
        }
        while let Ok(m) = h.msgs.try_recv() {
            h.r.apply(m);
        }
        h.frame_at(now);
        let img = h.read(WIDE);
        stdin.write_all(&img.2).expect("ffmpeg stdin");
        if f == still {
            image::save_buffer(a.out.join(format!("{id}.png")), &img.2, img.0, img.1, image::ExtendedColorType::Rgba8).unwrap();
        }
        let gpu = h.stats.view().gpu_ms;
        if f > FPS as u64 && gpu > 0.0 {
            gpu_sum += gpu;
            gpu_n += 1;
            gpu_max = gpu_max.max(gpu);
        }
    }
    drop(stdin);
    if let Some((mut dec, out, _)) = clip {
        drop(out);
        let _ = dec.kill();
        let _ = dec.wait();
    }
    assert!(ff.wait().expect("ffmpeg").success(), "ffmpeg failed");
    let gpu = format!("GPU {:.2} ms avg, {:.2} ms peak per frame at {w}x{hh} (whole frame)", gpu_sum / gpu_n.max(1) as f32, gpu_max);
    let (label, desc, layer) = p.map_or(("Baseline (no effect)".to_string(), "The backdrop alone".to_string(), "-".to_string()), |p| (p.label.clone(), p.description.clone(), format!("{} {}", p.kind, p.layer)));
    std::fs::write(a.out.join(format!("{id}.txt")), format!("{label}\n{desc}\n{layer}\n{gpu}\n")).unwrap();
    format!("{id}: {gpu} -> {}", mp4.display())
}

fn gallery(out: &Path) {
    let mut cards: Vec<(String, Vec<String>)> = std::fs::read_dir(out)
        .unwrap()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            (p.extension()? == "txt").then(|| (p.file_stem().unwrap().to_string_lossy().to_string(), std::fs::read_to_string(&p).unwrap_or_default().lines().map(String::from).collect()))
        })
        .filter(|(id, _)| out.join(format!("{id}.mp4")).exists())
        .collect();
    cards.sort();
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let mut html = String::from(
        "<!doctype html><meta charset=utf-8><title>FX previews</title><style>body{background:#111;color:#ddd;font:14px sans-serif;margin:20px}main{display:grid;grid-template-columns:repeat(auto-fill,minmax(480px,1fr));gap:18px}figure{margin:0;background:#1b1b1f;padding:10px;border-radius:8px}video{width:100%;border-radius:4px}b{font-size:16px;color:#fff}small{color:#888;display:block}</style><h1>FX previews</h1><p>Simulated 120 BPM groove with a fill every 4th bar; trigger every 4 s from 1 s.</p><main>",
    );
    for (id, l) in cards {
        let g = |i: usize| l.get(i).map(|s| esc(s)).unwrap_or_default();
        html += &format!("<figure><video src=\"{id}.mp4\" autoplay loop muted playsinline></video><figcaption><b>{}</b> <code>{id}</code><br>{}<small>{} · {}</small></figcaption></figure>", g(0), g(1), g(2), g(3));
    }
    html += "</main>";
    std::fs::write(out.join("index.html"), html).unwrap();
}

fn main() {
    let a = args();
    std::fs::create_dir_all(&a.out).unwrap();
    let backdrop = a.image.as_ref().map(|p| {
        let img = image::open(p).unwrap_or_else(|e| panic!("{}: {e}", p.display())).to_rgba8();
        (img.width(), img.height(), img.into_raw())
    });
    if a.baseline {
        println!("{}", render(&a, None, None, &backdrop));
    }
    for dir in &a.patches {
        let p = read_patch(dir);
        println!("{}", render(&a, Some(&p), Some(dir), &backdrop));
    }
    gallery(&a.out);
    println!("gallery: {}", a.out.join("index.html").display());
}
