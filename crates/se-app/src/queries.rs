//! Engine-level named queries (sessions, audit log, engine info).

use crate::daemon::Ctx;
use se_proto::Value;
use std::sync::Arc;

pub fn register(ctx: &Ctx) {
    let db = ctx.db.clone();
    // master-clock mappings (§3.2): wall, OBS stream/record, audio device, Twitch delay
    let clock = ctx.hub.clock.clone();
    ctx.hub.register_query(
        "clock",
        Arc::new(move |_, _| {
            let maps = clock.mappings();
            Box::pin(async move { serde_json::to_value(&maps).map(Value::from).map_err(|e| e.to_string()) })
        }),
    );
    ctx.hub.register_query(
        "sessions",
        Arc::new(move |_, args| {
            let db = db.clone();
            Box::pin(async move {
                let n = args.get_path("n").and_then(Value::as_i64).unwrap_or(50).clamp(1, 1000) as usize;
                db.sessions(n).map(Value::List).map_err(|e| e.to_string())
            })
        }),
    );
    let db = ctx.db.clone();
    ctx.hub.register_query(
        "audit",
        Arc::new(move |_, args| {
            let db = db.clone();
            Box::pin(async move {
                let n = args.get_path("n").and_then(Value::as_i64).unwrap_or(200).clamp(1, 5000) as usize;
                db.audit_recent(n).map(Value::List).map_err(|e| e.to_string())
            })
        }),
    );
    let hub = ctx.hub.clone();
    let project = ctx.project.root().display().to_string();
    ctx.hub.register_query(
        "engine.info",
        Arc::new(move |_, _| {
            let info = hub.info.read().clone();
            let project = project.clone();
            Box::pin(async move {
                Ok(Value::map()
                    .with("version", info.version)
                    .with("session", info.session)
                    .with("project", project)
                    .with("pid", std::process::id() as i64)
                    .with("started", info.started_wall))
            })
        }),
    );
}

/// Preflight (§17.1): every subsystem publishes `health.<check>` = `{status, detail}`
/// (`pass | warn | fail`); the engine adds disk, GPU, and idle-inhibitor checks.
pub fn register_preflight(ctx: &Ctx) {
    let hub = ctx.hub.clone();
    let root = ctx.project.root().to_path_buf();
    ctx.hub.register_query(
        "preflight",
        Arc::new(move |_, _| {
            let hub = hub.clone();
            let root = root.clone();
            Box::pin(async move {
                let mut items = Vec::new();
                let snap = hub.snapshot.load();
                let mut names: Vec<(&String, &usize)> = snap.index.iter().filter(|(a, _)| a.starts_with("health.")).collect();
                names.sort();
                for (a, i) in names {
                    let v = &snap.values[*i];
                    let status = v.get_path("status").and_then(Value::as_str).unwrap_or("warn").to_string();
                    let detail = v.get_path("detail").map(|d| d.to_string()).unwrap_or_default();
                    items.push(Value::map().with("name", a.trim_start_matches("health.")).with("status", status).with("detail", detail));
                }
                // disk space for recordings (project + ~/Videos)
                let free_gb = disk_free_gb(&root);
                items.push(check(
                    "disk",
                    if free_gb > 50.0 {
                        "pass"
                    } else if free_gb > 10.0 {
                        "warn"
                    } else {
                        "fail"
                    },
                    format!("{free_gb:.0} GB free"),
                ));
                let idle = snap.bool("system.idle_inhibited");
                let mode = snap.str("show.mode").unwrap_or("offline").to_string();
                items.push(check(
                    "idle_inhibitor",
                    if idle || mode == "offline" { "pass" } else { "warn" },
                    if idle { "stay-awake on".into() } else { "idle allowed (turns on at preshow)".to_string() },
                ));
                if let Some(k) = night_light_kelvin().await {
                    items.push(night_light_check(k));
                }
                if let Some((temp, used, total)) = gpu_stats().await {
                    let st = if temp > 85.0 || used / total > 0.9 {
                        "fail"
                    } else if temp > 78.0 || used / total > 0.8 {
                        "warn"
                    } else {
                        "pass"
                    };
                    items.push(check("gpu", st, format!("{temp:.0} °C, VRAM {used:.0}/{total:.0} MB")));
                }
                Ok(Value::List(items))
            })
        }),
    );
}

fn check(name: &str, status: &str, detail: String) -> Value {
    Value::map().with("name", name).with("status", status).with("detail", detail)
}

fn disk_free_gb(p: &std::path::Path) -> f64 {
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()) else { return 0.0 };
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid NUL-terminated path and out pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return 0.0;
    }
    st.f_bavail as f64 * st.f_frsize as f64 / 1e9
}

/// Colour temperature hyprsunset applies to the displays, if it's running (§16.3).
async fn night_light_kelvin() -> Option<u32> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        tokio::process::Command::new("hyprctl").args(["hyprsunset", "temperature"]).kill_on_drop(true).output(),
    )
    .await
    .ok()?
    .ok()?;
    String::from_utf8(out.stdout).ok()?.trim().parse().ok()
}

/// Night light doesn't reach the stream, but it makes the preview and colour work misleading.
/// Omarchy's night light is 4000 K; "off" (identity) reports 6000 K.
fn night_light_check(kelvin: u32) -> Value {
    if kelvin < 5500 {
        check(
            "night_light",
            "warn",
            format!("night light is on ({kelvin} K): colours on your screens look warmer than on stream; turn it off while you adjust colours or lights"),
        )
    } else {
        check("night_light", "pass", "off".into())
    }
}

/// `(temp °C, used MB, total MB)` from nvidia-smi.
async fn gpu_stats() -> Option<(f64, f64, f64)> {
    let out = tokio::process::Command::new("nvidia-smi")
        .args(["--query-gpu=temperature.gpu,memory.used,memory.total", "--format=csv,noheader,nounits"])
        .output()
        .await
        .ok()?;
    let s = String::from_utf8(out.stdout).ok()?;
    let v: Vec<f64> = s.lines().next()?.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    (v.len() == 3).then(|| (v[0], v[1], v[2]))
}

#[cfg(test)]
mod night_light_tests {
    use super::*;

    #[test]
    fn warm_night_light_warns_and_neutral_passes() {
        let st = |k| night_light_check(k).get_path("status").and_then(Value::as_str).unwrap_or("").to_string();
        assert_eq!(st(4000), "warn");
        assert_eq!(st(5499), "warn");
        assert_eq!(st(6000), "pass", "identity reports 6000 K");
        assert_eq!(st(6500), "pass");
    }
}
