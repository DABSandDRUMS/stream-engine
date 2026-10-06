/* SPDX-License-Identifier: GPL-2.0-or-later */
#include "plugin.h"

#include <obs-frontend-api.h>
#include <obs-hotkey.h>

/* Owned by the control thread. Keep only the outputs stopped by this switch;
 * enabling an idle canvas must never start a new broadcast. */
#define MAX_VERTICAL_OUTPUTS 16
struct paused_output {
	obs_encoder_t *encoder;
	bool stopped;
	uint64_t attempt_ns;
	char name[256];
};
static struct paused_output paused[MAX_VERTICAL_OUTPUTS];
static bool desired = true;
static bool ready;
static char error[256];

static void clear_output(struct paused_output *p)
{
	obs_encoder_release(p->encoder);
	memset(p, 0, sizeof(*p));
}

void se_vertical_reset(void)
{
	for (size_t i = 0; i < MAX_VERTICAL_OUTPUTS; i++)
		clear_output(&paused[i]);
	ready = false;
	error[0] = 0;
}

struct active_outputs {
	obs_output_t *items[MAX_VERTICAL_OUTPUTS];
	size_t n;
	bool overflow;
};

static bool collect_vertical(void *data, obs_output_t *output)
{
	struct active_outputs *a = data;
	if (!obs_output_active(output))
		return true;
	/* Never touch any output encoding the main OBS video, including recording. */
	video_t *video = obs_output_video(output);
	if (!video || video == obs_get_video())
		return true;
	char canvas[64];
	se_canvas_kind_for_video(video, canvas, sizeof(canvas));
	if (strcmp(canvas, "tall") != 0)
		return true;
	if (a->n == MAX_VERTICAL_OUTPUTS) {
		a->overflow = true;
		return true;
	}
	a->items[a->n++] = obs_output_get_ref(output);
	return true;
}

struct find_hotkey {
	const char *name;
	obs_hotkey_id id;
};

static bool find_start(void *data, obs_hotkey_id id, obs_hotkey_t *key)
{
	struct find_hotkey *find = data;
	if (strcmp(obs_hotkey_get_name(key), find->name) == 0) {
		find->id = id;
		return false;
	}
	return true;
}

static bool resume_output(struct paused_output *p, obs_output_t *output)
{
	/* Aitum's own start handler rebuilds its encoder and output settings. This is
	 * the same routed hotkey API used by its start_output websocket request. */
	static const char prefix[] = "Aitum Stream Suite Output ";
	if (strncmp(p->name, prefix, sizeof(prefix) - 1) == 0) {
		char name[320];
		snprintf(name, sizeof(name), "AitumStreamSuiteStartOutput%s", p->name + sizeof(prefix) - 1);
		struct find_hotkey find = {.name = name, .id = OBS_INVALID_HOTKEY_ID};
		obs_enum_hotkeys(find_start, &find);
		if (find.id == OBS_INVALID_HOTKEY_ID)
			return false;
		obs_hotkey_trigger_routed_callback(find.id, true);
		obs_hotkey_trigger_routed_callback(find.id, false);
		return true;
	}
	return output && obs_output_start(output);
}

void se_vertical_tick(bool enabled, uint64_t now)
{
	desired = enabled;
	ready = true;
	error[0] = 0;
	struct active_outputs active = {0};
	if (!enabled)
		obs_enum_outputs(collect_vertical, &active);
	if (active.overflow) {
		ready = false;
		snprintf(error, sizeof(error), "Too many vertical outputs to safely suspend");
	}
	for (size_t i = 0; i < active.n; i++) {
		obs_output_t *output = active.items[i];
		struct paused_output *p = NULL;
		for (size_t j = 0; j < MAX_VERTICAL_OUTPUTS; j++)
			if (strcmp(paused[j].name, obs_output_get_name(output)) == 0) {
				p = &paused[j];
				break;
			}
		if (!p)
			for (size_t j = 0; j < MAX_VERTICAL_OUTPUTS; j++)
				if (!paused[j].name[0]) {
					p = &paused[j];
					snprintf(p->name, sizeof(p->name), "%s", obs_output_get_name(output));
					break;
				}
		ready = false;
		if (!p) {
			snprintf(error, sizeof(error), "Too many remembered vertical outputs; leave vertical rendering on");
		} else if (!p->attempt_ns || now - p->attempt_ns >= 5000000000ull) {
			if (!p->encoder)
				p->encoder = obs_encoder_get_ref(obs_output_get_video_encoder(output));
			p->stopped = false;
			p->attempt_ns = now;
			obs_output_stop(output);
		}
		obs_output_release(output);
	}
	for (size_t i = 0; i < MAX_VERTICAL_OUTPUTS; i++) {
		struct paused_output *p = &paused[i];
		if (!p->name[0])
			continue;
		obs_output_t *output = obs_get_output_by_name(p->name);
		const bool active_now = output && obs_output_active(output);
		if (!p->stopped && !active_now && (!p->encoder || !obs_encoder_active(p->encoder))) {
			p->stopped = true;
			p->attempt_ns = 0;
			obs_encoder_release(p->encoder);
			p->encoder = NULL;
		}
		if (!enabled) {
			if (!p->stopped) {
				ready = false;
				if (p->attempt_ns && now - p->attempt_ns >= 2000000000ull)
					snprintf(error, sizeof(error), "Waiting for %.160s encoder to stop", p->name);
			}
			obs_output_release(output);
			continue;
		}
		/* Turning ON while the main broadcast is stopped only arms vertical. */
		if (!obs_frontend_streaming_active()) {
			obs_output_release(output);
			clear_output(p);
			continue;
		}
		if (p->stopped && active_now) {
			obs_output_release(output);
			clear_output(p);
			continue;
		}
		ready = false;
		if (!p->stopped) {
			obs_output_release(output);
			continue;
		}
		if (!p->attempt_ns || now - p->attempt_ns >= 5000000000ull) {
			p->attempt_ns = now;
			if (!resume_output(p, output))
				snprintf(error, sizeof(error), "Cannot resume %.160s; check its OBS output settings", p->name);
		} else if (now - p->attempt_ns >= 2000000000ull) {
			snprintf(error, sizeof(error), "Waiting for %.160s to restart; check its OBS output settings", p->name);
		}
		obs_output_release(output);
	}
}

json_t *se_vertical_status(void)
{
	return json_pack("{s:b, s:b, s:s}", "enabled", desired, "ready", ready, "error", error);
}
