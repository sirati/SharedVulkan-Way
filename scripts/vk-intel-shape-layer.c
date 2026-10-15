/*
 * VK_LAYER_SV_intel_shape: make a local driver answer the video queries the
 * way the testhost guest's Intel driver (igvk64.dll 101.7076) answers them,
 * so that an application on top takes the code path it takes on the guest.
 * docs/video-decode.md §7.10.
 *
 * Only *answers* change, and only toward less: the driver underneath still has
 * every capability the application is told about, so whatever the application
 * then does is valid on it and the validation layer below this one judges it
 * against the shape it was told.
 *
 *   - VK_KHR_video_maintenance1 and VK_KHR_video_maintenance2 are not
 *     advertised (Intel has neither). ffmpeg then creates bitstream buffers
 *     against a profile list and, the one that matters, uses a
 *     VkVideoSessionParametersKHR object instead of inline SPS/PPS.
 *   - H.264 decode: DPB_AND_OUTPUT_COINCIDE is cleared (Intel reports
 *     DPB_AND_OUTPUT_DISTINCT only), so ffmpeg allocates a dedicated DPB;
 *     maxDpbSlots and maxActiveReferencePictures are capped at 16; both
 *     bitstream alignments are raised to 4096.
 *   - SV_SHAPE_SEPARATE_REFS=0 also clears SEPARATE_REFERENCE_IMAGES, which
 *     makes ffmpeg's dedicated DPB one layered image instead of one image per
 *     picture. Intel's value of this bit was never recorded, so both are run.
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

static const char *hidden[] = {"VK_KHR_video_maintenance1", "VK_KHR_video_maintenance2"};

static int is_hidden(const char *name) {
    for (size_t i = 0; i < sizeof hidden / sizeof hidden[0]; i++)
        if (!strcmp(name, hidden[i]))
            return 1;
    return 0;
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

static VKAPI_ATTR VkResult VKAPI_CALL shape_GetPhysicalDeviceVideoCapabilitiesKHR(
    VkPhysicalDevice pd, const VkVideoProfileInfoKHR *profile, VkVideoCapabilitiesKHR *caps) {
    VkResult r = next_video_caps(pd, profile, caps);
    if (r != VK_SUCCESS || profile->videoCodecOperation != VK_VIDEO_CODEC_OPERATION_DECODE_H264_BIT_KHR)
        return r;
    const char *sep = getenv("SV_SHAPE_SEPARATE_REFS");
    VkVideoCapabilityFlagsKHR before = caps->flags;
    if (sep && !strcmp(sep, "0"))
        caps->flags &= ~VK_VIDEO_CAPABILITY_SEPARATE_REFERENCE_IMAGES_BIT_KHR;
    if (caps->maxDpbSlots > 16)
        caps->maxDpbSlots = 16;
    if (caps->maxActiveReferencePictures > 16)
        caps->maxActiveReferencePictures = 16;
    if (caps->minBitstreamBufferOffsetAlignment < 4096)
        caps->minBitstreamBufferOffsetAlignment = 4096;
    if (caps->minBitstreamBufferSizeAlignment < 4096)
        caps->minBitstreamBufferSizeAlignment = 4096;
    for (VkBaseOutStructure *s = caps->pNext; s; s = s->pNext) {
        if (s->sType == VK_STRUCTURE_TYPE_VIDEO_DECODE_CAPABILITIES_KHR) {
            VkVideoDecodeCapabilitiesKHR *d = (VkVideoDecodeCapabilitiesKHR *)s;
            fprintf(stderr, "[intel-shape] h264 decode caps: flags 0x%x -> 0x%x, decode flags 0x%x -> 0x%x, dpb slots %u\n",
                    before, caps->flags, d->flags,
                    d->flags & ~VK_VIDEO_DECODE_CAPABILITY_DPB_AND_OUTPUT_COINCIDE_BIT_KHR, caps->maxDpbSlots);
            d->flags &= ~VK_VIDEO_DECODE_CAPABILITY_DPB_AND_OUTPUT_COINCIDE_BIT_KHR;
        }
    }
    return r;
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
    fprintf(stderr, "[intel-shape] active: video_maintenance1/2 hidden, H.264 decode shaped as Intel 101.7076\n");
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
    return create(pd, ci, a, out);
}

static VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL shape_GetDeviceProcAddr(VkDevice d, const char *name) {
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
    if (!strcmp(name, "vkEnumerateDeviceExtensionProperties"))
        return (PFN_vkVoidFunction)shape_EnumerateDeviceExtensionProperties;
    if (!strcmp(name, "vkGetPhysicalDeviceVideoCapabilitiesKHR"))
        return (PFN_vkVoidFunction)shape_GetPhysicalDeviceVideoCapabilitiesKHR;
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
