//! Small shared helpers: project-section config with last-good semantics, time formatting,
//! value conversion, and commands issued by extras.

use se_hub::EngineCtx;
use se_proto::{Command, Op, Origin, Value};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// A `[section]` of `project.toml`, parsed into `T`. A broken section is reported once per
/// change and the last good value is kept (§3.4).
pub struct Section<T> {
    key: &'static str,
    target: &'static str,
    pub value: T,
    last_raw: Option<toml::Value>,
    initialized: bool,
}

impl<T: DeserializeOwned + Default> Section<T> {
    pub fn new(key: &'static str, target: &'static str) -> Self {
        Section { key, target, value: T::default(), last_raw: None, initialized: false }
    }

    /// Re-read from the current config; returns true when the effective value changed.
    pub fn reload(&mut self, ctx: &EngineCtx) -> bool {
        let raw = ctx.project_section(self.key);
        if self.initialized && raw == self.last_raw {
            return false;
        }
        self.initialized = true;
        self.last_raw = raw.clone();
        match parse::<T>(raw) {
            Ok(v) => {
                self.value = v;
                true
            }
            Err(e) => {
                ctx.hub.log("error", self.target, format!("[{}] in project.toml: {e}; keeping the previous settings", self.key));
                false
            }
        }
    }
}

pub fn parse<T: DeserializeOwned + Default>(raw: Option<toml::Value>) -> Result<T, String> {
    match raw {
        None => Ok(T::default()),
        Some(v) => v.try_into::<T>().map_err(|e| e.message().to_string()),
    }
}

pub fn to_value<T: Serialize>(t: &T) -> Value {
    serde_json::to_value(t).ok().and_then(|j| serde_json::from_value(j).ok()).unwrap_or(Value::Null)
}

pub fn health(status: &str, detail: impl Into<String>) -> Value {
    Value::map().with("status", status).with("detail", detail.into())
}

pub fn action(name: &str, args: Value) -> Command {
    Command::new(Origin::System, Op::Action { name: name.into(), args })
}

/// Chat announcement through the chatbot (`bot.say {args: [text]}`).
pub fn bot_say(ctx: &EngineCtx, text: &str) {
    if !text.trim().is_empty() {
        ctx.hub.command(action("bot.say", Value::map().with("args", vec![Value::Str(text.to_string())])));
    }
}

/// `{name}` substitution; unknown names are left as-is. Values are inserted verbatim (chat
/// text stays data: the result is sent as one chat message, never parsed as a command).
pub fn template(tpl: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(tpl.len() + 32);
    let mut rest = tpl;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) => {
                let key = &after[..end];
                match vars.iter().find(|(k, _)| *k == key) {
                    Some((_, v)) => out.push_str(v),
                    None => {
                        out.push('{');
                        out.push_str(key);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// UTC civil date/time from unix seconds (Hinnant's days-from-civil inverse).
pub fn civil(unix: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400) as u32;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d, secs / 3600, secs / 60 % 60, secs % 60)
}

/// `20260925-213000` (UTC), sortable.
pub fn stamp(unix: i64) -> String {
    let (y, m, d, h, mi, s) = civil(unix);
    format!("{y:04}{m:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// Show modes during which heavy maintenance is deferred.
pub fn on_air(mode: &str) -> bool {
    matches!(mode, "preshow" | "live" | "brb" | "ad_break" | "outro")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil(951_782_400), (2000, 2, 29, 0, 0, 0));
        assert_eq!(civil(1_790_371_845), (2026, 9, 25, 21, 30, 45));
        assert_eq!(civil(-1), (1969, 12, 31, 23, 59, 59));
        assert_eq!(stamp(1_790_371_845), "20260925-213045");
    }

    #[test]
    fn templating() {
        assert_eq!(template("hi {user}, {x} {missing} {", &[("user", "bob"), ("x", "{user}")]), "hi bob, {user} {missing} {");
    }

    #[derive(serde::Deserialize, Default, Debug, PartialEq)]
    #[serde(default)]
    struct S {
        a: i64,
    }

    #[test]
    fn section_parse() {
        assert_eq!(parse::<S>(None).unwrap(), S::default());
        assert_eq!(parse::<S>(Some(toml::Value::Table("a = 3".parse().unwrap()))).unwrap(), S { a: 3 });
        assert!(parse::<S>(Some(toml::Value::Table("a = 'x'".parse().unwrap()))).is_err());
    }
}
