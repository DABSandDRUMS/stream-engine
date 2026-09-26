/* SPDX-License-Identifier: GPL-2.0-or-later
 *
 * Minimal frames.sock server side (docs/frames-protocol.md) for tests and se-fake-frames.
 */
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>

#include "se-frames-proto.h"

/* Listening SOCK_SEQPACKET socket at `path` (an existing file is replaced). -1 on error. */
int fs_listen(const char *path);
/* Accepts one client; -1 on timeout/error. */
int fs_accept(int lfd, int timeout_ms);
/* Receives one message (fds are closed); -1 timeout/error, 0 peer closed. */
ssize_t fs_recv(int fd, void *buf, size_t len, int timeout_ms);
/* Sends one message with optional SCM_RIGHTS fds. */
bool fs_send(int fd, const void *msg, size_t len, const int *fds, size_t nfds);
/* Anonymous shared memory of `size` bytes. */
int fs_memfd(const char *name, size_t size);

struct se_canvas fs_canvas_msg(uint32_t canvas, uint32_t w, uint32_t h, uint32_t fourcc, uint64_t modifier,
			       uint32_t stride, uint32_t count, uint32_t generation);
struct se_frame fs_frame_msg(uint32_t canvas, uint32_t buffer, uint64_t seq, uint32_t generation, bool fence);
