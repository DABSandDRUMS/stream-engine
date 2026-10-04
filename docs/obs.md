# OBS link (plugin + `se-obs`)

OBS stays the streaming encoder/uplink (PLAN §5), not the recorder. Our only code inside OBS is
the `stream-engine` module (`obs-plugin/`, C, GPL-2.0-or-later). The engine side is `crates/se-obs`.
They talk over two Unix sockets described in [frames-protocol.md](frames-protocol.md).

## Install / update the plugin

```sh
obs-plugin/install.sh            # cmake build + ctest + install for this user
obs-plugin/install.sh --no-tests
obs-plugin/install.sh --uninstall
```

Installs `~/.config/obs-studio/plugins/stream-engine/bin/64bit/stream-engine.so` and
`…/stream-engine/data/`. The file is replaced by rename, so a running OBS is unaffected; restart OBS
to load the new build. Packages: `cmake -S obs-plugin -B build -DCMAKE_INSTALL_PREFIX=/usr
-DSE_OBS_BUILD_TESTS=OFF && cmake --build build && DESTDIR=… cmake --install build`
(→ `/usr/lib/obs-plugins/stream-engine.so`, `/usr/share/obs/obs-plugins/stream-engine/`).
Build deps: obs-studio 32 headers (libobs + obs-frontend-api CMake configs), simde, jansson, EGL.

## One-time OBS setup (the owner does this once)

Run `streamctl do obs.setup` while OBS and the engine are running (or use **Get started → OBS
→ Add our video to OBS**). It adds `stream-engine: wide` to the main canvas and
`stream-engine: tall` to the Vertical canvas; existing sources are untouched. It does **not**
configure audio or recording. Choose the stream's audio in OBS; choose the app recorder's
video and audio independently in **Settings → Accounts & app → Recording**.

1. Restart OBS after installing. `Help → Log Files → View Current Log` shows
   `[stream-engine] loaded v0.1.0`.
2. Main scene (normal Scenes dock): **Sources → + → `stream-engine: wide`** → OK → OK. Right-click
   the new source → **Transform → Fit to screen** (Ctrl+F).
3. Aitum Vertical dock, its scene: **+ → `stream-engine: tall`** → OK → OK, then Fit to screen.
4. The engine owns the cameras now: remove obsolete OBS `Video Capture Device` sources after
   backing up the collection, so OBS cannot compete with the engine for `/dev/video*`.
5. Audio: choose the intended streaming feed in OBS **Settings → Audio** or an **Audio Input
   Capture** source. This may be an interface mix or a configured virtual program source.
   Check that OBS's meter moves for every intended sound and avoid duplicate captures.
   A global Mic/Aux source remains audible through the technical-difficulties fallback;
   if you use a scene source instead, put it in that scene as well.
6. In **Settings → Output**, configure the streaming encoder and destinations. OBS recording
   settings and track assignments do not configure the app recorder.

Keep Aitum Stream Suite when the show uses its separate Vertical canvas, vertical stream,
replay buffer or virtual camera. Removing the plugin is not an equivalent single-instance
regular OBS configuration. Avoid duplicate camera/audio captures first; plugin removal needs
a measured benefit and an equivalent output plan.

For headroom, keep OBS **Recording Quality → Same as stream** when an OBS copy is
needed: it shares the stream encoder. A separate HQ recording starts another NVENC
session; the app's independent HEVC master is already a separate session.
Do not mistake inactive Advanced-mode settings for the encoder used by Simple mode.

Measure after startup using differences in `obs.render.lagged`,
`obs.encode.skipped`, `obs.stream.dropped`, `perf.dropped` and `perf.late`;
these are cumulative counters. Check the app recorder's `feeds[].dropped` through
`streamctl query recording.status` separately. Stable engine FPS and zero network
drops do not prove OBS rendered/encoded every frame. Also inspect `streamctl query obs`
for both canvases' frame progress, age and producer-fence timeouts. A private loopback
stream exercises encoding without broadcasting, but cannot validate remote uplink
quality or a separate Aitum encoder unless those paths are actually exercised.

Check: `streamctl preflight` → `obs: pass — OBS 32.2.2, plugin 0.1.0: receiving wide + tall`.

## Canvas color handling

The engine exports sRGB-encoded RGBA pixels. In OBS's linear-light draw path, the plugin
decodes DMA-BUF samples in the shader: EGL imports expose plain RGBA storage, so marking the
texture sample as sRGB does not decode it. Shared-memory uploads retain OBS's sRGB texture
storage and use hardware decoding instead. Both paths encode once into OBS's SDR framebuffer.

If OBS looks washed out or brighter than the same engine canvas, update the plugin and restart
OBS **while off air**. Do not compensate by changing camera exposure or adding a gamma filter.
The correction does not require changing the normal NV12 / Rec.709 / Partial output settings.

## GPU buffer ownership

DMA-BUF frames stay held until a GL completion fence explicitly signals. An unsignaled fence,
failed wait or failed fence allocation is not completion; the plugin retries a replacement
fence in the same graphics context without returning a sampled buffer to the renderer.
A full retirement queue discards only the unseen incoming frame. If GL synchronization is
unavailable, the source requests the existing shared-memory transport before importing DMA-BUFs.

Disconnect invalidates the import epoch immediately: the plugin stops submitting new draws
from that DMA-BUF pool and drops its cached textures on the next graphics tick. Shared-memory
sources may retain their already-copied last image. This prevents repeated sampling of a
disconnected pool; it does not supply a cross-process completion guarantee for GPU commands
already submitted when a client disconnects or dies.

## App recording is independent

Select recording sources, destination and automation in **Settings → Accounts & app → Recording**.
The app captures the selected feeds using FFmpeg and writes a folder per show under
`[recording] dir` (default `~/Videos/Stream Engine`). **Clipping** shows compact capture status
and Start/Stop controls above the recordings library. OBS need not be recording or connected.
Changing recording settings takes effect on the next capture. A mixed audio feed is retained
whole; separate tracks exist only when distinct feeds are selected. Available-source choices
are not exhaustive: manual FFmpeg format/source pairs are supported.
See [clips.md](clips.md) for configuration, the show timeline and review.

## Fallback scene (§22)

Each source instance watches its feed. A feed is **stale** when no frame arrived for `stale_ms`
(default 500 ms) or the engine said goodbye. When a stale source is visible in a canvas' program
(and, in the default `live` mode, a stream is running), that canvas switches to the
fallback scene — main canvas through the frontend (studio mode aware), other canvases (Aitum
`Vertical`) by swapping their output channel, restored exactly. When the feed is fresh again the
canvas switches back, unless the operator changed scenes in the meantime (then nothing is touched).
The scene `Technical Difficulties` (dark background + text) is created in a canvas **only if it does
not exist**; customize it freely — the plugin only looks it up by name. Manual control:
`streamctl do obs.fallback.on|off`; `streamctl do obs.fallback.setup` creates missing fallback scenes
without switching. The last engine config is persisted in
`~/.config/obs-studio/plugin_config/stream-engine/config.json`, so the fallback works while the
engine is down.

## Engine configuration (`project.toml`)

```toml
[obs]
# socket = "~/…/obs.sock"        # default $SE_RUNTIME_DIR/obs.sock, else $XDG_RUNTIME_DIR/stream-engine/obs.sock
stale_ms = 500                    # or "500ms"
fallback_mode = "live"            # off | live | always
fallback_scene = "Technical Difficulties"
fallback_text = "Technical difficulties — back in a moment"
command_timeout = "10s"           # how long obs.* actions wait for OBS
```

Hot-reloaded; a broken `[obs]` keeps the last good settings. The plugin finds the sockets in
`$SE_RUNTIME_DIR` if set (dev instances), else `$XDG_RUNTIME_DIR/stream-engine/`.

## What the engine sees

| Kind | Names |
|---|---|
| State | `obs.link`, `obs.version`, `obs.plugin.{version,installed}`, `obs.stream.{active,kbps,dropped,total,lag_ms,congestion}`, `obs.fps`, `obs.render.{ms,lagged}`, `obs.encode.skipped`, `obs.stale.{wide,tall}`, `obs.fallback.active`, `obs.scene`, `obs.output.<id>.{active,kbps,dropped,total,label,canvas,kind}` (generic output telemetry incl. Aitum's; independently active record outputs may appear, without file ownership), `health.obs` |
| Events | `obs.stream_started`, `obs.stream_stopped`, `obs.fallback {active,canvas,canvases,scene,from/to,reason}` — timestamped on the master clock; `obs.dry_run {action}` (a stream start skipped in rehearsal) |
| Actions | `obs.stream.start|stop`, `obs.fallback.on|off`, `obs.fallback.setup`, `obs.setup` (result or error in the engine log). In show mode `rehearsal`, `obs.stream.start` is skipped (never on air during a practice run). App recording remains independent. A mode change sent just before `obs.stream.start` counts, so "Go live" from rehearsal starts the stream. |
| Query | `obs` (link, config, per-canvas feed stats) |
| Clock | `hub.clock` `obs_stream`: other clock = ns since the first frame of the stream |

Recording actions, state and per-file session metadata belong to the app recorder, not this
protocol; see [clips.md](clips.md).

`lag_ms` is video the encoder could not keep up with during the last second (skipped frames ×
frame interval).

## Development

- `ctest --test-dir target-obs/obs-plugin` runs socket/protocol tests against fake servers,
  plus production GPU-lifetime logic with substituted GPU calls when the plugin is built.
  No running OBS or GPU is needed for these tests; `cargo test -p se-obs` runs the engine side
  against a fake plugin. Actual OBS DMA-BUF import and drawing still need an off-air smoke run.
- `se-fake-frames` (`-DSE_OBS_BUILD_TOOLS=ON`) serves animated `wide`/`tall` canvases without the
  renderer: GPU dmabufs rendered with GLES on the NVIDIA node with real sync_file fences, `--linear`
  CPU-written linear buffers (NVIDIA EGL cannot import those — exercises the plugin's automatic
  dmabuf → shm fallback), `--shm` memfds. `SIGUSR1` pauses/resumes (stale feed), `SIGUSR2` says
  goodbye.
- An isolated OBS for testing: copy `~/.config/obs-studio` to `$T/obs-studio`, point
  `global.ini [Locations]` at `$T`, then `XDG_CONFIG_HOME=$T SE_RUNTIME_DIR=… obs --multi`.
