/* SPDX-License-Identifier: GPL-2.0-or-later */
#define _GNU_SOURCE
#include "frames-client.h"

#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

#define RELEASE_QUEUE_CAP 64
#define BACKOFF_MIN_MS 100
#define BACKOFF_MAX_MS 2000
#define HOUSEKEEPING_MS 250
#define MAX_DIM 16384

struct release_item {
	uint64_t epoch;
	uint32_t buffer;
	uint64_t seq;
};

struct pending_frame {
	bool valid;
	uint64_t epoch;
	uint32_t buffer;
	uint64_t seq;
	uint64_t mono;
	int fence;
	uint64_t deadline;
};

struct se_frames_client {
	/* immutable after start */
	char path[sizeof(((struct sockaddr_un *)0)->sun_path)];
	uint32_t canvas;
	uint32_t client_kind;
	uint32_t fence_timeout_ms;
	se_log_fn log;
	void *log_ud;
	int wake_fd;
	pthread_t thread;

	/* shared, guarded by mu */
	pthread_mutex_t mu;
	bool stop;
	bool want_dmabuf;
	struct se_frames_import *pending_import;
	bool ready_valid;
	struct se_frames_ready ready;
	struct release_item releases[RELEASE_QUEUE_CAP];
	size_t n_releases;
	struct se_frames_stats stats;

	/* IO thread only */
	int sock;
	bool hello_dmabuf;
	uint64_t epoch;
	bool have_canvas;
	uint32_t generation;
	uint32_t buffer_count;
	struct pending_frame pending;
	struct release_item tx[RELEASE_QUEUE_CAP];
	size_t n_tx;
	bool logged_connect_error;
};

uint64_t se_mono_ns(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

bool se_runtime_path(const char *name, char *out, size_t len)
{
	const char *dir = getenv("SE_RUNTIME_DIR");
	int n;
	if (dir && *dir) {
		n = snprintf(out, len, "%s/%s", dir, name);
	} else {
		const char *xdg = getenv("XDG_RUNTIME_DIR");
		if (xdg && *xdg)
			n = snprintf(out, len, "%s/stream-engine/%s", xdg, name);
		else
			n = snprintf(out, len, "/run/user/%u/stream-engine/%s", (unsigned)getuid(), name);
	}
	return n > 0 && (size_t)n < len;
}

static void logf_(struct se_frames_client *c, int level, const char *fmt, ...) __attribute__((format(printf, 3, 4)));
static void logf_(struct se_frames_client *c, int level, const char *fmt, ...)
{
	if (!c->log)
		return;
	char buf[512];
	va_list ap;
	va_start(ap, fmt);
	vsnprintf(buf, sizeof(buf), fmt, ap);
	va_end(ap);
	c->log(c->log_ud, level, buf);
}

void se_frames_import_free(struct se_frames_import *imp)
{
	if (!imp)
		return;
	for (uint32_t i = 0; i < SE_MAX_BUFFERS; i++) {
		if (imp->maps[i])
			munmap((void *)imp->maps[i], imp->map_size);
		if (imp->fds[i] >= 0)
			close(imp->fds[i]);
	}
	free(imp);
}

static void wake(struct se_frames_client *c)
{
	uint64_t one = 1;
	ssize_t r = write(c->wake_fd, &one, sizeof(one));
	(void)r;
}

static void drain_wake(struct se_frames_client *c)
{
	uint64_t v;
	ssize_t r = read(c->wake_fd, &v, sizeof(v));
	(void)r;
}

static void close_fds(int *fds, size_t n)
{
	for (size_t i = 0; i < n; i++)
		if (fds[i] >= 0)
			close(fds[i]);
}

/* ---- transmit ------------------------------------------------------------------------ */

static void queue_tx(struct se_frames_client *c, uint64_t epoch, uint32_t buffer, uint64_t seq)
{
	if (epoch != c->epoch || !c->have_canvas)
		return; /* buffer of an older canvas: the engine already forgot it */
	if (c->n_tx == RELEASE_QUEUE_CAP) {
		/* Cannot happen with <= SE_MAX_BUFFERS buffers unless the engine stopped reading;
		 * keep the newest releases. */
		memmove(c->tx, c->tx + 1, sizeof(c->tx[0]) * (RELEASE_QUEUE_CAP - 1));
		c->n_tx--;
	}
	c->tx[c->n_tx++] = (struct release_item){epoch, buffer, seq};
}

/* Returns false on a fatal socket error. */
static bool flush_tx(struct se_frames_client *c)
{
	size_t sent = 0;
	while (sent < c->n_tx) {
		struct se_release r = {
			.h = se_hdr_make(SE_MSG_RELEASE),
			.canvas = c->canvas,
			.buffer = c->tx[sent].buffer,
			.seq = c->tx[sent].seq,
		};
		ssize_t n = send(c->sock, &r, sizeof(r), MSG_DONTWAIT | MSG_NOSIGNAL);
		if (n < 0) {
			if (errno == EINTR)
				continue;
			if (errno == EAGAIN || errno == EWOULDBLOCK)
				break;
			logf_(c, SE_LOG_WARNING, "frames[%s]: send release: %s", se_canvas_name(c->canvas), strerror(errno));
			return false;
		}
		sent++;
	}
	memmove(c->tx, c->tx + sent, sizeof(c->tx[0]) * (c->n_tx - sent));
	c->n_tx -= sent;
	return true;
}

/* ---- frame handoff ------------------------------------------------------------------- */

static void drop_pending(struct se_frames_client *c, bool release)
{
	if (!c->pending.valid)
		return;
	if (c->pending.fence >= 0)
		close(c->pending.fence);
	if (release)
		queue_tx(c, c->pending.epoch, c->pending.buffer, c->pending.seq);
	c->pending.valid = false;
	c->pending.fence = -1;
}

static void publish_ready(struct se_frames_client *c, uint64_t epoch, uint32_t buffer, uint64_t seq, uint64_t mono)
{
	struct se_frames_ready old;
	bool had_old;
	pthread_mutex_lock(&c->mu);
	had_old = c->ready_valid;
	old = c->ready;
	c->ready_valid = true;
	c->ready = (struct se_frames_ready){epoch, buffer, seq, mono};
	if (had_old)
		c->stats.superseded++;
	pthread_mutex_unlock(&c->mu);
	/* never sampled: give it straight back */
	if (had_old)
		queue_tx(c, old.epoch, old.buffer, old.seq);
}

/* ---- connection ---------------------------------------------------------------------- */

static void set_connected(struct se_frames_client *c, bool connected)
{
	pthread_mutex_lock(&c->mu);
	c->stats.connected = connected;
	if (connected) {
		c->stats.connects++;
		c->stats.dmabuf = c->hello_dmabuf;
	} else {
		/* the engine forgets our holds when we disconnect; a ready frame may be overwritten */
		c->ready_valid = false;
		c->n_releases = 0;
	}
	pthread_mutex_unlock(&c->mu);
}

static void disconnect(struct se_frames_client *c)
{
	if (c->sock < 0)
		return;
	close(c->sock);
	c->sock = -1;
	drop_pending(c, false);
	c->n_tx = 0;
	c->have_canvas = false;
	set_connected(c, false);
	logf_(c, SE_LOG_INFO, "frames[%s]: disconnected", se_canvas_name(c->canvas));
}

static bool try_connect(struct se_frames_client *c)
{
	int fd = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
	if (fd < 0)
		return false;
	struct sockaddr_un addr = {.sun_family = AF_UNIX};
	memcpy(addr.sun_path, c->path, sizeof(addr.sun_path));
	if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
		if (!c->logged_connect_error) {
			logf_(c, SE_LOG_INFO, "frames[%s]: cannot connect to %s (%s); retrying", se_canvas_name(c->canvas), c->path,
			      strerror(errno));
			c->logged_connect_error = true;
		}
		close(fd);
		return false;
	}
	pthread_mutex_lock(&c->mu);
	c->hello_dmabuf = c->want_dmabuf;
	pthread_mutex_unlock(&c->mu);
	struct se_hello h = {
		.h = se_hdr_make(SE_MSG_HELLO),
		.client = c->client_kind,
		.want = 1u << c->canvas,
		.flags = c->hello_dmabuf ? SE_HELLO_FLAG_DMABUF : 0,
	};
	c->sock = fd;
	c->logged_connect_error = false;
	set_connected(c, true);
	if (send(fd, &h, sizeof(h), MSG_NOSIGNAL) != (ssize_t)sizeof(h)) {
		logf_(c, SE_LOG_WARNING, "frames[%s]: hello failed: %s", se_canvas_name(c->canvas), strerror(errno));
		disconnect(c);
		return false;
	}
	logf_(c, SE_LOG_INFO, "frames[%s]: connected to %s (%s)", se_canvas_name(c->canvas), c->path,
	      c->hello_dmabuf ? "dmabuf" : "shm");
	return true;
}

/* ---- messages ------------------------------------------------------------------------ */

static bool fourcc_supported(uint32_t f)
{
	return f == SE_DRM_FORMAT_ABGR8888 || f == SE_DRM_FORMAT_XBGR8888 || f == SE_DRM_FORMAT_ARGB8888 ||
	       f == SE_DRM_FORMAT_XRGB8888;
}

static void protocol_error(struct se_frames_client *c, const char *what)
{
	pthread_mutex_lock(&c->mu);
	c->stats.protocol_errors++;
	pthread_mutex_unlock(&c->mu);
	logf_(c, SE_LOG_WARNING, "frames[%s]: protocol error: %s", se_canvas_name(c->canvas), what);
}

/* Returns false if the connection must be dropped. Takes ownership of fds. */
static bool on_canvas(struct se_frames_client *c, const struct se_canvas *m, int *fds, size_t nfds)
{
	if (m->canvas != c->canvas) {
		close_fds(fds, nfds);
		return true;
	}
	if (m->width == 0 || m->height == 0 || m->width > MAX_DIM || m->height > MAX_DIM || m->planes != 1 ||
	    m->buffer_count == 0 || m->buffer_count > SE_MAX_BUFFERS || nfds != m->buffer_count ||
	    m->strides[0] < m->width * 4u) {
		close_fds(fds, nfds);
		protocol_error(c, "invalid se_canvas");
		return false;
	}
	if (m->drm_fourcc != 0 && !fourcc_supported(m->drm_fourcc)) {
		close_fds(fds, nfds);
		logf_(c, SE_LOG_WARNING, "frames[%s]: unsupported fourcc 0x%08x; switching to the shm path",
		      se_canvas_name(c->canvas), m->drm_fourcc);
		pthread_mutex_lock(&c->mu);
		c->want_dmabuf = false;
		pthread_mutex_unlock(&c->mu);
		return false;
	}
	struct se_frames_import *imp = calloc(1, sizeof(*imp));
	if (!imp) {
		close_fds(fds, nfds);
		return false;
	}
	for (uint32_t i = 0; i < SE_MAX_BUFFERS; i++)
		imp->fds[i] = -1;
	imp->epoch = c->epoch + 1;
	imp->generation = m->generation;
	imp->canvas = m->canvas;
	imp->width = m->width;
	imp->height = m->height;
	imp->fourcc = m->drm_fourcc;
	imp->modifier = m->modifier;
	memcpy(imp->offsets, m->offsets, sizeof(imp->offsets));
	memcpy(imp->strides, m->strides, sizeof(imp->strides));
	imp->buffer_count = m->buffer_count;
	for (uint32_t i = 0; i < m->buffer_count; i++)
		imp->fds[i] = fds[i];
	if (m->drm_fourcc == 0) {
		imp->map_size = (size_t)m->offsets[0] + (size_t)m->strides[0] * m->height;
		for (uint32_t i = 0; i < m->buffer_count; i++) {
			struct stat st;
			if (fstat(imp->fds[i], &st) < 0 || (size_t)st.st_size < imp->map_size) {
				protocol_error(c, "shm buffer smaller than stride*height");
				se_frames_import_free(imp);
				return false;
			}
			void *p = mmap(NULL, imp->map_size, PROT_READ, MAP_SHARED, imp->fds[i], 0);
			if (p == MAP_FAILED) {
				logf_(c, SE_LOG_WARNING, "frames[%s]: mmap: %s", se_canvas_name(c->canvas), strerror(errno));
				se_frames_import_free(imp);
				return false;
			}
			imp->maps[i] = p;
		}
	}

	drop_pending(c, false);
	c->n_tx = 0;
	c->epoch = imp->epoch;
	c->have_canvas = true;
	c->generation = m->generation;
	c->buffer_count = m->buffer_count;

	struct se_frames_import *old;
	pthread_mutex_lock(&c->mu);
	old = c->pending_import;
	c->pending_import = imp;
	c->ready_valid = false;
	c->n_releases = 0;
	c->stats.goodbye = false;
	c->stats.goodbye_reason = 0;
	c->stats.width = m->width;
	c->stats.height = m->height;
	c->stats.fourcc = m->drm_fourcc;
	c->stats.generation = m->generation;
	pthread_mutex_unlock(&c->mu);
	se_frames_import_free(old);
	logf_(c, SE_LOG_INFO, "frames[%s]: canvas %ux%u gen %u, %u buffers, %s (fourcc 0x%08x modifier 0x%016llx stride %u)",
	      se_canvas_name(c->canvas), m->width, m->height, m->generation, m->buffer_count,
	      m->drm_fourcc ? "dmabuf" : "shm", m->drm_fourcc, (unsigned long long)m->modifier, m->strides[0]);
	return true;
}

static bool on_frame(struct se_frames_client *c, const struct se_frame *m, int *fds, size_t nfds)
{
	if (m->canvas != c->canvas || !c->have_canvas || m->generation != c->generation) {
		close_fds(fds, nfds);
		return true;
	}
	if (m->buffer >= c->buffer_count || (m->has_fence ? nfds != 1 : nfds != 0)) {
		close_fds(fds, nfds);
		protocol_error(c, "invalid se_frame");
		return false;
	}
	uint64_t now = se_mono_ns();
	pthread_mutex_lock(&c->mu);
	c->stats.last_frame_ns = now;
	c->stats.frames++;
	pthread_mutex_unlock(&c->mu);

	/* an older frame still waiting for its fence is superseded (never sampled) */
	if (c->pending.valid) {
		pthread_mutex_lock(&c->mu);
		c->stats.superseded++;
		pthread_mutex_unlock(&c->mu);
		drop_pending(c, true);
	}
	if (!m->has_fence) {
		publish_ready(c, c->epoch, m->buffer, m->seq, m->monotonic_ns);
		return true;
	}
	c->pending = (struct pending_frame){
		.valid = true,
		.epoch = c->epoch,
		.buffer = m->buffer,
		.seq = m->seq,
		.mono = m->monotonic_ns,
		.fence = fds[0],
		.deadline = now + (uint64_t)c->fence_timeout_ms * 1000000ull,
	};
	return true;
}

static void on_goodbye(struct se_frames_client *c, const struct se_goodbye *m)
{
	if (m->canvas != SE_CANVAS_ALL && m->canvas != c->canvas)
		return;
	drop_pending(c, false);
	pthread_mutex_lock(&c->mu);
	c->stats.goodbye = true;
	c->stats.goodbye_reason = m->reason;
	pthread_mutex_unlock(&c->mu);
	logf_(c, SE_LOG_INFO, "frames[%s]: engine goodbye (reason %u); keeping the last frame", se_canvas_name(c->canvas),
	      m->reason);
}

/* Drains the socket. Returns false if the connection must be dropped. */
static bool read_socket(struct se_frames_client *c)
{
	for (;;) {
		union {
			struct se_canvas canvas;
			struct se_frame frame;
			struct se_goodbye goodbye;
			uint8_t raw[256];
		} buf;
		union {
			char b[CMSG_SPACE(sizeof(int) * (SE_MAX_BUFFERS + 1))];
			struct cmsghdr align;
		} ctl;
		struct iovec iov = {.iov_base = &buf, .iov_len = sizeof(buf)};
		struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = ctl.b, .msg_controllen = sizeof(ctl.b)};
		ssize_t n = recvmsg(c->sock, &msg, MSG_DONTWAIT | MSG_CMSG_CLOEXEC);
		if (n < 0) {
			if (errno == EINTR)
				continue;
			if (errno == EAGAIN || errno == EWOULDBLOCK)
				return true;
			logf_(c, SE_LOG_WARNING, "frames[%s]: recv: %s", se_canvas_name(c->canvas), strerror(errno));
			return false;
		}
		if (n == 0)
			return false; /* engine closed the connection */

		int fds[SE_MAX_BUFFERS + 1];
		size_t nfds = 0;
		for (struct cmsghdr *cm = CMSG_FIRSTHDR(&msg); cm; cm = CMSG_NXTHDR(&msg, cm)) {
			if (cm->cmsg_level != SOL_SOCKET || cm->cmsg_type != SCM_RIGHTS)
				continue;
			size_t count = (cm->cmsg_len - CMSG_LEN(0)) / sizeof(int);
			const int *p = (const int *)CMSG_DATA(cm);
			for (size_t i = 0; i < count; i++) {
				if (nfds < SE_MAX_BUFFERS + 1)
					fds[nfds++] = p[i];
				else
					close(p[i]);
			}
		}
		if (msg.msg_flags & (MSG_TRUNC | MSG_CTRUNC)) {
			close_fds(fds, nfds);
			protocol_error(c, "truncated message");
			return false;
		}
		if ((size_t)n < sizeof(struct se_hdr) || buf.canvas.h.magic != SE_MAGIC || buf.canvas.h.version != SE_VERSION) {
			close_fds(fds, nfds);
			protocol_error(c, "bad header (magic/version)");
			return false;
		}
		bool ok = true;
		switch (buf.canvas.h.type) {
		case SE_MSG_CANVAS:
			if ((size_t)n < sizeof(struct se_canvas)) {
				close_fds(fds, nfds);
				protocol_error(c, "short se_canvas");
				return false;
			}
			ok = on_canvas(c, &buf.canvas, fds, nfds);
			break;
		case SE_MSG_FRAME:
			if ((size_t)n < sizeof(struct se_frame)) {
				close_fds(fds, nfds);
				protocol_error(c, "short se_frame");
				return false;
			}
			ok = on_frame(c, &buf.frame, fds, nfds);
			break;
		case SE_MSG_GOODBYE:
			close_fds(fds, nfds);
			if ((size_t)n < sizeof(struct se_goodbye)) {
				protocol_error(c, "short se_goodbye");
				return false;
			}
			on_goodbye(c, &buf.goodbye);
			break;
		default:
			/* newer engine: ignore unknown messages */
			close_fds(fds, nfds);
			break;
		}
		if (!ok)
			return false;
	}
}

static void check_fence(struct se_frames_client *c, short revents)
{
	if (!c->pending.valid)
		return;
	if (revents & POLLIN) {
		struct pending_frame p = c->pending;
		close(p.fence);
		c->pending.valid = false;
		c->pending.fence = -1;
		publish_ready(c, p.epoch, p.buffer, p.seq, p.mono);
		return;
	}
	if (revents & (POLLERR | POLLNVAL)) {
		logf_(c, SE_LOG_WARNING, "frames[%s]: fence error; dropping frame %llu", se_canvas_name(c->canvas),
		      (unsigned long long)c->pending.seq);
		drop_pending(c, true);
		return;
	}
	if (se_mono_ns() >= c->pending.deadline) {
		pthread_mutex_lock(&c->mu);
		c->stats.fence_timeouts++;
		pthread_mutex_unlock(&c->mu);
		logf_(c, SE_LOG_WARNING, "frames[%s]: fence of frame %llu did not signal within %u ms; dropping it",
		      se_canvas_name(c->canvas), (unsigned long long)c->pending.seq, c->fence_timeout_ms);
		drop_pending(c, true);
	}
}

static void take_releases(struct se_frames_client *c)
{
	struct release_item items[RELEASE_QUEUE_CAP];
	size_t n;
	pthread_mutex_lock(&c->mu);
	n = c->n_releases;
	memcpy(items, c->releases, sizeof(items[0]) * n);
	c->n_releases = 0;
	pthread_mutex_unlock(&c->mu);
	for (size_t i = 0; i < n; i++)
		queue_tx(c, items[i].epoch, items[i].buffer, items[i].seq);
}

static void *io_thread(void *arg)
{
	struct se_frames_client *c = arg;
	uint32_t backoff = BACKOFF_MIN_MS;
	uint64_t next_attempt = 0;

	for (;;) {
		pthread_mutex_lock(&c->mu);
		bool stop = c->stop;
		bool reconnect = c->sock >= 0 && c->want_dmabuf != c->hello_dmabuf;
		pthread_mutex_unlock(&c->mu);
		if (stop)
			break;
		if (reconnect)
			disconnect(c);

		uint64_t now = se_mono_ns();
		if (c->sock < 0 && now >= next_attempt) {
			if (try_connect(c)) {
				backoff = BACKOFF_MIN_MS;
			} else {
				next_attempt = now + (uint64_t)backoff * 1000000ull;
				backoff = backoff * 2 > BACKOFF_MAX_MS ? BACKOFF_MAX_MS : backoff * 2;
			}
		}

		struct pollfd pfd[3];
		int npfd = 0;
		pfd[npfd++] = (struct pollfd){.fd = c->wake_fd, .events = POLLIN};
		int sock_i = -1, fence_i = -1;
		const uint64_t polled_seq = c->pending.seq, polled_epoch = c->pending.epoch;
		if (c->sock >= 0) {
			sock_i = npfd;
			pfd[npfd++] = (struct pollfd){.fd = c->sock, .events = (short)(POLLIN | (c->n_tx ? POLLOUT : 0))};
			if (c->pending.valid) {
				fence_i = npfd;
				pfd[npfd++] = (struct pollfd){.fd = c->pending.fence, .events = POLLIN};
			}
		}
		int timeout = HOUSEKEEPING_MS;
		if (c->sock < 0) {
			uint64_t wait_ns = next_attempt > now ? next_attempt - now : 0;
			timeout = (int)(wait_ns / 1000000ull) + 1;
		} else if (c->pending.valid) {
			uint64_t left = c->pending.deadline > now ? c->pending.deadline - now : 0;
			int ms = (int)(left / 1000000ull) + 1;
			if (ms < timeout)
				timeout = ms;
		}
		int r = poll(pfd, (nfds_t)npfd, timeout);
		if (r < 0 && errno != EINTR) {
			logf_(c, SE_LOG_ERROR, "frames[%s]: poll: %s", se_canvas_name(c->canvas), strerror(errno));
			break;
		}
		if (r > 0 && (pfd[0].revents & POLLIN))
			drain_wake(c);
		if (c->sock < 0)
			continue;

		if (sock_i >= 0 && (pfd[sock_i].revents & (POLLIN | POLLHUP | POLLERR))) {
			if (!read_socket(c)) {
				disconnect(c);
				next_attempt = se_mono_ns() + (uint64_t)backoff * 1000000ull;
				continue;
			}
		}
		if (c->pending.valid) {
			/* revents only belong to the pending frame if read_socket() did not replace it */
			bool same = fence_i >= 0 && r > 0 && c->pending.seq == polled_seq && c->pending.epoch == polled_epoch;
			check_fence(c, same ? pfd[fence_i].revents : 0);
		}
		take_releases(c);
		if (c->n_tx && !flush_tx(c)) {
			disconnect(c);
			next_attempt = se_mono_ns() + (uint64_t)backoff * 1000000ull;
		}
	}
	disconnect(c);
	return NULL;
}

/* ---- public API ---------------------------------------------------------------------- */

struct se_frames_client *se_frames_client_start(const struct se_frames_opts *opts)
{
	if (!opts || !opts->socket_path || opts->canvas >= SE_CANVAS_COUNT)
		return NULL;
	struct se_frames_client *c = calloc(1, sizeof(*c));
	if (!c)
		return NULL;
	if (strlen(opts->socket_path) >= sizeof(c->path)) {
		free(c);
		return NULL;
	}
	strcpy(c->path, opts->socket_path);
	c->canvas = opts->canvas;
	c->client_kind = opts->client_kind ? opts->client_kind : SE_CLIENT_OTHER;
	c->fence_timeout_ms = opts->fence_timeout_ms ? opts->fence_timeout_ms : 1000;
	c->log = opts->log;
	c->log_ud = opts->log_ud;
	c->want_dmabuf = opts->dmabuf;
	c->sock = -1;
	c->pending.fence = -1;
	c->wake_fd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
	if (c->wake_fd < 0) {
		free(c);
		return NULL;
	}
	pthread_mutex_init(&c->mu, NULL);
	if (pthread_create(&c->thread, NULL, io_thread, c) != 0) {
		pthread_mutex_destroy(&c->mu);
		close(c->wake_fd);
		free(c);
		return NULL;
	}
	return c;
}

void se_frames_client_stop(struct se_frames_client *c)
{
	if (!c)
		return;
	pthread_mutex_lock(&c->mu);
	c->stop = true;
	pthread_mutex_unlock(&c->mu);
	wake(c);
	pthread_join(c->thread, NULL);
	se_frames_import_free(c->pending_import);
	close(c->wake_fd);
	pthread_mutex_destroy(&c->mu);
	free(c);
}

void se_frames_client_poll(struct se_frames_client *c, struct se_frames_update *out)
{
	pthread_mutex_lock(&c->mu);
	out->import = c->pending_import;
	c->pending_import = NULL;
	out->has_frame = c->ready_valid;
	out->frame = c->ready;
	c->ready_valid = false;
	pthread_mutex_unlock(&c->mu);
}

void se_frames_client_release(struct se_frames_client *c, uint64_t epoch, uint32_t buffer, uint64_t seq)
{
	bool queued = false;
	pthread_mutex_lock(&c->mu);
	if (c->stats.connected && c->n_releases < RELEASE_QUEUE_CAP) {
		c->releases[c->n_releases++] = (struct release_item){epoch, buffer, seq};
		queued = true;
	}
	pthread_mutex_unlock(&c->mu);
	if (queued)
		wake(c);
}

void se_frames_client_set_dmabuf(struct se_frames_client *c, bool dmabuf)
{
	pthread_mutex_lock(&c->mu);
	bool changed = c->want_dmabuf != dmabuf;
	c->want_dmabuf = dmabuf;
	pthread_mutex_unlock(&c->mu);
	if (changed)
		wake(c);
}

void se_frames_client_stats(struct se_frames_client *c, struct se_frames_stats *out)
{
	pthread_mutex_lock(&c->mu);
	*out = c->stats;
	pthread_mutex_unlock(&c->mu);
}
