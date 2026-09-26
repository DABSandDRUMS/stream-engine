/* SPDX-License-Identifier: GPL-2.0-or-later
 *
 * Fallback scene handling (UI thread). When a visible stream-engine feed goes stale, every
 * OBS canvas whose program shows it switches to its fallback scene (default "Technical
 * Difficulties", created in that canvas only if missing) and switches back once the feed of
 * the saved scene is fresh again — unless the operator changed scenes in the meantime. The
 * main canvas switches through the frontend API (studio mode aware); other canvases (e.g.
 * Aitum's "Vertical") by swapping their output channel, which is restored exactly.
 */
#include "plugin.h"

#include <obs-frontend-api.h>
#include <util/dstr.h>
#include <util/platform.h>

#define MAX_DEPTH 10

struct fb_entry {
	obs_weak_canvas_t *canvas;
	obs_weak_source_t *saved;    /* main: program scene; other: raw channel-0 source */
	obs_weak_source_t *fallback; /* scene we switched to */
	bool main;
	bool manual;
	char canvas_name[128];
};

static struct fb_entry entries[SE_MAX_FALLBACK_CANVASES];
static size_t n_entries;

/* ---- helpers ------------------------------------------------------------------------- */

struct canvas_list {
	obs_canvas_t *items[SE_MAX_FALLBACK_CANVASES];
	size_t n;
};

static bool collect_canvas(void *param, obs_canvas_t *canvas)
{
	struct canvas_list *l = param;
	uint32_t flags = obs_canvas_get_flags(canvas);
	if ((flags & EPHEMERAL) || obs_canvas_removed(canvas) || !obs_canvas_has_video(canvas))
		return true;
	if (l->n < SE_MAX_FALLBACK_CANVASES)
		l->items[l->n++] = obs_canvas_get_ref(canvas);
	return true;
}

/* Public canvases with video, main first. Release with release_canvases(). */
static void list_canvases(struct canvas_list *l)
{
	l->n = 0;
	obs_enum_canvases(collect_canvas, l);
	for (size_t i = 1; i < l->n; i++) {
		if (obs_canvas_get_flags(l->items[i]) & MAIN) {
			obs_canvas_t *t = l->items[0];
			l->items[0] = l->items[i];
			l->items[i] = t;
		}
	}
}

static void release_canvases(struct canvas_list *l)
{
	for (size_t i = 0; i < l->n; i++)
		obs_canvas_release(l->items[i]);
	l->n = 0;
}

static bool is_main(obs_canvas_t *canvas)
{
	return (obs_canvas_get_flags(canvas) & MAIN) != 0;
}

/* Raw program source of a canvas (ref'd): main → the frontend's program scene; others →
 * output channel 0 (often a transition). */
static obs_source_t *program_raw(obs_canvas_t *canvas)
{
	if (is_main(canvas))
		return obs_frontend_get_current_scene();
	return obs_canvas_get_channel(canvas, 0);
}

/* Scene shown by a raw program source (ref'd). */
static obs_source_t *resolve_scene(obs_source_t *raw)
{
	if (!raw)
		return NULL;
	if (obs_source_get_type(raw) == OBS_SOURCE_TYPE_TRANSITION)
		return obs_transition_get_active_source(raw);
	return obs_source_get_ref(raw);
}

struct find_ctx {
	bool stale_only;
	bool found;
	uint32_t kinds; /* bitmask of engine canvases found */
	int depth;
};

static bool find_item(obs_scene_t *scene, obs_sceneitem_t *item, void *param);

static void find_in_scene(obs_scene_t *scene, struct find_ctx *f)
{
	if (!scene || f->depth >= MAX_DEPTH)
		return;
	f->depth++;
	obs_scene_enum_items(scene, find_item, f);
	f->depth--;
}

static bool find_item(obs_scene_t *scene, obs_sceneitem_t *item, void *param)
{
	UNUSED_PARAMETER(scene);
	struct find_ctx *f = param;
	if (!obs_sceneitem_visible(item))
		return true;
	obs_source_t *src = obs_sceneitem_get_source(item);
	uint32_t canvas;
	bool stale;
	if (se_source_lookup(src, &canvas, &stale)) {
		f->kinds |= 1u << canvas;
		if (!f->stale_only || stale)
			f->found = true;
	} else if (obs_sceneitem_is_group(item) && f->depth < MAX_DEPTH) {
		f->depth++;
		obs_sceneitem_group_enum_items(item, find_item, f);
		f->depth--;
	} else if (obs_scene_from_source(src)) {
		find_in_scene(obs_scene_from_source(src), f);
	}
	return true;
}

static bool scene_shows_feed(obs_source_t *scene_src, bool stale_only, uint32_t *kinds)
{
	struct find_ctx f = {.stale_only = stale_only};
	find_in_scene(obs_scene_from_source(scene_src), &f);
	if (kinds)
		*kinds = f.kinds;
	return f.found;
}

static json_t *kinds_json(uint32_t kinds)
{
	json_t *a = json_array();
	for (uint32_t i = 0; i < SE_CANVAS_COUNT; i++)
		if (kinds & (1u << i))
			json_array_append_new(a, json_string(se_canvas_name(i)));
	return a;
}

static void canvas_size(obs_canvas_t *canvas, uint32_t *w, uint32_t *h)
{
	struct obs_video_info ovi;
	if (obs_canvas_get_video_info(canvas, &ovi)) {
		*w = ovi.base_width;
		*h = ovi.base_height;
	} else {
		*w = 1920;
		*h = 1080;
	}
}

/* Returns an existing input by name (ref'd) or creates it. */
static obs_source_t *get_or_create_input(const char *id, const char *name, obs_data_t *settings)
{
	obs_source_t *src = obs_get_source_by_name(name);
	if (src)
		return src;
	return obs_source_create(id, name, settings, NULL);
}

static void add_fitted(obs_scene_t *scene, obs_source_t *src, uint32_t w, uint32_t h, float frac_w, float frac_h)
{
	obs_sceneitem_t *item = obs_scene_add(scene, src);
	if (!item)
		return;
	struct vec2 pos, bounds;
	vec2_set(&pos, (float)w * 0.5f, (float)h * 0.5f);
	vec2_set(&bounds, (float)w * frac_w, (float)h * frac_h);
	obs_sceneitem_set_alignment(item, OBS_ALIGN_CENTER);
	obs_sceneitem_set_bounds_type(item, OBS_BOUNDS_SCALE_INNER);
	obs_sceneitem_set_bounds_alignment(item, OBS_ALIGN_CENTER);
	obs_sceneitem_set_bounds(item, &bounds);
	obs_sceneitem_set_pos(item, &pos);
}

/* Fallback scene of a canvas (ref'd scene source); created with a background and a text
 * source only if no scene of that name exists in the canvas. */
static obs_source_t *fallback_scene(obs_canvas_t *canvas, const struct se_config *cfg, bool *created)
{
	*created = false;
	obs_scene_t *scene = obs_canvas_get_scene_by_name(canvas, cfg->fallback_scene);
	if (scene)
		return obs_scene_get_source(scene); /* the scene ref is the source ref */

	const bool main = is_main(canvas);
	scene = main ? obs_scene_create(cfg->fallback_scene) : obs_canvas_scene_create(canvas, cfg->fallback_scene);
	if (!scene)
		return NULL;
	*created = true;
	uint32_t w, h;
	canvas_size(canvas, &w, &h);

	struct dstr bg_name = {0}, text_name = {0};
	if (main) {
		dstr_printf(&bg_name, "%s \xc2\xb7 background", cfg->fallback_scene);
		dstr_printf(&text_name, "%s \xc2\xb7 text", cfg->fallback_scene);
	} else {
		dstr_printf(&bg_name, "%s \xc2\xb7 background (%s)", cfg->fallback_scene, obs_canvas_get_name(canvas));
		dstr_printf(&text_name, "%s \xc2\xb7 text (%s)", cfg->fallback_scene, obs_canvas_get_name(canvas));
	}

	obs_data_t *bg_settings = obs_data_create();
	obs_data_set_int(bg_settings, "color", 0xff141414); /* ABGR */
	obs_data_set_int(bg_settings, "width", w);
	obs_data_set_int(bg_settings, "height", h);
	obs_source_t *bg = get_or_create_input("color_source_v3", bg_name.array, bg_settings);
	obs_data_release(bg_settings);
	if (bg) {
		add_fitted(scene, bg, w, h, 1.0f, 1.0f);
		obs_source_release(bg);
	}

	obs_data_t *text_settings = obs_data_create();
	obs_data_t *font = obs_data_create();
	obs_data_set_string(font, "face", "Sans Serif");
	obs_data_set_string(font, "style", "Bold");
	obs_data_set_int(font, "size", 96);
	obs_data_set_int(font, "flags", 1); /* bold */
	obs_data_set_obj(text_settings, "font", font);
	obs_data_release(font);
	obs_data_set_string(text_settings, "text", cfg->fallback_text);
	obs_data_set_int(text_settings, "color1", 0xffffffff);
	obs_data_set_int(text_settings, "color2", 0xffffffff);
	obs_data_set_bool(text_settings, "word_wrap", false);
	obs_source_t *text = get_or_create_input("text_ft2_source_v2", text_name.array, text_settings);
	obs_data_release(text_settings);
	if (text) {
		add_fitted(scene, text, w, h, 0.8f, 0.25f);
		obs_source_release(text);
	}
	dstr_free(&bg_name);
	dstr_free(&text_name);
	blog(LOG_INFO, "[stream-engine] created fallback scene '%s' in canvas '%s'", cfg->fallback_scene,
	     obs_canvas_get_name(canvas));
	/* keep one reference for the caller; the canvas holds its own (SCENE_REF) */
	return obs_scene_get_source(scene);
}

static void switch_program(obs_canvas_t *canvas, obs_source_t *target)
{
	if (is_main(canvas))
		obs_frontend_set_current_scene(target); /* transitions program, also in studio mode */
	else
		obs_canvas_set_channel(canvas, 0, target);
}

static struct fb_entry *entry_for(obs_canvas_t *canvas)
{
	for (size_t i = 0; i < n_entries; i++) {
		obs_canvas_t *c = obs_weak_canvas_get_canvas(entries[i].canvas);
		obs_canvas_release(c);
		if (c == canvas)
			return &entries[i];
	}
	return NULL;
}

static void remove_entry(size_t i)
{
	obs_weak_canvas_release(entries[i].canvas);
	obs_weak_source_release(entries[i].saved);
	obs_weak_source_release(entries[i].fallback);
	entries[i] = entries[--n_entries];
}

static void update_flags(void)
{
	bool any_auto = false;
	for (size_t i = 0; i < n_entries; i++)
		any_auto |= !entries[i].manual;
	atomic_store(&se_g.fallback_auto, any_auto);
	atomic_store(&se_g.fallback_engaged, n_entries > 0);
}

static void send_event(const char *name, const char *canvas, const char *scene, const char *other, uint32_t kinds,
		       const char *reason)
{
	json_t *m = json_pack("{s:s, s:s, s:s, s:s, s:s, s:o, s:s}", "t", "event", "name", name, "canvas", canvas, "scene",
			      scene ? scene : "", strcmp(name, "scene_fallback") == 0 ? "from" : "to", other ? other : "",
			      "canvases", kinds_json(kinds), "reason", reason);
	if (m) {
		se_stamp(m);
		se_control_send(se_g.control, m);
		json_decref(m);
	}
}

/* ---- public -------------------------------------------------------------------------- */

int se_fallback_engage(enum se_fallback_reason reason, char *err, size_t errlen)
{
	struct se_config cfg;
	se_config_copy(&cfg);
	struct canvas_list l;
	list_canvases(&l);
	int switched = 0;
	bool considered = false;
	if (err && errlen)
		err[0] = 0;

	/* a manual fallback covers the main canvas when no canvas program shows a feed at all */
	bool any_shows = false;
	if (reason == SE_REASON_MANUAL) {
		for (size_t i = 0; i < l.n && !any_shows; i++) {
			obs_source_t *raw = program_raw(l.items[i]);
			obs_source_t *scene = resolve_scene(raw);
			any_shows = scene && scene_shows_feed(scene, false, NULL);
			obs_source_release(scene);
			obs_source_release(raw);
		}
	}

	for (size_t i = 0; i < l.n; i++) {
		obs_canvas_t *canvas = l.items[i];
		obs_source_t *raw = program_raw(canvas);
		obs_source_t *scene = resolve_scene(raw);
		uint32_t kinds = 0;
		bool shows = scene && scene_shows_feed(scene, reason == SE_REASON_STALE, &kinds);
		if (reason == SE_REASON_MANUAL && !any_shows && is_main(canvas))
			shows = scene != NULL;
		const char *scene_name = scene ? obs_source_get_name(scene) : "";
		if (!shows || strcmp(scene_name, cfg.fallback_scene) == 0 || entry_for(canvas)) {
			obs_source_release(scene);
			obs_source_release(raw);
			continue;
		}
		considered = true;
		bool created;
		obs_source_t *fb = fallback_scene(canvas, &cfg, &created);
		if (!fb) {
			if (err && errlen)
				snprintf(err, errlen, "cannot create scene '%s' in canvas '%s'", cfg.fallback_scene,
					 obs_canvas_get_name(canvas));
			obs_source_release(scene);
			obs_source_release(raw);
			continue;
		}
		if (n_entries < SE_MAX_FALLBACK_CANVASES) {
			struct fb_entry *e = &entries[n_entries++];
			memset(e, 0, sizeof(*e));
			e->canvas = obs_canvas_get_weak_canvas(canvas);
			e->saved = obs_source_get_weak_source(is_main(canvas) ? scene : raw);
			e->fallback = obs_source_get_weak_source(fb);
			e->main = is_main(canvas);
			e->manual = reason == SE_REASON_MANUAL;
			snprintf(e->canvas_name, sizeof(e->canvas_name), "%s", obs_canvas_get_name(canvas));
		}
		switch_program(canvas, fb);
		switched++;
		blog(LOG_WARNING, "[stream-engine] %s: canvas '%s' switched from '%s' to fallback '%s'",
		     reason == SE_REASON_STALE ? "feed stale" : "manual fallback", obs_canvas_get_name(canvas), scene_name,
		     cfg.fallback_scene);
		send_event("scene_fallback", obs_canvas_get_name(canvas), cfg.fallback_scene, scene_name, kinds,
			   reason == SE_REASON_STALE ? "stale" : "manual");
		obs_source_release(fb);
		obs_source_release(scene);
		obs_source_release(raw);
	}
	release_canvases(&l);
	update_flags();
	if (!considered && err && errlen && !err[0])
		snprintf(err, errlen, "no canvas program shows a %sstream-engine source",
			 reason == SE_REASON_STALE ? "stale " : "");
	return switched;
}

int se_fallback_restore(bool force)
{
	int restored = 0;
	for (size_t i = 0; i < n_entries;) {
		struct fb_entry *e = &entries[i];
		if (e->manual && !force) {
			i++;
			continue;
		}
		obs_canvas_t *canvas = obs_weak_canvas_get_canvas(e->canvas);
		obs_source_t *saved = obs_weak_source_get_source(e->saved);
		obs_source_t *fb = obs_weak_source_get_source(e->fallback);
		if (!canvas || obs_canvas_removed(canvas)) {
			obs_source_release(saved);
			obs_source_release(fb);
			obs_canvas_release(canvas);
			remove_entry(i);
			continue;
		}
		obs_source_t *saved_scene = resolve_scene(saved);
		uint32_t kinds = 0;
		if (!force && saved_scene && scene_shows_feed(saved_scene, true, &kinds)) {
			/* the scene we would return to still shows a stale feed */
			obs_source_release(saved_scene);
			obs_source_release(saved);
			obs_source_release(fb);
			obs_canvas_release(canvas);
			i++;
			continue;
		}
		obs_source_t *raw = program_raw(canvas);
		obs_source_t *current = resolve_scene(raw);
		const bool still_fallback = current && current == fb;
		if (still_fallback && saved) {
			switch_program(canvas, saved);
			restored++;
			const char *to = saved_scene ? obs_source_get_name(saved_scene) : obs_source_get_name(saved);
			blog(LOG_INFO, "[stream-engine] canvas '%s' restored to '%s'", e->canvas_name, to);
			if (!kinds && saved_scene)
				scene_shows_feed(saved_scene, false, &kinds);
			send_event("scene_restored", e->canvas_name, fb ? obs_source_get_name(fb) : "", to, kinds,
				   force ? "manual" : "fresh");
		} else {
			blog(LOG_INFO, "[stream-engine] canvas '%s' left the fallback manually; not switching back",
			     e->canvas_name);
			send_event("scene_restored", e->canvas_name, fb ? obs_source_get_name(fb) : "",
				   current ? obs_source_get_name(current) : "", 0, "operator");
		}
		obs_source_release(current);
		obs_source_release(raw);
		obs_source_release(saved_scene);
		obs_source_release(saved);
		obs_source_release(fb);
		obs_canvas_release(canvas);
		remove_entry(i);
	}
	update_flags();
	return restored;
}

int se_fallback_prepare(char *err, size_t errlen)
{
	struct se_config cfg;
	se_config_copy(&cfg);
	struct canvas_list l;
	list_canvases(&l);
	int created_n = 0;
	if (err && errlen)
		err[0] = 0;
	for (size_t i = 0; i < l.n; i++) {
		bool created;
		obs_source_t *fb = fallback_scene(l.items[i], &cfg, &created);
		if (!fb && err && errlen)
			snprintf(err, errlen, "cannot create scene '%s' in canvas '%s'", cfg.fallback_scene,
				 obs_canvas_get_name(l.items[i]));
		created_n += created;
		obs_source_release(fb);
	}
	release_canvases(&l);
	return created_n;
}

void se_fallback_reset(void)
{
	while (n_entries)
		remove_entry(n_entries - 1);
	update_flags();
}

/* ---- setup helper -------------------------------------------------------------------- */

static json_t *add_feed(obs_canvas_t *canvas, uint32_t kind)
{
	const char *id = kind == SE_CANVAS_TALL ? "stream_engine_tall" : "stream_engine_wide";
	const char *name = kind == SE_CANVAS_TALL ? "stream-engine: tall" : "stream-engine: wide";
	json_t *r = json_pack("{s:s, s:s}", "canvas", obs_canvas_get_name(canvas), "source", name);
	obs_source_t *raw = program_raw(canvas);
	obs_source_t *scene_src = resolve_scene(raw);
	obs_source_release(raw);
	obs_scene_t *scene = obs_scene_from_source(scene_src);
	if (!scene) {
		json_object_set_new(r, "result", json_string("no program scene"));
		obs_source_release(scene_src);
		return r;
	}
	json_object_set_new(r, "scene", json_string(obs_source_get_name(scene_src)));
	uint32_t kinds = 0;
	scene_shows_feed(scene_src, false, &kinds);
	if (kinds & (1u << kind)) {
		json_object_set_new(r, "result", json_string("already present"));
		obs_source_release(scene_src);
		return r;
	}
	obs_source_t *src = get_or_create_input(id, name, NULL);
	if (!src) {
		json_object_set_new(r, "result", json_string("cannot create source"));
		obs_source_release(scene_src);
		return r;
	}
	uint32_t w, h;
	canvas_size(canvas, &w, &h);
	obs_sceneitem_t *item = obs_scene_add(scene, src);
	if (item) {
		struct vec2 pos, bounds;
		vec2_set(&pos, 0.0f, 0.0f);
		vec2_set(&bounds, (float)w, (float)h);
		obs_sceneitem_set_alignment(item, OBS_ALIGN_LEFT | OBS_ALIGN_TOP);
		obs_sceneitem_set_bounds_type(item, OBS_BOUNDS_SCALE_INNER);
		obs_sceneitem_set_bounds_alignment(item, OBS_ALIGN_CENTER);
		obs_sceneitem_set_bounds(item, &bounds);
		obs_sceneitem_set_pos(item, &pos);
		json_object_set_new(r, "result", json_string("added"));
		blog(LOG_INFO, "[stream-engine] setup: added '%s' to scene '%s' (canvas '%s')", name,
		     obs_source_get_name(scene_src), obs_canvas_get_name(canvas));
	} else {
		json_object_set_new(r, "result", json_string("cannot add to scene"));
	}
	obs_source_release(src);
	obs_source_release(scene_src);
	return r;
}

/* The engine's PipeWire audio nodes (PLAN §5): the program mix feeds the stream (track 1);
 * each stem gets its own recording track so clips can leave the music out. */
static const struct {
	const char *node;
	uint32_t mixers;
} engine_audio[] = {
	{"se-program", 1u << 0}, {"se-band", 1u << 1}, {"se-music", 1u << 2},
	{"se-sfx", 1u << 3},     {"se-tts", 1u << 4},  {"se-game", 1u << 5},
};

static bool add_to_scene(obs_source_t *scene_src, obs_source_t *src)
{
	obs_scene_t *scene = obs_scene_from_source(scene_src);
	if (!scene || obs_scene_find_source(scene, obs_source_get_name(src)))
		return false;
	return obs_scene_add(scene, src) != NULL;
}

/* Audio capture sources for every engine node, in the main canvas's program scene and its
 * fallback scene (so sound keeps going during technical difficulties). Existing sources with
 * the node's name are reused untouched: their tracks stay as the owner set them. */
static json_t *add_audio(obs_canvas_t *canvas)
{
	json_t *out = json_array();
	if (!obs_get_source_output_flags("pulse_input_capture")) {
		json_array_append_new(out, json_pack("{s:s, s:s}", "source", "audio",
						     "result", "PulseAudio capture is not available in this OBS"));
		return out;
	}
	struct se_config cfg;
	se_config_copy(&cfg);
	obs_source_t *raw = program_raw(canvas);
	obs_source_t *program = resolve_scene(raw);
	obs_source_release(raw);
	bool created_fb = false;
	obs_source_t *fb = fallback_scene(canvas, &cfg, &created_fb);
	for (size_t i = 0; i < sizeof(engine_audio) / sizeof(engine_audio[0]); i++) {
		const char *node = engine_audio[i].node;
		json_t *r = json_pack("{s:s}", "source", node);
		obs_source_t *src = obs_get_source_by_name(node);
		bool fresh = false;
		if (!src) {
			obs_data_t *st = obs_data_create();
			obs_data_set_string(st, "device_id", node);
			src = obs_source_create("pulse_input_capture", node, st, NULL);
			obs_data_release(st);
			fresh = src != NULL;
			if (src)
				obs_source_set_audio_mixers(src, engine_audio[i].mixers);
		}
		if (!src) {
			json_object_set_new(r, "result", json_string("cannot create source"));
			json_array_append_new(out, r);
			continue;
		}
		bool added = false;
		if (program && program != fb)
			added |= add_to_scene(program, src);
		if (fb)
			added |= add_to_scene(fb, src);
		json_object_set_new(r, "result", json_string(added ? "added" : "already present"));
		json_object_set_new(r, "tracks", json_integer((json_int_t)obs_source_get_audio_mixers(src)));
		if (added)
			blog(LOG_INFO, "[stream-engine] setup: %s audio '%s' (tracks 0x%x)", fresh ? "added" : "placed", node,
			     obs_source_get_audio_mixers(src));
		obs_source_release(src);
		json_array_append_new(out, r);
	}
	obs_source_release(fb);
	obs_source_release(program);
	return out;
}

json_t *se_setup_sources(void)
{
	struct canvas_list l;
	list_canvases(&l);
	json_t *out = json_array();
	obs_canvas_t *vertical = NULL;
	for (size_t i = 0; i < l.n; i++) {
		if (is_main(l.items[i])) {
			json_array_append_new(out, add_feed(l.items[i], SE_CANVAS_WIDE));
			json_t *audio = add_audio(l.items[i]);
			json_array_extend(out, audio);
			json_decref(audio);
		} else if (!vertical || strcmp(obs_canvas_get_name(l.items[i]), "Vertical") == 0) {
			vertical = l.items[i];
		}
	}
	if (vertical)
		json_array_append_new(out, add_feed(vertical, SE_CANVAS_TALL));
	release_canvases(&l);
	return out;
}

/* ---- output → canvas attribution (any thread) ---------------------------------------- */

struct kind_ctx {
	uint32_t kinds;
};

static bool kinds_of_scene(void *param, obs_source_t *scene_src)
{
	struct kind_ctx *k = param;
	uint32_t kinds = 0;
	scene_shows_feed(scene_src, false, &kinds);
	k->kinds |= kinds;
	return true;
}

void se_canvas_kind_for_video(video_t *video, char *out, size_t len)
{
	snprintf(out, len, "%s", "wide");
	if (!video || video == obs_get_video())
		return;
	struct canvas_list l;
	list_canvases(&l);
	for (size_t i = 0; i < l.n; i++) {
		if (obs_canvas_get_video(l.items[i]) != video)
			continue;
		if (is_main(l.items[i]))
			break;
		struct kind_ctx k = {0};
		obs_canvas_enum_scenes(l.items[i], kinds_of_scene, &k);
		if (k.kinds & (1u << SE_CANVAS_TALL))
			snprintf(out, len, "%s", "tall");
		else if (k.kinds & (1u << SE_CANVAS_WIDE))
			snprintf(out, len, "%s", "wide");
		else
			snprintf(out, len, "%s", obs_canvas_get_name(l.items[i]));
		break;
	}
	release_canvases(&l);
}
