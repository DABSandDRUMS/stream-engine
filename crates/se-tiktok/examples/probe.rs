//! Manual smoke test (hits TikTok; never run by `cargo test`):
//!
//! ```text
//! cargo run -p se-tiktok --example probe -- <unique_id>                  # room lookup only
//! cargo run -p se-tiktok --example probe -- <unique_id> --connect 60     # + sign + socket for 60 s
//! ```
//!
//! `--connect` signs through the default Euler Stream endpoint (or `--sign-url <url>`) with the
//! keyring key `tiktok.sign_api_key` when set (anonymous limits otherwise), then prints every
//! normalized event, state change and log line.

use se_proto::{Event, Value};
use se_tiktok::client::WebTransport;
use se_tiktok::config::{DEFAULT_SIGN_URL, Settings, normalize_unique_id};
use se_tiktok::session::{Out, Shared, Sink};
use se_tiktok::transport::Transport;
use std::sync::Arc;
use std::time::Duration;

struct Print;

impl Sink for Print {
    fn emit(&self, e: Event) {
        let who = e.actor.as_ref().map(|a| format!("{} {:?}", a.name, a.roles)).unwrap_or_default();
        println!("event  {:<14} {who:<32} {}", e.ty, e.payload);
    }
    fn publish(&self, address: &str, v: Value) {
        println!("state  {address} = {v}");
    }
    fn signal(&self, name: &str, v: f32) {
        println!("signal {name} = {v}");
    }
    fn log(&self, level: &str, msg: String) {
        println!("log    [{level}] {msg}");
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: probe <unique_id> [--connect <seconds>] [--sign-url <url>]";
    let raw = args.first().ok_or_else(|| anyhow::anyhow!(usage))?;
    let uid = normalize_unique_id(raw).map_err(anyhow::Error::msg)?;
    anyhow::ensure!(!uid.is_empty(), usage);
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let connect = flag("--connect").map(|s| s.parse::<u64>()).transpose()?;
    let sign_url = flag("--sign-url").unwrap_or_else(|| DEFAULT_SIGN_URL.to_string());

    let t = WebTransport::new(&sign_url)?;
    let started = std::time::Instant::now();
    let status = t.room_status(&uid).await;
    println!("room lookup @{uid}: {status:?} ({} ms)", started.elapsed().as_millis());

    let Some(secs) = connect else { return Ok(()) };
    let settings = Settings { enabled: true, unique_id: uid, sign_url, ..Default::default() };
    let shared = Arc::new(Shared::default());
    let out = Out::new(Arc::new(Print), shared.clone());
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let run = tokio::spawn(se_tiktok::session::run(t, settings, out, stop_rx));
    tokio::time::sleep(Duration::from_secs(secs)).await;
    let _ = stop_tx.send(true);
    run.await?;
    println!("summary {}", shared.to_value());
    Ok(())
}
