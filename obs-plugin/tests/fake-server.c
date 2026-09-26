/* SPDX-License-Identifier: GPL-2.0-or-later */
#define _GNU_SOURCE
#include "fake-server.h"

#include <errno.h>
#include <poll.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

int fs_listen(const char *path)
{
	int fd = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
	if (fd < 0)
		return -1;
	struct sockaddr_un addr = {.sun_family = AF_UNIX};
	if (strlen(path) >= sizeof(addr.sun_path)) {
		close(fd);
		return -1;
	}
	strcpy(addr.sun_path, path);
	unlink(path);
	if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) < 0 || listen(fd, 8) < 0) {
		close(fd);
		return -1;
	}
	return fd;
}

int fs_accept(int lfd, int timeout_ms)
{
	struct pollfd p = {.fd = lfd, .events = POLLIN};
	if (poll(&p, 1, timeout_ms) <= 0)
		return -1;
	return accept4(lfd, NULL, NULL, SOCK_CLOEXEC);
}

ssize_t fs_recv(int fd, void *buf, size_t len, int timeout_ms)
{
	struct pollfd p = {.fd = fd, .events = POLLIN};
	if (poll(&p, 1, timeout_ms) <= 0)
		return -1;
	union {
		char b[CMSG_SPACE(sizeof(int) * 16)];
		struct cmsghdr align;
	} ctl;
	struct iovec iov = {.iov_base = buf, .iov_len = len};
	struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = ctl.b, .msg_controllen = sizeof(ctl.b)};
	ssize_t n = recvmsg(fd, &msg, MSG_CMSG_CLOEXEC);
	for (struct cmsghdr *cm = CMSG_FIRSTHDR(&msg); n >= 0 && cm; cm = CMSG_NXTHDR(&msg, cm)) {
		if (cm->cmsg_level == SOL_SOCKET && cm->cmsg_type == SCM_RIGHTS) {
			size_t count = (cm->cmsg_len - CMSG_LEN(0)) / sizeof(int);
			for (size_t i = 0; i < count; i++)
				close(((int *)CMSG_DATA(cm))[i]);
		}
	}
	return n;
}

bool fs_send(int fd, const void *data, size_t len, const int *fds, size_t nfds)
{
	union {
		char b[CMSG_SPACE(sizeof(int) * 16)];
		struct cmsghdr align;
	} ctl;
	struct iovec iov = {.iov_base = (void *)data, .iov_len = len};
	struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1};
	if (nfds) {
		if (nfds > 16)
			return false;
		memset(ctl.b, 0, sizeof(ctl.b));
		msg.msg_control = ctl.b;
		msg.msg_controllen = CMSG_SPACE(sizeof(int) * nfds);
		struct cmsghdr *cm = CMSG_FIRSTHDR(&msg);
		cm->cmsg_level = SOL_SOCKET;
		cm->cmsg_type = SCM_RIGHTS;
		cm->cmsg_len = CMSG_LEN(sizeof(int) * nfds);
		memcpy(CMSG_DATA(cm), fds, sizeof(int) * nfds);
	}
	return sendmsg(fd, &msg, MSG_NOSIGNAL) == (ssize_t)len;
}

int fs_memfd(const char *name, size_t size)
{
	int fd = memfd_create(name, MFD_CLOEXEC);
	if (fd < 0)
		return -1;
	if (ftruncate(fd, (off_t)size) < 0) {
		close(fd);
		return -1;
	}
	return fd;
}

struct se_canvas fs_canvas_msg(uint32_t canvas, uint32_t w, uint32_t h, uint32_t fourcc, uint64_t modifier,
			       uint32_t stride, uint32_t count, uint32_t generation)
{
	struct se_canvas m = {
		.h = se_hdr_make(SE_MSG_CANVAS),
		.canvas = canvas,
		.width = w,
		.height = h,
		.drm_fourcc = fourcc,
		.modifier = modifier,
		.planes = 1,
		.buffer_count = count,
		.generation = generation,
	};
	m.strides[0] = stride;
	return m;
}

struct se_frame fs_frame_msg(uint32_t canvas, uint32_t buffer, uint64_t seq, uint32_t generation, bool fence)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	struct se_frame m = {
		.h = se_hdr_make(SE_MSG_FRAME),
		.canvas = canvas,
		.buffer = buffer,
		.seq = seq,
		.monotonic_ns = (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec,
		.generation = generation,
		.has_fence = fence ? 1 : 0,
	};
	return m;
}
