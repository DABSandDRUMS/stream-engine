/* SPDX-License-Identifier: GPL-2.0-or-later */
#define _GNU_SOURCE
#include "control-client.h"

#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#define BACKOFF_MIN_MS 250
#define BACKOFF_MAX_MS 5000
#define MAX_LINE (64 * 1024)
#define MAX_TX (1024 * 1024)

struct se_control {
	char path[sizeof(((struct sockaddr_un *)0)->sun_path)];
	uint32_t tick_ms;
	struct se_control_opts o;
	int wake_fd;
	pthread_t thread;

	pthread_mutex_t mu; /* guards stop, connected, tx */
	bool stop;
	bool connected;
	bool overflow;
	char *tx;
	size_t tx_len, tx_cap;

	/* control thread only */
	int sock;
	char rx[MAX_LINE];
	size_t rx_len;
	bool logged_connect_error;
};

static void logf_(struct se_control *c, int level, const char *fmt, ...) __attribute__((format(printf, 3, 4)));
static void logf_(struct se_control *c, int level, const char *fmt, ...)
{
	if (!c->o.log)
		return;
	char buf[512];
	va_list ap;
	va_start(ap, fmt);
	vsnprintf(buf, sizeof(buf), fmt, ap);
	va_end(ap);
	c->o.log(c->o.ud, level, buf);
}

static void wake(struct se_control *c)
{
	uint64_t one = 1;
	ssize_t r = write(c->wake_fd, &one, sizeof(one));
	(void)r;
}

static void disconnect(struct se_control *c, const char *why)
{
	if (c->sock < 0)
		return;
	close(c->sock);
	c->sock = -1;
	c->rx_len = 0;
	pthread_mutex_lock(&c->mu);
	c->connected = false;
	c->tx_len = 0;
	c->overflow = false;
	pthread_mutex_unlock(&c->mu);
	logf_(c, SE_LOG_INFO, "control: disconnected from engine (%s)", why);
	if (c->o.on_disconnect)
		c->o.on_disconnect(c->o.ud);
}

static bool try_connect(struct se_control *c)
{
	int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
	if (fd < 0)
		return false;
	struct sockaddr_un addr = {.sun_family = AF_UNIX};
	memcpy(addr.sun_path, c->path, sizeof(addr.sun_path));
	if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
		if (!c->logged_connect_error) {
			logf_(c, SE_LOG_INFO, "control: cannot connect to %s (%s); retrying", c->path, strerror(errno));
			c->logged_connect_error = true;
		}
		close(fd);
		return false;
	}
	c->sock = fd;
	c->rx_len = 0;
	c->logged_connect_error = false;
	pthread_mutex_lock(&c->mu);
	c->connected = true;
	c->tx_len = 0;
	c->overflow = false;
	pthread_mutex_unlock(&c->mu);
	logf_(c, SE_LOG_INFO, "control: connected to %s", c->path);
	if (c->o.on_connect)
		c->o.on_connect(c->o.ud);
	return true;
}

static void dispatch_line(struct se_control *c, const char *line, size_t len)
{
	if (len == 0)
		return;
	json_error_t err;
	json_t *msg = json_loadb(line, len, JSON_REJECT_DUPLICATES, &err);
	if (!msg) {
		logf_(c, SE_LOG_WARNING, "control: invalid JSON from engine: %s", err.text);
		return;
	}
	if (!json_is_object(msg) || !json_is_string(json_object_get(msg, "t"))) {
		logf_(c, SE_LOG_WARNING, "control: message without \"t\" ignored");
	} else if (c->o.on_message) {
		c->o.on_message(c->o.ud, msg);
	}
	json_decref(msg);
}

/* Returns false if the connection must be dropped. */
static bool read_socket(struct se_control *c)
{
	for (;;) {
		if (c->rx_len == sizeof(c->rx)) {
			logf_(c, SE_LOG_WARNING, "control: line longer than %d bytes", MAX_LINE);
			return false;
		}
		ssize_t n = recv(c->sock, c->rx + c->rx_len, sizeof(c->rx) - c->rx_len, MSG_DONTWAIT);
		if (n < 0) {
			if (errno == EINTR)
				continue;
			if (errno == EAGAIN || errno == EWOULDBLOCK)
				return true;
			return false;
		}
		if (n == 0)
			return false;
		size_t start = 0, scan = c->rx_len;
		c->rx_len += (size_t)n;
		for (size_t i = scan; i < c->rx_len; i++) {
			if (c->rx[i] == '\n') {
				dispatch_line(c, c->rx + start, i - start);
				start = i + 1;
			}
		}
		if (start > 0) {
			memmove(c->rx, c->rx + start, c->rx_len - start);
			c->rx_len -= start;
		}
	}
}

/* Returns false on a fatal error. */
static bool flush_tx(struct se_control *c)
{
	pthread_mutex_lock(&c->mu);
	if (c->overflow) {
		pthread_mutex_unlock(&c->mu);
		logf_(c, SE_LOG_WARNING, "control: engine is not reading (send queue over %d bytes)", MAX_TX);
		return false;
	}
	size_t off = 0;
	bool ok = true;
	while (off < c->tx_len) {
		ssize_t n = send(c->sock, c->tx + off, c->tx_len - off, MSG_DONTWAIT | MSG_NOSIGNAL);
		if (n < 0) {
			if (errno == EINTR)
				continue;
			if (errno != EAGAIN && errno != EWOULDBLOCK)
				ok = false;
			break;
		}
		off += (size_t)n;
	}
	memmove(c->tx, c->tx + off, c->tx_len - off);
	c->tx_len -= off;
	pthread_mutex_unlock(&c->mu);
	return ok;
}

static bool tx_pending(struct se_control *c)
{
	pthread_mutex_lock(&c->mu);
	bool p = c->tx_len > 0 || c->overflow;
	pthread_mutex_unlock(&c->mu);
	return p;
}

static void *control_thread(void *arg)
{
	struct se_control *c = arg;
	uint32_t backoff = BACKOFF_MIN_MS;
	uint64_t next_attempt = 0;
	uint64_t next_tick = se_mono_ns();

	for (;;) {
		pthread_mutex_lock(&c->mu);
		bool stop = c->stop;
		pthread_mutex_unlock(&c->mu);
		if (stop)
			break;

		uint64_t now = se_mono_ns();
		if (c->sock < 0 && now >= next_attempt) {
			if (try_connect(c)) {
				backoff = BACKOFF_MIN_MS;
			} else {
				next_attempt = now + (uint64_t)backoff * 1000000ull;
				backoff = backoff * 2 > BACKOFF_MAX_MS ? BACKOFF_MAX_MS : backoff * 2;
			}
		}
		if (now >= next_tick) {
			if (c->o.on_tick)
				c->o.on_tick(c->o.ud, now);
			next_tick += (uint64_t)c->tick_ms * 1000000ull;
			if (next_tick <= now) /* stalled (suspend, debugger): do not burst */
				next_tick = now + (uint64_t)c->tick_ms * 1000000ull;
		}
		if (c->sock >= 0 && tx_pending(c) && !flush_tx(c)) {
			disconnect(c, "send failed");
			next_attempt = se_mono_ns() + (uint64_t)backoff * 1000000ull;
			continue;
		}

		now = se_mono_ns();
		uint64_t until = next_tick;
		if (c->sock < 0 && next_attempt < until)
			until = next_attempt;
		int timeout = until > now ? (int)((until - now + 999999ull) / 1000000ull) : 0;
		struct pollfd pfd[2];
		int npfd = 0;
		pfd[npfd++] = (struct pollfd){.fd = c->wake_fd, .events = POLLIN};
		if (c->sock >= 0)
			pfd[npfd++] = (struct pollfd){.fd = c->sock, .events = (short)(POLLIN | (tx_pending(c) ? POLLOUT : 0))};
		int r = poll(pfd, (nfds_t)npfd, timeout);
		if (r < 0) {
			if (errno == EINTR)
				continue;
			logf_(c, SE_LOG_ERROR, "control: poll: %s", strerror(errno));
			break;
		}
		if (pfd[0].revents & POLLIN) {
			uint64_t v;
			ssize_t n = read(c->wake_fd, &v, sizeof(v));
			(void)n;
		}
		if (c->sock >= 0 && npfd > 1 && (pfd[1].revents & (POLLIN | POLLHUP | POLLERR))) {
			if (!read_socket(c)) {
				disconnect(c, "closed by engine");
				next_attempt = se_mono_ns() + (uint64_t)backoff * 1000000ull;
			}
		}
	}
	disconnect(c, "plugin stopping");
	return NULL;
}

struct se_control *se_control_start(const struct se_control_opts *opts)
{
	if (!opts || !opts->socket_path)
		return NULL;
	struct se_control *c = calloc(1, sizeof(*c));
	if (!c)
		return NULL;
	if (strlen(opts->socket_path) >= sizeof(c->path)) {
		free(c);
		return NULL;
	}
	strcpy(c->path, opts->socket_path);
	c->o = *opts;
	c->o.socket_path = c->path;
	c->tick_ms = opts->tick_ms ? opts->tick_ms : 100;
	c->sock = -1;
	c->wake_fd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
	if (c->wake_fd < 0) {
		free(c);
		return NULL;
	}
	pthread_mutex_init(&c->mu, NULL);
	if (pthread_create(&c->thread, NULL, control_thread, c) != 0) {
		pthread_mutex_destroy(&c->mu);
		close(c->wake_fd);
		free(c);
		return NULL;
	}
	return c;
}

void se_control_stop(struct se_control *c)
{
	if (!c)
		return;
	pthread_mutex_lock(&c->mu);
	c->stop = true;
	pthread_mutex_unlock(&c->mu);
	wake(c);
	pthread_join(c->thread, NULL);
	close(c->wake_fd);
	pthread_mutex_destroy(&c->mu);
	free(c->tx);
	free(c);
}

bool se_control_send(struct se_control *c, json_t *msg)
{
	char *s = json_dumps(msg, JSON_COMPACT | JSON_ENSURE_ASCII);
	if (!s)
		return false;
	size_t len = strlen(s);
	bool queued = false;
	pthread_mutex_lock(&c->mu);
	if (c->connected && !c->overflow) {
		if (c->tx_len + len + 1 > MAX_TX) {
			c->overflow = true;
		} else {
			if (c->tx_len + len + 1 > c->tx_cap) {
				size_t cap = c->tx_cap ? c->tx_cap : 4096;
				while (cap < c->tx_len + len + 1)
					cap *= 2;
				char *p = realloc(c->tx, cap);
				if (p) {
					c->tx = p;
					c->tx_cap = cap;
				}
			}
			if (c->tx_len + len + 1 <= c->tx_cap) {
				memcpy(c->tx + c->tx_len, s, len);
				c->tx[c->tx_len + len] = '\n';
				c->tx_len += len + 1;
				queued = true;
			}
		}
	}
	pthread_mutex_unlock(&c->mu);
	free(s);
	if (queued)
		wake(c);
	return queued;
}

bool se_control_connected(struct se_control *c)
{
	pthread_mutex_lock(&c->mu);
	bool v = c->connected;
	pthread_mutex_unlock(&c->mu);
	return v;
}
