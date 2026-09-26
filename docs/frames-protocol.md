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
  `stale_ms` (default 500 ms) and reports it; OBS then shows the last frame or the fallback scene.

## obs.sock (JSON lines, UTF-8, one object per line)

### Plugin → engine

```json
{"t":"hello","obs":"32.2.2","plugin":"0.1.0","canvases":["wide","tall"]}
{"t":"status","streaming":true,"recording":false,"kbps":6000,"dropped":0,"total":123456,"lag_ms":1.2,"fps":60.0,"obs_ns":123,"mono_ns":456,"stale":{"wide":false,"tall":false}}
{"t":"event","name":"stream_started"}            // stream_started|stream_stopped|record_started|record_stopped|scene_fallback|scene_restored
{"t":"record_path","path":"/home/u/Videos/2026-09-25 20-00-00.mkv","canvas":"wide"}
{"t":"reply","id":7,"ok":true,"error":null}
```

`status` is sent once per second. `obs_ns` is OBS's `os_gettime_ns()` and `mono_ns` is
`CLOCK_MONOTONIC` sampled together, for the clock mapping (§3.2).

### Engine → plugin

```json
{"t":"cmd","id":7,"op":"stream.start"}   // stream.start|stream.stop|record.start|record.stop|fallback.on|fallback.off
{"t":"config","stale_ms":500,"fallback_scene":"Technical Difficulties"}
```

The plugin executes `cmd` through the OBS frontend API and answers with `reply`.
