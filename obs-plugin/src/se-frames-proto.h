/* SPDX-License-Identifier: GPL-2.0-or-later
 *
 * frames.sock wire format (docs/frames-protocol.md). All integers little-endian; every struct
 * is naturally aligned, so the in-memory layout is the wire layout on x86-64/aarch64.
 */
#pragma once

#include <stddef.h>
#include <stdint.h>

#define SE_MAGIC 0x52464553u /* "SEFR" */
#define SE_VERSION 1u

enum se_msg_type {
	SE_MSG_HELLO = 1,
	SE_MSG_RELEASE = 3,
	SE_MSG_CANVAS = 10,
	SE_MSG_FRAME = 11,
	SE_MSG_GOODBYE = 12,
};

enum se_canvas_id {
	SE_CANVAS_WIDE = 0,
	SE_CANVAS_TALL = 1,
	SE_CANVAS_PREVIEW = 2,
	SE_CANVAS_ATLAS = 3,
	SE_CANVAS_COUNT = 4,
};

enum se_client_kind {
	SE_CLIENT_OBS = 1,
	SE_CLIENT_UI = 2,
	SE_CLIENT_OTHER = 3,
};

#define SE_HELLO_FLAG_DMABUF 1u

enum se_goodbye_reason {
	SE_GOODBYE_SHUTDOWN = 1,
	SE_GOODBYE_DEVICE_LOST = 2,
	SE_GOODBYE_CANVAS_REMOVED = 3,
};

#define SE_CANVAS_ALL 0xffffffffu
#define SE_MAX_BUFFERS 8u

/* DRM fourccs the plugin can import (drm_fourcc.h values). */
#define SE_DRM_FORMAT_ABGR8888 0x34324241u /* RGBA8 in memory */
#define SE_DRM_FORMAT_XBGR8888 0x34324258u
#define SE_DRM_FORMAT_ARGB8888 0x34325241u /* BGRA8 in memory */
#define SE_DRM_FORMAT_XRGB8888 0x34325258u

struct se_hdr {
	uint32_t magic;
	uint16_t type;
	uint16_t version;
};

struct se_hello {
	struct se_hdr h;
	uint32_t client;
	uint32_t want;
	uint32_t flags;
	uint32_t reserved;
};

struct se_release {
	struct se_hdr h;
	uint32_t canvas;
	uint32_t buffer;
	uint64_t seq;
};

struct se_canvas {
	struct se_hdr h;
	uint32_t canvas;
	uint32_t width, height;
	uint32_t drm_fourcc;
	uint64_t modifier;
	uint32_t offsets[4];
	uint32_t strides[4];
	uint32_t planes;
	uint32_t buffer_count;
	uint32_t generation;
	uint32_t reserved;
};

struct se_frame {
	struct se_hdr h;
	uint32_t canvas;
	uint32_t buffer;
	uint64_t seq;
	uint64_t monotonic_ns;
	uint32_t generation;
	uint32_t has_fence;
};

struct se_goodbye {
	struct se_hdr h;
	uint32_t reason;
	uint32_t canvas;
};

_Static_assert(sizeof(struct se_hdr) == 8, "se_hdr");
_Static_assert(sizeof(struct se_hello) == 24, "se_hello");
_Static_assert(sizeof(struct se_release) == 24, "se_release");
_Static_assert(sizeof(struct se_canvas) == 80, "se_canvas");
_Static_assert(offsetof(struct se_canvas, modifier) == 24, "se_canvas.modifier");
_Static_assert(offsetof(struct se_canvas, buffer_count) == 68, "se_canvas.buffer_count");
_Static_assert(sizeof(struct se_frame) == 40, "se_frame");
_Static_assert(offsetof(struct se_frame, monotonic_ns) == 24, "se_frame.monotonic_ns");
_Static_assert(sizeof(struct se_goodbye) == 16, "se_goodbye");

static inline struct se_hdr se_hdr_make(uint16_t type)
{
	struct se_hdr h = {SE_MAGIC, type, SE_VERSION};
	return h;
}

static inline const char *se_canvas_name(uint32_t canvas)
{
	switch (canvas) {
	case SE_CANVAS_WIDE:
		return "wide";
	case SE_CANVAS_TALL:
		return "tall";
	case SE_CANVAS_PREVIEW:
		return "preview";
	case SE_CANVAS_ATLAS:
		return "atlas";
	default:
		return "unknown";
	}
}
