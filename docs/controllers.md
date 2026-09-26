# Controllers: Stream Deck, MIDI, voice

`se-input` runs the physical controls (PLAN §10): the Stream Deck, MIDI surfaces (X-TOUCH MINI,
FBV Express, anything class-compliant, MCU/HUI surfaces), the Studio 24c DIN port (e-drums,
MTC for Timelines), and voice push-to-talk. Everything is configured in `controllers/*.toml`
(hot-reloaded; a broken file keeps its last good version) and shown in **Views → Controllers**.

The same preset can be fired from the deck, a MIDI button, a footswitch, voice, a keybind
(`stream do "preset.fire hype"`), and chat (a rule) — they all end up as the same core command, with
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

Key images are rendered with the Omarchy theme colors and font (`omarchy font current`, a
Nerd Font) and re-rendered on `omarchy theme set` / font changes. They follow state: presets
light up while active with a countdown bar for `hold`, scene keys show program (red edge) and
preview (yellow edge), toggles show on/off, `ptt` shows LISTEN / … / CONFIRM?. Only changed
keys are sent to the device. Unplugging and replugging the deck reconnects within ~1 s.

| | |
|---|---|
| State | `controllers.<deck>.{connected, page, serial, model, firmware, brightness}`, `controllers.page` (primary deck; the UI pad grid mirrors it) |
| Events | `deck.key {deck, page, key, down}`, `deck.connected`, `deck.disconnected` |
| Actions | `deck.page <name\|next\|prev>`, `deck.press {key, page?}` / `deck.release` (run a key as if pressed — the UI pads use this), `deck.brightness <0–100>`, `deck.assign {page, key, preset\|scene\|toggle\|momentary\|do\|page\|ptt, label?, icon?, color?, hold?, confirm?}` / `{…, clear = true}` (edits `controllers/deck.toml`, comments kept), `deck.refresh` |
| Queries | `controllers.page {page?}`, `controllers.pages`, `controllers.deck` (status + every page), `controllers.deck.preview {since?}` (key images as base64 JPEG) |
| Health | `health.deck` |

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

**Device identity.** `stream query controllers.midi.ports` lists every port with its device id
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

`stream do "midi.learn fx.rgb_split.amount"` (or the inspector's MIDI button, or the Controllers
view), then move a control:

* fader / knob / pedal → `[[binding]]` with `takeover = "pickup"` (`"jump"` on motor faders),
  `mode = "replace"`, `feedback = true`, range from the address metadata;
* encoder → `[[map]] target = …` with a step from the range;
* button / note → `[[map]]` with `preset`, `scene`, `toggle` (bool addresses), `trigger …`
  (trigger addresses), or `momentary`;
* a footswitch that sends CC (FBV) fired at a preset/scene/command → the CC is declared as a
  button (`[control.cc.N] switch = "momentary"`) and mapped;
* a Stream Deck key pressed while learning gets the target assigned.

Targets: any address, `preset.<name>`, `scene.<name>`, or `do:<command>`. The mapping is written
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
o.bind("SUPER", "space", "exec", "stream do 'voice.ptt start'")
o.bind("SUPER", "space", "exec", "stream do 'voice.ptt stop'", { release = true })
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
`stream do "midi.record xtouch 20 file=/path/fixture.txt"`. `tests/voice_e2e.rs` speaks commands
with espeak-ng and checks the recognized intents (`-- --ignored`, needs the model).
