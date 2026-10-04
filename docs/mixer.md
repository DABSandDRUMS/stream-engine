# StudioLive 16R (mixer) — operator notes

`se-mixer` connects the engine to the PreSonus StudioLive 16R over UCNET (the protocol UC
Surface uses). Everything the console exposes becomes addresses under `mixer.16r.*`, meters
become signals, and presets can recall or crossfade mix snapshots. The console stays the
authority for its own state: the engine never slams faders on connect.

## Finding the console

In order: `[mixer] host` → the last host that worked (runtime DB) → UDP discovery broadcasts
(port 47809, every 3 s) → a LAN probe (PreSonus MAC addresses from the ARP cache, then TCP
53000 across the local /24 of each wired interface).

On this machine the 16R is **10.0.0.187** (serial RA1E24110101, firmware 3.2.0.108461).
ufw's default-deny input policy drops the console's discovery broadcasts, so the probe finds
it. To make broadcast discovery work too (optional):

```sh
sudo ufw allow in proto udp from any port 53000 to any port 47809 comment 'PreSonus UCNET discovery'
```

Meters arrive over UDP from the console's port 53000. The adapter first sends an empty
datagram to that port so ufw treats the meter stream as replies (no rule needed). If the
Mixer view says "no meters" anyway: `sudo ufw allow in proto udp from 10.0.0.187 port 53000`.

## Configuration (`project.toml`)

```toml
[mixer]
host = "10.0.0.187"          # optional: skips discovery
# serial = "RA1E24110101"    # pick one console when several are on the LAN
name = "16r"                 # address namespace → mixer.16r.*
rate_hz = 50                 # max sends per control per second (latest value wins)
writable = ["**"]            # what the engine may change (patterns below mixer.16r.); the rest is readonly
panic_snapshot = "safe"      # mixes/safe.toml is recalled by Panic
touch_release = true         # moving a control on the console releases engine overrides on it
# store = [...]              # what `mixer.snapshot.store` captures (default: channel strips + sends)

[mixer.talk]                 # → signal mic.talking (0/1), for ducking bindings
channel = "VocalMic"         # line channel number or its console label
threshold_db = -38           # input level (dBFS) that counts as voice
attack = "40ms"
hold = "600ms"               # speech gaps shorter than this keep it on

[safety]
# Hard caps (core resolver) — including main/monitor output levels. A capped output above
# its cap is pulled down, also when it was raised on the console.
caps = { "mixer.16r.main.fader" = [0.0, 0.82], "mixer.16r.aux.*.fader" = [0.0, 0.82] }
```

Fader values are **console positions** 0–1: 0 = −∞, ≈0.72 = 0 dB, 1.0 = +10 dB (0.82 ≈
+3.6 dB, 0.5 ≈ −10 dB). `mixer.16r.<strip>.db` shows the level in dB.

## Addresses

Only what the connected console has is declared (coverage is measured on connection; query
`mixer.coverage` lists address, console path, and writability).

| Address | Meaning |
|---|---|
| `mixer.16r.connected`, `.model`, `.firmware`, `.serial`, `.host`, `.channels`, `.auxes`, `.fxes` | session + console info (readonly) |
| `mixer.16r.ch.N.fader\|mute\|pan` | line channel N (also `.name`, `.link`, `.db` readonly) |
| `mixer.16r.ret.N.*`, `mixer.16r.fxret.N.*`, `mixer.16r.tb.*` | digital return, FX returns, talkback |
| `mixer.16r.aux.A.fader\|mute` | aux (monitor) output A |
| `mixer.16r.aux.A.ch.N.send` (also `.ret.N`, `.fxret.N`, `.tb`) | send of a strip to aux A |
| `mixer.16r.fx.F.fader\|mute`, `mixer.16r.fx.F.ch.N.send` | FX bus F master and sends |
| `mixer.16r.fx.F.type`, `mixer.16r.fx.F.param.*` | FX processor (readback only) |
| `mixer.16r.main.fader\|mute` | main LR |

Signals: `mixer.16r.meter.ch.N` (input level, linear 0–1), `mixer.16r.meter.aux.A`,
`mixer.16r.meter.fx.F`, `mixer.16r.meter.main[.l|.r]`, `mic.talking`.

Events: `mixer.changed {address, value, source}` (a change made on the console / UC Surface,
never our own echoes; ≤ 10/s per control), `mixer.connected`, `mixer.disconnected {reason}`,
`mixer.snapshot.recalled {snapshot, fade, applied, skipped}`, `mixer.snapshot.stored`.

## How sync works

* Console → engine: every reported value becomes the address's base value. Our own changes
  come back as echoes (the console echoes mutes as `PV` and faders in an `MS fdrs` packet with
  every fader) and are recognized as such.
* Engine → console: a resolved value that changes (UI, CLI, MIDI binding, preset, snapshot) is
  sent if the console differs, at most `rate_hz` times per control, latest value wins.
* Someone moves a control on the console while the engine holds an override on it: the
  console wins, the override is released (`touch_release`), MIDI pickup must cross the new
  value before it takes over again.
* Reconnect: the console is authoritative; engine overrides that disagree are released.

## Snapshots (`mixes/*.toml`)

```toml
# mixes/brb.toml
label = "BRB"
fade = "2s"                   # default crossfade
[values]
"ch.10.fader" = 0.0           # addresses relative to mixer.16r (or full)
"ch.10.mute" = true           # mutes switch after a fade out / before a fade in
"aux.1.ch.10.send" = 0.0
"ch.11.fader" = 0.8
```

```toml
# presets/brb.toml
mix = { snapshot = "brb", fade = "3s" }
```

```sh
streamctl do mixer.snapshot.store snapshot=safe label=Safe       # capture the current mix
streamctl do mixer.snapshot.recall snapshot=brb fade=2s          # crossfade (fade=0: instant)
streamctl do mixer.panic                                         # stop fades, recall the safe mix
```

Recalls are overrides at the caller's priority with key `mixer:snapshot`; a newer recall
replaces them. Addresses the console doesn't have or that aren't writable are skipped (and
reported). **Store a `safe` mix** before the first show — Panic recalls it; without it Panic
only stops fades and enforces caps (health warns until it exists).

## Safety

* Chat can never touch the mixer: the core refuses chat-priority sets/animations/actions on
  `mixer.*` and skips `mix`/`mixer.*` parts of chat-fired presets; the adapter refuses
  chat-originated actions again.
* `writable` limits what the engine may change at all (everything else is readonly).
* Output caps via `[safety] caps`.
* The console's own scenes/projects are never recalled by the engine; when one is recalled
  on the console, the adapter follows whatever the console reports (parameter updates or a
  full state resend).

### This workstation: hardware effects stay operator-owned

`~/stream-project/project.toml` restricts `[mixer] writable` to ordinary channel/return,
talkback, main, and non-FX aux controls. FX buses, FX sends, FX returns, and FX-return aux
sends are read-only to Stream Engine. Never broaden this project's permissions to `["**"]`:
the owner's 16R reverb must not be muted, lowered, reset, or automated by the engine.
The X-Touch channel fader still works; UC Surface/the console retain direct FX control.

A `writable`-only change hot-updates permissions without reconnecting the console or
changing hardware levels. While live, obtain approval for that exact permissions change
first. Verify with `streamctl query mixer.coverage` (FX rows must have `writable: false`),
`streamctl get 'mixer.16r.fx.**'`, `streamctl get 'mixer.16r.fxret.**'`, and stream health.
Do not test protection by muting reverb during a live show. If it turns off while protected,
inspect console scene/project recalls and other UCNET clients; engine protection does not
prevent another client from changing the desk.

## Firmware

Pin the firmware. After an update, re-record the fixtures (see `crates/se-mixer/README.md`)
and run `cargo test -p se-mixer`: the recorded-traffic tests catch protocol changes.
