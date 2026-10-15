/*
 * VK_LAYER_SV_intel_shape: make a local driver answer the video queries the
 * way the testhost guest's Intel driver (igvk64.dll 101.7076) answers them,
 * so that an application on top takes the code path it takes on the guest.
 * docs/video-decode.md §7.10, §7.12.
 *
 * Only *answers* change, and only toward less: the driver underneath still has
 * every capability the application is told about, so whatever the application
 * then does is valid on it and the validation layer below this one judges it
 * against the shape it was told.
 *
 * Every codec, from docs/video-decode.md §1's survey of the guest:
 *
 *   - VK_KHR_video_maintenance1 and VK_KHR_video_maintenance2 are not
 *     advertised (Intel has neither). ffmpeg then creates bitstream buffers
 *     against a profile list and, the one that matters, uses a
 *     VkVideoSessionParametersKHR object instead of inline parameters.
 *   - VP9: Intel has no VK_KHR_video_decode_vp9. The extension is not
 *     advertised, the decode family's codec bits lose DECODE_VP9, and a VP9
 *     capability query is VK_ERROR_VIDEO_PROFILE_OPERATION_NOT_SUPPORTED_KHR.
 *   - Every decode codec: both bitstream alignments are raised to 4096, and
 *     DPB_AND_OUTPUT_COINCIDE is cleared. (On the guest H.264 really answers
 *     COINCIDE, §7.11.2; a layer cannot add a capability, and RADV reports
 *     DISTINCT only, so this is RADV's own answer.)
 *   - H.264: maxDpbSlots and maxActiveReferencePictures capped at 16.
 *   - H.265: capped at 15. Main10 answers its capabilities but lists no
 *     formats at all for DST or DPB use (§1 finding 5).
 *   - AV1: capped at 8, which is one fewer than AV1's eight reference names
 *     plus the picture being decoded. A profile with filmGrainSupport is
 *     VK_ERROR_VIDEO_PROFILE_CODEC_NOT_SUPPORTED_KHR, in the capability and
 *     format queries and at vkCreateVideoSessionKHR.
 *   - For experiments: SV_SHAPE_DPB_SLOTS=N replaces the codec's slot cap,
 *     SV_SHAPE_ALIGN=N the 4096 alignment, and SV_SHAPE_KEEP_EXT names hidden
 *     extensions to advertise after all.
 *   - SV_SHAPE_SEPARATE_REFS=0 also clears SEPARATE_REFERENCE_IMAGES, which
 *     makes ffmpeg's dedicated DPB one layered image instead of one image per
 *     picture.
 *
 * Build, inside `nix develop` for the Vulkan headers, into a directory beside
 * the manifest scripts/VkLayer_sv_intel_shape.json:
 *
 *   mkdir -p /tmp/shape && cp scripts/VkLayer_sv_intel_shape.json /tmp/shape/
 *   cc -shared -fPIC -O2 -o /tmp/shape/libVkLayer_sv_intel_shape.so scripts/vk-intel-shape-layer.c
 *   SVFD_GPU_LAYER_PATH=/tmp/shape SVFD_GPU_LAYERS=VK_LAYER_SV_intel_shape \
 *     scripts/sv-firefox-vulkan-decode.sh 140
 */
#include <vulkan/vulkan.h>
#include <vulkan/vk_layer.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define EXPORT __attribute__((visibility("default")))

static PFN_vkGetInstanceProcAddr next_gipa;
static PFN_vkGetDeviceProcAddr next_gdpa;
static PFN_vkEnumerateDeviceExtensionProperties next_enum_dev_ext;
static PFN_vkGetPhysicalDeviceVideoCapabilitiesKHR next_video_caps;
static PFN_vkGetPhysicalDeviceVideoFormatPropertiesKHR next_video_formats;
static PFN_vkGetPhysicalDeviceQueueFamilyProperties2 next_qfp2;
static PFN_vkGetPhysicalDeviceQueueFamilyProperties2 next_qfp2_khr;
static PFN_vkCreateVideoSessionKHR next_create_session;

static const char *hidden[] = {"VK_KHR_video_maintenance1", "VK_KHR_video_maintenance2",
                               "VK_KHR_video_decode_vp9"};

static int is_hidden(const char *name) {
    const char *keep = getenv("SV_SHAPE_KEEP_EXT");
    if (keep && strstr(keep, name))
        return 0;
    for (size_t i = 0; i < sizeof hidden / sizeof hidden[0]; i++)
        if (!strcmp(name, hidden[i]))
            return 1;
    return 0;
}

static const void *find(const void *chain, VkStructureType t) {
    for (const VkBaseInStructure *s = chain; s; s = s->pNext)
        if (s->sType == t)
            return s;
    return NULL;
}

/* What Intel answers for this profile, before any capability is read:
 * VK_SUCCESS, or the error its query returns. */
static VkResult refused(const VkVideoProfileInfoKHR *p) {
    switch (p->videoCodecOperation) {
    case VK_VIDEO_CODEC_OPERATION_DECODE_VP9_BIT_KHR:
        return VK_ERROR_VIDEO_PROFILE_OPERATION_NOT_SUPPORTED_KHR;
    case VK_VIDEO_CODEC_OPERATION_DECODE_AV1_BIT_KHR: {
        const VkVideoDecodeAV1ProfileInfoKHR *a =
            find(p->pNext, VK_STRUCTURE_TYPE_VIDEO_DECODE_AV1_PROFILE_INFO_KHR);
        if (a && a->filmGrainSupport)
            return VK_ERROR_VIDEO_PROFILE_CODEC_NOT_SUPPORTED_KHR;
        return VK_SUCCESS;
    }
    default:
        return VK_SUCCESS;
    }
}

static int h265_10bit(const VkVideoProfileInfoKHR *p) {
    return p->videoCodecOperation == VK_VIDEO_CODEC_OPERATION_DECODE_H265_BIT_KHR &&
           p->lumaBitDepth != VK_VIDEO_COMPONENT_BIT_DEPTH_8_BIT_KHR;
}

static const char *codec_name(VkVideoCodecOperationFlagBitsKHR op) {
    switch (op) {
    case VK_VIDEO_CODEC_OPERATION_DECODE_H264_BIT_KHR: return "h264";
    case VK_VIDEO_CODEC_OPERATION_DECODE_H265_BIT_KHR: return "h265";
    case VK_VIDEO_CODEC_OPERATION_DECODE_AV1_BIT_KHR: return "av1";
    case VK_VIDEO_CODEC_OPERATION_DECODE_VP9_BIT_KHR: return "vp9";
    default: return "other";
    }
}

static VKAPI_ATTR VkResult VKAPI_CALL shape_EnumerateDeviceExtensionProperties(
    VkPhysicalDevice pd, const char *layer, uint32_t *count, VkExtensionProperties *props) {
    if (layer && strcmp(layer, "VK_LAYER_SV_intel_shape"))
        return next_enum_dev_ext(pd, layer, count, props);
    if (layer) {
        *count = 0;
        return VK_SUCCESS;
    }
    uint32_t n = 0;
    VkResult r = next_enum_dev_ext(pd, NULL, &n, NULL);
    if (r != VK_SUCCESS)
        return r;
    VkExtensionProperties *all = calloc(n ? n : 1, sizeof *all);
    r = next_enum_dev_ext(pd, NULL, &n, all);
    if (r < 0) {
        free(all);
        return r;
    }
    uint32_t kept = 0;
    for (uint32_t i = 0; i < n; i++)
        if (!is_hidden(all[i].extensionName))
            all[kept++] = all[i];
    if (!props) {
        *count = kept;
        free(all);
        return VK_SUCCESS;
    }
    uint32_t w = *count < kept ? *count : kept;
    memcpy(props, all, w * sizeof *all);
    *count = w;
    free(all);
    return w < kept ? VK_INCOMPLETE : VK_SUCCESS;
}

static void mask_families(uint32_t *count, VkQueueFamilyProperties2 *props) {
    if (!props)
        return;
    for (uint32_t i = 0; i < *count; i++) {
        VkQueueFamilyVideoPropertiesKHR *v = (VkQueueFamilyVideoPropertiesKHR *)find(
            props[i].pNext, VK_STRUCTURE_TYPE_QUEUE_FAMILY_VIDEO_PROPERTIES_KHR);
        if (v)
            v->videoCodecOperations &= ~VK_VIDEO_CODEC_OPERATION_DECODE_VP9_BIT_KHR;
    }
}

static VKAPI_ATTR void VKAPI_CALL shape_GetPhysicalDeviceQueueFamilyProperties2(
    VkPhysicalDevice pd, uint32_t *count, VkQueueFamilyProperties2 *props) {
    next_qfp2(pd, count, props);
    mask_families(count, props);
}

static VKAPI_ATTR void VKAPI_CALL shape_GetPhysicalDeviceQueueFamilyProperties2KHR(
    VkPhysicalDevice pd, uint32_t *count, VkQueueFamilyProperties2 *props) {
    (next_qfp2_khr ? next_qfp2_khr : next_qfp2)(pd, count, props);
    mask_families(count, props);
}

static VKAPI_ATTR VkResult VKAPI_CALL shape_GetPhysicalDeviceVideoCapabilitiesKHR(
    VkPhysicalDevice pd, const VkVideoProfileInfoKHR *profile, VkVideoCapabilitiesKHR *caps) {
    VkResult no = refused(profile);
    if (no != VK_SUCCESS)
        return no;
    VkResult r = next_video_caps(pd, profile, caps);
    if (r != VK_SUCCESS)
        return r;
    uint32_t slots;
    switch (profile->videoCodecOperation) {
    case VK_VIDEO_CODEC_OPERATION_DECODE_H264_BIT_KHR: slots = 16; break;
    case VK_VIDEO_CODEC_OPERATION_DECODE_H265_BIT_KHR: slots = 15; break;
    case VK_VIDEO_CODEC_OPERATION_DECODE_AV1_BIT_KHR: slots = 8; break;
    default: return r;
    }
    const char *over = getenv("SV_SHAPE_DPB_SLOTS");
    if (over && *over)
        slots = (uint32_t)atoi(over);
    const char *al = getenv("SV_SHAPE_ALIGN");
    uint32_t align = al && *al ? (uint32_t)atoi(al) : 4096;
    const char *sep = getenv("SV_SHAPE_SEPARATE_REFS");
    VkVideoCapabilityFlagsKHR before = caps->flags;
    if (sep && !strcmp(sep, "0"))
        caps->flags &= ~VK_VIDEO_CAPABILITY_SEPARATE_REFERENCE_IMAGES_BIT_KHR;
    if (caps->maxDpbSlots > slots)
        caps->maxDpbSlots = slots;
    if (caps->maxActiveReferencePictures > slots)
        caps->maxActiveReferencePictures = slots;
    if (caps->minBitstreamBufferOffsetAlignment < align)
        caps->minBitstreamBufferOffsetAlignment = align;
    if (caps->minBitstreamBufferSizeAlignment < align)
        caps->minBitstreamBufferSizeAlignment = align;
    for (VkBaseOutStructure *s = caps->pNext; s; s = s->pNext) {
        if (s->sType == VK_STRUCTURE_TYPE_VIDEO_DECODE_CAPABILITIES_KHR) {
            VkVideoDecodeCapabilitiesKHR *d = (VkVideoDecodeCapabilitiesKHR *)s;
            fprintf(stderr, "[intel-shape] %s decode caps: flags 0x%x -> 0x%x, decode flags 0x%x -> 0x%x, dpb slots %u\n",
                    codec_name(profile->videoCodecOperation), before, caps->flags, d->flags,
                    d->flags & ~VK_VIDEO_DECODE_CAPABILITY_DPB_AND_OUTPUT_COINCIDE_BIT_KHR, caps->maxDpbSlots);
            d->flags &= ~VK_VIDEO_DECODE_CAPABILITY_DPB_AND_OUTPUT_COINCIDE_BIT_KHR;
        }
    }
    return r;
}

static VKAPI_ATTR VkResult VKAPI_CALL shape_GetPhysicalDeviceVideoFormatPropertiesKHR(
    VkPhysicalDevice pd, const VkPhysicalDeviceVideoFormatInfoKHR *info, uint32_t *count,
    VkVideoFormatPropertiesKHR *props) {
    const VkVideoProfileListInfoKHR *list = find(info->pNext, VK_STRUCTURE_TYPE_VIDEO_PROFILE_LIST_INFO_KHR);
    for (uint32_t i = 0; list && i < list->profileCount; i++) {
        VkResult no = refused(&list->pProfiles[i]);
        if (no != VK_SUCCESS)
            return no;
        if (h265_10bit(&list->pProfiles[i])) {
            fprintf(stderr, "[intel-shape] h265 10-bit: no formats, as Intel 101.7076\n");
            *count = 0;
            return VK_SUCCESS;
        }
    }
    return next_video_formats(pd, info, count, props);
}

static VKAPI_ATTR VkResult VKAPI_CALL shape_CreateVideoSessionKHR(VkDevice d, const VkVideoSessionCreateInfoKHR *ci,
                                                                  const VkAllocationCallbacks *a,
                                                                  VkVideoSessionKHR *out) {
    VkResult no = refused(ci->pVideoProfile);
    if (no != VK_SUCCESS) {
        fprintf(stderr, "[intel-shape] vkCreateVideoSessionKHR refused (%s), as Intel 101.7076\n",
                codec_name(ci->pVideoProfile->videoCodecOperation));
        return no;
    }
    return next_create_session(d, ci, a, out);
}

static VKAPI_ATTR VkResult VKAPI_CALL shape_CreateInstance(const VkInstanceCreateInfo *ci,
                                                           const VkAllocationCallbacks *a,
                                                           VkInstance *out) {
    VkLayerInstanceCreateInfo *li = (VkLayerInstanceCreateInfo *)ci->pNext;
    while (li && !(li->sType == VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO && li->function == VK_LAYER_LINK_INFO))
        li = (VkLayerInstanceCreateInfo *)li->pNext;
    if (!li)
        return VK_ERROR_INITIALIZATION_FAILED;
    next_gipa = li->u.pLayerInfo->pfnNextGetInstanceProcAddr;
    li->u.pLayerInfo = li->u.pLayerInfo->pNext;
    PFN_vkCreateInstance create = (PFN_vkCreateInstance)next_gipa(NULL, "vkCreateInstance");
    VkResult r = create(ci, a, out);
    if (r != VK_SUCCESS)
        return r;
    next_enum_dev_ext = (PFN_vkEnumerateDeviceExtensionProperties)next_gipa(*out, "vkEnumerateDeviceExtensionProperties");
    next_video_caps = (PFN_vkGetPhysicalDeviceVideoCapabilitiesKHR)next_gipa(*out, "vkGetPhysicalDeviceVideoCapabilitiesKHR");
    next_video_formats =
        (PFN_vkGetPhysicalDeviceVideoFormatPropertiesKHR)next_gipa(*out, "vkGetPhysicalDeviceVideoFormatPropertiesKHR");
    next_qfp2 = (PFN_vkGetPhysicalDeviceQueueFamilyProperties2)next_gipa(*out, "vkGetPhysicalDeviceQueueFamilyProperties2");
    next_qfp2_khr =
        (PFN_vkGetPhysicalDeviceQueueFamilyProperties2)next_gipa(*out, "vkGetPhysicalDeviceQueueFamilyProperties2KHR");
    fprintf(stderr, "[intel-shape] active: video_maintenance1/2 and VP9 hidden, "
                    "H.264/H.265/AV1 decode shaped as Intel 101.7076\n");
    return r;
}

static VKAPI_ATTR VkResult VKAPI_CALL shape_CreateDevice(VkPhysicalDevice pd, const VkDeviceCreateInfo *ci,
                                                         const VkAllocationCallbacks *a, VkDevice *out) {
    VkLayerDeviceCreateInfo *li = (VkLayerDeviceCreateInfo *)ci->pNext;
    while (li && !(li->sType == VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO && li->function == VK_LAYER_LINK_INFO))
        li = (VkLayerDeviceCreateInfo *)li->pNext;
    if (!li)
        return VK_ERROR_INITIALIZATION_FAILED;
    PFN_vkGetInstanceProcAddr gipa = li->u.pLayerInfo->pfnNextGetInstanceProcAddr;
    next_gdpa = li->u.pLayerInfo->pfnNextGetDeviceProcAddr;
    li->u.pLayerInfo = li->u.pLayerInfo->pNext;
    PFN_vkCreateDevice create = (PFN_vkCreateDevice)gipa(NULL, "vkCreateDevice");
    for (uint32_t i = 0; i < ci->enabledExtensionCount; i++)
        if (is_hidden(ci->ppEnabledExtensionNames[i]))
            fprintf(stderr, "[intel-shape] WARNING: %s enabled although hidden\n", ci->ppEnabledExtensionNames[i]);
    VkResult r = create(pd, ci, a, out);
    if (r == VK_SUCCESS)
        next_create_session = (PFN_vkCreateVideoSessionKHR)next_gdpa(*out, "vkCreateVideoSessionKHR");
    return r;
}

static VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL shape_GetDeviceProcAddr(VkDevice d, const char *name) {
    if (!strcmp(name, "vkCreateVideoSessionKHR"))
        return next_gdpa && next_gdpa(d, name) ? (PFN_vkVoidFunction)shape_CreateVideoSessionKHR : NULL;
    if (!strcmp(name, "vkGetDeviceProcAddr"))
        return (PFN_vkVoidFunction)shape_GetDeviceProcAddr;
    return next_gdpa ? next_gdpa(d, name) : NULL;
}

static VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL shape_GetInstanceProcAddr(VkInstance inst, const char *name) {
    if (!strcmp(name, "vkGetInstanceProcAddr"))
        return (PFN_vkVoidFunction)shape_GetInstanceProcAddr;
    if (!strcmp(name, "vkCreateInstance"))
        return (PFN_vkVoidFunction)shape_CreateInstance;
    if (!strcmp(name, "vkCreateDevice"))
        return (PFN_vkVoidFunction)shape_CreateDevice;
    if (!strcmp(name, "vkGetDeviceProcAddr"))
        return (PFN_vkVoidFunction)shape_GetDeviceProcAddr;
    if (!strcmp(name, "vkCreateVideoSessionKHR"))
        return (PFN_vkVoidFunction)shape_CreateVideoSessionKHR;
    if (!strcmp(name, "vkEnumerateDeviceExtensionProperties"))
        return (PFN_vkVoidFunction)shape_EnumerateDeviceExtensionProperties;
    if (!strcmp(name, "vkGetPhysicalDeviceVideoCapabilitiesKHR"))
        return (PFN_vkVoidFunction)shape_GetPhysicalDeviceVideoCapabilitiesKHR;
    if (!strcmp(name, "vkGetPhysicalDeviceVideoFormatPropertiesKHR"))
        return (PFN_vkVoidFunction)shape_GetPhysicalDeviceVideoFormatPropertiesKHR;
    if (!strcmp(name, "vkGetPhysicalDeviceQueueFamilyProperties2"))
        return (PFN_vkVoidFunction)shape_GetPhysicalDeviceQueueFamilyProperties2;
    if (!strcmp(name, "vkGetPhysicalDeviceQueueFamilyProperties2KHR"))
        return (PFN_vkVoidFunction)shape_GetPhysicalDeviceQueueFamilyProperties2KHR;
    return next_gipa ? next_gipa(inst, name) : NULL;
}

EXPORT VKAPI_ATTR VkResult VKAPI_CALL vkNegotiateLoaderLayerInterfaceVersion(VkNegotiateLayerInterface *v) {
    if (v->loaderLayerInterfaceVersion > 2)
        v->loaderLayerInterfaceVersion = 2;
    v->pfnGetInstanceProcAddr = shape_GetInstanceProcAddr;
    v->pfnGetDeviceProcAddr = shape_GetDeviceProcAddr;
    v->pfnGetPhysicalDeviceProcAddr = NULL;
    return VK_SUCCESS;
}
