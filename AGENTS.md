# Rules for agents working on stream-engine

Two owner requirements outrank everything else in this repository, including user requests for speed,
convenience, and features. Apply both without being reminded.

## 1. Live streams are never interrupted (LAW)

stream-engine produces live broadcasts. Stability is the product. A stream interruption from agent work is
the worst possible outcome. A slow, careful fix is acceptable; a broken stream is not.

### Know whether the show is live before acting

Before any command that could touch output, audio, video, chat, or the queue, run:

```sh
streamctl get show.mode && streamctl get twitch.stream.live
```
Treat the show as **LIVE** when `show.mode` is `live`, `twitch.stream.live` is `true`, the engine is
unreachable, or you are unsure. LIVE is the default assumption.

### Forbidden while LIVE unless the owner explicitly approves that exact action in this conversation

- Restarting, stopping, killing, or debugging the engine (`stream-engine.service`), its CEF host
  (`stream-engine-web`), render/output, PipeWire, or the compositor; crash or failover tests;
  `systemctl --user daemon-reload` followed by restarts.
- Installing or replacing binaries or player pages the running engine uses (`~/.local/bin/stream-engine`,
  `~/.local/share/stream-engine/cef/bin/`, `web/*.js|html` served live). A running engine picks up a new
  web host automatically: that restart counts as an interruption.
- Editing project files in a way that restarts or rewires a subsystem: `[web]`, `[canvas.*]`, devices,
  sources, scenes on program, mixer, audio routing, lights, outputs, recording, `[twitch]` client settings.
- Scene, program, mixer, light, or audio changes; `panic`; `clean`; releasing overrides.
- Enabling DevTools, remote debugging, or extra listeners; changing tunnels, DNS, or firewall rules.
- Queue/player experiments: requesting, seeking, skipping, pausing, or resuming songs.

Build and test freely; **deploy only when not LIVE**. If a fix needs any forbidden step, finish
everything else, report the exact remaining step and its expected interruption, and wait.

### Diagnose without touching the stream

1. Read-only first: `streamctl preflight`, `streamctl get 'health.*'`, `streamctl get '<subsystem>.*'`,
   `streamctl query <name>`, `journalctl --user -u stream-engine.service --since -15min`,
   `~/.local/share/stream-engine/cef/host.log`, the runtime DB opened with `sqlite3 -readonly`.
2. Never run a disruptive command "to see if it helps". Each action needs a stated cause and expected effect.
3. Prefer the narrowest recovery: reconnect one integration, reload one web source, restart one
   non-output helper (`stream-engine-queue.service`, `stream-engine-queue-tunnel.service`). Never escalate
4. Report the evidence, the root cause, what was changed, and what still needs a quiet moment.

### Engineering rules for stability

- **No permanent give-up.** Network, Twitch, YouTube, relay, tunnel, device, and host failures retry with
  bounded backoff forever. A transient failure at boot or mid-stream must heal without a human.
- **Fail visibly.** Every failure mode a viewer or the operator could notice must show in a `health.*`
  key with an actionable plain-language detail, so the UI health panel can alert. No silent failure, no
  error that is only logged.
- **Safety holds lift by themselves** when their cause clears (for example, re-verified YouTube identity).
  Only an explicit operator or mod action may leave something paused or stopped.
- **Isolate failures.** One broken integration (Twitch, YouTube, relay, a web source, a device) must
  never stall the core, render, or other subsystems.
- **Restart-safe.** Engine, CEF host, queue service, and tunnel restarts (and reboots) must restore the
  previous state without operator steps. systemd units use `Restart=always` and
  `StartLimitIntervalSec=0`.
- **Never `fork()` the engine.** Spawn child processes without `pre_exec` hooks (std then uses
  `posix_spawn`). A `pre_exec` forces `fork()`, which write-protects the whole multi-GB engine and
  stalls the render thread: five recorder encoders started that way froze rendering for 90 ms
  (4 dropped frames). Pass descriptors as stdin/stdout, set niceness after spawn, and get
  parent-death kills from `setpriv --pdeathsig KILL`. Measure render `perf.dropped` /
  `perf.frame_ms_max` around any new background work before shipping it.
- **Regression test every reliability fix**, and prove it on the real machine only when not LIVE.
- Update the operator docs for any new failure mode, health key, or recovery path.

### 16R hardware effects are operator-owned

Never disable, mute, lower, reset, or automate the owner's StudioLive 16R reverb or other
hardware FX. Do not write FX bus controls, FX sends, FX returns, or FX-return aux sends.
The live workstation project intentionally excludes them from `[mixer] writable`; never
restore `writable = ["**"]` or broaden permissions to include them. Channel/X-Touch control
must remain independent of hardware effects. Diagnose FX problems read-only; an intentional
FX change requires the owner's explicit approval of that exact change in this conversation.

## 2. YouTube queue account safety

Protect the personal YouTube account's watch history and recommendations.

- NEVER play, cue, preload, preview, or open any queued/requested/test YouTube video under the personal
  **Dabs & Drums** account. This includes muted playback, hidden preloads, smoke tests, debugging, and
  offline/rehearsal sessions. No exceptions for a short test.
- Queue playback may use only the approved **Dabs and Drums Covers** / **Dabs & Drum Covers** Brand
  channel. Identify the actual channel, not merely the Google login or Premium membership.
- Do not switch to the personal channel for queue work, comparisons, or history inspection. Do not change
  or delete personal history, recommendations, subscriptions, or account settings.
- A selected Covers channel in the engine's sign-in window does NOT by itself prove that the off-screen
  embedded player uses Covers. Verify the embedded player's identity without playing a video first. If
  identity is unknown, mismatched, or signed out, do not enqueue or test anything.
- Use only the engine's persistent browser profile for the approved session. Do not fall back to a
  personal Chrome profile or copy personal cookies into it.
- Do not treat Premium, API lookup, or player health passing as channel identity.

The approved embedded channel is `UCz7OyuTD7kJJ6nJHko6r7ZQ` (`@dabsdrumcovers4610`, Dabs & Drum Covers),
delegated session `102722501858912752217`, identified through metadata-only account-menu requests. The
song actor, player page, and native CEF host enforce an exact channel/delegate interlock. Do not bypass
it. While identity is unverified, both player slots stay stopped and no request is accepted. Once the
exact Covers identity verifies again, playback resumes by itself unless an operator or mod paused the
queue (owner decision, 2026-10-02: hands-free recovery after restarts). A successful video-free identity
check is not evidence of eventual YouTube history attribution.
