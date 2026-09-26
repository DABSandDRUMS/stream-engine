//! Render-time benchmark: a 1920×1080 draw list of ~200 mixed ops (rects, rounded and
//! stroked rects, circles, lines, paths, text, images, push/pop groups).
//!
//! `cargo run --release -p se-vector --example bench` (GPU picked like the tests: `SE_GPU`,
//! default "NVIDIA").

#[path = "../tests/support/mod.rs"]
mod support;

use std::time::{Duration, Instant};

use se_hub::draw::{DrawList, DrawOp, Paint, PathCmd, TextAlign};
use se_vector::{VectorOptions, VectorRenderer};

const FRAMES: usize = 300;

/// Deterministic pseudo-random numbers in 0..1.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
    fn color(&mut self) -> [f32; 4] {
        [self.next(), self.next(), self.next(), 0.5 + self.next() * 0.5]
    }
}

fn mixed_ops(with_text: bool) -> Vec<DrawOp> {
    let mut rng = Lcg(7);
    let mut ops = vec![DrawOp::Clear([0.05, 0.05, 0.08, 0.6])];
    for group in 0..10 {
        ops.push(DrawOp::Push {
            translate: [rng.next() * 0.2, rng.next() * 0.2],
            rotate: rng.next() * 0.5 - 0.25,
            scale: [0.8 + rng.next() * 0.4; 2],
            alpha: 0.6 + rng.next() * 0.4,
        });
        for _ in 0..3 {
            ops.push(DrawOp::Rect {
                xywh: [rng.next() * 0.8, rng.next() * 0.8, rng.next() * 0.2, rng.next() * 0.2],
                radius: 0.0,
                color: rng.color(),
                paint: Paint::Fill,
            });
        }
        ops.push(DrawOp::Rect { xywh: [rng.next() * 0.8, rng.next() * 0.8, 0.15, 0.1], radius: 0.02, color: rng.color(), paint: Paint::Fill });
        ops.push(DrawOp::Rect { xywh: [rng.next() * 0.8, rng.next() * 0.8, 0.15, 0.1], radius: 0.01, color: rng.color(), paint: Paint::Stroke(0.004) });
        for _ in 0..5 {
            ops.push(DrawOp::Circle { center: [rng.next(), rng.next()], radius: 0.01 + rng.next() * 0.05, color: rng.color(), paint: Paint::Fill });
        }
        for _ in 0..3 {
            ops.push(DrawOp::Line { a: [rng.next(), rng.next()], b: [rng.next(), rng.next()], width: 0.002 + rng.next() * 0.006, color: rng.color() });
        }
        ops.push(DrawOp::Path {
            cmds: vec![
                PathCmd::MoveTo([rng.next(), rng.next()]),
                PathCmd::LineTo([rng.next(), rng.next()]),
                PathCmd::LineTo([rng.next(), rng.next()]),
                PathCmd::Close,
            ],
            color: rng.color(),
            paint: Paint::Fill,
        });
        ops.push(DrawOp::Path {
            cmds: vec![
                PathCmd::MoveTo([rng.next(), rng.next()]),
                PathCmd::QuadTo([rng.next(), rng.next()], [rng.next(), rng.next()]),
                PathCmd::CubicTo([rng.next(), rng.next()], [rng.next(), rng.next()], [rng.next(), rng.next()]),
            ],
            color: rng.color(),
            paint: Paint::Stroke(0.003),
        });
        for i in 0..2 {
            let (pos, size, color) = ([rng.next() * 0.8, 0.1 + rng.next() * 0.8], 0.03 + rng.next() * 0.04, rng.color());
            if with_text {
                let text = format!("Group {group} · label {i} — 1234");
                ops.push(DrawOp::Text { pos, size, color, text, align: [TextAlign::Left, TextAlign::Center, TextAlign::Right][(group + i) % 3] });
            } else {
                ops.push(DrawOp::Rect { xywh: [pos[0], pos[1], 0.2, size], radius: 0.0, color, paint: Paint::Fill });
            }
        }
        ops.push(DrawOp::Image { path: format!("img{}.png", group % 3), xywh: [rng.next() * 0.8, rng.next() * 0.8, 0.12, 0.2], opacity: 0.9 });
        ops.push(DrawOp::Pop);
    }
    ops
}

fn stats(label: &str, mut v: Vec<Duration>) {
    v.sort();
    let mean = v.iter().sum::<Duration>() / v.len() as u32;
    let pct = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
    println!("{label}: mean {mean:?}  p50 {:?}  p99 {:?}  max {:?}", pct(0.5), pct(0.99), v[v.len() - 1]);
}

fn measure(renderer: &mut VectorRenderer, gpu: &support::Gpu, target: &support::Target, list: &DrawList) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        renderer.render(&gpu.device, &gpu.queue, list, &target.view, target.size)?;
        gpu.device.poll(wgpu::PollType::wait_indefinitely())?;
        if renderer.images_pending() == 0 {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "images did not load");
        std::thread::sleep(Duration::from_millis(2));
    }
    for _ in 0..30 {
        renderer.render(&gpu.device, &gpu.queue, list, &target.view, target.size)?;
    }
    gpu.device.poll(wgpu::PollType::wait_indefinitely())?;

    let (mut cpu, mut total) = (Vec::with_capacity(FRAMES), Vec::with_capacity(FRAMES));
    for _ in 0..FRAMES {
        let t0 = Instant::now();
        renderer.render(&gpu.device, &gpu.queue, list, &target.view, target.size)?;
        let t1 = Instant::now();
        gpu.device.poll(wgpu::PollType::wait_indefinitely())?;
        cpu.push(t1 - t0);
        total.push(t0.elapsed());
    }
    stats("  render() call (encode + submit)", cpu);
    stats("  render() + GPU completion      ", total);
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let gpu = support::gpu();
    let assets = tempfile::tempdir()?;
    for i in 0..3u8 {
        image::RgbaImage::from_fn(256, 256, |x, y| image::Rgba([x as u8, y as u8, i * 100, 255])).save(assets.path().join(format!("img{i}.png")))?;
    }
    let t = Instant::now();
    let mut renderer = VectorRenderer::new(&gpu.device, VectorOptions { assets_root: assets.path().to_path_buf(), font_family: None })?;
    println!("adapter: {}  |  VectorRenderer::new: {:?}", gpu.name, t.elapsed());

    let target = support::Target::new(gpu, 1920, 1080);
    for (label, with_text) in [("mixed", true), ("mixed, text ops replaced by rects", false)] {
        let list = DrawList { ops: mixed_ops(with_text), seq: 1 };
        println!("{label}: {} ops, {}×{}, {FRAMES} frames", list.ops.len(), target.size[0], target.size[1]);
        measure(&mut renderer, gpu, &target, &list)?;
    }
    Ok(())
}
