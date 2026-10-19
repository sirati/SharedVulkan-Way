/* egl-dmabuf — KWin's dma-buf path minus GBM, through Zink over sv-icd
 * (waycompose-design.md §104).
 *
 * KWin allocates its output buffers with GBM, imports each as an EGLImage with
 * EGL_EXT_image_dma_buf_import, renders into it through an FBO and hands the
 * dma-buf to the host. GBM needs a DRM fd this test cannot make without root,
 * so the buffer comes from EGL instead: a GL texture exported with
 * EGL_MESA_image_dma_buf_export (Zink re-creates it as a LINEAR modifier image
 * and calls vkGetMemoryFdKHR — our token), then imported back exactly as KWin
 * imports a GBM buffer: eglCreateImage(EGL_LINUX_DMA_BUF_EXT) with fourcc,
 * offset, pitch and the LINEAR modifier. The import is rendered into, glFinish,
 * and the pixels are read from the section the token names — /dev/shm/... on
 * this machine. Nothing copies them there: the GPU wrote the section.
 *
 * Exit 0 pass, 1 fail, 77 skip (the display offers no dma-buf import, or it is
 * not Zink — which would not be testing sv-icd at all).
 *
 * Build: cc -o egl-dmabuf egl-dmabuf.c -lEGL -lGLESv2
 */
#define _GNU_SOURCE
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define W 256
#define H 128

static int fail(const char *what) {
    fprintf(stderr, "egl-dmabuf: FAIL: %s (EGL 0x%x, GL 0x%x)\n", what, eglGetError(), glGetError());
    return 1;
}

static int has(const char *list, const char *ext) {
    const char *p = list;
    size_t n = strlen(ext);
    while ((p = strstr(p, ext))) {
        if ((p == list || p[-1] == ' ') && (p[n] == ' ' || p[n] == 0)) return 1;
        p += n;
    }
    return 0;
}

/* sv_drm::token's header: magic[8] version u32 hdr_len u32 size u64
 * nonce[16] name_len u32 name. */
static int read_token(int fd, char *name, size_t cap, uint64_t *size) {
    unsigned char b[44 + 256];
    ssize_t n = pread(fd, b, sizeof b, 0);
    if (n < 44 || memcmp(b, "SVDMABF1", 8) != 0) return -1;
    uint32_t len;
    memcpy(size, b + 16, 8);
    memcpy(&len, b + 40, 4);
    if (len >= cap || 44 + (ssize_t)len > n) return -1;
    memcpy(name, b + 44, len);
    name[len] = 0;
    return 0;
}

int main(void) {
    PFNEGLGETPLATFORMDISPLAYEXTPROC getPlatformDisplay =
        (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    EGLDisplay dpy = getPlatformDisplay ? getPlatformDisplay(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL)
                                        : eglGetDisplay(EGL_DEFAULT_DISPLAY);
    EGLint major, minor;
    if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, &major, &minor)) return fail("eglInitialize");
    const char *exts = eglQueryString(dpy, EGL_EXTENSIONS);
    printf("EGL %d.%d, vendor %s\n", major, minor, eglQueryString(dpy, EGL_VENDOR));
    if (!has(exts, "EGL_EXT_image_dma_buf_import") || !has(exts, "EGL_MESA_image_dma_buf_export")) {
        printf("egl-dmabuf: SKIP: no EGL_EXT_image_dma_buf_import / EGL_MESA_image_dma_buf_export\n");
        return 77;
    }
    int with_mods = has(exts, "EGL_EXT_image_dma_buf_import_modifiers");
    eglBindAPI(EGL_OPENGL_ES_API);
    const EGLint cattr[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, cattr);
    if (ctx == EGL_NO_CONTEXT) return fail("eglCreateContext");
    if (!eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) return fail("eglMakeCurrent");
    const char *renderer = (const char *)glGetString(GL_RENDERER);
    printf("renderer : %s\n", renderer);
    if (!strstr(renderer, "zink")) {
        printf("egl-dmabuf: SKIP: not Zink, so not sv-icd\n");
        return 77;
    }

    PFNEGLCREATEIMAGEKHRPROC createImage = (PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
    PFNEGLDESTROYIMAGEKHRPROC destroyImage = (PFNEGLDESTROYIMAGEKHRPROC)eglGetProcAddress("eglDestroyImageKHR");
    PFNEGLEXPORTDMABUFIMAGEQUERYMESAPROC exportQuery =
        (PFNEGLEXPORTDMABUFIMAGEQUERYMESAPROC)eglGetProcAddress("eglExportDMABUFImageQueryMESA");
    PFNEGLEXPORTDMABUFIMAGEMESAPROC exportImage =
        (PFNEGLEXPORTDMABUFIMAGEMESAPROC)eglGetProcAddress("eglExportDMABUFImageMESA");
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC imageTarget =
        (PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    if (!createImage || !exportQuery || !exportImage || !imageTarget) return fail("entry points");

    /* The buffer: a texture, exported (GBM's allocation, in effect). */
    GLuint tex;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, W, H, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    EGLImageKHR src = createImage(dpy, ctx, EGL_GL_TEXTURE_2D_KHR, (EGLClientBuffer)(uintptr_t)tex, NULL);
    if (src == EGL_NO_IMAGE_KHR) return fail("eglCreateImage(GL_TEXTURE_2D)");
    int fourcc = 0, planes = 0;
    EGLuint64KHR modifier = ~0ull;
    if (!exportQuery(dpy, src, &fourcc, &planes, &modifier)) return fail("eglExportDMABUFImageQueryMESA");
    int fds[4] = {-1, -1, -1, -1};
    EGLint strides[4] = {0}, offsets[4] = {0};
    if (!exportImage(dpy, src, fds, strides, offsets)) return fail("eglExportDMABUFImageMESA");
    printf("export   : fourcc %.4s, %d plane(s), modifier %#llx, fd %d, stride %d, offset %d\n", (char *)&fourcc,
           planes, (unsigned long long)modifier, fds[0], strides[0], offsets[0]);
    char name[256];
    uint64_t size = 0;
    if (read_token(fds[0], name, sizeof name, &size) != 0) return fail("the exported fd is not an sv dma-buf token");
    off_t end = lseek(fds[0], 0, SEEK_END);
    printf("token    : %s, %llu bytes (SEEK_END %lld)\n", name, (unsigned long long)size, (long long)end);
    /* A texture Zink re-creates for export is plain LINEAR tiling, which it
     * reports as DRM_FORMAT_MOD_INVALID; GBM's [LINEAR] allocation says 0.
     * Either way the bytes are LINEAR, and the import names LINEAR explicitly,
     * as KWin does (Zink refuses an INVALID import from an unknown driver). */
    if ((modifier != 0 && modifier != 0x00ffffffffffffffull) || planes != 1) return fail("not one LINEAR plane");
    modifier = 0;

    /* The import, as KWin imports a GBM buffer. */
    EGLint attr[32];
    int i = 0;
    attr[i++] = EGL_WIDTH; attr[i++] = W;
    attr[i++] = EGL_HEIGHT; attr[i++] = H;
    attr[i++] = EGL_LINUX_DRM_FOURCC_EXT; attr[i++] = fourcc;
    attr[i++] = EGL_DMA_BUF_PLANE0_FD_EXT; attr[i++] = fds[0];
    attr[i++] = EGL_DMA_BUF_PLANE0_OFFSET_EXT; attr[i++] = offsets[0];
    attr[i++] = EGL_DMA_BUF_PLANE0_PITCH_EXT; attr[i++] = strides[0];
    if (with_mods) {
        attr[i++] = EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT; attr[i++] = (EGLint)(modifier & 0xffffffff);
        attr[i++] = EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT; attr[i++] = (EGLint)(modifier >> 32);
    }
    attr[i++] = EGL_NONE;
    EGLImageKHR dst = createImage(dpy, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, attr);
    if (dst == EGL_NO_IMAGE_KHR) return fail("eglCreateImage(EGL_LINUX_DMA_BUF_EXT)");
    close(fds[0]); /* EGL took its own reference, as for a kernel dma-buf */

    GLuint tex2, fbo;
    glGenTextures(1, &tex2);
    glBindTexture(GL_TEXTURE_2D, tex2);
    imageTarget(GL_TEXTURE_2D, (GLeglImageOES)dst);
    if (glGetError() != GL_NO_ERROR) return fail("glEGLImageTargetTexture2DOES");
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex2, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) return fail("the imported FBO is incomplete");

    /* A frame: the whole buffer one colour, a 32x32 square another. */
    glViewport(0, 0, W, H);
    glClearColor(0.2f, 0.4f, 0.6f, 0.8f);
    glClear(GL_COLOR_BUFFER_BIT);
    glEnable(GL_SCISSOR_TEST);
    glScissor(16, 16, 32, 32);
    glClearColor(1.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glDisable(GL_SCISSOR_TEST);
    glFinish();

    /* The section, by name: what the compositor would map. */
    int sfd = open(name, O_RDONLY);
    if (sfd < 0) return fail("open the section by name");
    unsigned char *px = malloc(size);
    if (pread(sfd, px, size, 0) != (ssize_t)size) return fail("read the section");
    close(sfd);
    /* AB24 is R,G,B,A in memory; AR24/XR24 B,G,R,A. */
    int bgra = fourcc == 0x34325241 /* AR24 */ || fourcc == 0x34325258 /* XR24 */;
    unsigned char bg[4] = {51, 102, 153, 204}, sq[4] = {255, 0, 0, 255};
    if (bgra) {
        bg[0] = 153; bg[2] = 51;
        sq[0] = 0; sq[2] = 255;
    }
    long n_bg = 0, n_sq = 0, n_other = 0;
    for (int y = 0; y < H; y++)
        for (int x = 0; x < W; x++) {
            unsigned char *p = px + offsets[0] + (size_t)y * strides[0] + (size_t)x * 4;
            if (!memcmp(p, bg, 4)) n_bg++;
            else if (!memcmp(p, sq, 4)) n_sq++;
            else n_other++;
        }
    unsigned char *p0 = px + offsets[0];
    printf("section  : %ld background, %ld square, %ld other (first pixel %u,%u,%u,%u)\n", n_bg, n_sq, n_other, p0[0],
           p0[1], p0[2], p0[3]);
    /* And what GL itself reads back, for the same rows. */
    unsigned char *rb = malloc(W * H * 4);
    glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, rb);
    long same = 0;
    for (int y = 0; y < H; y++)
        for (int x = 0; x < W; x++) {
            unsigned char *s = px + offsets[0] + (size_t)y * strides[0] + (size_t)x * 4;
            unsigned char *g = rb + ((size_t)y * W + x) * 4;
            unsigned char c[4] = {s[0], s[1], s[2], s[3]};
            if (bgra) { c[0] = s[2]; c[2] = s[0]; }
            if (!memcmp(c, g, 4)) same++;
        }
    printf("readback : %ld of %d pixels equal to the section's\n", same, W * H);
    destroyImage(dpy, dst);
    destroyImage(dpy, src);
    if (n_sq != 32 * 32 || n_bg != W * H - 32 * 32 || same != W * H) {
        fprintf(stderr, "egl-dmabuf: FAIL: the section does not hold the frame\n");
        return 1;
    }
    printf("egl-dmabuf: PASS — Zink rendered into an imported dma-buf and the pixels are in %s\n", name);
    return 0;
}
