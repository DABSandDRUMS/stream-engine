# Recording and clips

`se-clips` (PLAN §18) manages recording, the time-aligned show timeline, and the review queue.
The live hype detector and manual markers identify moments; song and talking passages can also
produce candidates. After the show, the clip job ranks them, cuts wide + tall clips, and lets
you Keep, Skip, Trim, or Upload. A requested-song clip keeps the performance and backing music;
talk clips may omit music. A `Risk of DMCA` badge is information only, never a filter.

## Recording and show data

Set the recording destination on the **Clipping → Recording** screen. The engine writes the
choice to `[recording] dir` in `project.toml` and directs OBS's output to a show folder there.
With `auto = true`, recording starts in preshow/live and stops when you go off air; Start/Stop
recording remains available manually. A changed destination applies to the next recording.
The screen also shows whether OBS is recording, the real file and track layout, and free space.

Each recorded show has video files and `data/` beside them. The session journal is copied into
`data/session/` at sign-off, and a start-time snapshot of project text settings into `data/project/`.
Credential-named settings are excluded or redacted; keep secrets in the OS keyring, not project
text files. Assets and the runtime DB are not copied. `data/show.json` is the versioned time
base, `data/lanes/*.jsonl` are the songs, scenes,
lights, chat, markers and other show context, `data/features.csv` holds sampled values, and
`data/transcript.jsonl` holds timed speech. All `t` fields mean seconds since the first video
file began; later recording files have offsets in `show.json`. The index can be rebuilt from
the original journal. Retention never deletes a recording needed by a pending job or clip review.

```toml
[recording]
dir = "~/Videos/Stream Engine"
auto = true
modes = ["preshow", "live"]
snapshot_project = true

[recording.index]
enabled = true
transcript = true
```

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

Going off air closes the session after OBS has finished its file. Indexing the show and making
clips run in the background (`[clips] auto_process`); long shows can take time to transcribe.
Run the clip job again with `streamctl do "clips.process session=<id>"` (no session = latest
closed one). One clip job runs at a time, niced, and resumes after a restart.

1. **Candidates.** Existing manual/hype markers make windows. Each requested song also makes
   a candidate, plus speech-rich windows from the indexed full-show transcript when present.
   The job caps output at `max_clips`, so it need not render a clip for every song.
2. **Recording time.** Each window maps through its OBS file's master-clock start in
   `sessions/<id>/meta.toml`; a split file has its own offset. Wide and vertical recordings
   may cover different spans.
3. **Transcript.** The indexer transcribes the available speech track across the show with
   Whisper (`small.en` by default, CPU, timed words); the job reuses indexed words and only
   transcribes a candidate window when needed. A mixed band/program track can also be used as
   a best-effort source, flagged because music or lyrics may be mistaken for speech. The model
   is downloaded once and checked by SHA-256.
4. **In/out and ranking.** Talk clips favor clean spoken boundaries. Requested-song clips
   retain musical passages and may align to recorded beats/downbeats; if no reliable grid is
   available, they use a safe time window. Deterministic hype/song/speech scores have an
   automatic omp pass when available (or an explicit `rank_command`); model failure does not
   discard candidates.
5. **Cut.** Wide (1920×1080) and tall (1080×1920) MP4s with NVENC (libx264 fallback) and
   loudness normalization. Talk cuts may drop the isolated music track and burn spoken
   captions. Requested-song cuts keep both music and performance audio, never duplicate the
   combined program track, and skip unreliable lyric captions. Tall uses a tall recording when
   one covers the cut, else a 9:16 crop of wide.
6. **Review.** Clips, thumbnails and captions land in the show's `clips/` folder (legacy shows
   still use `sessions/<id>/clips/`); rows in the `clips` table feed the UI. Progress is exposed
   through `clips.progress`, `clips.job.*`, `clips.done` and `clips.failed`.

### Audio tracks (owner setup)

In OBS: **Settings → Output → Output Mode: Advanced → Recording → Audio Track**, tick separate
tracks for the available engine buses. `obs.setup` routes `se-program` to track 1 and
`se-band`, `se-music`, `se-sfx`, `se-tts`, `se-game` to tracks 2–6. The app reports the actual
tracks it sees; it does not add a mic-only or per-drum track unless the audio system supplies
one. `[clips.audio.roles]` identifies roles from OBS track names, source names and PipeWire
devices. The default `drop = ["music", "program"]` applies to talk cuts; requested-song cuts
keep music and the live performance without duplicating program. When everything is mixed
into a single track, separate removal is impossible and the UI says so.

## Review

**Clipping** master tab: **Recording** selects the OBS output folder and checks health.
**Past streams** opens a show with time-aligned lanes (songs, talk, scenes, modes, lights,
effects, chat activity, hype, markers and existing clips). Choose a time window, click start
and end on a lane, adjust the times, then **Make clip**. The selection must be between the
`min_len` and `max_len` returned by `clips.session` and fit inside one wide recording; the
manual job cuts wide and tall output, transcribes it when appropriate, and adds it to review.
**Make clips** still runs the automatic ranking job. **Clips** lists the review queue as
video cards with reasons, captions, music flag, **Keep**, **Skip**, **Upload**, and **Trim**
(re-cuts both canvases; the transcript is extended when needed).

CLI:

```sh
streamctl query clips '{"status": "ready"}'          # ranked review queue
streamctl query clips.session '{"session": "20260925-200000"}'
streamctl do "clips.make session=20260925-200000 in=1:02.5 out=1:31" # show time (s or m:ss.mmm)
streamctl do "clips.approve id=3"
streamctl do "clips.retrim id=3 in=1:02.5 out=1:31"  # recording time (s or m:ss.mmm)
streamctl do "clips.upload id=3"
```

Retention keeps a session folder and its recording while jobs need them or clips await review or
upload. Decisions and trim changes are saved as feedback beside the show timeline.

## AI ranking

By default, `[clips] auto_rank = true` uses `scripts/rank-clips-omp.py` when both the script and
`omp` are installed. It runs `omp -p` using the operator's model with no tools, project rules
or session history; it sees only bounded candidate context. A failure falls back to deterministic
ranking. Set `auto_rank = false` to use deterministic ranking only, or specify `rank_command` to
override the bundled ranker. The optional upload hook stays off until configured. Hooks are argv
lists: JSON on stdin, JSON on stdout.

`rank_command` accepts:

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
(non-zero exit, unparseable stdout, or still running after `rank_timeout`, default `"120s"`)
falls back to the deterministic ranking and the reason lands in the job report. The engine runs
the argv with its own environment and working directory; only a leading `~/` of the program is
expanded. Candidates missing from `clips` keep their deterministic values; `drop` removes one.

### The bundled LLM ranker

`scripts/rank-clips-llm.py` (Python 3, standard library only; installed as
`/usr/share/stream-engine/scripts/rank-clips-llm.py`) sends the candidates to any
OpenAI-compatible chat completions endpoint (Ollama, llama.cpp, vLLM, OpenAI, …). The prompt
carries each candidate's hype score, reasons, labels, peak, current in/out, transcript window and
its transcript condensed to timed sentences (split like the clip job: `.`/`!`/`?` or a ≥ 0.8 s
pause). The model scores 0–10, picks in/out, titles, or drops each one; the script asks for JSON
mode (retrying without it on HTTP 400) and also digs the JSON out of prose, code fences and
`<think>` blocks.

Every answer is checked before it reaches the engine: known keys only, candidate keys it was
given, finite numbers (score clamped to 0–10), in < out, each snapped to a sentence start/end
within 1 s and padded like the deterministic trim, inside the transcript window, length within
`min_len`–`max_len`. An invalid in/out is left out (score and title still count); an answer with
nothing valid is left out.

| Env | |
|---|---|
| `SE_RANK_URL` | base URL (`http://127.0.0.1:11434/v1`) or the full `…/chat/completions` URL (required) |
| `SE_RANK_MODEL` | model name (required) |
| `SE_RANK_TIMEOUT` | seconds for the whole run, default `90`; keep it below `rank_timeout` |
| `SE_RANK_BATCH` | candidates per request, default `8`; lower it for models with a small context window |
| `SE_RANK_KEY` | API key; unset → keyring (below); no key is fine for local servers |

Missing config, an HTTP error, a timeout, an unparseable reply, or no usable answer at all: one
line on stderr, exit 1, and the engine keeps its deterministic ranking. The key is never printed.

Enable it in `project.toml` (the repo path works too, e.g. `~/Github/stream-engine/scripts/…`):

```toml
[clips]
rank_command = ["/usr/share/stream-engine/scripts/rank-clips-llm.py"]
rank_timeout = "120s"
```

Give the engine's service the environment (the hook inherits it): `systemctl --user edit
stream-engine`, add a drop-in, then `systemctl --user restart stream-engine`. Local Ollama:

```ini
[Service]
Environment=SE_RANK_URL=http://127.0.0.1:11434/v1 SE_RANK_MODEL=qwen2.5:7b-instruct
```

Hosted (key from the keyring, below):

```ini
[Service]
Environment=SE_RANK_URL=https://api.openai.com/v1 SE_RANK_MODEL=gpt-4o-mini
```

Or keep it all in `project.toml` by wrapping it with `env`:

```toml
rank_command = ["env", "SE_RANK_URL=http://127.0.0.1:11434/v1", "SE_RANK_MODEL=qwen2.5:7b-instruct",
                "/usr/share/stream-engine/scripts/rank-clips-llm.py"]
```

A hosted API key goes in the keyring next to the engine's own secrets (service `stream-engine`),
not in the unit file:

```sh
secret-tool store --label="stream-engine clips.rank_key" service stream-engine username clips.rank_key
# paste the key at the prompt; the script reads it with:
secret-tool lookup service stream-engine username clips.rank_key
```

Try it by hand on any JSON in the input format above:
`SE_RANK_URL=… SE_RANK_MODEL=… scripts/rank-clips-llm.py < candidates.json`.

`upload_command` gets the clip (`id`, `wide`, `tall`, thumbnails, `captions`, `title`,
`duration`, `score`, …) and may answer `{"url": "…"}`; `upload_on_approve = true` runs it on
approval.

## Settings

```toml
[clips]
auto_process = true
auto_rank = true            # use omp when installed; false = deterministic-only
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
