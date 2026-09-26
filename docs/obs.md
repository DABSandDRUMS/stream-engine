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

Either run `streamctl do obs.setup` while OBS and the engine are running (it only **adds**
`stream-engine: wide` to the main canvas's current program scene and `stream-engine: tall` to the
current scene of the `Vertical` canvas, scaled to fill; it never removes, reorders, or changes
anything else), or click it yourself:

1. Restart OBS after installing. `Help → Log Files → View Current Log` shows
   `[stream-engine] loaded v0.1.0`.
2. Main scene (normal Scenes dock): **Sources → + → `stream-engine: wide`** → OK → OK. Right-click
   the new source → **Transform → Fit to screen** (Ctrl+F).
3. Aitum Vertical dock, its scene: **+ → `stream-engine: tall`** → OK → OK, then Fit to screen.
4. The engine owns the cameras now: hide (eye icon) OBS's own `Video Capture Device` sources, or
   they will fight the engine for `/dev/video*`.
5. Audio (PLAN §5): add one **Audio Input Capture (PulseAudio)** per engine node and name each OBS
   source exactly like the node (`se-program`, `se-band`, `se-music`, `se-sfx`, `se-tts`,
   `se-game`). For separate recording tracks switch **Settings → Output → Output Mode: Advanced**,
   enable the tracks under Recording, and assign sources to tracks in **Advanced Audio Properties**.
   The plugin reports which sources feed which recording track (session meta `recordings[].tracks`).

Check: `streamctl preflight` → `obs: pass — OBS 32.2.2, plugin 0.1.0: receiving wide + tall`.

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
| Events | `obs.stream_started`, `obs.stream_stopped`, `obs.record_started {path,canvas}`, `obs.record_stopped {path,canvas}`, `obs.fallback {active,canvas,canvases,scene,from/to,reason}` — timestamped on the master clock |
| Actions | `obs.stream.start|stop`, `obs.record.start|stop`, `obs.fallback.on|off`, `obs.fallback.setup`, `obs.setup` (result or error in the engine log) |
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
