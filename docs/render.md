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
use is published as `render.sources.used` (se-video-in captures only those).

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

## Effects

Each effect has params `fx.<name>.<param>`, a trigger `fx.<name>` (envelope `fx.<name>.env`), and
two standard params: `amount` (latched strength: presets `set`, bindings, manual) and `level`
(strength while triggered). The implicit global instance runs at
`max(amount, level × env)`; a trigger payload `amount`/`level` overrides `level` for that trigger.
Attached instances (`fx = [...]` on a source, node, scene, `canvas_fx`, `output_fx`) run at their
own `amount` (default 1) or, with `enabled = false`, only while triggered; `when` makes them
conditional, `group` makes them exclusive (the strongest in a group wins). Strength 0 skips the
pass.

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
| `chroma_key` | key_r/g/b, similarity, smoothness, spill | attach only (source/node) |
| `vignette` | radius, softness | canvas |
| `fade_to_black` | color_r/g/b | output (covers overlays) |

Effect-layer shader patches work the same way (`fx = [{ name = "patch.<id>" }]`); with a trigger
they also run at canvas level while triggered.

**Fused passes.** `grade`, `chroma_key`, `vignette`, and `fade_to_black` only look at the pixel
they write, so neighbours of that kind in one chain run as a single pass. At load and on every
reload, each chain (a source's, a node's, a scene's, `canvas_fx` + the canvas globals,
`output_fx` + the output globals) gets one pipeline for its pointwise effects in chain order,
built on the loader thread. Per frame, consecutive active pointwise effects of the chain run
through it (inactive ones are skipped inside the pass); anything else in between — `blur`,
`lut`, effects that sample neighbours, shader patches — keeps its own pass and splits the run.
The result equals one pass per effect (up to 8-bit rounding between passes, which the fused
pass no longer has). `perf.fx_passes` counts effect passes in the last frame and
`perf.fx_fused` the effects that ran inside fused passes (also in query `render`).

## Transitions

Driven by the core's `show.transition.*` state, interpolated by master-clock time every frame:

- `kind = "morph"`: nodes showing the same source animate rect/crop/radius/opacity/rotation with
  `ease`; others enter/exit with `enter`/`exit` (`fade`, `scale`, `slide_left|right|up|down`,
  `none`, or a custom shader style; per node overrides).
- `kind = "shader"`: both scenes render to textures and `shader` blends them. The file gets the
  generated patch header: `se.progress` (0→1), `se_input` (outgoing), `se_input_b` (incoming),
  `se_sampler`, extra TOML keys as params `p_<name>()`; entry point `fs`. `shader = "patch.<id>"`
  uses a transition-layer patch.
- `kind = "combined"`: morph geometry with the shader applied over it (A = B = the morph frame).

Shipped: `morph`, `fade`, `zoomblur` (gl-transitions CrossZoom, MIT), `glitch` (after
gl-transitions GlitchMemories, MIT), `morph_glitch` (combined). Ported shaders keep their license
header. A shader that fails to compile keeps its last good version (else a crossfade) and the
error is published at `render.transition.<name>.error`.

### Custom enter/exit styles

Instead of a built-in name, `enter`/`exit` (on a morph transition or on a node) can be a shader:
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
this order; the `scene.take` event says which (`by`: `command | fixed | vote | pool | default`):

1. the transition named by the command (`scene.cut wide zoomblur`, `scene.take fade 400ms`);
2. a fixed `name` for the scene pair, else the target scene's `name`;
3. the chat vote winner (below);
4. a weighted pick from the scene pair's pool, else the target scene's pool (skipping the last
   `avoid_repeat` transitions);
5. `fade`.

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
for first) and spends the votes; a transition named by the operator or a fixed `name` wins over
the vote, which then waits for the following take. Votes count only in the policy's effect modes
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

GPU device loss (driver reset, runaway shader) is recovered automatically: clients get
`se_goodbye{reason: 2}`, the device and every resource are recreated, pipelines are rebuilt from
the last good sources, and new `se_canvas` messages follow. Actions: `render.simulate_device_loss`
(test the path), `render.reload` (rescan patches).

## Testing

`cargo test -p se-render` runs headless on the GPU: golden images in `crates/se-render/tests/golden`
(scenes, transitions at fixed progress, every effect), YUYV accuracy, LUTs, last-good shaders,
particles, frames.sock export (dmabuf fences + shm content), and the no-allocation check.
Regenerate goldens after verifying the output with `SE_UPDATE_GOLDEN=1`.
`se-frames-client --want wide,tall --import --expect-fps 58` validates a running engine's
dmabufs (Vulkan import + pixel readback).
