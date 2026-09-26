//! One sandboxed Lua VM per script patch (§3.4, §6.4).
//!
//! * LuaJIT runs with its JIT compiler **off**: compiled traces never call debug hooks, so an
//!   infinite loop in a trace could not be interrupted. The interpreter keeps every budget
//!   enforceable.
//! * No `os`, `io`, `debug`, `package`, `ffi`, `jit`, bytecode loading, or `string.dump`;
//!   `require` only loads `.lua` text files from the patch folder.
//! * A count hook every [`HOOK_EVERY`] instructions enforces the per-call instruction cap, the
//!   per-call wall-time cap, and the heap cap. Once tripped it keeps failing until the call
//!   returns, so `pcall` cannot swallow it.

use super::convert::{from_lua, to_lua};
use super::draw::{self, DrawBuf};
use crate::manifest::Manifest;
use crate::wgsl::STANDARD_SIGNALS;
use mlua::chunk::ChunkMode;
use mlua::{Function, HookTriggers, Lua, LuaOptions, StdLib, Table, Value as Lv, VmState};
use se_hub::{Hub, Snapshot};
use se_proto::{Command, Ease, Event, Id, Op, Origin, Value, ValueType, address};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Instructions between hook calls.
pub const HOOK_EVERY: u32 = 1000;
/// Commands (`set`, `emit`, …) a patch may issue per tick.
pub const MAX_COMMANDS_PER_TICK: u32 = 256;
/// Signal updates a patch may publish per tick.
pub const MAX_SIGNALS_PER_TICK: u32 = 64;
/// Log lines a patch may write per tick.
pub const MAX_LOGS_PER_TICK: u32 = 20;
/// Largest string `string.rep` may build (bytes).
const MAX_REP: usize = 16 << 20;

/// Palette slots (Lua key, state address, default).
pub const PALETTE: [(&str, &str, [f32; 4]); 8] = [
    ("accent", "palette.accent", [0.49, 0.64, 0.97, 1.0]),
    ("background", "palette.background", [0.07, 0.07, 0.09, 1.0]),
    ("foreground", "palette.foreground", [0.93, 0.93, 0.95, 1.0]),
    ("red", "palette.red", [0.97, 0.33, 0.38, 1.0]),
    ("yellow", "palette.yellow", [0.98, 0.80, 0.33, 1.0]),
    ("green", "palette.green", [0.45, 0.85, 0.45, 1.0]),
    ("cyan", "palette.cyan", [0.35, 0.85, 0.90, 1.0]),
    ("magenta", "palette.magenta", [0.80, 0.45, 0.95, 1.0]),
];

/// Resource limits for one patch.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    /// Soft CPU budget per tick (all callbacks), ms.
    pub cpu_ms: f64,
    /// Hard wall-time cap for a single callback, ms.
    pub hard_ms: f64,
    /// Hard instruction cap for a single callback.
    pub instructions: u64,
    /// Heap cap, bytes.
    pub memory: usize,
    /// Caps for running the top-level chunk at load.
    pub load_ms: f64,
    pub load_instructions: u64,
}

impl Limits {
    pub const DEFAULT_CPU_MS: f64 = 2.0;
    pub const DEFAULT_INSTRUCTIONS: u64 = 20_000_000;
    pub const DEFAULT_MEMORY_MB: f64 = 128.0;

    pub fn from_manifest(m: &Manifest) -> Limits {
        let cpu_ms = m.budget.cpu_ms.filter(|v| v.is_finite() && *v > 0.0).unwrap_or(Self::DEFAULT_CPU_MS);
        let instructions = m.budget.instructions.filter(|v| *v > 0).unwrap_or(Self::DEFAULT_INSTRUCTIONS);
        let memory_mb = m.budget.memory_mb.filter(|v| v.is_finite() && *v > 0.0).unwrap_or(Self::DEFAULT_MEMORY_MB);
        Limits {
            cpu_ms,
            hard_ms: (cpu_ms * 10.0).max(25.0),
            instructions,
            memory: (memory_mb * 1024.0 * 1024.0) as usize,
            load_ms: 500.0,
            load_instructions: instructions.saturating_mul(10),
        }
    }
}

/// Budget state shared with the hook.
struct Budget {
    start: Cell<Instant>,
    instr: Cell<u64>,
    max_instr: Cell<u64>,
    hard: Cell<Duration>,
    memory: usize,
    hooks: Cell<u32>,
    tripped: RefCell<Option<String>>,
}

impl Budget {
    fn begin(&self, max_instr: u64, hard_ms: f64) {
        self.start.set(Instant::now());
        self.instr.set(0);
        self.max_instr.set(max_instr);
        self.hard.set(Duration::from_secs_f64(hard_ms / 1000.0));
        *self.tripped.borrow_mut() = None;
    }

    fn trip(&self, why: String) -> mlua::Error {
        let e = mlua::Error::runtime(why.clone());
        self.tripped.borrow_mut().get_or_insert(why);
        e
    }
}

/// Why a call failed.
#[derive(Clone, Debug, PartialEq)]
pub enum CallError {
    /// Budget exceeded (the patch must be suspended).
    Budget(String),
    /// A Lua error, formatted `file:line: message`.
    Script(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Budget(s) | CallError::Script(s) => f.write_str(s),
        }
    }
}

/// State shared by the API closures.
struct Shared {
    id: String,
    hub: Arc<Hub>,
    cmds: Cell<u32>,
    signals: Cell<u32>,
    logs: Cell<u32>,
    log_dropped: Cell<bool>,
    cause: Cell<Option<Id>>,
    handlers: RefCell<Vec<(String, Function)>>,
    budget: Budget,
}

impl Shared {
    fn command(&self, op: Op) -> mlua::Result<()> {
        let n = self.cmds.get();
        if n >= MAX_COMMANDS_PER_TICK {
            return Err(mlua::Error::runtime(format!("too many commands this tick (max {MAX_COMMANDS_PER_TICK})")));
        }
        self.cmds.set(n + 1);
        let mut c = Command::new(Origin::Patch, op).with_key(format!("patch:{}", self.id));
        c.causal = self.cause.get();
        self.hub.command(c);
        Ok(())
    }

    fn log(&self, level: &str, msg: String) {
        let n = self.logs.get();
        if n < MAX_LOGS_PER_TICK {
            self.logs.set(n + 1);
            self.hub.log(level, &format!("patch.{}", self.id), msg);
        } else if !self.log_dropped.replace(true) {
            self.hub.log("warn", &format!("patch.{}", self.id), format!("log rate limit ({MAX_LOGS_PER_TICK}/tick) reached; dropping lines"));
        }
    }
}

/// Where a signal leaf lives in the `signals` table.
struct Leaf {
    table: Table,
    key: mlua::LuaString,
    name: String,
    slot: Option<usize>,
}

/// Cached snapshot ids for the addresses a VM reads every tick.
struct Watched {
    addr: String,
    key: mlua::LuaString,
    ty: ValueType,
    id: Option<usize>,
    last: Option<Value>,
}

/// Inputs the VM reads from the engine every tick.
pub struct Inputs<'a> {
    pub snap: &'a Snapshot,
    pub resolution: [u32; 2],
}

pub struct ScriptVm {
    lua: Lua,
    sh: Rc<Shared>,
    draw: Rc<RefCell<DrawBuf>>,
    frame_fn: Option<Function>,
    params: Vec<Watched>,
    palette: Vec<(Watched, [f32; 4])>,
    params_tbl: Table,
    palette_tbl: Table,
    signals_tbl: Table,
    patch_tbl: Table,
    leaves: Vec<Leaf>,
    signal_names: Option<Arc<Vec<String>>>,
    extra_signals: Vec<String>,
    env_addr: String,
    env_id: Option<usize>,
    snap_generation: u64,
    trigger_event: String,
    limits: Limits,
    started: Instant,
    frames: u64,
    k_env: mlua::LuaString,
    k_time: mlua::LuaString,
    k_trigger: mlua::LuaString,
    k_frame: mlua::LuaString,
    /// Metatable giving color/vector arrays named components (`.r .g .b .a`, `.x .y .z .w`).
    vec_mt: Table,
}

fn fmt_error(e: &mlua::Error) -> String {
    match e {
        mlua::Error::SyntaxError { message, .. } => message.lines().next().unwrap_or_default().to_string(),
        mlua::Error::RuntimeError(m) => m.lines().next().unwrap_or_default().to_string(),
        mlua::Error::CallbackError { traceback, cause } => {
            let msg = fmt_error(cause);
            if has_location(&msg) {
                return msg;
            }
            // locate the Lua line that called into the API
            match traceback.lines().map(str::trim).find_map(lua_location) {
                Some(loc) => format!("{loc}: {msg}"),
                None => msg,
            }
        }
        mlua::Error::WithContext { cause, .. } => fmt_error(cause),
        mlua::Error::BadArgument { pos, name, cause, .. } => match name {
            Some(n) => format!("bad argument `{n}`: {}", fmt_error(cause)),
            None => format!("bad argument #{pos}: {}", fmt_error(cause)),
        },
        mlua::Error::MemoryError(m) => format!("out of memory: {m}"),
        other => other.to_string().lines().next().unwrap_or_default().to_string(),
    }
}

/// `main.lua:12` at the start of a traceback line.
fn lua_location(line: &str) -> Option<String> {
    let (file, rest) = line.split_once(':')?;
    if !file.ends_with(".lua") {
        return None;
    }
    let n: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    (!n.is_empty()).then(|| format!("{file}:{n}"))
}

fn has_location(msg: &str) -> bool {
    lua_location(msg).is_some()
}

/// Split `file:line: message` (also `file:line:col: message`) into parts.
pub fn split_location(msg: &str) -> Option<(String, u32)> {
    let mut it = msg.splitn(3, ':');
    let file = it.next()?.trim();
    let line: u32 = it.next()?.trim().parse().ok()?;
    if file.is_empty() || file.contains(' ') {
        return None;
    }
    Some((file.to_string(), line))
}

fn sanitize_module(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.len() > 128 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-') {
        return None;
    }
    let rel: PathBuf = name.split('.').collect();
    Some(rel.with_extension("lua"))
}

impl ScriptVm {
    /// Build a VM for `m` and run its top-level chunk. On failure the error is formatted
    /// `file:line: message`.
    pub fn load(m: &Manifest, hub: Arc<Hub>, inputs: &Inputs) -> Result<ScriptVm, CallError> {
        let src = std::fs::read_to_string(m.entry_path()).map_err(|e| CallError::Script(format!("{}:0: cannot read: {e}", m.entry)))?;
        let limits = Limits::from_manifest(m);
        let lua = Lua::new_with(StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::BIT | StdLib::JIT, LuaOptions::default())
            .map_err(|e| CallError::Script(format!("{}:0: cannot create VM: {e}", m.entry)))?;
        let sh = Rc::new(Shared {
            id: m.id.clone(),
            hub,
            cmds: Cell::new(0),
            signals: Cell::new(0),
            logs: Cell::new(0),
            log_dropped: Cell::new(false),
            cause: Cell::new(None),
            handlers: RefCell::new(Vec::new()),
            budget: Budget {
                start: Cell::new(Instant::now()),
                instr: Cell::new(0),
                max_instr: Cell::new(limits.load_instructions),
                hard: Cell::new(Duration::from_secs_f64(limits.load_ms / 1000.0)),
                memory: limits.memory,
                hooks: Cell::new(0),
                tripped: RefCell::new(None),
            },
        });
        let draw = Rc::new(RefCell::new(DrawBuf::default()));
        let mut vm = Self::build(lua, sh, draw, m, limits).map_err(|e| CallError::Script(format!("{}:0: {}", m.entry, fmt_error(&e))))?;
        vm.refresh(inputs).map_err(|e| CallError::Script(format!("{}:0: {}", m.entry, fmt_error(&e))))?;
        let chunk = vm.lua.load(&src).set_name(format!("@{}", m.entry)).set_mode(ChunkMode::Text);
        vm.sh.budget.begin(limits.load_instructions, limits.load_ms);
        let r = chunk.exec();
        vm.check(r)?;
        vm.frame_fn = match vm.lua.globals().raw_get::<Lv>("frame") {
            Ok(Lv::Function(f)) => Some(f),
            Ok(Lv::Nil) => None,
            Ok(other) => return Err(CallError::Script(format!("{}:0: `frame` must be a function, not a {}", m.entry, other.type_name()))),
            Err(e) => return Err(CallError::Script(fmt_error(&e))),
        };
        // the top-level chunk may have drawn or issued commands; start the first tick clean
        vm.draw.borrow_mut().reset();
        Ok(vm)
    }

    fn build(lua: Lua, sh: Rc<Shared>, draw_buf: Rc<RefCell<DrawBuf>>, m: &Manifest, limits: Limits) -> mlua::Result<ScriptVm> {
        let g = lua.globals();
        // --- sandbox -----------------------------------------------------------------------
        lua.load("jit.off() jit.flush()").exec()?;
        for name in [
            "jit",
            "dofile",
            "loadfile",
            "load",
            "loadstring",
            "require",
            "module",
            "gcinfo",
            "newproxy",
            "collectgarbage",
            "setfenv",
            "getfenv",
            "package",
            "debug",
            "io",
            "os",
            "ffi",
        ] {
            g.raw_set(name, Lv::Nil)?;
        }
        let string: Table = g.raw_get("string")?;
        string.raw_set("dump", Lv::Nil)?;
        let rep: Function = string.raw_get("rep")?;
        string.raw_set(
            "rep",
            lua.create_function(move |_, (s, n, sep): (mlua::LuaString, i64, Option<mlua::LuaString>)| {
                let unit = s.as_bytes().len() + sep.as_ref().map(|x| x.as_bytes().len()).unwrap_or(0);
                if n > 0 && unit.saturating_mul(n as usize) > MAX_REP {
                    return Err(mlua::Error::runtime(format!("string.rep result too large (max {} MB)", MAX_REP >> 20)));
                }
                rep.call::<mlua::LuaString>((s, n, sep))
            })?,
        )?;
        g.raw_set(
            "collectgarbage",
            lua.create_function(|lua, opt: Option<String>| match opt.as_deref().unwrap_or("collect") {
                "count" => Ok(Lv::Number(lua.used_memory() as f64 / 1024.0)),
                "collect" | "step" => {
                    lua.gc_collect()?;
                    Ok(Lv::Integer(0))
                }
                other => Err(mlua::Error::runtime(format!("collectgarbage(\"{other}\") is not available in patches"))),
            })?,
        )?;
        let seed = (se_clock::now() ^ (m.id.bytes().fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(b as u64)))) % 2_147_483_647;
        let math: Table = g.raw_get("math")?;
        math.get::<Function>("randomseed")?.call::<()>(seed as f64)?;

        // --- budget hook ---------------------------------------------------------------------
        let hook_sh = Rc::downgrade(&sh);
        // a *global* hook: LuaJIT hooks are per VM, and mlua's per-thread hooks would be removed
        // (for everyone) the first time a coroutine runs
        lua.set_global_hook(HookTriggers::new().every_nth_instruction(HOOK_EVERY), move |lua, _| {
            let Some(sh) = hook_sh.upgrade() else { return Ok(VmState::Continue) };
            let b = &sh.budget;
            if let Some(why) = b.tripped.borrow().clone() {
                return Err(mlua::Error::runtime(why));
            }
            let n = b.instr.get() + HOOK_EVERY as u64;
            b.instr.set(n);
            if n > b.max_instr.get() {
                return Err(b.trip(format!("instruction budget exceeded (> {} instructions in one call)", b.max_instr.get())));
            }
            if b.start.get().elapsed() > b.hard.get() {
                return Err(b.trip(format!("time budget exceeded (one call ran > {:.0} ms)", b.hard.get().as_secs_f64() * 1000.0)));
            }
            let h = b.hooks.get().wrapping_add(1);
            b.hooks.set(h);
            if h % 32 == 0 && lua.used_memory() > b.memory {
                return Err(b.trip(format!("memory budget exceeded (> {} MB)", b.memory >> 20)));
            }
            Ok(VmState::Continue)
        })?;

        // --- API -------------------------------------------------------------------------------
        let s = sh.clone();
        let trigger_event = format!("patch.{}.trigger", m.id);
        let te = trigger_event.clone();
        g.raw_set(
            "on",
            lua.create_function(move |_, (pattern, f): (String, Function)| {
                let p = if pattern == "trigger" { te.clone() } else { pattern };
                if !address::is_valid(&p, true) {
                    return Err(mlua::Error::runtime(format!("on: bad event pattern `{p}`")));
                }
                s.handlers.borrow_mut().push((p, f));
                Ok(())
            })?,
        )?;

        let s = sh.clone();
        g.raw_set(
            "get",
            lua.create_function(move |lua, addr: String| {
                let snap = s.hub.snapshot.load();
                match snap.get(&addr) {
                    Some(v) => to_lua(lua, v),
                    None => match snap.signal(&addr) {
                        Some(x) => Ok(Lv::Number(x as f64)),
                        None => Ok(Lv::Nil),
                    },
                }
            })?,
        )?;

        let s = sh.clone();
        g.raw_set(
            "set",
            lua.create_function(move |_, (addr, v): (String, Lv)| {
                let value = from_lua(&v).map_err(mlua::Error::runtime)?;
                if !address::is_valid(&addr, false) {
                    return Err(mlua::Error::runtime(format!("set: bad address `{addr}`")));
                }
                s.command(Op::Set { address: addr, value })
            })?,
        )?;

        let s = sh.clone();
        g.raw_set(
            "animate",
            lua.create_function(move |_, (addr, to, ms, ease): (String, Lv, f64, Option<String>)| {
                let to = from_lua(&to).map_err(mlua::Error::runtime)?;
                if !address::is_valid(&addr, false) {
                    return Err(mlua::Error::runtime(format!("animate: bad address `{addr}`")));
                }
                let ease = match ease.as_deref() {
                    None => Ease::default(),
                    Some(e) => Ease::parse(e).ok_or_else(|| mlua::Error::runtime(format!("animate: unknown easing `{e}`")))?,
                };
                s.command(Op::Animate { address: addr, to, ms: ms.clamp(0.0, 3_600_000.0) as u32, ease })
            })?,
        )?;

        let s = sh.clone();
        g.raw_set(
            "emit",
            lua.create_function(move |_, (ty, payload): (String, Option<Lv>)| {
                let payload = match payload {
                    Some(p) => from_lua(&p).map_err(mlua::Error::runtime)?,
                    None => Value::Null,
                };
                if !address::is_valid(&ty, false) {
                    return Err(mlua::Error::runtime(format!("emit: bad event type `{ty}`")));
                }
                s.command(Op::Emit { ty, payload })
            })?,
        )?;

        let s = sh.clone();
        g.raw_set(
            "trigger",
            lua.create_function(move |_, (addr, payload): (String, Option<Lv>)| {
                let payload = match payload {
                    Some(p) => from_lua(&p).map_err(mlua::Error::runtime)?,
                    None => Value::Null,
                };
                if !address::is_valid(&addr, false) {
                    return Err(mlua::Error::runtime(format!("trigger: bad address `{addr}`")));
                }
                s.command(Op::Trigger { address: addr, payload })
            })?,
        )?;

        let s = sh.clone();
        g.raw_set(
            "cmd",
            lua.create_function(move |_, text: String| {
                let op = Op::parse(&text).map_err(|e| mlua::Error::runtime(format!("cmd: {e}")))?;
                s.command(op)
            })?,
        )?;

        let s = sh.clone();
        g.raw_set(
            "signal",
            lua.create_function(move |_, (name, v): (String, f64)| {
                let own = format!("patch.{}", s.id);
                let full = if name.contains('.') { name } else { format!("{own}.{name}") };
                if !(full.starts_with(&own) && full[own.len()..].starts_with('.')) || !address::is_valid(&full, false) {
                    return Err(mlua::Error::runtime(format!("signal: `{full}` is outside this patch's namespace `{own}.*`")));
                }
                let n = s.signals.get();
                if n >= MAX_SIGNALS_PER_TICK {
                    return Err(mlua::Error::runtime(format!("too many signal updates this tick (max {MAX_SIGNALS_PER_TICK})")));
                }
                s.signals.set(n + 1);
                if !v.is_finite() {
                    return Err(mlua::Error::runtime("signal: value must be a finite number"));
                }
                s.hub.signal(&full, v as f32);
                Ok(())
            })?,
        )?;

        let log = lua.create_table()?;
        for level in ["info", "warn", "error"] {
            let s = sh.clone();
            log.raw_set(
                level,
                lua.create_function(move |_, args: mlua::Variadic<Lv>| {
                    let msg = args.iter().map(display).collect::<Vec<_>>().join(" ");
                    s.log(level, msg);
                    Ok(())
                })?,
            )?;
        }
        let s = sh.clone();
        g.raw_set(
            "print",
            lua.create_function(move |_, args: mlua::Variadic<Lv>| {
                s.log("info", args.iter().map(display).collect::<Vec<_>>().join(" "));
                Ok(())
            })?,
        )?;
        g.raw_set("log", log)?;

        let draw_tbl = draw::install(&lua, draw_buf.clone())?;
        g.raw_set("draw", draw_tbl)?;

        // patch-local `require` (text chunks only)
        let dir = m.dir.clone();
        let loaded = lua.create_table()?;
        g.raw_set(
            "require",
            lua.create_function(move |lua, name: String| {
                if let Some(v) = loaded.raw_get::<Option<Lv>>(name.as_str())? {
                    return Ok(v);
                }
                let rel = sanitize_module(&name).ok_or_else(|| mlua::Error::runtime(format!("require: bad module name `{name}`")))?;
                let path = dir.join(&rel);
                let src = std::fs::read_to_string(&path).map_err(|e| mlua::Error::runtime(format!("require `{name}`: {e}")))?;
                let v: Lv = lua.load(&src).set_name(format!("@{}", rel.display())).set_mode(ChunkMode::Text).call(())?;
                let v = if v.is_nil() { Lv::Boolean(true) } else { v };
                loaded.raw_set(name.as_str(), v.clone())?;
                Ok(v)
            })?,
        )?;

        // --- live tables -------------------------------------------------------------------
        let vec_mt: Table = lua
            .load("local idx = { r = 1, g = 2, b = 3, a = 4, x = 1, y = 2, z = 3, w = 4 } return { __index = function(t, k) local i = idx[k] if i then return rawget(t, i) end end }")
            .set_name("=vec")
            .eval()?;
        let params_tbl = lua.create_table()?;
        let palette_tbl = lua.create_table()?;
        let signals_tbl = lua.create_table()?;
        let patch_tbl = lua.create_table()?;
        patch_tbl.raw_set("id", m.id.as_str())?;
        patch_tbl.raw_set("label", m.label.as_str())?;
        g.raw_set("params", params_tbl.clone())?;
        g.raw_set("palette", palette_tbl.clone())?;
        g.raw_set("signals", signals_tbl.clone())?;
        g.raw_set("patch", patch_tbl.clone())?;
        g.raw_set("env", 0.0)?;
        g.raw_set("time", 0.0)?;

        let params = m
            .params
            .iter()
            .map(|(n, p)| Ok(Watched { addr: format!("patch.{}.{n}", m.id), key: lua.create_string(n)?, ty: p.value_type(), id: None, last: None }))
            .collect::<mlua::Result<Vec<_>>>()?;
        for (w, (_, p)) in params.iter().zip(&m.params) {
            params_tbl.raw_set(&w.key, typed_to_lua(&lua, &vec_mt, &p.default_value(), w.ty)?)?;
        }
        let palette = PALETTE
            .iter()
            .map(|(k, a, d)| {
                let key = lua.create_string(k)?;
                palette_tbl.raw_set(&key, color_table(&lua, &vec_mt, *d)?)?;
                Ok((Watched { addr: a.to_string(), key, ty: ValueType::Color, id: None, last: None }, *d))
            })
            .collect::<mlua::Result<Vec<_>>>()?;

        let mut extra_signals: Vec<String> = STANDARD_SIGNALS.iter().map(|s| s.to_string()).collect();
        for s in &m.signals {
            if !extra_signals.contains(s) {
                extra_signals.push(s.clone());
            }
        }

        Ok(ScriptVm {
            k_env: lua.create_string("env")?,
            k_time: lua.create_string("time")?,
            k_trigger: lua.create_string("trigger")?,
            k_frame: lua.create_string("frame")?,
            vec_mt,
            lua,
            sh,
            draw: draw_buf,
            frame_fn: None,
            params,
            palette,
            params_tbl,
            palette_tbl,
            signals_tbl,
            patch_tbl,
            leaves: Vec::new(),
            signal_names: None,
            extra_signals,
            env_addr: format!("patch.{}.env", m.id),
            env_id: None,
            snap_generation: u64::MAX,
            trigger_event,
            limits,
            started: Instant::now(),
            frames: 0,
        })
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Event patterns this script subscribed to with `on()`.
    pub fn patterns(&self) -> Vec<String> {
        let mut p: Vec<String> = self.sh.handlers.borrow().iter().map(|(p, _)| p.clone()).collect();
        p.dedup();
        p
    }

    pub fn has_frame(&self) -> bool {
        self.frame_fn.is_some()
    }

    pub fn used_memory(&self) -> usize {
        self.lua.used_memory()
    }

    /// Start of a tick: reset per-tick rate limits.
    pub fn begin_tick(&self) {
        self.sh.cmds.set(0);
        self.sh.signals.set(0);
        self.sh.logs.set(0);
        self.sh.log_dropped.set(false);
    }

    /// Copy params, palette, env, signals, and time from the snapshot into the Lua tables.
    pub fn refresh(&mut self, inputs: &Inputs) -> mlua::Result<()> {
        let snap = inputs.snap;
        if snap.generation != self.snap_generation {
            self.snap_generation = snap.generation;
            for w in self.params.iter_mut().chain(self.palette.iter_mut().map(|(w, _)| w)) {
                w.id = snap.id(&w.addr);
            }
            self.env_id = snap.id(&self.env_addr);
        }
        for w in &mut self.params {
            if let Some(i) = w.id {
                let v = snap.value(i);
                if w.last.as_ref() != Some(v) {
                    self.params_tbl.raw_set(&w.key, typed_to_lua(&self.lua, &self.vec_mt, v, w.ty)?)?;
                    w.last = Some(v.clone());
                }
            }
        }
        for (w, default) in &mut self.palette {
            let v = w.id.map(|i| snap.value(i));
            if w.last.as_ref() != v {
                let c = v.and_then(Value::as_color).unwrap_or(*default);
                self.palette_tbl.raw_set(&w.key, color_table(&self.lua, &self.vec_mt, c)?)?;
                w.last = v.cloned();
            }
        }
        let env = self.env_id.and_then(|i| snap.value(i).as_f64()).unwrap_or(0.0);
        let g = self.lua.globals();
        g.raw_set(&self.k_env, env)?;
        g.raw_set(&self.k_time, self.started.elapsed().as_secs_f64())?;
        self.patch_tbl.raw_set(&self.k_frame, self.frames as f64)?;
        let [w, h] = inputs.resolution;
        self.patch_tbl.raw_set("resolution", {
            let t = self.lua.create_table_with_capacity(2, 0)?;
            t.raw_set(1, w)?;
            t.raw_set(2, h)?;
            t
        })?;
        self.patch_tbl.raw_set("aspect", w as f64 / h.max(1) as f64)?;

        let names_changed = match (&self.signal_names, &snap.signal_names) {
            (Some(a), b) => !Arc::ptr_eq(a, b),
            (None, _) => true,
        };
        if names_changed {
            self.rebuild_signals(snap)?;
        }
        for l in &self.leaves {
            let v = l.slot.and_then(|i| snap.signals.get(i).copied()).unwrap_or(0.0);
            l.table.raw_set(&l.key, v as f64)?;
        }
        Ok(())
    }

    fn rebuild_signals(&mut self, snap: &Snapshot) -> mlua::Result<()> {
        self.signal_names = Some(snap.signal_names.clone());
        for pair in self.signals_tbl.clone().pairs::<Lv, Lv>() {
            let (k, _) = pair?;
            self.signals_tbl.raw_set(k, Lv::Nil)?;
        }
        self.leaves.clear();
        let mut nodes: HashMap<String, Table> = HashMap::new();
        let mut leaf_at: HashMap<String, usize> = HashMap::new();
        let mut names: Vec<&str> = snap.signal_names.iter().map(String::as_str).collect();
        for s in &self.extra_signals {
            if !snap.signal_index.contains_key(s) {
                names.push(s);
            }
        }
        names.sort_unstable();
        names.dedup();
        for name in names {
            let segs: Vec<&str> = name.split('.').collect();
            let mut table = self.signals_tbl.clone();
            let mut path = String::new();
            for seg in &segs[..segs.len() - 1] {
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(seg);
                table = match nodes.get(&path) {
                    Some(t) => t.clone(),
                    None => {
                        let t = self.lua.create_table()?;
                        table.raw_set(*seg, t.clone())?;
                        // a leaf already sits at this path: move it into `.value`
                        if let Some(&li) = leaf_at.get(&path) {
                            self.leaves[li].table = t.clone();
                            self.leaves[li].key = self.lua.create_string("value")?;
                        }
                        nodes.insert(path.clone(), t.clone());
                        t
                    }
                };
            }
            let last = segs[segs.len() - 1];
            let (table, key) = if nodes.contains_key(name) { (nodes[name].clone(), "value") } else { (table, last) };
            leaf_at.insert(name.to_string(), self.leaves.len());
            self.leaves.push(Leaf { table, key: self.lua.create_string(key)?, name: name.to_string(), slot: snap.signal_index.get(name).copied() });
        }
        Ok(())
    }

    /// Number of signal leaves (for stats/tests).
    pub fn signal_names(&self) -> impl Iterator<Item = &str> {
        self.leaves.iter().map(|l| l.name.as_str())
    }

    fn check(&self, r: mlua::Result<()>) -> Result<(), CallError> {
        let tripped = self.sh.budget.tripped.borrow_mut().take();
        if let Some(why) = tripped {
            return Err(CallError::Budget(why));
        }
        r.map_err(|e| CallError::Script(fmt_error(&e)))
    }

    /// Run the handlers matching `e`. Returns the time spent.
    pub fn dispatch(&mut self, e: &Event) -> Result<Duration, CallError> {
        let handlers: Vec<Function> = self.sh.handlers.borrow().iter().filter(|(p, _)| address::matches(p, &e.ty)).map(|(_, f)| f.clone()).collect();
        if handlers.is_empty() {
            return Ok(Duration::ZERO);
        }
        let t0 = Instant::now();
        let payload = match &e.payload {
            Value::Map(_) => to_lua(&self.lua, &e.payload),
            Value::Null => self.lua.create_table().map(Lv::Table),
            other => self.lua.create_table().and_then(|t| {
                t.raw_set("value", to_lua(&self.lua, other)?)?;
                Ok(Lv::Table(t))
            }),
        }
        .map_err(|err| CallError::Script(fmt_error(&err)))?;
        let info = self.event_info(e).map_err(|err| CallError::Script(fmt_error(&err)))?;
        if e.ty == self.trigger_event {
            self.lua.globals().raw_set(&self.k_trigger, payload.clone()).map_err(|err| CallError::Script(fmt_error(&err)))?;
        }
        self.sh.cause.set(Some(e.id));
        let mut result = Ok(());
        for f in handlers {
            self.sh.budget.begin(self.limits.instructions, self.limits.hard_ms);
            let r = f.call::<()>((payload.clone(), info.clone()));
            if let Err(err) = self.check(r) {
                result = Err(err);
                break;
            }
        }
        self.sh.cause.set(None);
        result.map(|_| t0.elapsed())
    }

    fn event_info(&self, e: &Event) -> mlua::Result<Table> {
        let t = self.lua.create_table()?;
        t.raw_set("type", e.ty.as_str())?;
        t.raw_set("origin", e.origin.as_str())?;
        t.raw_set("ts", e.ts as f64 / 1e9)?;
        if let Some(a) = &e.actor {
            let at = self.lua.create_table()?;
            at.raw_set("platform", a.platform.as_str())?;
            at.raw_set("id", a.id.as_str())?;
            at.raw_set("name", a.name.as_str())?;
            let roles = self.lua.create_table()?;
            for (i, r) in a.roles.iter().enumerate() {
                roles.raw_set(i + 1, format!("{r:?}").to_lowercase())?;
            }
            at.raw_set("roles", roles)?;
            t.raw_set("actor", at)?;
        }
        Ok(t)
    }

    /// Call `frame(dt, signals)` and hand the resulting ops to `publish`. On error nothing is
    /// published (the previous list stays on screen).
    pub fn frame(&mut self, dt: f64, publish: impl FnOnce(&mut Vec<se_hub::draw::DrawOp>)) -> Result<Duration, CallError> {
        let Some(f) = self.frame_fn.clone() else { return Ok(Duration::ZERO) };
        let t0 = Instant::now();
        self.draw.borrow_mut().reset();
        self.sh.budget.begin(self.limits.instructions, self.limits.hard_ms);
        let r = f.call::<()>((dt, self.signals_tbl.clone()));
        self.check(r)?;
        self.frames += 1;
        let mut b = self.draw.borrow_mut();
        b.finish();
        publish(&mut b.ops);
        Ok(t0.elapsed())
    }
}

fn display(v: &Lv) -> String {
    match v {
        Lv::Nil => "nil".into(),
        Lv::Boolean(b) => b.to_string(),
        Lv::Integer(i) => i.to_string(),
        Lv::Number(n) => {
            if n.fract() == 0.0 && n.abs() < 1e15 {
                format!("{}", *n as i64)
            } else {
                n.to_string()
            }
        }
        Lv::String(s) => s.to_string_lossy(),
        Lv::Table(_) => match from_lua(v) {
            Ok(x) => serde_json::to_string(&serde_json::Value::from(&x)).unwrap_or_else(|_| "table".into()),
            Err(_) => "table".into(),
        },
        other => other.type_name().to_string(),
    }
}

fn color_table(lua: &Lua, mt: &Table, c: [f32; 4]) -> mlua::Result<Table> {
    let t = lua.create_table_with_capacity(4, 0)?;
    for (i, v) in c.into_iter().enumerate() {
        t.raw_set(i + 1, v as f64)?;
    }
    t.set_metatable(Some(mt.clone()))?;
    Ok(t)
}

/// Param values typed by their Meta: colors/vectors become arrays that also answer
/// `.r .g .b .a` / `.x .y .z .w`.
fn typed_to_lua(lua: &Lua, mt: &Table, v: &Value, ty: ValueType) -> mlua::Result<Lv> {
    match ty {
        ValueType::Color => Ok(Lv::Table(color_table(lua, mt, v.as_color().unwrap_or([1.0; 4]))?)),
        ValueType::Vec2 | ValueType::Vec4 => {
            let t = match v.as_list() {
                Some(l) => {
                    let t = lua.create_table_with_capacity(l.len(), 0)?;
                    for (i, x) in l.iter().enumerate() {
                        t.raw_set(i + 1, x.as_f64().unwrap_or(0.0))?;
                    }
                    t
                }
                None => lua.create_table()?,
            };
            t.set_metatable(Some(mt.clone()))?;
            Ok(Lv::Table(t))
        }
        ValueType::Bool => Ok(Lv::Boolean(v.truthy())),
        _ => to_lua(lua, v),
    }
}
