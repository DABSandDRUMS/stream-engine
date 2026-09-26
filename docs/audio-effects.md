# Audio effect reference

Generated from the `se-dsp` registry — regenerate with
`cargo run -p se-dsp --example fx_reference > docs/audio-effects.md`.

Use a kind in an effect chain (`{ name = "x", kind = "<kind>", <param> = … }`, see
[audio.md](audio.md)); every parameter is live at `audio.bus.<bus>.fx.<name>.<param>`.
Effects output the processed signal; the slot mixes it with the dry signal using `wet`/`dry`.

| Kind | Description | Default wet/dry | Latency @ 48 kHz |
|---|---|---|---|
| `svf` | State-variable filter (TPT): low-pass, high-pass, band-pass, or notch with log-gliding cutoff and resonance. | 1 / 0 | 0 |
| `eq` | Parametric EQ: low shelf, three peaking bands, high shelf (RBJ biquads), output gain. | 1 / 0 | 0 |
| `djfilter` | One-knob DJ filter: left of center sweeps a low-pass down, right sweeps a high-pass up, flat in the middle. | 1 / 0 | 0 |
| `compressor` | Stereo-linked soft-knee compressor with optional sidechain key. | 1 / 0 | 0 |
| `limiter` | Lookahead brickwall limiter (inter-sample peak aware); output never exceeds the ceiling. | 1 / 0 | 146 samples |
| `gate` | Noise/ducking gate with hysteresis, hold, range, and optional sidechain key. | 1 / 0 | 0 |
| `transient` | Transient shaper: boost or cut attacks and sustain independently of level. | 1 / 0 | 0 |
| `saturation` | 2x-oversampled soft saturation (tanh, tape, tube) with tilt tone. | 1 / 0 | 15 samples |
| `distortion` | 2x-oversampled distortion (hard clip, wavefolder, fuzz) with tilt tone. | 1 / 0 | 15 samples |
| `bitcrush` | Bit-depth and sample-rate reduction with optional anti-alias pre-filter. | 1 / 0 | 0 |
| `delay` | Tempo-synced stereo/ping-pong delay with filtered feedback; outputs echoes only. | 0.35 / 1 | 0 |
| `reverb` | 8-line feedback-delay-network reverb with predelay, damping, and width; outputs the reverb only. | 0.25 / 1 | 0 |
| `chorus` | Stereo two-voice chorus (free or tempo-synced LFO); outputs the modulated voices. | 0.5 / 1 | 0 |
| `flanger` | Stereo flanger with feedback (free or tempo-synced LFO); outputs the swept delay. | 0.5 / 0.5 | 0 |
| `phaser` | Stereo allpass phaser, 2–12 stages with feedback (free or tempo-synced LFO); outputs the allpass chain. | 0.5 / 0.5 | 0 |
| `pitch` | Granular pitch shifter (±24 semitones); latency = half the window. | 1 / 0 | 1442 samples |
| `stutter` | Beat repeat: on trigger waits for the grid, captures a slice, and loops it; click-free release. | 1 / 0 | 0 |
| `tapestop` | Tape stop: on trigger playback slows to a standstill; release spins back up or crossfades. | 1 / 0 | 0 |
| `vinylbrake` | Vinyl brake (exponential slow-down) or backspin on trigger. | 1 / 0 | 0 |
| `reverse` | Reverse buffer: while triggered, every chunk plays the previous chunk backwards. | 1 / 0 | 0 |
| `chopper` | Tempo-synced gate chopper (straight, dotted, triplet, offbeat); always-on or triggered. | 1 / 0 | 0 |
| `gain` | Utility: gain, balance, stereo width, polarity invert. | 1 / 0 | 0 |

## `svf`

State-variable filter (TPT): low-pass, high-pass, band-pass, or notch with log-gliding cutoff and resonance.

| Param | Range | Default | Description |
|---|---|---|---|
| `mode` | `lp` `hp` `bp` `notch` | `lp` | response: low-pass, high-pass, band-pass (0 dB peak), notch |
| `cutoff` | 20 … 20000 Hz | 1000 | cutoff / center frequency (glides in log frequency) |
| `resonance` | 0 … 1 | 0.1 | 0–1 → Q 0.5–20 (log scale; 0.094 = Butterworth) |

## `eq`

Parametric EQ: low shelf, three peaking bands, high shelf (RBJ biquads), output gain.

| Param | Range | Default | Description |
|---|---|---|---|
| `low_freq` | 20 … 1000 Hz | 100 | low shelf corner |
| `low_gain` | -24 … 24 dB | 0 | low shelf gain |
| `low_q` | 0.3 … 2 | 0.707 | low shelf slope (Q; 0.707 = no overshoot) |
| `mid1_freq` | 20 … 20000 Hz | 250 | peaking band 1 center |
| `mid1_gain` | -24 … 24 dB | 0 | peaking band 1 gain |
| `mid1_q` | 0.1 … 18 | 1 | peaking band 1 bandwidth (Q) |
| `mid2_freq` | 20 … 20000 Hz | 1000 | peaking band 2 center |
| `mid2_gain` | -24 … 24 dB | 0 | peaking band 2 gain |
| `mid2_q` | 0.1 … 18 | 1 | peaking band 2 bandwidth (Q) |
| `mid3_freq` | 20 … 20000 Hz | 4000 | peaking band 3 center |
| `mid3_gain` | -24 … 24 dB | 0 | peaking band 3 gain |
| `mid3_q` | 0.1 … 18 | 1 | peaking band 3 bandwidth (Q) |
| `high_freq` | 1000 … 20000 Hz | 8000 | high shelf corner |
| `high_gain` | -24 … 24 dB | 0 | high shelf gain |
| `high_q` | 0.3 … 2 | 0.707 | high shelf slope (Q; 0.707 = no overshoot) |
| `output` | -24 … 24 dB | 0 | output gain |

## `djfilter`

One-knob DJ filter: left of center sweeps a low-pass down, right sweeps a high-pass up, flat in the middle.

| Param | Range | Default | Description |
|---|---|---|---|
| `filter` | -1 … 1 | 0 | −1…0 low-pass sweep (20 kHz → 40 Hz), 0…1 high-pass sweep (20 Hz → 10 kHz), flat near 0 |
| `resonance` | 0 … 1 | 0.3 | 0–1 → Q 0.5–20 (log scale) |

## `compressor`

Stereo-linked soft-knee compressor with optional sidechain key.

| Param | Range | Default | Description |
|---|---|---|---|
| `threshold` | -60 … 0 dB | -18 | level where compression starts |
| `ratio` | 1 … 20 :1 | 4 | compression ratio above the threshold |
| `attack` | 0.1 … 100 ms | 10 | gain-reduction attack time |
| `release` | 10 … 2000 ms | 150 | gain-reduction release time |
| `knee` | 0 … 24 dB | 6 | soft-knee width around the threshold |
| `makeup` | -12 … 24 dB | 0 | output gain after compression |
| `sidechain` | true / false | false | detect from the routed key signal instead of the input |

## `limiter`

Lookahead brickwall limiter (inter-sample peak aware); output never exceeds the ceiling.

| Param | Range | Default | Description |
|---|---|---|---|
| `ceiling` | -24 … 0 dB | -1 | output never exceeds this level |
| `gain` | 0 … 24 dB | 0 | drive into the limiter |
| `release` | 1 … 1000 ms | 100 | gain recovery time |
| `lookahead` | 0.5 … 10 ms | 3 | lookahead; latency = lookahead + 2 samples (changing it re-primes the detector) |

## `gate`

Noise/ducking gate with hysteresis, hold, range, and optional sidechain key.

| Param | Range | Default | Description |
|---|---|---|---|
| `threshold` | -80 … 0 dB | -40 | opens above this level (closes 4 dB below it) |
| `attack` | 0.05 … 50 ms | 0.5 | opening time |
| `hold` | 0 … 500 ms | 50 | stays open this long after the level falls |
| `release` | 5 … 2000 ms | 150 | closing time |
| `range` | -80 … 0 dB | -80 | attenuation while closed |
| `sidechain` | true / false | false | detect from the routed key signal instead of the input |

## `transient`

Transient shaper: boost or cut attacks and sustain independently of level.

| Param | Range | Default | Description |
|---|---|---|---|
| `attack` | -24 … 24 dB | 0 | boost/cut of transients (fast vs slow envelope) |
| `sustain` | -24 … 24 dB | 0 | boost/cut of the sustain/decay portion |
| `output` | -24 … 24 dB | 0 | output gain |

## `saturation`

2x-oversampled soft saturation (tanh, tape, tube) with tilt tone.

| Param | Range | Default | Description |
|---|---|---|---|
| `drive` | 0 … 36 dB | 6 | input gain into the shaper |
| `type` | `tanh` `tape` `tube` | `tanh` | tanh: symmetric soft clip; tape: softer exponential knee; tube: asymmetric (even harmonics) |
| `tone` | -1 … 1 | 0 | tilt EQ around 1 kHz after the shaper: −1 dark … +1 bright (±6 dB) |
| `output` | -24 … 12 dB | 0 | output gain |

## `distortion`

2x-oversampled distortion (hard clip, wavefolder, fuzz) with tilt tone.

| Param | Range | Default | Description |
|---|---|---|---|
| `drive` | 0 … 48 dB | 12 | input gain into the shaper |
| `type` | `hard` `fold` `fuzz` | `hard` | hard: hard clip at ±1; fold: sine wavefolder; fuzz: asymmetric clip |
| `tone` | -1 … 1 | 0 | tilt EQ around 1 kHz after the shaper: −1 dark … +1 bright (±6 dB) |
| `output` | -24 … 12 dB | -6 | output gain |

## `bitcrush`

Bit-depth and sample-rate reduction with optional anti-alias pre-filter.

| Param | Range | Default | Description |
|---|---|---|---|
| `bits` | 1 … 24 bits | 8 | quantizer resolution (fractional values glide smoothly) |
| `rate` | 100 … 48000 Hz | 12000 | sample-and-hold rate (at or above the graph rate = off) |
| `antialias` | true / false | false | 4th-order low-pass at 0.45 × rate before the rate reduction |
| `output` | -24 … 12 dB | 0 | output gain |

## `delay`

Tempo-synced stereo/ping-pong delay with filtered feedback; outputs echoes only.

| Param | Range | Default | Description |
|---|---|---|---|
| `sync` | true / false | true | echo spacing from `division` at the current tempo (else `time`) |
| `division` | `1/1` `1/2` `1/4` `1/8` `1/16` `1/32` `1/2t` `1/4t` `1/8t` `1/16t` `1/4d` `1/8d` `1/16d` | `1/8d` | echo spacing when synced |
| `time` | 1 … 2000 ms | 375 | echo spacing when not synced |
| `feedback` | 0 … 1 | 0.4 | repeat level (soft-limited above −6 dBFS; 1 = endless) |
| `ping_pong` | true / false | false | echoes alternate left/right (input summed to mono) |
| `tone` | 200 … 20000 Hz | 6000 | low-pass cutoff inside the feedback loop |

## `reverb`

8-line feedback-delay-network reverb with predelay, damping, and width; outputs the reverb only.

| Param | Range | Default | Description |
|---|---|---|---|
| `size` | 0 … 1 | 0.5 | room size (scales the network's delay lengths; glides) |
| `decay` | 0.1 … 20 s | 2 | RT60 decay time |
| `damping` | 0 … 1 | 0.5 | high-frequency damping per pass (0 = bright, 1 = dark) |
| `predelay` | 0 … 250 ms | 20 | delay before the reverb starts |
| `width` | 0 … 1 | 1 | stereo width of the tail (0 = mono) |

## `chorus`

Stereo two-voice chorus (free or tempo-synced LFO); outputs the modulated voices.

| Param | Range | Default | Description |
|---|---|---|---|
| `rate` | 0.01 … 10 Hz | 0.8 | LFO rate (free-running) |
| `sync` | true / false | false | LFO period from `division` (locked to the beat) instead of `rate` |
| `division` | `1/1` `1/2` `1/4` `1/8` `1/16` `1/32` `1/2t` `1/4t` `1/8t` `1/16t` `1/4d` `1/8d` `1/16d` | `1/1` | LFO period when synced |
| `depth` | 0 … 1 | 0.5 | delay modulation (±4 ms at 1) |
| `delay` | 5 … 40 ms | 15 | base delay of the voices |
| `spread` | 0 … 1 | 0.5 | right-channel LFO phase offset (0 = mono, 1 = 180°) |

## `flanger`

Stereo flanger with feedback (free or tempo-synced LFO); outputs the swept delay.

| Param | Range | Default | Description |
|---|---|---|---|
| `rate` | 0.01 … 10 Hz | 0.25 | LFO rate (free-running) |
| `sync` | true / false | false | LFO period from `division` (locked to the beat) instead of `rate` |
| `division` | `1/1` `1/2` `1/4` `1/8` `1/16` `1/32` `1/2t` `1/4t` `1/8t` `1/16t` `1/4d` `1/8d` `1/16d` | `1/1` | LFO period when synced |
| `depth` | 0 … 1 | 0.7 | sweep width (±2 octaves of delay at 1) |
| `delay` | 0.1 … 10 ms | 1 | sweep center delay |
| `feedback` | -0.95 … 0.95 | 0.5 | resonance (negative = odd-harmonic comb) |
| `spread` | 0 … 1 | 0.25 | right-channel LFO phase offset (0 = mono, 1 = 180°) |

## `phaser`

Stereo allpass phaser, 2–12 stages with feedback (free or tempo-synced LFO); outputs the allpass chain.

| Param | Range | Default | Description |
|---|---|---|---|
| `rate` | 0.01 … 10 Hz | 0.3 | LFO rate (free-running) |
| `sync` | true / false | false | LFO period from `division` (locked to the beat) instead of `rate` |
| `division` | `1/1` `1/2` `1/4` `1/8` `1/16` `1/32` `1/2t` `1/4t` `1/8t` `1/16t` `1/4d` `1/8d` `1/16d` | `1/1` | LFO period when synced |
| `depth` | 0 … 1 | 0.7 | sweep width (±2 octaves at 1) |
| `center` | 100 … 8000 Hz | 800 | sweep center frequency |
| `feedback` | -0.95 … 0.95 | 0.5 | resonance of the notches |
| `stages` | `2` `4` `6` `8` `12` | `4` | first-order allpass stages (notches = stages / 2) |
| `spread` | 0 … 1 | 0.5 | right-channel LFO phase offset (0 = mono, 1 = 180°) |

## `pitch`

Granular pitch shifter (±24 semitones); latency = half the window.

| Param | Range | Default | Description |
|---|---|---|---|
| `semitones` | -24 … 24 st | 0 | pitch shift (exactly 0 = clean delay by the reported latency) |
| `window` | 10 … 200 ms | 60 | grain window: longer is smoother on sustained tones; latency = window / 2 |

## `stutter`

Beat repeat: on trigger waits for the grid, captures a slice, and loops it; click-free release.

| Param | Range | Default | Description |
|---|---|---|---|
| `division` | `1/1` `1/2` `1/4` `1/8` `1/16` `1/32` `1/2t` `1/4t` `1/8t` `1/16t` `1/4d` `1/8d` `1/16d` | `1/16` | length of the captured slice that repeats |
| `quantize` | `off` `1/16` `1/8` `1/4` `1 bar` | `1/16` | on trigger, wait for this grid before capturing |
| `decay` | 0 … 1 | 0 | level drop per repeat (0 = none, 0.5 = −6 dB per repeat) |
| `gate` | 0.05 … 1 | 1 | sounding portion of each repeat |

## `tapestop`

Tape stop: on trigger playback slows to a standstill; release spins back up or crossfades.

| Param | Range | Default | Description |
|---|---|---|---|
| `time` | 50 … 4000 ms | 1000 | time to come to a stop (when not synced) |
| `sync` | true / false | false | stop time from `division` at the current tempo |
| `division` | `1/1` `1/2` `1/4` `1/8` `1/16` `1/32` `1/2t` `1/4t` `1/8t` `1/16t` `1/4d` `1/8d` `1/16d` | `1/2` | stop time when synced |
| `curve` | -1 … 1 | 0 | speed curve: −1 slow start / abrupt end, 0 linear, +1 fast start / long crawl |
| `restart` | 0 … 2000 ms | 0 | on release, spin back up over this time (0 = crossfade straight back to live) |

## `vinylbrake`

Vinyl brake (exponential slow-down) or backspin on trigger.

| Param | Range | Default | Description |
|---|---|---|---|
| `time` | 100 … 3000 ms | 800 | brake (or backspin) duration when not synced |
| `sync` | true / false | false | duration from `division` at the current tempo |
| `division` | `1/1` `1/2` `1/4` `1/8` `1/16` `1/32` `1/2t` `1/4t` `1/8t` `1/16t` `1/4d` `1/8d` `1/16d` | `1/4` | duration when synced |
| `backspin` | true / false | false | spin the record backwards instead of braking |
| `restart` | 0 … 2000 ms | 0 | on release, spin back up over this time (0 = crossfade straight back to live) |

## `reverse`

Reverse buffer: while triggered, every chunk plays the previous chunk backwards.

| Param | Range | Default | Description |
|---|---|---|---|
| `division` | `1/1` `1/2` `1/4` `1/8` `1/16` `1/32` `1/2t` `1/4t` `1/8t` `1/16t` `1/4d` `1/8d` `1/16d` | `1/4` | chunk length: each chunk plays the previous one backwards |
| `quantize` | `off` `1/16` `1/8` `1/4` `1 bar` | `off` | on trigger, wait for this grid before starting |

## `chopper`

Tempo-synced gate chopper (straight, dotted, triplet, offbeat); always-on or triggered.

| Param | Range | Default | Description |
|---|---|---|---|
| `division` | `1/1` `1/2` `1/4` `1/8` `1/16` `1/32` `1/2t` `1/4t` `1/8t` `1/16t` `1/4d` `1/8d` `1/16d` | `1/16` | gate step length |
| `duty` | 0.05 … 1 | 0.5 | open portion of each step |
| `smooth` | 0 … 20 ms | 3 | S-curve edge time (0 = hard gate) |
| `depth` | 0 … 1 | 1 | attenuation while closed (1 = silent) |
| `pattern` | `straight` `dotted` `triplet` `offbeat` | `straight` | step = division ×1 (straight), ×1.5 (dotted), ×2/3 (triplet), or straight shifted half a step (offbeat) |

## `gain`

Utility: gain, balance, stereo width, polarity invert.

| Param | Range | Default | Description |
|---|---|---|---|
| `gain` | -60 … 24 dB | 0 | level |
| `pan` | -1 … 1 | 0 | stereo balance (−1 left … +1 right; center = unity) |
| `width` | 0 … 2 | 1 | stereo width (0 = mono, 1 = unchanged, 2 = wide) |
| `invert_l` | true / false | false | invert left polarity |
| `invert_r` | true / false | false | invert right polarity |
