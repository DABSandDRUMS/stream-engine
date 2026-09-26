/* SPDX-License-Identifier: GPL-2.0-or-later
 *
 * se-fake-frames: a stand-in frames.sock server (docs/frames-protocol.md) for bringing up and
 * testing the OBS plugin without the engine's renderer. Serves animated `wide` (1920×1080) and
 * `tall` (1080×1920) canvases at 60 fps to every client. Clients that advertise dmabuf import
 * get GPU buffers (GBM on the NVIDIA render node by default) rendered with OpenGL ES through
 * EGL, each frame with a real sync_file fence (EGL_ANDROID_native_fence_sync); `--linear`
 * serves CPU-written linear GBM buffers instead (the NVIDIA GBM backend maps only buffers
 * allocated with GBM_BO_USE_LINEAR alone, and NVIDIA's EGL cannot import those — useful for
 * exercising the plugin's dmabuf → shm fallback). Other clients get memfds.
 *
 *   se-fake-frames [--socket PATH] [--fps N] [--shm] [--linear] [--render-node DEV]
 *                  [--fake-fences] [--buffers N]
 *
 * `--fake-fences` attaches an already-signaled pipe as the fence of CPU-written frames.
 *
 * Signals: SIGUSR1 toggles pausing frames (feed goes stale), SIGUSR2 sends goodbye (reason 1)
 * and pauses, SIGINT/SIGTERM send goodbye and exit.
 */
#define _GNU_SOURCE
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <gbm.h>
#include <limits.h>
#include <math.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#include "fake-server.h"

#define MAX_CLIENTS 16
#define MAX_BUF 8

static volatile sig_atomic_t g_pause_toggle, g_goodbye, g_quit;

static void on_signal(int sig)
{
	if (sig == SIGUSR1)
		g_pause_toggle = 1;
	else if (sig == SIGUSR2)
		g_goodbye = 1;
	else
		g_quit = 1;
}

static uint64_t now_ns(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

struct canvas_state {
	bool active;
	uint32_t w, h, stride, count, generation;
	uint64_t modifier;
	uint32_t offset;
	bool dmabuf;
	int fds[MAX_BUF];
	uint8_t *maps[MAX_BUF]; /* shm */
	struct gbm_bo *bos[MAX_BUF];
	bool gpu; /* rendered with GL into the dmabuf */
	EGLImageKHR imgs[MAX_BUF];
	GLuint rbs[MAX_BUF], fbos[MAX_BUF];
	bool held[MAX_BUF];
	int last_sent;
	uint64_t seq, skipped;
};

struct client {
	int fd;
	uint32_t want;
	bool dmabuf;
	struct canvas_state cv[2];
};

static struct {
	const char *render_node;
	struct gbm_device *gbm;
	int gbm_fd;
	bool force_shm;
	bool linear;
	bool fake_fences;
	EGLDisplay dpy;
	EGLContext ctx;
	PFNEGLCREATEIMAGEKHRPROC create_image;
	PFNEGLDESTROYIMAGEKHRPROC destroy_image;
	PFNEGLCREATESYNCKHRPROC create_sync;
	PFNEGLDESTROYSYNCKHRPROC destroy_sync;
	PFNEGLDUPNATIVEFENCEFDANDROIDPROC dup_fence;
	PFNGLEGLIMAGETARGETRENDERBUFFERSTORAGEOESPROC image_rb;
	uint32_t buffers;
	uint32_t generation;
} G = {.render_node = NULL, .gbm_fd = -1, .buffers = 4, .dpy = EGL_NO_DISPLAY, .ctx = EGL_NO_CONTEXT};

/* Surfaceless GLES2 context on the GBM device, for rendering into the canvas dmabufs. */
static bool init_egl(void)
{
	PFNEGLGETPLATFORMDISPLAYEXTPROC get_display = (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
	if (!get_display)
		return false;
	G.dpy = get_display(EGL_PLATFORM_GBM_KHR, G.gbm, NULL);
	if (G.dpy == EGL_NO_DISPLAY || !eglInitialize(G.dpy, NULL, NULL))
		return false;
	const char *ext = eglQueryString(G.dpy, EGL_EXTENSIONS);
	if (!ext || !strstr(ext, "EGL_EXT_image_dma_buf_import_modifiers") || !strstr(ext, "EGL_ANDROID_native_fence_sync") ||
	    !strstr(ext, "EGL_KHR_surfaceless_context") || !strstr(ext, "EGL_KHR_no_config_context")) {
		fprintf(stderr, "EGL lacks dmabuf import/native fences/surfaceless contexts\n");
		return false;
	}
	G.create_image = (PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
	G.destroy_image = (PFNEGLDESTROYIMAGEKHRPROC)eglGetProcAddress("eglDestroyImageKHR");
	G.create_sync = (PFNEGLCREATESYNCKHRPROC)eglGetProcAddress("eglCreateSyncKHR");
	G.destroy_sync = (PFNEGLDESTROYSYNCKHRPROC)eglGetProcAddress("eglDestroySyncKHR");
	G.dup_fence = (PFNEGLDUPNATIVEFENCEFDANDROIDPROC)eglGetProcAddress("eglDupNativeFenceFDANDROID");
	G.image_rb = (PFNGLEGLIMAGETARGETRENDERBUFFERSTORAGEOESPROC)eglGetProcAddress("glEGLImageTargetRenderbufferStorageOES");
	if (!G.create_image || !G.destroy_image || !G.create_sync || !G.destroy_sync || !G.dup_fence || !G.image_rb)
		return false;
	eglBindAPI(EGL_OPENGL_ES_API);
	const EGLint attrs[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
	G.ctx = eglCreateContext(G.dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, attrs);
	if (G.ctx == EGL_NO_CONTEXT || !eglMakeCurrent(G.dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, G.ctx))
		return false;
	fprintf(stderr, "GPU rendering: %s\n", (const char *)glGetString(GL_RENDERER));
	return true;
}

static const char *detect_nvidia_node(char *buf, size_t len)
{
	DIR *d = opendir("/sys/class/drm");
	if (!d)
		return NULL;
	struct dirent *e;
	const char *found = NULL;
	while ((e = readdir(d))) {
		if (strncmp(e->d_name, "renderD", 7) != 0)
			continue;
		char link[PATH_MAX], target[PATH_MAX];
		snprintf(link, sizeof(link), "/sys/class/drm/%s/device/driver", e->d_name);
		ssize_t n = readlink(link, target, sizeof(target) - 1);
		if (n <= 0)
			continue;
		target[n] = 0;
		if (!found || strstr(target, "nvidia")) {
			snprintf(buf, len, "/dev/dri/%s", e->d_name);
			found = buf;
			if (strstr(target, "nvidia"))
				break;
		}
	}
	closedir(d);
	return found;
}

static void free_canvas(struct canvas_state *c)
{
	for (uint32_t i = 0; i < c->count; i++) {
		if (c->fbos[i])
			glDeleteFramebuffers(1, &c->fbos[i]);
		if (c->rbs[i])
			glDeleteRenderbuffers(1, &c->rbs[i]);
		if (c->imgs[i])
			G.destroy_image(G.dpy, c->imgs[i]);
		if (c->maps[i])
			munmap(c->maps[i], (size_t)c->stride * c->h);
		if (c->fds[i] >= 0)
			close(c->fds[i]);
		if (c->bos[i])
			gbm_bo_destroy(c->bos[i]);
	}
	memset(c, 0, sizeof(*c));
}

/* GPU-local (tiled) buffers rendered through GL: what a real renderer exports. */
static bool alloc_gpu(struct canvas_state *c)
{
	for (uint32_t i = 0; i < c->count; i++) {
		c->bos[i] = gbm_bo_create(G.gbm, c->w, c->h, GBM_FORMAT_ABGR8888, GBM_BO_USE_RENDERING);
		if (!c->bos[i])
			return false;
		c->fds[i] = gbm_bo_get_fd(c->bos[i]);
		if (c->fds[i] < 0)
			return false;
		uint64_t mod = gbm_bo_get_modifier(c->bos[i]);
		const EGLint attrs[] = {
			EGL_WIDTH, (EGLint)c->w, EGL_HEIGHT, (EGLint)c->h, EGL_LINUX_DRM_FOURCC_EXT, (EGLint)GBM_FORMAT_ABGR8888,
			EGL_DMA_BUF_PLANE0_FD_EXT, c->fds[i], EGL_DMA_BUF_PLANE0_OFFSET_EXT, (EGLint)gbm_bo_get_offset(c->bos[i], 0),
			EGL_DMA_BUF_PLANE0_PITCH_EXT, (EGLint)gbm_bo_get_stride(c->bos[i]),
			EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, (EGLint)(mod & 0xffffffffu),
			EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, (EGLint)(mod >> 32), EGL_NONE};
		c->imgs[i] = G.create_image(G.dpy, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, attrs);
		if (!c->imgs[i]) {
			fprintf(stderr, "eglCreateImage failed: 0x%x\n", eglGetError());
			return false;
		}
		glGenRenderbuffers(1, &c->rbs[i]);
		glBindRenderbuffer(GL_RENDERBUFFER, c->rbs[i]);
		G.image_rb(GL_RENDERBUFFER, c->imgs[i]);
		glGenFramebuffers(1, &c->fbos[i]);
		glBindFramebuffer(GL_FRAMEBUFFER, c->fbos[i]);
		glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, c->rbs[i]);
		if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
			fprintf(stderr, "framebuffer incomplete\n");
			return false;
		}
	}
	c->stride = gbm_bo_get_stride(c->bos[0]);
	c->offset = gbm_bo_get_offset(c->bos[0], 0);
	c->modifier = gbm_bo_get_modifier(c->bos[0]);
	c->dmabuf = true;
	c->gpu = true;
	return true;
}

static bool alloc_dmabuf(struct canvas_state *c)
{
	if (!G.gbm)
		return false;
	if (!G.linear)
		return G.ctx != EGL_NO_CONTEXT && alloc_gpu(c);
	for (uint32_t i = 0; i < c->count; i++) {
		c->bos[i] = gbm_bo_create(G.gbm, c->w, c->h, GBM_FORMAT_ABGR8888, GBM_BO_USE_LINEAR);
		if (!c->bos[i]) {
			fprintf(stderr, "gbm_bo_create %ux%u failed: %s\n", c->w, c->h, strerror(errno));
			return false;
		}
		c->fds[i] = gbm_bo_get_fd(c->bos[i]);
		if (c->fds[i] < 0)
			return false;
	}
	c->stride = gbm_bo_get_stride(c->bos[0]);
	c->offset = gbm_bo_get_offset(c->bos[0], 0);
	c->modifier = gbm_bo_get_modifier(c->bos[0]);
	/* probe CPU mapping once */
	uint32_t stride;
	void *data = NULL;
	void *p = gbm_bo_map(c->bos[0], 0, 0, c->w, c->h, GBM_BO_TRANSFER_WRITE, &stride, &data);
	if (!p) {
		fprintf(stderr, "gbm_bo_map failed; serving shm instead\n");
		return false;
	}
	gbm_bo_unmap(c->bos[0], data);
	c->dmabuf = true;
	return true;
}

static bool alloc_shm(struct canvas_state *c)
{
	c->stride = c->w * 4;
	c->offset = 0;
	c->modifier = 0;
	c->dmabuf = false;
	for (uint32_t i = 0; i < c->count; i++) {
		c->fds[i] = fs_memfd("se-fake-frames", (size_t)c->stride * c->h);
		if (c->fds[i] < 0)
			return false;
		c->maps[i] = mmap(NULL, (size_t)c->stride * c->h, PROT_READ | PROT_WRITE, MAP_SHARED, c->fds[i], 0);
		if (c->maps[i] == MAP_FAILED) {
			c->maps[i] = NULL;
			return false;
		}
	}
	return true;
}

static bool setup_canvas(struct client *cl, uint32_t canvas)
{
	struct canvas_state *c = &cl->cv[canvas];
	free_canvas(c);
	for (uint32_t i = 0; i < MAX_BUF; i++)
		c->fds[i] = -1;
	c->w = canvas == SE_CANVAS_TALL ? 1080 : 1920;
	c->h = canvas == SE_CANVAS_TALL ? 1920 : 1080;
	c->count = G.buffers;
	c->generation = ++G.generation;
	c->last_sent = -1;
	bool ok = false;
	if (cl->dmabuf && !G.force_shm) {
		ok = alloc_dmabuf(c);
		if (!ok) {
			free_canvas(c);
			for (uint32_t i = 0; i < MAX_BUF; i++)
				c->fds[i] = -1;
			c->w = canvas == SE_CANVAS_TALL ? 1080 : 1920;
			c->h = canvas == SE_CANVAS_TALL ? 1920 : 1080;
			c->count = G.buffers;
			c->generation = G.generation;
			c->last_sent = -1;
		}
	}
	if (!ok)
		ok = alloc_shm(c);
	if (!ok)
		return false;
	struct se_canvas m = fs_canvas_msg(canvas, c->w, c->h, c->dmabuf ? SE_DRM_FORMAT_ABGR8888 : 0, c->modifier, c->stride,
					   c->count, c->generation);
	m.offsets[0] = c->offset;
	c->active = fs_send(cl->fd, &m, sizeof(m), c->fds, c->count);
	fprintf(stderr, "client %d: canvas %s %ux%u %s stride %u modifier 0x%llx gen %u\n", cl->fd, se_canvas_name(canvas),
		c->w, c->h, c->dmabuf ? "dmabuf" : "shm", c->stride, (unsigned long long)c->modifier, c->generation);
	return c->active;
}

/* RGBA8 test pattern: hue-cycling background, moving bar, canvas marker, frame counter bits. */
static void draw(uint8_t *px, uint32_t stride, uint32_t w, uint32_t h, uint32_t canvas, uint64_t seq)
{
	double t = (double)seq / 60.0;
	uint8_t bg[4] = {(uint8_t)(40 + 30 * sin(t)), (uint8_t)(40 + 30 * sin(t + 2.1)), (uint8_t)(60 + 30 * sin(t + 4.2)), 255};
	const int bar_w = (int)w / 16;
	const int bar_x = (int)((seq * 8) % (w + (uint32_t)bar_w)) - bar_w;
	const uint32_t bar_lo = bar_x < 0 ? 0 : (uint32_t)bar_x;
	const uint32_t bar_hi = bar_x + bar_w > (int)w ? w : (uint32_t)(bar_x + bar_w);
	uint32_t mark = canvas == SE_CANVAS_TALL ? 0xff2090ffu : 0xffff9020u; /* RGBA bytes: tall orange, wide blue */
	for (uint32_t y = 0; y < h; y++) {
		uint32_t *row = (uint32_t *)(px + (size_t)y * stride);
		uint32_t bgv;
		memcpy(&bgv, bg, 4);
		for (uint32_t x = 0; x < w; x++)
			row[x] = bgv;
		for (uint32_t x = bar_lo; x < bar_hi; x++)
			row[x] = 0xffffffffu;
		if (y < h / 8)
			for (uint32_t x = 0; x < w / 8; x++)
				row[x] = mark;
		/* 32 frame-counter bits along the bottom */
		if (y > h - 40 && y < h - 8)
			for (uint32_t b = 0; b < 32; b++) {
				uint32_t v = (seq >> b) & 1 ? 0xff00ff00u : 0xff000000u;
				for (uint32_t x = 8 + b * 40; x < 8 + b * 40 + 32 && x < w; x++)
					row[x] = v;
			}
	}
}

static void clear_rect(uint32_t x, uint32_t y, uint32_t w, uint32_t h, uint32_t rgba)
{
	/* memory row 0 is GL row 0 for an FBO over a dmabuf, so no flip */
	glScissor((GLint)x, (GLint)y, (GLsizei)w, (GLsizei)h);
	glClearColor((float)(rgba & 0xff) / 255.f, (float)((rgba >> 8) & 0xff) / 255.f, (float)((rgba >> 16) & 0xff) / 255.f,
		     (float)(rgba >> 24) / 255.f);
	glClear(GL_COLOR_BUFFER_BIT);
}

/* Same pattern as draw(), with scissored clears on the GPU. Returns a sync_file fd. */
static int draw_gpu(struct canvas_state *c, int b, uint32_t canvas, uint64_t seq)
{
	const uint32_t w = c->w, h = c->h;
	double t = (double)seq / 60.0;
	uint32_t bg = 0xff000000u | (uint32_t)(40 + 30 * sin(t)) | (uint32_t)(40 + 30 * sin(t + 2.1)) << 8 |
		      (uint32_t)(60 + 30 * sin(t + 4.2)) << 16;
	glBindFramebuffer(GL_FRAMEBUFFER, c->fbos[b]);
	glViewport(0, 0, (GLsizei)w, (GLsizei)h);
	glEnable(GL_SCISSOR_TEST);
	clear_rect(0, 0, w, h, bg);
	const int bar_w = (int)w / 16;
	const int bar_x = (int)((seq * 8) % (w + (uint32_t)bar_w)) - bar_w;
	const uint32_t lo = bar_x < 0 ? 0 : (uint32_t)bar_x, hi = bar_x + bar_w > (int)w ? w : (uint32_t)(bar_x + bar_w);
	if (hi > lo)
		clear_rect(lo, 0, hi - lo, h, 0xffffffffu);
	clear_rect(0, 0, w / 8, h / 8, canvas == SE_CANVAS_TALL ? 0xff2090ffu : 0xffff9020u);
	for (uint32_t bit = 0; bit < 32 && 8 + bit * 40 + 32 <= w; bit++)
		clear_rect(8 + bit * 40, h - 39, 32, 31, (seq >> bit) & 1 ? 0xff00ff00u : 0xff000000u);
	glDisable(GL_SCISSOR_TEST);
	const EGLint attrs[] = {EGL_SYNC_NATIVE_FENCE_FD_ANDROID, EGL_NO_NATIVE_FENCE_FD_ANDROID, EGL_NONE};
	EGLSyncKHR sync = G.create_sync(G.dpy, EGL_SYNC_NATIVE_FENCE_ANDROID, attrs);
	glFlush();
	int fd = sync != EGL_NO_SYNC_KHR ? G.dup_fence(G.dpy, sync) : -1;
	if (sync != EGL_NO_SYNC_KHR)
		G.destroy_sync(G.dpy, sync);
	if (fd < 0)
		glFinish(); /* no fence: make the frame complete before announcing it */
	return fd;
}

static void send_frame(struct client *cl, uint32_t canvas)
{
	struct canvas_state *c = &cl->cv[canvas];
	int b = -1;
	for (uint32_t i = 0; i < c->count; i++) {
		uint32_t k = (uint32_t)(c->last_sent + 1 + (int)i) % c->count;
		if (!c->held[k] && (int)k != c->last_sent) {
			b = (int)k;
			break;
		}
	}
	if (b < 0) {
		c->skipped++;
		return;
	}
	c->seq++;
	if (c->gpu) {
		int fence = draw_gpu(c, b, canvas, c->seq);
		struct se_frame f = fs_frame_msg(canvas, (uint32_t)b, c->seq, c->generation, fence >= 0);
		bool ok = fs_send(cl->fd, &f, sizeof(f), fence >= 0 ? &fence : NULL, fence >= 0 ? 1 : 0);
		if (fence >= 0)
			close(fence);
		if (ok) {
			c->held[b] = true;
			c->last_sent = b;
		}
		return;
	}
	if (c->dmabuf) {
		uint32_t stride;
		void *data = NULL;
		uint8_t *p = gbm_bo_map(c->bos[b], 0, 0, c->w, c->h, GBM_BO_TRANSFER_WRITE, &stride, &data);
		if (!p)
			return;
		draw(p, stride, c->w, c->h, canvas, c->seq);
		gbm_bo_unmap(c->bos[b], data);
	} else {
		draw(c->maps[b], c->stride, c->w, c->h, canvas, c->seq);
	}
	struct se_frame f = fs_frame_msg(canvas, (uint32_t)b, c->seq, c->generation, G.fake_fences);
	int pipefd[2] = {-1, -1};
	if (G.fake_fences && pipe2(pipefd, O_CLOEXEC) == 0) {
		/* a pipe signals like a sync_file (POLLIN); ours is already "done" */
		if (write(pipefd[1], "x", 1) != 1)
			f.has_fence = 0;
		close(pipefd[1]);
	} else {
		f.has_fence = 0;
	}
	bool ok = fs_send(cl->fd, &f, sizeof(f), f.has_fence ? &pipefd[0] : NULL, f.has_fence ? 1 : 0);
	if (pipefd[0] >= 0)
		close(pipefd[0]);
	if (ok) {
		c->held[b] = true;
		c->last_sent = b;
	}
}

static void drop_client(struct client *cl)
{
	fprintf(stderr, "client %d: gone\n", cl->fd);
	close(cl->fd);
	free_canvas(&cl->cv[0]);
	free_canvas(&cl->cv[1]);
	cl->fd = -1;
}

static void handle_client(struct client *cl)
{
	uint8_t buf[256];
	ssize_t n = fs_recv(cl->fd, buf, sizeof(buf), 0);
	if (n <= 0) {
		drop_client(cl);
		return;
	}
	const struct se_hdr *h = (const struct se_hdr *)buf;
	if ((size_t)n < sizeof(*h) || h->magic != SE_MAGIC)
		return;
	if (h->type == SE_MSG_HELLO && (size_t)n >= sizeof(struct se_hello)) {
		const struct se_hello *m = (const struct se_hello *)buf;
		cl->want = m->want;
		cl->dmabuf = m->flags & SE_HELLO_FLAG_DMABUF;
		fprintf(stderr, "client %d: hello kind %u want 0x%x %s\n", cl->fd, m->client, m->want, cl->dmabuf ? "dmabuf" : "shm");
		for (uint32_t k = 0; k < 2; k++)
			if (cl->want & (1u << k))
				setup_canvas(cl, k);
	} else if (h->type == SE_MSG_RELEASE && (size_t)n >= sizeof(struct se_release)) {
		const struct se_release *r = (const struct se_release *)buf;
		if (r->canvas < 2 && r->buffer < cl->cv[r->canvas].count)
			cl->cv[r->canvas].held[r->buffer] = false;
	}
}

static void goodbye_all(struct client *clients)
{
	struct se_goodbye g = {.h = se_hdr_make(SE_MSG_GOODBYE), .reason = SE_GOODBYE_SHUTDOWN, .canvas = SE_CANVAS_ALL};
	for (int i = 0; i < MAX_CLIENTS; i++)
		if (clients[i].fd >= 0)
			fs_send(clients[i].fd, &g, sizeof(g), NULL, 0);
}

int main(int argc, char **argv)
{
	char path[108] = "";
	int fps = 60;
	for (int i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "--socket") && i + 1 < argc)
			snprintf(path, sizeof(path), "%s", argv[++i]);
		else if (!strcmp(argv[i], "--fps") && i + 1 < argc)
			fps = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--shm"))
			G.force_shm = true;
		else if (!strcmp(argv[i], "--linear"))
			G.linear = true;
		else if (!strcmp(argv[i], "--fake-fences"))
			G.fake_fences = true;
		else if (!strcmp(argv[i], "--render-node") && i + 1 < argc)
			G.render_node = argv[++i];
		else if (!strcmp(argv[i], "--buffers") && i + 1 < argc)
			G.buffers = (uint32_t)atoi(argv[++i]);
		else {
			fprintf(stderr,
				"usage: %s [--socket PATH] [--fps N] [--shm] [--linear] [--fake-fences] [--render-node DEV] [--buffers N]\n",
				argv[0]);
			return 2;
		}
	}
	if (G.buffers < 2 || G.buffers > MAX_BUF || fps < 1 || fps > 240) {
		fprintf(stderr, "invalid --buffers or --fps\n");
		return 2;
	}
	if (!path[0]) {
		const char *dir = getenv("SE_RUNTIME_DIR");
		const char *xdg = getenv("XDG_RUNTIME_DIR");
		if (dir && *dir)
			snprintf(path, sizeof(path), "%s/frames.sock", dir);
		else
			snprintf(path, sizeof(path), "%s/stream-engine/frames.sock", xdg ? xdg : "/tmp");
		char d[108];
		snprintf(d, sizeof(d), "%s", path);
		*strrchr(d, '/') = 0;
		mkdir(d, 0700);
	}
	char node[PATH_MAX];
	if (!G.force_shm) {
		const char *dev = G.render_node ? G.render_node : detect_nvidia_node(node, sizeof(node));
		if (dev && (G.gbm_fd = open(dev, O_RDWR | O_CLOEXEC)) >= 0)
			G.gbm = gbm_create_device(G.gbm_fd);
		fprintf(stderr, "dmabuf: %s (%s)\n", G.gbm ? "GBM" : "unavailable", dev ? dev : "no render node");
		if (G.gbm && !G.linear && !init_egl())
			fprintf(stderr, "GPU rendering unavailable; dmabuf clients get shm\n");
	}

	int lfd = fs_listen(path);
	if (lfd < 0) {
		fprintf(stderr, "cannot listen on %s: %s\n", path, strerror(errno));
		return 1;
	}
	fprintf(stderr, "serving frames on %s at %d fps\n", path, fps);
	struct sigaction sa = {.sa_handler = on_signal};
	sigaction(SIGUSR1, &sa, NULL);
	sigaction(SIGUSR2, &sa, NULL);
	sigaction(SIGINT, &sa, NULL);
	sigaction(SIGTERM, &sa, NULL);
	signal(SIGPIPE, SIG_IGN);

	struct client clients[MAX_CLIENTS];
	for (int i = 0; i < MAX_CLIENTS; i++)
		clients[i].fd = -1;
	const uint64_t period = 1000000000ull / (uint64_t)fps;
	uint64_t next = now_ns();
	uint64_t next_report = next + 10000000000ull;
	bool paused = false;

	while (!g_quit) {
		if (g_pause_toggle) {
			g_pause_toggle = 0;
			paused = !paused;
			fprintf(stderr, "%s\n", paused ? "paused (feed stale)" : "resumed");
		}
		if (g_goodbye) {
			g_goodbye = 0;
			goodbye_all(clients);
			paused = true;
			fprintf(stderr, "goodbye sent; paused\n");
		}
		uint64_t now = now_ns();
		if (now >= next_report) {
			/* exports skipped because every buffer was held = a client not releasing */
			for (int i = 0; i < MAX_CLIENTS; i++)
				for (uint32_t k = 0; k < 2 && clients[i].fd >= 0; k++)
					if (clients[i].cv[k].active) {
						uint32_t held = 0;
						for (uint32_t b = 0; b < clients[i].cv[k].count; b++)
							held += clients[i].cv[k].held[b];
						fprintf(stderr, "client %d %s: seq %llu, skipped %llu, held now %u/%u\n", clients[i].fd,
							se_canvas_name(k), (unsigned long long)clients[i].cv[k].seq,
							(unsigned long long)clients[i].cv[k].skipped, held, clients[i].cv[k].count);
					}
			next_report = now + 10000000000ull;
		}
		if (now >= next) {
			if (!paused)
				for (int i = 0; i < MAX_CLIENTS; i++)
					for (uint32_t k = 0; k < 2 && clients[i].fd >= 0; k++)
						if (clients[i].cv[k].active)
							send_frame(&clients[i], k);
			next += period;
			if (next < now)
				next = now + period;
		}
		struct pollfd pfd[MAX_CLIENTS + 1];
		int map[MAX_CLIENTS + 1];
		int n = 0;
		pfd[n] = (struct pollfd){.fd = lfd, .events = POLLIN};
		map[n++] = -1;
		for (int i = 0; i < MAX_CLIENTS; i++)
			if (clients[i].fd >= 0) {
				pfd[n] = (struct pollfd){.fd = clients[i].fd, .events = POLLIN};
				map[n++] = i;
			}
		now = now_ns();
		int timeout = next > now ? (int)((next - now) / 1000000ull) : 0;
		if (poll(pfd, (nfds_t)n, timeout) <= 0)
			continue;
		for (int j = 0; j < n; j++) {
			if (!(pfd[j].revents & (POLLIN | POLLHUP | POLLERR)))
				continue;
			if (map[j] < 0) {
				int fd = fs_accept(lfd, 0);
				int slot = -1;
				for (int i = 0; i < MAX_CLIENTS && fd >= 0; i++)
					if (clients[i].fd < 0) {
						slot = i;
						break;
					}
				if (slot < 0) {
					if (fd >= 0)
						close(fd);
					continue;
				}
				memset(&clients[slot], 0, sizeof(clients[slot]));
				clients[slot].fd = fd;
				fprintf(stderr, "client %d: connected\n", fd);
			} else {
				handle_client(&clients[map[j]]);
			}
		}
	}
	goodbye_all(clients);
	for (int i = 0; i < MAX_CLIENTS; i++)
		if (clients[i].fd >= 0)
			drop_client(&clients[i]);
	close(lfd);
	unlink(path);
	if (G.ctx != EGL_NO_CONTEXT) {
		eglMakeCurrent(G.dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
		eglDestroyContext(G.dpy, G.ctx);
	}
	if (G.dpy != EGL_NO_DISPLAY)
		eglTerminate(G.dpy);
	if (G.gbm)
		gbm_device_destroy(G.gbm);
	return 0;
}
