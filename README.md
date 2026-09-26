# stream-engine

An all-in-one live-stream production engine for a single Linux (Omarchy) machine. It handles camera compositing, animated scene transitions, shader effects, generative overlays, DMX lights, the audio mix with audio effects and analysis, PreSonus StudioLive 16R control from MIDI, Twitch events, alerts, a chatbot, channel-point interactions, song requests with YouTube playback, moderation, and stream markers/clips. It's written in Rust and has one UI that follows the Omarchy theme.

OBS is used only as the encoder and uplink: this app sends it finished frames and audio, and OBS encodes with NVENC and streams.

Everything is hackable: drop a patch folder in (WGSL, Lua, or HTML), wire it to any event or signal, and it hot-reloads live without a restart.

**Start here:** [PLAN.md](PLAN.md). It covers the architecture, the core model, every subsystem, the repo layout, milestones with acceptance criteria, and the risks to verify first.

## Build and run

Requirements: Rust stable (`rustup`), PipeWire, libudev, LuaJIT, libturbojpeg, FFmpeg, Vulkan (see PLAN §24).

```sh
./packaging/dev-install.sh            # builds, installs ~/.local/bin/{stream-engine,stream}, the user unit, and ~/stream-project
systemctl --user enable --now stream-engine
stream-engine ui                       # the window (a client; closing it never affects the engine)
stream --trace fire twitch.cheer bits=1000 --as drumfan
stream sim gift_bomb count=50
stream get 'show.**'
stream explain fx.rgb_split.amount
stream-engine replay <session-id> --segment -1
```

Development run without systemd: `cargo run -p se-app -- daemon --project ./project-example --dev`.

The CLI talks to the engine over `$XDG_RUNTIME_DIR/stream-engine/engine.sock`; the WebSocket/OSC API (127.0.0.1:7870/7871) needs the token from `stream token`.

