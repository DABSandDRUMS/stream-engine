//! Headless screenshots of every page and tab, rendered with the real UI code against a running
//! engine (the default socket, or `--socket <path>`). For design review without touching the
//! desktop:
//!
//!     cargo run -p se-ui --example screens -- [--size 1720x1080] [--out ~/.cache/stream-engine/screens] [--only live]
//!
//! Video panes show the scene layout (headless has no dmabuf import); everything else is live
//! engine state.

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use se_ui::app::{App, Page};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn pump(h: &mut Harness<'_, App>, ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        h.step();
        std::thread::sleep(Duration::from_millis(16));
    }
}

fn main() {
    let (w, hgt) = arg("--size")
        .and_then(|s| {
            let (a, b) = s.split_once('x')?;
            Some((a.parse::<f32>().ok()?, b.parse::<f32>().ok()?))
        })
        .unwrap_or((1720.0, 1080.0));
    // on disk, not in /tmp (RAM with a per-user quota)
    let cache = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache"));
    let out = arg("--out").map(PathBuf::from).unwrap_or_else(|| cache.join("stream-engine/screens"));
    let only = arg("--only");
    std::fs::create_dir_all(&out).expect("create output dir");
    let opts = se_ui::UiOpts { socket: arg("--socket").map(PathBuf::from), layout: Some("single".into()), program_only: false };
    // No video in headless renders: kittest's device can't import dmabufs, and CPU-copied
    // frames would pile up in wgpu's staging memory between renders.
    // SAFETY: single-threaded at this point; nothing else reads the environment yet.
    unsafe { std::env::set_var("SE_FRAMES_SOCKET", "/nonexistent/se-screens-no-video") };
    let mut h = Harness::builder().with_size([w, hgt]).with_pixels_per_point(1.0).with_max_steps(3).wgpu().build_eframe(|cc| App::new(cc, opts));
    let t0 = Instant::now();
    while !h.state().m.connected && t0.elapsed() < Duration::from_secs(8) {
        pump(&mut h, 100);
    }
    if !h.state().m.connected {
        eprintln!("warning: engine not connected; screenshots show the offline state");
    }
    pump(&mut h, 2500);
    let mut shots = 0;
    for page in Page::ALL {
        let tabs = page.tabs();
        let n = tabs.len().max(1);
        for i in 0..n {
            let name = if tabs.len() > 1 {
                format!("{}-{}", page.id(), tabs[i].1.to_lowercase().replace([' ', '&'], "-").replace("--", "-"))
            } else {
                page.id().to_string()
            };
            if only.as_deref().is_some_and(|o| !name.starts_with(o)) {
                continue;
            }
            {
                let app = h.state_mut();
                app.page = page;
                app.tab[page as usize] = i;
            }
            pump(&mut h, 1200);
            if page == Page::Live {
                // one shot per lower-rail tab, with chat pinned above it
                for (tab, label, _) in se_ui::views::show::RailTab::ALL {
                    let name = format!("live-rail-{}", label.to_lowercase());
                    if only.as_deref().is_some_and(|o| !name.starts_with(o) && !"live".starts_with(o)) {
                        continue;
                    }
                    h.state_mut().show.rail = tab;
                    pump(&mut h, 800);
                    shots += save(&mut h, &out, &name);
                }
                h.state_mut().show.rail = se_ui::views::show::RailTab::Queue;
                pump(&mut h, 300);
            }
            shots += save(&mut h, &out, &name);
            if page == Page::Scenes && i == 1 {
                h.get_by_label("Target racks / saved chains").click();
                pump(&mut h, 300);
                shots += save(&mut h, &out, &format!("{name}-racks"));
                h.get_by_label("Targets — select a rack or check several for a preset").click();
                pump(&mut h, 300);
                shots += save(&mut h, &out, &format!("{name}-racks-controls"));
                h.get_by_label("Targets — select a rack or check several for a preset").click();
                pump(&mut h, 300);
                h.get_by_label("Effect library / defaults").click();
                pump(&mut h, 300);
            }
            if page == Page::Recordings {
                let pick = h.state().m.q_list("sessions").iter().find_map(|session| {
                    let id = session.get_path("id").and_then(se_proto::Value::as_str)?;
                    let summary = h.state().m.q(&format!("clips.session:{id}"))?;
                    let clips = summary.get_path("clips").and_then(se_proto::Value::as_list)?;
                    let clip = clips.first()?.get_path("id")?.as_i64()?;
                    Some((id.to_owned(), clip))
                });
                if let Some((session, clip)) = pick {
                    h.get_by_label(&format!("Open recording {session}")).click();
                    pump(&mut h, 2000);
                    shots += save(&mut h, &out, &format!("{name}-clips"));
                    h.get_by_label(&format!("Open clip {clip}")).hover();
                    pump(&mut h, 1800);
                    shots += save(&mut h, &out, &format!("{name}-hover-a"));
                    pump(&mut h, 700);
                    shots += save(&mut h, &out, &format!("{name}-hover-b"));
                    h.get_by_label(&format!("Open clip {clip}")).click();
                    pump(&mut h, 1600);
                    shots += save(&mut h, &out, &format!("{name}-detail"));
                }
            }
            if page == Page::Scenes && i == 0 {
                // the layer panel with a layer picked, and the scene settings
                let first = {
                    let app = h.state();
                    let scene = app.m.str("show.scene.preview").to_string();
                    se_ui::views::canvas::editor_nodes(app, &scene, "wide").first().map(|n| n.id.clone())
                };
                h.state_mut().build.canvas.selected = first;
                pump(&mut h, 800);
                shots += save(&mut h, &out, &format!("{name}-layer"));
                h.state_mut().build.canvas.selected = None;
                pump(&mut h, 1200);
                shots += save(&mut h, &out, &format!("{name}-scene"));
            }
        }
    }
    println!("{shots} screenshots in {}", out.display());
}

fn save(h: &mut Harness<'_, App>, out: &std::path::Path, name: &str) -> usize {
    match h.render() {
        Ok(img) => {
            let path = out.join(format!("{name}.png"));
            img.save(&path).expect("save png");
            println!("{}", path.display());
            1
        }
        Err(e) => {
            eprintln!("{name}: render failed: {e}");
            0
        }
    }
}
