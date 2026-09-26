/* SPDX-License-Identifier: GPL-2.0-or-later
 *
 * GPU-completion fences for the render thread. After drawing a dmabuf texture the plugin
 * inserts a GL sync object; the buffer is released to the engine only once the GPU finished
 * every command that sampled it (glClientWaitSync with a zero timeout never blocks). OBS on
 * Linux renders through EGL + OpenGL, so the entry points come from eglGetProcAddress.
 */
#include "plugin.h"

#include <EGL/egl.h>

typedef struct __GLsync *GLsync_;
typedef GLsync_ (*fence_sync_fn)(unsigned int condition, unsigned int flags);
typedef unsigned int (*client_wait_sync_fn)(GLsync_ sync, unsigned int flags, uint64_t timeout);
typedef void (*delete_sync_fn)(GLsync_ sync);

#define GL_SYNC_GPU_COMMANDS_COMPLETE_ 0x9117
#define GL_SYNC_FLUSH_COMMANDS_BIT_ 0x00000001
#define GL_ALREADY_SIGNALED_ 0x911A
#define GL_CONDITION_SATISFIED_ 0x911C
#define GL_WAIT_FAILED_ 0x911D

static fence_sync_fn fence_sync;
static client_wait_sync_fn client_wait_sync;
static delete_sync_fn delete_sync;
static pthread_once_t once = PTHREAD_ONCE_INIT;

static void load(void)
{
	fence_sync = (fence_sync_fn)eglGetProcAddress("glFenceSync");
	client_wait_sync = (client_wait_sync_fn)eglGetProcAddress("glClientWaitSync");
	delete_sync = (delete_sync_fn)eglGetProcAddress("glDeleteSync");
	if (!fence_sync || !client_wait_sync || !delete_sync) {
		fence_sync = NULL;
		blog(LOG_WARNING, "[stream-engine] GL sync objects unavailable; releasing buffers two frames after use");
	}
}

bool se_gl_fence_available(void)
{
	pthread_once(&once, load);
	return fence_sync != NULL;
}

void *se_gl_fence_create(void)
{
	if (!se_gl_fence_available())
		return NULL;
	return fence_sync(GL_SYNC_GPU_COMMANDS_COMPLETE_, 0);
}

bool se_gl_fence_signaled(void *fence)
{
	if (!fence || !fence_sync)
		return true;
	unsigned int r = client_wait_sync((GLsync_)fence, GL_SYNC_FLUSH_COMMANDS_BIT_, 0);
	return r == GL_ALREADY_SIGNALED_ || r == GL_CONDITION_SATISFIED_ || r == GL_WAIT_FAILED_;
}

void se_gl_fence_destroy(void *fence)
{
	if (fence && delete_sync)
		delete_sync((GLsync_)fence);
}
