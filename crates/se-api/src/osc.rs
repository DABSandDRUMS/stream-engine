//! OSC endpoint (§2.2, §10.1). Clients authenticate once with `/auth <token>`; the source
//! address stays authorized for an hour after its last message.
//!
//! | Address | Args | Effect |
//! |---|---|---|
//! | `/auth` | token | authorize this source |
//! | `/cmd` | text | any one-line command |
//! | `/set` | address, value | `set` |
//! | `/preset` | name | `preset.fire` |
//! | `/trigger`, `/release` | address | trigger / release |
//! | `/scene` | name | `scene.go`; `/take` = take |
//! | `/panic`, `/clean` | – | |
//! | `/event` | type, [key, value]… | emit an event |
//! | `/subscribe` | pattern | state changes fed back as `/<a>/<b> value` |
//! | `/subscribe/events` | pattern | events fed back as `/event/<a>/<b> [key, value]…` (the shape `/event` takes) |
//! | `/unsubscribe` | [pattern] | drop one state/event pattern, or all of them |
//! | anything else | one number | signal `osc.<path>`, plus event `osc.<path>` on a rising edge |

use crate::auth::{Auth, Scope};
use anyhow::Result;
use rosc::{OscMessage, OscPacket, OscType};
use se_hub::{Bus, Hub, any_match};
use se_proto::{Command, Event, Op, Origin, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

struct Peer {
    scope: Scope,
    seen: Instant,
    subs: Vec<String>,
    event_subs: Vec<String>,
}

/// An event as OSC: `/event/<type with dots as slashes>`, then the payload's fields as
/// key/value pairs (a non-map payload is one argument). Mirrors what `/event` accepts.
fn event_msg(e: &Event) -> OscMessage {
    let mut args = Vec::new();
    match &e.payload {
        Value::Map(m) => {
            for (k, v) in m {
                args.push(OscType::String(k.clone()));
                match to_osc(v).as_slice() {
                    [one] => args.push(one.clone()),
                    _ => args.push(OscType::String(v.to_string())),
                }
            }
        }
        Value::Null => {}
        other => args.extend(to_osc(other)),
    }
    OscMessage { addr: format!("/event/{}", e.ty.replace('.', "/")), args }
}

fn arg_value(a: &OscType) -> Value {
    match a {
        OscType::Int(i) => Value::Int(*i as i64),
        OscType::Long(i) => Value::Int(*i),
        OscType::Float(f) => Value::Float(*f as f64),
        OscType::Double(f) => Value::Float(*f),
        OscType::String(s) => Value::parse_text(s),
        OscType::Bool(b) => Value::Bool(*b),
        OscType::Nil | OscType::Inf => Value::Null,
        other => Value::Str(format!("{other:?}")),
    }
}

fn to_osc(v: &Value) -> Vec<OscType> {
    match v {
        Value::Null => vec![],
        Value::Bool(b) => vec![OscType::Bool(*b)],
        Value::Int(i) => vec![OscType::Int(*i as i32)],
        Value::Float(f) => vec![OscType::Float(*f as f32)],
        Value::Str(s) => vec![OscType::String(s.clone())],
        Value::List(l) => l.iter().flat_map(to_osc).collect(),
        Value::Map(_) => vec![OscType::String(v.to_string())],
    }
}

fn flatten(p: OscPacket, out: &mut Vec<OscMessage>) {
    match p {
        OscPacket::Message(m) => out.push(m),
        OscPacket::Bundle(b) => b.content.into_iter().for_each(|x| flatten(x, out)),
    }
}

pub async fn serve_osc(hub: Arc<Hub>, auth: Arc<Auth>, bind: SocketAddr) -> Result<()> {
    let sock = Arc::new(UdpSocket::bind(bind).await?);
    tracing::info!("api: osc on udp://{bind}");
    let peers: Arc<parking_lot::Mutex<HashMap<SocketAddr, Peer>>> = Default::default();
    // feedback task
    {
        let (sock, peers, hub) = (sock.clone(), peers.clone(), hub.clone());
        tokio::spawn(async move {
            let mut bus = hub.subscribe();
            while let Ok(b) = bus.recv().await {
                if let Bus::Event(e) = &*b {
                    let targets: Vec<SocketAddr> = peers.lock().iter().filter(|(_, p)| any_match(&p.event_subs, &e.ty)).map(|(a, _)| *a).collect();
                    if targets.is_empty() {
                        continue;
                    }
                    if let Ok(buf) = rosc::encoder::encode(&OscPacket::Message(event_msg(e))) {
                        for addr in targets {
                            let _ = sock.send_to(&buf, addr).await;
                        }
                    }
                    continue;
                }
                let Bus::Changes(changes) = &*b else { continue };
                let targets: Vec<(SocketAddr, Vec<String>)> =
                    peers.lock().iter().filter(|(_, p)| !p.subs.is_empty()).map(|(a, p)| (*a, p.subs.clone())).collect();
                for (addr, subs) in targets {
                    for (a, v) in changes.iter().filter(|(a, _)| any_match(&subs, a)) {
                        let msg = OscPacket::Message(OscMessage { addr: format!("/{}", a.replace('.', "/")), args: to_osc(v) });
                        if let Ok(buf) = rosc::encoder::encode(&msg) {
                            let _ = sock.send_to(&buf, addr).await;
                        }
                    }
                }
            }
        });
    }
    let mut edges: HashMap<String, bool> = HashMap::new();
    let mut buf = vec![0u8; 65536];
    loop {
        let (n, from) = sock.recv_from(&mut buf).await?;
        let Ok((_, pkt)) = rosc::decoder::decode_udp(&buf[..n]) else { continue };
        let mut msgs = Vec::new();
        flatten(pkt, &mut msgs);
        for m in msgs {
            if m.addr == "/auth" {
                let tok = match m.args.first() {
                    Some(OscType::String(s)) => s.clone(),
                    _ => String::new(),
                };
                let ok = match auth.check(&tok) {
                    Some(scope) => {
                        peers.lock().insert(from, Peer { scope, seen: Instant::now(), subs: Vec::new(), event_subs: Vec::new() });
                        true
                    }
                    None => false,
                };
                let reply = OscPacket::Message(OscMessage { addr: "/auth/result".into(), args: vec![OscType::Bool(ok)] });
                if let Ok(b) = rosc::encoder::encode(&reply) {
                    let _ = sock.send_to(&b, from).await;
                }
                continue;
            }
            let scope = {
                let mut p = peers.lock();
                p.retain(|_, x| x.seen.elapsed() < Duration::from_secs(3600));
                match p.get_mut(&from) {
                    Some(x) => {
                        x.seen = Instant::now();
                        x.scope.clone()
                    }
                    None => continue,
                }
            };
            let args: Vec<Value> = m.args.iter().map(arg_value).collect();
            let s = |i: usize| args.get(i).map(|v| v.to_string()).unwrap_or_default();
            let op = match m.addr.as_str() {
                "/cmd" => match Op::parse(&s(0)) {
                    Ok(op) => Some(op),
                    Err(e) => {
                        hub.log("warn", "osc", format!("osc /cmd: {e}"));
                        None
                    }
                },
                "/set" => Some(Op::Set { address: s(0), value: args.get(1).cloned().unwrap_or_default() }),
                "/preset" => Some(Op::PresetFire { name: s(0), payload: Value::Null }),
                "/trigger" => Some(Op::Trigger { address: s(0), payload: Value::Null }),
                "/release" => Some(Op::Release { address: s(0) }),
                "/scene" => Some(Op::SceneGo { scene: s(0) }),
                "/take" => Some(Op::SceneTake { transition: None, ms: None }),
                "/panic" => Some(Op::Panic),
                "/clean" => Some(Op::Clean),
                "/event" => {
                    let mut payload = Value::map();
                    for kv in args[1.min(args.len())..].chunks(2) {
                        if let [k, v] = kv {
                            payload = payload.with(k.to_string(), v.clone());
                        }
                    }
                    Some(Op::Emit { ty: s(0), payload })
                }
                "/subscribe" | "/subscribe/events" => {
                    let pat = s(0);
                    if se_proto::address::is_valid(&pat, true)
                        && let Some(p) = peers.lock().get_mut(&from)
                    {
                        let list = if m.addr == "/subscribe" { &mut p.subs } else { &mut p.event_subs };
                        if !list.contains(&pat) {
                            list.push(pat);
                        }
                    }
                    None
                }
                "/unsubscribe" => {
                    if let Some(p) = peers.lock().get_mut(&from) {
                        match args.first() {
                            Some(v) => {
                                let pat = v.to_string();
                                p.subs.retain(|x| *x != pat);
                                p.event_subs.retain(|x| *x != pat);
                            }
                            None => {
                                p.subs.clear();
                                p.event_subs.clear();
                            }
                        }
                    }
                    None
                }
                path => {
                    let name = format!("osc{}", path.replace('/', "."));
                    if !se_proto::address::is_valid(&name, false) || scope == Scope::ReadOnly {
                        continue;
                    }
                    if let Some(x) = args.first().and_then(Value::as_f32) {
                        hub.signal(&name, x);
                        let high = x >= 0.5;
                        let was = edges.insert(name.clone(), high).unwrap_or(false);
                        if high && !was {
                            hub.emit(Event::new(name, Origin::Osc, Value::map().with("value", x as f64)));
                        }
                    } else {
                        hub.emit(Event::new(name, Origin::Osc, Value::List(args.clone())));
                    }
                    None
                }
            };
            if let Some(op) = op {
                if scope.allows(&op) {
                    hub.command(Command::new(Origin::Osc, op));
                } else {
                    hub.log("warn", "osc", format!("osc: not permitted from {from}"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_go_out_in_the_shape_event_takes_in() {
        let e = Event::new("twitch.cheer", Origin::Sim, Value::map().with("bits", 1000).with("user", "drumfan42"));
        let m = event_msg(&e);
        assert_eq!(m.addr, "/event/twitch/cheer");
        assert_eq!(m.args.len(), 4);
        let pairs: Vec<(String, Value)> = m.args.chunks(2).map(|kv| (arg_value(&kv[0]).to_string(), arg_value(&kv[1]))).collect();
        assert!(pairs.contains(&("bits".into(), Value::Int(1000))));
        assert!(pairs.contains(&("user".into(), Value::Str("drumfan42".into()))));
        // nested values travel as one text argument so pairs stay aligned
        let nested = event_msg(&Event::new("x.y", Origin::Sim, Value::map().with("list", Value::List(vec![Value::Int(1), Value::Int(2)]))));
        assert_eq!(nested.args.len(), 2);
        assert!(event_msg(&Event::new("beat", Origin::Audio, Value::Null)).args.is_empty());
    }
}
