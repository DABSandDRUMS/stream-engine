//! `alerts/*.toml` (routing, variations, queue policy), `project.toml [goals.*]` and `[stats]`.
//!
//! ```toml
//! # alerts/queue.toml
//! [queue]
//! min_spacing   = "1s"        # gap between two alerts
//! max_on_screen = "15s"       # cap for any alert's duration
//! max_queue     = 200         # beyond this the lowest-priority queued alert is dropped
//! interrupt     = true        # big alerts may cut small ones short…
//! interrupt_margin = 30       # …when their priority is at least this much higher
//! on_interrupt  = "requeue"   # requeue | drop  (what happens to the interrupted alert)
//! pause_modes   = ["ad_break", "brb"]   # hold the queue in these modes, replay after
//! veto_window   = "3s"        # alerts with user text wait this long (unless already vetted by policy)
//! gift_window   = "3s"        # gifted subs within this window join their gift bomb
//! max_message   = 300         # user text cap (characters)
//!
//! # alerts/cheer.toml
//! [[alert]]
//! name     = "cheer"
//! when     = "twitch.cheer"
//! title    = "{user} cheered {amount} bits!"
//! message  = "{message}"
//! sound    = "cheer"
//! duration = "6s"
//! priority = 40
//! tts      = true
//! [[alert.variation]]
//! name     = "big"
//! if       = "amount >= 1000"
//! title    = "{user} cheered {amount} BITS!!"
//! sound    = "cheer_big"
//! duration = "10s"
//! priority = 70
//! ```

use se_core::config::Dur;
use se_expr::Expr;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnInterrupt {
    #[default]
    Requeue,
    Drop,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueuePolicy {
    pub min_spacing: Dur,
    pub max_on_screen: Dur,
    pub max_queue: usize,
    pub interrupt: bool,
    pub interrupt_margin: i64,
    pub on_interrupt: OnInterrupt,
    pub pause_modes: Vec<String>,
    pub veto_window: Dur,
    pub gift_window: Dur,
    pub max_message: usize,
}

impl Default for QueuePolicy {
    fn default() -> Self {
        QueuePolicy {
            min_spacing: Dur(1000),
            max_on_screen: Dur(15_000),
            max_queue: 200,
            interrupt: true,
            interrupt_margin: 30,
            on_interrupt: OnInterrupt::Requeue,
            pause_modes: vec!["ad_break".into(), "brb".into()],
            veto_window: Dur(3000),
            gift_window: Dur(3000),
            max_message: 300,
        }
    }
}

/// Fields shared by an alert and its variations (variations override what they set).
#[derive(Clone, Debug, PartialEq, Deserialize, Default)]
#[serde(default)]
pub struct Look {
    pub title: Option<String>,
    pub message: Option<String>,
    pub priority: Option<i64>,
    pub duration: Option<Dur>,
    pub sound: Option<String>,
    pub image: Option<String>,
    pub tts: Option<bool>,
    pub tts_text: Option<String>,
    pub voice: Option<String>,
    pub interrupt: Option<bool>,
    #[serde(rename = "do")]
    pub cmds: Option<Vec<String>>,
}

impl Look {
    fn over(&self, o: &Look) -> Look {
        Look {
            title: o.title.clone().or_else(|| self.title.clone()),
            message: o.message.clone().or_else(|| self.message.clone()),
            priority: o.priority.or(self.priority),
            duration: o.duration.or(self.duration),
            sound: o.sound.clone().or_else(|| self.sound.clone()),
            image: o.image.clone().or_else(|| self.image.clone()),
            tts: o.tts.or(self.tts),
            tts_text: o.tts_text.clone().or_else(|| self.tts_text.clone()),
            voice: o.voice.clone().or_else(|| self.voice.clone()),
            interrupt: o.interrupt.or(self.interrupt),
            cmds: o.cmds.clone().or_else(|| self.cmds.clone()),
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawVariation {
    name: String,
    #[serde(rename = "if")]
    cond: String,
    #[serde(flatten)]
    look: Look,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawAlert {
    name: String,
    when: String,
    #[serde(rename = "if")]
    cond: Option<String>,
    enabled: Option<bool>,
    veto: Option<bool>,
    /// Payload field holding viewer text (default `message`).
    user_text: Option<String>,
    variation: Vec<RawVariation>,
    #[serde(flatten)]
    look: Look,
}

#[derive(Clone, Debug)]
pub struct Variation {
    pub name: String,
    pub cond: Expr,
    pub look: Look,
}

#[derive(Clone, Debug)]
pub struct AlertDef {
    pub name: String,
    pub when: String,
    pub cond: Option<Expr>,
    pub enabled: bool,
    pub veto: bool,
    pub user_text: String,
    pub look: Look,
    pub variations: Vec<Variation>,
    pub file: String,
    pub index: usize,
}

/// The resolved look of one alert instance (after picking a variation).
#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    pub variation: String,
    pub title: String,
    pub message: String,
    pub priority: i64,
    pub duration_ms: u64,
    pub sound: Option<String>,
    pub image: Option<String>,
    pub tts: bool,
    pub tts_text: String,
    pub voice: Option<String>,
    pub interrupt: bool,
    pub cmds: Vec<String>,
}

impl AlertDef {
    /// Base look merged with the first variation whose condition holds.
    pub fn resolve(&self, holds: impl Fn(&Expr) -> bool) -> Resolved {
        let (vname, look) = match self.variations.iter().find(|v| holds(&v.cond)) {
            Some(v) => (v.name.clone(), self.look.over(&v.look)),
            None => (String::new(), self.look.clone()),
        };
        Resolved {
            variation: vname,
            title: look.title.unwrap_or_else(|| "{user}".into()),
            message: look.message.unwrap_or_else(|| "{message}".into()),
            priority: look.priority.unwrap_or(50),
            duration_ms: look.duration.map(Dur::ms).unwrap_or(6000),
            sound: look.sound.filter(|s| !s.is_empty()),
            image: look.image.filter(|s| !s.is_empty()),
            tts: look.tts.unwrap_or(false),
            tts_text: look.tts_text.unwrap_or_else(|| "{message}".into()),
            voice: look.voice.filter(|s| !s.is_empty()),
            interrupt: look.interrupt.unwrap_or(true),
            cmds: look.cmds.unwrap_or_default(),
        }
    }
}

/// Everything under `alerts/`.
#[derive(Clone, Debug, Default)]
pub struct AlertsConfig {
    pub queue: QueuePolicy,
    /// In file order (files sorted by name); the first matching alert handles an event.
    pub alerts: Vec<AlertDef>,
}

const LOOK_KEYS: &[&str] = &["title", "message", "priority", "duration", "sound", "image", "tts", "tts_text", "voice", "interrupt", "do"];
const ALERT_KEYS: &[&str] = &["name", "when", "if", "enabled", "veto", "user_text", "variation"];

/// serde can't deny unknown fields through `flatten`, so typos are caught here.
fn check_keys(v: &toml::Value, extra: &[&str], what: &str) -> Result<(), String> {
    if let Some(t) = v.as_table() {
        for k in t.keys() {
            if !LOOK_KEYS.contains(&k.as_str()) && !extra.contains(&k.as_str()) {
                return Err(format!("{what}: unknown key `{k}`"));
            }
        }
    }
    Ok(())
}

fn check_cmd(c: &str) -> Result<(), String> {
    se_proto::Op::parse(c).map(|_| ()).map_err(|e| format!("`{c}`: {e}"))
}

/// Parse one `alerts/<stem>.toml`: optional `[queue]` and `[[alert]]` entries.
pub fn parse_file(stem: &str, path: &str, t: &toml::Table) -> Result<(Option<QueuePolicy>, Vec<AlertDef>), String> {
    for k in t.keys() {
        if k != "queue" && k != "alert" {
            return Err(format!("unknown key `{k}` (expected [queue] / [[alert]])"));
        }
    }
    let queue = match t.get("queue") {
        Some(v) => Some(v.clone().try_into::<QueuePolicy>().map_err(|e| format!("[queue]: {}", e.message()))?),
        None => None,
    };
    let mut alerts = Vec::new();
    let arr = match t.get("alert") {
        None => Vec::new(),
        Some(toml::Value::Array(a)) => a.clone(),
        Some(_) => return Err("`alert` must be an array of tables ([[alert]])".into()),
    };
    for (i, v) in arr.into_iter().enumerate() {
        check_keys(&v, ALERT_KEYS, &format!("alert #{}", i + 1))?;
        if let Some(toml::Value::Array(vs)) = v.get("variation") {
            for (j, vv) in vs.iter().enumerate() {
                check_keys(vv, &["name", "if"], &format!("alert #{} variation #{}", i + 1, j + 1))?;
            }
        }
        let raw: RawAlert = v.try_into().map_err(|e: toml::de::Error| format!("alert #{}: {}", i + 1, e.message()))?;
        let name = if raw.name.is_empty() { format!("{stem}{}", if i == 0 { String::new() } else { format!("_{}", i + 1) }) } else { raw.name.clone() };
        let ctx = |m: String| format!("alert `{name}`: {m}");
        if raw.when.is_empty() {
            return Err(ctx("needs `when` (event pattern)".into()));
        }
        if !se_proto::address::is_valid(&raw.when, true) {
            return Err(ctx(format!("bad event pattern `{}`", raw.when)));
        }
        let cond = raw.cond.as_deref().map(Expr::parse).transpose().map_err(|e| ctx(format!("if: {e}")))?;
        for c in raw.look.cmds.iter().flatten() {
            check_cmd(c).map_err(&ctx)?;
        }
        let mut variations = Vec::new();
        for (j, rv) in raw.variation.into_iter().enumerate() {
            let vname = if rv.name.is_empty() { format!("v{}", j + 1) } else { rv.name };
            if rv.cond.trim().is_empty() {
                return Err(ctx(format!("variation `{vname}` needs `if`")));
            }
            let cond = Expr::parse(&rv.cond).map_err(|e| ctx(format!("variation `{vname}` if: {e}")))?;
            for c in rv.look.cmds.iter().flatten() {
                check_cmd(c).map_err(&ctx)?;
            }
            variations.push(Variation { name: vname, cond, look: rv.look });
        }
        alerts.push(AlertDef {
            name: se_bot::text::segment(&name),
            when: raw.when,
            cond,
            enabled: raw.enabled.unwrap_or(true),
            veto: raw.veto.unwrap_or(true),
            user_text: raw.user_text.unwrap_or_else(|| "message".into()),
            look: raw.look,
            variations,
            file: path.to_string(),
            index: i,
        });
    }
    Ok((queue, alerts))
}

/// What a goal counts.
#[derive(Clone, Debug)]
pub enum Counts {
    /// `follows | subs | sub_points | bits | tips | gifts | raids`
    Builtin(String),
    /// Any event pattern with an amount expression (`add = "event.cost"`).
    Custom { when: String, cond: Option<Expr>, add: Expr },
}

#[derive(Clone, Debug)]
pub struct GoalDef {
    pub name: String,
    pub label: String,
    pub target: f64,
    pub counts: Counts,
    /// Count simulator events (for testing overlays); default from `[stats] count_simulated`.
    pub count_simulated: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawGoal {
    label: Option<String>,
    target: f64,
    counts: Option<String>,
    when: Option<String>,
    #[serde(rename = "if")]
    cond: Option<String>,
    add: Option<String>,
    count_simulated: Option<bool>,
}

pub const GOAL_KINDS: &[&str] = &["follows", "subs", "sub_points", "bits", "tips", "gifts", "raids"];

/// `project.toml [goals.<name>]`.
pub fn parse_goals(v: Option<&toml::Value>) -> Result<Vec<GoalDef>, String> {
    let Some(v) = v else { return Ok(Vec::new()) };
    let t = v.as_table().ok_or("[goals] must be a table of [goals.<name>]")?;
    let mut out = Vec::new();
    for (name, gv) in t {
        if !se_proto::address::is_valid(name, false) || name.contains('.') {
            return Err(format!("goal name `{name}` must be one address segment"));
        }
        let raw: RawGoal = gv.clone().try_into().map_err(|e: toml::de::Error| format!("[goals.{name}]: {}", e.message()))?;
        let counts = match (&raw.counts, &raw.when) {
            (Some(c), None) if GOAL_KINDS.contains(&c.as_str()) => Counts::Builtin(c.clone()),
            (Some(c), None) => return Err(format!("[goals.{name}] counts = `{c}`; expected one of {}", GOAL_KINDS.join(", "))),
            (None, Some(w)) => Counts::Custom {
                when: w.clone(),
                cond: raw.cond.as_deref().map(Expr::parse).transpose().map_err(|e| format!("[goals.{name}] if: {e}"))?,
                add: Expr::parse(raw.add.as_deref().unwrap_or("1")).map_err(|e| format!("[goals.{name}] add: {e}"))?,
            },
            (None, None) if GOAL_KINDS.contains(&name.as_str()) => Counts::Builtin(name.clone()),
            _ => return Err(format!("[goals.{name}] needs `counts = \"…\"` or `when = \"event\"` (not both)")),
        };
        if raw.target <= 0.0 {
            return Err(format!("[goals.{name}] needs a positive `target`"));
        }
        out.push(GoalDef {
            name: name.clone(),
            label: raw.label.unwrap_or_else(|| name.replace('_', " ")),
            target: raw.target,
            counts,
            count_simulated: raw.count_simulated,
        });
    }
    Ok(out)
}

/// `project.toml [stats]`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatsSettings {
    /// Simulator events update stats and goals (marked so they can be purged later).
    pub count_simulated: bool,
    /// Never listed as top chatters (bots).
    pub ignore_chatters: Vec<String>,
    /// Leave the broadcaster out of top chatters.
    pub exclude_broadcaster: bool,
}

impl Default for StatsSettings {
    fn default() -> Self {
        StatsSettings {
            count_simulated: true,
            ignore_chatters: vec!["nightbot".into(), "streamelements".into(), "streamlabs".into(), "moobot".into(), "fossabot".into()],
            exclude_broadcaster: true,
        }
    }
}

impl StatsSettings {
    pub fn from_section(v: Option<&toml::Value>) -> Result<StatsSettings, String> {
        match v {
            None => Ok(StatsSettings::default()),
            Some(v) => v.clone().try_into().map_err(|e: toml::de::Error| format!("[stats]: {}", e.message())),
        }
    }
}

/// Build the alerts config from all `alerts/*` files, keeping the last good version of files
/// that fail. Returns errors per file.
pub fn build(
    files: &BTreeMap<String, (String, toml::Table)>,
    prev: &BTreeMap<String, (Option<QueuePolicy>, Vec<AlertDef>)>,
) -> (AlertsConfig, BTreeMap<String, (Option<QueuePolicy>, Vec<AlertDef>)>, Vec<(String, String)>) {
    let mut parsed = BTreeMap::new();
    let mut errs = Vec::new();
    for (stem, (path, table)) in files {
        match parse_file(stem, path, table) {
            Ok(p) => {
                parsed.insert(path.clone(), p);
            }
            Err(e) => {
                if let Some(old) = prev.get(path) {
                    parsed.insert(path.clone(), old.clone());
                }
                errs.push((path.clone(), e));
            }
        }
    }
    let mut cfg = AlertsConfig::default();
    let mut queue_from: Option<String> = None;
    for (path, (q, alerts)) in &parsed {
        if let Some(q) = q {
            if let Some(first) = &queue_from {
                errs.push((path.clone(), format!("[queue] is already set in {first}; ignoring this one")));
            } else {
                cfg.queue = q.clone();
                queue_from = Some(path.clone());
            }
        }
        cfg.alerts.extend(alerts.iter().cloned());
    }
    (cfg, parsed, errs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(s: &str) -> toml::Table {
        s.parse().unwrap()
    }

    #[test]
    fn parses_alerts_variations_and_queue() {
        let (q, a) = parse_file(
            "cheer",
            "alerts/cheer.toml",
            &table(
                r#"
[queue]
min_spacing = "500ms"
pause_modes = ["brb"]

[[alert]]
when = "twitch.cheer"
title = "{user} cheered {amount}"
sound = "cheer"
[[alert.variation]]
name = "big"
if = "amount >= 1000"
sound = "cheer_big"
priority = 70
"#,
            ),
        )
        .unwrap();
        let q = q.unwrap();
        assert_eq!(q.min_spacing.ms(), 500);
        assert_eq!(q.pause_modes, vec!["brb"]);
        assert_eq!(q.max_on_screen.ms(), 15_000, "defaults fill the rest");
        let d = &a[0];
        assert_eq!(d.name, "cheer");
        let small = d.resolve(|_| false);
        assert_eq!((small.variation.as_str(), small.sound.as_deref(), small.priority), ("", Some("cheer"), 50));
        let big = d.resolve(|_| true);
        assert_eq!((big.variation.as_str(), big.sound.as_deref(), big.priority), ("big", Some("cheer_big"), 70));
        assert_eq!(big.title, "{user} cheered {amount}", "unset fields come from the base");
    }

    #[test]
    fn rejects_bad_alerts() {
        assert!(parse_file("x", "alerts/x.toml", &table("[[alert]]\ntitle = \"t\"")).unwrap_err().contains("needs `when`"));
        assert!(parse_file("x", "alerts/x.toml", &table("[[alert]]\nwhen = \"a\"\nif = \"((\"")).is_err());
        assert!(parse_file("x", "alerts/x.toml", &table("[[alert]]\nwhen = \"a\"\n[[alert.variation]]\ntitle = \"x\"")).unwrap_err().contains("needs `if`"));
        assert!(parse_file("x", "alerts/x.toml", &table("[queue]\nmin_spacin = \"1s\"")).is_err());
        assert!(parse_file("x", "alerts/x.toml", &table("[[alert]]\nwhen = \"a\"\ndo = [\"set\"]")).is_err());
        assert!(parse_file("x", "alerts/x.toml", &table("[[alert]]\nwhen = \"a\"\ntitel = \"x\"")).unwrap_err().contains("unknown key `titel`"));
        assert!(parse_file("x", "alerts/x.toml", &table("[[alert]]\nwhen = \"a\"\n[[alert.variation]]\nif = \"true\"\nsond = \"x\"")).is_err());
    }

    #[test]
    fn goals_parse() {
        let v: toml::Value = toml::Value::Table(table(
            r#"
[subs]
target = 100
[bits]
label = "Bits for a new cymbal"
counts = "bits"
target = 10000
[hype]
when = "twitch.redeem"
if = "event.reward == 'HYPE'"
add = "1"
target = 20
"#,
        ));
        let g = parse_goals(Some(&v)).unwrap();
        assert_eq!(g.len(), 3);
        assert!(matches!(&g.iter().find(|x| x.name == "subs").unwrap().counts, Counts::Builtin(k) if k == "subs"));
        assert!(matches!(&g.iter().find(|x| x.name == "hype").unwrap().counts, Counts::Custom { .. }));
        let bad: toml::Value = toml::Value::Table(table("[x]\ntarget = 5"));
        assert!(parse_goals(Some(&bad)).is_err());
        let bad: toml::Value = toml::Value::Table(table("[subs]\ntarget = 0"));
        assert!(parse_goals(Some(&bad)).is_err());
    }
}
