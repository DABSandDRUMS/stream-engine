/* SPDX-License-Identifier: GPL-2.0-or-later
 *
 * stream-engine OBS module: registers the canvas sources and runs the obs.sock control
 * channel (hello, 1 Hz streaming status, events, commands) plus the stale-feed
 * monitor that drives the fallback scene. Nothing here blocks OBS's render thread: the
 * control thread only reads thread-safe libobs/frontend state and hands scene switches and
 * start/stop commands to the UI thread with obs_queue_task().
 */
#include "plugin.h"

#include <inttypes.h>
#include <obs-frontend-api.h>
#include <util/dstr.h>
#include <util/platform.h>
#include <unistd.h>

OBS_DECLARE_MODULE()
OBS_MODULE_USE_DEFAULT_LOCALE("stream-engine", "en-US")
OBS_MODULE_AUTHOR("stream-engine")

MODULE_EXPORT const char *obs_module_description(void)
{
	return "stream-engine canvas sources (dmabuf/shm) and engine control channel";
}

struct se_plugin se_g;

static atomic_bool loaded;        /* frontend finished loading the scene collection */
static atomic_bool force_status;  /* send a status on the next tick */
static atomic_bool engage_queued; /* coalesce UI tasks */
static atomic_bool restore_queued;

/* ---- logging + config ---------------------------------------------------------------- */

void se_log(void *ud, int level, const char *msg)
{
	UNUSED_PARAMETER(ud);
	blog(level, "[stream-engine] %s", msg);
}

static const char *mode_name(enum se_fallback_mode m)
{
	switch (m) {
	case SE_FALLBACK_OFF:
		return "off";
	case SE_FALLBACK_ALWAYS:
		return "always";
	default:
		return "live";
	}
}

static bool parse_mode(const char *s, enum se_fallback_mode *out)
{
	if (!s)
		return false;
	if (strcmp(s, "off") == 0)
		*out = SE_FALLBACK_OFF;
	else if (strcmp(s, "live") == 0)
		*out = SE_FALLBACK_LIVE;
	else if (strcmp(s, "always") == 0)
		*out = SE_FALLBACK_ALWAYS;
	else
		return false;
	return true;
}

void se_config_copy(struct se_config *out)
{
	pthread_mutex_lock(&se_g.mu);
	*out = se_g.config;
	pthread_mutex_unlock(&se_g.mu);
}

uint64_t se_stale_ms(void)
{
	pthread_mutex_lock(&se_g.mu);
	uint64_t v = se_g.config.stale_ms;
	pthread_mutex_unlock(&se_g.mu);
	return v;
}

static void config_defaults(struct se_config *c)
{
	c->stale_ms = 500;
	c->vertical_enabled = true;
	c->vertical_revision = 0;
	c->fallback_mode = SE_FALLBACK_LIVE;
	snprintf(c->fallback_scene, sizeof(c->fallback_scene), "%s", SE_DEFAULT_FALLBACK_SCENE);
	snprintf(c->fallback_text, sizeof(c->fallback_text), "%s", SE_DEFAULT_FALLBACK_TEXT);
}

/* Applies known keys from a JSON object; returns true if anything changed. */
static bool config_apply_json(struct se_config *c, json_t *o)
{
	bool changed = false;
	json_t *v;
	if (json_is_integer(v = json_object_get(o, "vertical_revision")))
		c->vertical_revision = (uint64_t)json_integer_value(v);
	if (json_is_boolean(v = json_object_get(o, "vertical_enabled"))) {
		bool enabled = json_is_true(v);
		changed |= enabled != c->vertical_enabled;
		c->vertical_enabled = enabled;
	}
	if (json_is_integer(v = json_object_get(o, "stale_ms"))) {
		json_int_t ms = json_integer_value(v);
		uint32_t clamped = ms < 50 ? 50 : ms > 60000 ? 60000 : (uint32_t)ms;
		changed |= clamped != c->stale_ms;
		c->stale_ms = clamped;
	}
	enum se_fallback_mode m;
	if (parse_mode(json_string_value(json_object_get(o, "fallback_mode")), &m)) {
		changed |= m != c->fallback_mode;
		c->fallback_mode = m;
	}
	const char *s = json_string_value(json_object_get(o, "fallback_scene"));
	if (s && *s && strcmp(s, c->fallback_scene) != 0) {
		snprintf(c->fallback_scene, sizeof(c->fallback_scene), "%s", s);
		changed = true;
	}
	s = json_string_value(json_object_get(o, "fallback_text"));
	if (s && *s && strcmp(s, c->fallback_text) != 0) {
		snprintf(c->fallback_text, sizeof(c->fallback_text), "%s", s);
		changed = true;
	}
	return changed;
}

static json_t *config_json(const struct se_config *c)
{
	return json_pack("{s:i, s:b, s:s, s:s, s:s}", "stale_ms", (int)c->stale_ms,
			 "vertical_enabled", c->vertical_enabled, "fallback_mode", mode_name(c->fallback_mode),
			 "fallback_scene", c->fallback_scene, "fallback_text", c->fallback_text);
}

static void plugin_config_load(void)
{
	struct se_config c;
	config_defaults(&c);
	char *path = obs_module_config_path("config.json");
	if (path && os_file_exists(path)) {
		json_error_t err;
		json_t *o = json_load_file(path, 0, &err);
		if (o) {
			config_apply_json(&c, o);
			json_decref(o);
		} else {
			blog(LOG_WARNING, "[stream-engine] ignoring %s: %s", path, err.text);
		}
	}
	bfree(path);
	pthread_mutex_lock(&se_g.mu);
	se_g.config = c;
	pthread_mutex_unlock(&se_g.mu);
}

static void plugin_config_save(const struct se_config *c)
{
	char *dir = obs_module_config_path("");
	char *path = obs_module_config_path("config.json");
	struct dstr tmp = {0};
	if (!dir || !path)
		goto out;
	os_mkdirs(dir);
	dstr_printf(&tmp, "%s.tmp", path);
	json_t *o = config_json(c);
	if (json_dump_file(o, tmp.array, JSON_INDENT(2)) == 0)
		os_rename(tmp.array, path);
	else
		blog(LOG_WARNING, "[stream-engine] cannot write %s", tmp.array);
	json_decref(o);
out:
	dstr_free(&tmp);
	bfree(dir);
	bfree(path);
}

/* ---- small helpers ------------------------------------------------------------------- */

static void send_json(json_t *m)
{
	if (m && se_g.control)
		se_control_send(se_g.control, m);
	json_decref(m);
}

/* OBS clock (os_gettime_ns) and CLOCK_MONOTONIC sampled together: the obs value sits at the
 * midpoint of two monotonic reads. */
static void clock_pair(uint64_t *obs_ns, uint64_t *mono_ns)
{
	uint64_t m0 = se_mono_ns();
	*obs_ns = os_gettime_ns();
	uint64_t m1 = se_mono_ns();
	*mono_ns = m0 + (m1 - m0) / 2;
}

void se_stamp(json_t *m)
{
	if (!m)
		return;
	uint64_t obs_ns, mono_ns;
	clock_pair(&obs_ns, &mono_ns);
	json_object_set_new(m, "obs_ns", json_integer((json_int_t)obs_ns));
	json_object_set_new(m, "mono_ns", json_integer((json_int_t)mono_ns));
}

static uint64_t frame_interval_ns(video_t *video)
{
	const struct video_output_info *vi = video ? video_output_get_info(video) : NULL;
	if (!vi || !vi->fps_num)
		return 16666667ull;
	return (uint64_t)vi->fps_den * 1000000000ull / vi->fps_num;
}

static char *weak_output_name(obs_weak_output_t *weak, char *out, size_t len)
{
	out[0] = 0;
	obs_output_t *o = obs_weak_output_get_output(weak);
	if (o) {
		snprintf(out, len, "%s", obs_output_get_name(o));
		obs_output_release(o);
	}
	return out;
}

/* ---- output tracking (control thread) ------------------------------------------------ */

#define MAX_TRACKED 16

struct tracked {
	bool used, seen;
	char name[128];
	char id[64];
	obs_weak_output_t *weak;
	video_t *video;
	char canvas[64];
	bool service;
	bool active;
	uint64_t start_obs_ns;
	/* stats */
	uint64_t prev_bytes, prev_ns;
	double kbps;
	int dropped, total;
	float congestion;
};

static struct tracked tracked[MAX_TRACKED];

struct enum_ctx {
	obs_weak_output_t *weak[MAX_TRACKED];
	size_t n;
};

static bool collect_output(void *param, obs_output_t *o)
{
	struct enum_ctx *e = param;
	const char *id = obs_output_get_id(o);
	if (!id || strcmp(id, "virtualcam_output") == 0 || strcmp(id, "replay_buffer") == 0)
		return true;
	if (e->n < MAX_TRACKED)
		e->weak[e->n++] = obs_output_get_weak_output(o);
	return true;
}

static struct tracked *track_slot(const char *name)
{
	struct tracked *free_slot = NULL;
	for (size_t i = 0; i < MAX_TRACKED; i++) {
		if (tracked[i].used && strcmp(tracked[i].name, name) == 0)
			return &tracked[i];
		if (!tracked[i].used && !free_slot)
			free_slot = &tracked[i];
	}
	if (free_slot) {
		memset(free_slot, 0, sizeof(*free_slot));
		free_slot->used = true;
		snprintf(free_slot->name, sizeof(free_slot->name), "%s", name);
	}
	return free_slot;
}

static void untrack(struct tracked *t)
{
	obs_weak_output_release(t->weak);
	memset(t, 0, sizeof(*t));
}

static void track_outputs(void)
{
	struct enum_ctx e = {0};
	obs_enum_outputs(collect_output, &e);
	for (size_t i = 0; i < MAX_TRACKED; i++)
		tracked[i].seen = false;

	for (size_t i = 0; i < e.n; i++) {
		obs_output_t *o = obs_weak_output_get_output(e.weak[i]);
		obs_weak_output_release(e.weak[i]);
		if (!o)
			continue;
		const uint32_t flags = obs_output_get_flags(o);
		const bool active = obs_output_active(o);
		struct tracked *t = track_slot(obs_output_get_name(o));
		if (!t) {
			obs_output_release(o);
			continue;
		}
		t->seen = true;
		if (!t->weak) {
			t->weak = obs_output_get_weak_output(o);
			snprintf(t->id, sizeof(t->id), "%s", obs_output_get_id(o));
		}
		t->service = (flags & OBS_OUTPUT_SERVICE) != 0;
		/* an idle encoded output has no encoder yet (obs_output_video would log an error) */
		video_t *video = active ? obs_output_video(o) : t->video;
		if (active && (video != t->video || !t->canvas[0])) {
			t->video = video;
			se_canvas_kind_for_video(video, t->canvas, sizeof(t->canvas));
		}
		const uint64_t now_obs = os_gettime_ns();
		if (active && !t->active) {
			/* stream t=0 is the first encoded frame: back-date by the frames sent so far */
			uint64_t back = (uint64_t)(obs_output_get_total_frames(o) > 0 ? obs_output_get_total_frames(o) : 0) *
					frame_interval_ns(video);
			t->start_obs_ns = now_obs > back ? now_obs - back : now_obs;
			t->prev_bytes = obs_output_get_total_bytes(o);
			t->prev_ns = se_mono_ns();
			t->kbps = 0;
		}
		t->active = active;
		obs_output_release(o);
	}
	for (size_t i = 0; i < MAX_TRACKED; i++)
		if (tracked[i].used && !tracked[i].seen)
			untrack(&tracked[i]);
}

static void update_output_stats(uint64_t now_mono)
{
	for (size_t i = 0; i < MAX_TRACKED; i++) {
		struct tracked *t = &tracked[i];
		if (!t->used)
			continue;
		obs_output_t *o = obs_weak_output_get_output(t->weak);
		if (!o)
			continue;
		uint64_t bytes = obs_output_get_total_bytes(o);
		if (t->active && bytes >= t->prev_bytes && now_mono > t->prev_ns) {
			double secs = (double)(now_mono - t->prev_ns) / 1e9;
			t->kbps = secs > 0.0 ? (double)(bytes - t->prev_bytes) * 8.0 / 1000.0 / secs : 0.0;
		} else {
			t->kbps = 0.0;
		}
		t->prev_bytes = bytes;
		t->prev_ns = now_mono;
		t->dropped = obs_output_get_frames_dropped(o);
		t->total = obs_output_get_total_frames(o);
		t->congestion = t->active && t->service ? obs_output_get_congestion(o) : 0.0f;
		obs_output_release(o);
	}
}

static struct tracked *tracked_by_name(const char *name)
{
	if (!name || !*name)
		return NULL;
	for (size_t i = 0; i < MAX_TRACKED; i++)
		if (tracked[i].used && strcmp(tracked[i].name, name) == 0)
			return &tracked[i];
	return NULL;
}

static bool any_output_live(void)
{
	for (size_t i = 0; i < MAX_TRACKED; i++)
		if (tracked[i].used && tracked[i].service && tracked[i].active)
			return true;
	return false;
}

/* ---- status (control thread) --------------------------------------------------------- */

static struct se_monitor_result last_mon;
static uint32_t prev_skipped;
static uint64_t prev_status_ns;

static void send_status(uint64_t now_mono, const struct se_monitor_result *mon)
{
	update_output_stats(now_mono);

	uint64_t obs_ns, mono_ns;
	clock_pair(&obs_ns, &mono_ns);

	char stream_name[128];
	pthread_mutex_lock(&se_g.mu);
	obs_weak_output_t *sw = se_g.stream_output;
	char scene[256];
	snprintf(scene, sizeof(scene), "%s", se_g.scene_name);
	obs_weak_output_addref(sw);
	pthread_mutex_unlock(&se_g.mu);
	weak_output_name(sw, stream_name, sizeof(stream_name));
	obs_weak_output_release(sw);
	struct tracked *st = tracked_by_name(stream_name);

	video_t *video = obs_get_video();
	uint32_t skipped = video ? video_output_get_skipped_frames(video) : 0;
	uint32_t encoded = video ? video_output_get_total_frames(video) : 0;
	double lag_ms = 0.0;
	if (skipped >= prev_skipped && prev_status_ns)
		lag_ms = (double)(skipped - prev_skipped) * (double)frame_interval_ns(video) / 1e6;
	prev_skipped = skipped;
	prev_status_ns = now_mono;

	json_t *stale = json_object(), *sources = json_object(), *feeds = json_object();
	for (uint32_t k = 0; k < SE_CANVAS_COUNT; k++) {
		const char *name = se_canvas_name(k);
		json_object_set_new(sources, name, json_integer(mon->count[k]));
		if (k <= SE_CANVAS_TALL || mon->count[k])
			json_object_set_new(stale, name, json_boolean(!mon->fresh[k]));
		if (!mon->count[k])
			continue;
		const struct se_frames_stats *fs = &mon->stats[k];
		json_object_set_new(feeds, name,
				    json_pack("{s:b, s:I, s:I, s:I, s:i, s:i, s:b, s:b, s:I}", "connected", fs->connected,
					      "frames", (json_int_t)fs->frames, "superseded", (json_int_t)fs->superseded,
					      "fence_timeouts", (json_int_t)fs->fence_timeouts, "width", (int)fs->width,
					      "height", (int)fs->height, "dmabuf", fs->fourcc != 0, "goodbye", fs->goodbye,
					      "age_ms",
					      (json_int_t)(fs->last_frame_ns && now_mono > fs->last_frame_ns
								   ? (now_mono - fs->last_frame_ns) / 1000000ull
								   : 0)));
	}

	json_t *outputs = json_array();
	for (size_t i = 0; i < MAX_TRACKED; i++) {
		struct tracked *t = &tracked[i];
		if (!t->used || (!t->service && !t->active))
			continue;
		json_t *o = json_pack("{s:s, s:s, s:s, s:b, s:f, s:i, s:i, s:f, s:s}", "name", t->name, "id", t->id, "kind",
				      t->service ? "stream" : "record", "active", t->active, "kbps", t->kbps, "dropped",
				      t->dropped, "total", t->total, "congestion", (double)t->congestion, "canvas", t->canvas);
		json_array_append_new(outputs, o);
	}

	json_t *m = json_object();
	json_object_set_new(m, "t", json_string("status"));
	json_object_set_new(m, "streaming", json_boolean(obs_frontend_streaming_active()));
	json_object_set_new(m, "kbps", json_real(st && st->active ? st->kbps : 0.0));
	json_object_set_new(m, "dropped", json_integer(st ? st->dropped : 0));
	json_object_set_new(m, "total", json_integer(st ? st->total : 0));
	json_object_set_new(m, "congestion", json_real(st && st->active ? st->congestion : 0.0));
	json_object_set_new(m, "lag_ms", json_real(lag_ms));
	json_object_set_new(m, "fps", json_real(obs_get_active_fps()));
	json_object_set_new(m, "render_ms", json_real((double)obs_get_average_frame_time_ns() / 1e6));
	json_object_set_new(m, "lagged", json_integer(obs_get_lagged_frames()));
	json_object_set_new(m, "rendered", json_integer(obs_get_total_frames()));
	json_object_set_new(m, "skipped", json_integer(skipped));
	json_object_set_new(m, "encoded", json_integer(encoded));
	json_object_set_new(m, "obs_ns", json_integer((json_int_t)obs_ns));
	json_object_set_new(m, "mono_ns", json_integer((json_int_t)mono_ns));
	json_object_set_new(m, "stream_start_ns", json_integer(st && st->active ? (json_int_t)st->start_obs_ns : 0));
	json_object_set_new(m, "scene", json_string(scene));
	json_object_set_new(m, "stale", stale);
	json_object_set_new(m, "sources", sources);
	json_object_set_new(m, "feeds", feeds);
	json_object_set_new(m, "fallback", json_boolean(atomic_load(&se_g.fallback_engaged)));
	json_object_set_new(m, "outputs", outputs);
	json_t *vertical = se_vertical_status();
	struct se_config cfg;
	se_config_copy(&cfg);
	json_object_set_new(vertical, "revision", json_integer((json_int_t)cfg.vertical_revision));
	json_object_set_new(m, "vertical", vertical);
	send_json(m);
}

/* ---- UI-thread tasks ------------------------------------------------------------------ */

static void task_engage(void *param)
{
	UNUSED_PARAMETER(param);
	atomic_store(&engage_queued, false);
	if (atomic_load(&se_g.exiting) || !atomic_load(&loaded))
		return;
	char err[256];
	if (se_fallback_engage(SE_REASON_STALE, err, sizeof(err)) > 0)
		atomic_store(&force_status, true);
}

static void task_restore(void *param)
{
	UNUSED_PARAMETER(param);
	atomic_store(&restore_queued, false);
	if (atomic_load(&se_g.exiting) || !atomic_load(&loaded))
		return;
	if (se_fallback_restore(false) > 0)
		atomic_store(&force_status, true);
}

struct cmd_task {
	json_int_t id;
	char op[64];
};

static void reply(json_int_t id, bool ok, const char *error, json_t *result)
{
	json_t *m = json_pack("{s:s, s:I, s:b}", "t", "reply", "id", id, "ok", ok);
	json_object_set_new(m, "error", error ? json_string(error) : json_null());
	if (result)
		json_object_set_new(m, "result", result);
	send_json(m);
}

static void task_cmd(void *param)
{
	struct cmd_task *t = param;
	const char *op = t->op;
	char err[256] = {0};
	if (atomic_load(&se_g.exiting)) {
		reply(t->id, false, "OBS is shutting down", NULL);
	} else if (!atomic_load(&loaded)) {
		reply(t->id, false, "OBS is still loading its scene collection", NULL);
	} else if (strcmp(op, "stream.start") == 0) {
		bool already = obs_frontend_streaming_active();
		if (!already)
			obs_frontend_streaming_start();
		reply(t->id, true, NULL, json_string(already ? "already streaming" : "starting"));
	} else if (strcmp(op, "stream.stop") == 0) {
		bool active = obs_frontend_streaming_active();
		if (active)
			obs_frontend_streaming_stop();
		reply(t->id, true, NULL, json_string(active ? "stopping" : "not streaming"));
	} else if (strcmp(op, "fallback.on") == 0) {
		int n = se_fallback_engage(SE_REASON_MANUAL, err, sizeof(err));
		atomic_store(&force_status, true);
		if (n > 0)
			reply(t->id, true, NULL, json_integer(n));
		else
			reply(t->id, false, err[0] ? err : "nothing to switch", NULL);
	} else if (strcmp(op, "fallback.off") == 0) {
		int n = se_fallback_restore(true);
		atomic_store(&force_status, true);
		reply(t->id, true, NULL, json_integer(n));
	} else if (strcmp(op, "fallback.setup") == 0) {
		int n = se_fallback_prepare(err, sizeof(err));
		if (err[0])
			reply(t->id, false, err, NULL);
		else
			reply(t->id, true, NULL, json_integer(n));
	} else if (strcmp(op, "setup") == 0) {
		reply(t->id, true, NULL, se_setup_sources());
	} else {
		char msg[128];
		snprintf(msg, sizeof(msg), "unknown op '%s'", op);
		reply(t->id, false, msg, NULL);
	}
	bfree(t);
}

/* ---- control callbacks (control thread) ---------------------------------------------- */

static json_t *source_kinds(void)
{
	json_t *a = json_array();
	uint32_t seen = 0;
	pthread_mutex_lock(&se_g.mu);
	for (size_t i = 0; i < se_g.n_sources; i++)
		seen |= 1u << se_g.sources[i]->canvas;
	pthread_mutex_unlock(&se_g.mu);
	for (uint32_t k = 0; k < SE_CANVAS_COUNT; k++)
		if (seen & (1u << k))
			json_array_append_new(a, json_string(se_canvas_name(k)));
	return a;
}

static void on_connect(void *ud)
{
	UNUSED_PARAMETER(ud);
	struct se_config cfg;
	se_config_copy(&cfg);
	json_t *m = json_pack("{s:s, s:s, s:s, s:o, s:i, s:o}", "t", "hello", "obs", obs_get_version_string(), "plugin",
			      SE_PLUGIN_VERSION, "canvases", source_kinds(), "pid", (int)getpid(), "config",
			      config_json(&cfg));
	send_json(m);
	atomic_store(&force_status, true);
}

static void on_disconnect(void *ud)
{
	UNUSED_PARAMETER(ud);
}

static void on_message(void *ud, json_t *msg)
{
	UNUSED_PARAMETER(ud);
	const char *t = json_string_value(json_object_get(msg, "t"));
	if (strcmp(t, "config") == 0) {
		pthread_mutex_lock(&se_g.mu);
		bool changed = config_apply_json(&se_g.config, msg);
		struct se_config c = se_g.config;
		pthread_mutex_unlock(&se_g.mu);
		if (changed) {
			blog(LOG_INFO, "[stream-engine] config: stale_ms=%u fallback_mode=%s fallback_scene='%s'", c.stale_ms,
			     mode_name(c.fallback_mode), c.fallback_scene);
			plugin_config_save(&c);
		}
	} else if (strcmp(t, "cmd") == 0) {
		json_t *id = json_object_get(msg, "id");
		const char *op = json_string_value(json_object_get(msg, "op"));
		if (!json_is_integer(id) || !op) {
			blog(LOG_WARNING, "[stream-engine] cmd without id/op ignored");
			return;
		}
		struct cmd_task *task = bzalloc(sizeof(*task));
		task->id = json_integer_value(id);
		snprintf(task->op, sizeof(task->op), "%s", op);
		blog(LOG_INFO, "[stream-engine] engine command %" PRId64 ": %s", (int64_t)task->id, task->op);
		obs_queue_task(OBS_TASK_UI, task_cmd, task, false);
	}
}

static void on_tick(void *ud, uint64_t now)
{
	UNUSED_PARAMETER(ud);
	if (atomic_load(&se_g.exiting))
		return;
	struct se_config cfg;
	se_config_copy(&cfg);

	if (atomic_load(&loaded))
		se_vertical_tick(cfg.vertical_enabled, now);
	else
		se_vertical_reset();
	track_outputs();
	const bool live = any_output_live();
	const bool allowed = atomic_load(&loaded) &&
			     (cfg.fallback_mode == SE_FALLBACK_ALWAYS || (cfg.fallback_mode == SE_FALLBACK_LIVE && live));
	struct se_monitor_result mon;
	se_sources_monitor(now, cfg.stale_ms, allowed, &mon);

	bool changed = false;
	for (uint32_t k = 0; k < SE_CANVAS_COUNT; k++)
		changed |= mon.fresh[k] != last_mon.fresh[k] || mon.count[k] != last_mon.count[k];

	if (mon.engage && !atomic_exchange(&engage_queued, true))
		obs_queue_task(OBS_TASK_UI, task_engage, NULL, false);

	static uint64_t last_restore_check;
	if (atomic_load(&se_g.fallback_auto) && (mon.recovered || now - last_restore_check >= 1000000000ull)) {
		last_restore_check = now;
		if (!atomic_exchange(&restore_queued, true))
			obs_queue_task(OBS_TASK_UI, task_restore, NULL, false);
	}

	if (changed || atomic_exchange(&force_status, false) || now - prev_status_ns >= 1000000000ull) {
		if (se_control_connected(se_g.control))
			send_status(now, &mon);
		else
			prev_status_ns = now;
	}
	last_mon = mon;
}

/* ---- frontend events (UI thread) ----------------------------------------------------- */

static void capture_output(obs_weak_output_t **slot, obs_output_t *o)
{
	obs_weak_output_t *w = o ? obs_output_get_weak_output(o) : NULL;
	pthread_mutex_lock(&se_g.mu);
	obs_weak_output_release(*slot);
	*slot = w;
	pthread_mutex_unlock(&se_g.mu);
	obs_output_release(o);
}

static void update_scene_name(void)
{
	obs_source_t *scene = obs_frontend_get_current_scene();
	pthread_mutex_lock(&se_g.mu);
	snprintf(se_g.scene_name, sizeof(se_g.scene_name), "%s", scene ? obs_source_get_name(scene) : "");
	pthread_mutex_unlock(&se_g.mu);
	obs_source_release(scene);
}

static void send_event(const char *name)
{
	json_t *m = json_pack("{s:s, s:s}", "t", "event", "name", name);
	se_stamp(m);
	send_json(m);
	atomic_store(&force_status, true);
}

static void on_frontend_event(enum obs_frontend_event event, void *data)
{
	UNUSED_PARAMETER(data);
	switch (event) {
	case OBS_FRONTEND_EVENT_FINISHED_LOADING:
	case OBS_FRONTEND_EVENT_SCENE_COLLECTION_CHANGED:
		update_scene_name();
		atomic_store(&loaded, true);
		break;
	case OBS_FRONTEND_EVENT_SCENE_COLLECTION_CHANGING:
	case OBS_FRONTEND_EVENT_SCENE_COLLECTION_CLEANUP:
		atomic_store(&loaded, false);
		se_fallback_reset();
		break;
	case OBS_FRONTEND_EVENT_SCENE_CHANGED:
		update_scene_name();
		atomic_store(&force_status, true);
		break;
	case OBS_FRONTEND_EVENT_STREAMING_STARTING:
		capture_output(&se_g.stream_output, obs_frontend_get_streaming_output());
		break;
	case OBS_FRONTEND_EVENT_STREAMING_STARTED:
		capture_output(&se_g.stream_output, obs_frontend_get_streaming_output());
		send_event("stream_started");
		break;
	case OBS_FRONTEND_EVENT_STREAMING_STOPPED:
		send_event("stream_stopped");
		break;
	case OBS_FRONTEND_EVENT_EXIT:
		atomic_store(&se_g.exiting, true);
		atomic_store(&loaded, false);
		se_control_stop(se_g.control);
		se_g.control = NULL;
		se_fallback_reset();
		break;
	default:
		break;
	}
}

/* ---- module -------------------------------------------------------------------------- */

/* Aitum's start path asks before allocating an encoder, including automatic
 * starts. Main-canvas outputs are never held. */
static void canvas_enabled(void *data, calldata_t *cd)
{
	UNUSED_PARAMETER(data);
	obs_canvas_t *canvas = calldata_ptr(cd, "canvas");
	struct se_config cfg;
	se_config_copy(&cfg);
	bool enabled = true;
	if (!cfg.vertical_enabled && canvas && !(obs_canvas_get_flags(canvas) & MAIN)) {
		char kind[64];
		se_canvas_kind_for_video(obs_canvas_get_video(canvas), kind, sizeof(kind));
		enabled = strcmp(kind, "tall") != 0;
	}
	calldata_set_bool(cd, "enabled", enabled);
}

bool obs_module_load(void)
{
	pthread_mutex_init(&se_g.mu, NULL);
	plugin_config_load();
	proc_handler_add(obs_get_proc_handler(), "void stream_engine_canvas_enabled(ptr canvas, out bool enabled)",
			 canvas_enabled, NULL);
	se_register_sources();

	char path[108] = "";
	if (!se_runtime_path("obs.sock", path, sizeof(path))) {
		blog(LOG_ERROR, "[stream-engine] obs.sock path too long; control channel disabled");
	} else {
		struct se_control_opts o = {
			.socket_path = path,
			.tick_ms = 100,
			.on_connect = on_connect,
			.on_disconnect = on_disconnect,
			.on_message = on_message,
			.on_tick = on_tick,
			.log = se_log,
		};
		se_g.control = se_control_start(&o);
		if (!se_g.control)
			blog(LOG_ERROR, "[stream-engine] cannot start the control channel");
	}
	obs_frontend_add_event_callback(on_frontend_event, NULL);
	struct se_config c;
	se_config_copy(&c);
	blog(LOG_INFO, "[stream-engine] loaded v%s (control %s; stale %u ms; fallback %s → '%s')", SE_PLUGIN_VERSION, path,
	     c.stale_ms, mode_name(c.fallback_mode), c.fallback_scene);
	return true;
}

void obs_module_unload(void)
{
	atomic_store(&se_g.exiting, true);
	se_control_stop(se_g.control);
	se_g.control = NULL;
	se_vertical_reset();
	for (size_t i = 0; i < MAX_TRACKED; i++) {
		if (tracked[i].used) {
			obs_weak_output_release(tracked[i].weak);
			memset(&tracked[i], 0, sizeof(tracked[i]));
		}
	}
	pthread_mutex_lock(&se_g.mu);
	obs_weak_output_release(se_g.stream_output);
	se_g.stream_output = NULL;
	pthread_mutex_unlock(&se_g.mu);
}
