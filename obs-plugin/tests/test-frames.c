/* SPDX-License-Identifier: GPL-2.0-or-later
 *
 * Runs the plugin's frames.sock client against a fake engine: hello, shm canvases, frames with
 * and without sync fences (a pipe stands in for the sync_file: both signal with POLLIN),
 * supersede/release accounting, fence timeouts, generation changes, goodbye, protocol errors,
 * reconnects, and the dmabuf → shm fallback.
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#include "fake-server.h"
#include "frames-client.h"

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

static void quiet_log(void *ud, int level, const char *msg)
{
	(void)ud;
	if (getenv("SE_TEST_VERBOSE"))
		fprintf(stderr, "[%d] %s\n", level, msg);
}

/* Polls the client until `pred` holds or `ms` elapse. */
static bool poll_update(struct se_frames_client *c, struct se_frames_update *acc, int ms, bool want_import, bool want_frame)
{
	for (int t = 0; t < ms; t += 5) {
		struct se_frames_update u;
		se_frames_client_poll(c, &u);
		if (u.import) {
			se_frames_import_free(acc->import);
			acc->import = u.import;
		}
		if (u.has_frame) {
			acc->has_frame = true;
			acc->frame = u.frame;
		}
		if ((!want_import || acc->import) && (!want_frame || acc->has_frame))
			return true;
		sleep_ms(5);
	}
	return false;
}

static bool expect_release(int fd, uint32_t buffer, uint64_t seq, int ms)
{
	struct se_release r;
	ssize_t n = fs_recv(fd, &r, sizeof(r), ms);
	if (n != (ssize_t)sizeof(r) || r.h.type != SE_MSG_RELEASE) {
		fprintf(stderr, "  expected release buffer %u seq %llu, got n=%zd type=%u\n", buffer,
			(unsigned long long)seq, n, n >= 8 ? r.h.type : 0);
		return false;
	}
	if (r.buffer != buffer || r.seq != seq) {
		fprintf(stderr, "  expected release buffer %u seq %llu, got buffer %u seq %llu\n", buffer,
			(unsigned long long)seq, r.buffer, (unsigned long long)r.seq);
		return false;
	}
	return true;
}

static bool no_message(int fd, int ms)
{
	uint8_t buf[256];
	return fs_recv(fd, buf, sizeof(buf), ms) < 0;
}

struct shm_canvas {
	int fds[SE_MAX_BUFFERS];
	uint8_t *maps[SE_MAX_BUFFERS];
	uint32_t count, stride, height;
};

static bool make_shm(struct shm_canvas *s, uint32_t w, uint32_t h, uint32_t count)
{
	s->count = count;
	s->stride = w * 4 + 64; /* padded rows must be honored */
	s->height = h;
	for (uint32_t i = 0; i < count; i++) {
		s->fds[i] = fs_memfd("se-test", (size_t)s->stride * h);
		if (s->fds[i] < 0)
			return false;
		s->maps[i] = mmap(NULL, (size_t)s->stride * h, PROT_READ | PROT_WRITE, MAP_SHARED, s->fds[i], 0);
		if (s->maps[i] == MAP_FAILED)
			return false;
	}
	return true;
}

static void free_shm(struct shm_canvas *s)
{
	for (uint32_t i = 0; i < s->count; i++) {
		munmap(s->maps[i], (size_t)s->stride * s->height);
		close(s->fds[i]);
	}
}

static bool send_canvas_shm(int fd, struct shm_canvas *s, uint32_t canvas, uint32_t w, uint32_t h, uint32_t gen)
{
	struct se_canvas m = fs_canvas_msg(canvas, w, h, 0, 0, s->stride, s->count, gen);
	return fs_send(fd, &m, sizeof(m), s->fds, s->count);
}

static bool send_frame(int fd, uint32_t canvas, uint32_t buffer, uint64_t seq, uint32_t gen, int fence)
{
	struct se_frame f = fs_frame_msg(canvas, buffer, seq, gen, fence >= 0);
	return fs_send(fd, &f, sizeof(f), fence >= 0 ? &fence : NULL, fence >= 0 ? 1 : 0);
}

static void test_session(const char *path)
{
	int lfd = fs_listen(path);
	CHECK(lfd >= 0, "listen %s", path);
	struct se_frames_opts o = {.socket_path = path, .canvas = SE_CANVAS_TALL, .client_kind = SE_CLIENT_OBS,
				   .dmabuf = false, .fence_timeout_ms = 300, .log = quiet_log};
	struct se_frames_client *c = se_frames_client_start(&o);
	CHECK(c, "client start");

	int fd = fs_accept(lfd, 2000);
	CHECK(fd >= 0, "client connected");
	struct se_hello hello;
	CHECK(fs_recv(fd, &hello, sizeof(hello), 1000) == (ssize_t)sizeof(hello), "hello received");
	CHECK(hello.h.magic == SE_MAGIC && hello.h.type == SE_MSG_HELLO && hello.h.version == SE_VERSION, "hello header");
	CHECK(hello.client == SE_CLIENT_OBS, "client kind %u", hello.client);
	CHECK(hello.want == (1u << SE_CANVAS_TALL), "want %u", hello.want);
	CHECK(hello.flags == 0, "shm client must not advertise dmabuf (flags %u)", hello.flags);

	/* stale before any frame */
	struct se_frames_stats st;
	se_frames_client_stats(c, &st);
	CHECK(st.connected && st.last_frame_ns == 0, "connected, no frames yet");

	/* canvas for another canvas id is ignored */
	struct shm_canvas other;
	CHECK(make_shm(&other, 8, 8, 1), "shm");
	struct se_canvas wrong = fs_canvas_msg(SE_CANVAS_WIDE, 8, 8, 0, 0, other.stride, 1, 1);
	CHECK(fs_send(fd, &wrong, sizeof(wrong), other.fds, 1), "send wrong canvas");
	free_shm(&other);

	/* shm canvas, 3 buffers */
	struct shm_canvas shm;
	CHECK(make_shm(&shm, 64, 32, 3), "shm");
	CHECK(send_canvas_shm(fd, &shm, SE_CANVAS_TALL, 64, 32, 1), "send canvas");
	struct se_frames_update acc = {0};
	CHECK(poll_update(c, &acc, 1000, true, false), "import arrives");
	CHECK(acc.import && acc.import->width == 64 && acc.import->height == 32 && acc.import->fourcc == 0 &&
		      acc.import->buffer_count == 3 && acc.import->strides[0] == shm.stride && acc.import->canvas == SE_CANVAS_TALL,
	      "import geometry");
	uint64_t epoch1 = acc.import ? acc.import->epoch : 0;
	CHECK(epoch1 == 1, "first epoch is 1 (got %llu)", (unsigned long long)epoch1);

	/* frame without fence: pixels visible through the client's read-only mapping */
	memset(shm.maps[0], 0xab, (size_t)shm.stride * 32);
	CHECK(send_frame(fd, SE_CANVAS_TALL, 0, 1, 1, -1), "send frame 1");
	CHECK(poll_update(c, &acc, 1000, false, true), "frame 1 ready");
	CHECK(acc.frame.buffer == 0 && acc.frame.seq == 1 && acc.frame.epoch == epoch1, "frame 1 fields");
	CHECK(acc.import && acc.import->maps[0] && acc.import->maps[0][shm.stride * 31 + 63 * 4] == 0xab, "shm pixels");
	se_frames_client_stats(c, &st);
	CHECK(st.last_frame_ns != 0 && st.frames == 1, "stats after frame 1");
	se_frames_client_release(c, epoch1, 0, 1);
	CHECK(expect_release(fd, 0, 1, 1000), "release of frame 1");

	/* fenced frame is held back until the fence signals */
	int fence[2];
	CHECK(pipe2(fence, O_CLOEXEC) == 0, "pipe");
	CHECK(send_frame(fd, SE_CANVAS_TALL, 1, 2, 1, fence[0]), "send fenced frame 2");
	close(fence[0]);
	acc.has_frame = false;
	CHECK(!poll_update(c, &acc, 150, false, true), "frame 2 must wait for its fence");
	CHECK(write(fence[1], "x", 1) == 1, "signal fence");
	close(fence[1]);
	CHECK(poll_update(c, &acc, 1000, false, true), "frame 2 ready after fence");
	CHECK(acc.frame.buffer == 1 && acc.frame.seq == 2, "frame 2 fields");
	/* consumer keeps holding buffer 1 (displayed) */

	/* a frame still waiting for its fence is superseded by a newer one and released */
	int f3[2];
	CHECK(pipe2(f3, O_CLOEXEC) == 0, "pipe");
	CHECK(send_frame(fd, SE_CANVAS_TALL, 2, 3, 1, f3[0]), "send fenced frame 3");
	close(f3[0]);
	sleep_ms(30);
	CHECK(send_frame(fd, SE_CANVAS_TALL, 0, 4, 1, -1), "send frame 4");
	CHECK(expect_release(fd, 2, 3, 1000), "pending frame 3 released unseen");
	acc.has_frame = false;
	CHECK(poll_update(c, &acc, 1000, false, true), "frame 4 ready");
	CHECK(acc.frame.buffer == 0 && acc.frame.seq == 4, "frame 4 fields");
	close(f3[1]);
	se_frames_client_release(c, epoch1, 1, 2);
	CHECK(expect_release(fd, 1, 2, 1000), "release of frame 2");

	/* ready frame not taken by the consumer is superseded and released */
	CHECK(send_frame(fd, SE_CANVAS_TALL, 1, 5, 1, -1), "send frame 5");
	sleep_ms(50);
	CHECK(send_frame(fd, SE_CANVAS_TALL, 2, 6, 1, -1), "send frame 6");
	CHECK(expect_release(fd, 1, 5, 1000), "unconsumed frame 5 released");
	acc.has_frame = false;
	CHECK(poll_update(c, &acc, 1000, false, true) && acc.frame.seq == 6, "newest frame 6 wins");
	se_frames_client_stats(c, &st);
	CHECK(st.superseded == 2, "superseded count %llu", (unsigned long long)st.superseded);

	/* fence that never signals: dropped and released after fence_timeout_ms */
	int f7[2];
	CHECK(pipe2(f7, O_CLOEXEC) == 0, "pipe");
	CHECK(send_frame(fd, SE_CANVAS_TALL, 1, 7, 1, f7[0]), "send frame 7");
	close(f7[0]);
	CHECK(expect_release(fd, 1, 7, 2000), "timed-out frame 7 released");
	se_frames_client_stats(c, &st);
	CHECK(st.fence_timeouts == 1, "fence timeout counted");
	close(f7[1]);

	/* frames of an older generation are ignored */
	CHECK(send_frame(fd, SE_CANVAS_TALL, 1, 8, 0, -1), "send stale-generation frame");
	acc.has_frame = false;
	CHECK(!poll_update(c, &acc, 100, false, true), "old generation ignored");

	/* canvas re-creation: new import, old buffers are no longer released */
	struct shm_canvas shm2;
	CHECK(make_shm(&shm2, 32, 16, 2), "shm2");
	CHECK(send_canvas_shm(fd, &shm2, SE_CANVAS_TALL, 32, 16, 2), "send canvas gen 2");
	se_frames_import_free(acc.import);
	acc.import = NULL;
	CHECK(poll_update(c, &acc, 1000, true, false), "import gen 2");
	CHECK(acc.import && acc.import->generation == 2 && acc.import->epoch == epoch1 + 1 && acc.import->width == 32,
	      "gen 2 import");
	se_frames_client_release(c, epoch1, 2, 6); /* belongs to the dead generation */
	CHECK(no_message(fd, 150), "no release for a previous generation");
	CHECK(send_frame(fd, SE_CANVAS_TALL, 0, 1, 1, -1), "frame of old generation after re-create");
	CHECK(send_frame(fd, SE_CANVAS_TALL, 1, 9, 2, -1), "frame gen 2");
	acc.has_frame = false;
	CHECK(poll_update(c, &acc, 1000, false, true), "gen 2 frame");
	CHECK(acc.frame.buffer == 1 && acc.frame.seq == 9 && acc.frame.epoch == epoch1 + 1, "gen 2 frame fields");

	/* invalid buffer index is a protocol error: reconnect, fresh hello */
	CHECK(send_frame(fd, SE_CANVAS_TALL, 7, 10, 2, -1), "frame with invalid buffer");
	uint8_t buf[256];
	CHECK(fs_recv(fd, buf, sizeof(buf), 1000) == 0, "client dropped the connection");
	close(fd);
	fd = fs_accept(lfd, 3000);
	CHECK(fd >= 0, "client reconnected");
	CHECK(fs_recv(fd, &hello, sizeof(hello), 1000) == (ssize_t)sizeof(hello) && hello.h.type == SE_MSG_HELLO,
	      "hello after reconnect");
	se_frames_client_stats(c, &st);
	CHECK(st.protocol_errors == 1 && st.connects == 2, "errors %llu connects %llu",
	      (unsigned long long)st.protocol_errors, (unsigned long long)st.connects);

	/* same generation number after reconnect is a new import (new epoch) */
	CHECK(send_canvas_shm(fd, &shm2, SE_CANVAS_TALL, 32, 16, 2), "re-send canvas gen 2");
	se_frames_import_free(acc.import);
	acc.import = NULL;
	CHECK(poll_update(c, &acc, 1000, true, false) && acc.import->epoch == epoch1 + 2, "epoch advances on re-send");

	/* goodbye marks the feed stale-by-shutdown but keeps the import */
	struct se_goodbye bye = {.h = se_hdr_make(SE_MSG_GOODBYE), .reason = SE_GOODBYE_SHUTDOWN, .canvas = SE_CANVAS_ALL};
	CHECK(fs_send(fd, &bye, sizeof(bye), NULL, 0), "goodbye");
	for (int t = 0; t < 1000; t += 5) {
		se_frames_client_stats(c, &st);
		if (st.goodbye)
			break;
		sleep_ms(5);
	}
	CHECK(st.goodbye && st.goodbye_reason == SE_GOODBYE_SHUTDOWN, "goodbye recorded");

	/* bad magic → protocol error + reconnect */
	struct se_goodbye bad = bye;
	bad.h.magic = 0xdeadbeef;
	CHECK(fs_send(fd, &bad, sizeof(bad), NULL, 0), "bad magic");
	CHECK(fs_recv(fd, buf, sizeof(buf), 1000) == 0, "dropped after bad magic");
	close(fd);
	fd = fs_accept(lfd, 3000);
	CHECK(fd >= 0, "reconnect after bad magic");
	CHECK(fs_recv(fd, &hello, sizeof(hello), 1000) == (ssize_t)sizeof(hello), "hello");

	/* engine closes the socket: client reconnects with backoff */
	close(fd);
	fd = fs_accept(lfd, 3000);
	CHECK(fd >= 0, "reconnect after engine restart");
	CHECK(fs_recv(fd, &hello, sizeof(hello), 1000) == (ssize_t)sizeof(hello), "hello");

	/* switching to dmabuf reconnects with the dmabuf flag */
	se_frames_client_set_dmabuf(c, true);
	CHECK(fs_recv(fd, buf, sizeof(buf), 1000) == 0, "client reconnects for new flags");
	close(fd);
	fd = fs_accept(lfd, 3000);
	CHECK(fd >= 0 && fs_recv(fd, &hello, sizeof(hello), 1000) == (ssize_t)sizeof(hello) &&
		      hello.flags == SE_HELLO_FLAG_DMABUF,
	      "hello advertises dmabuf");

	/* an unsupported fourcc makes the client fall back to shm */
	struct se_canvas odd = fs_canvas_msg(SE_CANVAS_TALL, 32, 16, 0x3231564e /* NV12 */, 0, 128, 2, 3);
	CHECK(fs_send(fd, &odd, sizeof(odd), shm2.fds, 2), "send unsupported fourcc");
	CHECK(fs_recv(fd, buf, sizeof(buf), 1000) == 0, "client drops the unsupported canvas");
	close(fd);
	fd = fs_accept(lfd, 3000);
	CHECK(fd >= 0 && fs_recv(fd, &hello, sizeof(hello), 1000) == (ssize_t)sizeof(hello) && hello.flags == 0,
	      "hello falls back to shm");

	/* a supported dmabuf canvas is handed over untouched (fds only, no mappings) */
	se_frames_client_set_dmabuf(c, true);
	CHECK(fs_recv(fd, buf, sizeof(buf), 1000) == 0, "reconnect for dmabuf");
	close(fd);
	fd = fs_accept(lfd, 3000);
	CHECK(fd >= 0 && fs_recv(fd, &hello, sizeof(hello), 1000) == (ssize_t)sizeof(hello), "hello");
	struct se_canvas dm = fs_canvas_msg(SE_CANVAS_TALL, 32, 16, SE_DRM_FORMAT_ABGR8888, 0x0300000000606014ull, 256, 2, 4);
	dm.offsets[0] = 0;
	CHECK(fs_send(fd, &dm, sizeof(dm), shm2.fds, 2), "send dmabuf canvas");
	se_frames_import_free(acc.import);
	acc.import = NULL;
	CHECK(poll_update(c, &acc, 1000, true, false), "dmabuf import");
	CHECK(acc.import && acc.import->fourcc == SE_DRM_FORMAT_ABGR8888 && acc.import->modifier == 0x0300000000606014ull &&
		      acc.import->maps[0] == NULL && acc.import->fds[0] >= 0 && acc.import->fds[1] >= 0,
	      "dmabuf import fields");

	se_frames_import_free(acc.import);
	se_frames_client_stop(c);
	close(fd);
	close(lfd);
	free_shm(&shm);
	free_shm(&shm2);
	unlink(path);
}

static void test_no_server(const char *path)
{
	unlink(path);
	struct se_frames_opts o = {.socket_path = path, .canvas = SE_CANVAS_WIDE, .log = quiet_log};
	struct se_frames_client *c = se_frames_client_start(&o);
	CHECK(c, "start without server");
	sleep_ms(300);
	struct se_frames_stats st;
	se_frames_client_stats(c, &st);
	CHECK(!st.connected && st.connects == 0, "not connected");
	/* the server appears later */
	int lfd = fs_listen(path);
	int fd = fs_accept(lfd, 4000);
	CHECK(fd >= 0, "connects once the engine is up");
	se_frames_client_stop(c); /* must join promptly */
	if (fd >= 0)
		close(fd);
	close(lfd);
	unlink(path);
}

int main(void)
{
	char path[108];
	snprintf(path, sizeof(path), "/tmp/se-test-frames-%d.sock", (int)getpid());
	test_session(path);
	test_no_server(path);
	if (failures) {
		fprintf(stderr, "%d check(s) failed\n", failures);
		return 1;
	}
	printf("frames-client: all checks passed\n");
	return 0;
}
