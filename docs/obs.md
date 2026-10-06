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

### Vertical output switch

**Overview → Vertical video** persists `[obs] vertical_enabled` (default `true`).
OFF stops active tall-canvas outputs, waits for their encoders to become inactive, and only
then suppresses the engine's tall render pass and frame export. Main-canvas outputs and the
app's landscape master recording are not stopped or reconfigured. Intentionally disabled
tall feeds do not trigger stale-feed fallback or missing-source health failures.
ON resumes the outputs stopped by this switch while the main broadcast remains active;
otherwise it only arms vertical. An idle vertical output is never started just by enabling it.

Use the current stream-engine OBS plugin and the patched Aitum 1.2.4 build below. The Aitum
patch checks `stream_engine_canvas_enabled` before allocating an encoder, including automatic
starts with the main stream. Old plugins and disconnected OBS remain unconfirmed; the engine
does not stop tall rendering on an unacknowledged setting. Errors appear beside the switch.
Resolution, frame rate, encoder settings, destinations and hardware audio routing are preserved.


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

### Aitum 1.2.4 shared-encoder lifetime repair

`scripts/install-aitum.sh` builds pinned upstream 1.2.4 with
`scripts/aitum-1.2.4-ownership.patch` and atomically replaces the user plugin.
It checks that the engine is off air before installation; restart OBS off air to load it.
`--build-only` does not install. Build dependencies: Git, patch, CMake, a C++ compiler,
OBS development files, Qt6 and libcurl. The previous binary stays in the printed build directory.

The repair takes a strong reference when reusing another output's encoder, retains the
stopped output across queued Qt cleanup, and ignores an old output's delayed stop callback
when a replacement output is current. Unpatched 1.2.4 stalled vertical video after stopping
its shared replay buffer; restarting that output then crashed the GPU encoder thread.
This is separate from NVIDIA mapping-allocation failures under browser load.
Check actual frame/byte progress after replay stop and output restart, not just `active`.
Keep OBS recording and Backtrack disabled for the normal engine-only recording workload.

Off-air regression: with an **isolated** `Untitled` OBS profile already streaming wide
and vertical to `127.0.0.1` receivers, and recording/Backtrack stopped, run
`node scripts/check-aitum-lifecycle.mjs /isolated/config/obs-studio`.
It cycles Backtrack three times and requires vertical frames and bytes to keep advancing
after every stop. It does not start a stream or modify destinations; do not use it on
production OBS. The separate output-restart smoke also requires reopening its receiver.


Check: `streamctl preflight` → `obs: pass — OBS 32.2.2, plugin 0.1.0: receiving wide + tall`.

## Measured workstation load

2026-10-06 UTC, RTX 3070 / driver 610.57.04, repaired Aitum 1.2.4, AMD-offloaded
offscreen CEF and private program-preview encoding, CPU private desktop-preview encoding:

- Both 1080p60 canvases streamed to local RTMP receivers for about 19 minutes. Only
  Stream Engine recorded the master; OBS recording and Backtrack remained stopped.
- The representative workload included the drum-camera scene, verified Covers-account
  YouTube queue playback, Apple Music playback, two ordinary Chromium windows in one
  profile animating WebGL and playing local 1080p60 video, AUTO FX plus kaleidoscope/cascade
  presets, and automatic USB DMX lighting. Hardware mixer FX and audio routing were untouched.
- The final 301.5-second telemetry window averaged 59.986 engine fps and 3.09 ms GPU
  frame time (4.44 ms maximum sampled). Engine counters increased by three dropped frames,
  zero late frames, zero device recoveries and zero real-time allocation violations.
  Master recording drops remained zero. OBS reported 150 rendering/encoding skips out of
  18,091 frames (0.83%); do not add those two counters as separate losses. Each output's
  frame counter advanced by 18,092 with zero network drops or DMA-BUF fence timeouts.
- BAR1 use was 165–181 / 256 MiB: at least 75 MiB (29%) remained free. Ordinary VRAM use
  was 3,915–3,955 MiB. GPU utilization averaged 41.6% (48% sampled maximum); encoder
  utilization averaged 68.5% and briefly reached 100%. Neither number guarantees spare
  encoder capacity. No matching NVIDIA GPU-fault or AMD reset/timeout messages appeared
  in the kernel log during the repaired run.
- YouTube frames and audio samples, both browser animations/videos, and Apple Music track
  positions advanced. USB DMX sent 13,244 frames at about 44 fps with zero errors and
  changing universe values. This verifies transport, not a visual inspection of every fixture.
- The isolated OBS captures contained music, but the master's configured Studio24c hardware
  return was almost silent across the five-minute sample (mean -85.5 dBFS, peak -53.9 dBFS).
  The interface input and desk Computer/Main controls were unmuted. Playback meters and
  OBS's desktop-monitor audio therefore do **not** certify the complete hardware-mixed
  recording path. Confirm the 16R-to-Studio24c return with an audible short master recording
  before a show; do not add duplicate music inputs or change hardware FX to bypass it.
- Full CPU decoding completed without errors for the 37m15s HEVC/FLAC master and both
  19m09s H.264/AAC loopback captures. Decodable audio tracks are not proof of audible content.

This is a bounded off-air local-output test, not a full-show or remote-ingest guarantee.
The remaining OBS skips mean it is not a zero-stutter result. The 256 MiB BAR1 limit remains.
Detailed measurements and saved captures are in
`~/.cache/stream-engine-gpu-stability-Dx8Z2a/`.

### Vertical OFF and OBS preview comparison

2026-10-06 UTC, same workstation, with the vertical switch installed. A matched workload
kept the drum-camera scene, verified Covers playback, Apple Music, two WebGL/video browser
windows, AUTO FX plus alternating kaleidoscope/cascade, USB DMX, and the app master running.
Only local RTMP receivers were used; OBS recording and replay remained off.

| Measured interval | OBS skipped / total frames | Encoder utilization, sampled mean |
|---|---:|---:|
| Both canvases, OBS preview on, 118 s | 63 / 7,080 (0.89%) | 69.6% |
| Landscape only, OBS preview on, 298 s | 157 / 17,886 (0.88%) | 49.1% |
| Landscape only, OBS preview off, 120 s | 2 / 7,198 (0.028%) | 50.5% |

Vertical OFF reduced active NVIDIA encoder sessions from three to two and the tall render
pass to zero; it did not materially reduce OBS skips by itself. Disabling the redundant OBS
preview reduced the observed skip rate by about 97% in this shorter sample. Keep OBS's
preview disabled on this workstation and use Stream Engine's program picture. The saved
setting is `[BasicWindow] PreviewEnabled=false` in `~/.config/obs-studio/user.ini`; normal
OBS startup was visually checked. Re-enable through OBS's **Enable Preview** button if needed.
This changes only local display work, not broadcast resolution, FPS, bitrate, or recording quality.

The real Overview switch was exercised OFF/ON while streaming and recording: vertical
frames/bytes resumed, landscape counters kept advancing, and the master stayed in the same
file. Recording had eight startup drops before the first checkpoint and no additional drops
through the rest of the run. Engine recovery counters did not increase in the measured
intervals; BAR1 use stayed below the 192 MiB guard. CPU decode checks passed for the master
and landscape intervals spanning the OFF/ON transitions, and samples from both vertical
captures. These were interval checks, not full-file decodes.

The 0.028% result is not zero stutter or proof of whole-show/remote-ingest reliability.
The hardware-audio caveat above still applies. Saved measurements, captures and screenshots:
`~/.cache/stream-engine-vertical-xQOtQ7/`.


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
vertical_enabled = true           # Overview → Vertical video; false suspends tall outputs
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
| Vertical state | `obs.vertical.enabled` (desired), `obs.vertical.ready` (current preference applied, including OFF), `obs.vertical.error`, `render.vertical.enabled` (effective render/export gate) |
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
