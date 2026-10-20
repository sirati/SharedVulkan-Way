/* Stock-Zink preflight comparison. No device, window or memory is created.
 * cc -Wall -Wextra -Werror probe-vk-image-usage.c -lvulkan -o probe-vk-image-usage
 * Run with vulkaninfo's ICD/environment. Compare plain LINEAR against DMA_BUF
 * LINEAR, including input attachment and mutable-format usage candidates.
 */
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int has_extension(VkPhysicalDevice pd, const char *name)
{
    uint32_t n = 0;
    if (vkEnumerateDeviceExtensionProperties(pd, NULL, &n, NULL) != VK_SUCCESS) return 0;
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

int main(void)
{
    VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "image-usage-probe", .apiVersion = VK_API_VERSION_1_1 };
    VkInstanceCreateInfo ci = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &app };
    VkInstance instance;
    VkResult r = vkCreateInstance(&ci, NULL, &instance);
    if (r != VK_SUCCESS) return 2;
    uint32_t n = 1;
    VkPhysicalDevice pd;
    r = vkEnumeratePhysicalDevices(instance, &n, &pd);
    if ((r != VK_SUCCESS && r != VK_INCOMPLETE) || !n) return 2;
    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(pd, &props);
    int have_mod = has_extension(pd, VK_EXT_IMAGE_DRM_FORMAT_MODIFIER_EXTENSION_NAME);
    int have_dma = has_extension(pd, VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME);
    printf("device=%s modifiers=%d dma_buf=%d\n", props.deviceName, have_mod, have_dma);
    const VkFormat formats[] = {VK_FORMAT_B8G8R8A8_UNORM, VK_FORMAT_R8G8B8A8_UNORM};
    const VkImageUsageFlags usages[] = {
        VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_SAMPLED_BIT,
        VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT,
        VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_INPUT_ATTACHMENT_BIT,
        VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_STORAGE_BIT,
    };
    for (unsigned f = 0; f < sizeof(formats)/sizeof(formats[0]); ++f)
        for (unsigned u = 0; u < sizeof(usages)/sizeof(usages[0]); ++u)
            for (unsigned mutable = 0; mutable < 2; ++mutable)
                for (unsigned shape = 0; shape < 4; ++shape) {
                    unsigned external = shape & 1;
                    unsigned modifier = shape >> 1;
                    if ((modifier && !have_mod) || (external && !have_dma)) continue;
                    VkPhysicalDeviceExternalImageFormatInfo ext = {
                        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO,
                        .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
                    VkPhysicalDeviceImageDrmFormatModifierInfoEXT mod = {
                        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_DRM_FORMAT_MODIFIER_INFO_EXT,
                        .pNext = external ? &ext : NULL,
                        .drmFormatModifier = 0, .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
                    VkPhysicalDeviceImageFormatInfo2 info = {
                        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2,
                        .pNext = modifier ? (void *)&mod : external ? (void *)&ext : NULL,
                        .format = formats[f], .type = VK_IMAGE_TYPE_2D,
                        .tiling = modifier ? VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT : VK_IMAGE_TILING_LINEAR, .usage = usages[u],
                        .flags = mutable ? VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT : 0 };
                    VkExternalImageFormatProperties ep = {
                        .sType = VK_STRUCTURE_TYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES };
                    VkImageFormatProperties2 out = {
                        .sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2,
                        .pNext = external ? &ep : NULL };
                    printf("begin format=%d usage=%#x flags=%#x external=%u modifier=%u\n",
                           formats[f], usages[u], info.flags, external, modifier);
                    fflush(stdout);
                    r = vkGetPhysicalDeviceImageFormatProperties2(pd, &info, &out);
                    printf("ok result=%d externalFeatures=%#x\n", r,
                           ep.externalMemoryProperties.externalMemoryFeatures);
                }
    vkDestroyInstance(instance, NULL);
    return 0;
}
