//! `stream-engine replay`: re-run a session segment through a fresh core with the project's
//! current configuration and print the resulting state (§2.2, §23).

use anyhow::{Context, Result, bail};
use se_core::{Config, Core};
use se_store::Project;
use se_store::session::{LogRec, read_log};
use std::path::Path;

pub struct ReplayOut {
    pub core: Core,
    pub inputs: usize,
    pub last_tick: u64,
}

/// Replay segment `segment` (0-based; negative counts from the end) of a session.
pub fn replay(project: &Path, session_dir: &Path, segment: isize, settle_ms: u64) -> Result<ReplayOut> {
    let project = Project::open(project)?;
    let loaded = project.load();
    let config = Config::build(&loaded.files);
    let recs = read_log(session_dir).with_context(|| format!("read {}", session_dir.display()))?;
    let starts: Vec<usize> = recs.iter().enumerate().filter(|(_, r)| matches!(r, LogRec::Start { .. })).map(|(i, _)| i).collect();
    if starts.is_empty() {
        bail!("session has no start record");
    }
    let idx = if segment < 0 { starts.len() as isize + segment } else { segment };
    let Some(&begin) = starts.get(idx.max(0) as usize) else { bail!("segment {segment} out of range ({} segments)", starts.len()) };
    let end = starts.iter().find(|&&s| s > begin).copied().unwrap_or(recs.len());
    let LogRec::Start { t0, tick, restore, .. } = &recs[begin] else { unreachable!() };
    let mut core = Core::new(config, *t0);
    core.seek_tick(*tick);
    if let Some(rs) = restore {
        core.restore(rs);
    }
    let inputs: Vec<(u64, se_core::Input)> = recs[begin..end]
        .iter()
        .filter_map(|r| match r {
            LogRec::In { tick, input } => Some((*tick, input.clone())),
            _ => None,
        })
        .collect();
    let last_tick = inputs.last().map(|(t, _)| *t).unwrap_or(*tick);
    let settle_ticks = settle_ms * 1_000_000 / core.period();
    let mut i = 0;
    while core.tick_index() < last_tick + settle_ticks {
        while i < inputs.len() && inputs[i].0 <= core.tick_index() + 1 {
            core.submit(inputs[i].1.clone());
            i += 1;
        }
        core.step();
        core.drain_outputs();
        core.drain_applied();
    }
    Ok(ReplayOut { core, inputs: inputs.len(), last_tick })
}

pub fn print(out: &ReplayOut, patterns: &[String]) {
    println!("replayed {} inputs up to tick {} (+settle)", out.inputs, out.last_tick);
    let st = out.core.state();
    let pats: Vec<String> = if patterns.is_empty() { vec!["show.**".into(), "preset.**".into()] } else { patterns.to_vec() };
    for p in &pats {
        for i in st.matching(p) {
            let prm = st.param(i);
            if !prm.resolved.is_null() {
                println!("{} = {}", prm.addr, prm.resolved);
            }
        }
    }
    let rs = out.core.runtime_state();
    println!("runtime: {}", serde_json::to_string(&rs).unwrap_or_default());
}
