//! "Fix with AI": a redacted diagnostic bundle (markdown) in the user's runtime directory, then
//! a diagnosis session in a terminal: `omp` in the Stream Engine folder with the
//! `packaging/omp-diagnose.{yml,md}` overlay, which asks before every change. Opening a session
//! never runs anything in the engine.

use crate::health::{self, Check};
use crate::model::Model;
use se_proto::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

/// Warn/error log lines included in a bundle.
pub const LOG_LINES: usize = 150;

const REDACTED: &str = "[REDACTED]";

// ---- redaction --------------------------------------------------------------------------------

/// Characters of a `key` in `key=value` / `key: value` / `"key": "value"`.
fn key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
}

/// Keys whose value is a secret (matched on the lowercased key's ending).
const SECRET_KEY_ENDINGS: &[&str] =
    &["token", "key", "secret", "password", "passwd", "sid", "sidts", "sidcc", "credential", "credentials", "oauth", "signature", "sig"];
/// Keys whose whole remaining line is secret (headers carry several values).
const SECRET_LINE_KEYS: &[&str] = &["authorization", "proxy-authorization", "cookie", "set-cookie"];

fn secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    SECRET_KEY_ENDINGS.iter().any(|e| k.ends_with(e))
}

/// End of a value starting at `i` (stops at whitespace, quotes, separators).
fn value_end(c: &[char], mut i: usize) -> usize {
    while i < c.len() && !c[i].is_whitespace() && !matches!(c[i], '"' | '\'' | '&' | ',' | ';' | ')' | ']' | '}' | '<' | '>') {
        i += 1;
    }
    i
}

/// Characters of a token-like run checked for blobs.
fn blob_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-' | '.')
}

/// Looks like a credential or opaque secret: OAuth/refresh/JWT shapes, a Twitch-style token, or
/// a long hex/base64 blob.
fn secret_blob(run: &str) -> bool {
    if run.starts_with("ya29.") || run.starts_with("1//") || (run.starts_with("eyJ") && run.len() >= 20) {
        return true;
    }
    let core = run.trim_matches(|c| c == '.' || c == '=');
    let alnum = |c: char| c.is_ascii_alphanumeric();
    let digit = core.chars().any(|c| c.is_ascii_digit());
    let lower = core.chars().any(|c| c.is_ascii_lowercase());
    let upper = core.chars().any(|c| c.is_ascii_uppercase());
    // Twitch access/refresh tokens: 30 lowercase letters and digits
    if core.len() == 30 && core.chars().all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()) && digit && lower {
        return true;
    }
    if core.len() < 32 {
        return false;
    }
    let hex = core.chars().all(|c| c.is_ascii_hexdigit()) && digit;
    let base64 = core.chars().all(|c| alnum(c) || matches!(c, '+' | '/' | '=' | '_' | '-')) && digit && lower && upper;
    hex || base64
}

/// Second pass: blobs (paths are checked per segment so ordinary paths survive).
fn redact_blobs(line: &str) -> String {
    let c: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < c.len() {
        if !blob_char(c[i]) {
            out.push(c[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < c.len() && blob_char(c[i]) {
            i += 1;
        }
        let run: String = c[start..i].iter().collect();
        if run.contains('/') && !run.starts_with("1//") {
            let parts: Vec<String> =
                run.split('/').map(|seg| if seg.split('.').any(secret_blob) || secret_blob(seg) { REDACTED.to_string() } else { seg.to_string() }).collect();
            out.push_str(&parts.join("/"));
        } else if secret_blob(&run) {
            out.push_str(REDACTED);
        } else {
            out.push_str(&run);
        }
    }
    out
}

fn redact_line(line: &str) -> String {
    let c: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < c.len() {
        if !key_char(c[i]) {
            out.push(c[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < c.len() && key_char(c[i]) {
            i += 1;
        }
        let word: String = c[start..i].iter().collect();
        out.push_str(&word);
        let lower = word.to_ascii_lowercase();
        // key=value, key: value, "key": "value"
        let mut j = i;
        if j < c.len() && matches!(c[j], '"' | '\'') {
            j += 1;
        }
        while j < c.len() && c[j] == ' ' {
            j += 1;
        }
        if j < c.len() && matches!(c[j], '=' | ':') {
            if SECRET_LINE_KEYS.contains(&lower.as_str()) {
                out.extend(&c[i..=j]);
                out.push(' ');
                out.push_str(REDACTED);
                return redact_blobs(&out);
            }
            if secret_key(&lower) {
                let mut v = j + 1;
                while v < c.len() && c[v] == ' ' {
                    v += 1;
                }
                if v < c.len() && matches!(c[v], '"' | '\'') {
                    v += 1;
                }
                let end = value_end(&c, v);
                if end > v {
                    out.extend(&c[i..v]);
                    out.push_str(REDACTED);
                    i = end;
                    continue;
                }
            }
        }
        // `Bearer <token>`, `relay.secret.set <secret>`, `youtube.key.set <key>`
        let takes_next = matches!(lower.as_str(), "bearer" | "basic") || lower.ends_with("secret.set") || lower.ends_with("key.set");
        if takes_next && i < c.len() && c[i] == ' ' {
            let mut v = i;
            while v < c.len() && c[v] == ' ' {
                v += 1;
            }
            let end = value_end(&c, v);
            if end > v {
                out.extend(&c[i..v]);
                out.push_str(REDACTED);
                i = end;
                continue;
            }
        }
    }
    redact_blobs(&out)
}

/// Remove credentials from free text: secret `key=value` pairs (URL queries, JSON, headers),
/// `Bearer …`, cookies (SAPISID & co.), OAuth/refresh tokens, `RELAY_SECRET`, and long hex or
/// base64 blobs. Ordinary sentences, paths and URLs stay readable.
pub fn redact(text: &str) -> String {
    text.split('\n').map(redact_line).collect::<Vec<_>>().join("\n")
}

// ---- bundle -----------------------------------------------------------------------------------

/// `YYYY-MM-DD HH:MM:SS UTC` for a unix time (civil-from-days, no time zone database needed).
pub fn utc(unix_s: i64) -> String {
    let days = unix_s.div_euclid(86_400);
    let secs = unix_s.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC", secs / 3600, (secs / 60) % 60, secs % 60)
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 400 { format!("{}…", s.chars().take(400).collect::<String>()) } else { s }
}

/// Everything a diagnosis needs, already gathered from the UI's mirror of the engine.
pub struct Bundle<'a> {
    pub unix_s: i64,
    pub m: &'a Model,
    pub on_air: bool,
    /// The check the operator asked about (`None`: general diagnosis).
    pub check: Option<&'a str>,
    pub checks: &'a [Check],
    pub tracker: &'a health::Tracker,
    /// Engine unreachable for this long (seconds), if it is.
    pub engine_down_s: Option<u64>,
    pub now: Instant,
}

impl Bundle<'_> {
    /// The bundle as markdown, redacted.
    pub fn markdown(&self) -> String {
        let m = self.m;
        let mut s = String::new();
        let mut line = |l: String| {
            s.push_str(&l);
            s.push('\n');
        };
        line("# Stream Engine diagnostic bundle".into());
        line(String::new());
        line(format!("- Created: {} (unix {})", utc(self.unix_s), self.unix_s));
        line(format!(
            "- Asked about: {}",
            self.check.map_or_else(|| "general health (no single check)".to_string(), |c| format!("`health.{c}` ({})", health::title(c)))
        ));
        line("- Opening this session changed nothing. Assume the show is LIVE until proven otherwise.".into());
        line(String::new());
        line("## On air".into());
        line(String::new());
        line(format!("- UI on-air judgement (OBS output active or show clock running): {}", if self.on_air { "ON AIR" } else { "off air" }));
        for a in ["show.mode", "show.live_since", "twitch.stream.live", "obs.stream.active", "obs.link"] {
            line(format!("- `{a}` = {}", m.get(a).map_or_else(|| "(not published)".to_string(), short)));
        }
        for (a, v) in m.under("obs.output").filter(|(a, _)| a.ends_with(".active")) {
            line(format!("- `{a}` = {}", short(v)));
        }
        match self.engine_down_s {
            Some(secs) => line(format!("- Engine: UNREACHABLE for {secs} s (last reason: {})", m.last_disconnect.as_deref().unwrap_or("unknown"))),
            None => {
                let info = m.q("engine.info");
                let f = |k: &str| info.and_then(|v| v.get_path(k)).map_or_else(|| "?".to_string(), short);
                line(format!("- Engine: connected (pid {}, version {}, session {}, project {})", m.engine_pid, f("version"), m.session, f("project")));
            }
        }
        line(String::new());
        line("## Failing now".into());
        line(String::new());
        let failing: Vec<&Check> = self.checks.iter().filter(|c| Some(c.name.as_str()) == self.check || health::alerting(c, self.on_air)).collect();
        if failing.is_empty() && self.engine_down_s.is_none() {
            line("- Nothing is failing right now.".into());
        }
        for c in failing {
            let how_long = self.tracker.get(&c.name).map(|t| t.for_how_long(self.now)).unwrap_or_default();
            line(format!("- **health.{}** ({}): {} {how_long} — {}", c.name, health::title(&c.name), c.status.word().to_uppercase(), c.detail));
        }
        line(String::new());
        line("## Every health check".into());
        line(String::new());
        line("| check | status | for | detail |".into());
        line("|---|---|---|---|".into());
        for c in self.checks {
            let how_long = self.tracker.get(&c.name).map(|t| t.for_how_long(self.now)).unwrap_or_default();
            line(format!("| {} | {} | {} | {} |", c.name, c.status.word(), how_long.trim_start_matches("for "), c.detail.replace('|', "\\|")));
        }
        line(String::new());
        line("## Related state".into());
        line(String::new());
        let mut related: Vec<(String, String)> = m.under("queue").map(|(a, v)| (a.clone(), short(v))).collect();
        for a in ["song.account", "twitch.auth.status", "twitch.eventsub.connected", "relay.queue_url"] {
            if let Some(v) = m.get(a) {
                related.push((a.into(), short(v)));
            }
        }
        if related.is_empty() {
            line("- (none published)".into());
        }
        for (a, v) in related {
            line(format!("- `{a}` = {v}"));
        }
        line(String::new());
        line(format!("## Last {LOG_LINES} warnings and errors seen by this UI (oldest first)"));
        line(String::new());
        line("```".into());
        let logs: Vec<_> = m.logs.iter().filter(|l| l.level == "warn" || l.level == "error").collect();
        if logs.is_empty() {
            line("(none since this window connected)".into());
        }
        for l in &logs[logs.len().saturating_sub(LOG_LINES)..] {
            let at = if l.received_ms > 0 { utc(l.received_ms / 1000) } else { "?".into() };
            line(format!("{at} {} {}: {}", l.level.to_uppercase(), l.target, l.msg.replace("```", "'''")));
        }
        line("```".into());
        redact(&s)
    }
}

/// `<check>` reduced to a safe file-name part.
fn file_part(check: Option<&str>) -> String {
    let raw = check.unwrap_or("general");
    let s: String = raw.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect();
    if s.is_empty() { "general".into() } else { s }
}

/// Write `$XDG_RUNTIME_DIR/stream-engine/diagnostics/<unix-ts>-<check>.md` (directory 0700,
/// file 0600, never overwriting).
pub fn write_bundle(runtime_dir: &Path, unix_s: i64, check: Option<&str>, text: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let dir = runtime_dir.join("stream-engine").join("diagnostics");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let base = format!("{unix_s}-{}", file_part(check));
    let mut n = 0;
    loop {
        let name = if n == 0 { format!("{base}.md") } else { format!("{base}-{n}.md") };
        let path = dir.join(name);
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
            Ok(mut f) => {
                f.write_all(text.as_bytes())?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 100 => n += 1,
            Err(e) => return Err(e),
        }
    }
}

/// The Stream Engine folder (`engine.info.share`, else `STREAM_ENGINE_SHARE`) when it holds the
/// diagnosis overlay; `Err` says why Fix with AI is unavailable.
pub fn share_dir(engine_share: &str, env_share: Option<&str>) -> Result<PathBuf, String> {
    let share = Some(engine_share).filter(|s| !s.is_empty()).or(env_share.filter(|s| !s.is_empty()));
    let Some(share) = share else {
        return Err("Fix with AI needs the Stream Engine folder: start the engine, or set STREAM_ENGINE_SHARE.".into());
    };
    let share = PathBuf::from(share);
    if !share.join("packaging/omp-diagnose.yml").is_file() || !share.join("packaging/omp-diagnose.md").is_file() {
        return Err(format!("Fix with AI needs packaging/omp-diagnose.yml and .md in {}.", share.display()));
    }
    Ok(share)
}

/// The one-line request that opens the session.
pub fn request(check: Option<&str>) -> String {
    match check {
        Some(c) => format!(
            "The {} health check (health.{c}) is failing during my stream. Diagnose it read-only from the attached bundle, tell me the cause, and propose the narrowest fix. Ask before changing anything.",
            health::title(c)
        ),
        None => "Check Stream Engine's health from the attached bundle, read-only. Tell me what is wrong and propose the narrowest fixes. Ask before changing anything.".into(),
    }
}

/// argv for the diagnosis terminal (no shell).
pub fn argv(share: &Path, project: Option<&Path>, bundle: &Path, check: Option<&str>) -> Vec<String> {
    let s = |p: &Path| p.display().to_string();
    let mut a = vec!["xdg-terminal-exec".to_string(), "--".into(), "omp".into(), "--cwd".into(), s(share)];
    if let Some(p) = project {
        a.extend(["--add-dir".into(), s(p)]);
    }
    a.extend([
        "--config".into(),
        s(&share.join("packaging/omp-diagnose.yml")),
        "--approval-mode".into(),
        "always-ask".into(),
        "--append-system-prompt".into(),
        s(&share.join("packaging/omp-diagnose.md")),
        format!("@{}", s(bundle)),
        request(check),
    ]);
    a
}

/// Launch detached (stdio closed, reaped in the background), like the editor launcher.
pub fn spawn(argv: &[String]) -> std::io::Result<()> {
    let mut child = Command::new(&argv[0]).args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    std::thread::Builder::new().name("se-diagnose-reap".into()).spawn(move || {
        let _ = child.wait();
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_never_survive_redaction() {
        let secrets = [
            ("https://relay.example/link?token=s3cr3tValue&x=1", "s3cr3tValue"),
            ("GET /patches/a/index.html?key=AbC123xyz", "AbC123xyz"),
            ("client_secret=hunter2 refresh_token=abcDEF", "hunter2"),
            ("refresh_token=abcDEF", "abcDEF"),
            ("Authorization: Bearer abc.def.ghi", "abc.def.ghi"),
            ("authorization: OAuth twitchtok", "twitchtok"),
            ("sent with bearer zzTopSecret99", "zzTopSecret99"),
            ("Cookie: SAPISID=AbCdEf/GhIjKl; __Secure-3PAPISID=Zz9", "AbCdEf"),
            ("SAPISID=AbCdEf/GhIjKl", "AbCdEf"),
            ("__Secure-3PAPISID=Zz9yYx", "Zz9yYx"),
            (r#"{"access_token":"tok_value_1","expires_in":3600}"#, "tok_value_1"),
            ("RELAY_SECRET=correct-horse-battery", "correct-horse-battery"),
            ("streamctl do relay.secret.set plainword", "plainword"),
            ("youtube.key.set AIzaSyD-shortish", "AIzaSyD-shortish"),
            ("token=ya29.a0AfH6SMBx", "ya29.a0AfH6SMBx"),
            ("got ya29.a0AfH6SMBxQ-long_google.token back", "a0AfH6SMBxQ"),
            ("refresh 1//0gAbCdEfGh-ij_kl for later", "0gAbCdEfGh"),
            ("jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sig", "eyJhbGciOiJIUzI1NiJ9"),
            ("twitch token used: 0123456789abcdefghijklmnopqrst", "0123456789abcdefghijklmnopqrst"),
            ("hash d41d8cd98f00b204e9800998ecf8427e00000000 seen", "d41d8cd98f00b204e9800998ecf8427e"),
            ("blob QWxhZGRpbjpvcGVuIHNlc2FtZQ0K9xYzAbCdEfGh123 end", "QWxhZGRpbjpvcGVuIHNlc2FtZQ0K9xYzAbCdEfGh123"),
            ("password: \"hunter22\"", "hunter22"),
        ];
        for (input, secret) in secrets {
            let out = redact(input);
            assert!(!out.contains(secret), "{input:?} → {out:?} still contains {secret:?}");
            assert!(out.contains(REDACTED), "{input:?} → {out:?}");
        }
    }

    #[test]
    fn ordinary_text_is_preserved() {
        for text in [
            "Your microphone is silent. Is it muted or unplugged?",
            "connection refused — retrying in 8s",
            "authorized as dabsanddrums (token 212m left); EventSub connected (14 subscriptions)",
            "authorized as x; EventSub connected; token expires in 300s",
            "token invalid (401 Unauthorized); run twitch.auth.start",
            "public queue page unreachable (queue.dabsanddrums.com): tunnel or DNS; chat requests still work",
            "https://queue.dabsanddrums.com/queue",
            "see /home/dabsanddrums/.local/share/stream-engine/cef/host.log",
            "/run/user/1000/stream-engine/diagnostics/1790000000-relay.md",
            "embedded YouTube channel UCz7OyuTD7kJJ6nJHko6r7ZQ verified (playing)",
            "API key set; 120/10000 units used today",
            "no YouTube API key — `streamctl do youtube.key.set <key>`; requests use the library only",
            "cam_3: no signal; sources/cam.toml: unknown field",
            "| relay | fail | 3 min | lost |",
            "123e4567-e89b-12d3-a456-426614174000",
        ] {
            assert_eq!(redact(text), text);
        }
        assert_eq!(redact("a\nb\n"), "a\nb\n", "line structure kept");
    }

    #[test]
    fn utc_formats_known_instants() {
        assert_eq!(utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc(1_790_000_000), "2026-09-21 14:13:20 UTC");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00:00 UTC");
    }

    #[test]
    fn missing_share_disables_the_launcher() {
        assert!(share_dir("", None).is_err());
        assert!(share_dir("", Some("")).is_err());
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_str().unwrap();
        assert!(share_dir(p, None).is_err(), "folder without the overlay");
        std::fs::create_dir_all(dir.path().join("packaging")).unwrap();
        std::fs::write(dir.path().join("packaging/omp-diagnose.yml"), "").unwrap();
        std::fs::write(dir.path().join("packaging/omp-diagnose.md"), "").unwrap();
        assert_eq!(share_dir("", Some(p)).unwrap(), dir.path(), "env fallback");
        assert_eq!(share_dir(p, Some("/nope")).unwrap(), dir.path(), "engine.info wins");
    }

    #[test]
    fn bundle_files_are_private_and_never_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let a = write_bundle(dir.path(), 5, Some("audio.input/../x"), "one").unwrap();
        let b = write_bundle(dir.path(), 5, Some("audio.input/../x"), "two").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.parent().unwrap(), dir.path().join("stream-engine/diagnostics"), "no path escape from the check name");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "one");
        assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(a.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
    }
}
