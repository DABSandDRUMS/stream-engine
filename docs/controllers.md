# Controllers: Stream Deck, MIDI, voice

`se-input` runs the physical controls (PLAN §10): the Stream Deck, MIDI surfaces (X-TOUCH MINI,
FBV Express, anything class-compliant, MCU/HUI surfaces), the Studio 24c DIN port (e-drums,
MTC for Timelines), and voice push-to-talk. Everything is configured in `controllers/*.toml`
(hot-reloaded; a broken file keeps its last good version) and shown in **Views → Controllers**.

The same preset can be fired from the deck, a MIDI button, a footswitch, voice, a keybind
(`streamctl do "preset.fire hype"`), and chat (a rule) — they all end up as the same core command, with
the controller's origin (`deck`, `midi`, `voice`) at manual priority in the trace.

## Stream Deck (`kind = "deck"`)

Supported: Stream Deck Original V2 (`0fd9:006d`, the unit on this machine), MK.2, 15-key module,
XL/XL V2 (same HID protocol). Access comes from `packaging/70-stream-engine.rules` (`uaccess`
tag for every `0fd9` hidraw node); on this machine `/dev/hidraw8` already had a user ACL.

```toml
# controllers/deck.toml
kind = "deck"
brightness = 70                        # live address controllers.deck.brightness (0–100)
pages = ["show", "mix", "fx", "songs"] # order for page next/prev
# serial = "AL46J2C62768"              # bind this file to one unit (several decks)
# start_page = "show"

[page.show.key.0]                      # key 0 = top left, 5 = middle row, 10 = bottom row
scene = "duo"
[page.show.key.5]
preset = "hype"
[page.show.key.9]
do = ["scene.take"]
color = "accent"
[page.show.key.12]
ptt = true                             # hold to talk
[page.show.key.14]
do = ["panic"]
hold = "1s"                            # hold-to-fire with a progress bar
```

A key does one of `preset`, `scene` (+ `cut = true`), `toggle = "<address>"`,
`momentary = "<address>"`, `do = [...]` (+ `release = [...]`, `wait 1s` allowed), `page`
(`next`/`prev`/name), `ptt = true`. Options: `label`, `icon` (a name such as `preset`, `scene`,
`mic`, `fire`, `drum`, `marker`, `clean`, `panic`, … or any Nerd Font glyph), `color` (`#rrggbb`
or a theme token: `red`, `bright_red`, `yellow`, `green`, `cyan`, `blue`, `magenta`, `orange`,
`accent`, `muted`, …), `state = "<address>"` (what lights the key), `hold`, `confirm = true`
(press twice within 3 s; presets marked `confirm` in their file need it automatically),
`cooldown = "2s"`.

Display options: `image = "assets/deck/name.png"` (project-relative 72×72 RGB PNG used as the
whole key face; its label/icon replace the built-in text), `clock = true` (display-only local
`HH:MM:SS`; no action allowed), and `disabled = true` (keeps the face visible but ignores
presses, holds, confirms and releases). Images load once when the file is (re)loaded.

Key images are rendered with the Omarchy theme colors and font (`omarchy font current`, a
Nerd Font) and re-rendered on `omarchy theme set` / font changes. They follow state: presets
light up while active with a countdown bar for `hold`, scene keys show program (red edge) and
preview (yellow edge), toggles show on/off, `ptt` shows LISTEN / … / CONFIRM?. Only changed
keys are sent to the device. Unplugging and replugging the deck reconnects within ~1 s.

| | |
|---|---|
| State | `controllers.<deck>.{connected, page, serial, model, firmware, brightness}`, `controllers.page` (primary deck; the UI pad grid mirrors it) |
| Events | `deck.key {deck, page, key, down}`, `deck.connected`, `deck.disconnected` |
| Actions | `deck.page <name\|next\|prev>`, `deck.press {key, page?}` / `deck.release` (run a key as if pressed — the UI pads use this), `deck.brightness <0–100>`, `deck.assign {page, key, preset\|scene (+cut)\|toggle\|momentary\|do (+release)\|target_page\|ptt, label?, icon?, color?, hold?, confirm?}` / `{…, clear = true}` (edits `controllers/deck.toml`, comments kept; `page` selects the page being edited, `target_page` is a navigation key's destination; `do`/`release` are command lists run on press and on key-up), `deck.refresh` |
| Queries | `controllers.page {page?}`, `controllers.pages`, `controllers.deck` (status + every page), `controllers.deck.preview {since?}` (key images as base64 JPEG) |
| Health | `health.deck` |

### Drum project's streaming deck

`controllers/deck.toml` recreates the earlier physical Win 3.1 deck. Main page, left to right:

| Row | Column 1 | Column 2 | Column 3 | Column 4 | Column 5 |
|---|---|---|---|---|---|
| Top (keys 0–4) | Starting | Drum chat | Main | Ambient | Effects |
| Middle (keys 5–9) | Pink | Cyan | Storm | Round Room | Default Lights |
| Bottom (keys 10–14) | Play | Pause | Next | Strobe | Cameras |

**Default Lights** (main page key 9, middle-right) runs `lights.default`: stops lighting
looks and pauses AUTO LIGHTS, restoring the configured idle room. This project's idle
requests Main smart plug ON and pink (`#ff69b4`) mirrored Mega Pixel LED bars at 30%, with other
fixtures dark. It replaces the unavailable Spoiler Alert media button. The same action
is available in Overview's Lights section and the Lights console's Master card.
**Play/Pause/Next** control the song queue and keep its embedded YouTube account interlock.

The pointing-hand **Cameras** key opens the existing manual camera buttons and three
independent, state-backed masters (keys 9–11), with **Back** in the lower-right corner:

| Key | Master | Action | Persisted state |
|---|---|---|---|
| 9 | **AUTO SCENES** | `autoseq.toggle drum_cycle` | `show.autoseq.active` |
| 10 | **AUTO LIGHTS** | `lights.auto.toggle` | `lights.auto` |
| 11 | **AUTO FX** | `fx.auto.toggle` | `fx.auto` |

**Effects** is one press from **Main key 4** or **Cameras key 12**. The clock formerly on
Main key 4 is now **Cameras key 13**; all lighting, queue, camera and automation buttons
keep their existing positions. Page order is `main`, `cameras`, `effects`, `sprinkles`,
`apple_music`.
Both Effects pages use labeled, colored built-in icons and fire existing presets at manual
priority, so AUTO FX being off never blocks these buttons.

| Key | Effects 1/2 (`effects`) | Effects 2/2 (`sprinkles`) |
|---|---|---|
| 0 | Blue screen (`fx_blue_screen`) | Confetti (`sp_confetti`) |
| 1 | CRT meltdown (`fx_crt`) | Sticker (`sp_sticker`) |
| 2 | Explosion (`fx_drum_explosion`) | Flying toasters (`sp_toasters`) |
| 3 | Wormhole (`fx_wormhole`) | Fill dialog (`sp_dialog`) |
| 4 | Drum portal (`fx_portal`) | Emoji drums (`sp_emoji`) |
| 5 | Kaleidoscope (`fx_kaleidoscope`) | Fireflies (`sp_fireflies`) |
| 6 | Freeze photo (`fx_freeze`) | Groove ripple (`sp_ripple`) |
| 7 | Pixel gravity (`fx_gravity`) | Tiny crowd (`sp_crowd`) |
| 8 | Window cascade (`fx_cascade`) | Kit lights (`sp_kit_lights`) |
| 9 | Solitaire (`fx_solitaire`) | Apple Music → `apple_music` |
| 10 | Song end (`fx_song_end`) | Cameras |
| 11 | Kit light show (`fx_kit_show`) | Spare |
| 12 | AUTO FX (`fx.auto.toggle`, state `fx.auto`) | AUTO FX (same master/state) |
| 13 | Next Effects → `sprinkles` | Previous Effects → `effects` |
| 14 | Main / Back → `main` | Main / Back → `main` |

These cover all twelve real `fx_` and all nine real `sp_` video presets. The `fx_` buttons
retain the `moment` lane and bar quantization; `sp_` buttons retain their independent
`sprinkle` lane and beat quantization. The six extended moments and Freeze reserve their
lane through the full attack/hold/release envelope; see [patches.md](patches.md) for timings.

There is one scene controller, `autoseq/drum_cycle.toml`, not a second camera automation
system. Starting **AUTO SCENES** immediately switches to an eligible camera scene;
later switches use the **45-second** dwell and exclude the **three most recent scenes**.
The current pool is `drum_cams`, `front_dolly`, `side_zoom`, `overhead_zoom`,
`side_youtube` and `overhead_youtube`; `starting_soon`, `chatting` and `private_camera`
are not in the automatic pool. Selecting **Just chatting / DRUM CHAT** stops the
sequence immediately through that scene's `on_enter` hook. AUTO LIGHTS and AUTO FX
remain independent. Edit the scene dwell times in
**Automation → Auto sequence → Drum camera cycle → Scenes**. **AUTO LIGHTS** controls the
existing context-driven lights director; the former **AUTO SHOW** label meant lights only.
**AUTO FX** controls the musical video director: intermittent, phrase-paced camera looks
chosen from the opted-in library using song mood/relative energy and the live lighting colors.
Turning it off stops new selections and fades out only its owned look over three seconds.
Notifications, viewer-paid/subscription/bits effects and manual effects stay independent,
including a manual copy of the same preset. `fx.enabled` remains the overall effects kill switch.
`fx.auto` defaults off. All three keys follow actual engine state, including changes made
elsewhere and persisted state after restart; they do not maintain local toggle flags.

To see the actual rendered key faces, open **Views → Controllers → Stream Deck → Cameras**
and choose **Show on the deck** if that page is not current. Selecting an inactive page alone
shows a sketch, not the rendered images. The page switch changes only the deck display, not
any master, camera scene, lights, effects, or queue action. The read-only
`streamctl query controllers.deck.preview` returns that current page's exact rendered key
images as base64 JPEGs (`keys[].jpeg`, indexed by `keys[].key`); it also works when the USB
deck is disconnected. After switching to Cameras, its `page` field must be `cameras`.

### Apple Music controls

The drum project has an **Apple Music** page, reached from **Effects 2/2 key 10**
(`sprinkles` key 9; UI key numbers start at 1). Its first four keys are **Play**,
**Pause**, **Volume Up**, and **Volume Down**. Middle-row buttons 6 and 7 are
**Back** (previous track) and **Forward** (next track); the lower-right **Main**
key returns to the main deck page. Main's existing Play/Pause/Next buttons still
control the YouTube queue.

Open **Automation → Buttons & pedals → Stream Deck → Apple music** to edit the
buttons in the same place as the rest of the deck. The six **Apple Music · …**
saved actions are also in **Drag onto a key → Saved actions**. Drop an action onto
the desired key, or click a key and choose **Run a saved action**. Clear the old
key if moving rather than copying. Assignments persist in `controllers/deck.toml`.

Each saved action emits
`apple_music.control action=play|pause|next|previous|volume_up|volume_down`.
Play and Pause request distinct states; repeated presses never toggle playback.
Volume changes Apple Music's own MusicKit volume by five percentage points,
clamped to 0–100%; it does not change system, mixer, or queue volume.
Choose a song or album in Apple Music before using Play after opening the app.
Forward and Back use [MusicKit JS](https://js-cdn.music.apple.com/musickit/v3/docs/index.html)
`skipToNextItem()` and `skipToPreviousItem()`, not seeking within a song or changing
the engine queue. MusicKit controls queue-boundary behavior and starts playback of
the selected track, even if it was paused before the skip.

#### Browser bridge and recovery

The installed Chromium Apple Music app uses the unpacked extension at
`scripts/apple-music/` in the engine repository. Chromium's
`~/.config/chromium-flags.conf` includes that directory in `--load-extension`.
The native-host registration is
`~/.config/chromium/NativeMessagingHosts/com.stream_engine.apple_music.json`;
its `path` points to the executable `scripts/apple-music/host.py`, and its
`allowed_origins` is
`["chrome-extension://kjhbkfofkaafamikeghibgkpajfhphha/"]`.
The manifest's public key keeps that extension ID stable. No engine restart or
browser remote-debugging port is required.

Open the Apple Music app normally after login/reboot. The extension starts the
native host, which reconnects to Stream Engine automatically. Startup only reads
status; it never starts playback or replays earlier controls. If the native
connection drops while Chromium remains open, the extension retries every minute;
engine reconnection uses bounded backoff up to 30 seconds. The bridge targets only
`https://music.apple.com`: one app/popup wins over ordinary music tabs; otherwise
an ambiguous target fails visibly rather than controlling the wrong tab.

`health.apple_music` shows connection/control errors in the engine health panel.
`apple_music.playing` and `apple_music.volume` reflect verified browser state.
The native host also writes
`~/.local/state/stream-engine/apple-music.json` with connection, status, last action,
error, and timestamp. Failed controls produce `apple_music.result` and an operator
notification; controls are never automatically retried. Successful five-second
status polling clears recovered errors. Closing the browser marks health unavailable.

For recovery, open Apple Music, select a track if needed, and close extra Apple
Music app windows/tabs if health reports ambiguity. If editing the bridge, reload
only **Stream Engine Apple Music Control** at `chrome://extensions`; do not restart
the engine. Keep generated `__pycache__` out of the extension directory (Chromium
rejects it); use `python -B` when importing the host for development.
`python -B scripts/test_apple_music.py` covers control recovery when the launcher
has closed its stderr pipe; a diagnostic write must not disable subsequent controls.


## MIDI (`kind = "midi"`)

All MIDI goes through the ALSA sequencer (client `stream-engine`), so other programs can use
the same ports. Every hardware port is read; devices that no file claims get an automatic id
(slug of the ALSA name, e.g. `x_touch_mini`) and still produce signals and events.

```toml
# controllers/xtouch.toml
kind = "midi"
match = "X-TOUCH MINI*"          # glob on the ALSA client name (or "client:port")
profile = "xtouch_mini_mc"       # generic | mcu | xtouch_mini_mc | hui
# usb = "usb-0000:0a:00.0-2.2"   # tell identical units apart (USB port path, see below)
# serial = "SC1M19120661"        # …or by USB serial when the device has a real one
# port = "*MIDI 1"               # only some ports of a multi-port device
```

**Device identity.** `streamctl query controllers.midi.ports` lists every port with its device id
and USB path (the `usb-…` path from `/proc/asound/cards`). Two identical controllers match the
same `match`; give each file a `usb =` (or `serial =`) and they stay apart across reboots and
replugging (the engine warns when a `match` is ambiguous).

**Controls.** Profiles name the controls of known surfaces:

* `xtouch_mini_mc` — `enc.1`–`enc.8` (relative, LED rings), `push.1`–`push.8`,
  `btn.1`–`btn.16` (LEDs; top row 1–8, bottom row 9–16), `layer.a`, `layer.b`, `fader`
  (pitch-bend channel 9, not motorized). The unit is switched to MC mode on connect
  (`mode_select = false` to skip).
* `mcu` — `fader.1`–`fader.8`, `fader.master` (motorized, touch-sensing), `vpot.N` (+ rings),
  `rec/solo/mute/select/vsel.N`, all global buttons by name (`play`, `stop`, `bank.left`, …),
  `jog`; scribble strips, meters and the timecode display are driven from `[bank]`.
* `hui` — 8 strips (`fader.N` 14-bit, `touch.N`, `vpot.N`, `select/mute/solo/auto/vsel/insert/rec.N`),
  4-character scribble strips, meters, and the HUI ping.
* `generic` — declare what you need:

```toml
[control.fs_a]
cc = 1
switch = "momentary"      # momentary (≥64 down) | trigger (every message is a press)
[control.pedal]
cc = 7                    # 7-bit, value 0–1
[control.volume]
cc = 7
lsb = 39                  # 14-bit CC pair (or `hires = true` for cc+32)
[control.cutoff]
nrpn = 130                # NRPN (14-bit data entry; `hires = false` for MSB only); `rpn = …`
[control.knob]
cc = 16
relative = "twos"         # endless encoder: mcu | hui | twos | offset64
out = { cc = 48, style = "behringer" }   # LED ring feedback (or `out = "echo"`)
[control.bend]
pitchbend = true
channel = 2               # 1–16; omitted = any channel
```

Undeclared messages are still named: `cc.<n>`, `note.<n>`, `pb`, `nrpn.<n>`, `pressure`
(prefixed `ch<N>.` off channel 1).

**Signals** `midi.<device>.<control>` (0–1; notes carry velocity while held; relative encoders
keep a 0–1 accumulator). Incoming CC streams are coalesced: at most one batch per device every
`coalesce_ms` (default 4 ms), latest value wins.
**Events** `midi.<device>.<control>` on press (`{velocity, down}`), `….up` on release,
`….touch` for touch-sensitive faders, `midi.<device>.program`, `midi.connected`,
`midi.disconnected`.

**Mappings** (`[[map]]`) — what a control does besides its signal:

```toml
[[map]]
control = "enc.1"
target = "fx.rgb_split.amount"   # relative encoder → set at manual priority
# step = 0.01                    # per detent (default: 1/100 of the address range)
# range = [0, 1]                 # override the address metadata
# ring = "fill"                  # dot | boost | fill | spread
[[map]]
control = "enc.8"
inc = ["scene.next"]             # encoder turns run commands
dec = ["scene.prev"]
[[map]]
control = "btn.1"
preset = "hype"                  # same actions and options as deck keys
feedback = true                  # LED follows state (default true)
```

LED rings and button LEDs follow the target's resolved value/state no matter who changed it
(UI, CLI, presets, another controller). While you move or touch a control its own feedback is
held back, so motor faders never fight your hand.

**Bindings with takeover** (absolute controls: faders, knobs, pedals) live in the same file and
are core bindings (§2.6), so pickup/scale/jump run in the core:

```toml
[[binding]]
target = "lights.cuelist.main.master"
signal = "midi.xtouch.fader"
takeover = "pickup"     # no effect until the fader passes the current value
feedback = true         # motor faders / rings follow the target
```

For the 16R use `curve = "linear"` — `mixer.16r.*.fader` are console fader positions.

**This workstation's selectable 16R fader** is configured in
`~/stream-project/controllers/xtouch.toml`. Top-row buttons select channels 1–8;
bottom-row buttons select 9–16. Bottom-row button 3 selects channel 11, the linked
11/12 stereo pair labeled `Computer`. This controls the mixer input, not an individual
YouTube application's volume; any other audio routed to that input changes with it.

`project.toml [params]` sets `controllers.xtouch.channel = 11` as the default.
Each button sets that address; one pickup binding per channel uses
`scope = "controllers.xtouch.channel == N"`, so only the selected channel follows
`midi.xtouch.fader`. Switching channels resets pickup without moving any levels.
Move the physical fader through the selected channel's current position before
adjusting it. Knobs and layer buttons are unassigned; selection LEDs are not driven.
Read or change the selection from the CLI:

```sh
streamctl get controllers.xtouch.channel
streamctl set controllers.xtouch.channel 11
```

The existing `stream-engine` user service handles both USB MIDI and UCNET; no separate
bridge service is required. Verification used emulated Mini MIDI messages through ALSA
and independent fader broadcasts from the actual 16R, including stereo linking,
unselected-channel isolation, and pickup reset after selection.

**MCU/HUI banks** map strips onto numbered targets; bank buttons (`bank.left/right`,
`channel.left/right`) move them, or `midi.bank <device> +8|-8` / `offset=`:

```toml
[bank]
count = 16
fader = "mixer.16r.ch.{n}.fader"
vpot = "mixer.16r.ch.{n}.pan"
mute = "mixer.16r.ch.{n}.mute"
select = "mixer.16r.ch.{n}.select"
name = "mixer.16r.ch.{n}.name"      # scribble strip text
meter = "mixer.16r.meter.ch.{n}"    # meter signal
```

**E-drums** (`[drums]`): note-on → `drums.<pad> {velocity, note, device}`. `gm = true` loads the
General MIDI drum map; add `36 = "kick"`-style overrides and `channel = 10`.

| | |
|---|---|
| Actions | `midi.learn {target}`, `midi.learn.cancel`, `midi.bank <device> <±n>` / `offset=`, `midi.send <device> <hex…>`, `midi.record <device> [seconds] [file=]` (fixture recording), `midi.unmap <device> <control>`, `midi.refresh` |
| Queries | `controllers.midi` (devices, controls with live values, mappings), `controllers.midi.ports`, `controllers.midi.monitor {device?, n?}` (message monitor; turns itself on for 30 s when queried) |
| State | `controllers.midi.<device>.{connected, bank}` |
| Health | `health.midi.<device>` for every configured device |

### Learn

`streamctl do "midi.learn fx.rgb_split.amount"` (or the inspector's MIDI button, or the Controllers
view), then move a control:

* fader / knob / pedal → `[[binding]]` with `takeover = "pickup"` (`"jump"` on motor faders),
  `mode = "replace"`, `feedback = true`, range from the address metadata;
* encoder → `[[map]] target = …` with a step from the range;
* button / note → `[[map]]` with `preset`, `scene`, `toggle` (bool addresses), `trigger …`
  (trigger addresses), or `momentary`;
* a footswitch that sends CC (FBV) fired at a preset/scene/command → the CC is declared as a
  button (`[control.cc.N] switch = "momentary"`) and mapped;
* a Stream Deck key pressed while learning gets the target assigned.

Targets: any address, `preset.<name>`, `scene.<name>`, or `do:<commands>` (one command per line). The mapping is written
to the device's controllers file (created for unconfigured devices, with `usb =` when
identical units are connected), the project reloads, and `midi.learned {target, signal, device,
control, file}` is emitted. Learning times out after 30 s (`midi.learn.timeout`).

### Raw MIDI for other subsystems (Timelines / MTC)

`se_input::midi::ports()`, `subscribe_raw(pattern) -> Receiver<RawMidi>` (every message,
unfiltered, with the master-clock receive time — MTC quarter frames and full-frame SysEx
included) and `output(pattern)?.send(bytes)` (direct, unqueued). Patterns are globs over the
device id or ALSA name (`studio24c`, `*24c*`).

## Voice push-to-talk

Hold a `ptt = true` key (deck or MIDI), hold the button in Views → Controllers, or bind a key:

```lua
-- ~/.config/hypr/bindings.lua: hold Super+Space to talk
o.bind("SUPER", "space", "exec", "streamctl do 'voice.ptt start'")
o.bind("SUPER", "space", "exec", "streamctl do 'voice.ptt stop'", { release = true })
```

Audio is captured from an ALSA PCM only while talking (default `default` = PipeWire), then
transcribed locally with Whisper (whisper.cpp). The model is downloaded once to
`~/.local/share/stream-engine/models/whisper/ggml-<model>.bin` (shared with the clip pipeline) and verified by SHA-1.

Grammar: `scene <name>` / `cut to <name>`, `preset <name>` / `fire <name>` / a bare preset
name, `release <preset>`, `mode <mode>` / `go live` / `be right back`, `take`, `panic`,
`clean`, `next` / `previous` (scene), `next song`, `marker [label]`, `page <name>`, and
`confirm` / `cancel`. Names match fuzzily against scenes, presets (and their labels), modes, and
deck pages; unknown names are rejected rather than guessed.

Risky intents (`[voice] confirm`, default `panic`, `mode`, `clean`, plus presets marked
`confirm`) wait for "confirm" (or `voice.confirm`, a YES key) within 10 s.

```toml
# project.toml
[voice]
enabled = true
device = "default"          # ALSA capture PCM
model = "base.en"           # tiny.en | base.en | small.en | /path/to/ggml.bin
max_seconds = 12
confirm = ["panic", "mode", "clean"]
confirm_timeout = "10s"
threads = 4
gpu = false
```

| | |
|---|---|
| Actions | `voice.ptt start\|stop\|toggle`, `voice.confirm`, `voice.cancel`, `voice.file <wav>` (run a WAV through the same path), `voice.model.download` |
| Events | `voice.intent {text, intent, arg, command, confidence, confirm, executed, ms}` |
| State | `controllers.voice.{state, active, text, pending}` |
| Query | `controllers.voice` (status, last transcripts) |
| Health | `health.voice` |

## Fixtures and tests

`crates/se-input/tests/fixtures/*.txt` hold MIDI/HID streams (`<ms> <hex>` per line) replayed
through the runtime decoders by `tests/fixtures.rs`. Record real ones with
`streamctl do "midi.record xtouch 20 file=/path/fixture.txt"`. `tests/voice_e2e.rs` speaks commands
with espeak-ng and checks the recognized intents (`-- --ignored`, needs the model).

### Hardware acceptance

`scripts/acceptance-surfaces.sh` walks through the M7/M8 checks on the real surfaces with the
running engine and the example mappings: the HYPE preset from the deck, the X-TOUCH, FBV
footswitch A, voice, a keybind and chat (each must arrive with its own origin), the X-TOUCH
encoder 1 LED ring following a value set elsewhere, and the Main cue list from the deck's LX GO
key (page MIX) and from the X-TOUCH fader (fader start). Pass step names to run a subset
(`scripts/acceptance-surfaces.sh deck fbv`); `TIMEOUT` (default 45 s) bounds each wait.
