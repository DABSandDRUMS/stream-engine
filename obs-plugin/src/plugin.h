/* SPDX-License-Identifier: GPL-2.0-or-later
 *
 * Shared state of the stream-engine OBS module (glue between libobs/frontend and the
 * OBS-independent frames/control clients).
 */
#pragma once

#include <obs-module.h>
#include <pthread.h>
#include <stdatomic.h>

#include "control-client.h"
#include "frames-client.h"

#define SE_PLUGIN_VERSION "0.1.0"
#define SE_DEFAULT_FALLBACK_SCENE "Technical Difficulties"
#define SE_DEFAULT_FALLBACK_TEXT "Technical difficulties \xe2\x80\x94 back in a moment"
#define SE_MAX_SOURCES 32
#define SE_MAX_FALLBACK_CANVASES 8

enum se_fallback_mode {
	SE_FALLBACK_OFF,    /* never switch automatically */
	SE_FALLBACK_LIVE,   /* switch only while a stream output is active */
	SE_FALLBACK_ALWAYS, /* switch whenever a visible feed goes stale */
};

struct se_config {
	uint32_t stale_ms;
	bool vertical_enabled;
	uint64_t vertical_revision;
	enum se_fallback_mode fallback_mode;
	char fallback_scene[256];
	char fallback_text[512];
};

/* One `stream-engine: …` source instance. Render fields belong to the graphics thread. */
struct se_canvas_source {
	obs_source_t *source;
	uint32_t canvas; /* enum se_canvas_id */
	bool generic;
	bool dmabuf_pref;
	struct se_frames_client *client;
	uint64_t created_ns;

	/* monitor (control thread) */
	bool latched; /* fallback already triggered for this stale period */
	atomic_bool stale;

	/* graphics thread */
	struct se_frames_import *imp;
	gs_texture_t *tex[SE_MAX_BUFFERS];
	bool shm;
	int32_t cur; /* displayed buffer (dmabuf) or -1 */
	uint64_t cur_seq;
	void *cur_fence; /* GLsync after the latest draw of `cur` */
	bool cur_drawn; /* NULL fence after a failed allocation still means GPU work is pending */
	bool has_image;
	struct {
		uint32_t buffer;
		uint64_t seq;
		void *fence;
		bool drawn; /* never-drawn buffers need no GPU completion fence */
	} retired[SE_MAX_BUFFERS * 2];
	size_t n_retired;
	bool import_failed_logged;
	atomic_uint width, height;
};

struct se_plugin {
	pthread_mutex_t mu; /* guards config, sources, scene_name, stream_output */
	struct se_config config;
	struct se_canvas_source *sources[SE_MAX_SOURCES];
	size_t n_sources;
	char scene_name[256];

	struct se_control *control;
	atomic_bool exiting;
	atomic_bool fallback_engaged; /* any canvas currently in a fallback we switched to */
	atomic_bool fallback_auto;    /* ... and at least one of them was automatic */

	/* frontend outputs captured on the UI thread (weak refs; guarded by mu) */
	obs_weak_output_t *stream_output;
};

extern struct se_plugin se_g;

void se_log(void *ud, int level, const char *msg);
void se_config_copy(struct se_config *out);
uint64_t se_stale_ms(void);
/* Adds "obs_ns" (os_gettime_ns) and "mono_ns" (CLOCK_MONOTONIC) sampled together. */
void se_stamp(json_t *m);

/* canvas-source.c */
void se_register_sources(void);
/* true if `src` is a registered stream-engine source; fills its canvas + stale flag */
bool se_source_lookup(const obs_source_t *src, uint32_t *canvas, bool *stale);

struct se_monitor_result {
	uint32_t count[SE_CANVAS_COUNT]; /* source instances per engine canvas */
	bool fresh[SE_CANVAS_COUNT];     /* at least one instance receives frames */
	bool engage;                     /* a visible source just went stale: switch to fallback */
	bool recovered;                  /* a source went from stale to fresh */
	struct se_frames_stats stats[SE_CANVAS_COUNT]; /* freshest instance per canvas */
};
/* Control thread, every tick: recompute staleness and fallback triggers. */
void se_sources_monitor(uint64_t now, uint32_t stale_ms, bool trigger_allowed, struct se_monitor_result *r);

/* fallback.c (UI thread unless noted) */
enum se_fallback_reason { SE_REASON_STALE, SE_REASON_MANUAL };
/* Switch every OBS canvas whose program shows a (stale, for SE_REASON_STALE) stream-engine
 * source to its fallback scene. Returns the number of canvases switched. */
int se_fallback_engage(enum se_fallback_reason reason, char *err, size_t errlen);
/* Switch back canvases whose saved scene no longer shows a stale feed (force: all, incl.
 * manual ones). Returns the number of canvases restored. */
int se_fallback_restore(bool force);
/* Create missing fallback scenes (no switching). */
int se_fallback_prepare(char *err, size_t errlen);
/* Drop all fallback bookkeeping (scene collection change / exit). */
void se_fallback_reset(void);
/* Add `stream-engine: wide` to the main program scene and `stream-engine: tall` to the
 * program scene of the first other canvas when missing. Never removes or reorders. */
json_t *se_setup_sources(void);
/* Name of the OBS canvas a video output renders (main = "Main"); any thread. */
void se_canvas_kind_for_video(video_t *video, char *out, size_t len);

/* vertical.c: control thread, reset after joining that thread on shutdown. */
void se_vertical_tick(bool enabled, uint64_t now);
json_t *se_vertical_status(void);
void se_vertical_reset(void);

/* gl-fence.c (graphics thread) */
enum se_gl_fence_status { SE_GL_FENCE_PENDING, SE_GL_FENCE_COMPLETE, SE_GL_FENCE_FAILED };
void *se_gl_fence_create(void);
enum se_gl_fence_status se_gl_fence_poll(void *fence);
void se_gl_fence_destroy(void *fence);
bool se_gl_fence_available(void);
