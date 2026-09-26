/* SPDX-License-Identifier: GPL-2.0-or-later
 *
 * obs.sock client (docs/frames-protocol.md): JSON lines over a Unix stream socket, independent
 * of libobs. One background thread connects with backoff, frames/parses lines, sends queued
 * lines, and calls `on_tick` every `tick_ms` whether or not the engine is connected (the
 * plugin's stale monitor runs on it, so the fallback works while the engine is down).
 */
#pragma once

#include <jansson.h>
#include <stdbool.h>
#include <stdint.h>

#include "frames-client.h"

#ifdef __cplusplus
extern "C" {
#endif

struct se_control_opts {
	const char *socket_path;
	uint32_t tick_ms; /* 0 = 100 */
	/* All callbacks run on the control thread. */
	void (*on_connect)(void *ud);
	void (*on_disconnect)(void *ud);
	void (*on_message)(void *ud, json_t *msg); /* borrowed; object with a string "t" */
	void (*on_tick)(void *ud, uint64_t now_ns);
	se_log_fn log;
	void *ud;
};

struct se_control;

struct se_control *se_control_start(const struct se_control_opts *opts);
/* Stops and joins the thread (callbacks are not called after it returns). */
void se_control_stop(struct se_control *c);
/* Serializes `msg` (reference borrowed) as one line and queues it. Thread-safe. Returns false
 * when not connected (the message is dropped: state is re-sent on every connect). */
bool se_control_send(struct se_control *c, json_t *msg);
bool se_control_connected(struct se_control *c);

#ifdef __cplusplus
}
#endif
