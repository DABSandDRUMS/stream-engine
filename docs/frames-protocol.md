# Frame and OBS-control protocols

Two local Unix sockets in `$XDG_RUNTIME_DIR/stream-engine/` (directory mode 0700):

| Socket | Type | Purpose | Server | Clients |
|---|---|---|---|---|
| `frames.sock` | `SOCK_SEQPACKET` | Zero-copy canvas frames (dmabuf fds + sync fences) | engine (`se-render`) | OBS plugin, UI |
| `obs.sock` | `SOCK_STREAM`, JSON lines | OBS health, timestamps, recording paths, start/stop | engine (`se-obs`) | OBS plugin |

All integers are little-endian. File descriptors travel as `SCM_RIGHTS` ancillary data on the
message they belong to. Every message starts with the same 8-byte header.

```c
struct se_hdr {
    uint32_t magic;   /* 0x52464553 = "SEFR" */
    uint16_t type;    /* message type below */
    uint16_t version; /* 1 */
};
```

## frames.sock

### Client → engine

```c
/* type 1 */
struct se_hello {
    struct se_hdr h;
    uint32_t client;     /* 1 = OBS plugin, 2 = UI, 3 = other */
    uint32_t want;       /* bitmask of canvases: 1<<0 wide, 1<<1 tall, 1<<2 preview, 1<<3 atlas */
    uint32_t flags;      /* bit0: client can import dmabuf; if 0 the engine uses the shm fallback */
    uint32_t reserved;
};

/* type 3: the client no longer reads this buffer (sent when it switches to a newer frame) */
struct se_release {
    struct se_hdr h;
    uint32_t canvas;     /* 0 wide, 1 tall, 2 preview, 3 atlas */
    uint32_t buffer;     /* buffer index from se_frame */
    uint64_t seq;
};
```

### Engine → client

```c
/* type 10: sent after hello and whenever a canvas is (re)created (resize, device loss).
 * SCM_RIGHTS carries `buffer_count` fds: one dmabuf per buffer (single plane), or with
 * drm_fourcc == 0 one memfd per buffer holding tightly packed RGBA8 rows of `strides[0]`. */
struct se_canvas {
    struct se_hdr h;
    uint32_t canvas;
    uint32_t width, height;
    uint32_t drm_fourcc;     /* DRM_FORMAT_ABGR8888 (0x34324241) for RGBA8 unorm; 0 = shm fallback */
    uint64_t modifier;       /* DRM format modifier (DRM_FORMAT_MOD_LINEAR = 0 or vendor tiled) */
    uint32_t offsets[4];
    uint32_t strides[4];
    uint32_t planes;         /* 1 */
    uint32_t buffer_count;   /* 3–4 */
    uint32_t generation;     /* increments on every re-create; frames from older generations are ignored */
    uint32_t reserved;
};

/* type 11: a new frame is ready in `buffer`. If `has_fence` is 1, SCM_RIGHTS carries one
 * sync_file fd that signals when the GPU finished writing; the client must not sample the
 * buffer before it signals (poll(POLLIN) with timeout 0, or import as an EGL/Vulkan fence). */
struct se_frame {
    struct se_hdr h;
    uint32_t canvas;
    uint32_t buffer;
    uint64_t seq;
    uint64_t monotonic_ns;   /* master clock (CLOCK_MONOTONIC) time the frame represents */
    uint32_t generation;
    uint32_t has_fence;
};

/* type 12: the engine is stopping or lost its GPU device; keep showing the last frame or
 * switch to the fallback until a new se_canvas arrives. */
struct se_goodbye {
    struct se_hdr h;
    uint32_t reason;         /* 1 shutdown, 2 device lost, 3 canvas removed */
    uint32_t canvas;         /* 0xffffffff = all */
};
```

### Rules

- The engine renders into a buffer that is neither the most recently sent one nor held by any
  client (held = sent and not yet released). With 4 buffers and two clients this never stalls;
  if every buffer is held, the engine skips the export for that frame (render never waits).
- Frames are paced by the engine's 60 fps clock; clients sample the newest.
- A client that stops reading for > 2 s is disconnected.
- Staleness: the OBS plugin treats a canvas as stale when no `se_frame` arrived for
  `stale_ms` (default 500 ms), or immediately after `se_goodbye`, and reports it; OBS then shows
  the last frame or switches to the fallback scene (`docs/obs.md`).
- `se_canvas` follows **every** hello, also after a reconnect with an unchanged generation;
  clients treat each `se_canvas` as a new import set.
- `offsets[0]`/`strides[0]`/`modifier` apply to every buffer of the canvas. The OBS plugin imports
  `DRM_FORMAT_ABGR8888` (preferred), `XBGR8888`, `ARGB8888`, `XRGB8888`; if the fourcc is
  unsupported or `gs_texture_create_from_dmabuf` fails it reconnects **without** the dmabuf flag
  and expects memfds (the shm fallback happens automatically).
- The OBS plugin releases a dmabuf buffer only after the GPU finished the draws that sampled it (GL
  fence, usually within one frame of switching to a newer frame); frames superseded before they
  were shown are released immediately, shm buffers right after the upload. In practice a plugin
  client holds 1–2 buffers, so 4 buffers per canvas never stall.
- Clients drop the connection on protocol errors (bad magic/version, truncated `SCM_RIGHTS`,
  buffer index out of range, fence count mismatch) and reconnect with backoff (100 ms → 2 s).
- Clients look for the sockets in `$SE_RUNTIME_DIR` when set (dev instances).

## obs.sock (JSON lines, UTF-8, one object per line)

One plugin connection at a time: while a connection is alive (a line within 5 s) a second one gets
`{"t":"error",…}` and is closed; a connection silent for 10 s is dropped. Every message with
`obs_ns` also carries `mono_ns`: OBS's `os_gettime_ns()` and `CLOCK_MONOTONIC` sampled together
(§3.2 clock mapping). Unknown `t` values are ignored in both directions.

### Plugin → engine

```json
{"t":"hello","obs":"32.2.2","plugin":"0.1.0","canvases":["wide","tall"],"pid":1234,"config":{…last engine config…}}
{"t":"status","streaming":true,"recording":false,"rec_paused":false,"kbps":6000.0,"dropped":0,"total":123456,"congestion":0.0,
 "rec_kbps":0.0,"lag_ms":0.0,"fps":60.0,"render_ms":0.4,"lagged":0,"rendered":1000,"skipped":0,"encoded":1000,
 "obs_ns":123,"mono_ns":456,"stream_start_ns":100,"record_start_ns":0,"record_path":"","record_dir":"/home/u/Videos",
 "scene":"Scene","stale":{"wide":false,"tall":false},"sources":{"wide":1,"tall":1,"preview":0,"atlas":0},
 "feeds":{"wide":{"connected":true,"frames":600,"superseded":0,"fence_timeouts":0,"width":1920,"height":1080,"dmabuf":true,"goodbye":false,"age_ms":8}},
 "fallback":false,
 "outputs":[{"name":"simple_stream","id":"rtmp_output","kind":"stream","active":true,"kbps":6000.0,"dropped":0,"total":3600,"congestion":0.0,"canvas":"wide"}]}
{"t":"event","name":"stream_started","obs_ns":123,"mono_ns":456}   // stream_started|stream_stopped|record_started|record_stopped (+ "path")|record_paused|record_unpaused
{"t":"event","name":"scene_fallback","canvas":"Main","scene":"Technical Difficulties","from":"Scene","canvases":["wide"],"reason":"stale","obs_ns":…,"mono_ns":…}
{"t":"event","name":"scene_restored","canvas":"Main","scene":"Technical Difficulties","to":"Scene","canvases":["wide"],"reason":"fresh","obs_ns":…,"mono_ns":…}   // reason fresh|manual|operator
{"t":"record_path","path":"/home/u/Videos/2026-09-25 20-00-00.mkv","canvas":"wide","output":"simple_file_output","start_obs_ns":120,"obs_ns":123,"mono_ns":456,
 "tracks":[{"index":0,"mixer":1,"name":"Track 1","sources":["se-program"],"devices":["se-program"]}]}
{"t":"record_end","path":"…","canvas":"wide","output":"simple_file_output","end_obs_ns":130,"obs_ns":131,"mono_ns":464}
{"t":"reply","id":7,"ok":true,"error":null,"result":"starting"}
```

`status` is sent once per second and immediately when a feed's staleness or the number of sources
changes. `lag_ms` = skipped (encoder-lag) frames in the last interval × frame interval.
`stream_start_ns`/`record_start_ns`/`start_obs_ns` are the OBS-clock time of the first frame of the
stream / file (0 = inactive). A `record_path` is sent for every file (including splits, and again
after a reconnect for files still being written); `canvas` is `wide`/`tall` from the
`stream-engine` source found in the recorded OBS canvas (else the OBS canvas name).

### Engine → plugin

```json
{"t":"config","stale_ms":500,"fallback_mode":"live","fallback_scene":"Technical Difficulties","fallback_text":"…"}   // after hello and on change
{"t":"cmd","id":7,"op":"stream.start"}   // stream.start|stream.stop|record.start|record.stop|fallback.on|fallback.off|fallback.setup|setup
{"t":"error","error":"another OBS instance is already connected to this engine"}
```

The plugin executes `cmd` on OBS's UI thread through the frontend API and answers with `reply`.
`fallback_mode`: `off` (manual only), `live` (only while an output is active), `always`.
