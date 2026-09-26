# Text to speech (`se-tts`)

Local TTS (PLAN §14.4): **Kokoro-82M v1.0** (Apache-2.0 weights) runs in-process on the CPU with
ONNX Runtime (the `ort` crate, statically linked); phonemes come from **`espeak-ng` run as a
separate process** (GPL-3.0 — never linked). Audio goes to the `tts` audio slot (48 kHz mono) →
the `tts` bus, which ducks music (`[audio.duck] keys = ["tts"]`, see [audio.md](audio.md)).

```text
tts.say ─► normalize ─► queue ─► worker thread: espeak-ng ─► misaki phoneme map ─► Kokoro (ORT)
                                        │ (one item ahead of playback)
                                        ▼
            `tts` slot ◄── player thread: 24→48 kHz polyphase sinc, volume, skip fade, ≤ 0.15 s buffered
```

## Setup

1. `espeak-ng` (installed: 1.52). `health.tts` fails with a hint when it is missing.
2. The model: `stream do tts.model.fetch`, the *Fetch / verify model* button in the TTS view, the
   setup wizard, or directly:

   ```sh
   scripts/fetch-tts-model.sh                      # → ~/.local/share/stream-engine/models/kokoro
   scripts/fetch-tts-model.sh --dir /some/dir --model model.onnx
   ```

   Every file is pinned to Hugging Face `onnx-community/Kokoro-82M-v1.0-ONNX` revision
   `1939ad2a8e416c0acfeecc08a694d14ef25f2231` and verified against the sha256 in the script; the
   script is idempotent (verified files are skipped, corrupt/partial ones re-downloaded) and exits
   1 on any download or checksum failure. Layout: `<dir>/model_quantized.onnx`,
   `<dir>/voices/<name>.bin` (54 voices + `af`, 510×256 f32 each), `<dir>/SOURCE`.

| File | Size | sha256 |
|---|---|---|
| `model_quantized.onnx` (default) | 92,361,116 B | `fbae9257e1e05ffc727e951ef9b9c98418e6d79f1c9b6b13bd59f5c9028a1478` |
| `model.onnx` (fp32) | 325,532,232 B | `8fbea51ea711f2af382e88c833d9e288c6dc82ce5e98421ea61c058ce21a34cb` |
| `voices/af_heart.bin` (default voice) | 522,240 B | `d583ccff3cdca2f7fae535cb998ac07e9fcb90f09737b9a41fa2734ec44a8f0b` |
| `voices/*.bin` (55 files) | 28,725,248 B total | pinned in the script |

## `[tts]` in `project.toml`

```toml
[tts]
enabled   = true                   # subsystem on/off (unloads the model); live toggle: `tts.enabled`
model_dir = "~/.local/share/stream-engine/models/kokoro"  # default <data dir>/models/kokoro; relative = under the data dir
model     = "model_quantized.onnx"
voice     = "af_heart"
speed     = 1.0                    # 0.5–2.0
volume    = 0.9                    # 0–2, linear gain into the tts bus (changes glide, no zipper)
max_chars = 300                    # longer text is cut at a word boundary
max_queue = 20                     # further requests are dropped (tts.finished with error)
lang      = "en-us"                # optional: espeak voice for every request

[[tts.voices]]                     # first match wins; `when` is an se-expr over the request args
when  = "kind == 'twitch.cheer' && amount >= 1000 or tier >= 2"
voice = "am_michael"
speed = 1.1                        # optional
lang  = "en-us"                    # optional
```

Voice choice: an explicit, installed `voice` in `tts.say` → the first matching rule → `voice`.
Speed: rule → `speed`. Language: rule → `lang` → the voice prefix (`a` en-us, `b` en-gb, `e` es,
`f` fr-fr, `h` hi, `i` it, `p` pt-br, `j` ja, `z` cmn) → en-us. Unknown keys and out-of-range
values are errors; a broken section is logged (`error`, target `tts`) and the last good settings
stay.

## Engine surface

| Kind | Name | Notes |
|---|---|---|
| action | `tts.say {text, id?, voice?, kind?, user?, user_id?, message_id?, amount?, tier?}` | text is already policy-filtered (alerts send it after the veto window); `tts.say hello there` also works |
| action | `tts.skip {id?}` | no id: stop the current item (12 ms fade); id: drop that queued item or stop it if playing |
| action | `tts.clear` | queue and current |
| action | `tts.model.fetch` | runs `<share>/scripts/fetch-tts-model.sh --dir <model_dir> --model <model>` in the background; progress in `tts.model.status`; success reloads |
| state | `tts.enabled` (bool, user) | off = stop, clear, and reject new requests |
| state | `tts.speaking`, `tts.queue`, `tts.current.{id,user,voice,text}`, `tts.model.status`, `tts.ready` | readbacks |
| health | `health.tts` | fail: espeak-ng missing / model or default voice missing (says to fetch) / load error; warn: disabled, loading, slot not consumed; pass: model + voice count |
| event | `tts.started {id, user, voice, duration_ms}` | when audio starts |
| event | `tts.finished {id, skipped, error?}` | also for requests dropped before playing (queue full, disabled, banned, …) |
| query | `tts` | `{ready, enabled, current, queue: [{id,user,voice,text}], voices, voice, model_status}` |

Deletion sync (§12.1): `twitch.user.purge {user_id, user}` (every ban/timeout) drops that user's
queued items and stops their current one (matched by `user_id`, else case-insensitive `user`);
`twitch.chat.delete {message_id}` drops the item created from that message; `twitch.chat.clear`
drops the queue (the current item finishes). Mods: `!skiptts` → `tts.skip`, `!cleartts` →
`tts.clear`.

## Text handling

Chat text is data, never markup: it goes to espeak only on stdin, never as arguments, and never
with `[`/`]` (`[[…]]` is espeak's raw-phoneme syntax). Before phonemizing: NFKC folding, invisible
and zalgo marks and emoji removed, URLs/e-mails/bare domains removed, `@user` → `user`, `#1` →
"number 1", runs of a letter capped at two ("sooooo" → "soo"), punctuation runs collapsed, the
same word more than three times in a row cut to three, money (`$5.50` → "5 dollars and 50 cents"),
digit-group commas, decades, clock times, `e.g.`/`Mr.`-style abbreviations; then the `max_chars`
cap.

Phonemes: the text is split at clause punctuation, the word runs are phonemized by one
`espeak-ng -q --ipa --tie=^ -v <lang>` process (one line per run; if espeak splits a very long run
itself, runs are phonemized one by one), the punctuation is re-inserted (Kokoro's vocabulary keeps
`;:,.!?—…"()“”`), and the IPA is mapped to the phoneme set Kokoro v1.0 was trained on (misaki's
espeak fallback: `a^ɪ`→`I`, `e^ɪ`→`A`, `o^ʊ`→`O`/`ə^ʊ`→`Q`, `r`→`ɹ`, `x`→`k`, `ɬ`→`l`, flap
`ɾ`→`T`, …), filtered to the vocabulary, and cut into ≤ 510-token chunks at sentence, then clause,
then word boundaries; chunk audio is joined with 120 ms pauses. The style vector is row
`tokens − 1` of the voice pack, as in hexgrad/kokoro.

## Offline tool

```sh
cargo run -p se-tts --example say -- --text "Thanks for the five hundred bits, let's go!" \
    --voice af_heart --out /tmp/x.wav [--rate 24000] [--model model.onnx] [--speed 1.1] [--lang en-gb]
```

Prints the phonemes, duration, RMS/peak, model load and synthesis time, and the real-time factor.

## Build notes

`ort` downloads a static ONNX Runtime (1.28, CPU) on the first build and caches it in
`~/.cache/ort.pyke.io/dfbin/`; later builds work offline. `cargo --offline` sets
`CARGO_NET_OFFLINE`, which makes `ort-sys` skip even the cached copy — for offline-flagged
builds point it at the cache: `ORT_LIB_PATH=~/.cache/ort.pyke.io/dfbin/x86_64-unknown-linux-gnu/<hash>`.
