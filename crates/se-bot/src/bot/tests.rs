use super::*;
use se_store::Db;
use std::path::Path;

struct TEnv {
    mode: String,
    state: HashMap<String, Value>,
    now: i64,
}

impl Env for TEnv {
    fn state(&self, a: &str) -> Option<Value> {
        self.state.get(a).cloned()
    }
    fn mode(&self) -> String {
        self.mode.clone()
    }
    fn now_unix(&self) -> i64 {
        self.now
    }
}

fn env(mode: &str) -> TEnv {
    TEnv { mode: mode.into(), state: HashMap::new(), now: 1_790_000_000 }
}

fn actor(name: &str, role: Role) -> Actor {
    Actor { platform: "twitch".into(), id: format!("id-{name}"), name: name.into(), roles: vec![role] }
}

fn line(text: &str, who: &Actor) -> ChatLine {
    ChatLine { text: text.into(), message_id: Some(format!("m-{text}")), actor: who.clone(), event_id: Some(7) }
}

const PROJECT: &str = "schema = 1\n[policy]\nblocklist = [\"badword\"]\n";

const SOCIALS: &str = r#"# Social links (keep this comment!)
[[command]]
name = "!discord"
aliases = ["!dc"]
reply = "Join the Discord, {user}: https://discord.gg/xxxx"
cooldown = { global = "30s", per_user = "2m" }

[[command]]
name = "!hype"
role = "vip"
do = ["preset.fire hype"]
reply = "HYPE by {user}"
deny_reply = "@{user} !hype is for VIPs"

[[command]]
name = "!death"
reply = "Deaths: {count}"

[[command]]
name = "!pick"
reply = "{random:red|green|blue}"

[[command]]
name = "!echo"
reply = "{user} says {args}"
min_args = 1
usage = "Usage: !echo <text>"

[[command]]
name = "!up"
reply = "Live for {uptime} playing {song}; subs {goals.subs.current}/{goals.subs.target}"

[[command]]
name = "!sr"
action = "queue.request"
args = { user = "{user}", text = "{args}" }
min_args = 1

[[command]]
name = "!remove"
role = "mod"
action = "queue.remove"
args = { index = "{1}" }

[[command]]
name = "!songban"
role = "mod"
action = "queue.ban_song"
args = { id = "{1}" }

[[command]]
name = "!queue"
reply = "Queue: {queue.url}"
sub.open = { role = "mod", action = "queue.open", reply = "Requests are open!" }

[[timer]]
name = "sr"
every = "20m"
min_chat_lines = 3
reply = "Requests are open: !sr <song>"
"#;

const MOD: &str = r#"# Moderator tools
[[command]]
name = "!addcom"
role = "mod"
builtin = "addcom"
min_args = 2
usage = "Usage: !addcom !name reply"

[[command]]
name = "!editcom"
role = "mod"
builtin = "editcom"

[[command]]
name = "!delcom"
role = "mod"
builtin = "delcom"

[[command]]
name = "!quote"
builtin = "quote"

[[command]]
name = "!addquote"
role = "vip"
builtin = "addquote"

[[command]]
name = "!setcounter"
role = "mod"
builtin = "setcounter"

[[command]]
name = "!commands"
builtin = "commands"
"#;

fn write(root: &Path, rel: &str, s: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn load_cfg(root: &Path) -> se_core::Config {
    let p = Project::open(root).unwrap();
    let l = p.load();
    assert!(l.errors.is_empty(), "{:?}", l.errors);
    se_core::Config::build(&l.files)
}

struct Fx {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    bot: Bot,
}

fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    write(&root, "project.toml", PROJECT);
    write(&root, "commands/socials.toml", SOCIALS);
    write(&root, "commands/mod.toml", MOD);
    let mut bot = Bot::new(Store::new(Db::memory().unwrap()).unwrap(), Some(Project::open(&root).unwrap()), 3);
    let errs = bot.apply_config(&load_cfg(&root));
    assert!(errs.is_empty(), "{errs:?}");
    Fx { _dir: dir, root, bot }
}

fn says(out: &[Out]) -> Vec<String> {
    out.iter()
        .filter_map(|o| match o {
            Out::Say { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn cmds(out: &[Out]) -> Vec<Command> {
    out.iter()
        .filter_map(|o| match o {
            Out::Command(c) => Some(c.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn replies_with_templates_and_aliases() {
    let mut f = fixture();
    let ana = actor("ana", Role::Everyone);
    let e = env("live");
    assert_eq!(says(&f.bot.handle_chat(&line("!discord", &ana), &e, 1_000)), vec!["Join the Discord, ana: https://discord.gg/xxxx"]);
    // alias, other user (per-user cooldown is per user; the global one blocks everyone for 30 s)
    let bo = actor("bo", Role::Everyone);
    assert!(says(&f.bot.handle_chat(&line("!DC", &bo), &e, 2_000)).is_empty(), "global cooldown");
    assert_eq!(says(&f.bot.handle_chat(&line("!DC", &bo), &e, 31_001)).len(), 1);
    // non-commands and unknown commands are ignored
    assert!(f.bot.handle_chat(&line("hello !discord", &ana), &e, 100_000).is_empty());
    assert!(f.bot.handle_chat(&line("!join", &ana), &e, 100_000).is_empty());
}

#[test]
fn per_user_cooldown_and_mod_bypass() {
    let mut f = fixture();
    let e = env("live");
    let ana = actor("ana", Role::Everyone);
    assert_eq!(says(&f.bot.handle_chat(&line("!discord", &ana), &e, 0)).len(), 1);
    // global cooldown over, but ana's per-user 2 min isn't
    assert!(says(&f.bot.handle_chat(&line("!discord", &ana), &e, 60_000)).is_empty());
    let bo = actor("bo", Role::Everyone);
    assert_eq!(says(&f.bot.handle_chat(&line("!discord", &bo), &e, 60_000)).len(), 1);
    assert_eq!(says(&f.bot.handle_chat(&line("!discord", &ana), &e, 120_001)).len(), 1);
    // mods skip cooldowns entirely
    let m = actor("modder", Role::Mod);
    assert_eq!(says(&f.bot.handle_chat(&line("!discord", &m), &e, 120_002)).len(), 1);
    assert_eq!(says(&f.bot.handle_chat(&line("!discord", &m), &e, 120_003)).len(), 1);
}

#[test]
fn role_gate_runs_preset_for_vips_only() {
    let mut f = fixture();
    let e = env("live");
    let pleb = actor("pleb", Role::Sub);
    let out = f.bot.handle_chat(&line("!hype", &pleb), &e, 0);
    assert!(cmds(&out).is_empty());
    assert_eq!(says(&out), vec!["@pleb !hype is for VIPs"]);
    let vip = actor("vippy", Role::Vip);
    let out = f.bot.handle_chat(&line("!hype", &vip), &e, 10);
    let c = cmds(&out);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].op, Op::PresetFire { name: "hype".into(), payload: Value::Null });
    assert_eq!(c[0].origin, Origin::Chat);
    assert_eq!(c[0].actor.as_ref().unwrap().name, "vippy");
    assert_eq!(c[0].causal, Some(7));
    assert_eq!(c[0].priority(), se_proto::PRIORITY_CHAT);
    assert_eq!(says(&out), vec!["HYPE by vippy"]);
    // the broadcaster's own commands run at preset priority
    let owner = actor("me", Role::Owner);
    let c = cmds(&f.bot.handle_chat(&line("!hype", &owner), &e, 20));
    assert_eq!(c[0].priority(), PRIORITY_PRESET);
}

#[test]
fn counters_random_usage_and_state_placeholders() {
    let mut f = fixture();
    let mut e = env("live");
    let ana = actor("ana", Role::Everyone);
    let out = f.bot.handle_chat(&line("!death", &ana), &e, 0);
    assert_eq!(says(&out), vec!["Deaths: 1"]);
    assert!(out.contains(&Out::Counter { name: "!death".trim_start_matches('!').into(), value: Some(1) }));
    assert_eq!(says(&f.bot.handle_chat(&line("!death", &ana), &e, 1)), vec!["Deaths: 2"]);
    let pick = says(&f.bot.handle_chat(&line("!pick", &ana), &e, 2));
    assert!(["red", "green", "blue"].contains(&pick[0].as_str()));
    assert_eq!(says(&f.bot.handle_chat(&line("!echo", &ana), &e, 3)), vec!["Usage: !echo <text>"]);
    assert_eq!(says(&f.bot.handle_chat(&line("!echo hi {user} {random:x|y}", &ana), &e, 4)), vec!["ana says hi {user} {random:x|y}"]);
    // uptime from twitch state, song, and arbitrary state (floats print like ints)
    e.state.insert("twitch.stream.live".into(), Value::Bool(true));
    e.state.insert("twitch.stream.started_at".into(), Value::Int(e.now - 3 * 3600 - 120));
    e.state.insert("queue.now.title".into(), "Rosanna".into());
    e.state.insert("goals.subs.current".into(), Value::Float(12.0));
    e.state.insert("goals.subs.target".into(), Value::Int(50));
    assert_eq!(says(&f.bot.handle_chat(&line("!up", &ana), &e, 5)), vec!["Live for 3h 2m playing Rosanna; subs 12/50"]);
    // offline with no twitch state
    let e2 = env("offline");
    assert_eq!(
        says(&f.bot.handle_chat(&line("!up", &ana), &e2, 6)),
        vec!["Live for offline playing nothing playing; subs {goals.subs.current}/{goals.subs.target}"]
    );
    // uptime falls back to the time since the show went live
    f.bot.on_mode("live", e2.now - 90);
    let e3 = env("live");
    assert!(says(&f.bot.handle_chat(&line("!up", &ana), &TEnv { now: e2.now, ..e3 }, 7))[0].starts_with("Live for 1m"));
}

#[test]
fn song_commands_map_to_queue_actions() {
    let mut f = fixture();
    let e = env("live");
    let ana = actor("ana", Role::Follower);
    let c = cmds(&f.bot.handle_chat(&line("!sr never gonna give you up", &ana), &e, 0));
    assert_eq!(c[0].op, Op::Action { name: "queue.request".into(), args: Value::map().with("user", "ana").with("text", "never gonna give you up") });
    // numbers stay strings unless the arg is a bare positional
    let c = cmds(&f.bot.handle_chat(&line("!sr 1999", &ana), &e, 1));
    assert_eq!(c[0].op, Op::Action { name: "queue.request".into(), args: Value::map().with("user", "ana").with("text", "1999") });
    let m = actor("modder", Role::Mod);
    let c = cmds(&f.bot.handle_chat(&line("!remove #3", &m), &e, 2));
    assert_eq!(c[0].op, Op::Action { name: "queue.remove".into(), args: Value::map().with("index", 3) });
    // empty positional → key omitted (ban the current song)
    let c = cmds(&f.bot.handle_chat(&line("!songban", &m), &e, 3));
    assert_eq!(c[0].op, Op::Action { name: "queue.ban_song".into(), args: Value::map() });
    // viewers can't moderate
    assert!(cmds(&f.bot.handle_chat(&line("!remove 3", &ana), &e, 4)).is_empty());
    // subcommands: `!queue open` is mod-only, plain `!queue` replies
    assert!(cmds(&f.bot.handle_chat(&line("!queue open", &ana), &e, 5)).is_empty());
    let out = f.bot.handle_chat(&line("!queue open", &m), &e, 6);
    assert_eq!(cmds(&out)[0].op, Op::Action { name: "queue.open".into(), args: Value::map() });
    assert_eq!(says(&out), vec!["Requests are open!"]);
    let mut e2 = env("live");
    e2.state.insert("queue.url".into(), "https://example.com/queue".into());
    assert_eq!(says(&f.bot.handle_chat(&line("!queue", &ana), &e2, 7)), vec!["Queue: https://example.com/queue"]);
}

#[test]
fn content_filter_withholds_user_text() {
    let mut f = fixture();
    let e = env("live");
    let ana = actor("ana", Role::Everyone);
    let out = f.bot.handle_chat(&line("!echo you badword", &ana), &e, 0);
    assert!(says(&out).is_empty(), "{out:?}");
    assert!(out.iter().any(|o| matches!(o, Out::Log { msg, .. } if msg.contains("content filter"))));
    let out = f.bot.handle_chat(&line("!sr badword song", &ana), &e, 1);
    assert!(cmds(&out).is_empty());
}

#[test]
fn echo_of_own_reply_is_ignored() {
    let mut f = fixture();
    let e = env("live");
    let me = actor("me", Role::Owner);
    let out = f.bot.handle_chat(&line("!addcom !loop !loop", &me), &e, 0);
    assert_eq!(says(&out), vec!["@me added !loop"]);
    // the new command replies "!loop", guarded so it can't re-trigger
    let r = says(&f.bot.handle_chat(&line("!loop", &me), &e, 10));
    assert_eq!(r.len(), 1);
    assert!(r[0].starts_with(text::ZWSP));
    // Twitch echoes the bot's message back as a chat line from the broadcaster
    assert!(f.bot.handle_chat(&line(&r[0], &me), &e, 20).is_empty());
    // even without the guard char (same text) within the echo window
    let hi = f.bot.outgoing("thanks for the follow!", 30).unwrap();
    assert!(f.bot.handle_chat(&line(&hi, &me), &e, 40).is_empty());
}

#[test]
fn addcom_editcom_delcom_write_back_with_comments() {
    let mut f = fixture();
    let e = env("live");
    let m = actor("modder", Role::Mod);
    let pleb = actor("pleb", Role::Everyone);
    // viewers can't
    assert!(f.bot.handle_chat(&line("!addcom !x y", &pleb), &e, 0).is_empty());
    assert!(!f.root.join("commands/custom.toml").exists());
    assert_eq!(says(&f.bot.handle_chat(&line("!addcom !merch Grab a shirt at {random:shop|store}.example", &m), &e, 1)), vec!["@modder added !merch"]);
    let custom = std::fs::read_to_string(f.root.join("commands/custom.toml")).unwrap();
    assert!(custom.contains("# added from chat by modder"), "{custom}");
    assert!(custom.contains("name = \"!merch\""));
    // live immediately
    let r = says(&f.bot.handle_chat(&line("!merch", &pleb), &e, 2));
    assert!(r[0] == "Grab a shirt at shop.example" || r[0] == "Grab a shirt at store.example");
    // duplicates and bad names are refused
    assert_eq!(says(&f.bot.handle_chat(&line("!addcom !discord spam", &m), &e, 3)), vec!["@modder !discord already exists"]);
    assert!(says(&f.bot.handle_chat(&line("!addcom merch2 x", &m), &e, 4))[0].contains("names look like"));
    // edit a command in socials.toml: comments survive
    f.bot.handle_chat(&line("!editcom !discord New invite: https://discord.gg/yyyy", &m), &e, 5);
    let socials = std::fs::read_to_string(f.root.join("commands/socials.toml")).unwrap();
    assert!(socials.starts_with("# Social links (keep this comment!)"));
    assert!(socials.contains("reply = \"New invite: https://discord.gg/yyyy\""));
    assert!(socials.contains("aliases = [\"!dc\"]"));
    assert_eq!(says(&f.bot.handle_chat(&line("!discord", &pleb), &e, 6)), vec!["New invite: https://discord.gg/yyyy"]);
    // non-reply commands are protected
    assert!(says(&f.bot.handle_chat(&line("!delcom !hype", &m), &e, 7))[0].contains("isn't a plain reply command"));
    assert!(says(&f.bot.handle_chat(&line("!editcom !addcom lol", &m), &e, 8))[0].contains("isn't a plain reply command"));
    // delete from the custom file
    assert_eq!(says(&f.bot.handle_chat(&line("!delcom !merch", &m), &e, 9)), vec!["@modder deleted !merch"]);
    assert!(f.bot.handle_chat(&line("!merch", &pleb), &e, 10).is_empty());
    assert!(std::fs::read_to_string(f.root.join("commands/custom.toml")).unwrap().find("!merch").is_none());
    // a reload from disk agrees with memory
    let errs = f.bot.apply_config(&load_cfg(&f.root));
    assert!(errs.is_empty(), "{errs:?}");
    assert_eq!(says(&f.bot.handle_chat(&line("!discord", &pleb), &e, 200_000)), vec!["New invite: https://discord.gg/yyyy"]);
}

#[test]
fn quotes_counters_and_command_list() {
    let mut f = fixture();
    let e = env("live");
    let vip = actor("vippy", Role::Vip);
    let pleb = actor("pleb", Role::Everyone);
    assert_eq!(says(&f.bot.handle_chat(&line("!quote", &pleb), &e, 0)), vec!["@pleb no quote found"]);
    assert_eq!(says(&f.bot.handle_chat(&line("!addquote more cowbell", &vip), &e, 1)), vec!["@vippy added quote #1"]);
    let q = says(&f.bot.handle_chat(&line("!quote 1", &pleb), &e, 2));
    assert!(q[0].starts_with("Quote #1: more cowbell ("), "{q:?}");
    assert!(says(&f.bot.handle_chat(&line("!quote cowbell", &pleb), &e, 3))[0].starts_with("Quote #1"));
    let m = actor("modder", Role::Mod);
    let out = f.bot.handle_chat(&line("!setcounter death 41", &m), &e, 4);
    assert_eq!(says(&out), vec!["death = 41"]);
    assert_eq!(says(&f.bot.handle_chat(&line("!death", &pleb), &e, 5)), vec!["Deaths: 42"]);
    assert_eq!(says(&f.bot.handle_chat(&line("!setcounter death -2", &m), &e, 6)), vec!["death = 40"]);
    let list = says(&f.bot.handle_chat(&line("!commands", &pleb), &e, 7));
    assert!(list[0].contains("!discord") && !list[0].contains("!addcom") && !list[0].contains("!hype"), "{list:?}");
}

#[test]
fn timers_need_interval_chat_activity_and_mode() {
    let mut f = fixture();
    let live = env("live");
    let off = env("offline");
    let ana = actor("ana", Role::Everyone);
    // offline: never, even with chatter
    for i in 0..5 {
        f.bot.handle_chat(&line(&format!("hi {i}"), &ana), &off, i);
    }
    assert!(f.bot.tick(&off, 2_000_000).is_empty());
    // live from t=2_000_000: first run one interval after entering the mode
    assert!(f.bot.tick(&live, 2_000_000).is_empty());
    assert!(f.bot.tick(&live, 2_000_000 + 1_199_000).is_empty());
    let out = f.bot.tick(&live, 2_000_000 + 1_200_000);
    assert_eq!(says(&out), vec!["Requests are open: !sr <song>"]);
    // next interval passed but chat was quiet: wait for 3 lines
    let t = 2_000_000 + 2_400_000;
    assert!(f.bot.tick(&live, t).is_empty());
    for i in 0..3 {
        f.bot.handle_chat(&line(&format!("msg {i}"), &ana), &live, t + i);
    }
    assert_eq!(says(&f.bot.tick(&live, t + 10)).len(), 1);
    // the bot's own timer message doesn't count as chat activity
    let echo = "Requests are open: !sr <song>";
    f.bot.handle_chat(&line(echo, &actor("me", Role::Owner)), &live, t + 20);
    assert!(f.bot.tick(&live, t + 10 + 1_200_000).is_empty());
}

#[test]
fn ui_save_and_delete_entries_validate_first() {
    let mut f = fixture();
    let e = env("live");
    let fields = Value::map().with("name", "!tip").with("reply", "Tip jar: https://ko-fi.com/x");
    let file = f.bot.save_entry("command", None, None, &fields).unwrap();
    assert_eq!(file, "commands/custom.toml");
    let pleb = actor("pleb", Role::Everyone);
    assert_eq!(says(&f.bot.handle_chat(&line("!tip", &pleb), &e, 0)).len(), 1);
    // invalid role is rejected before anything is written
    let before = std::fs::read_to_string(f.root.join("commands/custom.toml")).unwrap();
    assert!(f.bot.save_entry("command", Some("commands/custom.toml"), Some(0), &Value::map().with("role", "king")).is_err());
    assert_eq!(std::fs::read_to_string(f.root.join("commands/custom.toml")).unwrap(), before);
    // duplicate names across files are rejected
    assert!(f.bot.save_entry("command", None, None, &Value::map().with("name", "!discord").with("reply", "x")).is_err());
    // paths outside commands/ are refused
    assert!(f.bot.save_entry("command", Some("../project.toml"), None, &fields).is_err());
    // timers
    f.bot.save_entry("timer", Some("commands/custom.toml"), None, &Value::map().with("name", "tipjar").with("every", "30m").with("reply", "tip!")).unwrap();
    assert!(f.bot.query_timers(0).as_list().unwrap().iter().any(|t| t.get_path("name").and_then(Value::as_str) == Some("tipjar")));
    f.bot.delete_entry("command", "commands/custom.toml", 0).unwrap();
    assert!(f.bot.handle_chat(&line("!tip", &pleb), &e, 1).is_empty());
}
