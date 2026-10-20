/* Feature probe only: original PRIME2 NV12 -> EGLImage -> external GLES sampler.
 * No native NV12 mmap, decoded CPU copy, alternate plane textures or fallback.
 * The caller retains every VA/export owner until this function returns. */
#ifndef STOCK_VA_GPU_SAMPLE_H
#define STOCK_VA_GPU_SAMPLE_H
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>

#define SV_FOURCC(a,b,c,d) ((uint32_t)(a) | ((uint32_t)(b) << 8) | ((uint32_t)(c) << 16) | ((uint32_t)(d) << 24))
#define SV_DRM_NV12 SV_FOURCC('N','V','1','2')
#define SV_DRM_R8 SV_FOURCC('R','8',' ',' ')
#define SV_DRM_GR88 SV_FOURCC('G','R','8','8')
#define SV_MOD_INVALID UINT64_C(0x00ffffffffffffff)
struct sv_plane { uint32_t object, offset, pitch; };
static bool sv_extension(const char *extensions, const char *wanted) {
    if (!extensions || !wanted || !*wanted || strchr(wanted, ' ')) return false;
    size_t size = strlen(wanted);
    for (const char *at = extensions; (at = strstr(at, wanted)); at += size)
        if ((at == extensions || at[-1] == ' ') && (at[size] == 0 || at[size] == ' ')) return true;
    return false;
}
static bool sv_gpu_failure(const char *stage) {
    fprintf(stderr, "stock_va_gpu_sample result=unsupported_or_failed stage=%s egl_error=%04x no_cpu_fallback=true\n", stage, eglGetError());
    return false;
}
static GLuint sv_shader(GLenum type, const char *source) {
    GLuint shader = glCreateShader(type);
    if (!shader) return 0;
    glShaderSource(shader, 1, &source, NULL); glCompileShader(shader);
    GLint compiled = 0; glGetShaderiv(shader, GL_COMPILE_STATUS, &compiled);
    if (!compiled) {
        char log[1024] = {0}; glGetShaderInfoLog(shader, sizeof(log), NULL, log);
        fprintf(stderr, "stock_va_gpu_shader kind=%u log=%s\n", type, log);
        glDeleteShader(shader); return 0;
    }
    return shader;
}
static bool sv_planes(const VADRMPRIMESurfaceDescriptor *desc, struct sv_plane planes[2]) {
    if (desc->fourcc != SV_DRM_NV12 || desc->width != 64 || desc->height != 64
        || desc->num_objects == 0 || desc->num_objects > 4) return false;
    if (desc->num_layers == 1 && desc->layers[0].drm_format == SV_DRM_NV12 && desc->layers[0].num_planes == 2) {
        for (unsigned p = 0; p < 2; ++p) planes[p] = (struct sv_plane){
            desc->layers[0].object_index[p], desc->layers[0].offset[p], desc->layers[0].pitch[p] };
    } else if (desc->num_layers == 2 && desc->layers[0].drm_format == SV_DRM_R8
        && desc->layers[1].drm_format == SV_DRM_GR88
        && desc->layers[0].num_planes == 1 && desc->layers[1].num_planes == 1) {
        for (unsigned p = 0; p < 2; ++p) planes[p] = (struct sv_plane){
            desc->layers[p].object_index[0], desc->layers[p].offset[0], desc->layers[p].pitch[0] };
    } else return false;
    for (unsigned p = 0; p < 2; ++p) {
        if (planes[p].object >= desc->num_objects || !planes[p].pitch || planes[p].pitch > INT_MAX
            || planes[p].offset > INT_MAX || planes[p].offset >= desc->objects[planes[p].object].size
            || desc->objects[planes[p].object].drm_format_modifier == SV_MOD_INVALID
            || desc->objects[planes[p].object].fd < 0) return false;
        int flags = fcntl(desc->objects[planes[p].object].fd, F_GETFL);
        if (flags < 0 || (flags & O_ACCMODE) != O_RDWR) return false;
    }
    return true;
}
static bool stock_va_gpu_sample(const VADRMPRIMESurfaceDescriptor *desc) {
    struct sv_plane planes[2];
    if (!sv_planes(desc, planes)) return sv_gpu_failure("exact_nv12_descriptor_shape_permissions_or_modifier");
    EGLDisplay display = EGL_NO_DISPLAY;
    EGLContext context = EGL_NO_CONTEXT;
    EGLSurface surface = EGL_NO_SURFACE;
    EGLImageKHR image = EGL_NO_IMAGE_KHR;
    GLuint vertex = 0, fragment = 0, program = 0, texture = 0;
    bool initialized = false, current = false, success = false;
    const char *failure_stage = "egl_context_or_target_setup";
    PFNEGLDESTROYIMAGEKHRPROC destroy_image = NULL;
    const char *client = eglQueryString(EGL_NO_DISPLAY, EGL_EXTENSIONS);
    if (!sv_extension(client, "EGL_EXT_platform_base") || !sv_extension(client, "EGL_MESA_platform_surfaceless"))
        return sv_gpu_failure("surfaceless_platform_extensions");
    PFNEGLGETPLATFORMDISPLAYEXTPROC get_display = (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    PFNEGLCREATEIMAGEKHRPROC create_image = (PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
    destroy_image = (PFNEGLDESTROYIMAGEKHRPROC)eglGetProcAddress("eglDestroyImageKHR");
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC bind_image = (PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    if (!get_display || !create_image || !destroy_image || !bind_image) return sv_gpu_failure("required_image_entrypoints");
    display = get_display(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
    if (display == EGL_NO_DISPLAY) return sv_gpu_failure("surfaceless_display");
    EGLint major = 0, minor = 0;
    if (!eglInitialize(display, &major, &minor)) goto fail;
    initialized = true;
    const char *extensions = eglQueryString(display, EGL_EXTENSIONS);
    /* Require explicit modifiers even for linear objects so no imported layout
     * information is discarded or guessed. Unsupported import is a typed fail. */
    if (!sv_extension(extensions, "EGL_EXT_image_dma_buf_import")
        || !sv_extension(extensions, "EGL_EXT_image_dma_buf_import_modifiers")
        || !sv_extension(extensions, "EGL_KHR_image_base")) {
        failure_stage = "missing_required_egl_dma_buf_modifier_or_image_extension"; goto fail;
    }
    if (!eglBindAPI(EGL_OPENGL_ES_API)) goto fail;
    const EGLint config_attributes[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT, EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8,
        EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8, EGL_NONE };
    EGLConfig config = NULL; EGLint count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &count) || count != 1) goto fail;
    const EGLint channel_names[] = { EGL_RED_SIZE, EGL_GREEN_SIZE, EGL_BLUE_SIZE, EGL_ALPHA_SIZE };
    for (unsigned c = 0; c < 4; ++c) {
        EGLint bits = 0;
        if (!eglGetConfigAttrib(display, config, channel_names[c], &bits) || bits != 8) goto fail;
    }
    const EGLint context_attributes[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    context = eglCreateContext(display, config, EGL_NO_CONTEXT, context_attributes);
    if (context == EGL_NO_CONTEXT) goto fail;
    const EGLint surface_attributes[] = { EGL_WIDTH, 4, EGL_HEIGHT, 4, EGL_NONE };
    surface = eglCreatePbufferSurface(display, config, surface_attributes);
    if (surface == EGL_NO_SURFACE || !eglMakeCurrent(display, surface, surface, context)) goto fail;
    current = true;
    const char *renderer = (const char *)glGetString(GL_RENDERER);
    const char *gl_extensions = (const char *)glGetString(GL_EXTENSIONS);
    if (!renderer || !strstr(renderer, "zink") || !strstr(renderer, "Apple")
        || strstr(renderer, "llvmpipe") || strstr(renderer, "softpipe")
        || !sv_extension(gl_extensions, "GL_OES_EGL_image_external")) {
        failure_stage = "native_Apple_Zink_renderer_or_external_sampler_extension_absent"; goto fail;
    }
    printf("stock_va_gpu_renderer egl=%d.%d renderer=%s external_oes=true modifiers=true surfaceless=true target=4x4RGBA8\n", major, minor, renderer);
    /* No shader loop, varying index, image store, alternate conversion or NV12
     * host read. All16 fragments sample the same known constant decoded frame. */
    failure_stage = "fixed_external_sampler_shader_compile_or_link";
    vertex = sv_shader(GL_VERTEX_SHADER,
        "attribute vec2 position; void main(){gl_Position=vec4(position,0.0,1.0);}");
    fragment = sv_shader(GL_FRAGMENT_SHADER,
        "#extension GL_OES_EGL_image_external : require\n"
        "precision mediump float; uniform samplerExternalOES decoded;"
        "void main(){gl_FragColor=texture2D(decoded,vec2(0.5,0.5));}");
    if (!vertex || !fragment) goto fail;
    program = glCreateProgram(); if (!program) goto fail;
    glAttachShader(program, vertex); glAttachShader(program, fragment);
    glBindAttribLocation(program, 0, "position"); glLinkProgram(program);
    GLint linked = 0; glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) goto fail;
    /* EGL attributes are the ORIGINAL plane layout and object FDs, not a
     * flattened CPU buffer or replacement texture. Plane object sharing stays
     * intact by repeating its exact FD when both planes share an object. */
    EGLint attributes[64]; unsigned at = 0;
#define SV_ATTRIBUTE(key, value) do { attributes[at++] = (key); attributes[at++] = (EGLint)(value); } while (0)
    SV_ATTRIBUTE(EGL_WIDTH, desc->width); SV_ATTRIBUTE(EGL_HEIGHT, desc->height);
    SV_ATTRIBUTE(EGL_LINUX_DRM_FOURCC_EXT, SV_DRM_NV12);
    const EGLint fd_keys[2] = { EGL_DMA_BUF_PLANE0_FD_EXT, EGL_DMA_BUF_PLANE1_FD_EXT };
    const EGLint offset_keys[2] = { EGL_DMA_BUF_PLANE0_OFFSET_EXT, EGL_DMA_BUF_PLANE1_OFFSET_EXT };
    const EGLint pitch_keys[2] = { EGL_DMA_BUF_PLANE0_PITCH_EXT, EGL_DMA_BUF_PLANE1_PITCH_EXT };
    const EGLint low_keys[2] = { EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE1_MODIFIER_LO_EXT };
    const EGLint high_keys[2] = { EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, EGL_DMA_BUF_PLANE1_MODIFIER_HI_EXT };
    for (unsigned p = 0; p < 2; ++p) {
        unsigned object = planes[p].object;
        uint64_t modifier = desc->objects[object].drm_format_modifier;
        SV_ATTRIBUTE(fd_keys[p], desc->objects[object].fd);
        SV_ATTRIBUTE(offset_keys[p], planes[p].offset); SV_ATTRIBUTE(pitch_keys[p], planes[p].pitch);
        SV_ATTRIBUTE(low_keys[p], (uint32_t)modifier); SV_ATTRIBUTE(high_keys[p], (uint32_t)(modifier >> 32));
        printf("stock_va_gpu_import plane=%u original_object=%u original_fd=%d offset=%u pitch=%u modifier=%016" PRIx64 "\n", p, object, desc->objects[object].fd, planes[p].offset, planes[p].pitch, modifier);
    }
    SV_ATTRIBUTE(EGL_YUV_COLOR_SPACE_HINT_EXT, EGL_ITU_REC601_EXT);
    SV_ATTRIBUTE(EGL_SAMPLE_RANGE_HINT_EXT, EGL_YUV_NARROW_RANGE_EXT);
    SV_ATTRIBUTE(EGL_YUV_CHROMA_HORIZONTAL_SITING_HINT_EXT, EGL_YUV_CHROMA_SITING_0_5_EXT);
    SV_ATTRIBUTE(EGL_YUV_CHROMA_VERTICAL_SITING_HINT_EXT, EGL_YUV_CHROMA_SITING_0_5_EXT);
    attributes[at++] = EGL_NONE;
#undef SV_ATTRIBUTE
    /* 3dimension/format pairs +10plane pairs +4YUV pairs +terminator =35ints. */
    if (at > sizeof(attributes) / sizeof(attributes[0])) goto fail;
    failure_stage = "original_NV12_dma_buf_EGLImage_import";
    image = create_image(display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, attributes);
    if (image == EGL_NO_IMAGE_KHR) goto fail;
    glGenTextures(1, &texture); if (!texture) goto fail;
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_EXTERNAL_OES, texture);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    if (glGetError() != GL_NO_ERROR) goto fail;
    /* From native image binding through genuine GPU completion, any error must
     * retain every EGL/VA/export owner rather than release uncertain GPU work. */
    uncertain = true;
    bind_image(GL_TEXTURE_EXTERNAL_OES, (GLeglImageOES)image);
    if (glGetError() != GL_NO_ERROR) retain("external EGLImage texture binding failed after admission");
    glUseProgram(program);
    GLint sampler = glGetUniformLocation(program, "decoded");
    if (sampler < 0) retain("external sampler uniform missing after image binding");
    glUniform1i(sampler, 0);
    const GLfloat vertices[6] = { -1.0f, -1.0f, 3.0f, -1.0f, -1.0f, 3.0f };
    glViewport(0, 0, 4, 4); glDisable(GL_DITHER); glDisable(GL_BLEND);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, vertices); glEnableVertexAttribArray(0);
    glDrawArrays(GL_TRIANGLES, 0, 3); glFinish();
    if (glGetError() != GL_NO_ERROR) retain("external NV12 draw/finish failed; GPU completion unproven");
    /* Diagnostic RGBA readback only. NV12 compressed/decoded storage is never
     * host-addressed. glReadPixels follows actual native GPU completion. */
    unsigned char pixels[4 * 4 * 4] = {0};
    glReadPixels(0, 0, 4, 4, GL_RGBA, GL_UNSIGNED_BYTE, pixels);
    if (glGetError() != GL_NO_ERROR) retain("RGBA diagnostic read failed after external draw");
    glFinish();
    if (glGetError() != GL_NO_ERROR) retain("RGBA diagnostic GPU completion failed");
    uncertain = false;
    /* Independently specified BT.601 limited-range Y40/Cb80/Cr180:
     * Y=(40-16)/219, Cb=(80-128)/224, Cr=(180-128)/224.
     * RGB=[Y+1.402Cr,Y-.344136Cb-.714136Cr,Y+1.772Cb], clipped UNORM8. */
    const int expected[4] = {111, 4, 0, 255};
    int maximum = 0;
    for (unsigned p = 0; p < 16; ++p) for (unsigned c = 0; c < 4; ++c) {
        int error = abs((int)pixels[p * 4 + c] - expected[c]);
        if (error > maximum) maximum = error;
    }
    printf("stock_va_gpu_pixels first_rgba=[%u,%u,%u,%u] expected_rgba=[111,4,0,255] pixels=16 maximum_byte_error=%d tolerance=3 source_ycbcr=[40,80,180] color=601_narrow original_prime2=true shader_loops=0 draws=1 vertices=3\n", pixels[0], pixels[1], pixels[2], pixels[3], maximum);
    success = maximum <= 3;
    goto cleanup;
fail:
    sv_gpu_failure(failure_stage);
cleanup:
    if (uncertain) retain("GPU sampler cleanup requested before native completion");
    if (current) {
        if (texture) glDeleteTextures(1, &texture);
        if (program) glDeleteProgram(program);
        if (fragment) glDeleteShader(fragment);
        if (vertex) glDeleteShader(vertex);
        /* Even deletion is completed before imported image/export teardown. */
        glFinish();
        if (glGetError() != GL_NO_ERROR) retain("GPU sampler object cleanup/finish failed");
    }
    if (image != EGL_NO_IMAGE_KHR && !destroy_image(display, image)) retain("eglDestroyImage failed; imported owner disposition unknown");
    if (current && !eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT)) retain("GPU sampler unbind failed");
    if (surface != EGL_NO_SURFACE && !eglDestroySurface(display, surface)) retain("GPU sampler surface destruction failed");
    if (context != EGL_NO_CONTEXT && !eglDestroyContext(display, context)) retain("GPU sampler context destruction failed");
    if (initialized && !eglTerminate(display)) retain("GPU sampler EGL termination failed");
    if (success) printf("stock_va_gpu_sample result=pass actual_gpu_completion=true egl_image_destroyed=true original_va_export_owners_still_live=true\n");
    else fprintf(stderr, "stock_va_gpu_sample result=failed typed_no_fallback=true owners_retired_before_return=true\n");
    return success;
}
#endif
