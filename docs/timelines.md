# Timelines and timecode

Timelines sequence anything — lights, scenes, effects, audio, mixer, patches — against a
clock (PLAN §2.7). A timeline is one file in `timelines/<name>.toml`. It hot-reloads, shows up
in the Timeline view, and runs inside the deterministic core, so session replays reproduce it
exactly.

## File format

```toml
# timelines/nightly_song.toml
media = "yt:VIDEO_ID"            # or "file:<hash>" | "isrc:<code>"; or: source = "internal" | "manual" | "mtc[:port]" | "ltc[:tap]" | "media:<id>"
fps = "30"                       # 24 | 25 | 29.97df | 30 — timecode labels (MTC/LTC report their own)
priority = 200                   # override priority of this timeline's sets/automation (1–299)
length = "4:05"                  # internal/manual: stop (or loop) here; also the UI extent
loop = false                     # internal: start over at `length`
enabled = true                   # chased/manual sources follow from load; `timeline.stop` disarms
release_on_stop = false          # external sources: release the timeline's state when the source stops/is lost
snap = "beat"                    # off | beat | bar — recorded cues and snapped edits land on the beat grid
analysis = "assets/song.wav"     # audio file whose offline analysis gives the beat grid (optional)
record_track = "recorded"        # where record mode writes cues

[chase]                          # external sources only
jitter = "80ms"                  # lock tolerance (yt: 80 ms, file:/isrc: 20 ms; MTC/LTC 1 frame)
jump = "500ms"                   # beyond this a discontinuity is a locate (tracked state is applied)
freewheel = "1s"                 # keep running this long when the source goes quiet (MTC/LTC default 2 s)

[timecode_out]                   # generate timecode from this timeline, whatever its source
mtc = "studio24c"               # MIDI port (controllers/*.toml id, else ALSA-name glob)
ltc = true                       # audio slot `timecode.ltc` (one timeline at a time)

cues = [                         # shorthand for a cue track named "cues"
  { at = "1:12.400", do = ["preset.fire chorus_blast"], label = "chorus" },
]

[[track]]                        # cue track
name = "lights"
cues = [
  { at = "0:30.000", do = ["lights.cue cuelist=main cue=2"] },
  { at = "0:31.000", do = ["emit show.flash"], track = false },
]

[[track]]                        # automation lane
name = "front"
type = "automation"
address = "lights.group.front.master"   # any address or pattern
curve = "linear"                 # default segment shape
keys = [
  { at = "0:00.000", value = 0.0 },
  { at = "0:10.000", value = 1.0, curve = "smoothstep" },   # shapes the segment to the next key
  { at = "0:20.000", value = 0.5 },
]

[[track]]                        # region track: start/stop something for a span
name = "looks"
regions = [
  { start = "1:12.400", end = "1:43.000", preset = "chorus_blast" },
  { start = "2:00", end = "2:30", cuelist = "chase_fast" },            # optional `cue`
  { start = "2:30", end = "2:34", fx = "rgb_split" },                  # held for the whole span
  { start = "3:00", end = "3:30", do = ["set fx.vhs.amount 1"], undo = ["release fx.vhs.amount"] },
]
```

**Times** are `"m:ss.mmm"`, `"h:mm:ss.mmm"`, `"72.4s"`, a timecode label `"01:00:10:12"` (at
`fps`; drop-frame labels that don't exist are rejected), or a number of milliseconds. They are
always the *source's* time: song time for media, the timecode value for MTC/LTC.

**Commands** in `do` use the same one-line syntax as rules and presets (`wait 500ms` delays
the rest of the list). **Curves** are `linear`, `step`, `smoothstep`, `in_quad`, `out_quad`,
`in_out_quad`, `in_cubic`, `out_cubic`, `in_out_cubic`, `out_back`. Numbers and numeric lists
(colors) interpolate; other values step.

A broken file keeps its last good version running; the error shows in the UI console and the
`errors` query.

## Sources

| Source | Follows | Notes |
|---|---|---|
| `internal` | show clock | `timeline.play|pause|toggle|stop|locate|jog` from UI, deck, rules, CLI |
| `manual` | jog/locate | optional `scrub = "<signal>"` (0–1 × `scrub_range`, default `length`) for a fader/encoder |
| `media:yt:<id>` (`media = "yt:<id>"`) | `song.position` | active only while `song.media` is this id; holds while `song.state` isn't `playing`; a seek is a jump |
| `media:file:<hash>` | a media source playing that file | exact: the time of every shown frame; active while it plays, holds on pause/end |
| `media:isrc:<code>` | a media source whose file is tagged with that ISRC | as `file:`; `US-RC1-76-07839` and `USRC17607839` are the same code |
| `mtc` / `mtc:<port>` | MIDI Time Code | quarter + full-frame messages; port from `[timecode] mtc_in` |
| `ltc` / `ltc:<tap>` | SMPTE LTC audio | decoded from an input tap; tap from `[timecode] ltc_in` |

External sources are **chased**: observations are fitted to a smooth clock (position and speed),
jitter inside `jitter` is absorbed without steps, larger corrections re-sync, the timeline
freewheels through dropouts for `freewheel`, then holds (`lost`). Status: `timeline.<n>.status`
= `idle | disabled | stopped | paused | playing | waiting | locking | locked | freewheel | lost`.

### Media: who reports the song

* **The song player** (song requests, YouTube) is the only producer of `song.media`,
  `song.position` and `song.state`. Its reported time is smoothed and interpolated; `yt:`
  timelines follow it.
* **Local media** (media-file sources, `sources/<n>.toml` with `file = …`) publishes
  `source.<n>.media` = `file:<hash>` — the first 16 hex digits of the BLAKE3 hash of the file,
  the same key the offline analysis uses, so a `file:` timeline gets the file's beat grid — and
  `source.<n>.isrc` from the file's `ISRC`/`TSRC` tag. `file:`/`isrc:` timelines follow that
  playback with the exact position of each shown frame (pause, seek, loop, rate and end
  included; chase defaults: 20 ms jitter, 250 ms jump). If two sources play the same file at
  once, the one that started first drives the timeline until it stops.
* To find a file's id: `streamctl get source.<n>.media` while it plays.

### Jumps and tracked state

When the position jumps (a locate, a seek, a restart mid-song, `timeline.locate`), the timeline
does **not** fire the cues it skipped. It applies the **tracked state** at the new time:

* for every *stateful* target the latest command before the new time is applied instantly —
  `set`/`animate` (as a set), `lights.cue|goto` per cue list (as `lights.locate`: tracked, fade 0,
  no follow timers), latching presets (no `hold`), `scene.cut`, `scene.go`, `mode.set`,
  `mixer.snapshot.recall`;
* targets the timeline touched that no earlier cue sets are reverted (overrides released, cue
  lists released, latching presets released);
* regions are entered/left to match the new time; automation lanes jump to their value;
* momentary commands (`preset.fire` with a hold, triggers, `emit`, `bot.say`, sounds, …) are
  skipped. `track = true` on a cue makes all its commands re-apply on jumps; `track = false`
  makes a cue purely momentary.

A cue exactly at the new time fires normally. Small forward corrections fire the crossed cues
(late); small backward corrections never re-fire cues.

### Priority

Automation and cue `set`s are overrides with key `timeline:<name>` at the timeline's
`priority` (default 200 = presets). Several timelines, presets, chat, and manual moves resolve
through the normal resolver: manual (300) wins over a timeline, chat (100) loses.

## Timecode in and out

```toml
# project.toml
[timecode]
mtc_in = "studio24c"     # MIDI input for `source = "mtc"` (controllers/*.toml id, else ALSA name glob)
ltc_in = "input.ltc.0"    # audio tap for `source = "ltc"`: channel 0 of [audio.inputs.ltc]
ltc_level = -12.0         # LTC out level (dBFS)
ltc_latency = "0ms"       # output latency compensation for LTC out
mtc_latency = "0ms"       # receiver latency compensation for MTC out
```

* **MTC in/out** use the Inputs slice's MIDI ports (`se_input::midi`). Out sends a full-frame
  locate on start/jumps/stop and quarter frames while running (groups start on even frames).
* **LTC in** needs an audio input channel carrying LTC: add it in `[audio.inputs.ltc]` (see
  docs for the Audio slice), e.g. `target = "Studio 24c" channels = [2]` → tap `input.ltc.0`.
  The decoder handles ±10 % varispeed, reverse play, inverted polarity, and all four rates
  (rate detected from the drop-frame bit and frame-number wraps).
* **LTC out** writes the `timecode.ltc` audio slot; se-audio never mixes `timecode.*` slots
  into buses. Route it to a hardware output channel:
  `[audio.direct."timecode.ltc"] target = "Studio 24c" channel = 2`.
* Health: `health.timecode` (preflight) reports each input/output.

To check LTC externally, `ltcdump` from libltc/ltc-tools can be built without root into
`~/.local/share/stream-engine/tools/ltc/` (`ltcdump -f 30 file.wav`, `-f 30000/1001` for
29.97 DF). `crates/se-clock/tests/ltc_wav.rs` cross-checks our encoder against it when present.

## Transport, record, edit

| Action | Args |
|---|---|
| `timeline.play|pause|toggle|stop <name>` | chased sources: play = follow, stop = release and ignore the source |
| `timeline.locate <name> time=<t>` / `timeline.jog <name> delta=<s>` | internal/manual only |
| `timeline.record <name> [on=true|false] [arm=[patterns]]` | toggle record mode; arm defaults to the lanes' addresses |
| `timeline.tap [<name>] [label=…] [do=[…]]` | add a marker cue while recording |
| `timeline.create {name, source?, fps?, length?}` | new file |
| `timeline.edit.cue|key|region.add|set|remove {timeline, track, index?, …, snap?}` | editor writes (indices = time order) |
| `timeline.edit.track.add|remove|mute {timeline, name, type?, address?, mute?}` | |

**Record mode:** while the timeline runs, every manual command (UI, deck, MIDI, voice, OSC,
CLI) becomes a cue at the current time, `timeline.tap` adds marker cues, and changes of armed
addresses become keyframes (thinned). Stopping record writes the file (`record_track` for cues;
keys replace the lane's keys inside the recorded span), snapped to the beat grid when `snap`
is set. The writes keep comments and formatting.

**Beat grid:** `timeline.grid {name}` returns the offline analysis (`bpm`, `beats`,
`downbeats`, `sections`, `chorus`, `peaks`) for a `file:` media timeline or the timeline's
`analysis` file (analyzed once and cached by the Audio slice). YouTube songs can't be analyzed
offline (§8.4).

## State, events, queries

* State: `timeline.<n>.time` (s), `.playing`, `.active`, `.locked`, `.recording`, `.status`,
  `.source`, `.timecode`, `.length`.
* Events: `timeline.cue {timeline, track, cue, at, label}`, `timeline.jump {from, to}`,
  `timeline.started|stopped|ended|looped`, `timeline.region.on|off`, `timeline.record.started`,
  `timeline.recorded {cues, keys}`. Rules can hang anything off them, e.g.
  `when = "timeline.cue"`, `if = "event.label == 'chorus'"`.
* Queries: `timelines`, `timeline {name}` (definitions + runtime), `timeline.grid {name}`,
  `timeline.ports`.

## Replay

MTC/LTC positions enter the core as recorded `timecode` inputs (≈15 per second while locked,
every discontinuity immediately); so do local media positions (`media:file:<hash>`,
`media:isrc:<code>`). `song.position` and scrub signals aren't part of the session log, so the
core records each position it consumes as a `timecode` input itself; player state (`song.media`,
`song.state`) is replayed as state. `stream-engine replay` therefore reproduces timeline
positions, fired cues, and automation exactly.
