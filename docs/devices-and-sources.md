# Devices and video sources

Operator notes for the device registry (`se-devices`, PLAN §3.5) and camera/media sources
(`se-video-in`, PLAN §4.2).

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
`port` (udev `ID_PATH`, glob), `name` (case-insensitive glob). `optional = true` makes a missing
device a warning instead of a failure. The more specific entry wins when several could match.

State: `devices.<id>.{present,name,kind,path,identity}` (`<id>` = the expected key, or a slug of
the identity for undeclared devices). Preflight: `health.devices` fails when an expected device
is missing. Events: `devices.added` / `devices.removed` `{id, kind, name, identity, path}`.

Actions: `devices.expect identity=<identity> [id=…] [label=…]`, `devices.rename <id> <label>`,
`devices.forget <id>`, `devices.rescan`. They edit `project.toml` in place (comments kept).

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
# media file
file    = "assets/brb.mp4"
loop    = true
rate    = 1.0
hwaccel = "auto"                           # auto (NVDEC, CPU fallback) | cuda (NVDEC only) | none
```

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

Control changes are written to the camera only when the resolved value differs from what the
device has (checked at 20 Hz); auto modes are applied first, then manual values they gate.

Actions: `source.reopen <n>`, `source.restart <n>` (media from the start), `source.seek <n>
<seconds>`, `source.assign <n> <identity>` (creates `sources/<n>.toml` with the camera's best
mode if missing), `source.save_controls <n>` (writes current control values to the file).
Preflight: `health.sources` fails when a used camera is missing, warns on no signal.

Events: `source.ended {source}` when a non-looping media file finishes.
