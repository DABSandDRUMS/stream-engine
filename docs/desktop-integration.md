# Desktop integration (Arch / Omarchy)

How stream-engine is installed and wired into the desktop (PLAN §16). Everything under
`~/.config` is opt-in: the package never touches it.

## Install the package

Release builds use `packaging/PKGBUILD` (source: the `v$pkgver` tag on GitHub). To build the
current working tree instead, without installing anything:

```sh
packaging/build-local.sh          # → target-pkg/stream-engine-<ver>-<rel>-x86_64.pkg.tar.zst
sudo pacman -U target-pkg/stream-engine-*.pkg.tar.zst
```

`build-local.sh` snapshots tracked + untracked, non-ignored files, stages PKGBUILD + tarball in a
temp dir, writes the checksum, and runs `makepkg -f`. When cargo/cmake come from rustup/mise
(invisible to pacman) it passes `--nodeps` after checking the tools are on PATH. Useful
environment: `CARGO_TARGET_DIR` (default `target-pkg/cargo`, incremental between runs),
`CEF_PATH` (default `~/.local/share/stream-engine/cef`, the CEF download cache),
`SE_PKG_ALLOW_MISSING=obs,cef,scripts` (package without a component that has not landed; a
component that is present but fails to build always fails the build).

The package installs:

| Path | What |
|---|---|
| `/usr/bin/stream-engine`, `/usr/bin/streamctl` | engine/UI binary and CLI |
| `/usr/bin/stream-engine-launch-or-focus` | focus the UI window by exact app-id, or launch it (`--program` for the confidence window) |
| `/usr/share/stream-engine/{web,project-example,templates,scripts}` | engine data (`share_dir()` = `<exe>/../share/stream-engine`) |
| `/usr/share/stream-engine/omarchy/` | the Omarchy extras below, with their installer |
| `/usr/lib/stream-engine/` | CEF web host `stream-engine-web` + CEF runtime (found via `<share>/../../lib/stream-engine`) |
| `/usr/lib/obs-plugins/stream-engine.so`, `/usr/share/obs/obs-plugins/stream-engine/` | OBS plugin |
| `/usr/lib/systemd/user/stream-engine.service` | engine user service (`Type=notify`, watchdog, `Restart=always`) |
| `/usr/lib/udev/rules.d/70-stream-engine.rules` | device access, see below |
| `/usr/share/applications/stream-engine{,-program}.desktop`, hicolor icon | launchers (`StartupWMClass` = `stream-engine` / `stream-engine.program`) |

For a development install into `~/.local` use `packaging/dev-install.sh`.

## Enable the engine

Open **Stream Engine** from the app launcher. If the engine isn't running yet, the window says
so and its **Start Stream Engine** button runs `systemctl --user enable --now stream-engine`
(the same thing by hand). On its first start with no project configured, the engine creates the
starter project `~/stream-project` from `/usr/share/stream-engine/project-example` (or adopts a
project already there) and saves `project = "…"` in `~/.config/stream-engine/engine.toml`. Then
Settings → **Get started** walks through cameras, Twitch and OBS.

New projects are **blank production workspaces**, not demo shows. The starter keeps camera and
device declarations, canvas/output setup, neutral audio routing, safety limits, unassigned
controllers, and screen layouts. MIDI declarations do not assume an e-drum map. Fixture profiles
are a library, not installed lights; DMX output and RDM discovery start disabled, and lighting
panic defaults to blackout until explicitly configured. No
scenes, presets, overlays, patches, rules, bindings, timelines, alerts, rewards, chat commands,
lighting looks/effects/cue lists, audio inserts, or automatic ducking are installed. Recording,
indexing/transcription, clip processing/ranking, rehearsal simulation, voice/TTS and automatic
ad-break mode changes are opt-in. Create show content explicitly in its editor; patch templates
remain available through **New patch** and `patch.new`. Existing projects are adopted as-is,
never reset by first-run setup.

To use another folder: `stream-engine new <dir>` and set `project = "<dir>"` in
`~/.config/stream-engine/engine.toml` before the first start. `streamctl status` checks the
engine from a terminal.

The UI is launched on demand (`stream-engine ui`, the desktop entry, the bar widget, or the
keybind); closing it never affects output.

The native Rust UI uses shared semantic theme tokens: neutral page/card/sidebar surfaces,
consistent 6-point control and 8-point card radii, outlined secondary actions, and
high-contrast primary actions. Omarchy still supplies the live accent, light/dark mode,
and font; color is reserved for focus, selection and meaningful status rather than every
button. The component hierarchy follows [shadcn's theme-token model](https://ui.shadcn.com/docs/theming)
without adding a web frontend.

## Going live

**Go live** in the top bar first runs every preflight check (the same list as
`streamctl preflight`: devices, cameras, OBS, Twitch, sound, lights, disk space, graphics card,
screen lock, night light, …) and shows it in plain words, problems first, each with a **Fix**
button. The two start buttons ("Starting-soon screen first", "Go live right now") only work when
everything is green — optional extras that aren't set up (song requests, tips, TikTok) don't count
— or after switching on **Go live anyway**. The list refreshes every few seconds while it's open.
Starting sets the show mode first and then starts OBS, so going live from rehearsal works.

## Notifications

Background problems show up as desktop notifications (freedesktop, via `notify-send`; the
Omarchy shell is the notification daemon) — only while no Stream Engine window has focus, and
never for routine events:

| Problem | When |
|---|---|
| OBS stopped getting the main / vertical video | OBS is open, the feed has been stale for 5 s, and it matters (streaming, recording, or a show mode other than off air / rehearsal) |
| "*device*" was disconnected | an expected device (`[devices.expected]`) that was present went away for 3 s |
| Twitch (or the chat bot) needs you to sign in again | the sign-in expired or failed, or renewing keeps failing, for a minute |
| The disk is almost full | under 10 GB free on the project disk |
| Backups / recordings disk / recordings budget | asked for by the backup and retention tasks ([extras.md](extras.md#backups-and-retention)) |

Each problem is notified once per occurrence; the same problem isn't repeated within 30 minutes,
and notifications are at least 30 s apart (problems that come up in between arrive together as
"N things need your attention"). A problem that starts while a Stream Engine window is in front
counts as seen. Clicking a notification opens (or focuses) the UI on the page that fixes it.

The UI reports its focus with the action `ui.focus {focused, pid}` whenever it changes and after
every reconnect; a UI that has exited, or no UI at all, counts as not focused (`ui.focused`
state). Other subsystems, presets or rules can ask for one with
`notify.send {title, body?, key?, urgency = normal|critical, open?}` (same rules; `open` is the UI
page to show on click, e.g. `devices`). Every notification is also the event `notify.sent`.
Notifications are part of the `extras` subsystem (`SE_DISABLE=extras` turns them off in
development).

## Device access (udev)

`70-stream-engine.rules` gives the logged-in user (`TAG+="uaccess"`, mode 0660) access to:

- every Elgato Stream Deck (vendor `0fd9`, e.g. Original V2 `0fd9:006d`) hidraw node;
- the ENTTEC DMX USB PRO (FTDI `0403:6001` whose product string is `DMX USB PRO` or whose
  serial starts with `EN`), plus the stable symlink `/dev/dmx-usb-pro`; ModemManager is told to
  leave it alone.

pacman's udev hook reloads the rules and re-triggers attached devices on install. After a
manual copy: `sudo udevadm control --reload && sudo udevadm trigger`.

## Omarchy extras

```sh
/usr/share/stream-engine/omarchy/install.sh --dry-run     # show the plan
/usr/share/stream-engine/omarchy/install.sh               # plugin + menu + font hook
/usr/share/stream-engine/omarchy/install.sh --bar --hypr  # also place the widget and keybinds
```

(From a checkout: `omarchy/install.sh`.) Each changed file is backed up as
`<file>.bak.<timestamp>`; re-running is idempotent. The Omarchy shell and Hyprland watch these
files, so nothing needs restarting.

- **Bar widget** (`~/.config/omarchy/plugins/stream-engine.status/`): on-air tally (theme red
  when the show is live or OBS is streaming), show mode and engine uptime, preflight
  warn/fail count, pending approvals (song requests awaiting approval, alerts in their veto
  window, AutoMod-held messages, policy approvals). Left click opens/focuses the UI, middle
  click the program window, right click a details popup. It runs one
  `streamctl --json watch` for live state, polls `streamctl --json query engine.info` while the engine
  is down, and refreshes `streamctl --json preflight` every 30 s. Settings (shell.json entry):
  `streamCommand`, `socket`, `openCommand`, `preflightIntervalSec`, `retryIntervalSec`,
  `hideWhenOffline`. Place it with `--bar` or `omarchy plugin enable stream-engine.status`.
- **Menu** (`~/.config/omarchy/extensions/omarchy-menu.jsonc`): a *Stream* submenu with Open UI,
  Program Window, Go Live, BRB, Layout (single / show-2disp / show-3disp via
  `streamctl fire ui.layout name=…`), Preflight (in a terminal), and Panic. Existing entries with
  the same ids are never overwritten.
- **Font hook** (`~/.config/omarchy/hooks/font-set.d/stream-engine`): on `omarchy font set` it
  fires `omarchy.font_set {font}` so the UI switches fonts immediately; a no-op when the engine
  is not running.
- **Keybinds and window rules** (`--hypr`: `~/.config/hypr/stream-engine.lua`, required from
  `bindings.lua`): SUPER+CTRL+ALT + S (open UI), RETURN (Take), ESCAPE (Panic), C (Clean),
  B (BRB; replaces Omarchy's "Show battery remaining"), N / P (next / previous scene to Up next). Windows: `stream-engine` on DP-1,
  `stream-engine.<panel>` on DP-2, `stream-engine.program` fullscreen on the TV (DP-2 while the
  TV is absent) with `idle_inhibit = "always"`; all fully opaque. Edit the monitor names at the
  top of the file for other setups.
