use super::*;
use rusqlite::params;

const HOUR: i64 = 3600;

fn has_av1() -> bool {
    std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("libsvtav1") && String::from_utf8_lossy(&o.stdout).contains("libopus"))
}

struct Fixture {
    _tmp: tempfile::TempDir,
    db: Db,
    project: PathBuf,
    rec_dir: PathBuf,
    show_dir: PathBuf,
    session_dir: PathBuf,
    master: PathBuf,
    iso: PathBuf,
    ended_at: i64,
}

fn ffmpeg(args: &[&str]) {
    let st = std::process::Command::new("ffmpeg").args(["-nostdin", "-hide_banner", "-v", "error", "-y"]).args(args).status().unwrap();
    assert!(st.success(), "ffmpeg {args:?}");
}

/// A recorded, closed show: master (+2 audio tracks) and one camera ISO, data, a clip.
fn fixture(media: bool) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    let rec_dir = tmp.path().join("rec");
    let show_dir = rec_dir.join("2026-10-01 20-00 (s1)");
    let session_dir = project.join("sessions/s1");
    std::fs::create_dir_all(show_dir.join("data/lanes")).unwrap();
    std::fs::create_dir_all(show_dir.join("data/session")).unwrap();
    std::fs::create_dir_all(show_dir.join("clips")).unwrap();
    std::fs::create_dir_all(&session_dir).unwrap();
    let master = show_dir.join("main-100-0.mkv");
    let iso = show_dir.join("kit-100-1.mkv");
    if media {
        let m = master.to_string_lossy().into_owned();
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=320x180:r=60:d=3",
            "-f",
            "lavfi",
            "-i",
            "sine=f=440:d=3:sample_rate=48000",
            "-f",
            "lavfi",
            "-i",
            "sine=f=660:d=3:sample_rate=48000",
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map",
            "2:a",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "flac",
            &m,
        ]);
        ffmpeg(&["-f", "lavfi", "-i", "testsrc=s=160x90:r=30:d=1", "-c:v", "libx264", "-preset", "ultrafast", &iso.to_string_lossy()]);
    } else {
        std::fs::write(&master, b"master").unwrap();
        std::fs::write(&iso, b"iso").unwrap();
    }
    std::fs::write(
        show_dir.join("data/show.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema": 1, "session": "s1",
            "recordings": [
                {"path": master.to_string_lossy(), "canvas": "main", "role": "master", "source": "canvas:wide", "offset": 0.0},
                {"path": iso.to_string_lossy(), "canvas": "kit", "role": "iso", "source": "camera:cam_kit", "offset": 0.0},
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(show_dir.join("data/lanes/songs.jsonl"), "{\"t0\":0,\"t1\":1,\"label\":\"a\"}\n".repeat(5000)).unwrap();
    std::fs::write(show_dir.join("data/session/signals.bin"), vec![7u8; 200_000]).unwrap();
    std::fs::write(show_dir.join("clips/c1.mp4"), b"clip").unwrap();
    std::fs::write(
        session_dir.join("meta.toml"),
        format!(
            "clock = \"{{}}\"\nshow = {{ dir = {:?}, name = \"show\" }}\n\n[[recordings]]\npath = {:?}\ncanvas = \"main\"\nrole = \"master\"\nsource = \"canvas:wide\"\nstart_ns = 100\n\n[[recordings]]\npath = {:?}\ncanvas = \"kit\"\nrole = \"iso\"\nsource = \"camera:cam_kit\"\nstart_ns = 100\n",
            show_dir.to_string_lossy(),
            master.to_string_lossy(),
            iso.to_string_lossy()
        ),
    )
    .unwrap();
    let db = Db::memory().unwrap();
    store::migrate(&db).unwrap();
    let ended_at = unix_now() - HOUR;
    db.with(|c| c.execute("INSERT INTO sessions (id, started_at, ended_at, dir) VALUES ('s1', 0, ?1, ?2)", params![ended_at, session_dir.to_string_lossy()]))
        .unwrap();
    db.with(|c| {
        c.execute(
            "INSERT INTO clips (session, key, score, marker_score, start_ns, peak_ns, end_ns, recording, rec_start_ns, in_s, out_s, peak_s, wide_path, wide_thumb, status, created_at, updated_at)
             VALUES ('s1', 'k', 1, 1, 0, 0, 0, ?1, 0, 0, 1, 0, ?2, ?3, 'approved', 0, 0)",
            params![master.to_string_lossy(), show_dir.join("clips/c1.mp4").to_string_lossy(), show_dir.join("clips/c1.jpg").to_string_lossy()],
        )
    })
    .unwrap();
    Fixture { _tmp: tmp, db, project, rec_dir, show_dir, session_dir, master, iso, ended_at }
}

fn env(f: &Fixture, offline_s: i64) -> StageEnv {
    let mut cfg = RecordingConfig { dir: f.rec_dir.to_string_lossy().into_owned(), ..RecordingConfig::default() };
    cfg.archive.min_offline_minutes = 20;
    StageEnv { db: f.db.clone(), project_root: f.project.clone(), cfg, now: unix_now(), offline_s, force: false, progress: Arc::new(|_| {}) }
}

fn row(f: &Fixture) -> ArchiveRow {
    assert!(enqueue(&f.db, &f.project, "s1"));
    assert!(!enqueue(&f.db, &f.project, "s1"), "queued once");
    store::archive_get(&f.db, "s1").unwrap().unwrap()
}

fn clip_job(db: &Db, state: &str) {
    db.with(|c| c.execute("INSERT OR REPLACE INTO clip_jobs (session, state, queued_at) VALUES ('s1', ?1, 0)", [state])).unwrap();
}

#[test]
fn bitrate_hits_the_size_target_with_two_opus_tracks() {
    // 1.8 GB/h = 4000 kbps in total; 2 % overhead and 2 × 128 kbps Opus leave 3664 kbps of video
    assert_eq!(video_kbps(1.8, 2, 128), 3664);
    let total = (video_kbps(1.8, 2, 128) + 2 * 128) as f64;
    let gb_per_hour = total / (1.0 - OVERHEAD) * 3600.0 / 8_000_000.0;
    assert!((gb_per_hour - 1.8).abs() < 0.001, "{gb_per_hour}");
    // 5.4 GB for a 3-hour show
    assert!(((video_kbps(1.8, 2, 128) + 256) as f64 * 1000.0 / 8.0 * 3.0 * 3600.0 / 1e9 / (1.0 - OVERHEAD) - 5.4).abs() < 0.01);
    assert_eq!(video_kbps(1.8, 0, 128), 3920);
    assert_eq!(video_kbps(0.2, 8, 512), MIN_VIDEO_KBPS, "never below the floor");
    let args = encode_args(Path::new("/in.mkv"), Path::new("/out/main.mkv.part"), 3664, 128, 60.0);
    let after = |flag: &str| args.iter().position(|a| a == flag).map(|i| args[i + 1].as_str());
    assert_eq!(after("-c:v"), Some("libsvtav1"));
    assert_eq!(after("-b:v"), Some("3664k"));
    assert_eq!(after("-c:a"), Some("libopus"));
    assert_eq!(after("-b:a"), Some("128k"));
    assert_eq!(after("-fps_mode"), Some("passthrough"), "60 fps kept as recorded");
    assert_eq!(after("-g"), Some("300"));
    assert_eq!(args.iter().filter(|a| *a == "-map").count(), 2, "first video + every audio track");
    assert_eq!(after("-f"), Some("matroska"), "a .part name still writes Matroska");
}

#[test]
fn encodes_use_half_the_cores_and_back_off() {
    assert_eq!(encode_cpus(&[0, 1, 2, 3, 4, 5, 6, 7]), vec![4, 5, 6, 7]);
    assert_eq!(encode_cpus(&[0, 1, 2]), vec![1, 2]);
    assert_eq!(encode_cpus(&[0]), vec![0]);
    assert_eq!((backoff_s(1), backoff_s(2), backoff_s(3), backoff_s(50)), (60, 120, 240, 3600));
}

#[test]
fn plan_finds_master_segments_and_isos_but_not_legacy_tall() {
    let f = fixture(false);
    let r = row(&f);
    assert_eq!(r.masters, vec![ArchiveMaster { src: f.master.to_string_lossy().into_owned(), name: "main.mkv".into() }]);
    assert_eq!(r.isos, vec![f.iso.to_string_lossy().into_owned()]);
    assert_eq!((r.stage.as_str(), r.ended_at, r.show.as_str()), ("wait_offline", f.ended_at, "2026-10-01 20-00 (s1)"));
    // a legacy show (no roles): wide is the master, tall is neither archived nor deleted
    let wide = f.show_dir.join("wide.mkv");
    let wide2 = f.show_dir.join("wide-2.mkv");
    std::fs::write(
        f.session_dir.join("meta.toml"),
        format!(
            "show = {{ dir = {:?} }}\n[[recordings]]\npath = {:?}\ncanvas = \"wide\"\n[[recordings]]\npath = \"/t.mkv\"\ncanvas = \"tall\"\n[[recordings]]\npath = {:?}\ncanvas = \"wide\"\n",
            f.show_dir.to_string_lossy(),
            wide.to_string_lossy(),
            wide2.to_string_lossy()
        ),
    )
    .unwrap();
    let legacy = plan(&f.db, &f.project, "s1").unwrap();
    assert_eq!(legacy.masters.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), vec!["main.mkv", "main-2.mkv"]);
    assert!(legacy.isos.is_empty() && !legacy.isos_left);
}

#[test]
fn waits_for_the_offline_time_unless_forced() {
    let f = fixture(false);
    let mut r = row(&f);
    assert!(matches!(step(&env(&f, 19 * 60), &mut r).unwrap(), Step::Wait(_)));
    assert_eq!(r.stage, "wait_offline");
    let mut forced = env(&f, 0);
    forced.force = true;
    assert_eq!(step(&forced, &mut r).unwrap(), Step::Next);
    assert_eq!(r.stage, "iso_cleanup");
    let mut r = store::archive_get(&f.db, "s1").unwrap().unwrap();
    assert_eq!(step(&env(&f, 20 * 60), &mut r).unwrap(), Step::Next);
}

#[test]
fn iso_cleanup_follows_clips_days_and_protection() {
    let f = fixture(false);
    let mut r = row(&f);
    r.stage = "iso_cleanup".into();
    // the clip job is still running: keep the camera files
    clip_job(&f.db, "running");
    iso_sweep(&env(&f, HOUR), &mut r);
    assert!(f.iso.exists() && r.isos_left);
    // job done, but a clip awaits review
    clip_job(&f.db, "done");
    f.db.with(|c| c.execute("UPDATE clips SET status = 'ready'", [])).unwrap();
    iso_sweep(&env(&f, HOUR), &mut r);
    assert!(f.iso.exists());
    // the keep time is over: delete even with clips pending — unless the show is protected
    std::fs::write(f.show_dir.join(".keep"), "").unwrap();
    let mut late = env(&f, HOUR);
    late.now = f.ended_at + 14 * 86_400;
    iso_sweep(&late, &mut r);
    assert!(f.iso.exists(), "protected show keeps its ISOs");
    std::fs::remove_file(f.show_dir.join(".keep")).unwrap();
    iso_sweep(&late, &mut r);
    assert!(!f.iso.exists() && !r.isos_left);
    assert!(f.master.exists(), "ISO cleanup never touches the master");
    // clips reviewed: deleted on the next sweep (without waiting the days)
    let f = fixture(false);
    let mut r = row(&f);
    clip_job(&f.db, "done");
    iso_sweep(&env(&f, HOUR), &mut r);
    assert!(!f.iso.exists());
    // the stage itself never blocks archiving
    let f = fixture(false);
    let mut r = row(&f);
    clip_job(&f.db, "failed");
    r.stage = "iso_cleanup".into();
    assert_eq!(step(&env(&f, HOUR), &mut r).unwrap(), Step::Next);
    assert!(f.iso.exists() && r.isos_left && r.stage == "encode");
}

#[test]
fn missing_archive_folder_waits_and_keeps_the_recording() {
    let f = fixture(false);
    let mut r = row(&f);
    r.stage = "encode".into();
    let mut e = env(&f, HOUR);
    e.cfg.archive.dir = f.rec_dir.join("unplugged/Archive").to_string_lossy().into_owned();
    match step(&e, &mut r).unwrap() {
        Step::Blocked(why) => assert!(why.starts_with("archive folder missing"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(!f.rec_dir.join("unplugged").exists(), "a configured folder is never created");
    assert!(f.master.exists() && r.dest.is_empty() && r.stage == "encode");
    // a drive unplugged after encoding began: later stages wait too, nothing is deleted
    r.dest = f.rec_dir.join("gone/show").to_string_lossy().into_owned();
    for stage in ["encode", "verify", "package", "cleanup"] {
        r.stage = stage.into();
        assert!(matches!(step(&env(&f, HOUR), &mut r).unwrap(), Step::Blocked(_)), "{stage}");
    }
    assert!(f.master.exists() && f.show_dir.join("data").exists());
    assert!(health(true, false, &[r], Some("archive folder missing: /x")).0 == "warn");
}

#[test]
fn health_reports_waiting_failures_and_totals() {
    let done = ArchiveRow { show: "a".into(), stage: "done".into(), state: "done".into(), bytes_out: 5_400_000_000, ..Default::default() };
    assert_eq!(health(true, false, std::slice::from_ref(&done), None), ("pass", "archive up to date (1 show, 5.4 GB)".into()));
    let waiting = ArchiveRow { show: "b".into(), stage: "encode".into(), state: "running".into(), progress: 0.42, ..Default::default() };
    let (s, d) = health(true, false, &[done.clone(), waiting.clone()], None);
    assert_eq!(s, "warn");
    assert!(d.contains("1 show waiting") && d.contains("42%"), "{d}");
    let failing = ArchiveRow { state: "failed".into(), attempts: 3, detail: "boom".into(), ..waiting };
    assert_eq!(health(true, false, &[done.clone(), failing], None).0, "fail");
    assert_eq!(health(false, false, &[], None).0, "pass");
}

#[test]
fn archive_encode_freezes_while_live_and_finishes_after() {
    if !has_av1() {
        eprintln!("skipping: ffmpeg lacks libsvtav1/libopus");
        return;
    }
    let f = fixture(true);
    let out = f.show_dir.join("a.mkv.part");
    let halt = Halt::manual(OnHold::Pause);
    halt.set_hold(true);
    let h = halt.clone();
    let seen = Arc::new(Mutex::new(0.0f64));
    let s2 = seen.clone();
    let (src, o) = (f.master.clone(), out.clone());
    let t = std::thread::spawn(move || live::with_halt(h, || run_encode(&encode_args(&src, &o, 300, 64, 60.0), &[], move |t| *s2.lock() = t)));
    std::thread::sleep(Duration::from_millis(1500));
    assert!(*seen.lock() < 0.5, "frozen while live: {}", *seen.lock());
    halt.set_hold(false);
    t.join().unwrap().unwrap();
    assert!(ffmpeg::probe(&out).unwrap().duration > 2.5);
}

#[test]
fn stage_machine_archives_verifies_commits_and_resumes() {
    if !has_av1() {
        eprintln!("skipping: ffmpeg lacks libsvtav1/libopus");
        return;
    }
    let f = fixture(true);
    let mut r = row(&f);
    let e = env(&f, HOUR);
    assert_eq!(step(&e, &mut r).unwrap(), Step::Next); // wait_offline
    assert_eq!(step(&e, &mut r).unwrap(), Step::Next); // iso_cleanup: no clip job, nothing pending
    assert!(!f.iso.exists() && !r.isos_left);
    // a crash left a partial encode behind: it's removed and redone
    let dest = f.rec_dir.join("Archive").join(&r.show);
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("main.mkv.part"), b"half").unwrap();
    assert_eq!(step(&e, &mut r).unwrap(), Step::Next); // encode
    assert_eq!(PathBuf::from(&r.dest), dest);
    assert!(!dest.join("main.mkv.part").exists() && dest.join("main.mkv").exists());
    assert_eq!(r.video_kbps, 3664);
    // verify before anything is deleted: a broken copy is thrown away and encoded again
    let good = std::fs::read(dest.join("main.mkv")).unwrap();
    std::fs::write(dest.join("main.mkv"), &good[..good.len() / 3]).unwrap();
    assert!(step(&e, &mut r).is_err());
    assert_eq!(r.stage, "encode");
    assert!(!dest.join("main.mkv").exists() && f.master.exists());
    assert_eq!(step(&e, &mut r).unwrap(), Step::Next); // encode again
    assert_eq!(step(&e, &mut r).unwrap(), Step::Next); // verify
    let p = ffmpeg::probe(&dest.join("main.mkv")).unwrap();
    assert_eq!((p.audio_streams, p.fps.round() as i64), (2, 60));
    // verifying again is harmless (idempotent restart)
    r.stage = "verify".into();
    assert_eq!(step(&e, &mut r).unwrap(), Step::Next);
    // hold: a protected show keeps its full-quality copy
    std::fs::write(f.show_dir.join("keep"), "").unwrap();
    assert!(matches!(step(&e, &mut r).unwrap(), Step::Wait(_)));
    std::fs::remove_file(f.show_dir.join("keep")).unwrap();
    assert_eq!(step(&e, &mut r).unwrap(), Step::Next); // hold: clips settled
    assert_eq!(step(&e, &mut r).unwrap(), Step::Next); // package
    r.stage = "package".into();
    assert_eq!(step(&e, &mut r).unwrap(), Step::Next, "package redone after a crash");
    assert!(dest.join("data/session/signals.bin.zst").exists() && !dest.join("data/session/signals.bin").exists());
    assert!(dest.join("data/lanes/songs.jsonl").exists(), "timeline files stay readable");
    assert!(dest.join("clips/c1.mp4").exists() && !dest.join("data.part").exists());
    assert!(f.master.exists() && f.show_dir.join("data").exists(), "nothing deleted before commit");
    assert_eq!(step(&e, &mut r).unwrap(), Step::Next); // commit
    assert_eq!(r.stage, "cleanup");
    let committed = store::archive_get(&f.db, "s1").unwrap().unwrap();
    assert_eq!(committed.stage, "cleanup", "stage advanced with the clip paths in one transaction");
    let main = dest.join("main.mkv").to_string_lossy().into_owned();
    let clip = store::list(&f.db, Some("s1"), None, 10).unwrap().remove(0);
    assert_eq!(clip.recording, main);
    assert_eq!(clip.wide_path.as_deref(), Some(dest.join("clips/c1.mp4").to_string_lossy().as_ref()));
    let meta = session::parse_meta(&std::fs::read_to_string(f.session_dir.join("meta.toml")).unwrap()).unwrap();
    assert_eq!(meta.recordings.len(), 1);
    assert_eq!(meta.recordings[0].path, dest.join("main.mkv"));
    assert_eq!(show::show_dir_from_meta(&f.session_dir), Some(dest.clone()));
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(dest.join("data/show.json")).unwrap()).unwrap();
    assert_eq!(manifest["recordings"].as_array().unwrap().len(), 1);
    assert_eq!(manifest["recordings"][0]["path"], serde_json::json!(main));
    let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(dest.join("archive.json")).unwrap()).unwrap();
    assert_eq!(doc["schema"], serde_json::json!(SCHEMA));
    assert!(doc["verified_at"].as_i64().is_some() && doc["sizes"]["total_bytes"].as_u64().unwrap() > 0);
    // committing again changes nothing
    r.stage = "commit".into();
    assert_eq!(step(&e, &mut r).unwrap(), Step::Next);
    assert_eq!(store::list(&f.db, Some("s1"), None, 10).unwrap()[0].recording, main);
    assert_eq!(step(&e, &mut r).unwrap(), Step::Next); // cleanup
    assert_eq!(r.stage, "done");
    assert!(!f.master.exists() && !f.show_dir.exists(), "hot copy removed after commit");
    assert!(dest.join("main.mkv").exists() && dest.join("data/show.json").exists());
    assert!(matches!(step(&e, &mut r).unwrap(), Step::Wait(_)), "done stays done");
}
