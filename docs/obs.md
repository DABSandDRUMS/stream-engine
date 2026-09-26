# OBS link (plugin + `se-obs`)

OBS stays the encoder/uplink (PLAN §5). Our only code inside OBS is the `stream-engine` module
(`obs-plugin/`, C, GPL-2.0-or-later). The engine side is `crates/se-obs`. They talk over two Unix
sockets described in [frames-protocol.md](frames-protocol.md).

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

Either run `streamctl do obs.setup` while OBS and the engine are running (the UI's Get started →
OBS "Add our video and sound to OBS" and Sound → "Add our sound to OBS" run the same thing), or
click it yourself. `obs.setup` only **adds**: `stream-engine: wide` to the main canvas's current
program scene and `stream-engine: tall` to the current scene of the `Vertical` canvas, scaled to
fill; and one PulseAudio capture per engine audio node (`se-program` on track 1 for the stream,
`se-band`/`se-music`/`se-sfx`/`se-tts`/`se-game` on tracks 2–6 for the recording) in the main
program scene and in the fallback scene, so the sound carries on during technical difficulties.
Sources that already exist under those names are reused as they are. It never removes, reorders,
or changes anything else; running it again adds nothing.

1. Restart OBS after installing. `Help → Log Files → View Current Log` shows
   `[stream-engine] loaded v0.1.0`.
2. Main scene (normal Scenes dock): **Sources → + → `stream-engine: wide`** → OK → OK. Right-click
   the new source → **Transform → Fit to screen** (Ctrl+F).
3. Aitum Vertical dock, its scene: **+ → `stream-engine: tall`** → OK → OK, then Fit to screen.
4. The engine owns the cameras now: hide (eye icon) OBS's own `Video Capture Device` sources, or
   they will fight the engine for `/dev/video*`.
5. Audio (PLAN §5): add one **Audio Input Capture (PulseAudio)** per engine node and name each OBS
   source exactly like the node (`se-program`, `se-band`, `se-music`, `se-sfx`, `se-tts`,
   `se-game`); in **Advanced Audio Properties** put `se-program` on track 1 only and each stem on
   its own track (2–6). `obs.setup` does all of this.
6. Separate recording tracks (the only step `obs.setup` leaves to you, because it changes your
   encoder settings): **Settings → Output → Output Mode: Advanced**, check that Streaming and
   Recording still use NVENC, and tick tracks 1–6 under Recording. Talk clips can omit music
   while requested-song performance clips keep it (docs/clips.md). The plugin reports which
   sources feed which recording track (session meta `recordings[].tracks`).

Check: `streamctl preflight` → `obs: pass — OBS 32.2.2, plugin 0.1.0: receiving wide + tall`.

## Recording destination and automation

Set **Clipping → Recording → Save recordings in** to choose the folder. The app persists it as
`[recording] dir` in `project.toml`, creates a folder per show, and directs OBS's **actual output
path** there before starting a file. The default is `~/Videos/Stream Engine`. When `[recording]
auto = true`, recording starts in preshow/live and stops off air; the manual Start/Stop recording
controls remain available. Changing the folder never moves an in-progress file. The UI shows the
real OBS recording path, track count, free space and any capture errors. Separate tracks still
require OBS Advanced Output setup in step 6 above; selecting a folder does not change encoders
or tracks. See [clips.md](clips.md) for the show timeline and review flow.

## Fallback scene (§22)

Each source instance watches its feed. A feed is **stale** when no frame arrived for `stale_ms`
(default 500 ms) or the engine said goodbye. When a stale source is visible in a canvas' program
(and, in the default `live` mode, a stream or recording is running), that canvas switches to the
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
| State | `obs.link`, `obs.version`, `obs.plugin.{version,installed}`, `obs.stream.{active,kbps,dropped,total,lag_ms,congestion}`, `obs.record.{active,paused,path,dir,kbps}`, `obs.fps`, `obs.render.{ms,lagged}`, `obs.encode.skipped`, `obs.stale.{wide,tall}`, `obs.fallback.active`, `obs.scene`, `obs.output.<id>.{active,kbps,dropped,total,label,canvas,kind}` (every stream/record output incl. Aitum's), `health.obs` |
| Events | `obs.stream_started`, `obs.stream_stopped`, `obs.record_started {path,canvas}`, `obs.record_stopped {path,canvas}`, `obs.fallback {active,canvas,canvases,scene,from/to,reason}` — timestamped on the master clock; `obs.dry_run {action}` (a stream start skipped in rehearsal) |
| Actions | `obs.stream.start|stop`, `obs.record.start|stop`, `obs.fallback.on|off`, `obs.fallback.setup`, `obs.setup` (result or error in the engine log). In show mode `rehearsal`, `obs.stream.start` is skipped (never on air during a practice run); recording works. A mode change sent just before `obs.stream.start` counts, so "Go live" from rehearsal starts the stream. |
| Query | `obs` (link, config, recordings, per-canvas feed stats) |
| Clock | `hub.clock` `obs_stream` / `obs_record`: other clock = ns since the first frame of the stream / current main recording file |
| Session meta | `recordings` = `[{canvas, path, output, start_ns, end_ns?, tracks:[{index,mixer,name,sources,devices}]}]` (one entry per file incl. splits), `clock` = serialized `se_clock::Mappings` |

`lag_ms` is video the encoder could not keep up with during the last second (skipped frames ×
frame interval).

## Development

- `ctest --test-dir target-obs/obs-plugin` runs the plugin's socket/protocol code against fake
  servers (no OBS needed); `cargo test -p se-obs` runs the engine side against a fake plugin.
- `se-fake-frames` (`-DSE_OBS_BUILD_TOOLS=ON`) serves animated `wide`/`tall` canvases without the
  renderer: GPU dmabufs rendered with GLES on the NVIDIA node with real sync_file fences, `--linear`
  CPU-written linear buffers (NVIDIA EGL cannot import those — exercises the plugin's automatic
  dmabuf → shm fallback), `--shm` memfds. `SIGUSR1` pauses/resumes (stale feed), `SIGUSR2` says
  goodbye.
- An isolated OBS for testing: copy `~/.config/obs-studio` to `$T/obs-studio`, point
  `global.ini [Locations]` at `$T`, then `XDG_CONFIG_HOME=$T SE_RUNTIME_DIR=… obs --multi`.
