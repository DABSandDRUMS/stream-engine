# Compositor (se-render)

The engine renders the `wide` (1920×1080) and `tall` (1080×1920) canvases at 60 fps on a
dedicated render thread on the NVIDIA GPU (wgpu/Vulkan), plus the `preview` canvas (the scene on
preview, only while a client asks for it) and the multiview `atlas` (~30 fps). Canvases leave
the GPU as dmabufs with explicit sync over `frames.sock` (see `docs/frames-protocol.md`); the OBS
plugin and the UI import them without copies. A shared-memory copy is the debugging fallback.

Per frame and canvas: sources → scene nodes (+ node effects) → transition (morph and/or shader)
→ scene effects → overlays that opt into effects → canvas effects → overlays → output effects →
flash limiter → export.

## Settings (`project.toml`)

```toml
[canvas.wide]            # sizes and frame rate (the render clock follows wide.fps)
width = 1920
height = 1080
fps = 60

[render]
adapter = "NVIDIA"       # GPU name substring (default: the NVIDIA GPU; env SE_GPU overrides)
frames_socket = "/run/user/1000/stream-engine/frames.sock"  # default: $SE_FRAMES_SOCKET,
                         # else $SE_RUNTIME_DIR/frames.sock, else $XDG_RUNTIME_DIR/stream-engine/
export_modifier = "auto" # auto (non-compressed NVIDIA block-linear) | linear | "0x…"
buffers = 4              # dmabufs per canvas (3–4)
preview = { scale = 0.5, layout = "wide" }
atlas = { width = 1920, height = 1080, fps = 30 }
no_signal = "#101014"    # color of sources without frames ("transparent" hides them)
font = "JetBrainsMono Nerd Font"   # script patch text
canvas_fx.wide = [{ name = "grade", warmth = 0.1 }]      # always-on canvas effects
output_fx.tall = [{ name = "vignette", amount = 0.2 }]   # after overlays

[safety]
video_flash_limit = true # §22 flash limiter on the output
video_max_flashes = 3    # flashes per second allowed

[palette]                # stream palette (palette.<slot> state, patches, lights)
mode = "fixed"           # or "follow_theme" (tracks the Omarchy theme)
accent = "#7e9cd8"       # accent background foreground red yellow green cyan magenta

[overlays.alertbox]      # optional placement of an overlay-layer patch
canvases = ["wide", "tall"]
rect.tall = [0.0, 0.1, 1.0, 0.4]
when = "mode != 'brb'"
z = 10
fx = false               # true: drawn before canvas effects (effects apply to it)
```

## Sources

A node's `src` names a source:

| `src` | Source |
|---|---|
| `cam_kit`, `youtube`, … | `hub.video` slot of that name (cameras: se-video-in; web pages: se-web) |
| `patch.<id>` | a patch: shader/particles rendered here, script draw lists (vello), web pages (video slot) |
| `color:#rrggbb` | solid color |

One decode feeds every placement. Sources nobody shows are not uploaded or rendered; the set in
use is published as `render.sources.used` (se-video-in captures only those, plus cameras a
recording taps; see devices-and-sources.md).

Per-source color correction (read live): `source.<n>.color.{brightness,contrast,saturation,gamma,
temperature,tint}`, `source.<n>.lut` (project-relative `.cube`), `source.<n>.lut_amount`, and the
YUV hints `source.<n>.matrix` (`bt601`/`bt709`) and `source.<n>.range` (`full`/`limited`).
Source-level effects (all placements) go in `sources/<name>.toml`: `fx = [{ name = "chroma_key" }]`.

## Scenes and nodes

Every node property is live state: `scene.<s>.node.<id>.rect.<canvas>` (x, y, w, h normalized),
`crop.<canvas>` (left, top, right, bottom insets), `radius.<canvas>` (px, rounded corners with
anti-aliased SDF edges), `z.<canvas>`, `opacity`, `offset_x`/`offset_y` (px), `scale` (about the
center), `rotation` (degrees clockwise), `visible`. Nodes also take `blend` (`normal`, `add`,
`screen`, `multiply`), `mask` (image in the project; white = visible), `when` (expression;
hidden when false — a broken expression hides the node), `fx`, and `enter`/`exit` styles for
morphs. Transparent or off-canvas nodes are culled.

Node `fit` is a saved placement setting: `"stretch"` (default) fills the rectangle as before;
`"native"` makes the rectangle a clipping window. Native source pixels retain their size
while live `rect` or `scale` edits change the window: smaller windows clip the top/right,
keeping the bottom-left content; larger windows leave transparent space above/right.
Explicit source crop still applies. Preview pixels follow the preview canvas's overall
resolution scale, and node/group effects retain the same window mapping.

Use native fit for chat whose text must not shrink when its layer is resized:

```toml
[canvas.wide]
nodes = [
  { src = "patch.win31_chat", id = "chat", fit = "native", rect = [0.47, 0.13, 0.3, 0.75] },
]
```

Change the chat's **Text size** source setting to change glyph size; resize the node to
change how much chat is visible. The editor's atlas-based layout previews use the same
native placement and wait for actual source dimensions instead of stretching the tile.

## Effects

Each effect has params `fx.<name>.<param>`, a trigger `fx.<name>` (envelope `fx.<name>.env`), and
two standard params: `amount` (latched strength: presets `set`, bindings, manual) and `level`
(strength while triggered). The implicit global instance runs at
`max(amount, level × env)`; a trigger payload `amount`/`level` overrides `level` for that trigger.
Built-in attached instances run at their own `amount` (default 1), or at their own `level × env`
with `triggered = true`. `enabled = false` is a hard bypass, even during an active trigger;
`fx_enabled = false` bypasses a host's entire chain without deleting its settings. Canvas and
output gates also bypass their implicit global effects. `when` makes a slot conditional;
the slot's `group` is an exclusivity group (strongest wins), not a composited layer group.
Strength 0 skips the pass.

Effect-layer shader patches with a manifest `trigger` also get an implicit global canvas
instance (strength `env`) — unless any non-bypassed attachment of that patch sets
`triggered = true` in config. Such a patch is *scoped*: it runs only in its attachments
(triggered ones at `env`), never canvas-wide. The rule reads the static config value; flipping
`<slot>.triggered` at runtime does not add or remove the global instance. A patch referenced
only by bypassed (`enabled = false`) or non-triggered slots keeps the global instance.

| Effect | Params | Where the global instance runs |
|---|---|---|
| `zoom_pulse` | zoom, beat (1 = pulse on `beat.phase`), center_x, center_y | canvas |
| `pixelate` | size (px) | canvas |
| `blur` | radius (px; runs at quarter resolution) | canvas |
| `glitch` | blocks, shift, color, speed | canvas |
| `rgb_split` | angle (deg), spread | canvas |
| `vhs` | noise, jitter, scanlines, bleed | canvas |
| `grade` | warmth, tint, contrast, saturation, lift, exposure (on at identity = no pass) | canvas |
| `lut` | `fx.lut.file` (.cube) | canvas |
| `chroma_key` | key_r/g/b, similarity, smoothness, spill | attached chains only; no implicit global pass |
| `shake` | strength (fraction of the shorter side; zooms in just enough to hide the edges), speed (shakes/s) | canvas |
| `vignette` | radius, softness | canvas |
| `fade_to_black` | color_r/g/b | output (covers overlays) |

Effect-layer shader patches work the same way (`fx = [{ name = "patch.<id>" }]`); with a trigger
they also run at canvas level while triggered.

**Fused passes.** `grade`, `chroma_key`, `vignette`, and `fade_to_black` only look at the pixel
they write, so neighbours of that kind in one chain run as a single pass. At load and on every
reload, each chain (source, node, composited group, scene, scene layout, `canvas_fx` + canvas
globals, `output_fx` + output globals) gets one pipeline for its pointwise effects in chain order,
built on the loader thread. Per frame, consecutive active pointwise effects of the chain run
through it (inactive ones are skipped inside the pass); anything else in between — `blur`,
`lut`, effects that sample neighbours, shader patches — keeps its own pass and splits the run.
The result equals one pass per effect (up to 8-bit rounding between passes, which the fused
pass no longer has). `perf.fx_passes` counts effect passes in the last frame and
`perf.fx_fused` the effects that ran inside fused passes (also in query `render`).

### Slots, targets, and saved chains

`name` identifies the effect kind; `id` identifies one instance within its chain. Repeated kinds
are allowed and execute in list order:

```toml
fx_enabled = true
fx = [
  { id = "soft", name = "blur", radius = 4.0 },
  { id = "heavy", name = "blur", radius = 32.0, enabled = false },
]
```

Id-less entries receive unique effect-name-based IDs (`blur`, `blur_2`, …). Editing a chain
persists its IDs, so reordering does not change its control addresses; duplicate explicit IDs
are rejected. Legacy custom-effect IDs such as `patch.win31_video` retain their address.

Every host has `<host>.fx_enabled`, and every slot has `<host>.fx.<id>.enabled`,
`.triggered`, and its effect parameters (including live LUT `file`).

| Target | Host prefix | Authored location |
|---|---|---|
| Source, every placement | `source.<name>` | `sources/<name>.toml` |
| One scene layer | `scene.<s>.node.<id>` | matching nodes on every scene canvas |
| Whole scene | `scene.<s>` | scene top-level `fx` |
| One scene layout | `scene.<s>.canvas.<c>` | `[canvas.<c>]` in the scene |
| Composited group | `scene.<s>.canvas.<c>.group.<g>` | group in that layout |
| Master canvas, across scenes | `render.canvas.<c>` | `[render] canvas_fx.<c>` / `canvas_fx_enabled.<c>` |
| Final output, including overlays | `render.output.<c>` | `[render] output_fx.<c>` / `output_fx_enabled.<c>` |

Explicit slot settings are independent. Omitted settings inherit the current effect-library
defaults; removing an explicit setting restores that inheritance. Custom shader slots accept
their manifest's scalar/vector/color/bool/enum parameters; custom `amount` keeps its shader-defined
meaning, and trigger strength is delivered through `se.env`. Custom shaders remain responsible
for valid premultiplied-alpha output.

Reusable chains live in `fx_chains/<name>.toml`, with `label` and `fx` as above. Applying one
copies its slots independently to each target; it does not link their subsequent edits.
These differ from `presets/` show actions, which can coordinate lighting, sound, triggers, and
scene changes. Starter chains are **Warm stage**, **Monochrome**, **VHS tape**, and
**Digital breakup**, available but unapplied. See [FX authoring API](api.md#video-fx-authoring).

### Saved freeze-frame photos

Every accepted `patch.freeze_frame.trigger` automatically saves a PNG in
`~/Pictures/Stream Engine/Freeze Frames`. The directory is created by the save worker; there
is no extra enable switch. Filenames include the trigger's Unix timestamp (seconds and
nanoseconds), process ID, sequence, and render context, and never overwrite an existing photo.

The photo comes from the freeze shader's clean feedback-state texture immediately after its
rendered pass, not a delayed screenshot, preview, or decorated program output. RGB is saved
without the state alpha's bookkeeping bytes. For camera/node/group attachments this is the
isolated frozen camera layer; chat, other program layers, the PAUSED window, grain, shutter,
and animated push-in are not added. A canvas-wide attachment instead saves the canvas image
that it actually freezes. One photo is saved per accepted trigger, using the first rendered
freeze attachment/context (normally wide); multiple triggers reaching the same rendered pass
save separate files of that same frozen state. Retriggering during an existing hold saves that
same pristine still again, matching the shader's hold-extension behavior. A later trigger
after the effect finishes captures and saves a new still.

Observe `render.freeze_frames.directory`, `.last_path`, `.saved`, `.pending`, and `.failed`,
plus `health.render.freeze_frames`. To exercise it when not live, fire the normal Freeze Frame
deck key, preset, or `patch.freeze_frame.trigger`; wait for `.saved` to increase and open
`.last_path`. Trigger again during the hold to verify another file with identical frozen RGB,
then trigger after release to verify a new moment. A bypassed/off-screen/uncompiled freeze
target leaves requests pending; the health detail names the target/compilation checks.

GPU copies occur at the freeze-state edge; asynchronous mapping, PNG encoding, directory
creation, and disk writes run on `se-freeze-photos`, not the render thread. A pool of eight
readbacks is capped at 256 MiB, and up to 64 triggers can share a captured state. File/directory
errors retain captured photos and retry forever with 1–30 second backoff; fix `HOME`,
permissions, or free space and saving resumes automatically. A full trigger queue or staging
pool cannot retain another exact frame without exceeding those bounds: `.failed` increases
and health explicitly reports the missing photo and recovery steps, rather than silently
substituting a later frame. Shutdown drains captured work; readbacks have a five-second
shutdown deadline, and remaining unsaved photos are reported instead of hanging teardown.


### Shared palette colors

`[palette]` and the writable `palette.<slot>` state form one shared color vocabulary for
lighting, shader palette uniforms, and authored video-FX slots. Slots are `accent`,
`background`, `foreground`, `red`, `yellow`, `green`, `cyan`, and `magenta`; there is no
separate lighting/video palette bank.

A shader slot parameter declared `type = "color"` may use `tint = "stream:accent"` or
`tint = "@palette.accent"` instead of a fixed color. In **Scenes → Effects → Target racks /
saved chains**, its color-source chooser offers **Fixed color** and the eight palette slots.
Choosing a slot saves that reference through the existing FX authoring action. The color
picker is disabled while linked; choosing Fixed color captures the currently resolved color.
The same slot editor is used by the scene inspector.

Numeric RGB parameters may explicitly select components, for example an attached
`fade_to_black` slot with `color_r = "stream:accent.r"`, `color_g = "stream:accent.g"`,
and `color_b = "stream:accent.b"`. Components are `.r`, `.g`, `.b`, and `.a`, and bind only
to Float parameters. A whole-color reference binds only to Color parameters. Scalar controls
such as `grade.tint` and `glitch.color` are not RGB colors and are not automatically linked.
Text, effect enable gates, and triggers are never driven by palette-reference resolution.

References resolve as a derived base value before scene, binding, and manual override
ownership and final clamps. Thus a manual slot-color override wins; releasing it reveals
the **current** palette color, not a captured color from before the override. Standard
`set`/`animate` control of `palette.*` recolors linked DMX and video together; color endpoints
interpolate continuously. Runtime recoloring does not edit scene files, trigger effects,
or enable bypassed slots. Hot-added/replanned slots receive typed metadata so references
work without an engine restart.

Palette references are resolved centrally in state, with cached source IDs and sparse
linked targets, not parsed on each GPU frame. Changing a slot back to a literal or deleting
its host removes the link. `follow_theme` supplies palette defaults on load/replan; it is
not a claim that theme-file edits are watched live.


### Composited groups

A layout can group selected nodes into one atomic layer:

```toml
[canvas.wide]
nodes = [
  { id = "kit", src = "cam_kit", z = 0 },
  { id = "room", src = "cam_room", z = 1 },
  { id = "title", src = "patch.title", z = 5 },
]
groups = [
  { id = "cameras", nodes = ["kit", "room"], z = 2,
    fx = [{ id = "look", name = "grade", saturation = 0.0 }] },
]
```

Members render in their own z order onto transparency, then the group chain runs once on that
combined image. The group composites at its own z, opacity, and blend; unrelated layers retain
their z. Each node may belong to only one group per layout. Unknown/duplicate members and
duplicate group IDs reject the scene; groups are not nested.

Group FX bypass preserves grouping and stacking; deleting the group restores individual layer
placement. Groups use reusable full-canvas GPU surfaces, so memory grows with concurrently
visible groups and transition sides. Wide, tall, and preview use their respective layouts.
During morphs, matched membership/FX switch from the outgoing group to the incoming group at
halfway, like existing stacking selection; group settings are not interpolated.


## Transitions

Driven by the core's `show.transition.*` state, interpolated by master-clock time every frame.
`ease` shapes geometry (and shader progress for shader/combined, clamped to 0–1 in
`se.progress`); `label` is the name editors show. Omitted ease is `in_out_cubic` except for
glide, which uses `standard`.

- `kind = "morph"`: nodes showing the same source animate rect/crop/radius/opacity/rotation with
  `ease`; others enter/exit with `enter`/`exit` (`fade`, `scale`, `slide`, `slide_left|right|up|down`,
  `none`, or a custom shader style; per node overrides).
- `kind = "glide"`: matches by source and interpolates geometry, while rendering both scenes
  independently with their own node effects, masks, groups, z order, and scene effects.
  Their images crossfade continuously: no first-frame effect switch or halfway stacking switch.
  The incoming side starts transparent; the outgoing picture fills uncovered areas until the
  last quarter, when it smoothly becomes the incoming background. This cleanup uses linear
  time, independently of the content fade, and finishes exactly at the incoming scene.
  If either side has scene-level effects, glide instead blends complete scene images (including
  their backgrounds processed through those effects). This preserves effect/background
  correctness at both endpoints; uncovered areas then crossfade with the rest of the image,
  rather than retaining the outgoing background until the final quarter.
- `kind = "shader"`: both scenes render to textures and `shader` blends them. `shader` is a
  built-in shader name (below), a `.wgsl` file in the project, or `"patch.<id>"` (a
  transition-layer patch). Files get the generated patch header: `se.progress` (0→1), `se_input`
  (outgoing), `se_input_b` (incoming), `se_sampler`, extra TOML keys as params `p_<name>()`;
  entry point `fs`.
- `kind = "combined"`: morph geometry with the shader applied over it (A = B = the morph frame).

Built-in shaders (`se_core::transitions::SHADERS`, sources in `se-render/src/shaders/`): `fade`
(crossfade), `glitch` (after gl-transitions GlitchMemories, MIT; settings `strength` 0–3,
default 1, and `block` in pixels 4–64, default 16), `zoomblur` (gl-transitions CrossZoom, MIT;
`strength` 0.05–1, default 0.4). They compile with the transition file's settings; a missing
setting takes its default, and whole numbers count as decimals. Ported shaders keep their
license header. The example project ships `morph`, `fade`, `zoomblur`, `glitch` and
`morph_glitch` (combined) files using them; `se_core::transitions::STYLES` lists the starting
points editors offer. A shader that fails to compile keeps its last good version (else a
crossfade) and the error is published at `render.transition.<name>.error`.

### Natural glide choreography

Glide takes linear clock progress `u`. Matched geometry uses `ease = "standard"` by default;
`decelerate`, `accelerate`, and `emphasized` are also available (as are existing ease names).
Unmatched exits use accelerating disappearance over `exit_window = [0.0, 0.5]`; entries use
decelerating arrival over `enter_window = [0.3, 1.0]`. The overlap avoids a mechanical
all-out-then-all-in change. The independent content crossfade uses smoothstep over
`fade_window = [0.15, 0.75]`. Each optional window must contain finite values with
`0 <= start < end <= 1`; invalid intervals produce a configuration error and use the default.
Morph, shader, and combined timing is unchanged.

`enter`/`exit` default to `fade`. In glide, outgoing `fade` and `scale` retain image coverage:
the composite crossfade removes them instead of exposing black when the exit phase ends.
Slides and custom exits finish in the exit window. Unmatched entering items are transparent
before their phase starts; custom styles receive the role-local presence, not matched ease.

Custom entering styles additionally multiply opacity by role presence, ensuring a shader that
draws at presence zero cannot flash as the phase starts; opacity is exactly restored at the end.

`slide` chooses the nearest canvas edge and travels just far enough to clear it, with a
rotation-safe bound and a two-pixel margin. An incoming item starts beyond that same nearest
edge. Explicit `slide_left`, `slide_right`, `slide_up`, and `slide_down` keep their previous
full-canvas travel and directional semantics.

```toml
kind = "glide"
ms = 900
ease = "standard"
enter = "slide"
exit = "slide"
exit_window = [0.0, 0.55]
enter_window = [0.2, 1.0]
fade_window = [0.1, 0.85]
```

Use per-node `exit = "fade"` on a full-screen backdrop when other nodes slide away: this
retains coverage while the incoming layout grows into place.


### Custom enter/exit styles

Instead of a built-in name, `enter`/`exit` (on a morph/glide transition or on a node) can be a shader:
`"patch.<id>"` (a `kind = "shader"` patch; `layer = "effect"` is a good fit) or a `.wgsl` file in
the project (`"transitions/dissolve.wgsl"`; keep it under `transitions/` so saving it reloads it).
The node is drawn once on its own, in place and unrotated, then through the shader:

- `se_input`: the node alone on a transparent layer the size of the canvas;
- `se.region`: the node's box on that layer (uv `x0, y0, x1, y1`);
- `se.progress`: presence, 0 = gone … 1 = fully in place (enter runs 0 → 1, exit 1 → 0, so one
  shader serves both);
- the usual header (`se.time`, palette, signals; a patch also gets its params).

Return premultiplied color; only the part inside the node's box is shown, with its rounded
corners, mask, rotation, and opacity applied afterwards. Until the shader compiles (or if it
fails), the node fades instead; file errors are published at `render.style.<path>.error` and
logged. Example wipe:

```wgsl
@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let c = textureSample(se_input, se_sampler, in.uv);
    let x = (in.uv.x - se.region.x) / max(se.region.z - se.region.x, 1e-6);
    return c * step(x, se.progress);
}
```

### Choosing the transition

A take that doesn't name its transition (`scene.cut wide`, deck keys, `scene.take`) picks one in
this order; the `scene.take` event says which (`by`: `command | fixed | vote | pool | project |
default`):

1. the transition named by the command (`scene.cut wide zoomblur`, `scene.take fade 400ms`);
2. a fixed `name` for the scene pair, else the target scene's `name`;
3. the chat vote winner (below);
4. a weighted pick from the scene pair's pool, else the target scene's pool (skipping the last
   `avoid_repeat` transitions);
5. the project-wide default, `[transitions]` in `project.toml` (same keys, pairs included): its
   pair's or its own fixed `name`, else a pick from its pool (`by = "project"`);
6. `fade`.

A cut stays instant unless the command gives a duration. Otherwise the duration is the pair's,
else the scene's, else the project default's `ms`, else the transition's own.

```toml
# scenes/wide.toml
[transitions]
pool = [{ name = "morph", w = 2 }, { name = "zoomblur", w = 1 }]
avoid_repeat = 1
ms = [500, 800]                  # duration range (or one value); else the transition's own `ms`

[transitions.from.kit]           # coming from `kit`: its own pool, duration, lights
pool = [{ name = "glitch" }]
ms = 400

[transitions.from.brb]           # only the duration differs; the pool above still applies
ms = 1200
```

A pair table overrides the target scene's settings key by key (`pool` with its own
`avoid_repeat`, `ms`, `lights`, `name`); a pair with its own `pool` ignores the scene's fixed
`name`. Unknown scenes or transitions in a pair show as project errors.

```toml
# project.toml: every scene without a pool or name of its own
[transitions]
pool = [{ name = "morph" }, { name = "fade" }, { name = "zoomblur" }]
avoid_repeat = 1

[transitions.from.brb]           # leaving `brb`, whatever the target
name = "cut"
```

The Transitions tab (Scenes → Transitions) edits these: "How scenes switch" is the project
default; "Exceptions" are the scenes' `[transitions]` and `[transitions.from.<scene>]` tables.

**Chat vote.** With `[transition_vote]` in `project.toml`, viewers vote for the next take's
transition with `transition.vote {name}` — the starter project maps `!transition <name>` to it
(`commands/show.toml`, a per-viewer cooldown and role gate like any bot command):

```toml
[transition_vote]
window = "60s"                   # only votes from the last minute count
choices = ["fade", "morph", "zoomblur", "glitch"]   # default: every transition
# enabled = false                # keep the table, stop counting votes
```

Each viewer has one vote (a new one replaces theirs); names match without regard to case. The
next take without a named transition uses the most-voted one (a tie goes to the transition voted
for first) and spends the votes; a transition named by the operator or a scene's (or pair's)
fixed `name` wins over the vote, which then waits for the following take. The vote wins over the
project default, even its fixed `name`. Votes count only in the policy's effect modes
(`live`, `rehearsal`), like every chat effect. Live tally: `show.transition.votes`
(`{transition: votes}`); each vote emits `transition.vote {user, name, votes}`.

## Shader and particles patches

The renderer compiles `shader` and `particles` patches (the patch loader declares their params and
triggers). The generated header is `se_patch::wgsl::Layout::header()` (time, dt, frame, env,
resolution, progress, trigger_count, region, the last trigger's payload `se.trigger`, palette,
params, signals — see docs/patches.md). The payload arrives with the trigger that fired the patch
(`patch.<id>.trigger` event), in the same frame as its `trigger_count` step.

- `shader`: fragment entry `fs(in: SeVsOut) -> @location(0) vec4<f32>`, premultiplied output.
- `particles`: `sim.wgsl` (`@compute @workgroup_size(64) fn sim`) and `draw.wgsl` (`vs` with
  `vertex_index`/`instance_index`, 6 vertices per particle, and `fs`), with the particles prelude
  (`Particle`, `se_particles`, `SE_PARTICLE_COUNT`, `se_hash`).

Pipelines are built on a loader thread when files change, never on trigger. A compile error
publishes `patch.<id>.error` as `file:line:col: message` (line in your file) and the previous
version keeps running; `""` when it compiles again. Overlay-layer patches draw over every canvas
while `patch.<id>.env > 0` (or always without a trigger); `fps = N` in the manifest throttles a
patch.

## Safety: video flash limiter

The output of wide/tall is reduced on the GPU to an 8×8 grid of relative luminance; a flash is a
pair of opposing changes ≥ 10 % with the darker state below 0.8 (WCAG). When more than 10 % of
the screen flashes faster than `video_max_flashes`, flashy effects are scaled down and the output
pass caps the per-frame luminance step. State: `safety.video.limited`, `safety.video.limit`,
`safety.video.flash_rate`.

## Performance and health

`perf.fps`, `perf.frame_ms` (render thread CPU), `perf.frame_ms_max`, `perf.gpu_ms` (GPU
timestamps), `perf.pass.<sources|wide|tall|preview|atlas|output>_ms`, `perf.dropped`, `perf.late`,
`perf.vram_mb` / `perf.vram_budget_mb` (VK_EXT_memory_budget, whole process),
`perf.render_mb` (renderer's own textures), `perf.fx_passes` / `perf.fx_fused` (effect passes and
fused effects in the last frame), `render.clients`, `render.export`,
`render.recoveries`, `health.render`, query `render`. The frame loop does no heap allocation in
steady state (checked in debug builds by the counting allocator; the GPU API's own allocations
are excluded).

Renderer and UI devices use wgpu's `MemoryUsage` allocator policy. On a discrete GPU with a
256 MiB CPU-visible VRAM heap, `Performance` reserves a 64 MiB staging block even for a
1 MiB upload; the smaller policy starts at 4 MiB. Full-resolution camera and browser upload
rings additionally use persistently mapped, host-coherent **system RAM**, not device-local
CPU-visible VRAM. Their three slots are reused only after GPU submission completion; GPU
textures, color conversion, canvas resolution and frame rates are unchanged.
This is not a bound on total GPU allocations: imported buffers, OBS, encoders and desktop
applications still share the GPU. Check `nvidia-smi -q -d MEMORY` for **BAR1** usage:
free framebuffer VRAM does not imply free CPU-visible mapping space.

GPU device loss (driver reset, runaway shader) is recovered automatically: clients get
`se_goodbye{reason: 2}`, the device and every resource are recreated, pipelines are rebuilt from
the last good sources, and new `se_canvas` messages follow. Actions: `render.simulate_device_loss`
(test the path), `render.reload` (rescan patches).

Recovery requires a responsive GPU driver. NVIDIA `can't alloc VA space for mapping`,
`NV_ERR_NO_MEMORY`, Xid 119/120 (GSP timeout) or `GPU probably locked` can leave the entire
desktop GPU wedged; restarting the engine cannot guarantee recovery. Preserve the kernel
journal and follow the driver's reset/reboot guidance before resuming a broadcast.
Capture-card `hws ... AMD-Vi ... IO_PAGE_FAULT` is a separate kernel DMA fault, not an
engine device-loss notification. Stop capture and repair the driver; do not disable IOMMU
protection to conceal invalid DMA.

## Testing

`cargo test -p se-render` runs headless on the GPU: golden images in `crates/se-render/tests/golden`
(scenes, transitions at fixed progress, every effect), YUYV accuracy, LUTs, last-good shaders,
particles, frames.sock export (dmabuf fences + shm content), and the no-allocation check.
Regenerate goldens after verifying the output with `SE_UPDATE_GOLDEN=1`.
`cargo test -p se-render --test memory_budget -- --nocapture` verifies upload/readback content
and the device's small CPU-visible heap reservation on supported NVIDIA Vulkan adapters,
including ten simultaneous full-HD source upload rings without consuming that heap.
`se-frames-client --want wide,tall --import --expect-fps 58` validates a running engine's
dmabufs (Vulkan import + pixel readback).
