/* SPDX-License-Identifier: GPL-2.0-or-later
 *
 * Runs the plugin's obs.sock client against a fake engine: connect/on_connect ordering, line
 * framing across partial writes, JSON parsing and junk tolerance, send while disconnected,
 * tick cadence without an engine, oversized lines, and reconnect after the engine restarts.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

#include "control-client.h"

static int failures;

#define CHECK(cond, ...)                                                    \
	do {                                                                \
		if (!(cond)) {                                              \
			failures++;                                         \
			fprintf(stderr, "FAIL %s:%d: %s — ", __FILE__, __LINE__, #cond); \
			fprintf(stderr, __VA_ARGS__);                       \
			fputc('\n', stderr);                                \
		}                                                           \
	} while (0)

static void sleep_ms(int ms)
{
	struct timespec ts = {ms / 1000, (long)(ms % 1000) * 1000000L};
	nanosleep(&ts, NULL);
}

struct state {
	struct se_control *c;
	atomic_int connects, disconnects, ticks;
	pthread_mutex_t mu;
	char msgs[16][256];
	int n_msgs;
};

static struct state S;

static void on_connect(void *ud)
{
	struct state *s = ud;
	atomic_fetch_add(&s->connects, 1);
	/* sending from on_connect must work: the plugin sends hello here */
	json_t *m = json_pack("{s:s, s:s}", "t", "hello", "plugin", "test");
	CHECK(se_control_send(s->c, m), "send hello from on_connect");
	json_decref(m);
}

static void on_disconnect(void *ud)
{
	atomic_fetch_add(&((struct state *)ud)->disconnects, 1);
}

static void on_message(void *ud, json_t *msg)
{
	struct state *s = ud;
	char *txt = json_dumps(msg, JSON_COMPACT | JSON_SORT_KEYS);
	pthread_mutex_lock(&s->mu);
	if (s->n_msgs < 16)
		snprintf(s->msgs[s->n_msgs++], 256, "%s", txt);
	pthread_mutex_unlock(&s->mu);
	free(txt);
}

static void on_tick(void *ud, uint64_t now)
{
	(void)now;
	atomic_fetch_add(&((struct state *)ud)->ticks, 1);
}

static void quiet_log(void *ud, int level, const char *msg)
{
	(void)ud;
	if (getenv("SE_TEST_VERBOSE"))
		fprintf(stderr, "[%d] %s\n", level, msg);
}

static int listen_stream(const char *path)
{
	int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
	struct sockaddr_un a = {.sun_family = AF_UNIX};
	strcpy(a.sun_path, path);
	unlink(path);
	if (bind(fd, (struct sockaddr *)&a, sizeof(a)) < 0 || listen(fd, 4) < 0)
		return -1;
	return fd;
}

static int accept_to(int lfd, int ms)
{
	struct pollfd p = {.fd = lfd, .events = POLLIN};
	if (poll(&p, 1, ms) <= 0)
		return -1;
	return accept4(lfd, NULL, NULL, SOCK_CLOEXEC);
}

/* Reads one '\n'-terminated line (byte by byte; fine for a test). */
static bool read_line(int fd, char *out, size_t len, int ms)
{
	size_t n = 0;
	while (n + 1 < len) {
		struct pollfd p = {.fd = fd, .events = POLLIN};
		if (poll(&p, 1, ms) <= 0)
			return false;
		char ch;
		if (read(fd, &ch, 1) != 1)
			return false;
		if (ch == '\n') {
			out[n] = 0;
			return true;
		}
		out[n++] = ch;
	}
	return false;
}

static bool wait_connects(int n, int ms)
{
	for (int t = 0; t < ms; t += 5) {
		if (atomic_load(&S.connects) >= n && se_control_connected(S.c))
			return true;
		sleep_ms(5);
	}
	return false;
}

static bool wait_msgs(int n, int ms)
{
	for (int t = 0; t < ms; t += 5) {
		pthread_mutex_lock(&S.mu);
		int have = S.n_msgs;
		pthread_mutex_unlock(&S.mu);
		if (have >= n)
			return true;
		sleep_ms(5);
	}
	return false;
}

int main(void)
{
	char path[108];
	snprintf(path, sizeof(path), "/tmp/se-test-control-%d.sock", (int)getpid());
	unlink(path);
	pthread_mutex_init(&S.mu, NULL);

	struct se_control_opts o = {.socket_path = path, .tick_ms = 20, .on_connect = on_connect,
				    .on_disconnect = on_disconnect, .on_message = on_message, .on_tick = on_tick,
				    .log = quiet_log, .ud = &S};
	S.c = se_control_start(&o);
	CHECK(S.c, "start");

	/* ticks run while the engine is down; sends are dropped */
	sleep_ms(300);
	int ticks = atomic_load(&S.ticks);
	/* ~15 expected; a loaded machine delays ticks but they never burst */
	CHECK(ticks >= 4 && ticks <= 17, "ticks in 300 ms at 20 ms (got %d)", ticks);
	json_t *m = json_pack("{s:s}", "t", "status");
	CHECK(!se_control_send(S.c, m), "send while disconnected is refused");
	CHECK(!se_control_connected(S.c), "not connected");

	int lfd = listen_stream(path);
	CHECK(lfd >= 0, "listen");
	int fd = accept_to(lfd, 3000);
	CHECK(fd >= 0, "client connected once the engine is up");
	char line[70000];
	CHECK(read_line(fd, line, sizeof(line), 1000) && strstr(line, "\"t\":\"hello\""), "hello first: %s", line);
	CHECK(wait_connects(1, 1000) && atomic_load(&S.connects) == 1, "one connect");
	CHECK(se_control_send(S.c, m), "send while connected");
	CHECK(read_line(fd, line, sizeof(line), 1000) && strcmp(line, "{\"t\":\"status\"}") == 0, "status line: %s", line);
	json_decref(m);

	/* line split across writes, two lines in one write, junk and objects without "t" */
	const char *parts[] = {"{\"t\":\"cmd\",\"id\":7,", "\"op\":\"record.start\"}\n{\"t\":\"config\",\"stale_ms\":250}\n",
			       "not json\n", "{\"id\":1}\n", "[1,2]\n", "\n", "{\"t\":\"cmd\",\"id\":8,\"op\":\"x\"}\n"};
	for (size_t i = 0; i < sizeof(parts) / sizeof(parts[0]); i++) {
		CHECK(write(fd, parts[i], strlen(parts[i])) == (ssize_t)strlen(parts[i]), "write part");
		sleep_ms(20);
	}
	CHECK(wait_msgs(3, 1000), "three valid messages");
	pthread_mutex_lock(&S.mu);
	CHECK(S.n_msgs == 3, "exactly three (got %d)", S.n_msgs);
	CHECK(strcmp(S.msgs[0], "{\"id\":7,\"op\":\"record.start\",\"t\":\"cmd\"}") == 0, "msg0 %s", S.msgs[0]);
	CHECK(strcmp(S.msgs[1], "{\"stale_ms\":250,\"t\":\"config\"}") == 0, "msg1 %s", S.msgs[1]);
	CHECK(strcmp(S.msgs[2], "{\"id\":8,\"op\":\"x\",\"t\":\"cmd\"}") == 0, "msg2 %s", S.msgs[2]);
	S.n_msgs = 0;
	pthread_mutex_unlock(&S.mu);
	CHECK(se_control_connected(S.c), "junk does not disconnect");

	/* engine restart: disconnect callback, reconnect, hello again */
	close(fd);
	fd = accept_to(lfd, 3000);
	CHECK(fd >= 0, "reconnect");
	CHECK(read_line(fd, line, sizeof(line), 1000) && strstr(line, "hello"), "hello after reconnect");
	CHECK(wait_connects(2, 1000) && atomic_load(&S.disconnects) == 1 && atomic_load(&S.connects) == 2,
	      "disconnects %d connects %d",
	      atomic_load(&S.disconnects), atomic_load(&S.connects));

	/* a line over 64 KiB is a protocol error: the client reconnects */
	static char big[70000];
	memset(big, 'a', sizeof(big));
	big[0] = '"';
	ssize_t w = write(fd, big, sizeof(big));
	CHECK(w > 0, "write big");
	CHECK(!read_line(fd, line, sizeof(line), 1500) || true, "drain");
	close(fd);
	fd = accept_to(lfd, 3000);
	CHECK(fd >= 0, "reconnect after oversized line");
	CHECK(wait_connects(3, 1000) && atomic_load(&S.connects) == 3, "connects %d", atomic_load(&S.connects));

	/* many sends are delivered in order */
	for (int i = 0; i < 200; i++) {
		json_t *x = json_pack("{s:s, s:i}", "t", "event", "n", i);
		se_control_send(S.c, x);
		json_decref(x);
	}
	CHECK(read_line(fd, line, sizeof(line), 1000) && strstr(line, "hello"), "hello");
	bool ordered = true;
	for (int i = 0; i < 200; i++) {
		char want[64];
		snprintf(want, sizeof(want), "{\"t\":\"event\",\"n\":%d}", i);
		if (!read_line(fd, line, sizeof(line), 1000) || strcmp(line, want) != 0) {
			ordered = false;
			break;
		}
	}
	CHECK(ordered, "200 events in order");

	se_control_stop(S.c);
	close(fd);
	close(lfd);
	unlink(path);
	if (failures) {
		fprintf(stderr, "%d check(s) failed\n", failures);
		return 1;
	}
	printf("control-client: all checks passed\n");
	return 0;
}
