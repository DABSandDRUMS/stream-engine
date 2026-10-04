# Audio

The audio graph is configurable: hardware inputs, virtual sources and generated sounds can
feed named buses, published PipeWire sources, playback outputs and a monitor mix. No particular
interface, mixer or motherboard output is required.

The app captures feeds selected in **Settings → Accounts & app → Recording**.
OBS only streams and has its own audio selection. A finished hardware or software mix remains
one complete recorded source; bus names do not create isolated stems. Separate recording
tracks exist only when separate feeds are actually selected.

`[playback]` routes selected post-fader buses to a chosen output. For an external mixer return,
exclude any input/bus already carrying that return to avoid feedback. Choose the target and
buses to match your actual wiring; do not assume an output is safe because of its device name.

Crates: `se-audio` (graph, PipeWire, control), `se-dsp` (effects, sampler, wasm host; reference:
[audio-effects.md](audio-effects.md)), `se-analysis` (live + offline analysis).

This workstation's external-mixer route is:

```text
YouTube → music bus → motherboard rear stereo output → StudioLive 16R
StudioLive 16R whole mix → Studio 24c stereo input → OBS
```

`~/stream-project/audio/graph.toml [playback]` sends only `music` to
`alsa_output.usb-Generic_USB_Audio-00.HiFi__Speaker__sink`, channels 1/2.
The `band` bus captures the 24c input for analysis and is excluded from playback.
Do not capture `se-music` or `se-program` alongside the 24c whole mix in OBS:
that would duplicate the music and bypass the 16R channel fader.
The saved OBS Mic/Aux source uses the default input, currently the Studio 24c;
desktop defaults are not changed by this playback route.


## Inputs, app audio, and buses

- **Audio inputs** are capture feeds you explicitly add: an interface channel, microphone,
  virtual source, or loopback. An empty input configuration captures nothing; it does not
  guess an interface or recreate a removed input.
- **App audio** comes from running features such as song playback, TTS, sound effects and
  audio patches. These producers are not additional physical inputs.
- **Mix buses** combine those feeds. `music`, `sfx`, `tts`, `scene` and `program` always exist
  for app audio; audio from every scene layer (`patch.*`) goes to the one `scene` bus
  ("Scene audio"). `band`, `game`, `drums` and `mic` exist only when an input, route or setting uses
  them; none of them implies hardware. Bus strips and their effects are under
  **Sound → Mix → Advanced**, not in the input-facing mixer.

Every input has a **Name** you choose (`label = "16R output"` in its `[inputs.<id>]` table).
The app shows that name everywhere: Sources, the Overview sound bar, routing, signals for
lights, and the PipeWire bus description (for example "Stream Engine: 16R output"). A bus is
named after the inputs routed into it. Without a name, an input shows its configured device;
a bus nothing feeds keeps its identifier. The app never substitutes invented names.
The mixer (Sources, the Overview sound bar and the mixer popout) has one control per capture
input and one per kind of app audio — Music, Sound effects, Read-out voice, Scene audio — which
sets that group's level. It never shows one channel per scene layer or producer; those stay
adjustable individually under **Advanced → What plays here**.
The starter project does not preconfigure a Studio 24c capture input.

Manage capture feeds from **Sound → Mix** with **Add input** and **Manage inputs**. Choose a
discovered device or enter its exact source target, select its channels and destination bus,
then choose **Save input**. Changes remain a draft until saved; a project reload does not
overwrite an open draft. Saving or removing an input updates the project and reloads the graph.
Discovery alone never adds an input. Removal is refused while another configuration entry
depends on it; the manager identifies those references rather than silently breaking them.

Camera and media-file scene sources are managed separately under **Inputs**. The recorder
has its own source selection in **Settings → Accounts & app → Recording**.

### Saved-input API

`project.audio.inputs` reports saved inputs with their effective settings, origin file/key
locations, dependency references, configured-bus intent and parse errors. It remains available
when the audio subsystem is stopped; `audio.mix` describes the running graph instead.

- `project.audio.input.save {name, target, channels, bus, create?, gain?, mute?, delay_ms?}`:
  use `create: true` for a new name; omit it when editing an existing input. Channels are
  one-based device channel numbers. Existing names stay fixed.
- `project.audio.input.remove {name}`: removes every merged definition of the input so an
  older fragment cannot bring it back after restart.

Edits preserve unrelated effects, comments and configuration. Success emits
`project.audio.inputs.changed {name}` after persistence and reload; failure emits
`project.audio.input.failed {name, action, error}`. A command transport acknowledgement alone
does not confirm persistence.


## PipeWire nodes

| Node | Kind | What |
|---|---|---|
| `se-engine` | filter (DSP ports) | the whole graph; runs in PipeWire's real-time data thread |
| `se-music`, `se-sfx`, `se-tts`, `se-program` (+ `se-band`, `se-game`, `se-drums`, `se-mic` only when used) | `Audio/Source/Virtual` | software buses; selectable feeds, not automatically recorded stems |

Check them with `pw-cli ls Node | grep -A2 se-` or `pactl list short sources | grep se-`.

**Streaming:** select the intended audio feed in OBS and avoid capturing the same mix twice.
`obs.setup` adds video sources only; it configures neither audio nor recording. Check the OBS
meter for the stream and a short app recording for the separately selected recording feeds.
The engine links its PipeWire graph itself (WirePlumber doesn't touch `se-engine`); links are
re-made on hotplug.

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

[inputs.band]              # selected capture source → bus
target = "Your input source" # node.name, or case-insensitive substring of node.name/description
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
                           #           game, game.* → game; patch.* → scene; timecode.* → none

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
beat_source = "auto"       # audible music first, otherwise most confident audible bus; or a bus name
min_bpm = 70
max_bpm = 180
mic = "vox"                # input analysed as the mic: mic.level, mic.hype, mic.talking
talk_threshold = -42.0     # dBFS: mic.talking = a voice (built-in voice detector) above this level
talk_hold = "600ms"

[sources."patch.drone"]    # `dsp` patch with layer = "audio-source" → bus
bus = "sfx"
gain = -6.0

[playback]                 # selected post-fader buses to your chosen output
target = "Your playback output"
channels = [1, 2]
buses = ["music", "sfx", "tts", "game"] # exclude any bus carrying this output's return

[monitor]                  # separate low-latency monitor, if needed
target = "Your monitor output"
channels = [1, 2]
buses = ["drums"]          # post-chain, pre-fader
inputs = ["kick", "snare"] # post input chain
gain = 0.0
fx = [ … ]

[direct."timecode.ltc"]    # a hub.audio slot straight to a device channel (never mixed)
target = "Your timecode output"
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
`hat` onset envelopes; `flux`, `novelty`), `beat.bpm`, `beat.phase`, `beat.position`,
`beat.confidence`, `beat.locked`, `beat.freewheel`, `beat.source`,
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
| `audio.tap` / `audio.tap.clear` | tap tempo (≥ 2 taps within 2 s) / clear all tempo overrides back to automatic detection |
| `audio.bpm {bpm}` / `audio.bpm.clear` | explicit finite 20–400 BPM / clear all tempo overrides back to automatic detection |
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

## Musical timing

Audio publishes one continuous musical clock, shared by the audio transport and lighting.
`beat.position` is the monotonic beat count; `beat.phase` is its fractional part (0–1),
and `beat.bpm` is the clock's tempo, not operator energy or vibe. The clock runs even
without audible input: it starts at 120 BPM, then retains the last trusted tempo through
silence, uncertain estimates, or a disconnected input instead of resetting to a default.
Automatic tempo converges with a two-second smoothing time; phase correction slews at
at most 20% of the beat rate, so reacquisition never jumps backwards.

With `analysis.beat_source = "auto"`, a fresh audible `music` bus owns the clock's
input, even if the drum kit is more confident. While the song tracker is acquiring,
the clock honestly freewheels; selecting the song does not manufacture confidence.
Without audible music, auto chooses a fresh audible non-mic bus by measured beat
confidence (other live buses require a 0.15 confidence advantage for two seconds to
replace it). A silent or stale bus yields immediately, including during acquisition;
with no audible bus, auto has no selected bus. An explicit bus name disables this
source preference, not the confidence or freshness requirements.

Stereo tap timing uses audio frames, not interleaved sample counts. This keeps
observations aligned to the master clock during long playback. The beat tracker
needs at least two seconds of onset history before its first estimate; regular
rhythms can acquire after a few seconds, but ambiguous or non-percussive material
may remain freewheeling. Track tempo and confidence in `analysis.music` are the
actual detector measurements, distinct from the slewed global clock.

Automatic lock requires confidence ≥ 0.6, remains locked down to 0.35, and expires
250 ms after the last audible analysis observation. Silent, stopped, or rebuilt sources
relinquish confidence; they cannot leave a stale lock. `beat.confidence` is effective
confidence (0 when freewheeling), `beat.locked` and `beat.freewheel` are complementary
0/1 signals, and `beat.source` is 0 = fallback, 1 = live audio, 2 = explicit BPM,
3 = tap. `audio.mix` → `analysis.beat` exposes readable `source`, `status`, selected
`bus`, `age_ms` (age of that bus's latest analysed audio, or null), tempo, position,
phase, and confidence. The global `beat` events follow this same clock, including
freewheel; `downbeat` marks a four-beat clock boundary, not a verified musical meter.

An explicit `audio.bpm` overrides detection immediately without resetting beat position.
Two valid taps supersede either explicit BPM or automatic detection; a single tap does
not replace the current tempo. Explicit BPM can in turn replace a tapped tempo.
Either clear action releases every tempo override; fresh live audio must reacquire
lock, and the released tempo freewheels until then. Manual/tap overrides remain usable
without audio input and report confidence 1. Lighting follows `beat.position` when
available and uses the same allocation-free clock implementation to freewheel if
publications stop; repeated reads never reset phase.

## Offline analysis (library songs)

`analysis.grid {path: "songs/x.flac"}` decodes and analyses a file (beat grid, downbeats,
sections with labels, chorus candidates, waveform peaks per 10 ms), caches the result in the
runtime DB by content hash, and returns
`{media: "file:<hash>", duration, bpm, beats, downbeats, sections: [{start, end, label}], chorus, peaks}`
(seconds). `analysis.grid {media: "file:<hash>"}` returns the cached result or null. Timelines use
it for beat-snapped cues and suggested chorus cues. These are **offline editing grids**,
not live clock locks: the current local sampler does not publish synchronized persistent
file identity/playback position, and the song player reports YouTube playback rather than
local-file playback. Live audio remains the universal automatic timing source; merely
loading a cached grid or pausing/replacing a timeline cannot confer musical clock lock.

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

* `health.audio.input.band` fails: check the configured source and input profile with
  `pactl list short sources`, then correct the input target.
* No sound in OBS: check its selected streaming source and meter. For silent app recordings,
  check **Settings → Accounts & app → Recording**, `recording.status` and `health.recording` instead.
* `health.audio.rt` fails with `RLIMIT_RTPRIO=0`: log out and in again (see above).
* Xruns at 64: raise `quantum` to 128/256 for the stream path; keep 64 only with the monitor.
