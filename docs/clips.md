# Recording and clips

`se-clips` (PLAN §18) manages recording, the time-aligned show timeline, and the review queue.
The live hype detector and manual markers identify moments; song and talking passages can also
produce candidates. After the show, the clip job ranks them, cuts wide + tall clips, and lets
you approve, reject, trim, or upload them. The app records the video and audio feeds you select in
Settings; OBS only streams. A complete mixed source stays complete in both song and talk clips.
A `Risk of DMCA` badge is information only, never a filter.

## Recording and show data

Choose video feeds, audio feeds, the destination and automatic recording options in
**Settings → Accounts & app → Recording**. The app captures those sources with FFmpeg into a show folder under
`[recording] dir`; OBS does not record or control these files. With `auto = true`, recording
starts in the configured modes (by default preshow/live) and stops when you go off air.
**Clipping** keeps capture status and manual Start/Stop controls in a compact header above the
recordings library. Settings changes apply to the next recording, not the active file.

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
dir = "~/Videos/Stream Engine"   # one folder per show is made in here
fallback_dir = ""                # master only, when `dir` is missing/unwritable/full at start;
                                 # "" = ~/Videos/Stream Engine (fallback)
min_free_gb = 20                 # cameras stop under 1.5 × this, the master under 1 ×
auto = true                      # every show is recorded unless you turn this off
modes = ["preshow", "live"]
snapshot_project = true
fps = 60                         # every file is constant 60 fps
encoder = "auto"                 # auto (NVENC, software only if NVENC cannot start) | nvenc | software
video = [
  { name = "main", source = "canvas:wide", role = "master", codec = "hevc", cq = 19, max_mbps = 35 },
  { name = "kit",  source = "camera:cam_kit",  role = "iso", height = 1080, mbps = 8 },
  { name = "kick", source = "camera:cam_kick", role = "iso", height = 720,  mbps = 3.5 },
]
audio = [                        # tracks of the master file
  { name = "Mix",   source = "se-band",  format = "pulse" },
  { name = "Music", source = "se-music", format = "pulse" },
]
frames_socket = ""               # use the engine's default frame socket

[recording.index]
enabled = true
transcript = true
```

Without a `[recording]` section the app records the wide canvas as the master with the
`se-band` and `se-music` tracks, at 60 fps, automatically in preshow and live.

**Sources.** `canvas:wide` / `canvas:tall` are the engine's rendered canvases.
`camera:<id>` records a camera source (`sources/<id>.toml`) from the frames the engine already
captures — the device is never opened twice, and recording a camera keeps it captured even when
no scene shows it. Any other video or audio entry is an FFmpeg input format and source, supplied
as arguments rather than shell commands. `recording.sources` offers available choices
(canvases, engine cameras, V4L2 devices, Pulse sources); Settings also accepts a manual
format/source pair. `default` is the system's default audio input, not a promise of a complete
program mix. Multiple audio entries are separately selected tracks; a single mixed input cannot
be split into instruments or voices by naming it differently.

**Roles.** Exactly one video input is the `master`: the canonical show file, carrying every
audio track (lossless FLAC), recorded at constant quality `cq` (lower is better, 0–51) and capped
at `max_mbps`. Every other video input is an `iso`: video-only, at a target `mbps`, optionally
scaled down to `height` (aspect kept, never scaled up). Without `role`, the first `canvas:*`
input is the master (else the first input that is not a camera) and the rest are ISOs, so older
configs with only `name`/`source`/`format` keep working (an old `tall` entry becomes a
video-only ISO; tall clips take their sound from the master). `codec` is `hevc` (default) or
`h264`. `cq`/`max_mbps` apply to the master only, `mbps` to ISOs only; settings that do not
apply are rejected with a plain explanation. Clips are always cut from master files.

**Encoding.** Each video input has its own FFmpeg encoder, so the files never share fate. The
master uses NVENC HEVC (`p5`, constant quality, 2 s keyframes); only when NVENC cannot start at
all does `encoder = "auto"` record the master with software x265 instead (health says so). ISOs
use NVENC at their bitrate from the camera's native YUYV/RGBA pictures and never fall back to
software, because CPU belongs to the stream. Files are Matroska with packets flushed as written
and one-second clusters: a crash or power loss leaves a playable file up to the last second.
Every picture carries its master-clock capture time, so the files line up exactly.

**Failures restart, never give up.** A failing or stalled ISO never stops the master. Any
encoder that fails (input lost, encoder error, stall) is restarted into a new file segment after
1, 2, 4 … up to 60 s, for as long as the show is in an auto mode; a run that lasted a minute
resets the delay. If a recording cannot start at all (no usable folder, no canvas yet), it is
retried the same way. Only **Stop** (or going offline) stops recording and auto-record until the
next mode change or **Start**. Every segment is listed in the session metadata and
`data/show.json` with its own start, so nothing recorded is lost and the timeline stays exact
across gaps. Files are named `<name>-<start>-<n>.mkv`.

**Disk safety.** At start the recording folder must exist (a missing external drive's folder is
never recreated on the system disk), be writable, and have `min_free_gb` free. Otherwise the
master alone records to `fallback_dir` and health warns with the reason. While recording, free
space is checked every 10 s: under 1.5 × `min_free_gb` the ISOs stop, under `min_free_gb` the
master stops too (health fails). Both resume by themselves once space is freed (at 1.2 × the
threshold plus 1 GB). `recording.dir_free_gb` shows the master's filesystem while recording,
and the saved recording destination while idle.

**Changing the recording folder.** The saved folder applies at the next recording start,
including another start in the same session; an old session folder never overrides it.
A recording already running stays on its current drive until stopped. Existing files stay
where they were recorded, and the session metadata retains their individual paths.

**Load protection.** When the render loop drops or runs late frames, its frame rate sinks, or
the master encoder falls behind real time for 3 s, the recorder pauses one ISO — those scaled
below 1080 lines first, later-configured first — and another every 10 s while pressure lasts.
After a minute of calm they return one at a time. An ISO whose own encoder cannot keep up (a
quarter of its pictures dropped, or under 90 % real time, for 5 s) is stopped and retried after
at least 30 s. ISO encoders run at lower CPU priority. The master is never paused for load.
Speed is measured from encoded pictures per second over the last second, and only after an
encoder's first 10 s; ffmpeg's own `speed=` averages from process start and read "behind" at
every start, which used to shed every camera when a show began.

**No render impact from starting encoders.** Encoders are launched with `posix_spawn` (via
`setpriv --pdeathsig KILL`, which still kills them if the engine dies), never by forking the
engine, and receive pictures on stdin. Forking the multi-GB engine five times at once froze
rendering for 90 ms (4 dropped frames) at each recording start; measured off air on the
workstation, starting and stopping the full set (main + 4 cameras) now drops no frames.

**Health.** `health.recording` names what is recording ("Recording main + 2 cameras (2 audio
tracks)"). It warns when an ISO is paused or failing ("camera kick dropped: encoder busy;
retrying in 30 s"), when recording to the fallback folder, or when the master uses the software
encoder; it fails while the master is not recording (restarting or disk full) or a recording
cannot start. `streamctl query recording.status` adds `feeds` — per input `{name, role, source,
state, active, path, bytes, dropped, segments, detail}` with `state` one of `starting`,
`recording`, `retrying`, `shed`, `low_disk`, `stopped` — plus `dir_free_gb`, `fallback`,
`fallback_reason` and `segment` (the master's restart count).

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

Going off air closes the session after the app recorder has finalized its files. Indexing the
show and making clips run in the background (`[clips] auto_process`); long shows can take time to transcribe.
Run the clip job again with `streamctl do "clips.process session=<id>"` (no session = latest
closed one). One clip job runs at a time, niced, and resumes after a restart.

**Nothing runs while you are live.** Indexing, clip jobs (including manual cuts, retrims and
uploads) and the archive wait while the show counts as live: `show.mode` is anything but
`offline` (preshow and rehearsal included), `twitch.stream.live` or `obs.stream.active` is
true, or the recorder is active. If any of those is unknown (for example while the engine
starts) it counts as live. When a show starts in the middle of background work, the work stops
within 2 s — a clip job or index build is cancelled and starts over after the show; an archive
encode is frozen and continues where it was. `clips.job.stage` and `recording.index.progress`
say "waiting for the show to end (…)" meanwhile.

1. **Candidates.** Existing manual/hype markers make windows. Each requested song also makes
   a candidate, plus speech-rich windows from the indexed full-show transcript when present.
   The job caps output at `max_clips`, so it need not render a clip for every song.
2. **Recording time.** Each window maps through its file's app master-clock start in
   `sessions/<id>/meta.toml`; each recording has its own offset, independent of OBS.
   Wide and vertical recordings may cover different spans.
3. **Transcript.** The indexer can transcribe the selected recorded audio across the show with
   Whisper (`small.en` by default, CPU, timed words); the job reuses indexed words and only
   transcribes a candidate window when needed. If drums, backing music and lyrics share a feed,
   the transcript is best-effort and may mistake lyrics for speech. The model is downloaded
   once and checked by SHA-256.
4. **In/out and ranking.** Talk clips favor clean spoken boundaries. Requested-song clips
   retain musical passages and may align to recorded beats/downbeats; if no reliable grid is
   available, they use a safe time window. Deterministic hype/song/speech scores have an
   automatic omp pass when available (or an explicit `rank_command`); model failure does not
   discard candidates.
5. **Cut.** Wide (1920×1080) and tall (1080×1920) MP4s with NVENC (libx264 fallback) and
   loudness normalization. By default both talk and song cuts retain every selected recorded
   audio feed, including complete mixed sources. Talk-track exclusions are explicit opt-in
   `[clips.audio] drop` settings, not deductions from device or track names.
   Talk cuts may burn spoken captions; song cuts skip unreliable lyric captions. Tall uses a tall recording when one covers the
   cut, else a 9:16 crop of wide.
6. **Review.** Clips, thumbnails and captions land in the show's `clips/` folder (legacy shows
   still use `sessions/<id>/clips/`); rows in the `clips` table feed the UI. Progress is exposed
   through `clips.progress`, `clips.job.*`, `clips.done` and `clips.failed`.

### Audio sources and complete mixes

Select the audio feed in **Settings → Accounts & app → Recording**, whether it is an interface input,
a virtual program mix, or another supported FFmpeg source. No specific interface, mixer or
playback output is required. A source already containing drums and backing music is one
complete mix: clipping cannot remove or rebalance its components. Defaults
`[clips.audio] mixed = "keep"` and `drop = []` retain the selected audio, even if its track is
named `band`, `music` or `program`. Separate stems exist only when distinct feeds actually
contain separate audio. Selecting duplicate inputs records them as chosen; the app does not
infer their contents or deduplicate the mix. Check a short app recording with all intended
sounds before a show. OBS's stream meter is not proof of app recording audio.

For a hardware-mixed show, select **one stereo Mix track** from the interface carrying the
desk's main output. Music already entering the desk (including Apple Music) is already in
that recording; do not add another music input or isolated stems.

## After the show: archive

Every recorded show also gets a compact forever copy, about 5–8 GB for a 3-hour show, so the
recording folder can be cleared without losing the show. It runs by itself after the show; no
operator step is needed.

```toml
[recording.archive]
enabled = true
dir = ""                  # "" = "<recording dir>/Archive"; another drive works too
gb_per_hour = 1.8         # size target for the whole show (video + audio + data)
audio_kbps = 128          # Opus, per audio track
iso_keep_days = 14        # camera ISO files are deleted once the clips are reviewed, or after this
min_offline_minutes = 20  # archive work starts only after the show has been offline this long
```

Each show moves through these stages (shown in **Clipping** and `archive.queue`):

1. **wait_offline** — the show has been offline `min_offline_minutes`.
2. **iso_cleanup** — camera ISO files (`role = "iso"`) are deleted once the show's clip job
   finished and no clip awaits review, or `iso_keep_days` after the show. This never holds up
   the archive and is re-checked every minute.
3. **encode** — each master file becomes `main.mkv` (`main-2.mkv`, … when the recorder
   restarted mid-show) in `<archive dir>/<show folder>/`: AV1 (SVT-AV1, 10-bit), frame rate as
   recorded, every audio track as Opus. The video bitrate is
   `gb_per_hour × 8000 / 3600 × 0.98 − tracks × audio_kbps` kbps (1.8 GB/h with two tracks:
   3664 kbps). The encoder runs at the lowest CPU and disk priority on half the CPU cores.
   Background FFmpeg jobs use exec-only `setpriv` → `nice` → `ionice` → optional `taskset`
   launchers, never a `pre_exec` fork of the threaded engine. These require `coreutils` and
   `util-linux`; the actual encoder keeps the child PID and is killed if its parent dies.
4. **verify** — the copy must have the recording's real length (within 1 s), the same audio
   tracks, and decode cleanly at its first and last 2 s. A bad copy is deleted and encoded again.
5. **hold** — the full-quality recording stays in the recording folder until the clips are
   settled (as for ISOs) or `iso_keep_days` have passed, so retrims still cut from it.
6. **package** — `data/` is copied with large files compressed (`*.zst`; `show.json`, lanes,
   `features.csv`, `transcript.jsonl` and `feedback.jsonl` stay readable), and `clips/` as it is.
7. **commit** — `data/show.json`, the session's `meta.toml` (`show.dir`, recordings) and the
   clip rows switch to the archive in one step; `archive.json` records sources, sizes, bitrate
   and verification time.
8. **cleanup** — only now are the hot master files, `data/` and `clips/` deleted.

Restarts resume at the stage a show had reached (a half-written `*.part` file is removed and
redone). A show folder containing `.keep` (or `keep`) is protected: its archive is made and
verified, but its ISO and master files are never deleted while the file is there.

**When something is in the way:** a missing archive folder (unplugged drive; a configured
`dir` is never created, only the default one) or too little free space waits and retries with
backoff while every recording is kept; `health.archive` warns with the reason. Failures retry
with backoff forever; after three in a row `health.archive` fails with the error. Retention
never counts or deletes the archive folder or any folder with `archive.json`, and while the
archive is on it never deletes a show's master recording (see `[retention.recordings]`).

State: `health.archive` (`pass` "archive up to date (N shows, X GB)", `warn` "N shows waiting
to archive" / "archive folder missing: …", `fail` on repeated failures), `archive.queue`
(`[{show, session, stage, state, progress, detail}]` for shows not yet archived),
`archive.dir_free_gb`, `archive.paused`. Actions: `archive.run {show?}` (start now, skipping the
offline wait and the hold; still waits while live; also resumes a pause), `archive.pause`,
`archive.retry {show?}` (no show = every waiting/failed show). Query `archive` lists every
show's row. Events: `archive.done`, `archive.failed`.

## Review

**Clipping** opens the recordings library: browse recorded shows by thumbnail, date, duration,
and clip count. Open a show to see its generated clips as a thumbnail grid. Hover a clip for a
muted preview; click it for focused playback, trimming, context, and review. The grid is for
browsing, not a wall of inline editing controls. Source and destination configuration stays in
**Settings → Accounts & app → Recording**.

The selected show's source timeline is a secondary tool with time-aligned lanes (songs, talk,
scenes, modes, lights, effects, chat activity, hype, markers and existing clips). Choose a time
window, click start and end on a lane, adjust the times, then **Make clip**. The selection must
be between the `min_len` and `max_len` returned by `clips.session` and fit inside one wide
recording; the manual job cuts wide and tall output and adds it to the show's grid.
**Make clips** runs the automatic ranking job. Clip detail offers **Approve**, **Reject**,
**Upload**, and **Apply trim** (re-cuts both canvases; the transcript is extended when needed).

CLI:

```sh
streamctl query recording.status
streamctl query recording.sources
streamctl do recording.start
streamctl do recording.stop
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
