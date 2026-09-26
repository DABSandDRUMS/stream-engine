//! Policy pipeline (§12.1) end to end through the core: managed rewards with refunds, the
//! approval queue, the veto window, deletion sync, chat filtering, and ad breaks.

use se_core::config::SourceFile;
use se_core::{Config, Core, Input, Output};
use se_proto::{Actor, Command, Event, Op, Origin, Role, Value};

const MS: u64 = 1_000_000;

fn file(kind: &str, name: &str, src: &str) -> SourceFile {
    SourceFile { kind: kind.into(), name: name.into(), path: format!("{kind}/{name}.toml"), table: toml::from_str(src).unwrap() }
}

fn core() -> Core {
    let files = vec![
        file("project", "project", "schema = 1\n[policy]\nblocklist = [\"badword\"]\n[policy.veto]\nms = \"3s\"\nmin_bits = 500"),
        file("presets", "hype", "hold = \"8s\"\nset = { \"fx.rgb_split.amount\" = 0.8 }"),
        file("presets", "confetti", "hold = \"2s\"\nset = { \"fx.confetti.amount\" = 1.0 }"),
        file(
            "rewards",
            "hype",
            "title = \"HYPE\"\ncost = 2000\ncooldown = \"5m\"\nmax_per_user_per_stream = 3\nfires = \"preset.hype\"\non_reject = \"refund\"",
        ),
        file(
            "rules",
            "chat",
            "[[rule]]\nname = \"big\"\nwhen = \"twitch.chat\"\nif = \"event.message == '!big'\"\napproval = true\ndo = [\"preset.fire confetti\"]\n[[rule]]\nname = \"vip\"\nwhen = \"twitch.chat\"\nif = \"event.message == '!vip'\"\nrole = \"vip\"\ndo = [\"preset.fire confetti\"]",
        ),
    ];
    let c = Config::build(&files);
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    Core::new(c, 1_000 * MS)
}

fn run(c: &mut Core, ms: u64) -> Vec<Output> {
    let mut out = Vec::new();
    for _ in 0..(ms * MS).div_ceil(c.period()) {
        c.step();
        out.extend(c.drain_outputs());
    }
    out
}

fn viewer(id: &str, roles: Vec<Role>) -> Actor {
    Actor { platform: "twitch".into(), id: id.into(), name: format!("viewer{id}"), roles }
}

fn ev(ty: &str, payload: Value, actor: Option<Actor>) -> Input {
    let mut e = Event::new(ty, Origin::Twitch, payload);
    e.actor = actor;
    Input::Event { event: e }
}

fn redeem(red: &str, user: &str) -> Input {
    ev(
        "twitch.redeem",
        Value::map()
            .with("reward", "HYPE")
            .with("reward_key", "hype")
            .with("reward_id", "rw")
            .with("redemption_id", red)
            .with("cost", 2000)
            .with("input", "")
            .with("status", "unfulfilled")
            .with("user", user),
        Some(viewer(user, vec![Role::Follower])),
    )
}

fn ui(op: Op) -> Input {
    Input::Command { cmd: Command::new(Origin::Ui, op) }
}

fn actions(out: &[Output]) -> Vec<(String, Value)> {
    out.iter()
        .filter_map(|o| match o {
            Output::Action(c) => match &c.op {
                Op::Action { name, args } => Some((name.clone(), args.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn events(out: &[Output]) -> Vec<Event> {
    out.iter()
        .filter_map(|o| match o {
            Output::Event(e) => Some(e.clone()),
            _ => None,
        })
        .collect()
}

fn live(c: &mut Core) {
    c.submit(ui(Op::ModeSet { mode: "live".into() }));
    run(c, 5);
}

#[test]
fn redeem_fires_preset_then_cooldown_rejection_is_refunded() {
    let mut c = core();
    live(&mut c);
    c.submit(redeem("r1", "a"));
    let out = run(&mut c, 10);
    assert!(c.get("preset.hype.active").unwrap().truthy(), "reward fires its preset");
    let acts = actions(&out);
    assert!(acts.iter().any(|(n, a)| n == "twitch.fulfill" && a.get_path("redemption_id").unwrap().as_str() == Some("r1")), "{acts:?}");
    assert!(events(&out).iter().any(|e| e.ty == "policy.accepted"));
    // second viewer inside the 5 min cooldown → rejected + refunded, no redeem event published
    c.submit(redeem("r2", "b"));
    let out = run(&mut c, 10);
    let acts = actions(&out);
    let refund = acts.iter().find(|(n, _)| n == "twitch.refund").expect("refund");
    assert_eq!(refund.1.get_path("redemption_id").unwrap().as_str(), Some("r2"));
    let evs = events(&out);
    assert!(!evs.iter().any(|e| e.ty == "twitch.redeem"), "rejected redemptions never reach rules/overlays");
    let rej = evs.iter().find(|e| e.ty == "policy.rejected").unwrap();
    assert_eq!(rej.payload.get_path("refunded"), Some(&Value::Bool(true)));
}

#[test]
fn redeem_outside_live_is_refunded() {
    let mut c = core();
    c.submit(ui(Op::ModeSet { mode: "brb".into() }));
    c.submit(redeem("r1", "a"));
    let out = run(&mut c, 10);
    assert!(!c.get("preset.hype.active").unwrap().truthy());
    assert!(actions(&out).iter().any(|(n, _)| n == "twitch.refund"));
}

#[test]
fn ad_break_switches_mode_and_returns() {
    let mut c = core();
    live(&mut c);
    c.submit(ev("twitch.ad_break", Value::map().with("duration", 2).with("automatic", false), None));
    run(&mut c, 10);
    assert_eq!(c.mode_str(), "ad_break");
    run(&mut c, 2_100);
    assert_eq!(c.mode_str(), "live");
}

#[test]
fn approval_queue_state_and_mod_approve() {
    let mut c = core();
    live(&mut c);
    c.submit(ev("twitch.chat", Value::map().with("message", "!big").with("message_id", "m1").with("user", "a"), Some(viewer("a", vec![]))));
    run(&mut c, 10);
    assert!(!c.get("preset.confetti.active").unwrap().truthy(), "waits for a mod");
    assert_eq!(c.get("policy.pending.count"), Some(&Value::Int(1)));
    let id = c.get("policy.pending").unwrap().get_path("0.id").unwrap().as_str().unwrap().to_string();
    // a viewer can't approve their own request
    let mut cmd = Command::new(Origin::Chat, Op::Action { name: "mod.approve".into(), args: Value::map().with("id", id.clone()) });
    cmd.actor = Some(viewer("a", vec![]));
    c.submit(Input::Command { cmd });
    let out = run(&mut c, 5);
    assert!(out.iter().any(|o| matches!(o, Output::Ack { ok: false, error: Some(e), .. } if e.contains("moderator"))));
    let mut cmd = Command::new(Origin::Chat, Op::Action { name: "mod.approve".into(), args: Value::map().with("id", id) });
    cmd.actor = Some(viewer("m", vec![Role::Mod]));
    c.submit(Input::Command { cmd });
    run(&mut c, 5);
    assert!(c.get("preset.confetti.active").unwrap().truthy());
    assert_eq!(c.get("policy.pending.count"), Some(&Value::Int(0)));
}

#[test]
fn rule_role_gate() {
    let mut c = core();
    live(&mut c);
    c.submit(ev("twitch.chat", Value::map().with("message", "!vip").with("user", "a"), Some(viewer("a", vec![Role::Sub]))));
    run(&mut c, 10);
    assert!(!c.get("preset.confetti.active").unwrap().truthy());
    c.submit(ev("twitch.chat", Value::map().with("message", "!vip").with("user", "b"), Some(viewer("b", vec![Role::Vip]))));
    run(&mut c, 10);
    assert!(c.get("preset.confetti.active").unwrap().truthy());
}

#[test]
fn blocked_chat_never_published_and_veto_released_after_window() {
    let mut c = core();
    live(&mut c);
    c.submit(ev("twitch.chat", Value::map().with("message", "B4DW0RD lol").with("message_id", "m9").with("user", "a"), Some(viewer("a", vec![]))));
    let evs = events(&run(&mut c, 10));
    assert!(!evs.iter().any(|e| e.ty == "twitch.chat"));
    assert!(evs.iter().any(|e| e.ty == "policy.filtered" && e.payload.get_path("message_id").and_then(Value::as_str) == Some("m9")));
    c.submit(ev("twitch.cheer", Value::map().with("bits", 1000).with("message", "Cheer1000 hello stream").with("user", "a"), Some(viewer("a", vec![]))));
    let evs = events(&run(&mut c, 100));
    assert!(!evs.iter().any(|e| e.ty == "twitch.cheer"), "held in the veto window");
    let evs = events(&run(&mut c, 3_000));
    let cheer = evs.iter().find(|e| e.ty == "twitch.cheer").expect("released");
    assert_eq!(cheer.payload.get_path("vetted"), Some(&Value::Bool(true)));
}

#[test]
fn viewers_cannot_moderate_directly() {
    let mut c = core();
    let mut cmd = Command::new(Origin::Chat, Op::Action { name: "mod.ban".into(), args: Value::map().with("user", "x") });
    cmd.actor = Some(viewer("a", vec![Role::Vip]));
    c.submit(Input::Command { cmd });
    let out = run(&mut c, 5);
    assert!(out.iter().any(|o| matches!(o, Output::Ack { ok: false, .. })));
    assert!(actions(&out).is_empty());
}
