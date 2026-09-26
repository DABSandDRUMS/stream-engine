# stream-engine

An all-in-one live-stream production engine for a single Linux (Omarchy) machine. It handles camera compositing, animated scene transitions, shader effects, generative overlays, DMX lights, the audio mix with audio effects and analysis, PreSonus StudioLive 16R control from MIDI, Twitch events, alerts, a chatbot, channel-point interactions, song requests with YouTube playback, moderation, and stream markers/clips. It's written in Rust and has one UI that follows the Omarchy theme.

OBS is used only as the encoder and uplink: this app sends it finished frames and audio, and OBS encodes with NVENC and streams.

Everything is hackable: drop a patch folder in (WGSL, Lua, or HTML), wire it to any event or signal, and it hot-reloads live without a restart.

**Start here:** [PLAN.md](PLAN.md). It covers the architecture, the core model, every subsystem, the repo layout, milestones with acceptance criteria, and the risks to verify first.

**Running a show:** [docs/operator-guide.md](docs/operator-guide.md), a plain-language guide to the app from before the stream to after it. **Building on it:** [docs/api.md](docs/api.md), the socket/WebSocket/OSC API reference; [docs/patches.md](docs/patches.md) for patches; the other files in [docs/](docs/) cover each subsystem.

New projects start **blank**: device setup, source declarations, neutral audio routing,
and authoring tools, without scenes, looks, presets, controller actions, or automation.
Create and connect those explicitly in the app or project files. Optional patch templates
are available on demand; setup never installs them into your show. Preview and program
canvases remain empty until you create a scene; connected devices remain available in setup.

## Build and run

Requirements: Rust stable (`rustup`), PipeWire, libudev, LuaJIT, libturbojpeg, FFmpeg, Vulkan (see PLAN §24).

```sh
./packaging/dev-install.sh            # builds, installs ~/.local/bin/{stream-engine,streamctl}, the user unit, and ~/stream-project
systemctl --user enable --now stream-engine
stream-engine ui                       # the window (a client; closing it never affects the engine)
streamctl preflight                    # connections and setup still required
streamctl get 'show.**'
streamctl query scenes                 # empty until you create scenes
stream-engine replay <session-id> --segment -1
```

Development run without systemd: `cargo run -p se-app -- daemon --project ./project-example --dev`.

The CLI talks to the engine over `$XDG_RUNTIME_DIR/stream-engine/engine.sock`; the WebSocket/OSC API (127.0.0.1:7870/7871) needs the token from `streamctl token`.

## Checks on the real machine

- `streamctl preflight`: every health check (the UI's Go live window shows the same list).
- `scripts/audio-soak.sh --duration 4h`: audio xruns and allocations in the real-time callback.
- `scripts/soak.sh --duration 8h [--allow-kill-service]`: simulator storm at ~5 events/s with memory, frame, xrun sampling and a `kill -9` restore check (off air: it fires real effects).
- `scripts/acceptance-surfaces.sh`: legacy guided hardware checks requiring explicitly configured test mappings and content; not applicable to a blank project.

