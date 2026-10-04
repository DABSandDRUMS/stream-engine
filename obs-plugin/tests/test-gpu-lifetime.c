/* SPDX-License-Identifier: GPL-2.0-or-later
 *
 * Exercise the production canvas/fence state machine without a GPU. Only external OBS,
 * socket-client and GL calls are substituted; retirement/import/render logic is unchanged.
 */
#include "../src/plugin.h"

#include <EGL/egl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int failures;
#define CHECK(cond, ...)                                                        \
	do {                                                                    \
		if (!(cond)) {                                                  \
			failures++;                                             \
			fprintf(stderr, "FAIL %s:%d: %s — ", __FILE__, __LINE__, #cond); \
			fprintf(stderr, __VA_ARGS__);                            \
			fputc('\n', stderr);                                    \
		}                                                               \
	} while (0)

struct __GLsync {
	bool live;
};

static __eglMustCastToProperFunctionPointerType test_get_proc(const char *name);
static void test_log(int level, const char *format, ...);
static const char *test_name(const obs_source_t *source);
static const char *test_text(const char *text);
static void test_void(void);
static bool test_false(void);
static void test_enable(bool enable);
static gs_effect_t *test_effect(enum obs_base_effect effect);
static gs_eparam_t *test_param(const gs_effect_t *effect, const char *name);
static void test_set_texture(gs_eparam_t *param, gs_texture_t *texture);
static bool test_effect_loop(gs_effect_t *effect, const char *name);
static void test_draw(gs_texture_t *texture, uint32_t flip, uint32_t width, uint32_t height);
static gs_texture_t *test_texture_create(uint32_t width, uint32_t height, enum gs_color_format format,
					 uint32_t levels, const uint8_t **data, uint32_t flags);
static gs_texture_t *test_dmabuf_import(unsigned int width, unsigned int height, uint32_t fourcc,
					 enum gs_color_format format, uint32_t planes, const int *fds,
					 const uint32_t *strides, const uint32_t *offsets, const uint64_t *modifiers);
static void test_texture_destroy(gs_texture_t *texture);
static void test_release(struct se_frames_client *client, uint64_t epoch, uint32_t buffer, uint64_t seq);
static void test_dmabuf(struct se_frames_client *client, bool dmabuf);
static bool test_import_current(struct se_frames_client *client, uint64_t epoch);
static void test_poll(struct se_frames_client *client, struct se_frames_update *out);

#define eglGetProcAddress test_get_proc
#define blog test_log
#define obs_source_get_name test_name
#undef obs_module_text
#define obs_module_text test_text
#define obs_enter_graphics test_void
#define obs_leave_graphics test_void
#define gs_get_linear_srgb test_false
#define gs_framebuffer_srgb_enabled test_false
#define gs_enable_framebuffer_srgb test_enable
#define gs_enable_blending test_enable
#define gs_blend_state_push test_void
#define gs_blend_state_pop test_void
#define obs_get_base_effect test_effect
#define gs_effect_get_param_by_name test_param
#define gs_effect_set_texture test_set_texture
#define gs_effect_set_texture_srgb test_set_texture
#define gs_effect_loop test_effect_loop
#define gs_draw_sprite test_draw
#define gs_texture_create test_texture_create
#define gs_texture_create_from_dmabuf test_dmabuf_import
#define gs_texture_destroy test_texture_destroy
#define se_frames_client_release test_release
#define se_frames_client_set_dmabuf test_dmabuf
#define se_frames_client_import_current test_import_current
#define se_frames_client_poll test_poll
#include "../src/gl-fence.c"
#include "../src/canvas-source.c"

struct se_plugin se_g = {.mu = PTHREAD_MUTEX_INITIALIZER};
void se_log(void *ud, int level, const char *message)
{
}

static struct __GLsync syncs[64];
static size_t next_sync;
static unsigned int wait_result;
static bool fail_create, import_current;
static unsigned int creates, deletes, releases, draws, imports, textures_destroyed, dmabuf_changes, effect_pass;
static bool requested_dmabuf;
static struct se_frames_ready last_release;
static char texture_token, effect_token, param_token, client_token;

static GLsync_ test_fence_sync(unsigned int condition, unsigned int flags)
{
	creates++;
	CHECK(condition == GL_SYNC_GPU_COMMANDS_COMPLETE_ && flags == 0, "fence creation arguments");
	if (fail_create)
		return NULL;
	CHECK(next_sync < sizeof(syncs) / sizeof(syncs[0]), "test fence pool exhausted");
	if (next_sync == sizeof(syncs) / sizeof(syncs[0]))
		return NULL;
	syncs[next_sync].live = true;
	return &syncs[next_sync++];
}

static unsigned int test_wait_sync(GLsync_ sync, unsigned int flags, uint64_t timeout)
{
	CHECK(sync && sync->live, "wait only polls a live fence");
	CHECK(flags == GL_SYNC_FLUSH_COMMANDS_BIT_ && timeout == 0, "GPU polling stays non-blocking");
	return wait_result;
}

static void test_delete_sync(GLsync_ sync)
{
	CHECK(sync && sync->live, "each fence is deleted once");
	sync->live = false;
	deletes++;
}

static __eglMustCastToProperFunctionPointerType test_get_proc(const char *name)
{
	if (strcmp(name, "glFenceSync") == 0)
		return (__eglMustCastToProperFunctionPointerType)test_fence_sync;
	if (strcmp(name, "glClientWaitSync") == 0)
		return (__eglMustCastToProperFunctionPointerType)test_wait_sync;
	if (strcmp(name, "glDeleteSync") == 0)
		return (__eglMustCastToProperFunctionPointerType)test_delete_sync;
	return NULL;
}

static void test_log(int level, const char *format, ...)
{
}
static const char *test_name(const obs_source_t *source)
{
	return "test canvas";
}
static const char *test_text(const char *text)
{
	return text;
}
static void test_void(void)
{
}
static bool test_false(void)
{
	return false;
}
static void test_enable(bool enable)
{
}
static gs_effect_t *test_effect(enum obs_base_effect effect)
{
	return (gs_effect_t *)&effect_token;
}
static gs_eparam_t *test_param(const gs_effect_t *effect, const char *name)
{
	return (gs_eparam_t *)&param_token;
}
static void test_set_texture(gs_eparam_t *param, gs_texture_t *texture)
{
}
static bool test_effect_loop(gs_effect_t *effect, const char *name)
{
	return (effect_pass++ % 2) == 0;
}
static void test_draw(gs_texture_t *texture, uint32_t flip, uint32_t width, uint32_t height)
{
	draws++;
}
static gs_texture_t *test_texture_create(uint32_t width, uint32_t height, enum gs_color_format format,
					 uint32_t levels, const uint8_t **data, uint32_t flags)
{
	return (gs_texture_t *)&texture_token;
}
static gs_texture_t *test_dmabuf_import(unsigned int width, unsigned int height, uint32_t fourcc,
					 enum gs_color_format format, uint32_t planes, const int *fds,
					 const uint32_t *strides, const uint32_t *offsets, const uint64_t *modifiers)
{
	imports++;
	return (gs_texture_t *)&texture_token;
}
static void test_texture_destroy(gs_texture_t *texture)
{
	if (texture)
		textures_destroyed++;
}
static void test_release(struct se_frames_client *client, uint64_t epoch, uint32_t buffer, uint64_t seq)
{
	releases++;
	last_release = (struct se_frames_ready){.epoch = epoch, .buffer = buffer, .seq = seq};
}
static void test_dmabuf(struct se_frames_client *client, bool dmabuf)
{
	dmabuf_changes++;
	requested_dmabuf = dmabuf;
}
static bool test_import_current(struct se_frames_client *client, uint64_t epoch)
{
	return import_current;
}
static void test_poll(struct se_frames_client *client, struct se_frames_update *out)
{
	memset(out, 0, sizeof(*out));
}

static void reset_gpu(void)
{
	se_gl_fence_available();
	fence_sync = test_fence_sync;
	client_wait_sync = test_wait_sync;
	delete_sync = test_delete_sync;
	next_sync = 0;
	wait_result = 0x911B; /* GL_TIMEOUT_EXPIRED */
	fail_create = false;
	import_current = true;
	creates = deletes = releases = draws = imports = textures_destroyed = dmabuf_changes = effect_pass = 0;
	requested_dmabuf = true;
	memset(&last_release, 0, sizeof(last_release));
}

static struct se_frames_import *make_import(uint32_t fourcc, uint32_t count)
{
	struct se_frames_import *imp = calloc(1, sizeof(*imp));
	if (!imp)
		abort();
	imp->epoch = 7;
	imp->width = 64;
	imp->height = 32;
	imp->fourcc = fourcc;
	imp->buffer_count = count;
	imp->strides[0] = 256;
	for (uint32_t i = 0; i < SE_MAX_BUFFERS; i++)
		imp->fds[i] = -1;
	return imp;
}

static void test_fence_results(void)
{
	reset_gpu();
	CHECK(se_gl_fence_poll(NULL) == SE_GL_FENCE_FAILED, "missing fence is not completion");
	void *fence = se_gl_fence_create();
	CHECK(se_gl_fence_poll(fence) == SE_GL_FENCE_PENDING, "timeout is pending");
	wait_result = GL_WAIT_FAILED_;
	CHECK(se_gl_fence_poll(fence) == SE_GL_FENCE_FAILED, "wait error is not completion");
	wait_result = GL_ALREADY_SIGNALED_;
	CHECK(se_gl_fence_poll(fence) == SE_GL_FENCE_COMPLETE, "already-signaled fence completes");
	wait_result = GL_CONDITION_SATISFIED_;
	CHECK(se_gl_fence_poll(fence) == SE_GL_FENCE_COMPLETE, "satisfied fence completes");
	se_gl_fence_destroy(fence);
}

static void test_retirement_waits_and_recovers(void)
{
	reset_gpu();
	struct se_frames_import imp = {.epoch = 7, .buffer_count = 3};
	struct se_canvas_source s = {.imp = &imp, .cur = 0, .cur_seq = 10, .cur_drawn = true};
	s.cur_fence = se_gl_fence_create();
	CHECK(retire_current(&s), "retire sampled current buffer");
	for (unsigned int i = 0; i < 1200; i++)
		release_finished(&s);
	CHECK(releases == 0 && s.n_retired == 1 && creates == 1, "unsignaled fence survives arbitrary ticks");
	wait_result = GL_WAIT_FAILED_;
	for (unsigned int i = 0; i < 3; i++)
		release_finished(&s);
	CHECK(releases == 0 && s.n_retired == 1 && deletes == 3, "wait errors retry without releasing GPU hold");
	wait_result = GL_CONDITION_SATISFIED_;
	release_finished(&s);
	CHECK(releases == 1 && s.n_retired == 0 && creates == deletes, "repaired fence releases once on completion");
	CHECK(last_release.epoch == 7 && last_release.buffer == 0 && last_release.seq == 10, "release identity is preserved");
}

static void test_failed_draw_fence_recovers(void)
{
	reset_gpu();
	struct se_frames_import imp = {.epoch = 7, .width = 64, .height = 32, .buffer_count = 3};
	struct se_canvas_source s = {.imp = &imp, .cur = 0, .cur_seq = 10, .has_image = true};
	s.tex[0] = (gs_texture_t *)&texture_token;
	se_render(&s, NULL);
	CHECK(draws == 1 && s.cur_drawn && s.cur_fence, "first draw records its GPU use");
	fail_create = true;
	se_render(&s, NULL);
	CHECK(draws == 2 && s.cur_drawn && !s.cur_fence && deletes == 1, "failed newer fence cannot retain stale completion");
	struct se_frames_ready next = {.epoch = 7, .buffer = 1, .seq = 11};
	swap_in(&s, &next);
	for (unsigned int i = 0; i < 1200; i++)
		release_finished(&s);
	CHECK(releases == 0 && s.n_retired == 1, "a drawn buffer without a fence stays held");
	fail_create = false;
	release_finished(&s);
	CHECK(releases == 0 && s.retired[0].fence, "fence allocation retries after recovery");
	wait_result = GL_ALREADY_SIGNALED_;
	release_finished(&s);
	CHECK(releases == 1 && last_release.buffer == 0 && last_release.seq == 10, "recovered draw releases the original hold");
	CHECK(retire_current(&s), "retire never-drawn successor");
	unsigned int before = creates;
	release_finished(&s);
	CHECK(releases == 2 && last_release.buffer == 1 && creates == before, "never-drawn successor releases without a fence");
}

static void test_full_retirement_queue(void)
{
	reset_gpu();
	struct se_frames_import imp = {.epoch = 7, .buffer_count = 4};
	struct se_canvas_source s = {.imp = &imp, .cur = 2, .cur_seq = 20, .cur_drawn = true};
	s.cur_fence = se_gl_fence_create();
	s.n_retired = sizeof(s.retired) / sizeof(s.retired[0]);
	for (size_t i = 0; i < s.n_retired; i++)
		s.retired[i].drawn = true;
	struct se_frames_ready next = {.epoch = 7, .buffer = 3, .seq = 21};
	swap_in(&s, &next);
	CHECK(s.cur == 2 && s.cur_seq == 20 && s.cur_fence && s.cur_drawn, "full queue preserves the sampled current hold");
	CHECK(s.n_retired == sizeof(s.retired) / sizeof(s.retired[0]), "full queue stays bounded");
	CHECK(releases == 1 && last_release.buffer == 3 && last_release.seq == 21, "only never-sampled incoming frame is released");
	se_gl_fence_destroy(s.cur_fence);
}

static void test_missing_sync_uses_shm(void)
{
	reset_gpu();
	fence_sync = NULL;
	struct se_canvas_source s = {.cur = -1};
	apply_import(&s, make_import(SE_DRM_FORMAT_ABGR8888, 3));
	CHECK(!s.imp && imports == 0, "no dmabuf textures are created without GPU completion support");
	CHECK(dmabuf_changes == 1 && !requested_dmabuf, "missing sync support requests the safe shm path");
	fence_sync = test_fence_sync;
}

static void test_import_cache_and_teardown(void)
{
	reset_gpu();
	struct se_canvas_source s = {.cur = -1};
	apply_import(&s, make_import(SE_DRM_FORMAT_ABGR8888, 3));
	CHECK(imports == 3, "one texture import per pool buffer");
	for (uint64_t seq = 1; seq <= 12; seq++) {
		struct se_frames_ready next = {.epoch = 7, .buffer = (uint32_t)((seq - 1) % 3), .seq = seq};
		swap_in(&s, &next);
		release_finished(&s);
	}
	CHECK(imports == 3 && releases == 11, "swapping frames reuses the fixed texture cache");
	se_render(&s, NULL);
	CHECK(s.cur_drawn && s.cur_fence, "final cached texture draw has a fence");
	drop_render_state(&s, false);
	CHECK(textures_destroyed == 3 && creates == deletes && !s.imp && s.cur == -1 && !s.cur_drawn,
	      "teardown destroys every cached texture/fence and resets ownership");
	CHECK(releases == 11, "teardown does not claim pending GPU completion");
}

static void test_disconnect_stops_dmabuf_sampling(void)
{
	reset_gpu();
	struct se_canvas_source s = {.cur = -1, .client = (struct se_frames_client *)&client_token};
	apply_import(&s, make_import(SE_DRM_FORMAT_ABGR8888, 3));
	struct se_frames_ready next = {.epoch = 7, .buffer = 0, .seq = 1};
	swap_in(&s, &next);
	se_render(&s, NULL);
	CHECK(draws == 1 && s.cur_fence, "connected import is sampled");
	import_current = false;
	se_render(&s, NULL);
	CHECK(draws == 1, "invalidated import is not sampled again before the next tick");
	se_tick(&s, 0);
	CHECK(!s.imp && !s.has_image && s.cur == -1 && textures_destroyed == 3 && creates == deletes,
	      "tick drops the invalidated dmabuf cache without waiting for a reconnect import");
	CHECK(releases == 0, "disconnect teardown does not release in-flight GPU holds as complete");
}

static void test_disconnect_preserves_copied_shm_image(void)
{
	reset_gpu();
	struct se_canvas_source s = {.cur = -1, .client = (struct se_frames_client *)&client_token};
	apply_import(&s, make_import(0, 3));
	s.has_image = true; /* previously uploaded into OBS-owned storage */
	import_current = false;
	se_tick(&s, 0);
	se_render(&s, NULL);
	CHECK(s.imp && draws == 1 && creates == 0, "disconnected shm source can show its safe local last image");
	drop_render_state(&s, false);
}

int main(void)
{
	test_fence_results();
	test_retirement_waits_and_recovers();
	test_failed_draw_fence_recovers();
	test_full_retirement_queue();
	test_missing_sync_uses_shm();
	test_import_cache_and_teardown();
	test_disconnect_stops_dmabuf_sampling();
	test_disconnect_preserves_copied_shm_image();
	if (failures) {
		fprintf(stderr, "%d check(s) failed\n", failures);
		return 1;
	}
	printf("gpu-lifetime: all checks passed\n");
	return 0;
}
