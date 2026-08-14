/* gl4-probe — the three OpenGL 4.x capabilities that were missing, exercised.
 *
 * WHY THIS EXISTS SEPARATELY FROM gl-probe
 *
 * `gl-probe` renders a mesh and checks its pixels; it is deliberately written
 * against GLSL 1.50 so that it measures the *pipeline* and not the version, and
 * it passed identically at GL 3.2 and at GL 4.6. So it cannot tell the two
 * apart, and a version string is not evidence: Mesa computes it from capability
 * bits, and a capability bit is a claim about a driver, not a test of one.
 *
 * The jump from 3.2 to 4.6 came from exactly three capabilities that were
 * being reported as absent (docs/design.md §0.5). This program uses all three, in
 * one frame, and judges the result on its pixels:
 *
 *   - **GL_ARB_tessellation_shader** (the GL 4.0 gate). A four-vertex patch is
 *     subdivided at tessellation level 16 and the domain is mapped onto a disc.
 *     The check is geometric and does not depend on colour: a disc fills
 *     pi/4 = 78.5% of its bounding box, and the *same program with no
 *     subdivision* — the four corners alone — is a square inscribed in that
 *     circle, which fills 50%. Measuring 78% is a statement that the
 *     tessellator ran and that its output crossed the wire.
 *
 *   - **GL_ARB_instanced_arrays** (the GL 3.3 gate). The disc's centre is a
 *     vertex attribute with `glVertexAttribDivisor(.., 1)`, and four instances
 *     are drawn in one call. Four discs in four places is the divisor being
 *     honoured; one disc, or four on top of each other, is not.
 *
 *   - **GL_ARB_indirect_parameters** (the GL 4.6 gate), with
 *     `gl_DrawID` from ARB_shader_draw_parameters alongside it.
 *     `glMultiDrawArraysIndirectCount` is given an indirect buffer holding
 *     **four** draws and a parameter buffer holding the count **two**, with
 *     `maxdrawcount` 4. Each draw places its mark by `gl_DrawID`, so the frame
 *     says plainly which number was believed: two marks means the parameter
 *     buffer was read, four means `maxdrawcount` was used instead, and one
 *     means `gl_DrawID` is not varying.
 *
 * It also wraps the tessellated draw in an **occlusion query**, which is not a
 * GL 4.x capability at all — it is GL 1.5 — and is here because it is the one
 * answer in this program that does not come back through the framebuffer. A
 * forwarder can render a perfect frame and still return nothing from a query,
 * and nothing that draws would notice; the number is checked against the
 * pixels, so "it returned something" is not enough either.
 *
 * As with gl-probe the frame is also written out, so the harness can compare it
 * against the same program on llvmpipe. Two unrelated implementations agreeing
 * is worth more than either one's self-assessment.
 *
 * Build: cc -o gl4-probe gl4-probe.c -lEGL -lGL -lm
 */
#define _GNU_SOURCE
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GL/gl.h>
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define W 256
#define H 256

/* Enums this system's <GL/gl.h> is too old to carry. */
#define GL_ARRAY_BUFFER_ 0x8892
#define GL_DRAW_INDIRECT_BUFFER_ 0x8F3F
#define GL_PARAMETER_BUFFER_ 0x80EE
#define GL_STATIC_DRAW_ 0x88E4
#define GL_VERTEX_SHADER_ 0x8B31
#define GL_FRAGMENT_SHADER_ 0x8B30
#define GL_TESS_CONTROL_SHADER_ 0x8E88
#define GL_TESS_EVALUATION_SHADER_ 0x8E87
#define GL_COMPILE_STATUS_ 0x8B81
#define GL_LINK_STATUS_ 0x8B82
#define GL_FRAMEBUFFER_ 0x8D40
#define GL_RENDERBUFFER_ 0x8D41
#define GL_COLOR_ATTACHMENT0_ 0x8CE0
#define GL_FRAMEBUFFER_COMPLETE_ 0x8CD5
#define GL_RGBA8_ 0x8058
#define GL_PATCHES_ 0x000E
#define GL_PATCH_VERTICES_ 0x8E72
#define GL_SAMPLES_PASSED_ 0x8914
#define GL_QUERY_RESULT_ 0x8866

typedef void(APIENTRY *P_glGenBuffers)(GLsizei, GLuint *);
typedef void(APIENTRY *P_glBindBuffer)(GLenum, GLuint);
typedef void(APIENTRY *P_glBufferData)(GLenum, ptrdiff_t, const void *, GLenum);
typedef void(APIENTRY *P_glGenVertexArrays)(GLsizei, GLuint *);
typedef void(APIENTRY *P_glBindVertexArray)(GLuint);
typedef void(APIENTRY *P_glEnableVertexAttribArray)(GLuint);
typedef void(APIENTRY *P_glVertexAttribPointer)(GLuint, GLint, GLenum, GLboolean, GLsizei,
                                                const void *);
typedef void(APIENTRY *P_glVertexAttribDivisor)(GLuint, GLuint);
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
typedef void(APIENTRY *P_glGenFramebuffers)(GLsizei, GLuint *);
typedef void(APIENTRY *P_glBindFramebuffer)(GLenum, GLuint);
typedef void(APIENTRY *P_glGenRenderbuffers)(GLsizei, GLuint *);
typedef void(APIENTRY *P_glBindRenderbuffer)(GLenum, GLuint);
typedef void(APIENTRY *P_glRenderbufferStorage)(GLenum, GLenum, GLsizei, GLsizei);
typedef void(APIENTRY *P_glFramebufferRenderbuffer)(GLenum, GLenum, GLenum, GLuint);
typedef GLenum(APIENTRY *P_glCheckFramebufferStatus)(GLenum);
typedef void(APIENTRY *P_glPatchParameteri)(GLenum, GLint);
typedef void(APIENTRY *P_glDrawArraysInstanced)(GLenum, GLint, GLsizei, GLsizei);
typedef void(APIENTRY *P_glMultiDrawArraysIndirectCount)(GLenum, const void *, ptrdiff_t, GLsizei,
                                                         GLsizei);
typedef void(APIENTRY *P_glGenQueries)(GLsizei, GLuint *);
typedef void(APIENTRY *P_glBeginQuery)(GLenum, GLuint);
typedef void(APIENTRY *P_glEndQuery)(GLenum);
typedef void(APIENTRY *P_glGetQueryObjectuiv)(GLuint, GLenum, GLuint *);

static P_glGenBuffers p_glGenBuffers;
static P_glBindBuffer p_glBindBuffer;
static P_glBufferData p_glBufferData;
static P_glGenVertexArrays p_glGenVertexArrays;
static P_glBindVertexArray p_glBindVertexArray;
static P_glEnableVertexAttribArray p_glEnableVertexAttribArray;
static P_glVertexAttribPointer p_glVertexAttribPointer;
static P_glVertexAttribDivisor p_glVertexAttribDivisor;
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
static P_glGenFramebuffers p_glGenFramebuffers;
static P_glBindFramebuffer p_glBindFramebuffer;
static P_glGenRenderbuffers p_glGenRenderbuffers;
static P_glBindRenderbuffer p_glBindRenderbuffer;
static P_glRenderbufferStorage p_glRenderbufferStorage;
static P_glFramebufferRenderbuffer p_glFramebufferRenderbuffer;
static P_glCheckFramebufferStatus p_glCheckFramebufferStatus;
static P_glPatchParameteri p_glPatchParameteri;
static P_glDrawArraysInstanced p_glDrawArraysInstanced;
static P_glMultiDrawArraysIndirectCount p_glMultiDrawArraysIndirectCount;
static P_glGenQueries p_glGenQueries;
static P_glBeginQuery p_glBeginQuery;
static P_glEndQuery p_glEndQuery;
static P_glGetQueryObjectuiv p_glGetQueryObjectuiv;

#define LOAD(v, n)                                                                                 \
    do {                                                                                           \
        v = (void *)eglGetProcAddress(n);                                                          \
        if (!v) {                                                                                  \
            fprintf(stderr, "gl4-probe: no %s\n", n);                                              \
            return 2;                                                                              \
        }                                                                                          \
    } while (0)

/* The tessellated, instanced disc. `aInst` is the per-instance centre and is the
 * only thing that distinguishes the four instances, so a divisor that is not
 * honoured draws four discs in one place. */
static const char *VS =
    "#version 460 core\n"
    "layout(location=0) in vec2 aPos;\n"
    "layout(location=1) in vec2 aInst;\n"
    "out vec2 vPos;\n"
    "out vec2 vInst;\n"
    "void main() { vPos = aPos; vInst = aInst; }\n";

static const char *TCS =
    "#version 460 core\n"
    "layout(vertices = 4) out;\n"
    "in vec2 vPos[];\n"
    "in vec2 vInst[];\n"
    "out vec2 tcPos[];\n"
    "out vec2 tcInst[];\n"
    "void main() {\n"
    "  tcPos[gl_InvocationID] = vPos[gl_InvocationID];\n"
    "  tcInst[gl_InvocationID] = vInst[gl_InvocationID];\n"
    "  if (gl_InvocationID == 0) {\n"
    "    gl_TessLevelOuter[0] = 16.0; gl_TessLevelOuter[1] = 16.0;\n"
    "    gl_TessLevelOuter[2] = 16.0; gl_TessLevelOuter[3] = 16.0;\n"
    "    gl_TessLevelInner[0] = 16.0; gl_TessLevelInner[1] = 16.0;\n"
    "  }\n"
    "}\n";

/* The square-to-disc map is the "elliptical grid" one, chosen because it makes
 * the tessellation level *visible in the area*: the domain boundary lands on the
 * circle, so the four corners alone give an inscribed square (50% of the
 * bounding box) and a subdivided boundary gives the disc (78.5%). */
static const char *TES =
    "#version 460 core\n"
    "layout(quads, equal_spacing, ccw) in;\n"
    "in vec2 tcPos[];\n"
    "in vec2 tcInst[];\n"
    "out vec3 vCol;\n"
    "void main() {\n"
    "  vec2 a = mix(tcPos[0], tcPos[1], gl_TessCoord.x);\n"
    "  vec2 b = mix(tcPos[3], tcPos[2], gl_TessCoord.x);\n"
    "  vec2 uv = mix(a, b, gl_TessCoord.y);\n"
    "  vec2 d = vec2(uv.x * sqrt(1.0 - uv.y * uv.y * 0.5),\n"
    "                uv.y * sqrt(1.0 - uv.x * uv.x * 0.5));\n"
    "  vec2 c = tcInst[0];\n"
    "  vCol = vec3(0.25 + 0.5 * (0.5 + 0.5 * d.x),\n"
    "              0.25 + 0.5 * (0.5 + 0.5 * d.y),\n"
    "              0.30 + 0.30 * (c.x + c.y));\n"
    "  gl_Position = vec4(c + 0.35 * d, 0.0, 1.0);\n"
    "}\n";

static const char *FS =
    "#version 460 core\n"
    "in vec3 vCol;\n"
    "out vec4 oColour;\n"
    "void main() { oColour = vec4(vCol, 1.0); }\n";

/* The indirect-count marks. `gl_DrawID` is core in 4.6; each draw puts its mark
 * at its own x, so the number of marks *and their positions* say what count was
 * used. */
static const char *MVS =
    "#version 460 core\n"
    "layout(location=0) in vec2 bPos;\n"
    "void main() {\n"
    "  vec2 off = vec2(-0.9 + 0.2 * float(gl_DrawID), 0.9);\n"
    "  gl_Position = vec4(bPos * 0.05 + off, 0.0, 1.0);\n"
    "}\n";

static const char *MFS =
    "#version 460 core\n"
    "out vec4 oColour;\n"
    "void main() { oColour = vec4(1.0, 0.0, 0.0, 1.0); }\n";

static int compile(GLenum stage, const char *src, GLuint *out) {
    GLuint s = p_glCreateShader(stage);
    p_glShaderSource(s, 1, &src, NULL);
    p_glCompileShader(s);
    GLint ok = 0;
    p_glGetShaderiv(s, GL_COMPILE_STATUS_, &ok);
    if (!ok) {
        char log[4096];
        p_glGetShaderInfoLog(s, sizeof log, NULL, log);
        fprintf(stderr, "gl4-probe: shader failed: %s\n", log);
        return 0;
    }
    *out = s;
    return 1;
}

static int link_program(GLuint *out, const char **srcs, const GLenum *stages, int n) {
    GLuint p = p_glCreateProgram();
    for (int i = 0; i < n; i++) {
        GLuint s;
        if (!compile(stages[i], srcs[i], &s))
            return 0;
        p_glAttachShader(p, s);
    }
    p_glLinkProgram(p);
    GLint ok = 0;
    p_glGetProgramiv(p, GL_LINK_STATUS_, &ok);
    if (!ok) {
        char log[4096];
        p_glGetProgramInfoLog(p, sizeof log, NULL, log);
        fprintf(stderr, "gl4-probe: link failed: %s\n", log);
        return 0;
    }
    *out = p;
    return 1;
}

/* A pixel is "drawn" if it is not the clear colour. */
static int drawn(const unsigned char *px, int x, int y) {
    const unsigned char *p = px + ((size_t)y * W + x) * 4;
    return p[0] > 8 || p[1] > 8 || p[2] > 8;
}

static int is_red(const unsigned char *px, int x, int y) {
    const unsigned char *p = px + ((size_t)y * W + x) * 4;
    return p[0] > 180 && p[1] < 70 && p[2] < 70;
}

/* Non-background pixels in the column band [x0,x1) over the top of the frame,
 * which is where the indirect marks are. */
static int red_in_band(const unsigned char *px, int x0, int x1) {
    int n = 0;
    for (int y = H - 40; y < H; y++)
        for (int x = x0; x < x1 && x < W; x++)
            if (x >= 0 && is_red(px, x, y))
                n++;
    return n;
}

int main(void) {
    static const EGLint disp_attr[] = {EGL_NONE};
    PFNEGLGETPLATFORMDISPLAYEXTPROC get_disp =
        (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
    EGLDisplay dpy = get_disp ? get_disp(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY,
                                         disp_attr)
                              : eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, NULL, NULL)) {
        fprintf(stderr, "gl4-probe: eglInitialize failed\n");
        return 1;
    }
    eglBindAPI(EGL_OPENGL_API);
    EGLint cfgattr[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                        EGL_RED_SIZE,     8,               EGL_GREEN_SIZE,      8,
                        EGL_BLUE_SIZE,    8,               EGL_NONE};
    EGLConfig cfg;
    EGLint n = 0;
    if (!eglChooseConfig(dpy, cfgattr, &cfg, 1, &n) || n < 1) {
        fprintf(stderr, "gl4-probe: no config\n");
        return 1;
    }
    EGLint ctxattr[] = {EGL_CONTEXT_MAJOR_VERSION,
                        4,
                        EGL_CONTEXT_MINOR_VERSION,
                        6,
                        EGL_CONTEXT_OPENGL_PROFILE_MASK,
                        EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT,
                        EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctxattr);
    if (ctx == EGL_NO_CONTEXT) {
        fprintf(stderr, "gl4-probe: no 4.6 core context\n");
        return 3;
    }
    if (!eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) {
        fprintf(stderr, "gl4-probe: eglMakeCurrent failed\n");
        return 1;
    }
    printf("renderer %s\n", glGetString(GL_RENDERER));
    printf("version  %s\n", glGetString(GL_VERSION));

    LOAD(p_glGenBuffers, "glGenBuffers");
    LOAD(p_glBindBuffer, "glBindBuffer");
    LOAD(p_glBufferData, "glBufferData");
    LOAD(p_glGenVertexArrays, "glGenVertexArrays");
    LOAD(p_glBindVertexArray, "glBindVertexArray");
    LOAD(p_glEnableVertexAttribArray, "glEnableVertexAttribArray");
    LOAD(p_glVertexAttribPointer, "glVertexAttribPointer");
    LOAD(p_glVertexAttribDivisor, "glVertexAttribDivisor");
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
    LOAD(p_glGenFramebuffers, "glGenFramebuffers");
    LOAD(p_glBindFramebuffer, "glBindFramebuffer");
    LOAD(p_glGenRenderbuffers, "glGenRenderbuffers");
    LOAD(p_glBindRenderbuffer, "glBindRenderbuffer");
    LOAD(p_glRenderbufferStorage, "glRenderbufferStorage");
    LOAD(p_glFramebufferRenderbuffer, "glFramebufferRenderbuffer");
    LOAD(p_glCheckFramebufferStatus, "glCheckFramebufferStatus");
    LOAD(p_glPatchParameteri, "glPatchParameteri");
    LOAD(p_glDrawArraysInstanced, "glDrawArraysInstanced");
    LOAD(p_glMultiDrawArraysIndirectCount, "glMultiDrawArraysIndirectCount");
    LOAD(p_glGenQueries, "glGenQueries");
    LOAD(p_glBeginQuery, "glBeginQuery");
    LOAD(p_glEndQuery, "glEndQuery");
    LOAD(p_glGetQueryObjectuiv, "glGetQueryObjectuiv");

    GLuint fbo, rb;
    p_glGenFramebuffers(1, &fbo);
    p_glBindFramebuffer(GL_FRAMEBUFFER_, fbo);
    p_glGenRenderbuffers(1, &rb);
    p_glBindRenderbuffer(GL_RENDERBUFFER_, rb);
    p_glRenderbufferStorage(GL_RENDERBUFFER_, GL_RGBA8_, W, H);
    p_glFramebufferRenderbuffer(GL_FRAMEBUFFER_, GL_COLOR_ATTACHMENT0_, GL_RENDERBUFFER_, rb);
    if (p_glCheckFramebufferStatus(GL_FRAMEBUFFER_) != GL_FRAMEBUFFER_COMPLETE_) {
        fprintf(stderr, "gl4-probe: incomplete framebuffer\n");
        return 1;
    }

    GLuint vao;
    p_glGenVertexArrays(1, &vao);
    p_glBindVertexArray(vao);

    /* The patch: the corners of the [-1,1] domain square. */
    static const float patch[8] = {-1.f, -1.f, 1.f, -1.f, 1.f, 1.f, -1.f, 1.f};
    /* Four instance centres, one per quadrant. */
    static const float inst[8] = {-0.5f, -0.5f, 0.5f, -0.5f, -0.5f, 0.5f, 0.5f, 0.5f};
    GLuint vbo, ibo;
    p_glGenBuffers(1, &vbo);
    p_glBindBuffer(GL_ARRAY_BUFFER_, vbo);
    p_glBufferData(GL_ARRAY_BUFFER_, sizeof patch, patch, GL_STATIC_DRAW_);
    p_glEnableVertexAttribArray(0);
    p_glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 8, (void *)0);
    p_glGenBuffers(1, &ibo);
    p_glBindBuffer(GL_ARRAY_BUFFER_, ibo);
    p_glBufferData(GL_ARRAY_BUFFER_, sizeof inst, inst, GL_STATIC_DRAW_);
    p_glEnableVertexAttribArray(1);
    p_glVertexAttribPointer(1, 2, GL_FLOAT, GL_FALSE, 8, (void *)0);
    p_glVertexAttribDivisor(1, 1);

    const char *srcs[4] = {VS, TCS, TES, FS};
    const GLenum stages[4] = {GL_VERTEX_SHADER_, GL_TESS_CONTROL_SHADER_,
                              GL_TESS_EVALUATION_SHADER_, GL_FRAGMENT_SHADER_};
    GLuint prog;
    if (!link_program(&prog, srcs, stages, 4))
        return 4;

    glViewport(0, 0, W, H);
    glClearColor(0.f, 0.f, 0.f, 1.f);
    glClear(GL_COLOR_BUFFER_BIT);
    glDisable(GL_DEPTH_TEST);
    glDisable(GL_CULL_FACE);

    GLuint query;
    p_glGenQueries(1, &query);
    p_glUseProgram(prog);
    p_glPatchParameteri(GL_PATCH_VERTICES_, 4);
    p_glBeginQuery(GL_SAMPLES_PASSED_, query);
    p_glDrawArraysInstanced(GL_PATCHES_, 0, 4, 4);
    p_glEndQuery(GL_SAMPLES_PASSED_);

    /* --- the indirect marks ------------------------------------------------
     * Four commands in the indirect buffer, `maxdrawcount` 4, and a parameter
     * buffer that says 2. Only a driver that read the parameter buffer draws
     * two. */
    static const float tri[6] = {-1.f, -1.f, 1.f, -1.f, 0.f, 1.f};
    GLuint tvbo;
    p_glGenBuffers(1, &tvbo);
    p_glBindBuffer(GL_ARRAY_BUFFER_, tvbo);
    p_glBufferData(GL_ARRAY_BUFFER_, sizeof tri, tri, GL_STATIC_DRAW_);
    p_glEnableVertexAttribArray(0);
    p_glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 8, (void *)0);
    p_glVertexAttribDivisor(1, 0);

    struct {
        uint32_t count, instance_count, first, base_instance;
    } cmds[4];
    for (int i = 0; i < 4; i++) {
        cmds[i].count = 3;
        cmds[i].instance_count = 1;
        cmds[i].first = 0;
        cmds[i].base_instance = 0;
    }
    GLuint dib, pb;
    p_glGenBuffers(1, &dib);
    p_glBindBuffer(GL_DRAW_INDIRECT_BUFFER_, dib);
    p_glBufferData(GL_DRAW_INDIRECT_BUFFER_, sizeof cmds, cmds, GL_STATIC_DRAW_);
    const uint32_t how_many = 2;
    p_glGenBuffers(1, &pb);
    p_glBindBuffer(GL_PARAMETER_BUFFER_, pb);
    p_glBufferData(GL_PARAMETER_BUFFER_, sizeof how_many, &how_many, GL_STATIC_DRAW_);

    GLuint mprog;
    const char *msrcs[2] = {MVS, MFS};
    const GLenum mstages[2] = {GL_VERTEX_SHADER_, GL_FRAGMENT_SHADER_};
    if (!link_program(&mprog, msrcs, mstages, 2))
        return 4;
    p_glUseProgram(mprog);
    p_glMultiDrawArraysIndirectCount(GL_TRIANGLES, (void *)0, 0, 4, 0);

    glFinish();
    /* Deliberately read *after* the frame, so that the number can be checked
     * against the pixels rather than only against zero. */
    GLuint samples = 0xdeadbeef;
    p_glGetQueryObjectuiv(query, GL_QUERY_RESULT_, &samples);

    unsigned char *px = malloc((size_t)W * H * 4);
    glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, px);
    GLenum err = glGetError();
    if (err != GL_NO_ERROR) {
        fprintf(stderr, "gl4-probe: GL error 0x%x\n", err);
        return 5;
    }

    /* --- the checks --------------------------------------------------------
     * The disc in the lower-left quadrant is centred at (-0.5,-0.5) with radius
     * 0.35, so its bounding box in pixels is [19,109) on both axes. */
    const int x0 = (int)((-0.85 + 1.0) * 0.5 * W), x1 = (int)((-0.15 + 1.0) * 0.5 * W);
    long inbox = 0;
    for (int y = x0; y < x1; y++)
        for (int x = x0; x < x1; x++)
            if (drawn(px, x, y))
                inbox++;
    double fill = (double)inbox / (double)((x1 - x0) * (x1 - x0));

    /* Four instances means four quadrants covered. */
    int quads = 0;
    const double cx[4] = {-0.5, 0.5, -0.5, 0.5}, cy[4] = {-0.5, -0.5, 0.5, 0.5};
    for (int i = 0; i < 4; i++) {
        int x = (int)((cx[i] + 1.0) * 0.5 * W), y = (int)((cy[i] + 1.0) * 0.5 * H);
        if (drawn(px, x, y))
            quads++;
    }

    /* Marks: draw i sits at NDC x = -0.9 + 0.2*i, ±0.05 wide. */
    int mark[4];
    for (int i = 0; i < 4; i++) {
        double c = -0.9 + 0.2 * i;
        int a = (int)((c - 0.06 + 1.0) * 0.5 * W), b = (int)((c + 0.06 + 1.0) * 0.5 * W);
        mark[i] = red_in_band(px, a, b) > 20;
    }

    long cover = 0, disc_px = 0;
    for (int y = 0; y < H; y++)
        for (int x = 0; x < W; x++)
            if (drawn(px, x, y)) {
                cover++;
                if (!is_red(px, x, y))
                    disc_px++;
            }

    printf("coverage %.1f%% of %dx%d\n", 100.0 * cover / (W * H), W, H);
    printf("occlusion query %u samples (%ld disc pixels)\n", samples, disc_px);
    printf("disc fill %.3f of its bounding box (0.785 tessellated, 0.500 not)\n", fill);
    printf("instances %d of 4 quadrants drawn\n", quads);
    printf("indirect marks %d%d%d%d (expect 1100)\n", mark[0], mark[1], mark[2], mark[3]);

    const char *out = getenv("GL4_PROBE_PPM");
    if (out) {
        FILE *f = fopen(out, "wb");
        if (f) {
            fprintf(f, "P6\n%d %d\n255\n", W, H);
            for (int y = H - 1; y >= 0; y--)
                for (int x = 0; x < W; x++) {
                    const unsigned char *p = px + ((size_t)y * W + x) * 4;
                    fwrite(p, 1, 3, f);
                }
            fclose(f);
        }
    }

    int bad = 0;
    if (fill < 0.70 || fill > 0.85) {
        fprintf(stderr, "gl4-probe: disc fill %.3f — the patch was not subdivided\n", fill);
        bad = 1;
    }
    if (quads != 4) {
        fprintf(stderr, "gl4-probe: %d instances placed, not 4 — the divisor was ignored\n", quads);
        bad = 1;
    }
    /* The query counted the samples that made it into the frame, so the frame
     * itself is the reference: the discs are opaque, un-overlapping and the only
     * thing drawn inside the query. Zero is what a dropped output buffer gives;
     * `0xdeadbeef` is what an untouched one gives. */
    if (labs((long)samples - disc_px) > disc_px / 20) {
        fprintf(stderr,
                "gl4-probe: the occlusion query says %u samples but %ld pixels were drawn\n",
                samples, disc_px);
        bad = 1;
    }
    if (!(mark[0] && mark[1] && !mark[2] && !mark[3])) {
        fprintf(stderr, "gl4-probe: marks %d%d%d%d — the parameter buffer was not honoured\n",
                mark[0], mark[1], mark[2], mark[3]);
        bad = 1;
    }
    free(px);
    return bad ? 6 : 0;
}
