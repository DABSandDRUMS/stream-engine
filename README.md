# stream-engine

An all-in-one live-stream production engine for a single Linux (Omarchy) machine. It handles camera compositing, animated scene transitions, shader effects, generative overlays, DMX lights, the audio mix with audio effects and analysis, PreSonus StudioLive 16R control from MIDI, Twitch events, alerts, a chatbot, channel-point interactions, song requests with YouTube playback, moderation, and stream markers/clips. It's written in Rust and has one UI that follows the Omarchy theme.

OBS is used only as the encoder and uplink: this app sends it finished frames and audio, and OBS encodes with NVENC and streams.

Everything is hackable: drop a patch folder in (WGSL, Lua, or HTML), wire it to any event or signal, and it hot-reloads live without a restart.

**Start here:** [PLAN.md](PLAN.md). It covers the architecture, the core model, every subsystem, the repo layout, milestones with acceptance criteria, and the risks to verify first.

Status: planning. Nothing is built yet.
