# Patch authoring guide

A **patch** is a folder in `project/patches/<id>/` with a `patch.toml` manifest and an entry
file. Save a file and the engine reloads the patch in place — no rebuild, no restart. If the
new version is broken, **the last good version keeps running** and the error (with its file
and line) shows in the UI, in `streamctl get patch.<id>.error`, and in the log.

Every patch gets the same addresses:

| Address | What |
|---|---|
| `patch.<id>` | the trigger (fire with `streamctl do patch.<id>.trigger tier=2`, rules, presets, deck keys, chat) |
| `patch.<id>.env` / `.active` | trigger envelope 0–1 (attack/hold/release from the manifest) and whether it is running |
| `patch.<id>.<param>` | every manifest param, typed from its `Meta` (the UI generates controls for them) |
| `patch.<id>.state` | `loaded` · `error` · `suspended` · `disabled` |
| `patch.<id>.error` | last error, `file:line: message` (empty when healthy) |
| event `patch.<id>.trigger` | emitted on every trigger with the trigger payload |
| `patch.<id>.payload.*` | numbers of the last trigger's payload (§4), read-only |

## 1. Create a patch

- **UI:** *Views → Patches → New patch* (id, kind, template, "open in editor").
- **CLI:** `streamctl do patch.new id=my_fx kind=script template=default open=true`
- **By hand:** make `patches/my_fx/` with a `patch.toml` and the entry file — it appears as soon as `patch.toml` exists.

Templates live in `<share>/templates/patches/<kind>/<template>/` (list them with
`streamctl query patch.templates`): `script/default`, `script/burst`, `shader/default` (source),
`shader/effect`, `shader/transition`, `particles/default`, `web/default`, `dsp/default`.
Text files may use `{{id}}` and `{{label}}` placeholders.

Other actions: `patch.reload [id]` (re-read from disk; no id = all), `patch.disable <id>`,
`patch.enable <id>` (also resumes a suspended script), `patch.open <id>` (editor). The editor
is `editor=` from the action, else `$STREAM_ENGINE_EDITOR`, `omarchy-launch-editor`, `$VISUAL`,
then `xdg-open`.

Then (§6.5): route it (scene node `{ src = "patch.<id>" }`, overlay layer, effect, or
transition), fire it from the simulator (`streamctl fire twitch.sub tier=2` with a rule), and bind
params to signals.

## 2. Kinds and layers

| Kind | Entry | Runs in | Use |
|---|---|---|---|
| `script` | `main.lua` | a Lua VM on its own thread; draw lists rendered by vello | logic + 2D drawing |
| `shader` | `main.wgsl` (`fs`) | the renderer | generative visuals, effects, transitions |
| `particles` | `sim.wgsl` + `draw.wgsl` | the renderer (compute + instanced draw) | thousands of particles |
| `web` | `index.html` | Chromium (CEF) off screen, served by the engine | HTML/CSS/Canvas/Three.js widgets |
| `dsp` | `main.wasm` | the audio graph (WebAssembly) | audio effects/generators |

| Layer | Meaning | How it is used |
|---|---|---|
| `source` | placeable in scenes | scene node `{ src = "patch.<id>", rect = [...] }` |
| `overlay` | global layer above the scene on every canvas | drawn while `patch.<id>.active` or `env > 0`; always if the patch has no trigger |
| `effect` | takes an input texture (`se_input`) | `fx = [{ name = "patch.<id>", amount = 0.8 }]` on a source, scene node, scene, `[render.canvas_fx.<canvas>]`, or `[render.output_fx.<canvas>]`; with a trigger it also runs canvas-wide while `env > 0` (so presets can `fx = [{ name = "patch.<id>" }]`). Also usable as a morph enter/exit style (`enter = "patch.<id>"`: the node in `se_input`, its box in `se.region`, presence in `se.progress`; docs/render.md) |
| `transition` | takes A (`se_input`), B (`se_input_b`), `se.progress` | `scene.take transition=patch.<id>`, a scene's `transitions.pool = [{ name = "patch.<id>" }]`, or `transitions/<name>.toml` with `kind = "shader"`, `shader = "patch.<id>"`, `ms = 700` |
| `audio-effect` | insert on a bus/input (`dsp` only) | audio chains `{ patch = "<id>" }` |
| `audio-source` | generates audio (`dsp` only) | `[audio.sources."patch.<id>"] bus = "sfx"` |

## 3. Manifest (`patch.toml`)

```toml
kind        = "script"            # shader | particles | script | web | dsp
entry       = "main.lua"          # default per kind: main.lua, main.wgsl, sim.wgsl, index.html, main.wasm
layer       = "overlay"           # source | overlay | effect | transition | audio-effect | audio-source
label       = "Sub meteors"       # UI name (default: the folder name)
description = "Meteor shower scaled by sub tier"
params.count = { type = "int",   default = 40,  range = [1, 400], description = "Meteors per tier" }
params.speed = { type = "float", default = 1.0, range = [0.1, 4.0], unit = "x" }
params.tint  = { type = "color", default = "#ffaa33" }
params.mode  = { type = "enum",  options = ["rain", "burst"] }
trigger = { attack = "100ms", hold = "4s", release = "1s", retrigger = "stack" }
budget  = { cpu_ms = 1.0 }        # scripts: + instructions, memory_mb; shaders: gpu_ms
signals = ["twitch.chat_rate"]    # extra signals (shaders get s_<name>(); scripts get them as 0 until published)
size    = [1920, 1080]            # render size for source/overlay web and script layers (optional)
fps     = 60                      # web pages: CEF frame rate 1–120 (default 60)
grants  = ["lights.*", "source.cam1"]   # web pages: extra things the page may control (see §8)
particles = { count = 4096, sim = "sim.wgsl", draw = "draw.wgsl" }   # particles only
```

- The folder name is the id: one address segment (`a-z`, `0-9`, `_`, `-`), no dots.
- Param types: `float` (default), `int`, `bool`, `color` (`"#rrggbb[aa]"` or `[r,g,b,a]`), `vec2`, `vec4`, `enum` (`options`), `string`, `texture`. Reserved names: `env`, `active`, `error`, `trigger`.
- `trigger`: `attack`/`hold`/`release` durations (`hold` absent = until released), `retrigger` = `stack` (default) · `replace` · `queue` · `reject`. The payload may override `hold`/`attack`/`release` per fire.
- Manifest errors are reported as `patch.toml:<line>: message`; the previous manifest stays live.

## 4. Inputs every patch receives (§6.3)

`time`, `dt`, `frame`; `env` (trigger envelope 0–1); `trigger` (payload of the last trigger:
`user`, `amount`, `tier`, `message`, …); `params.*`; `signals.*` (all registered signals plus
the standard set); `palette.*` (the stream palette: `accent`, `background`, `foreground`, `red`,
`yellow`, `green`, `cyan`, `magenta` — from the `palette.*` addresses); `resolution`; the input
texture(s) for effect/transition layers.

Shader, particles, and dsp patches can't read text, so they get the payload's numbers
(`se.trigger.*` in WGSL, the params block in dsp modules, `patch.<id>.payload.*` in state):

| Field | Value |
|---|---|
| `amount` | how big the event was: payload `amount`, else `bits`, `count`, `viewers` |
| `bits`, `tier`, `months`, `viewers`, `count` | the payload's numbers (numeric text counts too), 0 when missing |
| `user_hash` | 0–1, the same for the same user every time (0 without a user) |
| `user_color` | the user's chat colour (payload `color`), else a colour picked by `user_hash`; transparent without a user |

A rule passes the event's numbers along: `do = ["patch.sparks.trigger bits={event.bits}"]` (the
user comes from the event). The payload is set in the same tick as `patch.<id>.active`, so a
patch never sees a new trigger with the previous payload; it stays until the next trigger.

Standard signals (always present, 0 until something publishes them): `band.level`,
`band.bass`, `band.mid`, `band.high`, `band.kick`, `band.snare`, `band.hat`, `band.centroid`,
`music.level`, `music.bass`, `music.mid`, `music.high`, `beat.phase`, `beat.bpm`, `mic.level`,
`lfo.slow`, `lfo.mid`, `lfo.fast`, `lfo.beat`, `lfo.bar`, `lfo.random`.

## 5. Lua scripts (`kind = "script"`)

LuaJIT (Lua 5.1 syntax) in **interpreter mode** — the JIT is off so that every budget can
interrupt any loop. One VM and one thread per patch: a runaway script only ever blocks itself.

```lua
-- patches/sub_meteors/main.lua (PLAN §6.4)
local rocks = {}
on("trigger", function(e)
  for _ = 1, params.count * (e.tier or 1) do
    rocks[#rocks + 1] = { x = math.random(), y = -0.1, v = 0.3 + math.random() * params.speed }
  end
  emit("lights.flash", { color = palette.accent, ms = 400 })
end)
function frame(dt, s)
  draw.clear()
  for i = #rocks, 1, -1 do
    local r = rocks[i]
    r.y = r.y + r.v * dt * (1 + s.music.bass)
    if r.y > 1.1 then table.remove(rocks, i)
    else draw.circle(r.x, r.y, 0.006, palette.accent, env) end
  end
end
```

### Callbacks

| | |
|---|---|
| `function frame(dt, s)` | called at the render rate (the highest canvas fps). `dt` in seconds (≤ 0.25), `s` = the signals table. Whatever `draw.*` produced is published as this frame's draw list (slot `patch.<id>`); if `frame` errors, the previous list stays on screen. |
| `on(pattern, fn)` | subscribe to events. `"trigger"` = this patch was triggered (`patch.<id>.trigger`); otherwise an event-type glob (`twitch.*`, `band.kick`, `alert.**`). `fn(payload, ev)`: `payload` is the event payload table (non-table payloads arrive as `{ value = … }`); `ev = { type, origin, ts, actor = { platform, id, name, roles } }`. Handlers run as soon as the event reaches the engine. |

### Globals (refreshed before every callback)

| | |
|---|---|
| `params` | current values of `patch.<id>.<param>` (resolved: base → scene → bindings → overrides). Colors and vectors are arrays `{r, g, b, a}` that also answer `.r .g .b .a` / `.x .y .z .w`. Assigning to `params` does nothing — use `set()`. |
| `palette` | the stream palette (color arrays as above) |
| `env` | this patch's trigger envelope 0–1 |
| `trigger` | payload of the most recent trigger (or `nil`) |
| `signals` | same table as `s` in `frame`: nested by name (`s.band.kick`, `s.twitch.chat_rate`). A name that is both a value and a prefix (`band.kick` and `band.kick.env`) is `s.band.kick.value` + `s.band.kick.env`. |
| `time` | seconds since this version was loaded |
| `patch` | `{ id, label, frame, resolution = { w, h }, aspect }` (resolution = manifest `size` or the main canvas) |

### Functions

| | |
|---|---|
| `get(address)` | resolved state value (or a signal value) |
| `set(address, value)` | override at patch priority (origin `patch`, layer key `patch:<id>`) |
| `animate(address, to, ms, ease?)` | eases: `linear`, `in_quad`, `out_quad`, `in_out_quad`, `in_cubic`, `out_cubic`, `in_out_cubic`, `smoothstep`, `out_back`, `step` |
| `emit(type, payload?)` | fire an event into the core (rules can react: `when = "hype.peak"`) |
| `trigger(address, payload?)` | fire any triggerable address |
| `cmd(text)` | any one-line command: `cmd("preset.fire hype")`, `cmd("lights.cue chase_fast")` |
| `signal(name, value)` | publish a signal under `patch.<id>.<name>` (bind params, drive lights) |
| `log.info(...)`, `log.warn(...)`, `log.error(...)`, `print(...)` | to the engine log (target `patch.<id>`) |
| `require("module")` | load `module.lua` (dots = folders) from the patch folder, text only |

Commands issued inside an event handler carry that event as their cause, so `streamctl trace`
shows the chain.

### Drawing (`draw.*`)

Coordinates are 0–1 of the layer ((0,0) top-left, (1,1) bottom-right); sizes, radii, stroke
widths, and text sizes are fractions of the layer **height**. Colors: `{r,g,b[,a]}`,
`{r=,g=,b=,a=}`, `"#rrggbb[aa]"`, a palette entry, or a gray number. `alpha` (optional)
multiplies the color's alpha — pass `env` to fade with the trigger.

| | |
|---|---|
| `draw.clear(color?)` | start over (transparent by default); drops everything drawn before it this frame |
| `draw.rect(x, y, w, h, color, alpha?, { radius=, stroke= }?)` | |
| `draw.circle(x, y, r, color, alpha?, { stroke= }?)` | |
| `draw.line(x1, y1, x2, y2, width, color, alpha?)` | |
| `draw.path(points, color, alpha?, { close=, stroke= }?)` | `points` = `{x1, y1, x2, y2, …}` or commands `{{"M",x,y}, {"L",x,y}, {"Q",cx,cy,x,y}, {"C",c1x,c1y,c2x,c2y,x,y}, {"Z"}}` |
| `draw.text(text, x, y, size, color, alpha?, { align = "left"|"center"|"right" }?)` | baseline at `y` |
| `draw.image(path, x, y, w, h, alpha?)` | `path` relative to the project `assets/` |
| `draw.push({ x=, y=, rotate=, scale= n or {sx, sy}, alpha= }?)` / `draw.pop()` | transform + opacity scope; unbalanced pushes are closed at the end of the frame |

At most 100 000 draw calls per frame.

### Sandbox and budgets

- Available: `string` (no `dump`; `rep` capped at 16 MB), `table`, `math` (`math.random` seeded per load), `bit`, `coroutine`, `pairs`/`ipairs`/`pcall`/… and `collectgarbage("count"|"collect"|"step")`.
- Not available: `os`, `io`, `debug`, `package`, `ffi`, `jit`, `load`/`loadstring`/`dofile`/`loadfile`, bytecode.
- **Per call:** more than `budget.instructions` VM instructions (default 20 000 000) or longer than `max(10 × cpu_ms, 25 ms)` → the call is aborted and the patch **suspended** (`pcall` cannot catch it). The top-level chunk gets 10× the instructions and 500 ms; exceeding that fails the load (the previous version stays live).
- **Per tick** (one render frame: all handlers + `frame`): more than `budget.cpu_ms` (default 2 ms) on 30 consecutive ticks → suspended.
- **Heap:** above `budget.memory_mb` (default 128) → suspended.
- Per tick: at most 256 commands (`set`/`emit`/…), 64 `signal` updates, 20 log lines.
- A suspended patch's output is cleared. Save the file (reload) or `patch.enable <id>` to run it again. The `patches` query shows per-patch CPU (average/peak), memory, handlers, events, dropped events, and draw ops.

### Errors

Load errors (syntax, top-level runtime errors, over budget) keep the previous version running;
`patch.<id>.state = "error"` and `patch.<id>.error = "main.lua:12: …"`. Runtime errors in
callbacks are reported the same way (the patch keeps running; the state stays `error` until the
next successful reload). Errors inside API calls point at the calling line
(`main.lua:4: bad argument #4: bad color …`).

## 6. Shaders (`kind = "shader"`)

Write a fragment entry `fs`; the engine prepends a generated header and draws a full-screen
triangle with the header's `se_vs`. Output is **premultiplied alpha**.

```wgsl
@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let p = in.uv - vec2<f32>(0.5);
    let pulse = 0.2 + 0.1 * s_band_kick() + 0.2 * se.env;
    let d = smoothstep(pulse, pulse - 0.01, length(p));
    let c = palette(PAL_ACCENT) * p_tint();
    return vec4<f32>(c.rgb * d, d);
}
```

The header (see `crates/se-patch/src/wgsl.rs`):

| Binding / item | |
|---|---|
| `se: SeInputs` (`@group(0) @binding(0)`, uniform) | `time`, `dt`, `frame: u32`, `env`, `resolution: vec2<f32>`, `progress` (transitions, enter/exit styles), `trigger_count: u32`, `beat_phase`, `bpm`, `region: vec4<f32>` (uv `x0, y0, x1, y1` of the node an effect or enter/exit style runs for; `0, 0, 1, 1` otherwise), `trigger: SeTrigger`, `palette: array<vec4<f32>, 8>`, `params`, `signals` |
| `se.trigger` (`SeTrigger`) | payload of the last trigger: `amount`, `bits`, `tier`, `months`, `viewers`, `count`, `user_hash` (0–1), `user_color: vec4<f32>` — see §4 |
| `se_sampler` (`binding(1)`) | linear, clamp |
| `se_input` (`binding(2)`) | effect input / transition A (outgoing) |
| `se_input_b` (`binding(3)`) | transition B (incoming) |
| `p_<param>()` | one accessor per param: `f32` (float), `i32` (int, enum index), `bool`, `vec2<f32>`, `vec4<f32>` (color, vec4); string/texture params have none |
| `s_<signal>()` | every standard signal plus the manifest's `signals`, dots → `_` (`s_band_kick()`, `s_twitch_chat_rate()`) |
| `palette(i)` with `PAL_ACCENT`, `PAL_BACKGROUND`, `PAL_FOREGROUND`, `PAL_RED`, `PAL_YELLOW`, `PAL_GREEN`, `PAL_CYAN`, `PAL_MAGENTA` | stream palette |
| `SeVsOut { pos, uv }`, `@vertex fn se_vs` | full-screen triangle; `uv` (0,0) top-left |

Compile errors are mapped back to your file (`main.wgsl:12:5: …`); a failed compile keeps the
old pipeline.

## 7. Particles (`kind = "particles"`)

`sim.wgsl` and `draw.wgsl` get the same header plus the particles prelude
(`se_patch::wgsl::particles_prelude`):

```wgsl
struct Particle { pos: vec2<f32>, vel: vec2<f32>, color: vec4<f32>, life: f32, size: f32, seed: f32, age: f32 };
@group(0) @binding(4) var<storage, read_write> se_particles: array<Particle>;   // `read` in draw.wgsl
const SE_PARTICLE_COUNT: u32 = <particles.count>u;
fn se_hash(n: u32) -> f32   // 0..1
```

- `sim.wgsl`: `@compute @workgroup_size(64) fn sim(@builtin(global_invocation_id) id: vec3<u32>)`, dispatched `ceil(count / 64)` times per frame. The buffer starts zeroed (`life = 0` = dead); spawn from `se.env` / `se.trigger_count`, and shape the burst with `se.trigger` (the template spawns more for bigger events and colours some particles with `se.trigger.user_color`).
- `draw.wgsl`: `@vertex fn vs(@builtin(vertex_index) v: u32, @builtin(instance_index) i: u32)` (6 vertices per particle, instance = particle index) and `@fragment fn fs(...)`; your own vertex-output struct is fine. Positions are 0–1 of the layer (y down): clip = `(x * 2 - 1, 1 - y * 2)`. Output premultiplied alpha.

## 8. Web patches (`kind = "web"`)

The page is rendered by Chromium (CEF) off screen at the size of its largest scene node (or
the canvas for overlays, or manifest `size`), at `fps`, into the video slot `patch.<id>`; page
audio goes to the audio slot `patch.<id>`. It is loaded from
`http://localhost:<http port>/patches/<id>/index.html?token=<token>`; the token is scoped to
the patch (§19): it can read everything, but only write `patch.<id>.*` (set, animate, trigger,
release, emit `patch.<id>.*` events, `patch.<id>.*` actions) plus what the manifest `grants`
allow (below). Keep the page background transparent. A renderer crash reloads the page; the
rest of the engine is unaffected. Saving any file in the folder reloads the page. See
`docs/web.md` for the CEF runtime.

```html
<script src="/engine.js"></script>
<script>
  const se = Engine.connect();                 // token from ?token=…, reconnects on its own
  se.on(`patch.${se.patch.id}.trigger`, e => show(e.payload));
  se.state("show.*", (addr, v) => …);          // state changes (globs)
  se.signals("band.*", vals => …, 30);         // {name: value} at up to 30 Hz
  se.patch.params                              // live params of this patch
  se.patch.env                                 // live trigger envelope
  se.set(`patch.${se.patch.id}.count`, 40);    // within the token scope
  se.emit(`patch.${se.patch.id}.clicked`, {}); // events in the patch namespace
  await se.get("song.*");  await se.query("patches");
</script>
```

### Grants: letting a page control more

```toml
kind   = "web"
grants = ["lights.*", "source.cam1", "scene.**", "preset.fire"]
```

Each grant is an address pattern with the same `*` (one segment, or a glob inside a segment)
and `**` (any depth) wildcards as `set`/`animate`/`release`, and it covers every address it
matches **and everything beneath it**: `lights.*` covers `lights.cue` and
`lights.fixture.par1.dimmer`; `source.cam1` covers `source.cam1.exposure`. What a grant is
compared against:

| Op | Checked name |
|---|---|
| `set`, `animate`, `trigger`, `release` | the address |
| `emit` | the event type |
| actions (`lights.cue`, `obs.stream.start`, …) | the action name |
| `scene.go` / `scene.cut` / `scene.take`, `preset.fire` / `preset.release`, `mode.set` | that command word (so `scene.*` or `scene.go` allows switching scenes) |

A command whose address is itself a pattern is allowed only when its wildcards stay inside a
grant (`lights.*` allows `set lights.* 0` but not `set *.dimmer 0`).

Rules:

- Grants apply to `kind = "web"` only; on other kinds they are a manifest error.
- A grant must start with a name: `*`, `**`, `*.fader` or `li*.cue` are manifest errors, so a
  page can never be given everything. Empty or malformed patterns (`lights..cue`, spaces) are
  errors too, reported as `patch.toml:0: grants: …`.
- Never grantable, whatever the pattern: `api.*` (tokens, device pairing), `secrets.*`,
  `project.*` (file writes, reload), `patch.new`, `patch.open`, any secret-carrying action
  (`*.key.set`, `*.secret.set`, `*.token.set`), `set_base` (project edits), and `panic`,
  `clean`, `undo`, `redo`.
- Changing `grants` and saving takes effect on the page's existing token immediately (the log
  says `patch.<id>: page permissions now [...]`); removing them drops the page back to its own
  namespace.
- *Scenes → Overlays* shows each patch's grants in plain words ("Can also control:
  lights, Cam1") on its card.

### Bringing your own overlay pages

A page made for an OBS browser source (its own Twitch IRC client, a `?channel=` URL, one
full-screen HTML file) ports to the engine as a few web patches placed in a scene. The worked
example is the owner's win31 DOS "the stream is about to start" page, now the `starting_soon`
scene (mode `preshow`) built from `patches/terminal_title`, `terminal_chat` and `terminal_boot`
plus the shared `patches/_lib/win31/`.

1. **Split the page into pieces** that can move on their own: one patch folder per box on
   screen (`layer = "source"`), each with a transparent page background. A plain background
   is not a patch: use the scene's `[canvas.<name>] background = "#000000"` (or a colour
   node). Size a piece's page from its own viewport so it scales with the node: the terminal
   pieces draw an 80-column text screen (960 px at 1x) and `Dos.fit()` scales it to the page
   width, so a 1920-wide node draws exactly like the original page at 1920×1080. The page is
   rendered at its largest node's size (docs/web.md), so give its other nodes the same shape.
2. **Share code, styles and fonts** from `patches/_lib/<name>/`. Everything under `patches/` is
   served at `/patches/…`; a folder without `patch.toml` is not a patch, and ids starting with
   `_` can't be created, so `_lib` never collides. Pages load
   `/patches/_lib/win31/dos.css` + `dos.js`; the CSS loads the fonts with URLs relative to
   itself (`fonts/…`). Keep each font's licence next to it (`_lib/win31/fonts/FONTS.txt`:
   PxPlus IBM VGA 8x16, VileR's Ultimate Oldschool PC Font Pack, CC BY-SA 4.0 — ship the
   licence text and credit; Fixedsys Excelsior 3.02, public domain). Saving a file in `_lib`
   does not reload the pages; `streamctl do web.reload` does.
3. **Swap the Twitch client for the engine.** Load `/engine.js` and `/web/overlay.js`, then:

   | Browser-source page | Engine page |
   |---|---|
   | IRC `PRIVMSG` | `se.on("chat.message", …)`: `{id, user, login, user_id, color, text, fragments, …}`, already filtered |
   | backlog on load | `await se.query("chat.recent")` in `se.onconnect` (last 100, oldest first, deleted ones gone) |
   | `CLEARMSG` / `CLEARCHAT user` / `CLEARCHAT` | `chat.delete {message_id}` / `chat.purge {user_id, user}` / `chat.clear` |
   | `USERNOTICE` sub/resub/gift/raid, cheers | `se.on("alert.show", …)`: `{id, event, user, amount, currency, tier, title, variation}` in queue order, after the veto window; drop a line again on `alert.hide` with reason `vetoed` or `purged`. `amount` is months for subs, count for gifts, viewers for raids, bits for cheers, money for tips |
   | `?message=…&startingMinutes=5` | manifest `params` (below) |
   | `localStorage` | patch state: `se.set("patch.<id>.<key>", v)` + `se.state(…)`; the page may write any `patch.<id>.*` address, declared or not, and it survives a page reload |
   | page load = "start" | a trigger: the scene's `on_enter = ["patch.<id>.trigger"]` |

   Insert viewer text with `textContent` only. Alert sounds already play from the alert queue;
   don't add page sounds for them.
4. **Settings as params.** Each thing the owner should change becomes a manifest param whose
   `description` is the plain label shown in *Scenes → Overlays* (keep it under ~20
   characters): `title` ("Title"), `count_to` ("Countdown", `minutes` | `clock_time`),
   `minutes`, `clock_time` on terminal_title; `prompt`, `lines` ("Chat lines to keep"),
   `show_alerts` on terminal_chat; `brand` ("Name on boot screen") on terminal_boot. Read them
   with `Overlay.params(se, defaults, onChange)` so edits apply live (and `?param=` works in a
   normal browser). Machine state (terminal_title's `started`) stays out of the manifest so it
   isn't shown as a setting.
5. **Make it survive reloads.** The countdown keeps the moment the scene came on in
   `patch.terminal_title.started` (written through `Dos.keeper`, which resends after a
   reconnect) and derives the end from the current settings, so a reload, a crash or a
   settings change continues the same countdown.
6. **Sequence the pieces with the trigger envelope, not page messages.**
   `patch.terminal_boot.active` is on from the scene's first frame, so the scene places the boot
   piece with `when = "patch.terminal_boot.active"` and the title and chat with
   `when = "!patch.terminal_boot.active"` (no flash of the other pieces). When its typing is done
   the page ends the trigger itself, 0.7 s later: `se.cmd("patch.terminal_boot.trigger hold=0
   done=true")`. That works because the manifest's `retrigger = "replace"` drops the running
   instance, and it's within the page's own namespace. A plain `release` from the page would
   only end the page's own instances, not the scene's. The page's trigger handler ignores
   `done`. The manifest's `hold = "16s"` is only the safety net for a page that never loads.
   The chat page reads the same address to hold new lines until the boot is done.
7. **Wire the scene.** `rules/modes.toml` cuts to `starting_soon` on `mode.enter.preshow` (and
   to `duo` when going live from it); `[overlays.countdown]` and `[overlays.goals]` have
   `scene != 'starting_soon'` in their `when` so the generic countdown and goal bars don't
   cover the terminal.

Try it on a dev engine: `streamctl mode preshow`, then `streamctl sim chat user=Clippy
message='how do i double click'`, `streamctl sim sub`, `streamctl sim raid viewers=42`, and
`streamctl fire twitch.chat.delete message_id=<id from streamctl query chat.recent>`. Open a
piece in a normal browser at
`http://127.0.0.1:<http port>/patches/terminal_chat/index.html?token=<API token>` (sized like
its node) to work on it.

## 9. DSP patches (`kind = "dsp"`)

WebAssembly block ABI (host: `se-dsp`): no imports; export `memory`, `se_dsp_abi() -> 2` (1 still
loads), `init(sample_rate: f32, channels: i32, max_frames: i32) -> i32` (0 = ok; allocate only
here), `input_buffer()`, `output_buffer()`, `params_buffer()` (byte offsets), `process(in, out,
frames, params)`, optional `reset()` and `latency() -> i32`. Buffers are planar f32 (channel
`c` at `ptr + c * max_frames * 4`), `max_frames = 1024`, 2 channels. The params block is
`[env, bpm, beat_phase, bar_phase, trigger_count, 0, 0, 0, <params…>]` with the manifest
params in alphabetical order, each `slots()` floats (bool 0/1, enum = option index), 64 slots.
ABI 2 appends the last trigger's payload (88 floats in all): `[72]` amount, `[73]` bits, `[74]`
tier, `[75]` months, `[76]` viewers, `[77]` count, `[78]` user_hash, `[79]` 0, `[80..84]`
user_color r g b a, `[84..88]` 0. It changes in the same block as the trigger edge (never
after it); an ABI 1 module keeps its 72-float block. Over `budget.cpu_ms` per block three times
in a row, or a trap → auto-bypass with a crossfade and `patch.<id>.error`. The template
(`dsp/default`, ABI 2: a sub's tier pushes its drive) is a Rust `no_std` crate with `build.sh`
(`rustup target add wasm32-unknown-unknown`); the engine hot-swaps `main.wasm` on save.

## 10. Troubleshooting

- `streamctl query patches` — every patch with state, error (`error_file`, `error_line`), whether a previous version is still live, params with current values, and script stats.
- `streamctl get 'patch.<id>.**'` — its addresses; `streamctl trace <id>` — what a trigger caused.
- `health.patches` in `streamctl preflight` lists patches in `error`/`suspended`.
- A disabled patch stays disabled across restarts (`patch.enable <id>`).
