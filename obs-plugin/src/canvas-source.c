/* SPDX-License-Identifier: GPL-2.0-or-later
 *
 * `stream-engine: wide`, `stream-engine: tall`, and `stream-engine: canvas` video sources.
 * Each instance owns a frames.sock client; the graphics thread imports the canvas buffers
 * (dmabuf → gs_texture_create_from_dmabuf, zero copy; or memfd → gs_texture_set_image),
 * swaps to the newest frame whose fence signaled, and releases a buffer to the engine only
 * after the GPU finished the draws that sampled it.
 */
#include "plugin.h"

#include <util/dstr.h>
#include <util/platform.h>

static uint32_t default_width(uint32_t canvas)
{
	return canvas == SE_CANVAS_TALL ? 1080 : 1920;
}

static uint32_t default_height(uint32_t canvas)
{
	return canvas == SE_CANVAS_TALL ? 1920 : 1080;
}

/* ---- registry ------------------------------------------------------------------------ */

static void registry_add(struct se_canvas_source *s)
{
	pthread_mutex_lock(&se_g.mu);
	if (se_g.n_sources < SE_MAX_SOURCES)
		se_g.sources[se_g.n_sources++] = s;
	else
		blog(LOG_WARNING, "[stream-engine] more than %d sources; '%s' is not monitored for staleness", SE_MAX_SOURCES,
		     obs_source_get_name(s->source));
	pthread_mutex_unlock(&se_g.mu);
}

static void registry_remove(struct se_canvas_source *s)
{
	pthread_mutex_lock(&se_g.mu);
	for (size_t i = 0; i < se_g.n_sources; i++) {
		if (se_g.sources[i] == s) {
			se_g.sources[i] = se_g.sources[--se_g.n_sources];
			break;
		}
	}
	pthread_mutex_unlock(&se_g.mu);
}

bool se_source_lookup(const obs_source_t *src, uint32_t *canvas, bool *stale)
{
	bool found = false;
	pthread_mutex_lock(&se_g.mu);
	for (size_t i = 0; i < se_g.n_sources; i++) {
		if (se_g.sources[i]->source == src) {
			if (canvas)
				*canvas = se_g.sources[i]->canvas;
			if (stale)
				*stale = atomic_load(&se_g.sources[i]->stale);
			found = true;
			break;
		}
	}
	pthread_mutex_unlock(&se_g.mu);
	return found;
}

void se_sources_monitor(uint64_t now, uint32_t stale_ms, bool trigger_allowed, struct se_monitor_result *r)
{
	memset(r, 0, sizeof(*r));
	const uint64_t limit = (uint64_t)stale_ms * 1000000ull;
	pthread_mutex_lock(&se_g.mu);
	for (size_t i = 0; i < se_g.n_sources; i++) {
		struct se_canvas_source *s = se_g.sources[i];
		if (!s->client)
			continue;
		struct se_frames_stats st;
		se_frames_client_stats(s->client, &st);
		const uint64_t since = st.last_frame_ns ? st.last_frame_ns : s->created_ns;
		const bool stale = st.goodbye || (now > since && now - since > limit);
		const bool was = atomic_exchange(&s->stale, stale);
		if (was && !stale)
			r->recovered = true;
		const uint32_t k = s->canvas;
		if (r->count[k]++ == 0 || st.last_frame_ns > r->stats[k].last_frame_ns)
			r->stats[k] = st;
		if (!stale) {
			r->fresh[k] = true;
			s->latched = false;
		} else if (trigger_allowed && !s->latched && obs_source_active(s->source)) {
			s->latched = true;
			r->engage = true;
		}
	}
	pthread_mutex_unlock(&se_g.mu);
}

/* ---- graphics-thread state ----------------------------------------------------------- */

static enum gs_color_format color_format(uint32_t fourcc)
{
	switch (fourcc) {
	case SE_DRM_FORMAT_ARGB8888:
		return GS_BGRA;
	case SE_DRM_FORMAT_XRGB8888:
		return GS_BGRX;
	default: /* ABGR8888 / XBGR8888 / shm RGBA8 */
		return GS_RGBA;
	}
}

static void release_retired_at(struct se_canvas_source *s, size_t i, bool send)
{
	se_gl_fence_destroy(s->retired[i].fence);
	if (send && s->imp)
		se_frames_client_release(s->client, s->imp->epoch, s->retired[i].buffer, s->retired[i].seq);
	s->retired[i] = s->retired[--s->n_retired];
}

/* Drops textures, fences, and the import. `send`: release held buffers to the engine. */
static void drop_render_state(struct se_canvas_source *s, bool send)
{
	while (s->n_retired)
		release_retired_at(s, s->n_retired - 1, send);
	if (s->cur >= 0 && send && s->imp)
		se_frames_client_release(s->client, s->imp->epoch, (uint32_t)s->cur, s->cur_seq);
	se_gl_fence_destroy(s->cur_fence);
	s->cur_fence = NULL;
	s->cur_drawn = false;
	s->cur = -1;
	for (uint32_t i = 0; i < SE_MAX_BUFFERS; i++) {
		gs_texture_destroy(s->tex[i]);
		s->tex[i] = NULL;
	}
	se_frames_import_free(s->imp);
	s->imp = NULL;
	s->has_image = false;
}

static void apply_import(struct se_canvas_source *s, struct se_frames_import *imp)
{
	/* buffers of the previous canvas belong to an engine generation that no longer exists */
	drop_render_state(s, false);
	s->imp = imp;
	s->shm = imp->fourcc == 0;
	if (!s->shm && !se_gl_fence_available()) {
		drop_render_state(s, false);
		se_frames_client_set_dmabuf(s->client, false);
		return;
	}
	if (s->shm) {
		s->tex[0] = gs_texture_create(imp->width, imp->height, GS_RGBA, 1, NULL, GS_DYNAMIC);
		if (!s->tex[0]) {
			blog(LOG_ERROR, "[stream-engine] %s: cannot create a %ux%u texture", obs_source_get_name(s->source),
			     imp->width, imp->height);
			drop_render_state(s, false);
			return;
		}
	} else {
		for (uint32_t i = 0; i < imp->buffer_count; i++) {
			int fd = imp->fds[i];
			uint32_t stride = imp->strides[0], offset = imp->offsets[0];
			uint64_t modifier = imp->modifier;
			s->tex[i] = gs_texture_create_from_dmabuf(imp->width, imp->height, imp->fourcc, color_format(imp->fourcc), 1,
								  &fd, &stride, &offset, &modifier);
			if (!s->tex[i]) {
				blog(LOG_WARNING,
				     "[stream-engine] %s: dmabuf import failed (%ux%u fourcc 0x%08x modifier 0x%016llx); "
				     "switching to the shared-memory path",
				     obs_source_get_name(s->source), imp->width, imp->height, imp->fourcc,
				     (unsigned long long)imp->modifier);
				drop_render_state(s, false);
				se_frames_client_set_dmabuf(s->client, false);
				return;
			}
		}
	}
	atomic_store(&s->width, imp->width);
	atomic_store(&s->height, imp->height);
	blog(LOG_INFO, "[stream-engine] %s: imported %ux%u (%s, %u buffers)", obs_source_get_name(s->source), imp->width,
	     imp->height, s->shm ? "shm" : "dmabuf", imp->buffer_count);
}

static bool retire_current(struct se_canvas_source *s)
{
	if (s->cur < 0)
		return true;
	if (s->n_retired == sizeof(s->retired) / sizeof(s->retired[0]))
		return false;
	s->retired[s->n_retired++] =
		(typeof(s->retired[0])){(uint32_t)s->cur, s->cur_seq, s->cur_fence, s->cur_drawn};
	s->cur_fence = NULL;
	s->cur_drawn = false;
	s->cur = -1;
	return true;
}

static void swap_in(struct se_canvas_source *s, const struct se_frames_ready *f)
{
	const struct se_frames_import *imp = s->imp;
	if (f->buffer >= imp->buffer_count)
		return;
	if (s->shm) {
		/* gs_texture_set_image copies synchronously, so the buffer is free right after */
		gs_texture_set_image(s->tex[0], imp->maps[f->buffer] + imp->offsets[0], imp->strides[0], false);
		se_frames_client_release(s->client, imp->epoch, f->buffer, f->seq);
		s->has_image = true;
		return;
	}
	if (!retire_current(s)) {
		/* No room to track another GPU hold: discard the never-sampled new frame. */
		se_frames_client_release(s->client, imp->epoch, f->buffer, f->seq);
		return;
	}
	s->cur = (int32_t)f->buffer;
	s->cur_seq = f->seq;
	s->has_image = true;
}

static void release_finished(struct se_canvas_source *s)
{
	for (size_t i = 0; i < s->n_retired;) {
		bool done = !s->retired[i].drawn;
		if (!done) {
			/* A replacement fence in the same context covers every earlier draw too.
			 * Retry allocation/wait errors; elapsed ticks never prove GPU completion. */
			if (!s->retired[i].fence)
				s->retired[i].fence = se_gl_fence_create();
			enum se_gl_fence_status status = se_gl_fence_poll(s->retired[i].fence);
			done = status == SE_GL_FENCE_COMPLETE;
			if (status == SE_GL_FENCE_FAILED) {
				se_gl_fence_destroy(s->retired[i].fence);
				s->retired[i].fence = NULL;
			}
		}
		if (done)
			release_retired_at(s, i, true);
		else
			i++;
	}
}

/* ---- obs_source_info callbacks ------------------------------------------------------- */

static void start_client(struct se_canvas_source *s)
{
	char path[108];
	if (!se_runtime_path("frames.sock", path, sizeof(path))) {
		blog(LOG_ERROR, "[stream-engine] frames.sock path too long");
		return;
	}
	struct se_frames_opts o = {
		.socket_path = path,
		.canvas = s->canvas,
		.client_kind = SE_CLIENT_OBS,
		.dmabuf = s->dmabuf_pref,
		.log = se_log,
	};
	s->client = se_frames_client_start(&o);
	if (!s->client)
		blog(LOG_ERROR, "[stream-engine] cannot start the frames client for %s", se_canvas_name(s->canvas));
}

static uint32_t settings_canvas(obs_data_t *settings)
{
	long long c = obs_data_get_int(settings, "canvas");
	return c >= 0 && c < SE_CANVAS_COUNT ? (uint32_t)c : SE_CANVAS_WIDE;
}

static void *create_common(obs_data_t *settings, obs_source_t *source, uint32_t canvas, bool generic)
{
	struct se_canvas_source *s = bzalloc(sizeof(*s));
	s->source = source;
	s->generic = generic;
	s->canvas = generic ? settings_canvas(settings) : canvas;
	s->dmabuf_pref = obs_data_get_bool(settings, "dmabuf");
	s->created_ns = se_mono_ns();
	s->cur = -1;
	atomic_init(&s->stale, true);
	atomic_init(&s->width, 0);
	atomic_init(&s->height, 0);
	start_client(s);
	if (!s->client) {
		bfree(s);
		return NULL;
	}
	registry_add(s);
	return s;
}

static void *create_wide(obs_data_t *settings, obs_source_t *source)
{
	return create_common(settings, source, SE_CANVAS_WIDE, false);
}

static void *create_tall(obs_data_t *settings, obs_source_t *source)
{
	return create_common(settings, source, SE_CANVAS_TALL, false);
}

static void *create_generic(obs_data_t *settings, obs_source_t *source)
{
	return create_common(settings, source, SE_CANVAS_WIDE, true);
}

static void se_destroy(void *data)
{
	struct se_canvas_source *s = data;
	registry_remove(s);
	obs_enter_graphics();
	drop_render_state(s, false);
	obs_leave_graphics();
	se_frames_client_stop(s->client);
	bfree(s);
}

static void se_update(void *data, obs_data_t *settings)
{
	struct se_canvas_source *s = data;
	bool dmabuf = obs_data_get_bool(settings, "dmabuf");
	uint32_t canvas = s->generic ? settings_canvas(settings) : s->canvas;
	if (canvas != s->canvas) {
		/* the graphics lock serializes with video_tick, the registry lock with the monitor */
		obs_enter_graphics();
		pthread_mutex_lock(&se_g.mu);
		drop_render_state(s, false);
		struct se_frames_client *old = s->client;
		s->client = NULL;
		s->canvas = canvas;
		s->dmabuf_pref = dmabuf;
		s->created_ns = se_mono_ns();
		s->latched = false;
		atomic_store(&s->width, 0);
		atomic_store(&s->height, 0);
		start_client(s);
		pthread_mutex_unlock(&se_g.mu);
		obs_leave_graphics();
		se_frames_client_stop(old);
		return;
	}
	if (dmabuf != s->dmabuf_pref) {
		s->dmabuf_pref = dmabuf;
		se_frames_client_set_dmabuf(s->client, dmabuf);
	}
}

static void se_tick(void *data, float seconds)
{
	UNUSED_PARAMETER(seconds);
	struct se_canvas_source *s = data;
	if (!s->client)
		return;
	obs_enter_graphics();
	struct se_frames_update up;
	se_frames_client_poll(s->client, &up);
	if (up.import)
		apply_import(s, up.import);
	const bool current = s->imp && se_frames_client_import_current(s->client, s->imp->epoch);
	if (s->imp && !s->shm && !current)
		drop_render_state(s, false);
	if (up.has_frame && current && s->imp && up.frame.epoch == s->imp->epoch)
		swap_in(s, &up.frame);
	else if (up.has_frame)
		se_frames_client_release(s->client, up.frame.epoch, up.frame.buffer, up.frame.seq);
	release_finished(s);
	obs_leave_graphics();
}

static void se_render(void *data, gs_effect_t *unused)
{
	UNUSED_PARAMETER(unused);
	struct se_canvas_source *s = data;
	if (!s->has_image || !s->imp)
		return;
	if (!s->shm && !se_frames_client_import_current(s->client, s->imp->epoch))
		return;
	gs_texture_t *tex = s->shm ? s->tex[0] : (s->cur >= 0 ? s->tex[s->cur] : NULL);
	if (!tex)
		return;

	const bool linear_srgb = gs_get_linear_srgb();
	const bool previous = gs_framebuffer_srgb_enabled();
	gs_enable_framebuffer_srgb(linear_srgb);
	gs_effect_t *effect = obs_get_base_effect(OBS_EFFECT_DEFAULT);
	gs_eparam_t *image = gs_effect_get_param_by_name(effect, "image");
	/* EGL DMA-BUF images replace the texture's sRGB storage with plain RGBA.
	 * Decode those samples in the shader; uploaded shm textures retain sRGB storage. */
	const bool shader_decode = linear_srgb && !s->shm;
	if (linear_srgb && s->shm)
		gs_effect_set_texture_srgb(image, tex);
	else
		gs_effect_set_texture(image, tex);
	const bool opaque = s->imp->fourcc == SE_DRM_FORMAT_XBGR8888 || s->imp->fourcc == SE_DRM_FORMAT_XRGB8888;
	if (opaque) {
		gs_blend_state_push();
		gs_enable_blending(false);
	}
	while (gs_effect_loop(effect, shader_decode ? "DrawSrgbDecompress" : "Draw"))
		gs_draw_sprite(tex, 0, s->imp->width, s->imp->height);
	if (opaque)
		gs_blend_state_pop();
	gs_enable_framebuffer_srgb(previous);

	if (!s->shm) {
		s->cur_drawn = true;
		se_gl_fence_destroy(s->cur_fence);
		s->cur_fence = se_gl_fence_create();
	}
}

static uint32_t se_width(void *data)
{
	struct se_canvas_source *s = data;
	uint32_t w = atomic_load(&s->width);
	return w ? w : default_width(s->canvas);
}

static uint32_t se_height(void *data)
{
	struct se_canvas_source *s = data;
	uint32_t h = atomic_load(&s->height);
	return h ? h : default_height(s->canvas);
}

static void se_defaults(obs_data_t *settings)
{
	obs_data_set_default_bool(settings, "dmabuf", true);
	obs_data_set_default_int(settings, "canvas", SE_CANVAS_WIDE);
}

static obs_properties_t *props_common(bool generic)
{
	obs_properties_t *props = obs_properties_create();
	if (generic) {
		obs_property_t *p = obs_properties_add_list(props, "canvas", obs_module_text("Canvas"), OBS_COMBO_TYPE_LIST,
							    OBS_COMBO_FORMAT_INT);
		for (uint32_t i = 0; i < SE_CANVAS_COUNT; i++)
			obs_property_list_add_int(p, se_canvas_name(i), i);
	}
	obs_properties_add_bool(props, "dmabuf", obs_module_text("ZeroCopy"));
	char path[108];
	if (se_runtime_path("frames.sock", path, sizeof(path))) {
		struct dstr info = {0};
		dstr_printf(&info, "%s %s", obs_module_text("Socket"), path);
		obs_properties_add_text(props, "info", info.array, OBS_TEXT_INFO);
		dstr_free(&info);
	}
	return props;
}

static obs_properties_t *props_fixed(void *data)
{
	UNUSED_PARAMETER(data);
	return props_common(false);
}

static obs_properties_t *props_generic(void *data)
{
	UNUSED_PARAMETER(data);
	return props_common(true);
}

static const char *name_wide(void *type_data)
{
	UNUSED_PARAMETER(type_data);
	return "stream-engine: wide";
}

static const char *name_tall(void *type_data)
{
	UNUSED_PARAMETER(type_data);
	return "stream-engine: tall";
}

static const char *name_generic(void *type_data)
{
	UNUSED_PARAMETER(type_data);
	return "stream-engine: canvas";
}

void se_register_sources(void)
{
	struct obs_source_info base = {
		.type = OBS_SOURCE_TYPE_INPUT,
		.output_flags = OBS_SOURCE_VIDEO | OBS_SOURCE_CUSTOM_DRAW | OBS_SOURCE_SRGB | OBS_SOURCE_DO_NOT_DUPLICATE,
		.destroy = se_destroy,
		.update = se_update,
		.video_tick = se_tick,
		.video_render = se_render,
		.get_width = se_width,
		.get_height = se_height,
		.get_defaults = se_defaults,
		.icon_type = OBS_ICON_TYPE_CUSTOM,
	};

	struct obs_source_info wide = base;
	wide.id = "stream_engine_wide";
	wide.get_name = name_wide;
	wide.create = create_wide;
	wide.get_properties = props_fixed;
	obs_register_source(&wide);

	struct obs_source_info tall = base;
	tall.id = "stream_engine_tall";
	tall.get_name = name_tall;
	tall.create = create_tall;
	tall.get_properties = props_fixed;
	obs_register_source(&tall);

	struct obs_source_info generic = base;
	generic.id = "stream_engine_canvas";
	generic.get_name = name_generic;
	generic.create = create_generic;
	generic.get_properties = props_generic;
	obs_register_source(&generic);
}
