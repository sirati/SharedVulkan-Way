/* gl-probe — a real OpenGL program, rendered through Zink over shared_vulkan.
 *
 * WHY THIS EXISTS AND NOT glxgears
 *
 * glxgears is a GLX program and GLX needs a window system. This machine is
 * headless, and Xvfb's GLX has no DRI3 — so a Zink screen cannot be created on
 * it, because Zink's window-system path (`zink_kopper`) wants a real swapchain.
 * design.md §10 puts presentation outside the protocol on purpose and §13 step 5
 * targets headless first, so a windowed GL test would be measuring the one thing
 * this stage deliberately does not implement.
 *
 * What it does instead is EGL's surfaceless platform and an FBO, which removes
 * the window system and *keeps everything else*: this draws an indexed,
 * depth-tested, textured, lit mesh through a real graphics pipeline and then
 * reads the pixels back. Every family design.md §11 names as Zink's requirement
 * set is exercised — buffers, images, samplers, descriptor sets, a render pass,
 * a graphics pipeline, synchronisation and a transfer — and the result is
 * checked rather than assumed.
 *
 * WHAT IT CHECKS, and why "it ran" is not enough
 *
 * A forwarder that silently drops every draw call produces a clean run, a
 * plausible frame time and a uniformly cleared framebuffer. So the test is on
 * the pixels:
 *
 *   - the mesh must cover between 5% and 95% of the frame — a black frame and a
 *     frame filled by one huge triangle both fail;
 *   - there must be many distinct colours, which is the lighting and the texture
 *     actually being evaluated rather than a flat fill;
 *   - the depth test must have done something: the far half of the mesh is drawn
 *     *after* the near half and in a different colour, so if depth is ignored
 *     the near half is overpainted and the check fires.
 *
 * Build: cc -o gl-probe gl-probe.c -lEGL -lGL -lm
 */
#define _GNU_SOURCE
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GL/gl.h>
#include <math.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* GL entry points resolved through EGL rather than linked against,
 * because the system libGL header is whatever version it is. */
#define GLPROC(t, n) static t n
typedef void (*PFNV)(void);
typedef void(APIENTRY *P_glGenBuffers)(GLsizei, GLuint *);
typedef void(APIENTRY *P_glBindBuffer)(GLenum, GLuint);
typedef void(APIENTRY *P_glBufferData)(GLenum, ptrdiff_t, const void *, GLenum);
typedef void(APIENTRY *P_glGenVertexArrays)(GLsizei, GLuint *);
typedef void(APIENTRY *P_glBindVertexArray)(GLuint);
typedef void(APIENTRY *P_glEnableVertexAttribArray)(GLuint);
typedef void(APIENTRY *P_glVertexAttribPointer)(GLuint, GLint, GLenum, GLboolean, GLsizei,
                                                const void *);
typedef GLuint(APIENTRY *P_glCreateShader)(GLenum);
typedef void(APIENTRY *P_glShaderSource)(GLuint, GLsizei, const char *const *, const GLint *);
typedef void(APIENTRY *P_glCompileShader)(GLuint);
typedef void(APIENTRY *P_glGetShaderiv)(GLuint, GLenum, GLint *);
typedef void(APIENTRY *P_glGetShaderInfoLog)(GLuint, GLsizei, GLsizei *, char *);
typedef GLuint(APIENTRY *P_glCreateProgram)(void);
typedef void(APIENTRY *P_glAttachShader)(GLuint, GLuint);
typedef void(APIENTRY *P_glLinkProgram)(GLuint);
typedef void(APIENTRY *P_glGetProgramiv)(GLuint, GLenum, GLint *);
typedef void(APIENTRY *P_glGetProgramInfoLog)(GLuint, GLsizei, GLsizei *, char *);
typedef void(APIENTRY *P_glUseProgram)(GLuint);
typedef GLint(APIENTRY *P_glGetUniformLocation)(GLuint, const char *);
typedef void(APIENTRY *P_glUniformMatrix4fv)(GLint, GLsizei, GLboolean, const GLfloat *);
typedef void(APIENTRY *P_glUniform3f)(GLint, GLfloat, GLfloat, GLfloat);
typedef void(APIENTRY *P_glUniform1i)(GLint, GLint);
typedef void(APIENTRY *P_glGenFramebuffers)(GLsizei, GLuint *);
typedef void(APIENTRY *P_glBindFramebuffer)(GLenum, GLuint);
typedef void(APIENTRY *P_glGenRenderbuffers)(GLsizei, GLuint *);
typedef void(APIENTRY *P_glBindRenderbuffer)(GLenum, GLuint);
typedef void(APIENTRY *P_glRenderbufferStorage)(GLenum, GLenum, GLsizei, GLsizei);
typedef void(APIENTRY *P_glFramebufferRenderbuffer)(GLenum, GLenum, GLenum, GLuint);
typedef GLenum(APIENTRY *P_glCheckFramebufferStatus)(GLenum);
typedef void(APIENTRY *P_glGenerateMipmap)(GLenum);
typedef void(APIENTRY *P_glBindAttribLocation)(GLuint, GLuint, const char *);

static P_glGenBuffers p_glGenBuffers;
static P_glBindBuffer p_glBindBuffer;
static P_glBufferData p_glBufferData;
static P_glGenVertexArrays p_glGenVertexArrays;
static P_glBindVertexArray p_glBindVertexArray;
static P_glEnableVertexAttribArray p_glEnableVertexAttribArray;
static P_glVertexAttribPointer p_glVertexAttribPointer;
static P_glCreateShader p_glCreateShader;
static P_glShaderSource p_glShaderSource;
static P_glCompileShader p_glCompileShader;
static P_glGetShaderiv p_glGetShaderiv;
static P_glGetShaderInfoLog p_glGetShaderInfoLog;
static P_glCreateProgram p_glCreateProgram;
static P_glAttachShader p_glAttachShader;
static P_glLinkProgram p_glLinkProgram;
static P_glGetProgramiv p_glGetProgramiv;
static P_glGetProgramInfoLog p_glGetProgramInfoLog;
static P_glUseProgram p_glUseProgram;
static P_glGetUniformLocation p_glGetUniformLocation;
static P_glUniformMatrix4fv p_glUniformMatrix4fv;
static P_glUniform3f p_glUniform3f;
static P_glUniform1i p_glUniform1i;
static P_glGenFramebuffers p_glGenFramebuffers;
static P_glBindFramebuffer p_glBindFramebuffer;
static P_glGenRenderbuffers p_glGenRenderbuffers;
static P_glBindRenderbuffer p_glBindRenderbuffer;
static P_glRenderbufferStorage p_glRenderbufferStorage;
static P_glFramebufferRenderbuffer p_glFramebufferRenderbuffer;
static P_glCheckFramebufferStatus p_glCheckFramebufferStatus;
static P_glGenerateMipmap p_glGenerateMipmap;
static P_glBindAttribLocation p_glBindAttribLocation;

#define LOAD(v, n)                                                                                 \
    do {                                                                                           \
        v = (void *)eglGetProcAddress(n);                                                          \
        if (!v) {                                                                                  \
            fprintf(stderr, "gl-probe: no %s\n", n);                                                \
            return 2;                                                                              \
        }                                                                                          \
    } while (0)

#define GL_ARRAY_BUFFER_ 0x8892
#define GL_ELEMENT_ARRAY_BUFFER_ 0x8893
#define GL_STATIC_DRAW_ 0x88E4
#define GL_VERTEX_SHADER_ 0x8B31
#define GL_FRAGMENT_SHADER_ 0x8B30
#define GL_COMPILE_STATUS_ 0x8B81
#define GL_LINK_STATUS_ 0x8B82
#define GL_FRAMEBUFFER_ 0x8D40
#define GL_RENDERBUFFER_ 0x8D41
#define GL_COLOR_ATTACHMENT0_ 0x8CE0
#define GL_DEPTH_ATTACHMENT_ 0x8D00
#define GL_FRAMEBUFFER_COMPLETE_ 0x8CD5
#define GL_DEPTH_COMPONENT24_ 0x81A6
#define GL_RGBA8_ 0x8058
#define GL_CLAMP_TO_EDGE_ 0x812F
#define GL_LINEAR_MIPMAP_LINEAR_ 0x2703

#define W 256
#define H 256

/* GLSL 1.50, not 3.30: Zink over this forwarder advertises GL **3.2**, and a
 * `#version 330` shader is rejected outright. Attribute locations are bound with
 * `glBindAttribLocation` for the same reason — `layout(location=)` on an input
 * is a 3.3 feature. */
static const char *VS =
    "#version 150 core\n"
    "in vec3 aPos;\n"
    "in vec3 aNormal;\n"
    "in vec2 aUV;\n"
    "uniform mat4 uMVP;\n"
    "out vec3 vN;\n"
    "out vec2 vUV;\n"
    "void main() {\n"
    "  vN = aNormal;\n"
    "  vUV = aUV;\n"
    "  gl_Position = uMVP * vec4(aPos, 1.0);\n"
    "}\n";

static const char *FS =
    "#version 150 core\n"
    "in vec3 vN;\n"
    "in vec2 vUV;\n"
    "uniform vec3 uTint;\n"
    "uniform sampler2D uTex;\n"
    "out vec4 oColour;\n"
    "void main() {\n"
    "  float l = max(dot(normalize(vN), normalize(vec3(0.4, 0.7, 0.6))), 0.0);\n"
    "  vec3 t = texture(uTex, vUV).rgb;\n"
    "  oColour = vec4(uTint * t * (0.15 + 0.85 * l), 1.0);\n"
    "}\n";

static int compile(GLenum stage, const char *src, GLuint *out) {
    GLuint s = p_glCreateShader(stage);
    p_glShaderSource(s, 1, &src, NULL);
    p_glCompileShader(s);
    GLint ok = 0;
    p_glGetShaderiv(s, GL_COMPILE_STATUS_, &ok);
    if (!ok) {
        char log[4096];
        p_glGetShaderInfoLog(s, sizeof log, NULL, log);
        fprintf(stderr, "gl-probe: shader failed: %s\n", log);
        return 0;
    }
    *out = s;
    return 1;
}

/* A UV sphere: 48x48 quads is 4608 triangles and 2401 vertices, which is enough
 * geometry that a dropped draw or a broken index buffer is obvious in the
 * coverage number rather than a subtle shading difference. */
#define SEG 48
#define RING 48

int main(void) {
    /* The surfaceless platform, named rather than left to `EGL_DEFAULT_DISPLAY`:
     * the default picks GBM or X11 first when either is present, and both of
     * those need a DRM device this configuration does not have. */
    PFNEGLGETPLATFORMDISPLAYEXTPROC getdpy =
        (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
    EGLDisplay dpy = getdpy ? getdpy(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL)
                            : eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (dpy == EGL_NO_DISPLAY) {
        fprintf(stderr, "gl-probe: no EGL display\n");
        return 1;
    }
    EGLint major = 0, minor = 0;
    if (!eglInitialize(dpy, &major, &minor)) {
        fprintf(stderr, "gl-probe: eglInitialize failed (0x%x)\n", eglGetError());
        return 1;
    }
    if (!eglBindAPI(EGL_OPENGL_API)) {
        fprintf(stderr, "gl-probe: no desktop GL\n");
        return 1;
    }
    EGLint cfg_attrs[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE,
                          EGL_OPENGL_BIT,   EGL_NONE};
    EGLConfig cfg;
    EGLint n = 0;
    if (!eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &n) || n == 0) {
        fprintf(stderr, "gl-probe: no EGL config\n");
        return 1;
    }
    /* 3.2 core, which is the floor this program needs (VAOs, `#version 150`,
     * FBOs, mipmap generation) and is what Zink advertises here. Asking for a
     * version the implementation does not have is `EGL_BAD_MATCH` and nothing
     * more informative, so the request is the minimum rather than the maximum. */
    EGLint ctx_attrs[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 2,
                          EGL_CONTEXT_OPENGL_PROFILE_MASK,
                          EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attrs);
    if (ctx == EGL_NO_CONTEXT) {
        fprintf(stderr, "gl-probe: no GL 3.2 core context (0x%x)\n", eglGetError());
        return 1;
    }
    if (!eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) {
        fprintf(stderr, "gl-probe: eglMakeCurrent failed (0x%x)\n", eglGetError());
        return 1;
    }

    printf("renderer : %s\n", glGetString(GL_RENDERER));
    printf("version  : %s\n", glGetString(GL_VERSION));

    LOAD(p_glGenBuffers, "glGenBuffers");
    LOAD(p_glBindBuffer, "glBindBuffer");
    LOAD(p_glBufferData, "glBufferData");
    LOAD(p_glGenVertexArrays, "glGenVertexArrays");
    LOAD(p_glBindVertexArray, "glBindVertexArray");
    LOAD(p_glEnableVertexAttribArray, "glEnableVertexAttribArray");
    LOAD(p_glVertexAttribPointer, "glVertexAttribPointer");
    LOAD(p_glCreateShader, "glCreateShader");
    LOAD(p_glShaderSource, "glShaderSource");
    LOAD(p_glCompileShader, "glCompileShader");
    LOAD(p_glGetShaderiv, "glGetShaderiv");
    LOAD(p_glGetShaderInfoLog, "glGetShaderInfoLog");
    LOAD(p_glCreateProgram, "glCreateProgram");
    LOAD(p_glAttachShader, "glAttachShader");
    LOAD(p_glLinkProgram, "glLinkProgram");
    LOAD(p_glGetProgramiv, "glGetProgramiv");
    LOAD(p_glGetProgramInfoLog, "glGetProgramInfoLog");
    LOAD(p_glUseProgram, "glUseProgram");
    LOAD(p_glGetUniformLocation, "glGetUniformLocation");
    LOAD(p_glUniformMatrix4fv, "glUniformMatrix4fv");
    LOAD(p_glUniform3f, "glUniform3f");
    LOAD(p_glUniform1i, "glUniform1i");
    LOAD(p_glGenFramebuffers, "glGenFramebuffers");
    LOAD(p_glBindFramebuffer, "glBindFramebuffer");
    LOAD(p_glGenRenderbuffers, "glGenRenderbuffers");
    LOAD(p_glBindRenderbuffer, "glBindRenderbuffer");
    LOAD(p_glRenderbufferStorage, "glRenderbufferStorage");
    LOAD(p_glFramebufferRenderbuffer, "glFramebufferRenderbuffer");
    LOAD(p_glCheckFramebufferStatus, "glCheckFramebufferStatus");
    LOAD(p_glGenerateMipmap, "glGenerateMipmap");
    LOAD(p_glBindAttribLocation, "glBindAttribLocation");

    /* --- the render target ------------------------------------------------ */
    GLuint fbo = 0, colour = 0, depth = 0;
    p_glGenFramebuffers(1, &fbo);
    p_glBindFramebuffer(GL_FRAMEBUFFER_, fbo);
    p_glGenRenderbuffers(1, &colour);
    p_glBindRenderbuffer(GL_RENDERBUFFER_, colour);
    p_glRenderbufferStorage(GL_RENDERBUFFER_, GL_RGBA8_, W, H);
    p_glFramebufferRenderbuffer(GL_FRAMEBUFFER_, GL_COLOR_ATTACHMENT0_, GL_RENDERBUFFER_, colour);
    p_glGenRenderbuffers(1, &depth);
    p_glBindRenderbuffer(GL_RENDERBUFFER_, depth);
    p_glRenderbufferStorage(GL_RENDERBUFFER_, GL_DEPTH_COMPONENT24_, W, H);
    p_glFramebufferRenderbuffer(GL_FRAMEBUFFER_, GL_DEPTH_ATTACHMENT_, GL_RENDERBUFFER_, depth);
    if (p_glCheckFramebufferStatus(GL_FRAMEBUFFER_) != GL_FRAMEBUFFER_COMPLETE_) {
        fprintf(stderr, "gl-probe: the framebuffer is not complete\n");
        return 1;
    }

    /* --- geometry --------------------------------------------------------- */
    static float verts[(SEG + 1) * (RING + 1) * 8];
    static unsigned idx[SEG * RING * 6];
    int v = 0;
    for (int i = 0; i <= RING; i++) {
        float phi = (float)M_PI * (float)i / RING;
        for (int j = 0; j <= SEG; j++) {
            float th = 2.0f * (float)M_PI * (float)j / SEG;
            float x = sinf(phi) * cosf(th), y = cosf(phi), z = sinf(phi) * sinf(th);
            verts[v++] = x;
            verts[v++] = y;
            verts[v++] = z;
            verts[v++] = x;
            verts[v++] = y;
            verts[v++] = z;
            verts[v++] = (float)j / SEG;
            verts[v++] = (float)i / RING;
        }
    }
    int q = 0;
    for (int i = 0; i < RING; i++) {
        for (int j = 0; j < SEG; j++) {
            unsigned a = i * (SEG + 1) + j, b = a + SEG + 1;
            idx[q++] = a;
            idx[q++] = b;
            idx[q++] = a + 1;
            idx[q++] = a + 1;
            idx[q++] = b;
            idx[q++] = b + 1;
        }
    }

    GLuint vao = 0, vbo = 0, ibo = 0;
    p_glGenVertexArrays(1, &vao);
    p_glBindVertexArray(vao);
    p_glGenBuffers(1, &vbo);
    p_glBindBuffer(GL_ARRAY_BUFFER_, vbo);
    p_glBufferData(GL_ARRAY_BUFFER_, sizeof verts, verts, GL_STATIC_DRAW_);
    p_glGenBuffers(1, &ibo);
    p_glBindBuffer(GL_ELEMENT_ARRAY_BUFFER_, ibo);
    p_glBufferData(GL_ELEMENT_ARRAY_BUFFER_, sizeof idx, idx, GL_STATIC_DRAW_);
    p_glEnableVertexAttribArray(0);
    p_glVertexAttribPointer(0, 3, GL_FLOAT, GL_FALSE, 8 * sizeof(float), (void *)0);
    p_glEnableVertexAttribArray(1);
    p_glVertexAttribPointer(1, 3, GL_FLOAT, GL_FALSE, 8 * sizeof(float),
                            (void *)(3 * sizeof(float)));
    p_glEnableVertexAttribArray(2);
    p_glVertexAttribPointer(2, 2, GL_FLOAT, GL_FALSE, 8 * sizeof(float),
                            (void *)(6 * sizeof(float)));

    /* --- a texture, so samplers and image views are exercised too ---------- */
    static unsigned char tex[64 * 64 * 4];
    for (int y = 0; y < 64; y++)
        for (int x = 0; x < 64; x++) {
            int c = ((x >> 3) ^ (y >> 3)) & 1;
            unsigned char *p = &tex[(y * 64 + x) * 4];
            p[0] = c ? 235 : 60;
            p[1] = (unsigned char)(x * 4);
            p[2] = (unsigned char)(y * 4);
            p[3] = 255;
        }
    GLuint texid = 0;
    glGenTextures(1, &texid);
    glBindTexture(GL_TEXTURE_2D, texid);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, 64, 64, 0, GL_RGBA, GL_UNSIGNED_BYTE, tex);
    p_glGenerateMipmap(GL_TEXTURE_2D);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR_MIPMAP_LINEAR_);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE_);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE_);

    /* --- the pipeline ------------------------------------------------------ */
    GLuint vs = 0, fs = 0;
    if (!compile(GL_VERTEX_SHADER_, VS, &vs) || !compile(GL_FRAGMENT_SHADER_, FS, &fs)) return 1;
    GLuint prog = p_glCreateProgram();
    p_glAttachShader(prog, vs);
    p_glAttachShader(prog, fs);
    p_glBindAttribLocation(prog, 0, "aPos");
    p_glBindAttribLocation(prog, 1, "aNormal");
    p_glBindAttribLocation(prog, 2, "aUV");
    p_glLinkProgram(prog);
    GLint ok = 0;
    p_glGetProgramiv(prog, GL_LINK_STATUS_, &ok);
    if (!ok) {
        char log[4096];
        p_glGetProgramInfoLog(prog, sizeof log, NULL, log);
        fprintf(stderr, "gl-probe: link failed: %s\n", log);
        return 1;
    }
    p_glUseProgram(prog);
    GLint uMVP = p_glGetUniformLocation(prog, "uMVP");
    GLint uTint = p_glGetUniformLocation(prog, "uTint");
    GLint uTex = p_glGetUniformLocation(prog, "uTex");
    p_glUniform1i(uTex, 0);

    /* --- draw -------------------------------------------------------------- */
    glViewport(0, 0, W, H);
    glEnable(GL_DEPTH_TEST);
    glDepthFunc(GL_LESS);
    glClearColor(0.05f, 0.05f, 0.10f, 1.0f);

    /* Two passes over the same mesh at different depths, the far one drawn
     * second. If the depth test is ignored the far mesh overpaints the near one
     * and the colour balance flips — which the checks below detect. */
    for (int frame = 0; frame < 8; frame++) {
        glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
        float a = 0.4f * frame;
        for (int pass = 0; pass < 2; pass++) {
            /* The far copy is the *larger* one, so that if the depth test is
             * ignored it overpaints the near one completely and the warm/cool
             * balance inverts rather than merely shifting. */
            float s = pass ? 0.95f : 0.85f;
            float zoff = pass ? 0.45f : 0.0f;
            float c = cosf(a), sn = sinf(a);
            /* column-major: scale, rotate about Y, translate in Z, then a
             * trivial perspective divide by w = 1 + z. */
            float m[16] = {s * c, 0, s * sn, 0,      0,          s, 0, 0,
                           -s * sn, 0, s * c,  0.35f, 0,          0, zoff, 1};
            p_glUniformMatrix4fv(uMVP, 1, GL_FALSE, m);
            if (pass)
                p_glUniform3f(uTint, 0.25f, 1.0f, 0.35f);
            else
                p_glUniform3f(uTint, 1.0f, 0.45f, 0.2f);
            glDrawElements(GL_TRIANGLES, SEG * RING * 6, GL_UNSIGNED_INT, 0);
        }
        glFinish();
    }

    /* --- read the pixels back and judge them ------------------------------- */
    static unsigned char px[W * H * 4];
    glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, px);
    GLenum err = glGetError();
    if (err != GL_NO_ERROR) {
        fprintf(stderr, "gl-probe: GL error 0x%x\n", err);
        return 1;
    }

    long covered = 0, warm = 0, cool = 0;
    /* A cheap distinct-colour count: a 5-bit-per-channel occupancy bitmap. */
    static unsigned char seen[1 << 15];
    long distinct = 0;
    for (int i = 0; i < W * H; i++) {
        unsigned r = px[i * 4], g = px[i * 4 + 1], b = px[i * 4 + 2];
        if (r > 20 || g > 20 || b > 40) covered++;
        if (r > g) warm++;
        if (g > r && g > b) cool++;
        unsigned key = ((r >> 3) << 10) | ((g >> 3) << 5) | (b >> 3);
        if (!seen[key]) {
            seen[key] = 1;
            distinct++;
        }
    }
    double cov = 100.0 * covered / (W * H);
    printf("coverage : %.1f%% of %dx%d\n", cov, W, H);
    printf("colours  : %ld distinct (5 bits per channel)\n", distinct);
    printf("warm/cool: %ld / %ld\n", warm, cool);

    const char *out = getenv("GL_PROBE_PPM");
    if (out) {
        FILE *f = fopen(out, "wb");
        if (f) {
            fprintf(f, "P6\n%d %d\n255\n", W, H);
            /* glReadPixels is bottom-up; PPM is top-down. */
            for (int y = H - 1; y >= 0; y--)
                for (int x = 0; x < W; x++) fwrite(&px[(y * W + x) * 4], 1, 3, f);
            fclose(f);
            printf("wrote    : %s\n", out);
        }
    }

    int bad = 0;
    if (cov < 5.0 || cov > 95.0) {
        fprintf(stderr, "gl-probe: FAIL coverage %.1f%% is not a rendered mesh\n", cov);
        bad = 1;
    }
    if (distinct < 200) {
        fprintf(stderr, "gl-probe: FAIL only %ld colours; shading did not run\n", distinct);
        bad = 1;
    }
    /* The near mesh is warm and drawn first; with a working depth test it stays
     * in front, so warm pixels must outnumber cool ones. */
    if (warm <= cool) {
        fprintf(stderr, "gl-probe: FAIL the far mesh is in front; depth testing is broken\n");
        bad = 1;
    }
    if (!bad) printf("gl-probe: PASS\n");
    return bad;
}
