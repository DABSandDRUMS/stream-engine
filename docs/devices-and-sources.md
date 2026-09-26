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
renderer publishes it) every source referenced by a scene. A source that stops being used keeps
capturing for 5 s so transitions don't reopen devices.

### Addresses per source

| Address | Kind | Meaning |
|---|---|---|
| `source.<n>.signal` | readback | live picture (not no-signal, flat/black, or timed out) |
| `source.<n>.fps`, `.dropped`, `.cpu` | readback | measured rate, dropped frames, capture-thread CPU (% of one core) |
| `source.<n>.width/height/format/matrix/range` | readback | slot layout; YUV matrix/range for the shader |
| `source.<n>.capturing`, `.path`, `.device`, `.error` | readback | worker state |
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
