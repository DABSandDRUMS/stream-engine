//! `stream` — command-line client for the engine (§2.2). Anything clickable is scriptable:
//! Hyprland keybinds, Stream Deck fallbacks, and scripts all go through here.

use anyhow::{Result, anyhow, bail};
use clap::{Parser, Subcommand};
use se_client::Conn;
use se_proto::wire::{ServerMsg, Subscription};
use se_proto::{Actor, Command, Op, Origin, Role, Value};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "stream", version, about = "Control the stream engine")]
struct Cli {
    /// Engine socket (default $XDG_RUNTIME_DIR/stream-engine/engine.sock)
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// Print the cause chain after the command.
    #[arg(long, global = true)]
    trace: bool,
    /// Machine-readable output.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Fire an event: `stream fire twitch.cheer bits=1000 message='hi'`
    Fire {
        event: String,
        args: Vec<String>,
        /// Attach a viewer actor (as if from chat).
        #[arg(long = "as")]
        user: Option<String>,
    },
    /// Run a simulator preset: `stream sim gift_bomb count=50`
    Sim {
        preset: String,
        args: Vec<String>,
    },
    /// Any one-line command: `stream do "preset.fire hype"`
    Do {
        text: Vec<String>,
    },
    /// Scene to preview (or program with --cut).
    Scene {
        name: String,
        #[arg(long)]
        cut: bool,
    },
    /// Preview → program.
    Take {
        transition: Option<String>,
    },
    Preset {
        name: String,
    },
    Release {
        name: String,
    },
    Mode {
        mode: String,
    },
    Set {
        address: String,
        value: String,
    },
    Panic,
    Clean,
    Undo,
    Redo,
    /// Toggle BRB (live ⇄ brb).
    Brb,
    /// Next/previous scene to preview.
    Next,
    Prev,
    /// Add a session marker ("clip that").
    Marker {
        label: Option<String>,
    },
    /// Read values: `stream get 'fx.*'`
    Get {
        pattern: String,
        #[arg(long)]
        meta: bool,
    },
    /// Why does an address have its value?
    Explain {
        address: String,
    },
    /// Cause chain of a trace id (`last` = most recent root).
    Trace {
        id: String,
    },
    /// Named query: `stream query presets`
    Query {
        name: String,
        args: Option<String>,
    },
    /// Stream events/changes: `stream watch --events 'twitch.*' --state 'show.*'`
    Watch {
        #[arg(long, default_value = "**")]
        events: Vec<String>,
        #[arg(long)]
        state: Vec<String>,
        #[arg(long)]
        signals: Vec<String>,
        #[arg(long)]
        logs: bool,
    },
    /// Preflight checklist.
    Preflight,
    /// Engine status.
    Status,
    /// Print the WebSocket/OSC API token from the keyring.
    Token,
}

fn kv_payload(args: &[String]) -> Value {
    let mut v = Value::map();
    for a in args {
        if let Some((k, x)) = a.split_once('=') {
            v = v.with(k, Value::parse_text(x));
        }
    }
    v
}

fn print_value(v: &Value, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    } else {
        match v {
            Value::List(l) => {
                for x in l {
                    println!("{}", serde_json::to_string(x).unwrap_or_default());
                }
            }
            other => println!("{}", serde_json::to_string_pretty(other).unwrap_or_default()),
        }
    }
}

fn run_op(c: &mut Conn, op: Op, actor: Option<Actor>, trace: bool) -> Result<()> {
    let mut cmd = Command::new(Origin::Cli, op);
    cmd.actor = actor;
    let label = cmd.op.describe();
    match c.exec(cmd)? {
        Ok(id) => {
            eprintln!("ok: {label}");
            if trace {
                std::thread::sleep(Duration::from_millis(120));
                print_trace(c, id)?;
            }
            Ok(())
        }
        Err(e) => bail!("{label}: {e}"),
    }
}

fn print_trace(c: &mut Conn, id: u64) -> Result<()> {
    let recs = c.trace(id)?;
    let mut depth = std::collections::HashMap::new();
    for r in recs {
        let d = r.parent.and_then(|p| depth.get(&p).copied()).map(|d: usize| d + 1).unwrap_or(0);
        depth.insert(r.id, d);
        println!("{}{:<8} {}", "  ".repeat(d), r.kind, r.label);
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Cmd::Token = cli.cmd {
        let e = keyring::Entry::new("stream-engine", "api.token")?;
        println!("{}", e.get_password().map_err(|e| anyhow!("no API token in the keyring ({e}); start the engine once"))?);
        return Ok(());
    }
    let mut c = Conn::connect(cli.socket.as_deref(), "cli")
        .map_err(|e| anyhow!("cannot reach the engine ({e}). Is `systemctl --user status stream-engine` running?"))?;
    let tr = cli.trace;
    match cli.cmd {
        Cmd::Fire { event, args, user } => {
            let actor = user.map(|u| Actor { platform: "twitch".into(), id: format!("cli-{u}"), name: u, roles: vec![Role::Everyone] });
            run_op(&mut c, Op::Emit { ty: event, payload: kv_payload(&args) }, actor, tr)?
        }
        Cmd::Sim { preset, args } => run_op(&mut c, Op::Action { name: format!("sim.{preset}"), args: kv_payload(&args) }, None, tr)?,
        Cmd::Do { text } => {
            let t = text.join(" ");
            run_op(&mut c, Op::parse(&t)?, None, tr)?
        }
        Cmd::Scene { name, cut } => run_op(&mut c, if cut { Op::SceneCut { scene: name, transition: None } } else { Op::SceneGo { scene: name } }, None, tr)?,
        Cmd::Take { transition } => run_op(&mut c, Op::SceneTake { transition, ms: None }, None, tr)?,
        Cmd::Preset { name } => run_op(&mut c, Op::PresetFire { name, payload: Value::Null }, None, tr)?,
        Cmd::Release { name } => run_op(&mut c, Op::PresetRelease { name }, None, tr)?,
        Cmd::Mode { mode } => run_op(&mut c, Op::ModeSet { mode }, None, tr)?,
        Cmd::Set { address, value } => run_op(&mut c, Op::Set { address, value: Value::parse_text(&value) }, None, tr)?,
        Cmd::Panic => run_op(&mut c, Op::Panic, None, tr)?,
        Cmd::Clean => run_op(&mut c, Op::Clean, None, tr)?,
        Cmd::Undo => run_op(&mut c, Op::Undo, None, tr)?,
        Cmd::Redo => run_op(&mut c, Op::Redo, None, tr)?,
        Cmd::Brb => {
            let mode = c.get("show.mode", false)?.first().and_then(|e| e.value.as_str().map(String::from)).unwrap_or_default();
            let next = if mode == "brb" { "live" } else { "brb" };
            run_op(&mut c, Op::ModeSet { mode: next.into() }, None, tr)?
        }
        Cmd::Next => run_op(&mut c, Op::Action { name: "scene.next".into(), args: Value::Null }, None, tr)?,
        Cmd::Prev => run_op(&mut c, Op::Action { name: "scene.prev".into(), args: Value::Null }, None, tr)?,
        Cmd::Marker { label } => {
            run_op(&mut c, Op::Action { name: "session.marker".into(), args: Value::map().with("label", label.unwrap_or_else(|| "clip".into())) }, None, tr)?
        }
        Cmd::Get { pattern, meta } => {
            for e in c.get(&pattern, meta)? {
                if cli.json {
                    println!("{}", serde_json::to_string(&e)?);
                } else {
                    println!("{} = {}", e.address, e.value);
                }
            }
        }
        Cmd::Explain { address } => match c.explain(&address)? {
            Some(p) => {
                println!("{} = {}", p.address, p.value);
                for l in p.layers {
                    println!(
                        "  {} {:<9} {:<24} {}{}",
                        if l.active { "▸" } else { " " },
                        l.kind,
                        l.source,
                        l.value,
                        l.priority.map(|p| format!("  (priority {p})")).unwrap_or_default()
                    );
                }
            }
            None => bail!("unknown address {address}"),
        },
        Cmd::Trace { id } => {
            let id = if id == "last" {
                let v = c.query("trace.recent", Value::map().with("n", 200))?.map_err(|e| anyhow!(e))?;
                v.as_list()
                    .and_then(|l| {
                        l.iter().rev().find(|r| r.get_path("parent").is_none_or(Value::is_null) && r.get_path("kind").and_then(Value::as_str) != Some("change"))
                    })
                    .and_then(|r| r.get_path("id").and_then(Value::as_i64))
                    .ok_or_else(|| anyhow!("no trace records"))? as u64
            } else {
                id.parse()?
            };
            print_trace(&mut c, id)?
        }
        Cmd::Query { name, args } => {
            let args = match args {
                Some(a) => Value::from(serde_json::from_str::<serde_json::Value>(&a)?),
                None => Value::Null,
            };
            let v = c.query(&name, args)?.map_err(|e| anyhow!(e))?;
            print_value(&v, cli.json);
        }
        Cmd::Watch { events, state, signals, logs } => {
            c.set_timeout(None);
            c.subscribe(Subscription { events, state, signals, signal_hz: Some(10.0), logs, trace: false })?;
            loop {
                match c.recv()? {
                    ServerMsg::Event { event } => {
                        if cli.json {
                            println!("{}", serde_json::to_string(&event)?);
                        } else {
                            let who = event.actor.as_ref().map(|a| format!(" by {}", a.name)).unwrap_or_default();
                            println!("event {}{who} {}", event.ty, if event.payload.is_null() { String::new() } else { event.payload.to_string() });
                        }
                    }
                    ServerMsg::State { changes } => {
                        for (a, v) in changes {
                            println!("state {a} = {v}");
                        }
                    }
                    ServerMsg::Signals { values, .. } => {
                        println!("signals {}", values.iter().map(|(n, v)| format!("{n}={v:.3}")).collect::<Vec<_>>().join(" "));
                    }
                    ServerMsg::Log { level, target, msg, .. } => println!("log [{level}] {target}: {msg}"),
                    _ => {}
                }
            }
        }
        Cmd::Preflight => {
            let v = c.query("preflight", Value::Null)?.map_err(|e| anyhow!(e))?;
            if cli.json {
                print_value(&v, true);
            } else {
                let mut fails = 0;
                for item in v.as_list().unwrap_or(&[]) {
                    let st = item.get_path("status").and_then(Value::as_str).unwrap_or("?");
                    if st == "fail" {
                        fails += 1;
                    }
                    let mark = match st {
                        "pass" => "✓",
                        "warn" => "!",
                        "fail" => "✗",
                        _ => "?",
                    };
                    println!(
                        "{mark} {:<28} {}",
                        item.get_path("name").map(|v| v.to_string()).unwrap_or_default(),
                        item.get_path("detail").map(|v| v.to_string()).unwrap_or_default()
                    );
                }
                if fails > 0 {
                    std::process::exit(1);
                }
            }
        }
        Cmd::Status => {
            let info = c.query("engine.info", Value::Null)?.map_err(|e| anyhow!(e))?;
            let st = c.get("show.*", false)?;
            println!(
                "engine {} pid {} session {}",
                info.get_path("version").map(|v| v.to_string()).unwrap_or_default(),
                info.get_path("pid").map(|v| v.to_string()).unwrap_or_default(),
                info.get_path("session").map(|v| v.to_string()).unwrap_or_default()
            );
            println!("project {}", info.get_path("project").map(|v| v.to_string()).unwrap_or_default());
            for e in st {
                println!("{} = {}", e.address, e.value);
            }
        }
        Cmd::Token => unreachable!(),
    }
    Ok(())
}
