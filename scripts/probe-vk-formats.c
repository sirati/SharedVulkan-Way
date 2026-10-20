/* Offline-built format-chain probe. Run with the same ICD/environment as
 * vulkaninfo. Every call is flushed before entering the driver, so the final
 * "begin" line identifies a crash. No device, window or GPU memory is created.
 *
 * cc -Wall -Wextra -Werror probe-vk-formats.c -lvulkan -o probe-vk-formats
 * probe-vk-formats [format [chain]]
 * chain: 0=legacy, 1=properties2, 2=properties3, 3=modifier list,
 *        4=modifier list2, 5=properties3 + both modifier lists,
 *        6=subpass resolve performance, 7=properties3 + resolve performance.
 * With no arguments, sweep core, YCbCr, ASTC HDR and PVRTC format values.
 * Only advertised extension chains or promoted core structures are queried.
 */
#include <vulkan/vulkan.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int has_extension(VkPhysicalDevice pd, const char *name)
{
    uint32_t n = 0;
    if (vkEnumerateDeviceExtensionProperties(pd, NULL, &n, NULL) != VK_SUCCESS)
        return 0;
    VkExtensionProperties *p = calloc(n ? n : 1, sizeof(*p));
    if (!p) return 0;
    VkResult r = vkEnumerateDeviceExtensionProperties(pd, NULL, &n, p);
    int found = 0;
    if (r == VK_SUCCESS)
        for (uint32_t i = 0; i < n; ++i)
            if (!strcmp(p[i].extensionName, name)) found = 1;
    free(p);
    return found;
}

static void query(VkPhysicalDevice pd, int format, unsigned chain)
{
    VkFormatProperties3 p3 = { .sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_3 };
    VkDrmFormatModifierPropertiesListEXT m1 = {
        .sType = VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT };
    VkDrmFormatModifierPropertiesList2EXT m2 = {
        .sType = VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_2_EXT };
    VkSubpassResolvePerformanceQueryEXT resolve = {
        .sType = VK_STRUCTURE_TYPE_SUBPASS_RESOLVE_PERFORMANCE_QUERY_EXT };
    VkFormatProperties2 p = { .sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2 };
    void **next = &p.pNext;
    if (chain == 2 || chain == 5 || chain == 7) { *next = &p3; next = &p3.pNext; }
    if (chain == 3 || chain == 5) { *next = &m1; next = &m1.pNext; }
    if (chain == 4 || chain == 5) { *next = &m2; next = &m2.pNext; }
    if (chain == 6 || chain == 7) { *next = &resolve; next = &resolve.pNext; }
    (void)next;
    printf("begin format=%d chain=%u phase=count\n", format, chain);
    fflush(stdout);
    if (!chain) vkGetPhysicalDeviceFormatProperties(pd, (VkFormat)format, &p.formatProperties);
    else vkGetPhysicalDeviceFormatProperties2(pd, (VkFormat)format, &p);
    printf("ok linear=%#x optimal=%#x linear3=%#" PRIx64 " count1=%u count2=%u\n",
           p.formatProperties.linearTilingFeatures, p.formatProperties.optimalTilingFeatures,
           (uint64_t)p3.linearTilingFeatures, m1.drmFormatModifierCount, m2.drmFormatModifierCount);
    if (chain < 3 || chain > 5) return;
    /* Preserve capacities, including zero, and test the fill call separately.
     * Refuse absurd driver counts rather than allowing an unbounded allocation. */
    if (m1.drmFormatModifierCount > 4096 || m2.drmFormatModifierCount > 4096) {
        fprintf(stderr, "invalid modifier count\n"); exit(3);
    }
    m1.pDrmFormatModifierProperties = calloc(m1.drmFormatModifierCount ? m1.drmFormatModifierCount : 1,
                                            sizeof(*m1.pDrmFormatModifierProperties));
    m2.pDrmFormatModifierProperties = calloc(m2.drmFormatModifierCount ? m2.drmFormatModifierCount : 1,
                                            sizeof(*m2.pDrmFormatModifierProperties));
    if (!m1.pDrmFormatModifierProperties || !m2.pDrmFormatModifierProperties) exit(3);
    printf("begin format=%d chain=%u phase=fill\n", format, chain);
    fflush(stdout);
    vkGetPhysicalDeviceFormatProperties2(pd, (VkFormat)format, &p);
    printf("ok filled1=%u filled2=%u\n", m1.drmFormatModifierCount, m2.drmFormatModifierCount);
    free(m1.pDrmFormatModifierProperties);
    free(m2.pDrmFormatModifierProperties);
}

int main(int argc, char **argv)
{
    uint32_t version = VK_API_VERSION_1_0;
    vkEnumerateInstanceVersion(&version);
    if (version < VK_API_VERSION_1_1) return 2;
    VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "format-probe", .apiVersion = version < VK_API_VERSION_1_3 ? version : VK_API_VERSION_1_3 };
    VkInstanceCreateInfo ci = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app };
    VkInstance instance;
    VkResult r = vkCreateInstance(&ci, NULL, &instance);
    if (r != VK_SUCCESS) { fprintf(stderr, "vkCreateInstance=%d\n", r); return 2; }
    uint32_t n = 1;
    VkPhysicalDevice pd;
    r = vkEnumeratePhysicalDevices(instance, &n, &pd);
    if ((r != VK_SUCCESS && r != VK_INCOMPLETE) || !n) { vkDestroyInstance(instance, NULL); return 2; }
    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(pd, &props);
    int mod = has_extension(pd, VK_EXT_IMAGE_DRM_FORMAT_MODIFIER_EXTENSION_NAME);
    int p3 = props.apiVersion >= VK_API_VERSION_1_3 ||
        has_extension(pd, VK_KHR_FORMAT_FEATURE_FLAGS_2_EXTENSION_NAME);
    int resolve = has_extension(pd, VK_EXT_MULTISAMPLED_RENDER_TO_SINGLE_SAMPLED_EXTENSION_NAME);
    printf("device=%s api=%#x modifiers=%d properties3=%d\n", props.deviceName, props.apiVersion, mod, p3);
    if (argc > 1) {
        unsigned chain = argc > 2 ? (unsigned)strtoul(argv[2], NULL, 0) : 1;
        if (chain > 7 || ((chain == 2 || chain == 5 || chain == 7) && !p3) ||
            (chain >= 3 && chain <= 5 && !mod) || (chain >= 6 && !resolve)) return 2;
        query(pd, (int)strtol(argv[1], NULL, 0), chain);
    } else {
        const int ranges[][2] = {{0,184}, {1000156000,1000156033},
                                 {1000066000,1000066013}, {1000054000,1000054007}};
        for (size_t i = 0; i < sizeof(ranges)/sizeof(ranges[0]); ++i)
            for (int f = ranges[i][0]; f <= ranges[i][1]; ++f)
                for (unsigned chain = 0; chain <= 7; ++chain) {
                    if ((chain == 2 || chain == 5 || chain == 7) && !p3) continue;
                    if (chain >= 3 && chain <= 5 && !mod) continue;
                    if (chain >= 6 && !resolve) continue;
                    query(pd, f, chain);
                }
    }
    vkDestroyInstance(instance, NULL);
    return 0;
}
