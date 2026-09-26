//! Capture from the real cameras on the show machine and read the video slots like the
//! renderer does. Run with `cargo test -p se-video-in --test hardware -- --ignored --nocapture`.

use se_hub::Hub;
use se_hub::media::PixelFormat;
use se_video_in::{camera, config, status::Status};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

struct Cam {
    name: &'static str,
    toml: &'static str,
    fps: f64,
    format: PixelFormat,
    size: (u32, u32),
}

const CAMS: &[Cam] = &[
    Cam {
        name: "cam_kit",
        toml: "device = \"pci-0000:05:00.0-video-index0\"\nformat = \"yuyv\"\nsize = [1920, 1080]\nfps = 60",
        fps: 60.0,
        format: PixelFormat::Yuyv,
        size: (1920, 1080),
    },
    Cam {
        name: "cam_kick",
        toml: "device = \"pci-0000:05:00.0-video-index3\"\nformat = \"yuyv\"\nsize = [1920, 1080]\nfps = 60",
        fps: 60.0,
        format: PixelFormat::Yuyv,
        size: (1920, 1080),
    },
    Cam {
        name: "cam_room",
        toml: "device = \"usb-Sonix_Technology_Co.__Ltd._USB_Camera_*-video-index0\"\nformat = \"mjpeg\"\nsize = [1920, 1080]\nfps = 30\n[controls]\nexposure_dynamic_framerate = false",
        fps: 30.0,
        format: PixelFormat::Rgba8,
        size: (1920, 1080),
    },
];

#[test]
#[ignore = "needs the AVMatrix VC42 (inputs 1 and 4 live) and the Sonix USB camera"]
fn cameras_capture_into_slots_at_native_rate() {
    let (hub, core_rx) = Hub::new(Arc::new(se_clock::Clock::new()));
    // The engine applies `[controls]` through the core; without a core here, hold the Sonix at
    // its nominal rate directly (auto exposure may otherwise stretch frames in low light).
    let room = se_devices::find_camera("usb-Sonix_Technology_Co.__Ltd._USB_Camera_*-video-index0").expect("Sonix camera present");
    let dev = se_devices::v4l2::Device::open(&room.path, true).unwrap();
    let c = dev.controls().unwrap().into_iter().find(|c| c.name == "exposure_dynamic_framerate").unwrap();
    dev.set_control(c.id, 0, false).unwrap();
    drop(dev);
    // no core in this test: drain what the workers publish
    std::thread::spawn(move || while core_rx.recv().is_ok() {});
    let mut workers = Vec::new();
    let mut readers = Vec::new();
    for c in CAMS {
        let def = Arc::new(config::parse(c.name, &toml::from_str(c.toml).unwrap(), std::path::Path::new("/")).unwrap());
        let writer = hub.video.register(c.name);
        readers.push(hub.video.take_reader(c.name).unwrap());
        let status = Arc::new(Status::default());
        let (tx, rx) = crossbeam_channel::unbounded();
        let h = camera::spawn(hub.clone(), def, status.clone(), writer, rx).unwrap();
        workers.push((tx, h, status));
    }
    // warm up (device open, stream start, first frames)
    let warm = Instant::now() + Duration::from_secs(2);
    while Instant::now() < warm {
        for r in &mut readers {
            let _ = r.fresh();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    // (first seq, first ts, last seq, last ts) per slot: publish sequence numbers count every
    // frame that reached the slot, even ones the reader didn't look at.
    let mut span = vec![(0u64, 0u64, 0u64, 0u64); CAMS.len()];
    let mut last_ts = vec![0u64; CAMS.len()];
    let window = Duration::from_secs(5);
    let start = Instant::now();
    while start.elapsed() < window {
        for (i, r) in readers.iter_mut().enumerate() {
            if let Some(f) = r.fresh() {
                let c = &CAMS[i];
                assert_eq!(f.format, c.format, "{}", c.name);
                assert_eq!((f.width, f.height), c.size, "{}", c.name);
                assert!(f.ts > last_ts[i], "{}: timestamps must increase", c.name);
                let now = se_clock::now();
                assert!(now >= f.ts && now - f.ts < 200_000_000, "{}: frame ts is master-clock capture time", c.name);
                last_ts[i] = f.ts;
                if span[i].0 == 0 {
                    span[i] = (f.seq, f.ts, f.seq, f.ts);
                }
                span[i].2 = f.seq;
                span[i].3 = f.ts;
            }
        }
        std::thread::sleep(Duration::from_micros(500));
    }
    for (i, c) in CAMS.iter().enumerate() {
        let st = &workers[i].2;
        let (s0, t0, s1, t1) = span[i];
        let fps = (s1 - s0) as f64 * 1e9 / (t1 - t0).max(1) as f64;
        println!(
            "{:<9} slot fps {fps:6.2} | worker fps {:6.2} | signal {} | dropped {} | cpu {:5.1}% | {}",
            c.name,
            st.fps(),
            st.signal.load(Ordering::Relaxed),
            st.dropped.load(Ordering::Relaxed),
            st.cpu(),
            st.info.lock().path
        );
        assert!(st.capturing.load(Ordering::Relaxed), "{} capturing", c.name);
        // Cameras deliver slightly under nominal (59.94, or ~58 on VC42 input 1 per v4l2-ctl);
        // every frame the driver delivered must reach the slot.
        assert!((fps - c.fps).abs() / c.fps < 0.05, "{}: slot rate {fps:.2} vs nominal {}", c.name, c.fps);
        assert_eq!(st.dropped.load(Ordering::Relaxed), 0, "{}: frames lost between driver and slot", c.name);
        assert!(st.signal.load(Ordering::Relaxed), "{}: live picture expected", c.name);
    }
    for (tx, h, _) in workers {
        tx.send(camera::Cmd::Stop).unwrap();
        h.join().unwrap();
    }
}
