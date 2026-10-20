/* Source-only bounded stock-libva probe. No driver, libva or asset modifications.
 * Build: cc -std=c11 -Wall -Wextra -Werror stock-va-h264-client.c \
 *           $(pkg-config --cflags --libs libva libva-drm) -o stock-va-h264-client
 * Input MUST be original-pcm-poc0-marking-64x64.h264 from this repository.
 * Submit exactly its first THREE original VCL NALs: IDR, marking I_PCM, P-skip.
 * No decoded pixel is mapped, copied or read by this program.
 */
#define _POSIX_C_SOURCE 200809L
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_drmcommon.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <limits.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

#define STREAM_SIZE 24790u
#define STREAM_FNV UINT64_C(0x8e6ec57aa8e3701f)
#define FRAMES 3
#define MAX_NALS 16
#define MAX_RBSP 8192
static bool uncertain;
static void retain(const char *why) {
    fprintf(stderr, "FAIL stock-va pid=%ld native_disposition_unproven=true retain_owners=true reason=%s\n", (long)getpid(), why);
    fflush(NULL);
    for (;;) pause(); /* no watchdog, native retries, allocation or teardown */
}
static void require(bool condition, const char *why) {
    if (condition) return;
    if (uncertain) retain(why);
    fprintf(stderr, "FAIL stock-va: %s\n", why);
    exit(1);
}
static bool va_ok(VAStatus status, const char *operation) {
    if (status == VA_STATUS_SUCCESS) return true;
    fprintf(stderr, "VA error operation=%s status=%08x text=%s\n", operation, status, vaErrorStr(status));
    if (uncertain) retain(operation);
    return false;
}
static uint64_t checksum(const uint8_t *data, size_t size) {
    uint64_t value = UINT64_C(0xcbf29ce484222325);
    for (size_t i = 0; i < size; ++i) value = (value ^ data[i]) * UINT64_C(0x100000001b3);
    return value;
}
struct nal { size_t start, offset, length; unsigned type, ref; };
struct bits { uint8_t bytes[MAX_RBSP]; size_t size, at; };
static uint32_t read_bits(struct bits *bits, unsigned count) {
    require(count <= 32 && bits->at + count <= bits->size * 8, "truncated fixture header");
    uint32_t result = 0;
    for (unsigned i = 0; i < count; ++i) {
        result = (result << 1) | ((bits->bytes[bits->at / 8] >> (7 - bits->at % 8)) & 1);
        ++bits->at;
    }
    return result;
}
static uint32_t ue(struct bits *bits) {
    unsigned zeros = 0;
    while (!read_bits(bits, 1)) require(++zeros < 31, "fixture Exp-Golomb overflow");
    return ((UINT32_C(1) << zeros) - 1) + read_bits(bits, zeros);
}
static int32_t se(struct bits *bits) {
    uint32_t value = ue(bits);
    return (value & 1) ? (int32_t)((value + 1) / 2) : -(int32_t)(value / 2);
}
static struct bits rbsp(const uint8_t *stream, struct nal nal) {
    struct bits result = {0};
    unsigned zeros = 0;
    require(nal.length > 1 && nal.length <= MAX_RBSP, "fixture NAL size bound");
    /* Parser storage ONLY. SliceData always uses original escaped bytes below. */
    for (size_t i = 1; i < nal.length; ++i) {
        uint8_t value = stream[nal.offset + i];
        if (zeros == 2 && value == 3) { zeros = 0; continue; }
        result.bytes[result.size++] = value;
        zeros = value == 0 ? zeros + 1 : 0;
    }
    return result;
}
static unsigned split(const uint8_t *stream, size_t size, struct nal nals[MAX_NALS]) {
    unsigned count = 0;
    for (size_t i = 0; i + 3 < size;) {
        unsigned prefix = 0;
        if (stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1) prefix = 3;
        else if (i + 4 < size && stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 0 && stream[i + 3] == 1) prefix = 4;
        if (!prefix) { ++i; continue; }
        require(count < MAX_NALS, "fixture NAL count bound");
        if (count) nals[count - 1].length = i - nals[count - 1].offset;
        nals[count++] = (struct nal){ .start = i, .offset = i + prefix,
            .type = stream[i + prefix] & 31, .ref = (stream[i + prefix] >> 5) & 3 };
        i += prefix + 1;
    }
    require(count > 0, "no Annex B fixture NALs");
    nals[count - 1].length = size - nals[count - 1].offset;
    return count;
}
static void parameters(const uint8_t *stream, const struct nal *nals, unsigned count) {
    require(count == 9 && nals[0].type == 7 && nals[1].type == 8, "expected original SPS/PPS and seven VCLs");
    struct bits s = rbsp(stream, nals[0]);
    require(read_bits(&s, 8) == 66 && read_bits(&s, 8) == 0 && read_bits(&s, 8) == 10,
        "expected Baseline profile66 constraints0 level10");
    require(ue(&s) == 0 && ue(&s) == 0 && ue(&s) == 0 && ue(&s) == 0,
        "expected SPSid0 frame_num4 POCtype0 POClsb4");
    require(ue(&s) == 4 && read_bits(&s, 1) == 0 && ue(&s) == 3 && ue(&s) == 3,
        "expected four references, no gaps, 64x64");
    require(read_bits(&s, 1) == 1 && read_bits(&s, 1) == 1 && read_bits(&s, 1) == 0 && read_bits(&s, 1) == 0,
        "expected progressive4208 direct8x8 no crop/VUI");
    struct bits p = rbsp(stream, nals[1]);
    require(ue(&p) == 0 && ue(&p) == 0 && read_bits(&p, 1) == 0 && read_bits(&p, 1) == 0,
        "expected PPS0 SPS0 CAVLC no bottomPOC");
    require(ue(&p) == 0 && ue(&p) == 0 && ue(&p) == 0 && read_bits(&p, 1) == 0 && read_bits(&p, 2) == 0,
        "expected no FMO one active ref and unweighted prediction");
    require(se(&p) == 0 && se(&p) == 0 && se(&p) == 0 && read_bits(&p, 1) == 1
        && read_bits(&p, 1) == 0 && read_bits(&p, 1) == 0, "unexpected PPS QP/deblock flags");
}
struct slice { unsigned frame, poc, type, header_bits; int qp; unsigned disable_deblock; };
static struct slice parse_slice(const uint8_t *stream, struct nal nal, unsigned frame) {
    struct bits b = rbsp(stream, nal);
    struct slice result = {0};
    require(nal.type == (frame == 0 ? 5u : 1u) && nal.ref != 0, "unexpected NAL reference/type");
    require(ue(&b) == 0, "slice first_mb must be zero");
    result.type = ue(&b);
    require(result.type == (frame == 2 ? 0u : 2u) && ue(&b) == 0, "unexpected I/I/P slice or PPS");
    result.frame = read_bits(&b, 4);
    require(result.frame == frame, "fixture frame_num differs");
    if (frame == 0) require(ue(&b) == 0, "IDRid must be zero");
    result.poc = read_bits(&b, 4);
    require(result.poc == frame * 2, "fixture source POC differs");
    if (frame == 2) {
        require(read_bits(&b, 1) == 0, "unexpected active ref override");
        require(read_bits(&b, 1) == 1 && ue(&b) == 2 && ue(&b) == 1 && ue(&b) == 3,
            "dependent P must select long_term_pic_num1 then terminate L0 modification");
    }
    if (frame == 0) {
        require(read_bits(&b, 1) == 0 && read_bits(&b, 1) == 0, "unexpected IDR marking");
    } else if (frame == 1) {
        require(read_bits(&b, 1) == 1 && ue(&b) == 4 && ue(&b) == 2,
            "expected MMCO4 max_long_term_frame_idx_plus1=2");
        require(ue(&b) == 3 && ue(&b) == 0 && ue(&b) == 1 && ue(&b) == 0,
            "expected MMCO3 difference_of_pic_nums_minus1=0 long_term_frame_idx1 then terminate");
    } else require(read_bits(&b, 1) == 0, "unexpected P marking");
    result.qp = se(&b);
    result.disable_deblock = ue(&b);
    require(result.qp == 0 && result.disable_deblock == 1, "unexpected slice QP/deblocking");
    /* libva specifies unescaped slice_header bits INCLUDING the NAL header. */
    result.header_bits = (unsigned)b.at + 8;
    require(result.header_bits <= UINT16_MAX, "slice header offset overflow");
    if (frame == 2) require(ue(&b) == 16, "dependent P must skip all16 macroblocks");
    return result;
}
static VAPictureH264 picture(VASurfaceID surface, unsigned index, int poc, bool long_term) {
    VAPictureH264 value = {0};
    value.picture_id = surface;
    value.frame_idx = index; /* LT index for long refs; frame_num for short refs. */
    value.flags = long_term ? VA_PICTURE_H264_LONG_TERM_REFERENCE : VA_PICTURE_H264_SHORT_TERM_REFERENCE;
    value.TopFieldOrderCnt = value.BottomFieldOrderCnt = poc;
    return value;
}
static void invalidate(VAPictureH264 *array, unsigned count) {
    for (unsigned i = 0; i < count; ++i) {
        array[i].picture_id = VA_INVALID_SURFACE;
        array[i].flags = VA_PICTURE_H264_INVALID;
    }
}
static void metadata(unsigned frame, const VASurfaceID surfaces[FRAMES], struct slice source,
    VAPictureParameterBufferH264 *pic, VAIQMatrixBufferH264 *iq, VASliceParameterBufferH264 *slice) {
    memset(pic, 0, sizeof(*pic)); memset(iq, 0, sizeof(*iq)); memset(slice, 0, sizeof(*slice));
    pic->CurrPic = picture(surfaces[frame], frame, (int)source.poc, false);
    invalidate(pic->ReferenceFrames, 16);
    if (frame == 1) pic->ReferenceFrames[0] = picture(surfaces[0], 0, 0, false);
    if (frame == 2) {
        pic->ReferenceFrames[0] = picture(surfaces[1], 1, 2, false);
        pic->ReferenceFrames[1] = picture(surfaces[0], 1, 0, true);
    }
    pic->picture_width_in_mbs_minus1 = pic->picture_height_in_mbs_minus1 = 3;
    pic->num_ref_frames = 4;
    pic->seq_fields.bits.chroma_format_idc = 1;
    pic->seq_fields.bits.frame_mbs_only_flag = 1;
    pic->seq_fields.bits.direct_8x8_inference_flag = 1;
    pic->pic_fields.bits.deblocking_filter_control_present_flag = 1;
    pic->pic_fields.bits.reference_pic_flag = 1;
    pic->frame_num = (uint16_t)source.frame;
    memset(iq->ScalingList4x4, 16, sizeof(iq->ScalingList4x4));
    memset(iq->ScalingList8x8, 16, sizeof(iq->ScalingList8x8));
    slice->slice_data_flag = VA_SLICE_DATA_FLAG_ALL;
    slice->slice_data_offset = 0; /* buffer begins at ORIGINAL NAL header */
    slice->slice_data_bit_offset = (uint16_t)source.header_bits;
    slice->slice_type = (uint8_t)source.type;
    slice->slice_qp_delta = (int8_t)source.qp;
    slice->disable_deblocking_filter_idc = (uint8_t)source.disable_deblock;
    invalidate(slice->RefPicList0, 32); invalidate(slice->RefPicList1, 32);
    if (frame == 2) slice->RefPicList0[0] = picture(surfaces[0], 1, 0, true);
}

/* SCM wire version1 is host-endian LOCAL Linux/ARM64, fixed width, no FD numbers.
 * payload: header[magic,version,fourcc,width,height,num_objects,num_layers], then
 * objects[4]{size,reserved,modifier}, then layers[4]{format,num_planes,index[4],
 * offset[4],pitch[4]}. All unused bytes zero. SCM_RIGHTS order == object index.
 * Receiver must ACK one byte 'R' only AFTER GPU sampling and imported-owner
 * retirement; sender retains VA surfaces+context+config+exportFDs until then.
 */
struct export_wire {
    uint32_t magic, version, fourcc, width, height, num_objects, num_layers, reserved;
    struct { uint32_t size, reserved; uint64_t modifier; } objects[4];
    struct { uint32_t format, num_planes, object_index[4], offset[4], pitch[4]; } layers[4];
};
_Static_assert(sizeof(struct export_wire) == 320, "VA export wire v1 layout changed");
static bool handoff(const char *path, const VADRMPRIMESurfaceDescriptor *desc) {
    int socket_fd = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
    if (socket_fd < 0) return false;
    struct sockaddr_un address = { .sun_family = AF_UNIX };
    if (strlen(path) >= sizeof(address.sun_path)) { close(socket_fd); return false; }
    memcpy(address.sun_path, path, strlen(path) + 1);
    if (connect(socket_fd, (struct sockaddr *)&address, sizeof(address)) != 0) { close(socket_fd); return false; }
    struct export_wire payload = { .magic = UINT32_C(0x56415031), .version = 1,
        .fourcc = desc->fourcc, .width = desc->width, .height = desc->height,
        .num_objects = desc->num_objects, .num_layers = desc->num_layers };
    int fds[4];
    for (unsigned i = 0; i < desc->num_objects; ++i) {
        payload.objects[i].size = desc->objects[i].size;
        payload.objects[i].modifier = desc->objects[i].drm_format_modifier;
        fds[i] = desc->objects[i].fd;
    }
    for (unsigned i = 0; i < desc->num_layers; ++i) {
        payload.layers[i].format = desc->layers[i].drm_format;
        payload.layers[i].num_planes = desc->layers[i].num_planes;
        memcpy(payload.layers[i].object_index, desc->layers[i].object_index, sizeof(payload.layers[i].object_index));
        memcpy(payload.layers[i].offset, desc->layers[i].offset, sizeof(payload.layers[i].offset));
        memcpy(payload.layers[i].pitch, desc->layers[i].pitch, sizeof(payload.layers[i].pitch));
    }
    union { struct cmsghdr align; unsigned char bytes[CMSG_SPACE(sizeof(fds))]; } control = {0};
    struct iovec iov = { .iov_base = &payload, .iov_len = sizeof(payload) };
    struct msghdr message = { .msg_iov = &iov, .msg_iovlen = 1,
        .msg_control = control.bytes, .msg_controllen = CMSG_SPACE(sizeof(int) * desc->num_objects) };
    struct cmsghdr *rights = CMSG_FIRSTHDR(&message);
    rights->cmsg_level = SOL_SOCKET; rights->cmsg_type = SCM_RIGHTS;
    rights->cmsg_len = CMSG_LEN(sizeof(int) * desc->num_objects);
    memcpy(CMSG_DATA(rights), fds, sizeof(int) * desc->num_objects);
    ssize_t sent = sendmsg(socket_fd, &message, MSG_NOSIGNAL);
    if (sent != (ssize_t)sizeof(payload)) {
        if (sent > 0) retain("partial export descriptor transfer");
        close(socket_fd); return false;
    }
    fprintf(stderr, "stock_va_handoff sent=true socket=%s objects=%u owner_retained=true waiting_ack=R\n", path, desc->num_objects);
    char ack = 0;
    /* Once receiver has FDs, EOF/error is NOT permission to release VA owners. */
    if (recv(socket_fd, &ack, 1, 0) != 1 || ack != 'R') retain("consumer ACK absent or invalid after descriptor transfer");
    close(socket_fd);
    return true;
}
int main(int argc, char **argv) {
    const char *drm_path = NULL, *stream_path = NULL, *socket_path = NULL;
    bool hold = false;
    for (int i = 1; i < argc; ++i) {
        if (!strcmp(argv[i], "--hold")) hold = true;
        else if (i + 1 < argc && !strcmp(argv[i], "--drm")) drm_path = argv[++i];
        else if (i + 1 < argc && !strcmp(argv[i], "--stream")) stream_path = argv[++i];
        else if (i + 1 < argc && !strcmp(argv[i], "--socket")) socket_path = argv[++i];
        else { fprintf(stderr, "usage: %s --drm PATH --stream ORIGINAL_PCM_MARKING0 [--socket SEQPACKET_PATH | --hold]\n", argv[0]); return 2; }
    }
    require(drm_path && stream_path && !(hold && socket_path), "required paths or conflicting handoff modes");
    FILE *file = fopen(stream_path, "rb"); require(file != NULL, "open original stream");
    uint8_t stream[STREAM_SIZE + 1];
    size_t size = fread(stream, 1, sizeof(stream), file);
    require(!ferror(file) && size == STREAM_SIZE && fgetc(file) == EOF, "original fixture file size differs"); fclose(file);
    require(checksum(stream, size) == STREAM_FNV, "original fixture checksum differs");
    struct nal nals[MAX_NALS]; unsigned count = split(stream, size, nals);
    parameters(stream, nals, count);
    struct slice source[FRAMES];
    for (unsigned i = 0; i < FRAMES; ++i) source[i] = parse_slice(stream, nals[i + 2], i);
    printf("stock_va_source file=%s bytes=%zu fnv1a64=%016" PRIx64 " checked_in_sha256=9e7d2e1f7ca7c636c08532c03945612113c908b57decd75a6f871d36f32040c5 frames=3 original_vcl_bytes=true\n", stream_path, size, checksum(stream, size));
    int drm_fd = open(drm_path, O_RDWR | O_CLOEXEC); require(drm_fd >= 0, "open configured DRM node");
    VADisplay display = vaGetDisplayDRM(drm_fd); require(display != NULL, "vaGetDisplayDRM");
    VAConfigID config = VA_INVALID_ID; VAContextID context = VA_INVALID_ID;
    VASurfaceID surfaces[FRAMES] = { VA_INVALID_SURFACE, VA_INVALID_SURFACE, VA_INVALID_SURFACE };
    VABufferID buffers[4] = { VA_INVALID_ID, VA_INVALID_ID, VA_INVALID_ID, VA_INVALID_ID };
    VADRMPRIMESurfaceDescriptor exported = {0};
    bool initialized = false, created_surfaces = false, exported_ok = false;
    int status = 1, major = 0, minor = 0;
    if (!va_ok(vaInitialize(display, &major, &minor), "vaInitialize")) goto cleanup;
    initialized = true;
    printf("stock_va_runtime libva=%d.%d vendor=%s drm=%s pid=%ld\n", major, minor, vaQueryVendorString(display), drm_path, (long)getpid());
    /* This exact Baseline66 fixture uses only the ConstrainedBaseline subset:
     * progressive CAVLC I/P, no FMO/ASO/redundant slices. Its SPS does not
     * assert constraint_set1; log that actual source fact rather than edit it. */
    VAProfile profiles[64]; int num_profiles = 0;
    require(vaMaxNumProfiles(display) > 0 && vaMaxNumProfiles(display) <= 64, "VA profile count exceeds fixed probe bound");
    if (!va_ok(vaQueryConfigProfiles(display, profiles, &num_profiles), "vaQueryConfigProfiles")) goto cleanup;
    VAProfile selected = VAProfileNone;
    for (int i = 0; i < num_profiles; ++i) if (profiles[i] == VAProfileH264ConstrainedBaseline) selected = profiles[i];
    require(selected != VAProfileNone, "stock driver does not expose ConstrainedBaseline VLD");
    printf("stock_va_config profile=H264ConstrainedBaseline source_profile_idc=66 source_constraint_bits=0 entrypoint=VLD frame_size=64x64 progressive=1 depth=8 chroma=420\n");
    VAConfigAttrib format = { .type = VAConfigAttribRTFormat, .value = 0 };
    if (!va_ok(vaGetConfigAttributes(display, selected, VAEntrypointVLD, &format, 1), "vaGetConfigAttributes")) goto cleanup;
    require(format.value != VA_ATTRIB_NOT_SUPPORTED && (format.value & VA_RT_FORMAT_YUV420), "YUV420 VLD unavailable");
    format.value = VA_RT_FORMAT_YUV420;
    if (!va_ok(vaCreateConfig(display, selected, VAEntrypointVLD, &format, 1, &config), "vaCreateConfig")) goto cleanup;
    VASurfaceAttrib attribute = { .type = VASurfaceAttribPixelFormat, .flags = VA_SURFACE_ATTRIB_SETTABLE,
        .value = { .type = VAGenericValueTypeInteger, .value.i = VA_FOURCC_NV12 } };
    if (!va_ok(vaCreateSurfaces(display, VA_RT_FORMAT_YUV420, 64, 64, surfaces, FRAMES, &attribute, 1), "vaCreateSurfaces")) goto cleanup;
    created_surfaces = true;
    if (!va_ok(vaCreateContext(display, config, 64, 64, VA_PROGRESSIVE, surfaces, FRAMES, &context), "vaCreateContext")) goto cleanup;
    for (unsigned frame = 0; frame < FRAMES; ++frame) {
        VAPictureParameterBufferH264 pic; VAIQMatrixBufferH264 iq; VASliceParameterBufferH264 slice;
        metadata(frame, surfaces, source[frame], &pic, &iq, &slice);
        struct nal nal = nals[frame + 2]; slice.slice_data_size = (uint32_t)nal.length;
        printf("stock_va_picture frame=%u surface=%u frame_num=%u poc=[%d,%d] seq=%08x pic=%08x ref_capacity=4 slice_type=%u slice_bits=%u nal_start=%zu nal_offset=%zu nal_bytes=%zu nal_fnv1a64=%016" PRIx64 " source_rewritten=false iq_flat=16 expected_yuv=[%u,80,180]\n", frame, surfaces[frame], pic.frame_num, pic.CurrPic.TopFieldOrderCnt, pic.CurrPic.BottomFieldOrderCnt, pic.seq_fields.value, pic.pic_fields.value, slice.slice_type, slice.slice_data_bit_offset, nal.start, nal.offset, nal.length, checksum(stream + nal.offset, nal.length), frame == 1 ? 80u : 40u);
        for (unsigned ref = 0; ref < 16; ++ref) if (!(pic.ReferenceFrames[ref].flags & VA_PICTURE_H264_INVALID)) printf("stock_va_dpb frame=%u slot=%u surface=%u index=%u flags=%08x poc=[%d,%d]\n", frame, ref, pic.ReferenceFrames[ref].picture_id, pic.ReferenceFrames[ref].frame_idx, pic.ReferenceFrames[ref].flags, pic.ReferenceFrames[ref].TopFieldOrderCnt, pic.ReferenceFrames[ref].BottomFieldOrderCnt);
        if (frame == 2) printf("stock_va_l0 frame=2 surface=%u long_term_pic_num=1 flags=%08x mmco4_prior=2 mmco3_prior_difference=0 mmco3_prior_long_index=1\n", slice.RefPicList0[0].picture_id, slice.RefPicList0[0].flags);
        if (!va_ok(vaCreateBuffer(display, context, VAPictureParameterBufferType, sizeof(pic), 1, &pic, &buffers[0]), "picture buffer")) goto cleanup;
        if (!va_ok(vaCreateBuffer(display, context, VAIQMatrixBufferType, sizeof(iq), 1, &iq, &buffers[1]), "IQ buffer")) goto cleanup;
        if (!va_ok(vaCreateBuffer(display, context, VASliceParameterBufferType, sizeof(slice), 1, &slice, &buffers[2]), "slice buffer")) goto cleanup;
        if (!va_ok(vaCreateBuffer(display, context, VASliceDataBufferType, (unsigned)nal.length, 1, stream + nal.offset, &buffers[3]), "original VCL buffer")) goto cleanup;
        if (!va_ok(vaBeginPicture(display, context, surfaces[frame]), "vaBeginPicture")) goto cleanup;
        uncertain = true; /* From Begin to genuine Sync success, never free owners on error. */
        va_ok(vaRenderPicture(display, context, buffers, 4), "vaRenderPicture");
        va_ok(vaEndPicture(display, context), "vaEndPicture");
        va_ok(vaSyncSurface(display, surfaces[frame]), "vaSyncSurface");
        uncertain = false;
        printf("stock_va_complete frame=%u actual_sync_surface=true\n", frame); fflush(NULL);
        for (unsigned i = 0; i < 4; ++i) {
            if (!va_ok(vaDestroyBuffer(display, buffers[i]), "vaDestroyBuffer")) goto cleanup;
            buffers[i] = VA_INVALID_ID;
        }
    }
    if (!va_ok(vaExportSurfaceHandle(display, surfaces[2], VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
        VA_EXPORT_SURFACE_READ_WRITE | VA_EXPORT_SURFACE_SEPARATE_LAYERS, &exported), "vaExportSurfaceHandle READ_WRITE PRIME2")) goto cleanup;
    exported_ok = true;
    require(exported.num_objects > 0 && exported.num_objects <= 4 && exported.num_layers > 0 && exported.num_layers <= 4, "export descriptor count invalid");
    printf("stock_va_export fourcc=%08x width=%u height=%u objects=%u layers=%u requested_permissions=READ_WRITE cpu_decoded_pixel_access=false\n", exported.fourcc, exported.width, exported.height, exported.num_objects, exported.num_layers);
    require(exported.width == 64 && exported.height == 64 && exported.fourcc == VA_FOURCC_NV12, "export dimensions/format differ");
    for (unsigned i = 0; i < exported.num_objects; ++i) {
        int flags = fcntl(exported.objects[i].fd, F_GETFL);
        require(flags >= 0 && (flags & O_ACCMODE) == O_RDWR, "export object lacks requested legal READ_WRITE access");
        printf("stock_va_object index=%u fd=%d size=%u modifier=%016" PRIx64 " access_mode=%d\n", i, exported.objects[i].fd, exported.objects[i].size, exported.objects[i].drm_format_modifier, flags & O_ACCMODE);
    }
    for (unsigned i = 0; i < exported.num_layers; ++i) {
        require(exported.layers[i].num_planes > 0 && exported.layers[i].num_planes <= 4, "invalid plane count");
        printf("stock_va_layer index=%u drm_fourcc=%08x planes=%u\n", i, exported.layers[i].drm_format, exported.layers[i].num_planes);
        for (unsigned p = 0; p < exported.layers[i].num_planes; ++p) {
            unsigned object = exported.layers[i].object_index[p];
            require(object < exported.num_objects && exported.layers[i].pitch[p] > 0 && exported.layers[i].offset[p] < exported.objects[object].size, "invalid plane object/range");
            printf("stock_va_plane layer=%u plane=%u object=%u offset=%u pitch=%u\n", i, p, object, exported.layers[i].offset[p], exported.layers[i].pitch[p]);
        }
    }
    fflush(NULL);
    if (socket_path && !handoff(socket_path, &exported)) { fprintf(stderr, "handoff failed before ownership transfer: %s\n", strerror(errno)); goto cleanup; }
    if (hold) {
        printf("STOCK_VA_HOLD pid=%ld release_byte=R owners=3surfaces+context+config+exportFDs\n", (long)getpid()); fflush(NULL);
        char ack = 0;
        if (read(STDIN_FILENO, &ack, 1) != 1 || ack != 'R') retain("stdin release ACK absent or invalid");
    }
    status = 0;
cleanup:
    if (uncertain) retain("cleanup without actual surface retirement");
    if (exported_ok) for (unsigned i = 0; i < exported.num_objects && i < 4; ++i) if (exported.objects[i].fd >= 0) close(exported.objects[i].fd);
    if (context != VA_INVALID_ID) {
        for (unsigned i = 0; i < 4; ++i) if (buffers[i] != VA_INVALID_ID && !va_ok(vaDestroyBuffer(display, buffers[i]), "cleanup buffer")) status = 1;
        if (!va_ok(vaDestroyContext(display, context), "cleanup context")) status = 1;
    }
    if (created_surfaces && !va_ok(vaDestroySurfaces(display, surfaces, FRAMES), "cleanup surfaces")) status = 1;
    if (config != VA_INVALID_ID && !va_ok(vaDestroyConfig(display, config), "cleanup config")) status = 1;
    if (initialized && !va_ok(vaTerminate(display), "vaTerminate")) status = 1;
    close(drm_fd);
    if (status == 0) printf("PASS stock_va_original_vcl frames=3 dependent_long_term_p=true actual_sync=true prime2_export=true gpu_pixel_validation=false cleanup=true\n");
    return status;
}
