# Web sources (CEF)

Web patches (`kind = "web"`) and the YouTube player page are rendered off-screen by Chromium
(CEF) and delivered to the compositor as CPU video frames, with their audio in the audio graph
(PLAN §4.2, §6.1, §13.3).

## Architecture

The engine never links CEF — it starts and runs without it. `se-web` (inside the engine)
supervises a separate host process, `stream-engine-web` (crate `se-web-host`), which is both the
CEF browser process and Chromium's subprocess helper.

```text
engine (se-web)                                  stream-engine-web
─────────────────                                ──────────────────────────────────────────
supervisor task ── commands (SEQPACKET, JSON) ─▶ one off-screen browser per source
IPC thread ◀─ events + memfds (SCM_RIGHTS) ───── on_paint → sealed memfd, 3 BGRA frame slots
  frame slot ── copy ─▶ hub.video[<slot>]         AudioHandler → shared-memory f32 ring
  audio ring ── copy ─▶ hub.audio[<slot>]         renderer crash → report + reload (backoff)
```

- **Video:** `on_paint` (CPU paint path) copies the full view into a free slot of a sealed memfd
  (3 slots, one being copied by the engine); the engine copies it into the `hub.video` slot as
  `Bgra8` (**premultiplied alpha**, upper-left origin, `stride = width × 4`) stamped with the paint
  time on the master clock, then hands the slot back. Pages without a background are
  transparent.
- **Audio:** CEF is asked for 48 kHz stereo; packets are interleaved into a shared-memory
  single-producer/single-consumer ring and moved into the `hub.audio` slot (interleaved `f32`,
  2 ch, 48 kHz, 0.5 s buffer). With an audio handler Chromium plays nothing to the speakers.
- **Chromium setup:** `--ozone-platform=headless` (no display server needed), WebGL and
  compositing on the RTX 3070 through ANGLE on Vulkan (`--use-angle=vulkan`), `--no-sandbox`,
  `--autoplay-policy=no-user-gesture-required`, background throttling off, no media-key/MPRIS
  integration, `--password-store=basic` (cookies never depend on an unlocked keyring).
- **Isolation:** a renderer crash is reported and the page reloads (0.25 s, doubling up to 30 s
  when a page keeps crashing). If the host dies or stops answering pings, the engine restarts it
  (after 0.25 s, doubling up to 30 s; reset after 30 s of stable running). Video slots keep their last
  frame meanwhile; the engine is unaffected. Shared memory is sealed against shrinking and
  validated before mapping, so a broken host cannot fault the engine.

## Install

```sh
scripts/install-cef.sh            # release build; --debug for a quicker debug build
```

The script builds `se-web-host` with `CEF_PATH=~/.local/share/stream-engine/cef` — the `cef`
crate's build script downloads the matching CEF minimal distribution (CEF 154.0.23, Chromium
154.0.8037.17, ≈300 MB download) into `~/.local/share/stream-engine/cef/154.0.23/cef_linux_x86_64/`
— and installs the host with the runtime files (libcef.so stripped from 1.4 GB, resources,
locales, CEF license and Chromium credits) into `~/.local/share/stream-engine/cef/bin/`.

The engine looks for the host in this order (a candidate counts only with `libcef.so` beside
it) and starts it with `LD_LIBRARY_PATH` set to that directory (the binary also has
`RUNPATH=$ORIGIN`):

1. `$STREAM_ENGINE_WEB_HOST`
2. next to the engine executable (development: `target*/debug/stream-engine-web`, where
   `cef-dll-sys` copies the runtime)
3. `<share>/../../lib/stream-engine/stream-engine-web` (packages: `/usr/lib/stream-engine/`)
4. `~/.local/share/stream-engine/cef/bin/stream-engine-web`

The host is started when the first web source appears and stopped 30 s after the last one goes
away. Installing while the engine runs is picked up automatically.

## Sources

| Source | Slot (video + audio) | URL | Token scope | Size | fps |
|---|---|---|---|---|---|
| enabled `kind = "web"` patch | `patch.<id>` | `http://localhost:<http port>/patches/<id>/<entry>?token=…` | `patch.<id>.*` | manifest `size`, else overlays: largest canvas, else largest scene node | manifest `fps` (default 60, CEF max 60) |
| YouTube player page | `youtube` | `http://localhost:<http port>/web/player.html?token=…` | `patch.youtube.*` | `[web.youtube] size`, else largest node | 30 |

- Pages load from `localhost`, never an IP literal: YouTube's IFrame player rejects every video
  with error 150 ("embedding disabled") when the embedding origin is `http://127.0.0.1:…`
  (verified 2026-09-25). The engine can stay bound to 127.0.0.1; Chromium falls back to IPv4.
  Cookies in the profile are keyed to this origin.
- Each page gets its own random 32-byte token (registered with the API as `web.<id>` /
  `web.youtube`, revoked when the source closes); `engine.js` picks it up from `?token=`.
- **Size** = the largest node (by area) whose `src` is the slot, across every scene and canvas:
  `rect[2] × canvas width` by `rect[3] × canvas height`, rounded to even. Scene edits resize the
  page live. Default 1920×1080.
- A patch file change (`generation`) reloads the page bypassing the cache; disabling or removing
  a patch closes its browser and publishes one transparent frame so the slot stops showing it.
- The player page is opened only if `<share>/web/player.html` exists or a URL is configured.

`project.toml`:

```toml
[web]
gpu = true              # false = software rendering (--disable-gpu), e.g. to rule out GPU issues
devtools_port = 9222    # optional: Chromium remote debugging on 127.0.0.1 (restarts the host)

[web.youtube]
enabled = true
url = "/web/player.html"  # path on the engine (token added) or an absolute URL (no token sent)
fps = 30
size = [1280, 720]
```

## Status, queries, actions

| Address / query | Meaning |
|---|---|
| `health.cef` | `pass` (host running, or installed and idle), `warn` (not installed: "CEF runtime not installed: run scripts/install-cef.sh"; starting; restarting; sign-in window open), `fail` (host keeps failing) |
| `patch.<id>.error` | `""` when the page loaded; `load failed: <url> (<error>, <code>)`, `HTTP 404 loading <url>`, `renderer crashed (<reason>) — restarted`, `CEF host exited (<status>) — restarting`, or the install hint. Crash messages stay 30 s after recovery. The patch loader derives `patch.<id>.state` from it. |
| `web.<slot>.fps` | measured paint rate (static pages paint only when they change) |
| `web.<slot>.crashes` | renderer crashes since the engine started |
| `web.<slot>.status` | `starting · loading · running · error · crashed · restarting · paused · unavailable` |
| `web.<slot>.error` | same text as `patch.<id>.error` (also for `youtube`) |
| query `web` (`stream query web`) | `{host: {state, pid, cef, chromium, gpu, installed, exe, restarts, health}, sources: [{slot, url (token redacted), size, frame_size, fps, target_fps, status, error, crashes, frames, latency_ms, audio_samples}]}` |
| `web.reload [slot]` | reload one source (or all), bypassing the cache — `stream do web.reload patch.aurora` |
| `web.login [url]` | open the sign-in window (below) |

Page `console.error` messages appear in the engine log as `web: <slot>: console: …` (at most 20
per 10 s per page).

## Profile and YouTube sign-in (§13.3)

The CEF profile is persistent and private: `<data dir>/cef/profile` (default
`~/.local/share/stream-engine/cef/profile`, mode 0700) holds cookies (session cookies are kept),
local storage, and the HTTP cache. Third-party cookies are allowed
(`profile.cookie_controls_mode = 0`, `profile.block_third_party_cookies = false`, cookie content
setting *allow* by default and for youtube.com / google.com), and third-party storage
partitioning is off, so the youtube.com iframe inside the player page is signed in — required for
YouTube Premium to remove ads.

Sign in once:

```sh
stream do web.login                      # or: stream do web.login url=https://www.youtube.com/
```

This stops the off-screen host (web sources keep their last frame, status `paused`), and opens a
normal Chrome-style window on the same profile (X11 through XWayland when `DISPLAY` is set, else
Wayland). Sign into the Google account, check that youtube.com shows the account (and Premium),
then **close the window**: the cookies are saved and the web sources restart automatically.
The engine needs `DISPLAY` or `WAYLAND_DISPLAY` in its environment (systemd user service:
`systemctl --user import-environment DISPLAY WAYLAND_DISPLAY`).

The profile directory contains the signed-in Google session: treat it like a browser profile
(do not copy it around; delete it to sign out everywhere).

## Troubleshooting

- **`health.cef` warns "CEF runtime not installed"** — run `scripts/install-cef.sh`, or point
  `STREAM_ENGINE_WEB_HOST` at a built host that has `libcef.so` next to it.
- **Host keeps restarting / `fail`** — read `<data dir>/cef/host.log` (Chromium's warnings and
  errors, rotated at 16 MB) and the engine log (`se_web::host` lines are the host's stderr;
  run the engine with `RUST_LOG=se_web=debug` for everything). Check the install with
  `~/.local/share/stream-engine/cef/bin/stream-engine-web --se-version`.
- **"CEF initialization failed (is another stream-engine-web using the profile…)"** — only one
  CEF process may use a profile: a stray host from another engine instance with the same data
  dir, or a still-open sign-in window.
- **Blank or black WebGL pages** — set `[web] gpu = false` to compare with software rendering;
  GPU process problems show up in `host.log`.
- **A page's error says `load failed … (-102)`** — Chromium net error codes
  (`-102` connection refused, `-105` name not resolved, `-6` file not found); `HTTP 404` means
  the entry file is missing from the patch folder.
- **Page debugging** — set `[web] devtools_port = 9222` and open `http://127.0.0.1:9222` (or
  `chrome://inspect` in Chrome/Chromium) to get DevTools for every off-screen page.
- **Google refuses the sign-in** ("this browser or app may not be secure") — sign in through
  youtube.com's own *Sign in* button in the window rather than a deep link; the window is a
  regular Chrome-style browser with the stock Chromium user agent.
- **No sound from a page** — web audio never reaches the speakers directly; it goes to the
  `patch.<id>` / `youtube` audio slots and from there through the audio graph (`music` bus for
  the player).

## Future: shared-texture (zero-copy) path

Today frames go GPU → CPU (`on_paint`) → shared memory → hub slot → GPU upload. On NVIDIA,
CEF's accelerated off-screen rendering (`WindowInfo.shared_texture_enabled` +
`RenderHandler::on_accelerated_paint`, which hands out dmabuf planes) does not work until CEF
ships chromiumembedded/cef PR #4238 (expected around M156, PLAN §4.2). When a CEF build with it
lands: keep ANGLE on Vulkan (`--use-angle=vulkan`; `gl-egl` is the documented alternative),
enable `shared_texture_enabled`, pass the dmabuf fds of `on_accelerated_paint` over the same
socket (`SCM_RIGHTS`), and import them in `se-render` like camera dmabufs — no CPU copies, and
frame rates above 60 become possible with `external_begin_frame_enabled`.
