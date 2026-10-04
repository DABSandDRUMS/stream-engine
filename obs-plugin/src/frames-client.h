/* SPDX-License-Identifier: GPL-2.0-or-later
 *
 * frames.sock client (docs/frames-protocol.md), independent of libobs so it can be tested
 * against a fake engine. One background IO thread per client connects (with backoff), sends
 * hello, receives canvases (dmabuf or memfd fds) and frames, waits for each frame's sync_file
 * fence, and hands the newest ready frame to the consumer. The consumer (OBS render thread)
 * only calls the non-blocking se_frames_client_poll()/se_frames_client_release().
 */
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "se-frames-proto.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Log levels (numerically equal to libobs' LOG_ERROR/LOG_WARNING/LOG_INFO/LOG_DEBUG). */
enum se_log_level {
	SE_LOG_ERROR = 100,
	SE_LOG_WARNING = 200,
	SE_LOG_INFO = 300,
	SE_LOG_DEBUG = 400,
};

typedef void (*se_log_fn)(void *ud, int level, const char *msg);

struct se_frames_opts {
	const char *socket_path;
	uint32_t canvas;      /* enum se_canvas_id */
	uint32_t client_kind; /* enum se_client_kind */
	bool dmabuf;          /* advertise dmabuf import; false = ask for the memfd (shm) path */
	uint32_t fence_timeout_ms; /* 0 = 1000 */
	se_log_fn log;
	void *log_ud;
};

/* One canvas (re)creation. Ownership passes to the consumer through se_frames_client_poll();
 * free it with se_frames_import_free() once its textures are gone. */
struct se_frames_import {
	uint64_t epoch; /* local; increments with every se_canvas message */
	uint32_t generation;
	uint32_t canvas;
	uint32_t width, height;
	uint32_t fourcc; /* 0 = shm (memfd, RGBA8 rows of strides[0]) */
	uint64_t modifier;
	uint32_t offsets[4];
	uint32_t strides[4];
	uint32_t buffer_count;
	int fds[SE_MAX_BUFFERS];
	const uint8_t *maps[SE_MAX_BUFFERS]; /* shm only: read-only mappings (pixel data at offsets[0]) */
	size_t map_size;
};

void se_frames_import_free(struct se_frames_import *imp);

struct se_frames_ready {
	uint64_t epoch;
	uint32_t buffer;
	uint64_t seq;
	uint64_t monotonic_ns;
};

struct se_frames_update {
	/* Non-NULL when the canvas was (re)created since the last poll; the consumer owns it and
	 * must drop every texture of older imports. */
	struct se_frames_import *import;
	/* Newest frame whose fence has signaled (always from the latest import). The consumer holds
	 * the buffer until it calls se_frames_client_release(). */
	bool has_frame;
	struct se_frames_ready frame;
};

struct se_frames_stats {
	bool connected;
	bool goodbye;
	uint32_t goodbye_reason;
	uint64_t last_frame_ns; /* CLOCK_MONOTONIC arrival of the last se_frame; 0 = none yet */
	uint64_t frames;        /* se_frame messages accepted */
	uint64_t superseded;    /* ready frames replaced before the consumer took them */
	uint64_t fence_timeouts;
	uint64_t protocol_errors;
	uint64_t connects;
	uint32_t width, height, fourcc, generation;
	bool dmabuf; /* current hello flag */
};

struct se_frames_client *se_frames_client_start(const struct se_frames_opts *opts);
/* Stops and joins the IO thread and frees everything not handed to the consumer. */
void se_frames_client_stop(struct se_frames_client *c);

/* Non-blocking; safe to call from the render thread every frame. */
void se_frames_client_poll(struct se_frames_client *c, struct se_frames_update *out);
/* False after disconnect, including after reconnect before a fresh canvas arrives.
 * Consumers must stop sampling textures of an invalidated import. */
bool se_frames_client_import_current(struct se_frames_client *c, uint64_t epoch);
/* The consumer no longer reads `buffer` of import `epoch` (GPU work finished). */
void se_frames_client_release(struct se_frames_client *c, uint64_t epoch, uint32_t buffer, uint64_t seq);
/* Switch between dmabuf import and the shm fallback; reconnects if the flag changes. */
void se_frames_client_set_dmabuf(struct se_frames_client *c, bool dmabuf);
void se_frames_client_stats(struct se_frames_client *c, struct se_frames_stats *out);

/* CLOCK_MONOTONIC in ns. */
uint64_t se_mono_ns(void);

/* `$SE_RUNTIME_DIR/<name>` if set, else `$XDG_RUNTIME_DIR/stream-engine/<name>`
 * (`/run/user/<uid>/stream-engine/<name>` without XDG_RUNTIME_DIR). Returns false if it does
 * not fit into `len` bytes. */
bool se_runtime_path(const char *name, char *out, size_t len);

#ifdef __cplusplus
}
#endif
