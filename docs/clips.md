# Clips: hype markers, the post-stream clip job, and review

`se-clips` (PLAN §18, M10). During the show it scores hype and writes markers; when the session
closes it turns markers into ranked wide + tall clips with burned-in captions and without the
music track; the Session review view is where you approve, reject, retrim, and upload them.

## During the show: the hype detector

Every 100 ms of *screen time* the detector adds up:

| Input | Source | Contribution (defaults in `[clips.hype.weights]`) |
|---|---|---|
| chat rate vs baseline | `twitch.chat` events (and the `twitch.chat_rate` signal) | `0.8 × ln(rate / baseline)` over a 5 s window; 5-min baseline |
| emote spam | adapter emote counts, else known emote names in the text | `0.5 × ln(rate / baseline)` |
| copypasta waves | the same message from different viewers | `0.08` per repeat |
| `!clip` votes | chat messages starting with `!clip` | `0.4` per unique voter; 3 voters force a marker |
| bits / subs / gifts / raids / tips / hype train | `twitch.*`, `tip` | decaying impulses (8 s), log-scaled by size |
| mic spikes, laughter | `mic.level` vs its baseline | z-score above 2.5 σ; bursty 3–8 Hz energy |
| music drops | `band.drop` / `music.drop`, `music.level`/`band.level` novelty | `0.5` per drop, jumps in level |

Viewer reactions (chat, emotes, votes, bits) are shifted back by the measured Twitch delay
(`twitch_delay_ms` clock mapping; 3 s until the Twitch adapter measures it), so they line up
with the moment they react to. A moment is scored once `delay + 2 s` has passed.

When the score crosses `clips.hype.threshold` (default 1.0, live-adjustable) an episode opens;
it closes when the score stays under 60 % of the threshold for 2 s (episodes within 8 s merge,
60 s max). The result is a marker written through `session.marker`:

```json
{"kind": "hype", "start": …, "peak": …, "end": …, "score": 1.62, "reasons": ["chat", "emotes"], "components": {…}}
```

(master-clock ns; the window starts 10–30 s before the peak) plus a Twitch stream marker
(`twitch.marker {description}`). Manual markers — `streamctl marker [label]`, or a deck key /
voice command bound to `session.marker` ("clip that") — get a Twitch marker too and become a
window from 30 s before to 8 s after the press.

State: `clips.hype.enabled`, `clips.hype.threshold` (settable), readback `clips.hype.active`,
`clips.hype.delay_ms`, `clips.hype.markers`. Signal: `hype.score` (bind it, scope it).

**Script edition.** `patches/hype_detector` is the same detector as a Lua script patch. It
always publishes `patch.hype_detector.score`; set its `write_markers` param (and turn the
built-in one off with `set clips.hype.enabled false`) to let it write the markers. Script
markers give their window relative to the press (`start_ago`, `peak_ago`, `end_ago` seconds).

## After the show: the clip job

Ending the show (`mode.set offline`) closes the session; the job is queued automatically
(`[clips] auto_process`). Run it by hand with `streamctl do "clips.process session=<id>"` (no
session = the last closed one). One job runs at a time, niced, and resumes after a restart.

1. **Markers → windows.** Overlapping marker windows merge (their scores add up).
2. **Recording time.** Windows map to the OBS recording through the recording's master-clock
   start (`recordings[].start_ns` in `sessions/<id>/meta.toml`, written by the OBS adapter;
   fallback: the `clock.obs_record` mapping).
3. **Transcript.** The mic track (see *Audio tracks*) around each window is transcribed with
   Whisper (`small.en`, CPU, word timestamps). The model downloads once to
   `~/.local/share/stream-engine/models/whisper/` and is checked by SHA-256.
4. **In/out and ranking.** In/out snap to sentence boundaries near the window (never
   mid-word, 8–60 s). Score = marker score + speech density at the peak + excitement ("let's
   go", "!", laughter) − length penalty + clean-edge bonus. An optional external ranker can
   override (below).
5. **Cut.** Wide (1920×1080) and tall (1080×1920) MP4s with NVENC (`h264_nvenc`; libx264 when
   NVENC can't open a session because OBS holds them), captions burned in (ASS, spoken word
   highlighted), loudness-normalized to −14 LUFS, music dropped. Tall comes from the tall canvas
   recording when one covers the clip, else a 9:16 crop of the wide one
   (`tall_crop_center`).
6. **Queue.** Files land in `sessions/<id>/clips/` (`p<peak>_wide.mp4`, `_tall.mp4`, `.jpg`
   thumbnails, `.ass` captions) and rows in the `clips` table. Progress: `clips.progress`
   events and `clips.job.*` state; `clips.done {timings}` / `clips.failed` at the end.

### Audio tracks (owner setup)

Clips can only leave music out when OBS records it on its own track. In OBS: **Settings →
Output → Output Mode: Advanced → Recording → Audio Track** tick one track per engine bus, and
name each OBS audio source after its engine node (`se-mic`/`se-band`, `se-music`, …); the OBS
adapter reports each track's sources and PipeWire nodes, and `[clips.audio.roles]` globs
classify them. Tracks carrying a role in `drop` (default `music`, `program`) stay out of the
clip; the rest are mixed. With a single mixed track the clip keeps its audio and is flagged
"music may be included" (`[clips.audio] mixed = "mute"` silences it instead). Without any
track info, `[clips.audio] tracks = ["mic", "music", "band"]` names the streams by index.

## Review

**Recordings** page: *Clips to review* (every clip waiting for a decision, as video cards), and
*Past streams* (one card per stream with its length, markers and clips; open one for its markers
timeline, recordings and clips). Clip cards: wide + vertical thumbnails (click to play with
`xdg-open`), reasons, captions, music flag, **Keep** (approve), **Skip** (reject), **Upload**, and
**Trim** (re-cuts both canvases; the transcript is extended when needed).

CLI:

```sh
streamctl query clips '{"status": "ready"}'          # ranked review queue
streamctl query clips.session '{"session": "20260925-200000"}'
streamctl do "clips.approve id=3"
streamctl do "clips.retrim id=3 in=1:02.5 out=1:31"  # recording time (s or m:ss.mmm)
streamctl do "clips.upload id=3"
```

Retention keeps a session folder while it has queued jobs or clips that are unreviewed or
approved-but-not-uploaded (`sessions/<id>/.keep`).

## Hooks (off by default)

Both are argv lists in `[clips]`; the program gets JSON on stdin and answers JSON on stdout.

`rank_command` — e.g. an LLM pass:

```json
in:  {"session": "…", "min_len": 8, "max_len": 60,
      "candidates": [{"key": "p5060000", "score": 2.1, "marker_score": 1.6, "reasons": ["chat"],
                      "labels": [], "in": 51.8, "out": 68.4, "peak": 60.0,
                      "transcript_from": 33.0, "transcript_to": 80.0,
                      "transcript": "Oh my god, did you see that? …", "words": [{"t0": 52.1, "t1": 52.4, "w": "Oh"}, …]}]}
out: {"clips": [{"key": "p5060000", "score": 9.1, "in": 51.5, "out": 66.0, "title": "Best fill ever"},
                {"key": "p5145000", "drop": true}]}
```

Invalid in/out (outside the transcript or the length limits) are ignored; a failing ranker
falls back to the deterministic ranking. `upload_command` gets the clip (`id`, `wide`, `tall`,
thumbnails, `captions`, `title`, `duration`, `score`, …) and may answer `{"url": "…"}`;
`upload_on_approve = true` runs it on approval.

## Settings

```toml
[clips]
auto_process = true
encoder = "auto"            # auto | nvenc | x264
canvases = ["wide", "tall"]
tall_source = "auto"        # auto | recording | crop
tall_crop_center = 0.5
min_len = "8s"
max_len = "60s"
manual_preroll = "30s"
manual_postroll = "8s"
max_clips = 20
nice = 10

[clips.video]    # nvenc_cq, nvenc_preset, x264_crf, x264_preset, max_bitrate, audio_bitrate, loudnorm, loudness_lufs, thumb_width
[clips.captions] # enabled, font, size_wide, size_tall, max_chars_wide, max_chars_tall, margin_wide, margin_tall, color, highlight, outline, uppercase
[clips.whisper]  # model ("small.en", "base.en", "small.en-q5_1", … or a path), language, threads, pad, prompt
[clips.audio]    # drop, transcribe, tracks, roles, mixed
[clips.ranking]  # marker, speech, excitement, length_penalty, clean_edges, keywords
[clips.hype]     # enabled, threshold, release, hold, preroll_min/max, postroll, max_len, merge_gap, settle,
                 # fallback_delay, chat_window, baseline, clip_votes, vote_window, clip_command, emotes, shift, weights
```

A broken `[clips]` keeps the previous settings and logs the error. Preflight shows
`health.clips` (ffmpeg, NVENC, Whisper model).
