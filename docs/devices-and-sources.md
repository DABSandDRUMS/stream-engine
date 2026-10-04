# Devices and video sources

Operator notes for the device registry (`se-devices`, PLAN §3.5) and camera/media sources
(`se-video-in`, PLAN §4.2).

## Manage sources in the app

Open **Sources → Sources** to make, edit, or remove sources: cameras and capture cards, media
files (videos, pictures, GIFs), web pages and generative visuals. A source is a saved project
definition; a discovered device is only an available choice. Discovery does not create source
files or add scene layers.

Choose **+** (or **New source**), pick a kind, set it up, then **Create**. Camera setup uses the
device's stable identity and supported format, size and frame rate; manual device entry
remains available. Media sources expose looping and speed. Changes to a saved source keep its
unrelated color, control or effect settings. **Add to scene…** places it as a new layer.
**Sources → Files → Make a source** starts a draft from a picture or video.

**Remove** (hold) deletes the source definition, not the underlying media file or physical
device. If a scene still uses it, the page names those scenes and removal waits until it is
taken out of them. A missing or unplugged source remains editable and removable. Removing a
web page or generative visual moves its folder to `patches/.removed/<id>-<time>/`
(`patch.remove`).

Audio capture inputs belong in **Sound → Mix → Manage inputs**. Recording source selection
belongs in **Settings → Accounts & app → Recording**; it is independent of scene source setup.

### Saved-source API

`project.sources` lists saved source definitions with origin paths, scene references and
parse errors, independently of video capture. `sources` remains the running video inventory.

- `project.source.save {name, create?, label?, device, format, size, fps}` saves a camera.
- `project.source.save {name, create?, label?, file, loop?, rate?}` saves a media-file source
  (`file`: a video, or a PNG/JPEG/WebP picture or GIF, in `assets/`).
- `project.source.remove {name}` removes an unused source definition.

Use `create: true` for a new source; edits require an existing name. Names and source types
stay fixed on edit. New media selections must be video assets inside the project. Success
emits `project.sources.changed {name, action}` after persistence and reload; failure emits
`project.source.failed {name, error}` without discarding the editor's draft.


## Stable identities

Every device gets an identity that survives reboots and replugging:

| Kind | Identity | Example |
|---|---|---|
| `camera` (V4L2 capture node) | udev `by-id` name (USB serial) else `by-path` name | `usb-Sonix_Technology_Co.__Ltd._USB_Camera_SN0001-video-index0`, `pci-0000:05:00.0-video-index2` (VC42 input 3) |
| `audio_card` (ALSA) | udev `ID_ID` (`/dev/snd/by-id`) else port path | `usb-PreSonus_Studio_24c_SC1M19120661-00` |
| `midi` (ALSA rawmidi) | card identity + `-midi<N>` | `usb-Behringer_X-TOUCH_MINI_1.0.1-00-midi0` |
| `hid` (hidraw) | `usb-<serial>-if<NN>` (+ `-hid-<vid>:<pid>` for nested HID devices) | `usb-Elgato_Stream_Deck_AL46J2C62768-if00` |
| `serial` (USB serial) | `/dev/serial/by-id` name | `usb-ENTTEC_DMX_USB_PRO_EN405589-if00-port0` |
| `audio_node` (PipeWire) | `node.name` | `alsa_input.usb-PreSonus_Studio_24c_SC1M19120661-00.analog-stereo` |
| `ucnet` (PreSonus StudioLive on the network) | `ucnet-<serial>` | `ucnet-RA1E24110101` |
| `artnet` (Art-Net node on the network) | `mac-<MAC>` from its ArtPollReply, else `artnet-<ip>` | `mac-00:50:c2:12:34:56` |

The AVMatrix VC42 exposes one PCI device with four capture nodes; its inputs are told apart by
PCI path + input index, so `pci-0000:05:00.0-video-index0` is always HDMI 1.

`streamctl query devices` prints the full registry: identities, device nodes, USB ids, and for
cameras every format × size × frame rate (DV timings for HDMI inputs), the current format,
the HDMI input status, and the control names.

### Local HWS capture-driver repair

On the show machine, the distribution HWS driver from Linux 7.2.5 produced repeated
`AMD-Vi: IO_PAGE_FAULT` writes from `0000:05:00.0`. The repair sources are in
`~/.local/src/hws-stream-fix-7.2.5.1`; `upstream.json` records the pinned kernel
and vendor reference sources. This is a local driver patch, not an upstream release.

The patch keeps a coherent DMA ring alive for each HDMI input for the entire device
lifetime. A threaded IRQ copies the first and second hardware halves into CPU-backed
V4L2 buffers, publishing only a complete pair. Consumer queue rotation never changes
the card's DMA translations. STREAMOFF drains IRQ completion before returning buffers;
device removal disables PCI bus mastering before freeing the rings. The additional CPU
copy is intentional: it removes the unsafe direct-to-consumer DMA lifetime.

The VC42 uses one managed MSI vector instead of legacy INTx. The legacy route stalled
under sustained consumer frame copies: all four completion bits stayed pending while
Linux reported the IRQ enabled, unmasked and inactive. A module reload or isolated
PCIe bus reset did not sustain capture. MSI passed the same four-input copy/starvation
regression and restored advancing engine inputs. The exact legacy transport failure
is not established; do not interpret this as proof of a GPU or camera-power fault.
The IRQ thread acknowledges its observed completion flags before copying so a new
half arriving during the copy retains its own pending bit. The DMA-ring lifetime
repair and copy-consistency checks remain in place.

IRQ numbers change with MSI allocation: identify the line by `0000:05:00.0`, not a
hard-coded IRQ24. Kernel startup should report `IRQ mode: threaded MSI`.

Install and reload **off-air**, with Stream Engine and every other capture client stopped:

```sh
systemctl --user stop stream-engine.service
sudo cp -r --no-preserve=ownership ~/.local/src/hws-stream-fix-7.2.5.1 /usr/src/
# Fresh registration only: skip `dkms add` when this package is already registered.
sudo dkms add -m hws-stream-fix -v 7.2.5.1
sudo dkms build -m hws-stream-fix -v 7.2.5.1 -k "$(uname -r)" --force
sudo dkms install -m hws-stream-fix -v 7.2.5.1 -k "$(uname -r)" --force
sudo modprobe -r hws
sudo modprobe hws
sudo udevadm settle --timeout=15
modinfo -F filename hws
cat /sys/module/hws/srcversion
modinfo -F srcversion hws
```

The selected module should be under `updates/dkms`, and its `srcversion` must match
the running module. Installation alone does not replace an already-loaded driver.
DKMS rebuilds the patch on kernel updates; a successful rebuild is not proof that a
new kernel preserves the hardware behavior.

Before restarting the engine, run the real-device regression:

```sh
cd ~/.local/src/hws-stream-fix-7.2.5.1
make check
cc -O2 -std=gnu11 -Wall -Wextra -Werror -pthread tests/capture-cycle.c -o /tmp/hws-capture-cycle
cursor=$(journalctl -k -b -n 1 -o json --no-pager | jq -r '.__CURSOR')
/tmp/hws-capture-cycle /dev/video0 /dev/video1 /dev/video2 /dev/video3
journalctl -k -b --after-cursor "$cursor" --no-pager -g 'IO_PAGE_FAULT|IRQ storm|NVRM'
rm /tmp/hws-capture-cycle
systemctl --user start stream-engine.service
```

Use the current HWS device nodes, not arbitrary USB camera nodes. The hardware check
captures 2,160 frames per connected input across six STREAMON/STREAMOFF, unmap and
reallocation cycles, alternating normal queues with deliberate single-buffer starvation.
Each frame is copied into a reusable consumer allocation before requeueing. This load
reproduced the legacy IRQ stall that the earlier 120-frame, metadata-only cycles missed.
It queues only one buffer during starvation even if VB2 allocates a spare. An input
reporting NO_SIGNAL instead exercises 18 fallback frames across the same six cycles;
there is no physical frame clock to validate on an unplugged input. The check rejects
error frames, invalid payload bounds, non-monotonic completions and an incorrect
whole-frame rate on connected inputs. Require no new capture DMA faults, then inspect the actual camera
images and exercise the engine/OBS workload off-air. A short clean run does not
establish long-duration whole-PC stability or prove that the independent NVIDIA
mapping failures have the same cause.

The installed/running MSI + early-ack revision has `srcversion`
`C4CCD7CD8FD55205C3AEDE5`. Its off-air check included fresh decoded pixels from all
four leased previews, both OBS DMA-BUF canvases, a 258.65 s app HEVC 1080p60 master
with exactly one 48 kHz stereo FLAC mix track, NVENC p5 loopback streaming, native
GPU browser windows and the running DMX transport. The eight steady-load samples
kept BAR1 at 134/256 MiB used, CPU frame time 2.53–5.56 ms and GPU time 2.79–4.88 ms.
Renderer dropped/late/recovery counters did not increase; the recorder's one
already-counted dropped frame did not increase during sampling. OBS added two
render misses over 14,664 frames and no encoder skips. Both canvases had zero
producer-fence timeouts; DMX ran at 44 fps with 0.28 ms p99 jitter. No new capture
DMA, NVIDIA or kernel-fault messages appeared through owned-client teardown.
The bounded local receiver expired and induced an expected OBS reconnect before
the test stream was stopped; this was not a Twitch/uplink test.

Kit capture measured roughly 54–58 fps during those load samples, even though
the canvas and recording run at 60 fps; do not claim four lossless 60 fps inputs
from configured rates or zero app-side drop counters. Apple Music's existing
16R main-mix route, hardware FX and routing were not changed. The app still
records only the Studio24c stereo mix, without a second Apple Music capture.

Device presence and a successful DV-timing query are not capture-liveness checks.
The local driver can fall back to cached geometry/frame rate. Inspect
`source.<name>.fps`, `source.<name>.signal` and the `frames` / `measured_fps`
fields in `streamctl --json query sources`; the query's `fps` field is configured,
not measured. If frame counts and the card's `/proc/interrupts` count stop advancing,
confirm continuous HDMI output physically before treating this as an app problem.
A channel-level `source.reopen` does not reset the shared card core or interrupt gates.
Any card reset/module reload requires an approved off-air interruption, all capture
clients stopped and administrator authentication. Do not add repeated reopen loops
or roll back to the DMA-unsafe distribution driver to conceal a stalled capture path.

If a driver revision fails, restore only a previously verified local module with the
coherent-ring lifetime repair, with capture clients stopped and administrator approval.
Do not roll back to the distribution HWS driver: its observed DMA faults can crash the
workstation. Keep IOMMU protection enabled.

## Expected devices (preflight)

`project.toml`:

```toml
[devices.expected]
vc42_1 = { kind = "camera", label = "VC42 HDMI 1 (kit)", identity = "pci-0000:05:00.0-video-index0" }
deck   = { kind = "hid", label = "Stream Deck Original V2", usb = "0fd9:006d" }
dmx    = { kind = "serial", label = "ENTTEC DMX USB PRO", identity = "usb-ENTTEC_DMX_USB_PRO_*" }
```

Criteria (all given ones must match): `identity` (glob), `usb` (`vendor:product` hex), `serial`,
`port` (udev `ID_PATH`, glob), `name` (case-insensitive glob), `mac` (network devices; `:`/`-`/no
separators). `optional = true` makes a missing device a warning instead of a failure. The more
specific entry wins when several could match.

Network devices are declared the same way:

```toml
[devices.expected]
desk  = { kind = "ucnet", label = "StudioLive 16R", serial = "RA1E24110101" }
truss = { kind = "artnet", label = "Truss node", mac = "00:50:c2:12:34:56" }
```

State: `devices.<id>.{present,name,kind,path,identity}` (`<id>` = the expected key, or a slug of
the identity for undeclared devices). Preflight: `health.devices` fails when an expected device
is missing. Events: `devices.added` / `devices.removed` `{id, kind, name, identity, path}`.

Actions: `devices.expect identity=<identity> [id=…] [label=…]`, `devices.rename <id> <label>`,
`devices.forget <id>`, `devices.rescan` (also sends an ArtPoll right away). They edit
`project.toml` in place (comments kept).

## Network devices

The registry also lists devices found on the LAN, with the same state, events and preflight as
plugged-in ones:

- **PreSonus StudioLive consoles (UCNET)**: consoles announce themselves every 3 s (UDP 53000 →
  broadcast port 47809). The engine listens passively; the port is shared (`SO_REUSEPORT`) with
  the mixer adapter and any other listener, since broadcasts reach all of them. The identity is
  the console serial (the announcement carries no MAC); `extra.mac` shows the MAC when the
  kernel neighbour table knows it, `extra.model`/`extra.ip` the rest. `path` is `ip:port`.
- **Art-Net nodes**: an ArtPoll goes out every 3 s from UDP 6454 to 255.255.255.255 and each LAN
  interface's broadcast address; nodes answer with ArtPollReply (name, IP, firmware, MAC). Extra
  replies of multi-port nodes (bind index 2+) fold into the root device.
- A device not heard for 10 s (three missed announcements/polls) is removed. Devices heard in
  the first 5 s after start were already there: no `devices.added` event for them.

Port 6454 is bound **exclusively**: with a shared port the kernel gives each unicast reply to
only one socket, so a second engine (a dev build) would silently take the live engine's replies.
The first program to bind keeps Art-Net discovery; any other reports "UDP 6454 is in use" and
retries every poll. se-dmx sends ArtDmx from an ephemeral port and is unaffected. If another
Art-Net program on this computer needs port 6454, turn discovery off:

```toml
[devices.network]
artnet = false   # frees UDP 6454
ucnet  = true
```

The `devices` query reports discovery status under `network.{ucnet,artnet}` (`ok`, `detail`).
A default-deny host firewall (ufw on the show machine) drops both protocols' inbound datagrams;
to let them in:

```sh
sudo ufw allow in proto udp from any port 53000 to any port 47809 comment 'PreSonus UCNET discovery'
sudo ufw allow in proto udp from any port 6454 to any port 6454 comment 'Art-Net'
```

## Live thumbnails (Settings → Devices)

Every camera input on the Devices page shows a live thumbnail, even before a source uses it:

- a camera a scene shows uses its multiview picture;
- any other camera gets a temporary preview from video-in. The page leases it with
  `video_in.preview identity=<identity> [on=true|false] [lease=6]` and renews the lease every
  2 s while the thumbnail is on screen; the query `video_in.preview [have=[{identity, seq}]]`
  returns `{previews: [{identity, state, source, error, seq, width, height, jpeg}]}` (JPEG,
  base64, left out when the client already has `seq`). `state` is `live`, `starting`,
  `no_picture`, `waiting` (a source is opening the camera) or `error`.
- A camera a source is capturing is never reopened: the source's capture thread hands over a
  320-pixel-wide thumbnail at most 4 times a second while someone looks. Any other camera is
  opened at its lightest mode (the smallest YUYV/MJPEG size at least 320 wide, lowest rate of
  5 fps or more) and closed when the lease runs out (1–60 s, default 6 s) or on `on=false`, so a
  closed page or crashed UI releases it within seconds. A source that starts using the camera
  takes it over: the preview is stopped first.

### Private camera and desktop video over Tailscale

The private viewers use continuous **H264 WebRTC**, not thumbnail polling or VNC video:

| Viewer | Workstation URL | Source |
|---|---|---|
| Camera | `https://omarchy.tailc04968.ts.net:10001/` | Existing `cam_wide` capture, 1920×1080 at 30 fps |
| Desktop | `https://omarchy.tailc04968.ts.net:10000/` | DP-2 monitor containing Stream Engine, 3440×1440 at 30 fps |
| Mouse/keyboard control | `https://omarchy.tailc04968.ts.net:10002/vnc.html?autoconnect=true&resize=scale` | Existing noVNC/WayVNC control path |

The WebRTC viewers are view-only. The original desktop URL now serves encoded video;
VNC is retained separately for input. Existing Serve routes on 443 and 8443 are unchanged.
No viewer records files, captures audio, changes lighting, or exposes engine commands.

`se-camera-stream` reads the engine's `preview` SHM canvas through `frames.sock`, releases
skipped buffers promptly, and feeds FFmpeg NVENC directly without a second V4L2 capture.
The project's `private_camera` preview scene contains only `cam_wide`; `[render]` sets
preview scale to 1 and disables preview canvas/output FX. **Selecting a different preview
scene changes this camera feed**; it is not an independent preview selection.
The desktop publisher uses the installed `gpu-screen-recorder`, H264 baseline, 6 Mbps,
30 fps and a one-second keyframe interval. The camera publisher uses approximately 4 Mbps.

Stock [MediaMTX v1.21.1](https://github.com/bluenviron/mediamtx/releases/tag/v1.21.1)
receives both publishers on loopback RTSP/TCP port 18554. Its player/WHEP listener is
loopback-only on 18889; ICE UDP binds only the workstation's Tailscale IP, port 18189,
with no public STUN server. `scripts/camera-preview.py` is a standard-library-only
owner gateway, using port 18770 for the camera and 18771 with `--default-stream desktop`
for the desktop. Its allowed routes are the two player pages, `reader.js`, and WHEP
signaling; administrative and publishing HTTP routes are not exposed.

The installed user units are `stream-engine-camera-relay.service`,
`stream-engine-camera-publisher.service`, `stream-engine-camera-preview.service`,
`stream-engine-desktop-publisher.service` and `stream-engine-desktop-preview.service`.
They restart indefinitely on helper failure. A replaced canvas or engine disconnect
restarts the camera publisher with fresh mappings; the stock player reconnects after
stream loss and displays errors while unavailable. The frame writer allows bounded
cold encoder initialization, then limits stalled writes to 100 ms rather than queueing
old video or retaining the engine's whole buffer ring.

Remote encoded-video access must use **Tailscale Serve, not Funnel**. Serve strips
supplied identity headers and injects the authenticated peer's `Tailscale-User-Login`;
each gateway request requires exactly one header matching the configured owner.
Missing/wrong/duplicate identities and cross-site embedding are denied; external-link
document navigation is allowed. Loopback origins trust local processes. VNC retains its
existing tailnet access boundary; it is not served by this owner-checking video gateway.

Verification from the owner's MacBook Tailscale peer: actual camera and desktop pictures,
1920×1080 camera video arriving at about 30 fps, and 3440×1440 desktop video arriving at
about 29 fps. Camera frame metadata showed approximately 51 ms from receiver arrival to
presentation; this is **not a measured end-to-end capture latency**. Both streams are
H264 baseline. Owner gateway denial/navigation/header-stripping checks passed.
The camera recovered after the offline engine upgrade; VNC HTML remained reachable
from the remote peer. Camera exposure and white balance still limit color judgments;
video delivery is not certification of fixture timing, mode, or wireless reception.

## Sources (`sources/<name>.toml`)

The file stem is the source name used by scenes (`src = "cam_kit"`) and the name of its video
slot.

```toml
# camera
label  = "Kit (HDMI 1)"
device = "pci-0000:05:00.0-video-index0"   # identity (glob ok), /dev/v4l/by-*/… or /dev/videoN
format = "yuyv"                            # yuyv (uploaded as-is) | mjpeg (decoded to RGBA on the CPU)
size   = [1920, 1080]
fps    = 60
buffers = 4                                # V4L2 mmap buffers (2..16)

[controls]                                 # initial camera controls, names as in `v4l2-ctl -L`
brightness = 128
auto_exposure = "manual"                   # menus: option name, label, unique prefix, or index

[color]                                    # GPU color correction (renderer)
contrast = 1.05
saturation = 1.0
lut = "assets/luts/kit.cube"
lut_amount = 1.0

[signal]
timeout = "500ms"                          # no frame for this long → no signal
black_level = 0.12                         # brightest sampled luma below this → black
hold = "700ms"                             # a bad picture must persist this long
```

```toml
# media file: a video, or a picture (PNG, JPEG, WebP) or GIF
file    = "assets/brb.mp4"
loop    = true
rate    = 1.0
hwaccel = "auto"                           # auto (NVDEC, CPU fallback) | cuda (NVDEC only) | none
```

A file with a single picture (PNG, JPEG, WebP, a one-frame GIF) is decoded once and held: it
never restarts and never sends `source.ended`. Animated GIFs play at their own frame timing and
loop like videos. RGB and palette pictures (PNG, GIF) keep full color and transparency (RGBA);
YUV files (video, JPEG) use NV12. SVG is not a source format.

Only sources in use are captured: the renderer's `render.sources.used` list, or (until the
renderer publishes it) every source referenced by a scene, plus every source the recorder taps
(camera ISO recordings, `source = "camera:<n>"` in `[recording]`). A source that stops being used
keeps capturing for 5 s so transitions don't reopen devices.

**Recording taps.** A camera is opened once; the recorder receives the same frames the renderer
gets (YUYV as captured, or MJPEG decoded to RGBA, with the master-clock capture time) through a
small queue per recording. A tapped camera is captured even when no scene shows it, and is released
5 s after its last recording stops. A recorder that falls behind loses its oldest queued frames
(counted by the recorder); the live picture and the capture thread never wait for it. Without a
recording the capture path is unchanged. `video_in.<n>.taps` shows how many recordings hold
the source.

### Addresses per source

| Address | Kind | Meaning |
|---|---|---|
| `source.<n>.signal` | readback | live picture (not no-signal, flat/black, or timed out) |
| `source.<n>.fps`, `.dropped`, `.cpu` | readback | measured rate, dropped frames, capture-thread CPU (% of one core) |
| `source.<n>.width/height/format/matrix/range` | readback | slot layout; YUV matrix/range for the shader |
| `source.<n>.capturing`, `.path`, `.device`, `.error` | readback | worker state |
| `video_in.<n>.taps` | readback | recordings holding the source (captured while > 0) |
| `source.<n>.ctrl.<control>` | parameter | camera control with the device's real range; scenes/presets/bindings can set it |
| `source.<n>.color.{brightness,contrast,saturation,gamma,temperature,tint}`, `.lut`, `.lut_amount` | parameter | color correction applied by the renderer |
| `source.<n>.paused`, `.rate`, `.loop` | parameter | media files |
| `source.<n>.position`, `.duration`, `.playing` | readback | media files |
| `source.<n>.media`, `.isrc` | readback | media files: timeline id `file:<hash>` and the ISRC tag; `file:`/`isrc:` timelines follow the exact frame position (docs/timelines.md) |

Control changes are written to the camera only when the resolved value differs from what the
device has (checked at 20 Hz); auto modes are applied first, then manual values they gate.

Actions: `source.reopen <n>`, `source.restart <n>` (media from the start), `source.seek <n>
<seconds>`, `source.assign <n> <identity>` (creates `sources/<n>.toml` with the camera's best
mode if missing), `source.save_controls <n>` (writes current control values to the file).
Preflight: `health.sources` fails when a used camera is missing, warns on no signal.

Events: `source.ended {source}` when a non-looping media file finishes.
