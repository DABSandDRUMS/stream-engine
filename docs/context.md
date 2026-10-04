# Context and operator control

Automatic effects and lights should come in when the moment calls for it, and the operator
must be able to stop them at once. Two parts, both inside the core (`crates/se-core/src/context.rs`):

- **Operator switches:** `fx.enabled` (overall effects kill switch), `fx.auto` (musical
  video director, default off) and `lights.auto` (lighting director).
  Only the operator can flip them; all three are kept across restarts.
- **Context layer:** signals that describe the room (`context.song`, `context.drums`,
  `context.chat`, `context.energy`, `context.talking`, `context.budget`), the song's mood
  (`context.mood`), occasional moment events, and beat/song-aware lighting and musical FX
  directors. Both select authored content; neither generates fixture output.

## Operator switches

| Action | Effect |
|---|---|
| `fx.off` / `fx.on` / `fx.toggle` | state `fx.enabled` (default `true`); emits `fx.changed {enabled}` when it changes |
| `fx.auto.off` / `fx.auto.on` / `fx.auto.toggle` | state `fx.auto` (default `false`); emits `fx.auto.changed {enabled}` when it changes |
| `lights.auto.off` / `lights.auto.on` / `lights.auto.toggle` | state `lights.auto` (default `true`); emits `lights.auto.changed {enabled}` when it changes |

**Who is the operator:** commands from `ui cli api deck midi voice osc mixer binding` (at
their normal manual priority), commands carrying a local actor or the channel owner, scene
`on_enter`/`on_exit` lists, and rules reacting to an event the operator caused (an `emit`
from the UI or deck, an owner's chat command). Viewers (`chat twitch relay`), patches,
timelines, the system and rules on automatic events (song analysis, viewer events, timers)
are not. Anyone else gets `only the operator can switch effects on or off`,
`only the operator can switch automatic effects` or `only the operator can switch light automation`.
All three states are read-only. Use their actions; operator `set fx.auto true|false` and
`set lights.auto true|false` (including generic deck toggles) route through the same guarded
switch actions and persistence, never a temporary override. Switch actions discard any
overrides masking their durable base. Repeating explicit off safely cancels pending automatic
video starts or releases remaining automation-owned lighting layers, respectively.

**`fx.off`** at once:

- releases every running preset that has `fx`, `pick` or `lane` (its `on_release` runs);
- releases every running `patch.*` and `fx.*` trigger envelope;
- drops lane queues, quantized starts and `conflict = "queue"` firings of those presets.

**While `fx.enabled` is false**, from every non-operator origin, `patch.<id>.trigger`,
`fx.<name>.trigger` and `preset.fire` of a preset with `fx`, `pick` or `lane` fail with
`effects are off`. A reward whose commands fail this way is refunded. Presets without effects
(lights-only, `set`-only) are not affected. The operator can still fire anything.

`[fx] exempt` keeps chosen patches working (alerts, title cards):

```toml
# project.toml
[fx]
exempt = ["win31_alerts", "win31_alerts_tall"]   # patch ids, or full trigger addresses ("fx.grade")
```

Exempt triggers keep firing and are not released by `fx.off`. A preset whose only effects are
exempt (and that has no `pick`/`lane`) counts as a normal preset.

**`fx.auto.off`** stops new musical selections, cancels pending starts whose original event
is exactly `context.musical_fx`, and fades out the director's current instance over three
seconds. Ownership is per instance: even a manually fired copy of the **same preset** stays
untouched. Viewer effects, notifications, song-start/end rituals, unrelated `context.*`
rules, lighting jobs and `context.spend` remain independent. `fx.enabled` still applies
separately; `fx.off` retains the existing overall effects kill behavior.

The switch survives restarts, but director-owned instances do not: restart never restores
an orphan musical look. Speech, song silence, or a configuration rebuild also fades out
the owned instance. Analysis and context event detection continue while AUTO FX is off.

## Musical AUTO FX

The musical director chooses **opted-in show presets**, through the existing preset and
trigger machinery. It requires actual song audio: chat and drums can influence a choice
or its conservative intensity, but cannot independently start musical FX. Loudness is
relative to the current song, not a fixed “loud song” threshold. Mood and relative energy
filter the library; weighted selection avoids the last two looks while alternatives exist.

Selections are intermittent: at least 12 seconds of song warmup, one finite look at a time,
then at least 45 seconds of clean camera after its release tail. Trusted beat grids align
opportunities to phrases; seeks rebase timing, and a stalled or unavailable grid falls back
to the timed opportunity rather than preventing all future selections. Speech suppresses
new looks and releases the current one. The default maximum is 24 selections per rolling
hour, independent of `context.auto_per_hour` and the older `context.spend` budget.

```toml
# project.toml — optional; defaults shown
[context.musical_fx]
start_delay = "12s"       # 12–120 seconds
quiet_gap = "45s"         # 45–600 seconds, after the release tail
phrase_beats = 32         # 16–128, a multiple of four
max_per_hour = 24         # 1–48
```

Library entry:

```toml
# presets/auto_fx_atmosphere.toml
label = "AUTO · Atmosphere"
chat = false
hold = "24s"
auto_fx = { moods = [], energy = [0.0, 1.0], weight = 2.0 }
fx = [{ name = "patch.musical_atmosphere", attack = "2s", release = "3s" }]
```

`auto_fx.moods` accepts `none`, `chill`, `groove`, `bright`, `heavy`, `hype`; empty means any.
`energy` is an inclusive finite range within 0–1; `weight` is positive and at most 1000.
An entry must be video-FX-only, with a positive authored hold no longer than 30 seconds.
Any per-effect hold must be positive and no longer than the preset hold. Lights, sound,
mix, set, commands, release actions, scene/mode changes, roulettes and latching are not
eligible. Manually active candidates are skipped, not commandeered. Director envelopes
use a two-second attack, three-second release and conservative 0.18–0.4 plateau.

Attach the corresponding effect in the normal routing system:

```toml
# sources/cam_kit.toml — alongside the source's existing settings
fx = [{ id = "musical_atmosphere", name = "patch.musical_atmosphere", triggered = true }]
```

The drum project installs atmosphere, bloom, highlight sheen and edge leaks on its six
camera sources, before compositing window chrome, titles, YouTube and notifications.
Each patch reads the active lighting scheme through manifest palette aliases
([patches.md](patches.md)); missing/transparent colors fall back to the stream palette.
AUTO LIGHTS and AUTO FX remain independent. Existing notification/viewer libraries are
not part of the musical pool; the older automatic peak/fill rules remain disabled.

The project shaders are calibrated for the director's 0.18–0.4 envelope, not a manual
full-strength preview. Bloom's default highlight threshold is 0.28, sheen's 0.24, and
leaks reach 0.30 of the source edge, so dim drum-camera highlights and zoomed shots can
show the look without raising the whole picture's black floor. Atmosphere retains skin
and luminance protection. Shader/manifest saves hot-reload without restarting the engine.
To diagnose “nothing happening,” inspect `context.fx.reason`, the selected patch's `.env`
and `.error`, and the camera source's `.fx_enabled` / slot `.enabled`. `quiet` deliberately
shows clean camera; `active` with a positive envelope requires checking actual dark camera
output, not only a bright-room or full-envelope preview.

Read-only diagnostics:

| Address | Meaning |
|---|---|
| `context.fx.preset` | Owned preset, including its release tail; empty when none |
| `context.fx.reason` | `active`, `quiet`, `off`, `no-song`, `speech`, `no-match`, or `error` |
| `context.fx.next_at` | Earliest/scheduled opportunity, core-clock nanoseconds; zero while suppressed/active |
| `health.context.fx` | Actionable invalid-library or firing failure detail; empty when healthy |

`context.musical_fx` is a system event with `preset`, `mood`, `energy`, `level`,
`attack` (2000 ms), and `release` (3000 ms). Preset/trigger traces reference that event.
Fix an invalid opted-in preset on disk; ordinary hot reload rebuilds the candidate cache.
`no-match` means the current song has no eligible library entry, not a renderer failure.


## Lighting switch

**`lights.auto.off`** releases the lighting layers automation owns, by sending
`lights.layer.release {layer, owner: "context"}` for `base`, `rhythm` and `accent`. Rules that
drive lights should use owner `context` and check the switch:

```toml
[[rule]]
when = "context.fill_landed"
if = "lights.auto && context.talking < 0.5"
do = ["lights.layer.pick layer=accent tags=[\"accent\",\"fill\"] kind=cuelist duration=2s owner=context"]
```

Twitch rewards marked `fx = true` are paused on Twitch while effects are off
([twitch.md](twitch.md)).

## Signals

All 0–1 (except `context.budget`), smoothed, published every core tick without allocating.

| Signal | Meaning |
|---|---|
| `context.song` | Song energy against **this song's own** running baseline (~45 s): 0.5 is typical for the song, louder/busier/more changing sections push toward 1. Mixes loudness (`music.lufs`, else `music.level`; 6 dB above the baseline = full), spectral flux and onset density (`music.kick/snare/hat` envelopes). 0 when no song audio. |
| `context.drums` | How hard the drummer plays against the session's playing baseline (~5 min, updated only while playing): `band.level` (60 %) and onset density (40 %). 0.5 = the session norm; 0 when the band is silent (below −45 dBFS). |
| `context.chat` | Chat heat: `twitch.chat_rate` against a ~10 minute baseline (never below 3 msgs/min); 3× the baseline is full. Plus bumps that decay over ~45 s: cheers (0.1 + bits/2500, up to 0.5), subs/resubs (0.2), gifts (0.15 per gift, up to 0.6), raids (0.3 + viewers/200, up to 0.7), hype train begin/progress (0.25). |
| `context.energy` | Overall show energy: 70 % music (the mean of song and drums when both play, else whichever plays) + 30 % chat, + 0.1 + 0.03 × level (up to 0.25) during a hype train (`twitch.hype_train`). ~1 s smoothing. |
| `context.talking` | 1 while the host talks between songs: mic speech (`mic.talking`, else `mic.level` > 0.03) while no song plays and the band is below −40 dBFS. On after 0.5 s, off 2.5 s after the condition ends. |
| `context.budget` | Automatic moments left in the rolling hour: `[context] auto_per_hour` minus the `context.spend` calls of the last 60 minutes. |

A song counts as playing above −45 LUFS (1 s debounce). It keeps its baseline through breaks
shorter than 10 s; a longer gap or `queue.song_started` starts a new baseline.

**`context.spend`** takes one moment from the budget (rules call it when they fire an automatic
moment and gate with `if = "context.budget >= 1"`). With nothing left it fails with
`no automatic moments left this hour`. The budget starts full after a restart.

## Mood

State `context.mood`: `none` (no song yet) | `chill` | `groove` | `bright` | `heavy` | `hype`.
The first verdict comes 8 s into a song; after that it is re-evaluated at most every
`mood_every` (20 s) while a song plays and held otherwise. A change emits
`context.mood {mood, previous}`.

**Genres first.** When `song.current.genres` (lowercase list, from song metadata) has a known
genre, it decides. Keywords match anywhere in a genre name, most specific first; the first
genre in the list with a match wins:

| Mood | Genre keywords |
|---|---|
| `hype` | pop punk, punk, ska, drum and bass |
| `heavy` | metal, hardcore, djent, grunge, hard rock, stoner |
| `chill` | lo-fi, lofi, ambient, jazz, acoustic, folk, singer-songwriter |
| `groove` | funk, disco, hip hop, hip-hop, rap, r&b, rnb, soul, reggae |
| `bright` | pop, edm, dance, house, electro, synth |

(`pop punk` → hype before `pop` → bright; `metalcore` → heavy.) A near-silent passage
(sustained energy below 0.2: a ballad, a breakdown) reads `chill` whatever the genre.

**Audio otherwise**, from ~8 s averages of the song bus: sustained energy (onset envelope
density, full at a mean of 0.3, and flux, full at 1.5), tempo (`beat.bpm` when
`beat.confidence` ≥ 0.3), spectral centroid and the bass share of bass+mid+high:

1. energy < 0.3 → `chill`
2. tempo ≥ 150 and energy ≥ 0.6 → `hype`
3. bass share ≥ 0.55 and energy ≥ 0.55 → `heavy`
4. tempo < 90 and energy < 0.45 → `chill`
5. centroid ≥ 0.62 → `bright`
6. otherwise `groove`

## Moment events

| Event | When |
|---|---|
| `context.peak {energy, mood}` | `context.energy` ≥ `peak_on` (0.72) for `peak_hold` (6 s). One per crossing (energy must fall below `peak_off` before the next) and at most one per `peak_cooldown` (2 min). |
| `context.settle {mood}` | After a `context.peak`: energy below `peak_off` (0.5) for `settle_hold` (3 s). |
| `context.song_peak {strength, mood}` | The song hits a big section: a `music.drop`, or a `music.section` with novelty ≥ `section_novelty` (0.35), followed by `context.song` ≥ `song_peak_on` (0.7) for `song_peak_hold` (4 s), the run starting within `song_peak_window` (10 s). At most one per `song_peak_cooldown` (45 s). Never while the host talks: a drop arriving while `context.talking` is 1 is ignored. `strength` = mean of the drop strength (or section novelty) and `context.song`. |
| `context.fill_landed {strength}` | A drum fill: ≥ `fill_hits` (5) `band.snare`/`band.tom*` hits (velocity ≥ 0.15) within `fill_window` (1.2 s), then a `band.kick` with velocity ≥ `fill_land_velocity` (0.4), or a `band.crash`/`band.cymbal`, within `fill_land` (0.6 s) of the last hit, within `fill_downbeat` (0.5) beats of a bar start (`beat.position` on a 4/4 grid; any time when `beat.confidence` < 0.3 or `fill_downbeat = 0`). At most one per `fill_cooldown` (20 s). `strength` = mean of the landing velocity and the hit count (8 = full). |
| `context.mood {mood, previous}` | The mood changed (above). |

## AUTO LIGHTS director

The project's `rules/context.toml` keeps three independent jobs:

| Layer | Authored library | Selection |
|---|---|---|
| `base` | `scheme` cue lists writing all six `lx.color.a`…`f` slots | Mood and palette character, independent of motion |
| `rhythm` | `motion` cue lists using live color-slot references | Relative song/drum energy, source and section |
| `accent` | Finite `accent` cue lists | Drops, landed fills, raids and hype trains |

Turning automation on initializes the current room context immediately. A song start selects a
fresh motion; the first song also initializes its scheme. Audio has a ten-second startup grace
after `queue.song_started`, so buffering does not immediately return the room to idle. The first
mood verdict still arrives after eight seconds. A changed mood selects a new scheme, not a
replacement motion. Heavy music mostly uses `solid`, with occasional `rich`; chill mostly uses
`rich`, with occasional `solid`; groove/bright alternate `duo` and `rich`; hype uses `rich` twice
as often as `rainbow`. Required mood/character tags exclude special-only schemes from regular
selection.

Motion energy is `low` below 0.35, `mid` from 0.35 to below 0.65, and `high` at 0.65 or above.
Ordinary band changes must hold for three seconds to avoid jitter. `music.section` with novelty
at least `section_novelty` immediately reselects; low sections use `breakdown`, and rising high
energy uses `build`. Drops immediately choose high motion and a finite drop accent; only a drop
or a context peak at song energy at least 0.85 permits `peak`/`strobe`, for at most four seconds
before normal energy selection resumes. Palette selection is not affected by a drop.
Motion and scheme selections normally quantize to four beats; build motions enter on a
sixteen-beat phrase boundary so authored rate changes remain phase-continuous. At 120BPM a build
can wait up to eight seconds, while drops still select their accent on the next beat.


The director alternates motion deadlines at 64 and 128 beats (16/32 four-beat bars) on published
`beat.position`. A clock rewind rebases the deadline without firing catch-up selections.
Palette deadlines alternate every three and two song starts, unless a mood/source change needs
a new palette sooner. Motion picks avoid the last six selections; schemes avoid the last four.
Core picks use the normal candidate-exhaustion behavior when a pool is smaller than that history.
Brightness and effect depth follow smoothed energy with an 800ms fade, updated at most once per
two seconds after a meaningful energy change; this does not restart the selected motion.

Without song audio, live drums require `drums`-source motions. Between-song host speech chooses
a low ambient motion, reduced brightness/depth and doubled beat periods. When both song and
drums are quiet, the director waits two seconds and releases only owner `context` on all three
layers. Rig defaults supply the idle pink overheads, and the committed-output main-light handoff
restores Main **ON**. It never writes global light controls or directly toggles a plug.

`twitch.raid` starts a 20-second celebration; `twitch.hype_train.begin` starts a 32-second
celebration (progress events do not extend it). A train end restores normal context immediately.
Celebrations select special-only schemes and an eight-second accent; high motion continues the
show even without a song. Host speech still forces calm motion and suppresses celebration hits.
On expiry, the director selects a normal scheme for the current mood and resumes current source/
energy, or releases to idle. This is a fresh normal scheme, not a rewind of the previous color.
Disabling auto cancels director deadlines and pending moments; re-enabling does not replay an
old raid, drop or train.

| Director event | Meaning |
|---|---|
| `context.lights_palette` | Select normal scheme by `mood` and `character` |
| `context.lights_motion` | Select motion by `energy`, `family`, required `source` and preferred `prefer` tags |
| `context.lights_controls` | Update current rhythm `brightness`, `depth` (layer energy) and `rhythm` without restarting |
| `context.lights_idle` | Release automation-owned base, rhythm and accent |
| `context.lights_special` | Select special scheme/accent tagged `special` (`raid` or `hypetrain`) |
| `context.lights_drop` | Fire a finite drop accent |

All six carry the same payload: `mood`, `character`, `energy`, `family`, `source`, `prefer`,
`brightness`, `depth`, `rhythm`, `special`. `source = "drums"` is required for drums-only playback;
otherwise it is the neutral `motion` tag so beat-clock and track-reactive content can alternate.
All lighting rules check `lights.auto` and use `owner=context`; auto-off's existing guarded release
cannot remove manual or viewer replacements. No director event spends the video-effects budget.


## Configuration

```toml
# project.toml — every key optional; defaults shown
[context]
auto_per_hour = 6
peak_on = 0.72
peak_off = 0.5
peak_hold = "6s"
settle_hold = "3s"
peak_cooldown = "2m"
song_peak_on = 0.7
song_peak_hold = "4s"
song_peak_window = "10s"
song_peak_cooldown = "45s"
section_novelty = 0.35
fill_hits = 5
fill_window = "1200ms"
fill_land = "600ms"
fill_land_velocity = 0.4
fill_downbeat = 0.5
fill_cooldown = "20s"
mood_every = "20s"
```

Unknown keys or bad values are project errors (the defaults apply). The constants that only
shape the 0–1 scales (floors, baselines, smoothing) are documented at the top of
`context.rs`.
