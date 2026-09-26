# Audio

The audio subsystem (`crates/se-audio`, PLAN §8) runs our own PipeWire graph: it captures the
band (the StudioLive 16R main mix on the Studio 24c input), mixes the YouTube player, TTS, sound
effects, and game audio into buses, runs effect chains and ducking, publishes every bus to OBS
as its own PipeWire node, and analyses the band and music in real time (levels, bands, onsets,
tempo/beat) for rules, bindings, lights, and visuals.

```text
Studio 24c in ──► input `band` ─┐                                 ┌─► se-band ─┐
hub.audio "youtube" ──► music ──┤  bus: fx chain → duck → fader → ├─► se-music ├─► se-program ─► OBS
hub.audio "tts"     ──► tts ────┤       limiter                   ├─► se-tts   │  (fx, limiter)
audio.play (sampler) ──► sfx ───┤                                 ├─► se-sfx   │
hub.audio "game"    ──► game ───┘                                 └─► se-game ─┘
```

Crates: `se-audio` (graph, PipeWire, control), `se-dsp` (effects, sampler, wasm host; reference:
[audio-effects.md](audio-effects.md)), `se-analysis` (live + offline analysis).

## PipeWire nodes

| Node | Kind | What |
|---|---|---|
| `se-engine` | filter (DSP ports) | the whole graph; runs in PipeWire's real-time data thread |
| `se-band`, `se-music`, `se-sfx`, `se-tts`, `se-game`, `se-program` (+ `se-drums`, `se-mic`) | `Audio/Source/Virtual` | one stereo source per bus, for OBS |

Check them with `pw-cli ls Node | grep -A2 se-` or `pactl list short sources | grep se-`.

**OBS:** add *Audio Input Capture (PulseAudio)* sources: `se-program` for the stream mix, and
`se-band`, `se-music`, … on separate tracks for the recording (so VODs/clips can drop music).
The engine links everything itself (WirePlumber doesn't touch `se-engine`); links are re-made
on hotplug. `health.audio.obs` turns green when something captures `se-program`.

**Real-time scheduling:** the data thread must run `SCHED_FIFO`. The user is in the `realtime`
group (`realtime-privileges`); the limits apply after a fresh login (the PipeWire daemon and the
engine inherit `RLIMIT_RTPRIO` from the session). `health.audio.rt` reports the engine's data
thread and the PipeWire daemon's `data-loop` policy and priority, `RLIMIT_RTPRIO`, and what to do
when it's missing. Manually: `chrt -p <tid>` on the `data-loop.0` threads
(`ps -eLo tid,comm,cls,rtprio | grep data-loop`).

## Configuration

`[audio]` in `project.toml` and every `audio/*.toml` file are merged (file-name order; tables
merge key by key), so the example keeps everything in `project-example/audio/`. A broken file is
reported (`health.audio.config`, log) and the last good configuration stays live. Changes apply
live: parameter changes flow through the state tree; structural changes (inputs, buses, chains,
sounds) rebuild the graph and swap it in with a 25 ms crossfade.

Blank projects capture no hardware and keep only the bus infrastructure and program safety
limiter; they install no effect chains and do not lower any channel automatically.
Ducking is opt-in: omit `[duck]` to leave it unconfigured,
or add `[duck]` to use the music / microphone / TTS defaults shown below. For manual-only
ducking, set `targets` explicitly and use `signals = []` and `keys = []`.

```toml
quantum = 256              # frames per PipeWire cycle (32–2048); 64 for the drum monitor path
rate = 48000
max_delay = "2s"           # A/V delay buffer per input/source

[inputs.band]              # hardware capture → bus
target = "Studio 24c"      # node.name, or case-insensitive substring of node.name/description
                           # (hardware sources and virtual sources/loopbacks; never our own se-* nodes)
channels = [1, 2]          # 1-based device channels (1 = mono, 2 = stereo)
bus = "band"               # default: the input's own name (creates the bus)
gain = 0.0                 # dB
mute = false
delay = "0ms"              # A/V delay
fx = [ … ]                 # input insert chain (same format as bus chains)

[slots]                    # hub.audio producers → buses (patterns; config wins over defaults)
"youtube" = "music"        # or { bus = "music", delay = "120ms" }
"web.*" = "music"          # defaults: youtube, web.* → music; tts, tts.* → tts; sfx.* → sfx;
                           #           game, game.* → game; patch.* → sfx; timecode.* → none

[buses.music]
gain = -3.0                # dB (live: audio.bus.music.gain)
mute = false
limiter = false            # brickwall limiter (program: on by default)
ceiling = -1.0             # dBFS
to_program = true          # summed into program (all buses except program)
node = true                # publish se-music
fx = [ … ]

[duck]
targets = ["music"]        # buses that duck
depth = -12.0              # dB
attack = "40ms"
release = "400ms"
hold = "250ms"             # stay ducked this long after the last key activity
signals = ["mic.talking"]  # duck while any of these signals/state values is > 0.5
keys = ["tts"]             # duck while any of these buses is above `threshold`
threshold = -45.0          # dBFS

[sounds.airhorn]           # sampler (audio.play {sound})
file = "assets/sounds/airhorn.wav"   # or files = [...] (round robin) or layers:
# layers = [{ vel = [0.0, 0.5], files = ["soft1.wav", "soft2.wav"] }, { vel = [0.5, 1.0], files = ["hard.wav"] }]
pick = "round_robin"       # or "random"
gain = -4.0
bus = "sfx"
choke = 1                  # sounds in the same choke group cut each other
voices = 2                 # polyphony per sound (oldest voice is stolen with a fade)

[analysis]
buses = ["band", "music"]  # analysed into <bus>.* signals
beat_source = "auto"       # beat.* from band when confident, else music; or a bus name
min_bpm = 70
max_bpm = 180
mic = "vox"                # input analysed as the mic: mic.level, mic.hype, mic.talking
talk_threshold = -42.0     # dBFS: mic.talking = a voice (built-in voice detector) above this level
talk_hold = "600ms"

[sources."patch.drone"]    # `dsp` patch with layer = "audio-source" → bus
bus = "sfx"
gain = -6.0

[monitor]                  # low-latency drummer monitor (M12)
target = "Studio 24c"      # sink node
channels = [1, 2]
buses = ["drums"]          # post-chain, pre-fader
inputs = ["kick", "snare"] # post input chain
gain = 0.0
fx = [ … ]                 # lighter chain than the stream path

[direct."timecode.ltc"]    # a hub.audio slot straight to a device channel (never mixed)
target = "Studio 24c"
channel = 2

[drums.pads.kick]          # per-drum triggers on close mics (M12) → drums.kick {velocity}
input = "kick"
channel = 0                # channel within the input (0-based)
band = [40, 150]           # Hz (defaults by name: kick/snare/hat/tom/cymbal)
threshold = -30            # dBFS (live: audio.drums.kick.threshold)
retrigger = "40ms"         # flams inside this window are one hit
scan = "3ms"               # peak window for velocity (= decision latency)
bleed = 6.0                # reject when another pad's band peak is this many dB louder
velocity_curve = 1.0
velocity_range = [-40, -6] # dBFS mapped to velocity 0–1
layer = { sound = "kick_layer", gain = -6 }   # sampler layer fired on the audio thread
```

### Effect chain entries

```toml
fx = [
  { name = "stutter", kind = "stutter", division = "1/8", quantize = "1/16",
    trigger = { attack = "5ms", hold = "2s", release = "60ms", retrigger = "replace" } },
  { name = "filter", kind = "djfilter" },
  { name = "comp", kind = "compressor", key = "band", sidechain = true },
  { name = "space", kind = "reverb", wet = 0.2, dry = 1.0, when = "mode == 'chill'" },
  { name = "ringmod", patch = "ringmod" },
]
```

| Key | Meaning |
|---|---|
| `name` | id in the chain (address segment); defaults to the kind |
| `kind` | built-in effect ([audio-effects.md](audio-effects.md)) — or `patch = "<id>"` for a `dsp` patch |
| `trigger` | `true` or `{ attack, hold, release, retrigger }`: one-shot effect; the core's trigger envelope (`<fx>.env`) is applied to `wet`, and `<fx>.active` starts/stops performance effects |
| `wet`, `dry` | output mix (defaults per effect: inserts 1/0, delay/reverb sends 0.35/1, …) |
| `bypass` | start bypassed |
| `when` | expression (rules language: `mode`, `scene`, any state/signal); bypassed while false |
| `key` | sidechain key bus (mono sum of its input) for `sidechain = true` dynamics |
| anything else | initial parameter value (choices by name, e.g. `division = "1/8"`) |

Mix law: `out = dry_in · (1 − a·e·(1 − dry)) + fx(in) · a·e·wet` (a = enabled, e = envelope), all
smoothed per sample; bypass and hot swaps crossfade; the dry path is delayed by the effect's
latency, and buses are delay-aligned before the program sum (latency compensation).

## Addresses, signals, events, actions

State (declared with metadata — the UI builds controls from it):

| Address | |
|---|---|
| `audio.bus.<b>.gain` (dB −60…12), `.mute`, `.limiter`, `.ceiling` | bus strip |
| `audio.bus.<b>.level`, `.peak` | readback dBFS, ~1 Hz (use the signals for meters) |
| `audio.bus.<b>.ducked` | current duck gain reduction (dB) on duck targets |
| `audio.bus.<b>.fx.<e>.<param>`, `.wet`, `.dry`, `.bypass` | effect parameters |
| `audio.bus.<b>.fx.<e>` (trigger) → `.env`, `.active` | one-shot effects (`trigger audio.bus.music.fx.stutter`) |
| `audio.input.<n>.gain`, `.mute`, `.delay_ms`, `.fx.<e>.*` | hardware inputs |
| `audio.input.<slot>.gain`, `.mute`, `.delay_ms` | hub.audio sources (`web.player` → `audio.input.web_player`) |
| `audio.duck.active`, `.depth`, `.attack`, `.release` | ducking |
| `audio.monitor.gain`, `.roundtrip_ms`, `audio.drums.<pad>.threshold`, `audio.source.<id>.gain` | M12 |
| `patch.<id>.<param>`, `patch.<id>` (trigger), `patch.<id>.error` | `dsp` patches |
| `perf.audio.xruns`, `.load`, `.dsp_ms`, `.quantum`, `.rate`, `.latency_ms`, `.driver_delay_ms`, `.allocs` | perf |
| `health.audio.pipewire`, `.rt`, `.obs`, `.xruns`, `.config`, `.input.<n>`, `.mic` (with `analysis.mic`: sound within the last minute) | preflight |

Signals: `band.*` and `music.*` (`level`, `peak` linear; `lufs` short-term, `lufs_m`; `bass`,
`mid`, `high` linear band RMS; `b.0`…`b.30` 1/3-octave bands; `centroid` 0–1; `kick`, `snare`,
`hat` onset envelopes; `flux`, `novelty`), `beat.bpm`, `beat.phase`, `beat.confidence`,
`mic.level`, `mic.hype`, `mic.voice` (voice-detector score 0–1), `mic.talking` (with `analysis.mic`), `audio.<bus>.level|peak` and
`audio.input.<n>.level|peak` (meters, linear), `audio.duck.amount` (0–1), `drums.<pad>`.
All level-like signals are linear (proportional to the audio), so binding `auto_normalize`
gives the same feel on quiet and loud songs.

Events: `band.kick|snare|hat` and `music.kick|snare|hat` `{velocity}` (timestamped at the
onset), `beat` `{index, bpm, downbeat, source}`, `band.drop`/`music.drop` `{strength}`,
`band.section` `{novelty}`, `mic.hype` `{level}`, `drums.<pad>` `{velocity}`.

Actions:

| Action | |
|---|---|
| `audio.play {sound, gain?, velocity?, pan?, pitch?}` (`audio.play airhorn`) | sampler one-shot (presets: `sound = "airhorn"`) |
| `audio.stop` | fade out all sounds |
| `audio.duck {on: true\|false\|"toggle", hold?, depth?}` | manual ducking (release with `on=false`; `hold="8s"` auto-releases) |
| `audio.panic` | safe mix: sounds stopped, effects released, ducking off, bus gains/mutes back to the project values (the core's `panic` sends it) |
| `audio.tap` / `audio.tap.clear` | tap tempo (≥ 2 taps within 2 s) / back to detected tempo |
| `audio.fx.trigger\|release\|bypass\|enable\|set bus=<b>\|input=<i> fx=<e> …` | effect control by name (`set` takes `param=… value=…`) |
| `audio.monitor.measure {input?}` | click on the monitor output, detect it on the input → `audio.monitor.roundtrip_ms` (needs a loopback cable) |
| `audio.reload` | re-read sounds and dsp patches and rebuild the graph |

Queries: `audio.mix` (structure + live levels, fx activity, links, consumers, perf, errors — used
by the Mix view), `audio.devices` (PipeWire audio nodes), `analysis.grid {path}` /
`{media}` (offline analysis, below).

## Sounds

`audio.play` looks up `[audio.sounds.<name>]`, then `assets/sounds/<name>.{wav,flac,ogg,mp3}`
(any rate, mono or stereo, ≤ 60 s; resampled to 48 kHz at load; cached by mtime). New projects
contain no sounds; add the files and any sampler settings explicitly.

## `dsp` patches (WebAssembly)

A patch folder with `kind = "dsp"` and `main.wasm` (see `templates/patches/dsp/ringmod/`,
available through `patch.new`, built from Rust with `./build.sh`, target `wasm32-unknown-unknown`). Modules are compiled ahead
of time on load (wasmtime/cranelift), have no imports, and implement ABI v1:
`se_dsp_abi() -> 1`, `init(sample_rate, channels, max_frames) -> i32` (all allocation here),
`input_buffer()`, `output_buffer()`, `params_buffer()`, `process(in, out, frames, params)` on
planar f32 blocks, optional `reset()` and `latency()`. Params block: `[env, bpm, beat phase, bar
phase, trigger count, 0, 0, 0, manifest params…]` (manifest params in alphabetical order).

Use one as an insert (`{ patch = "ringmod" }`) or a source (`[sources."patch.<id>"]`). A patch
with a manifest `trigger` is gated by its envelope (`trigger patch.ringmod`). Over its
`budget.cpu_ms` for 3 blocks in a row, a trap, or memory growth → it is bypassed with a
crossfade and `patch.<id>.error` says why. Rebuilding `main.wasm` hot-swaps every instance at a
block boundary with a crossfade; a module that fails to compile leaves the old one live.

## Offline analysis (library songs)

`analysis.grid {path: "songs/x.flac"}` decodes and analyses a file (beat grid, downbeats,
sections with labels, chorus candidates, waveform peaks per 10 ms), caches the result in the
runtime DB by content hash, and returns
`{media: "file:<hash>", duration, bpm, beats, downbeats, sections: [{start, end, label}], chorus, peaks}`
(seconds). `analysis.grid {media: "file:<hash>"}` returns the cached result or null. Timelines use
it for beat-snapped cues and suggested chorus cues. YouTube audio gets live analysis only.

## Taps (Rust API for other subsystems)

`se_audio_taps::open("input.ltc.0", seconds)` (crate `se-audio-taps`, re-exported as `se_audio::taps`) → `Tap { rate, samples: rtrb::Consumer<f32>,
clock: rtrb::Consumer<BlockStamp> }`: a lock-free mono copy of a raw input channel
(`input.<input>.<ch>`) or a bus input channel (`bus.<bus>.<ch>`), with the master-clock time of
every block. Timelines read LTC this way (`[timecode] ltc_in = "input.ltc.0"` needs an
`[audio.inputs.ltc]` on a free interface channel).

## Performance, latency, xruns

* The audio callback never allocates, locks, or does I/O: parameters arrive as lock-free
  messages and are smoothed per sample; new graphs, sounds and wasm instances are built on the
  control thread and swapped in; replaced objects travel back to be freed off the audio thread.
  Debug builds count allocations inside the callback (`perf.audio.allocs`, must stay 0).
* `perf.audio.latency_ms` = one quantum + effect lookahead (latency-compensated across buses);
  `perf.audio.driver_delay_ms` = what the driver reports to the hardware. At quantum 256 the
  engine adds 5.3 ms (+ the program limiter's lookahead); at 64, 1.3 ms.
* Xruns are counted from the driver clock (reported xrun time, recovery flag, and position
  discontinuities): `perf.audio.xruns`, `health.audio.xruns` (last minute).
* Soak: `scripts/audio-soak.sh --duration 4h` attaches to the running engine (or `--start
  --project <dir> --quantum 256` runs its own; `BIN_DIR=<target>/debug` picks the binaries,
  `--socket` a dev engine) and samples xruns, DSP load, and PipeWire's own per-node error
  counter (`pw-top`) every minute; exit status 0 = no xruns.
* Beat clock: `beat.*` signals/events only come from a bus above −50 dBFS RMS (no tempo from
  the noise floor of a silent input).

## Monitor path (M12, live drums)

Quantum 64 at 48 kHz with `SCHED_FIFO` gives a 1.33 ms period. The monitor mix (`[monitor]`)
takes the drum inputs after their (lighter) chains and plays them on the interface outputs in
the same cycle. Round trip = input period + output period + converter/USB latency; measure it
with a loopback cable from the monitor output to an input and `audio.monitor.measure`.

## Troubleshooting

* `health.audio.input.band` fails: the Studio 24c isn't present or its profile has no input —
  `pactl list short sources | grep -i 24c`.
* No sound in OBS: `health.audio.obs`; in OBS pick the `stream-engine program` source.
* `health.audio.rt` fails with `RLIMIT_RTPRIO=0`: log out and in again (see above).
* Xruns at 64: raise `quantum` to 128/256 for the stream path; keep 64 only with the monitor.
