//! Sessions (§3.3): one stream run's event log, downsampled signal history, markers, and
//! metadata, under `sessions/<id>/`.
//!
//! `events.jsonl.zst` is a series of zstd frames (one per engine run/flush), each holding
//! JSON lines. A crash truncates at most the unflushed tail; readers stop at the first
//! damaged frame.

use anyhow::Result;
use se_core::{Input, RuntimeState};
use se_proto::{Event, Ts, Value};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum LogRec {
    /// Engine (re)start: replay begins from this point.
    Start { t0: Ts, period: Ts, tick: u64, wall_ns: i64, restore: Option<RuntimeState> },
    /// A replayable input applied at `tick`.
    In { tick: u64, input: Input },
    /// An event the core produced (for review, clips, credits).
    Ev { event: Event },
}

pub struct SessionWriter {
    pub id: String,
    pub dir: PathBuf,
    events: Option<zstd::Encoder<'static, File>>,
    signals: Option<zstd::Encoder<'static, File>>,
    signal_names: Vec<String>,
    dirty: bool,
}

pub fn new_session_id() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    // YYYYMMDD-HHMMSS in UTC without a date crate
    let days = secs / 86400;
    let (y, m, d) = civil_from_days(days as i64);
    let s = secs % 86400;
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", s / 3600, (s / 60) % 60, s % 60)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

impl SessionWriter {
    /// Open (or continue) a session directory.
    pub fn open(sessions_root: &Path, id: &str) -> Result<SessionWriter> {
        let dir = sessions_root.join(id);
        std::fs::create_dir_all(&dir)?;
        let ev = OpenOptions::new().create(true).append(true).open(dir.join("events.jsonl.zst"))?;
        let sg = OpenOptions::new().create(true).append(true).open(dir.join("signals.bin"))?;
        if !dir.join("markers.json").exists() {
            std::fs::write(dir.join("markers.json"), "[]\n")?;
        }
        Ok(SessionWriter {
            id: id.to_string(),
            dir,
            events: Some(zstd::Encoder::new(ev, 3)?),
            signals: Some(zstd::Encoder::new(sg, 3)?),
            signal_names: Vec::new(),
            dirty: false,
        })
    }

    pub fn write(&mut self, rec: &LogRec) -> Result<()> {
        if let Some(e) = &mut self.events {
            serde_json::to_writer(&mut *e, rec)?;
            e.write_all(b"\n")?;
            self.dirty = true;
        }
        Ok(())
    }

    /// Append one downsampled signal frame.
    pub fn write_signals(&mut self, ts: Ts, names: &[String], values: &[f32]) -> Result<()> {
        let Some(s) = &mut self.signals else { return Ok(()) };
        if names != self.signal_names.as_slice() {
            s.write_all(&[1u8])?;
            s.write_all(&(names.len() as u32).to_le_bytes())?;
            for n in names {
                s.write_all(&(n.len() as u16).to_le_bytes())?;
                s.write_all(n.as_bytes())?;
            }
            self.signal_names = names.to_vec();
        }
        s.write_all(&[2u8])?;
        s.write_all(&ts.to_le_bytes())?;
        s.write_all(&(values.len() as u32).to_le_bytes())?;
        for v in values {
            s.write_all(&v.to_le_bytes())?;
        }
        self.dirty = true;
        Ok(())
    }

    /// End the current zstd frames so everything written so far is durable, and start new ones.
    pub fn flush(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.dirty = false;
        for slot in [&mut self.events, &mut self.signals] {
            if let Some(enc) = slot.take() {
                let f = enc.finish()?;
                f.sync_data()?;
                *slot = Some(zstd::Encoder::new(f, 3)?);
            }
        }
        // a new frame on signals.bin must restate names
        self.signal_names.clear();
        Ok(())
    }

    pub fn add_marker(&self, marker: &Value) -> Result<()> {
        let p = self.dir.join("markers.json");
        let mut list: Vec<Value> = std::fs::read_to_string(&p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        list.push(marker.clone());
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&list)? + "\n")?;
        std::fs::rename(tmp, p)?;
        Ok(())
    }

    /// Merge keys into `meta.toml` (OBS recording paths, clock mappings, …).
    pub fn set_meta(&self, key: &str, value: &Value) -> Result<()> {
        let p = self.dir.join("meta.toml");
        let mut doc: toml_edit::DocumentMut = std::fs::read_to_string(&p).unwrap_or_default().parse()?;
        match crate::project::to_edit_value(value) {
            Some(v) => doc[key] = toml_edit::Item::Value(v),
            None => {
                doc.remove(key);
            }
        }
        std::fs::write(&p, doc.to_string())?;
        Ok(())
    }

    pub fn close(mut self) -> Result<()> {
        for slot in [&mut self.events, &mut self.signals] {
            if let Some(enc) = slot.take() {
                enc.finish()?.sync_all()?;
            }
        }
        Ok(())
    }
}

impl Drop for SessionWriter {
    fn drop(&mut self) {
        for slot in [&mut self.events, &mut self.signals] {
            if let Some(enc) = slot.take() {
                let _ = enc.finish();
            }
        }
    }
}

/// Read every intact record (stops quietly at a truncated tail).
pub fn read_log(dir: &Path) -> Result<Vec<LogRec>> {
    let bytes = std::fs::read(dir.join("events.jsonl.zst"))?;
    let mut out = Vec::new();
    let mut dec = zstd::stream::read::Decoder::new(&bytes[..])?;
    let mut buf = Vec::new();
    // Read as much as decodes cleanly.
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match dec.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    for line in BufReader::new(&buf[..]).lines() {
        let Ok(line) = line else { break };
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<LogRec>(&line) {
            Ok(r) => out.push(r),
            Err(_) => break, // partial last line
        }
    }
    Ok(out)
}

/// Decoded signal history: `(names, frames of (ts, values))`.
pub fn read_signals(dir: &Path) -> Result<Vec<(Vec<String>, Vec<(Ts, Vec<f32>)>)>> {
    let bytes = std::fs::read(dir.join("signals.bin"))?;
    let mut dec = zstd::stream::read::Decoder::new(&bytes[..])?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match dec.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let mut out: Vec<(Vec<String>, Vec<(Ts, Vec<f32>)>)> = Vec::new();
    let mut i = 0usize;
    let take = |i: &mut usize, n: usize| -> Option<&[u8]> {
        let s = buf.get(*i..*i + n)?;
        *i += n;
        Some(s)
    };
    while let Some(t) = take(&mut i, 1) {
        match t[0] {
            1 => {
                let Some(c) = take(&mut i, 4) else { break };
                let n = u32::from_le_bytes(c.try_into().unwrap()) as usize;
                let mut names = Vec::with_capacity(n);
                for _ in 0..n {
                    let Some(l) = take(&mut i, 2) else { break };
                    let l = u16::from_le_bytes(l.try_into().unwrap()) as usize;
                    let Some(s) = take(&mut i, l) else { break };
                    names.push(String::from_utf8_lossy(s).into_owned());
                }
                out.push((names, Vec::new()));
            }
            2 => {
                let Some(ts) = take(&mut i, 8) else { break };
                let ts = u64::from_le_bytes(ts.try_into().unwrap());
                let Some(c) = take(&mut i, 4) else { break };
                let n = u32::from_le_bytes(c.try_into().unwrap()) as usize;
                let Some(raw) = take(&mut i, n * 4) else { break };
                let vals = raw.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
                if let Some(last) = out.last_mut() {
                    last.1.push((ts, vals));
                }
            }
            _ => break,
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_proto::{Command, Op, Origin};

    #[test]
    fn log_survives_restart_and_truncation() {
        let d = tempfile::tempdir().unwrap();
        {
            let mut w = SessionWriter::open(d.path(), "s1").unwrap();
            w.write(&LogRec::Start { t0: 1, period: 4_166_666, tick: 0, wall_ns: 0, restore: None }).unwrap();
            w.write(&LogRec::In { tick: 5, input: Input::Command { cmd: Command::new(Origin::Cli, Op::Panic) } }).unwrap();
            w.flush().unwrap();
            w.write_signals(10, &["a".into(), "b".into()], &[0.5, 1.0]).unwrap();
            w.write_signals(20, &["a".into(), "b".into()], &[0.25, 0.0]).unwrap();
            w.close().unwrap();
        }
        {
            // "crash": a second run appends, then leaves an unfinished frame
            let mut w = SessionWriter::open(d.path(), "s1").unwrap();
            w.write(&LogRec::Start { t0: 2, period: 4_166_666, tick: 0, wall_ns: 0, restore: None }).unwrap();
            w.flush().unwrap();
            w.write(&LogRec::Ev { event: Event::new("x", Origin::Sim, Value::Null) }).unwrap();
            std::mem::forget(w);
        }
        let recs = read_log(&d.path().join("s1")).unwrap();
        assert_eq!(recs.len(), 3, "{recs:?}");
        let sig = read_signals(&d.path().join("s1")).unwrap();
        assert_eq!(sig[0].0, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(sig[0].1.len(), 2);
        assert_eq!(sig[0].1[1].1, vec![0.25, 0.0]);
    }

    #[test]
    fn session_ids_sortable() {
        let id = new_session_id();
        assert_eq!(id.len(), 15);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_000), (2024, 10, 4));
    }
}
