//! Real-time scheduling status (§21, §28.4): the engine's own PipeWire data thread and the
//! PipeWire daemon's `data-loop` threads, reported as `health.audio.rt`.

use crate::graph::RtStats;
use std::sync::atomic::Ordering;

pub fn policy_name(p: i32) -> &'static str {
    match p & !libc::SCHED_RESET_ON_FORK {
        libc::SCHED_OTHER => "SCHED_OTHER",
        libc::SCHED_FIFO => "SCHED_FIFO",
        libc::SCHED_RR => "SCHED_RR",
        libc::SCHED_BATCH => "SCHED_BATCH",
        libc::SCHED_IDLE => "SCHED_IDLE",
        -1 => "unknown",
        _ => "other",
    }
}

fn is_rt(p: i32) -> bool {
    matches!(p & !libc::SCHED_RESET_ON_FORK, libc::SCHED_FIFO | libc::SCHED_RR)
}

/// Scheduling of every PipeWire daemon `data-loop*` thread owned by this user:
/// `(pid, tid, policy, priority)`.
pub fn server_data_loops() -> Vec<(i32, i32, i32, i32)> {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else { return out };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        if comm.trim() != "pipewire" {
            continue;
        }
        let owner = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|s| s.lines().find(|l| l.starts_with("Uid:")).and_then(|l| l.split_whitespace().nth(1)).and_then(|x| x.parse::<u32>().ok()));
        if owner != Some(uid) {
            continue;
        }
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else { continue };
        for t in tasks.flatten() {
            let Some(tid) = t.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
            let tcomm = std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/comm")).unwrap_or_default();
            if !tcomm.trim().starts_with("data-loop") {
                continue;
            }
            // SAFETY: querying another thread's policy is permitted for the same user.
            let pol = unsafe { libc::sched_getscheduler(tid) };
            let mut sp = libc::sched_param { sched_priority: 0 };
            // SAFETY: valid out pointer.
            unsafe { libc::sched_getparam(tid, &mut sp) };
            out.push((pid, tid, pol, sp.sched_priority));
        }
    }
    out
}

/// RLIMIT_RTPRIO of this process (soft).
pub fn rtprio_limit() -> u64 {
    let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: valid out pointer.
    unsafe { libc::getrlimit(libc::RLIMIT_RTPRIO, &mut r) };
    r.rlim_cur
}

/// Is this process in the `realtime` group (effective supplementary groups)?
pub fn in_realtime_group() -> (bool, bool) {
    let gid = std::fs::read_to_string("/etc/group")
        .ok()
        .and_then(|g| g.lines().find(|l| l.starts_with("realtime:")).and_then(|l| l.split(':').nth(2)).and_then(|x| x.parse::<u32>().ok()));
    let Some(gid) = gid else { return (false, false) };
    // SAFETY: first call gets the count, second fills the buffer.
    let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    let mut groups = vec![0 as libc::gid_t; n.max(0) as usize];
    let n = unsafe { libc::getgroups(groups.len() as i32, groups.as_mut_ptr()) };
    groups.truncate(n.max(0) as usize);
    (true, groups.contains(&gid))
}

/// `(status, detail)` for `health.audio.rt`.
pub fn rt_health(stats: &RtStats) -> (&'static str, String) {
    let pol = stats.policy.load(Ordering::Relaxed);
    let prio = stats.priority.load(Ordering::Relaxed);
    let tid = stats.tid.load(Ordering::Relaxed);
    let server = server_data_loops();
    let server_rt = !server.is_empty() && server.iter().all(|(_, _, p, _)| is_rt(*p));
    let ours = if tid < 0 { "engine data thread not running yet".to_string() } else { format!("engine data thread {tid}: {} prio {prio}", policy_name(pol)) };
    let srv = if server.is_empty() {
        "PipeWire daemon data-loop not found".to_string()
    } else {
        server.iter().map(|(pid, t, p, pr)| format!("pipewire[{pid}] data-loop {t}: {} prio {pr}", policy_name(*p))).collect::<Vec<_>>().join(", ")
    };
    let lim = rtprio_limit();
    let (grp_exists, in_grp) = in_realtime_group();
    let mut detail = format!("{ours}; {srv}; RLIMIT_RTPRIO={lim}");
    let status = if is_rt(pol) && server_rt {
        "pass"
    } else {
        if lim == 0 {
            if grp_exists && !in_grp {
                detail.push_str("; the user is in the `realtime` group but this login session predates it — log out and back in (or reboot) so PipeWire and the engine get rtprio");
            } else if !grp_exists {
                detail.push_str("; install `realtime-privileges` and add the user to `realtime`, then log in again");
            } else {
                detail.push_str("; realtime group active but RLIMIT_RTPRIO is 0 — check /etc/security/limits.d");
            }
        }
        if tid < 0 { "warn" } else { "fail" }
    };
    (status, detail)
}
