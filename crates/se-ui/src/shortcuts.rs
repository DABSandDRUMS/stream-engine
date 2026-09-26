//! Rebindable shortcuts (§15.7), persisted in `layouts/shortcuts.toml`.
//!
//! Actions have stable string ids ([`ACTIONS`]). A [`Chord`] is `[hold ]Ctrl+Shift+Alt+Key` with
//! exact modifier matching. [`Shortcuts`] maps actions to chords; the file stores only overrides
//! of the defaults (`[keys] take = ["Enter"]`) and is rewritten with `toml_edit` so comments stay.

use anyhow::{Context as _, Result};
use egui::{Context, Event, InputState, Key};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use toml_edit::{Array, DocumentMut, Item, Table, Value};

/// How long a hold chord (Panic) must be held.
pub const HOLD_SECS: f64 = 1.0;

/// One bindable action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActionInfo {
    /// Stable id used in `shortcuts.toml` and by the UI.
    pub id: &'static str,
    /// Human description for the palette, tooltips, and settings.
    pub desc: &'static str,
    pub defaults: &'static [&'static str],
    /// Fires after holding its chord for [`HOLD_SECS`] (hold-to-confirm), not on press.
    pub hold: bool,
    /// Fires even while a text field has keyboard focus.
    pub global: bool,
    /// Key auto-repeat fires it again.
    pub repeat: bool,
}

const fn act(id: &'static str, desc: &'static str, defaults: &'static [&'static str]) -> ActionInfo {
    ActionInfo { id, desc, defaults, hold: false, global: false, repeat: false }
}

const fn global(a: ActionInfo) -> ActionInfo {
    ActionInfo { global: true, ..a }
}

const fn repeat(a: ActionInfo) -> ActionInfo {
    ActionInfo { repeat: true, ..a }
}

/// Every action, in display order.
pub const ACTIONS: &[ActionInfo] = &[
    global(act("palette", "Command palette", &["Ctrl+K"])),
    act("mode.toggle", "Jump between Live and Scenes", &["Tab"]),
    act("scene.preview.1", "Scene 1 to preview", &["1"]),
    act("scene.preview.2", "Scene 2 to preview", &["2"]),
    act("scene.preview.3", "Scene 3 to preview", &["3"]),
    act("scene.preview.4", "Scene 4 to preview", &["4"]),
    act("scene.preview.5", "Scene 5 to preview", &["5"]),
    act("scene.preview.6", "Scene 6 to preview", &["6"]),
    act("scene.preview.7", "Scene 7 to preview", &["7"]),
    act("scene.preview.8", "Scene 8 to preview", &["8"]),
    act("scene.preview.9", "Scene 9 to preview", &["9"]),
    act("take", "Take (preview to program)", &["Enter"]),
    act("scene.program.1", "Scene 1 direct to program", &["Shift+1"]),
    act("scene.program.2", "Scene 2 direct to program", &["Shift+2"]),
    act("scene.program.3", "Scene 3 direct to program", &["Shift+3"]),
    act("scene.program.4", "Scene 4 direct to program", &["Shift+4"]),
    act("scene.program.5", "Scene 5 direct to program", &["Shift+5"]),
    act("scene.program.6", "Scene 6 direct to program", &["Shift+6"]),
    act("scene.program.7", "Scene 7 direct to program", &["Shift+7"]),
    act("scene.program.8", "Scene 8 direct to program", &["Shift+8"]),
    act("scene.program.9", "Scene 9 direct to program", &["Shift+9"]),
    global(act("pad.1", "Preset pad 1 (current page)", &["F1"])),
    global(act("pad.2", "Preset pad 2 (current page)", &["F2"])),
    global(act("pad.3", "Preset pad 3 (current page)", &["F3"])),
    global(act("pad.4", "Preset pad 4 (current page)", &["F4"])),
    global(act("pad.5", "Preset pad 5 (current page)", &["F5"])),
    global(act("pad.6", "Preset pad 6 (current page)", &["F6"])),
    global(act("pad.7", "Preset pad 7 (current page)", &["F7"])),
    global(act("pad.8", "Preset pad 8 (current page)", &["F8"])),
    global(act("pad.9", "Preset pad 9 (current page)", &["F9"])),
    global(act("pad.10", "Preset pad 10 (current page)", &["F10"])),
    global(act("pad.11", "Preset pad 11 (current page)", &["F11"])),
    global(act("pad.12", "Preset pad 12 (current page)", &["F12"])),
    repeat(act("undo", "Undo", &["Ctrl+Z"])),
    repeat(act("redo", "Redo", &["Ctrl+Shift+Z"])),
    global(act("clean", "Clean (clear chat effects)", &["Ctrl+."])),
    ActionInfo { hold: true, global: true, ..act("panic", "Panic (hold)", &["hold Ctrl+Esc"]) },
    global(act("layout.next", "Switch to the next layout", &["Ctrl+L"])),
    global(act("search", "Search library / chat", &["Ctrl+F"])),
    global(repeat(act("zoom.in", "Zoom in", &["Ctrl+=", "Ctrl+Plus"]))),
    global(repeat(act("zoom.out", "Zoom out", &["Ctrl+-"]))),
    act("escape", "Cancel / close", &["Esc"]),
];

/// Metadata for an action id.
pub fn action(id: &str) -> Option<&'static ActionInfo> {
    ACTIONS.iter().find(|a| a.id == id)
}

/// A key plus exact modifiers; `hold` chords fire after being held (see [`HoldTracker`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: Key,
    pub hold: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChordError(pub String);

impl fmt::Display for ChordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ChordError {}

fn is_digit(k: Key) -> bool {
    matches!(k, Key::Num0 | Key::Num1 | Key::Num2 | Key::Num3 | Key::Num4 | Key::Num5 | Key::Num6 | Key::Num7 | Key::Num8 | Key::Num9)
}

fn is_modifier_key(k: Key) -> bool {
    matches!(k, Key::ShiftLeft | Key::ShiftRight | Key::ControlLeft | Key::ControlRight | Key::AltLeft | Key::AltRight | Key::SuperLeft | Key::SuperRight)
}

/// Symbols that need Shift on common layouts: a chord without Shift still matches them.
fn is_shifted_symbol(k: Key) -> bool {
    matches!(k, Key::Plus | Key::Colon | Key::Questionmark | Key::Exclamationmark | Key::Pipe | Key::OpenCurlyBracket | Key::CloseCurlyBracket)
}

fn key_label(k: Key) -> &'static str {
    match k {
        Key::Escape => "Esc",
        // `+` is the separator.
        Key::Plus => "Plus",
        Key::Minus => "-",
        Key::ArrowDown | Key::ArrowLeft | Key::ArrowRight | Key::ArrowUp => k.name(),
        _ => k.symbol_or_name(),
    }
}

fn parse_key(s: &str) -> Option<Key> {
    let alias = match s.to_ascii_lowercase().as_str() {
        "esc" => Some(Key::Escape),
        "return" => Some(Key::Enter),
        "del" => Some(Key::Delete),
        "ins" => Some(Key::Insert),
        "pgup" => Some(Key::PageUp),
        "pgdn" => Some(Key::PageDown),
        "dot" => Some(Key::Period),
        _ => None,
    };
    let key = Key::from_name(s).or(alias).or_else(|| Key::ALL.iter().copied().find(|k| k.name().eq_ignore_ascii_case(s)))?;
    (!is_modifier_key(key)).then_some(key)
}

impl Chord {
    pub fn new(key: Key) -> Chord {
        Chord { ctrl: false, shift: false, alt: false, key, hold: false }
    }

    /// Parse `"Ctrl+Shift+Z"`, `"hold Ctrl+Esc"`, `"Shift+1"`, `"Ctrl+."`, `"Ctrl++"`, … Modifier and
    /// key names are case-insensitive.
    pub fn parse(s: &str) -> Result<Chord, ChordError> {
        let err = |why: &str| ChordError(format!("invalid shortcut `{s}`: {why}"));
        let mut rest = s.trim();
        let hold = rest.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("hold ")) || rest.eq_ignore_ascii_case("hold");
        if hold {
            rest = rest.get(4..).unwrap_or("").trim_start();
        }
        let mut chord = Chord::new(Key::Escape);
        chord.hold = hold;
        while let Some((token, tail)) = rest.split_once('+') {
            let flag = match token.trim().to_ascii_lowercase().as_str() {
                "ctrl" | "control" => &mut chord.ctrl,
                "shift" => &mut chord.shift,
                "alt" => &mut chord.alt,
                "super" | "meta" | "cmd" | "win" => return Err(err("Super is reserved for Hyprland")),
                _ => break,
            };
            if tail.trim().is_empty() {
                return Err(err("missing key"));
            }
            if *flag {
                return Err(err("repeated modifier"));
            }
            *flag = true;
            rest = tail;
        }
        let name = rest.trim();
        if name.is_empty() {
            return Err(err("missing key"));
        }
        chord.key = parse_key(name).ok_or_else(|| err(&format!("unknown key `{name}`")))?;
        Ok(chord)
    }

    /// Same keys and modifiers (ignoring `hold`): the two can't both be bound.
    pub fn same_keys(&self, other: &Chord) -> bool {
        (self.ctrl, self.shift, self.alt, self.key) == (other.ctrl, other.shift, other.alt, other.key)
    }

    fn matches(&self, key: Key, physical: Option<Key>, m: egui::Modifiers) -> bool {
        // Shift+digit arrives as the shifted symbol (`!`) with the digit as the physical key.
        let key_ok = key == self.key || (is_digit(self.key) && physical == Some(self.key));
        let shift_ok = m.shift == self.shift || (!self.shift && key == self.key && is_shifted_symbol(self.key));
        key_ok && shift_ok && m.ctrl == self.ctrl && m.alt == self.alt && !m.mac_cmd
    }

    /// Consume every press of this chord in `input` (so nothing else sees it); true when at least
    /// one counts (auto-repeat counts only with `allow_repeat`). Modifiers must match exactly.
    pub fn consume(&self, input: &mut InputState, allow_repeat: bool) -> bool {
        let mut fired = false;
        input.events.retain(|e| match e {
            Event::Key { key, physical_key, pressed: true, repeat, modifiers } if self.matches(*key, *physical_key, *modifiers) => {
                fired |= allow_repeat || !*repeat;
                false
            }
            _ => true,
        });
        fired
    }

    /// Was this chord pressed this frame? Consumes it; ignores auto-repeat.
    pub fn pressed(&self, ctx: &Context) -> bool {
        ctx.input_mut(|i| self.consume(i, false))
    }

    /// Is the chord's key held right now with exactly these modifiers?
    pub fn held(&self, input: &InputState) -> bool {
        let m = input.modifiers;
        input.key_down(self.key) && m.ctrl == self.ctrl && m.shift == self.shift && m.alt == self.alt
    }

    /// The first non-modifier key press this frame as a chord (for "press a key" rebinding).
    /// Consumes it.
    pub fn capture(ctx: &Context) -> Option<Chord> {
        ctx.input_mut(|i| {
            let at = i.events.iter().position(|e| matches!(e, Event::Key { key, pressed: true, repeat: false, .. } if !is_modifier_key(*key)))?;
            match i.events.remove(at) {
                Event::Key { key, physical_key, modifiers, .. } => {
                    let key = match physical_key {
                        Some(p) if is_digit(p) && !is_digit(key) => p,
                        _ => key,
                    };
                    Some(Chord { ctrl: modifiers.ctrl, shift: modifiers.shift, alt: modifiers.alt, key, hold: false })
                }
                _ => None,
            }
        })
    }
}

impl std::str::FromStr for Chord {
    type Err = ChordError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Chord::parse(s)
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.hold {
            f.write_str("hold ")?;
        }
        for (on, name) in [(self.ctrl, "Ctrl+"), (self.shift, "Shift+"), (self.alt, "Alt+")] {
            if on {
                f.write_str(name)?;
            }
        }
        f.write_str(key_label(self.key))
    }
}

/// Progress of a hold chord (see [`HoldTracker::update`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HoldState {
    Idle,
    /// Held; progress 0..1 toward firing (draw a ring/bar).
    Holding(f32),
    /// Fired this frame; fires once per press.
    Fired,
}

/// Tracks one hold action across frames.
#[derive(Clone, Copy, Debug, Default)]
pub struct HoldTracker {
    since: Option<f64>,
    fired: bool,
}

impl HoldTracker {
    /// Call every frame with the action's chords. While held, the chord's key presses are
    /// consumed (so a held Ctrl+Esc doesn't also act as Esc) and repaints are requested so the
    /// firing moment doesn't wait for input.
    pub fn update(&mut self, ctx: &Context, chords: &[Chord], secs: f64) -> HoldState {
        let (down, now) = ctx.input_mut(|i| {
            let down = chords.iter().any(|c| c.held(i));
            if down {
                for c in chords {
                    c.consume(i, true);
                }
            }
            (down, i.time)
        });
        if !down {
            *self = HoldTracker::default();
            return HoldState::Idle;
        }
        let since = *self.since.get_or_insert(now);
        if self.fired {
            return HoldState::Idle;
        }
        let progress = if secs > 0.0 { ((now - since) / secs) as f32 } else { 1.0 };
        if progress >= 1.0 {
            self.fired = true;
            HoldState::Fired
        } else {
            ctx.request_repaint();
            HoldState::Holding(progress.max(0.0))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RebindError {
    /// The chord already triggers `other_action`.
    Conflict {
        chord: Chord,
        other_action: &'static str,
    },
    UnknownAction(String),
}

impl fmt::Display for RebindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RebindError::Conflict { chord, other_action } => {
                let desc = action(other_action).map_or("", |a| a.desc);
                write!(f, "{chord} is already bound to {desc} ({other_action})")
            }
            RebindError::UnknownAction(a) => write!(f, "unknown action `{a}`"),
        }
    }
}

impl std::error::Error for RebindError {}

/// Action → chords. Every id in [`ACTIONS`] has an entry (possibly empty = unbound).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shortcuts {
    map: BTreeMap<&'static str, Vec<Chord>>,
}

impl Default for Shortcuts {
    fn default() -> Self {
        Shortcuts { map: ACTIONS.iter().map(|a| (a.id, default_chords(a))).collect() }
    }
}

fn default_chords(a: &ActionInfo) -> Vec<Chord> {
    a.defaults.iter().map(|s| Chord::parse(s).expect("built-in shortcut parses")).map(|c| Chord { hold: a.hold, ..c }).collect()
}

/// Chords from a `shortcuts.toml` value: a string or an array of strings.
fn chords_from(item: &Item) -> Result<Vec<Chord>, String> {
    let strings: Vec<&str> = match item {
        Item::Value(Value::String(s)) => vec![s.value().as_str()],
        Item::Value(Value::Array(a)) => a.iter().map(|v| v.as_str().ok_or("list entries must be strings")).collect::<Result<_, _>>()?,
        _ => return Err("expected a string or a list of strings".into()),
    };
    strings.into_iter().map(|s| Chord::parse(s).map_err(|e| e.0)).collect()
}

impl Shortcuts {
    /// The §15.7 defaults.
    pub fn defaults() -> Shortcuts {
        Shortcuts::default()
    }

    pub fn chords(&self, action: &str) -> &[Chord] {
        self.map.get(action).map_or(&[], Vec::as_slice)
    }

    /// For tooltips and the palette: `"Ctrl+K"`, `"Ctrl+= / Ctrl+Plus"`, or `""` when unbound.
    pub fn label(&self, action: &str) -> String {
        let mut out = String::new();
        for (i, c) in self.chords(action).iter().enumerate() {
            if i > 0 {
                out.push_str(" / ");
            }
            out.push_str(&c.to_string());
        }
        out
    }

    /// The action a chord triggers, if any (ignoring `hold`).
    pub fn action_for(&self, chord: &Chord) -> Option<&'static str> {
        self.map.iter().find(|(_, cs)| cs.iter().any(|c| c.same_keys(chord))).map(|(a, _)| *a)
    }

    pub fn is_default(&self, action: &str) -> bool {
        self::action(action).is_some_and(|a| self.chords(action) == default_chords(a))
    }

    /// Bind `action` to `chord` alone. The chord's `hold` flag is normalized to the action's
    /// (Panic is always hold-to-confirm; nothing else is). Fails if another action uses it.
    pub fn rebind(&mut self, action: &str, chord: Chord) -> Result<(), RebindError> {
        let info = self::action(action).ok_or_else(|| RebindError::UnknownAction(action.to_string()))?;
        let chord = Chord { hold: info.hold, ..chord };
        if let Some(other) = self.action_for(&chord).filter(|o| *o != info.id) {
            return Err(RebindError::Conflict { chord, other_action: other });
        }
        self.map.insert(info.id, vec![chord]);
        Ok(())
    }

    /// Remove every chord of `action`.
    pub fn unbind(&mut self, action: &str) {
        if let Some(cs) = self.map.get_mut(action) {
            cs.clear();
        }
    }

    /// Restore the default chords of `action`; fails if one is now used by another action.
    pub fn reset(&mut self, action: &str) -> Result<(), RebindError> {
        let info = self::action(action).ok_or_else(|| RebindError::UnknownAction(action.to_string()))?;
        let chords = default_chords(info);
        for c in &chords {
            if let Some(other) = self.action_for(c).filter(|o| *o != info.id) {
                return Err(RebindError::Conflict { chord: *c, other_action: other });
            }
        }
        self.map.insert(info.id, chords);
        Ok(())
    }

    /// Did `action` fire this frame? Consumes its key presses. Non-global actions don't fire
    /// while a widget (typically a text field) has keyboard focus; hold actions never fire here (use [`HoldTracker`]).
    pub fn fired(&self, ctx: &Context, action: &str) -> bool {
        let Some(info) = self::action(action) else { return false };
        if info.hold || (!info.global && ctx.egui_wants_keyboard_input()) {
            return false;
        }
        let chords = self.chords(action);
        // Consume all matching chords, not just the first that fires.
        ctx.input_mut(|i| chords.iter().fold(false, |any, c| c.consume(i, info.repeat) | any))
    }

    /// Load `layouts/shortcuts.toml` over the defaults. Invalid TOML is an error; unknown actions,
    /// bad chords, and conflicts are skipped with a warning each. On conflict the file's binding
    /// wins over a default; between two file bindings the earlier action (in [`ACTIONS`]) wins.
    pub fn load(text: &str) -> Result<(Shortcuts, Vec<String>)> {
        let doc: DocumentMut = text.parse().context("invalid TOML in shortcuts.toml")?;
        let mut warnings = Vec::new();
        for (k, _) in doc.iter().filter(|(k, _)| *k != "keys") {
            warnings.push(format!("shortcuts.toml: unknown section `{k}`"));
        }
        let mut overrides: HashMap<&'static str, Vec<Chord>> = HashMap::new();
        match doc.get("keys") {
            None => {}
            Some(Item::Table(t)) => {
                for (id, item) in t.iter() {
                    let Some(info) = self::action(id) else {
                        warnings.push(format!("shortcuts.toml: unknown action `{id}`"));
                        continue;
                    };
                    match chords_from(item) {
                        Ok(cs) => {
                            overrides.insert(info.id, cs.into_iter().map(|c| Chord { hold: info.hold, ..c }).collect());
                        }
                        Err(e) => warnings.push(format!("shortcuts.toml: {id}: {e} (keeping the default)")),
                    }
                }
            }
            Some(_) => warnings.push("shortcuts.toml: `keys` must be a table".into()),
        }
        let mut s = Shortcuts::default();
        let mut taken: Vec<(Chord, &'static str)> = Vec::new();
        // Overridden actions claim their chords first, then defaults fill in around them.
        for pass_overrides in [true, false] {
            for info in ACTIONS.iter().filter(|a| overrides.contains_key(a.id) == pass_overrides) {
                let wanted = overrides.get(info.id).cloned().unwrap_or_else(|| default_chords(info));
                let mut kept = Vec::with_capacity(wanted.len());
                for c in wanted {
                    match taken.iter().find(|(t, _)| t.same_keys(&c)) {
                        Some((_, other)) => {
                            let what = if pass_overrides { "" } else { "default " };
                            warnings.push(format!("shortcuts.toml: {}: {what}{c} is already used by {other}", info.id));
                        }
                        None => {
                            taken.push((c, info.id));
                            kept.push(c);
                        }
                    }
                }
                s.map.insert(info.id, kept);
            }
        }
        Ok((s, warnings))
    }

    /// Serialize the overrides. With `existing` (current file text) unchanged entries, comments,
    /// and unknown keys are kept; entries back at their default are removed. Without it (or if it
    /// doesn't parse) a fresh file listing every action and default is written.
    pub fn to_toml(&self, existing: Option<&str>) -> String {
        let overrides: Vec<(&'static str, Value)> = ACTIONS
            .iter()
            .filter(|a| !self.is_default(a.id))
            .map(|a| (a.id, Value::Array(self.chords(a.id).iter().map(|c| c.to_string()).collect::<Array>())))
            .collect();
        let mut doc = match existing.map(str::parse::<DocumentMut>) {
            Some(Ok(doc)) => doc,
            _ => return fresh_file(&overrides),
        };
        if !doc.get("keys").is_some_and(Item::is_table) {
            // The new table goes last: text trailing the document (comments) stays above it.
            let mut keys = Table::new();
            if let Some(trailing) = doc.trailing().as_str().filter(|t| !t.is_empty()).map(str::to_string) {
                doc.set_trailing("");
                keys.decor_mut().set_prefix(trailing);
            }
            doc.insert("keys", Item::Table(keys));
        }
        let Some(keys) = doc.get_mut("keys").and_then(Item::as_table_mut) else { return fresh_file(&overrides) };
        for info in ACTIONS {
            let want = overrides.iter().find(|(id, _)| *id == info.id).map(|(_, v)| v);
            match (keys.get_mut(info.id), want) {
                (Some(_), None) => {
                    keys.remove(info.id);
                }
                (Some(item), Some(v)) => {
                    let current = chords_from(item).map(|cs| cs.into_iter().map(|c| Chord { hold: info.hold, ..c }).collect::<Vec<_>>());
                    if current.as_deref() != Ok(self.chords(info.id)) {
                        let decor = item.as_value().map(|old| old.decor().clone());
                        let mut v = v.clone();
                        if let Some(decor) = decor {
                            *v.decor_mut() = decor;
                        }
                        *item = Item::Value(v);
                    }
                }
                (None, Some(v)) => {
                    keys.insert(info.id, Item::Value(v.clone()));
                }
                (None, None) => {}
            }
        }
        doc.to_string()
    }
}

fn fresh_file(overrides: &[(&'static str, Value)]) -> String {
    let mut out = String::from(
        "# stream-engine shortcuts (PLAN §15.7). Only overrides live here: delete a line to restore\n\
         # the default. Chords: \"Ctrl+K\", \"Ctrl+Shift+Z\", \"Shift+1\", \"F5\", \"Ctrl+.\", \"hold Ctrl+Esc\".\n\
         # A list binds several chords; [] unbinds. Global binds (outside the UI) live in Hyprland.\n#\n",
    );
    let w = ACTIONS.iter().map(|a| a.id.len()).max().unwrap_or(0);
    let defaults: Vec<String> = ACTIONS.iter().map(|a| a.defaults.join(" / ")).collect();
    let dw = defaults.iter().map(String::len).max().unwrap_or(0);
    out.push_str(&format!("# {:w$}  {:dw$}  what it does\n", "action", "default"));
    for (a, d) in ACTIONS.iter().zip(&defaults) {
        out.push_str(&format!("# {:w$}  {:dw$}  {}\n", a.id, d, a.desc));
    }
    out.push_str("\n[keys]\n");
    for (id, v) in overrides {
        let mut v = v.clone();
        v.decor_mut().clear();
        out.push_str(&format!("{} = {v}\n", toml_edit::Key::new(*id).display_repr()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Modifiers, RawInput};

    fn c(s: &str) -> Chord {
        Chord::parse(s).unwrap()
    }

    #[test]
    fn parse_and_display_are_canonical() {
        for (input, canonical) in [
            ("Ctrl+K", "Ctrl+K"),
            ("ctrl+shift+z", "Ctrl+Shift+Z"),
            ("Shift+Ctrl+Z", "Ctrl+Shift+Z"),
            ("hold Ctrl+Esc", "hold Ctrl+Esc"),
            ("HOLD  control+escape", "hold Ctrl+Esc"),
            ("Ctrl+.", "Ctrl+."),
            ("Ctrl+Period", "Ctrl+."),
            ("Ctrl+=", "Ctrl+="),
            ("Ctrl+-", "Ctrl+-"),
            ("Ctrl++", "Ctrl+Plus"),
            ("Ctrl+plus", "Ctrl+Plus"),
            ("Shift+1", "Shift+1"),
            ("f12", "F12"),
            ("Enter", "Enter"),
            ("return", "Enter"),
            ("Tab", "Tab"),
            ("Alt + Left", "Alt+Left"),
            ("pageup", "PageUp"),
            ("Space", "Space"),
            ("Ctrl+[", "Ctrl+["),
        ] {
            let chord = Chord::parse(input).unwrap_or_else(|e| panic!("{input}: {e}"));
            assert_eq!(chord.to_string(), canonical, "{input}");
            assert_eq!(Chord::parse(canonical).unwrap(), chord, "{canonical} must round-trip");
        }
        let z = c("Ctrl+Shift+Z");
        assert_eq!((z.ctrl, z.shift, z.alt, z.key, z.hold), (true, true, false, Key::Z, false));
        assert!(c("hold Ctrl+Esc").hold);
    }

    #[test]
    fn parse_rejects_nonsense() {
        for bad in ["", "Ctrl+", "Ctrl+Ctrl+K", "Ctrl+Nope", "Super+K", "Shift", "Ctrl+ShiftLeft", "hold", "hold "] {
            assert!(Chord::parse(bad).is_err(), "{bad:?} should fail");
        }
        assert!(Chord::parse("Ctrl+Nope").unwrap_err().to_string().contains("unknown key `Nope`"));
    }

    #[test]
    fn every_action_default_parses_and_nothing_conflicts() {
        let s = Shortcuts::defaults();
        let mut seen: Vec<(Chord, &str)> = Vec::new();
        for a in ACTIONS {
            assert!(!a.defaults.is_empty(), "{}", a.id);
            for ch in s.chords(a.id) {
                assert_eq!(ch.hold, a.hold, "{}", a.id);
                assert!(!seen.iter().any(|(o, _)| o.same_keys(ch)), "{} conflicts", a.id);
                seen.push((*ch, a.id));
            }
        }
        assert_eq!(ACTIONS.len(), 42);
        assert_eq!(s.label("palette"), "Ctrl+K");
        assert_eq!(s.label("zoom.in"), "Ctrl+= / Ctrl+Plus");
        assert_eq!(s.label("panic"), "hold Ctrl+Esc");
        assert_eq!(s.label("scene.program.3"), "Shift+3");
        assert_eq!(s.label("nope"), "");
        assert_eq!(s.action_for(&c("Ctrl+Esc")), Some("panic"));
        assert_eq!(s.action_for(&c("Esc")), Some("escape"));
    }

    #[test]
    fn rebind_conflicts_and_hold_normalization() {
        let mut s = Shortcuts::defaults();
        assert_eq!(s.rebind("search", c("Ctrl+K")), Err(RebindError::Conflict { chord: c("Ctrl+K"), other_action: "palette" }));
        // `hold` doesn't make a chord distinct.
        assert_eq!(s.rebind("clean", c("hold Ctrl+Esc")).unwrap_err(), RebindError::Conflict { chord: c("Ctrl+Esc"), other_action: "panic" });
        assert!(matches!(s.rebind("nope", c("Ctrl+J")), Err(RebindError::UnknownAction(_))));
        // Rebinding to its own chord is fine.
        s.rebind("palette", c("Ctrl+K")).unwrap();
        s.rebind("take", c("Space")).unwrap();
        assert_eq!(s.chords("take"), [c("Space")]);
        // Panic stays hold-to-confirm; others never hold.
        s.rebind("panic", c("Ctrl+Shift+P")).unwrap();
        assert_eq!(s.label("panic"), "hold Ctrl+Shift+P");
        s.rebind("clean", c("hold Ctrl+J")).unwrap();
        assert_eq!(s.label("clean"), "Ctrl+J");
        // Freed chords can be reused; reset reports a default that got taken.
        s.rebind("search", c("Enter")).unwrap();
        assert_eq!(s.reset("take").unwrap_err(), RebindError::Conflict { chord: c("Enter"), other_action: "search" });
        s.unbind("search");
        s.reset("take").unwrap();
        assert!(s.is_default("take"));
        assert_eq!(s.label("search"), "");
        let msg = s.rebind("search", c("Ctrl+K")).unwrap_err().to_string();
        assert_eq!(msg, "Ctrl+K is already bound to Command palette (palette)");
    }

    #[test]
    fn load_applies_overrides_and_reports_problems() {
        let text = r#"
[keys]
take = "Space"
search = ["Ctrl+K", "Ctrl+Shift+F"]   # steals Ctrl+K from the palette
clean = ["Ctrl+Space"]
panic = "Ctrl+Shift+Esc"
undo = ["Ctrl+Space"]                 # conflicts with clean: undo comes first in ACTIONS and wins
redo = "Ctrl+Bogus"
nope = "F1"
zoom.in = []
[extra]
"#;
        let (s, warnings) = Shortcuts::load(text).unwrap();
        assert_eq!(s.chords("take"), [c("Space")]);
        assert_eq!(s.label("search"), "Ctrl+K / Ctrl+Shift+F");
        assert_eq!(s.label("palette"), "");
        assert_eq!(s.label("undo"), "Ctrl+Space");
        assert_eq!(s.label("clean"), "");
        assert_eq!(s.label("panic"), "hold Ctrl+Shift+Esc");
        assert!(s.is_default("redo"));
        let w = warnings.join("\n");
        for needle in [
            "unknown section `extra`",
            "unknown action `nope`",
            "redo: invalid shortcut `Ctrl+Bogus`",
            "palette: default Ctrl+K is already used by search",
            "clean: Ctrl+Space is already used by undo",
        ] {
            assert!(w.contains(needle), "missing {needle:?} in\n{w}");
        }
        // `zoom.in = []` in TOML is a dotted key (zoom → in), not the action id.
        assert!(w.contains("unknown action `zoom`"), "{w}");
        assert!(s.is_default("zoom.in"));
        let (s, w) = Shortcuts::load("[keys]\n\"zoom.in\" = []\n").unwrap();
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(s.label("zoom.in"), "");
        assert!(Shortcuts::load("[keys\n").is_err());
        let (s, w) = Shortcuts::load("").unwrap();
        assert!(w.is_empty() && s == Shortcuts::defaults());
    }

    #[test]
    fn to_toml_round_trips_and_keeps_comments() {
        let mut s = Shortcuts::defaults();
        let fresh = s.to_toml(None);
        assert!(fresh.contains("# palette") && fresh.contains("Ctrl+K") && fresh.trim_end().ends_with("[keys]"), "{fresh}");
        assert_eq!(Shortcuts::load(&fresh).unwrap(), (s.clone(), vec![]));

        s.rebind("take", c("Space")).unwrap();
        s.rebind("panic", c("Ctrl+Shift+Esc")).unwrap();
        s.unbind("zoom.in");
        let out = s.to_toml(None);
        assert!(out.contains("take = [\"Space\"]\n") && out.contains("panic = [\"hold Ctrl+Shift+Esc\"]\n") && out.contains("\"zoom.in\" = []\n"), "{out}");
        assert_eq!(Shortcuts::load(&out).unwrap().0, s);

        let existing = "# my keys\n[keys]\n# muscle memory\ntake = \"Space\"   # like OBS\nclean = \"Ctrl+J\"\nmystery = \"F13\"\n";
        let mut s = Shortcuts::load(existing).unwrap().0;
        // take unchanged (string form kept), clean back to default (removed), redo added.
        s.reset("clean").unwrap();
        s.rebind("redo", c("Ctrl+Y")).unwrap();
        let out = s.to_toml(Some(existing));
        assert_eq!(out, "# my keys\n[keys]\n# muscle memory\ntake = \"Space\"   # like OBS\nmystery = \"F13\"\nredo = [\"Ctrl+Y\"]\n");
        // Changing a commented entry keeps its comments.
        s.rebind("take", c("Ctrl+Enter")).unwrap();
        let out2 = s.to_toml(Some(&out));
        assert!(out2.contains("# muscle memory\ntake = [\"Ctrl+Enter\"]   # like OBS\n"), "{out2}");
        assert_eq!(Shortcuts::load(&out2).unwrap().0, s);
        // No `[keys]` yet.
        let out3 = s.to_toml(Some("# just a comment\n"));
        assert_eq!(Shortcuts::load(&out3).unwrap().0, s);
        assert!(out3.starts_with("# just a comment\n"));
    }

    #[test]
    fn example_shortcuts_file_loads_clean() {
        let path = format!("{}/../../project-example/layouts/shortcuts.toml", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(path).unwrap();
        let (s, warnings) = Shortcuts::load(&text).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        // Saving unchanged shortcuts rewrites nothing.
        assert_eq!(s.to_toml(Some(&text)), text);
    }

    fn key_event(key: Key, physical: Option<Key>, modifiers: Modifiers, pressed: bool, repeat: bool) -> Event {
        Event::Key { key, physical_key: physical, pressed, repeat, modifiers }
    }

    /// Run one frame with `events`; returns what `f` computed.
    fn frame<T>(ctx: &Context, time: f64, modifiers: Modifiers, events: Vec<Event>, mut f: impl FnMut(&mut egui::Ui) -> T) -> T {
        let mut out = None;
        let events = std::iter::once(Event::ModifiersChanged(modifiers)).chain(events).collect();
        ctx.run_ui(RawInput { time: Some(time), events, ..Default::default() }, |ui| out = Some(f(ui))).drop_without_applying_deltas();
        out.expect("frame ran")
    }

    #[test]
    fn pressed_needs_exact_modifiers_and_consumes() {
        let ctx = Context::default();
        let ctrl_shift = Modifiers { ctrl: true, shift: true, command: true, ..Default::default() };
        let ctrl = Modifiers { ctrl: true, command: true, ..Default::default() };
        // Ctrl+Shift+Z must not trigger Ctrl+Z (undo) — and the event is consumed by redo.
        let ev = vec![key_event(Key::Z, Some(Key::Z), ctrl_shift, true, false)];
        let (undo, redo, again) =
            frame(&ctx, 0.0, ctrl_shift, ev, |ui| (c("Ctrl+Z").pressed(ui.ctx()), c("Ctrl+Shift+Z").pressed(ui.ctx()), c("Ctrl+Shift+Z").pressed(ui.ctx())));
        assert_eq!((undo, redo, again), (false, true, false));
        let ev = vec![key_event(Key::Z, Some(Key::Z), ctrl_shift, false, false), key_event(Key::Z, Some(Key::Z), ctrl, true, false)];
        assert!(frame(&ctx, 0.1, ctrl, ev, |ui| c("Ctrl+Z").pressed(ui.ctx())));
        // Shift+1 on a US layout arrives as `!` with physical key 1.
        let shift = Modifiers { shift: true, ..Default::default() };
        let ev = vec![key_event(Key::Exclamationmark, Some(Key::Num1), shift, true, false)];
        let (plain, shifted) = frame(&ctx, 0.2, shift, ev, |ui| (c("1").pressed(ui.ctx()), c("Shift+1").pressed(ui.ctx())));
        assert_eq!((plain, shifted), (false, true));
        // Ctrl+Plus usually needs Shift; a chord without Shift still matches it.
        let ev = vec![key_event(Key::Plus, Some(Key::Equals), ctrl_shift, true, false)];
        assert!(frame(&ctx, 0.3, ctrl_shift, ev, |ui| c("Ctrl+Plus").pressed(ui.ctx())));
        // Auto-repeat doesn't re-fire a press chord (no double Take): egui flags a press of a
        // key that is already down as a repeat.
        let enter = || vec![key_event(Key::Enter, Some(Key::Enter), Modifiers::NONE, true, false)];
        assert!(frame(&ctx, 0.4, Modifiers::NONE, enter(), |ui| c("Enter").pressed(ui.ctx())));
        assert!(!frame(&ctx, 0.5, Modifiers::NONE, enter(), |ui| c("Enter").pressed(ui.ctx())));
    }

    #[test]
    fn fired_respects_typing_repeat_and_hold() {
        let ctx = Context::default();
        let s = Shortcuts::defaults();
        let ctrl = Modifiers { ctrl: true, command: true, ..Default::default() };
        let eq = || vec![key_event(Key::Equals, Some(Key::Equals), ctrl, true, false)];
        assert!(frame(&ctx, 0.0, ctrl, eq(), |ui| s.fired(ui.ctx(), "zoom.in")));
        assert!(frame(&ctx, 0.05, ctrl, eq(), |ui| s.fired(ui.ctx(), "zoom.in")), "zoom repeats");
        let ev = vec![key_event(Key::Escape, Some(Key::Escape), ctrl, true, false)];
        assert!(!frame(&ctx, 0.1, ctrl, ev, |ui| s.fired(ui.ctx(), "panic")), "hold actions never fire on press");
        // With a focused text field, only global actions fire.
        let id = egui::Id::new("field");
        let ev = vec![key_event(Key::Enter, Some(Key::Enter), Modifiers::NONE, true, false), key_event(Key::F1, Some(Key::F1), Modifiers::NONE, true, false)];
        let mut text = String::new();
        frame(&ctx, 0.2, Modifiers::NONE, vec![], |ui| {
            ui.memory_mut(|m| m.request_focus(id));
            ui.add(se_ui_kit::widgets::field(&mut text).id(id));
        });
        let (take, pad) = frame(&ctx, 0.3, Modifiers::NONE, ev, |ui| {
            let fired = (s.fired(ui.ctx(), "take"), s.fired(ui.ctx(), "pad.1"));
            ui.add(se_ui_kit::widgets::field(&mut text).id(id));
            fired
        });
        assert_eq!((take, pad), (false, true));
        // Without focus, Take fires.
        let release = vec![key_event(Key::Enter, Some(Key::Enter), Modifiers::NONE, false, false)];
        frame(&ctx, 0.4, Modifiers::NONE, release, |ui| ui.memory_mut(|m| m.surrender_focus(id)));
        let ev = vec![key_event(Key::Enter, Some(Key::Enter), Modifiers::NONE, true, false)];
        assert!(frame(&ctx, 0.5, Modifiers::NONE, ev, |ui| s.fired(ui.ctx(), "take")));
    }

    #[test]
    fn hold_tracker_fires_once_after_holding() {
        let ctx = Context::default();
        let chords = [c("hold Ctrl+Esc")];
        let ctrl = Modifiers { ctrl: true, command: true, ..Default::default() };
        let mut t = HoldTracker::default();
        let press = vec![key_event(Key::Escape, Some(Key::Escape), ctrl, true, false)];
        let (st, esc_seen) = frame(&ctx, 10.0, ctrl, press, |ui| (t.update(ui.ctx(), &chords, HOLD_SECS), c("Ctrl+Esc").pressed(ui.ctx())));
        assert_eq!(st, HoldState::Holding(0.0));
        assert!(!esc_seen, "held chord's press is consumed");
        assert!(matches!(frame(&ctx, 10.5, ctrl, vec![], |ui| t.update(ui.ctx(), &chords, HOLD_SECS)), HoldState::Holding(p) if (p - 0.5).abs() < 1e-6));
        assert_eq!(frame(&ctx, 11.0, ctrl, vec![], |ui| t.update(ui.ctx(), &chords, HOLD_SECS)), HoldState::Fired);
        assert_eq!(frame(&ctx, 12.0, ctrl, vec![], |ui| t.update(ui.ctx(), &chords, HOLD_SECS)), HoldState::Idle, "fires once per press");
        // Releasing Ctrl early cancels.
        let release = vec![key_event(Key::Escape, Some(Key::Escape), ctrl, false, false)];
        assert_eq!(frame(&ctx, 12.1, ctrl, release, |ui| t.update(ui.ctx(), &chords, HOLD_SECS)), HoldState::Idle);
        let press = vec![key_event(Key::Escape, Some(Key::Escape), ctrl, true, false)];
        assert_eq!(frame(&ctx, 13.0, ctrl, press, |ui| t.update(ui.ctx(), &chords, HOLD_SECS)), HoldState::Holding(0.0));
        assert_eq!(frame(&ctx, 13.5, Modifiers::NONE, vec![], |ui| t.update(ui.ctx(), &chords, HOLD_SECS)), HoldState::Idle);
        assert_eq!(frame(&ctx, 14.5, ctrl, vec![], |ui| t.update(ui.ctx(), &chords, HOLD_SECS)), HoldState::Holding(0.0), "restarts from zero");
    }

    #[test]
    fn capture_reports_physical_digit_for_shifted_numbers() {
        let ctx = Context::default();
        let shift = Modifiers { shift: true, ..Default::default() };
        let ev =
            vec![key_event(Key::ShiftLeft, Some(Key::ShiftLeft), shift, true, false), key_event(Key::Exclamationmark, Some(Key::Num1), shift, true, false)];
        assert_eq!(frame(&ctx, 0.0, shift, ev, |ui| Chord::capture(ui.ctx())), Some(c("Shift+1")));
        assert_eq!(frame(&ctx, 0.1, Modifiers::NONE, vec![], |ui| Chord::capture(ui.ctx())), None);
    }
}
