/* vk-present-probe — a Vulkan program that presents, and the frame is checked.
 *
 * docs/design.md §10 stage 2's acceptance criterion, written as a program: put a
 * known image into a swapchain image on the GPU side, present it, and see the
 * same bytes come out on the app side.
 *
 * WHY IT UPLOADS RATHER THAN DRAWS
 *
 * `gl-probe` and `gl4-probe` already establish that a real graphics pipeline
 * produces the right pixels across this boundary, to 64.1 dB against llvmpipe.
 * Repeating that here would measure the thing that already works. What is new is
 * **presentation**, and the sharpest instrument for it is a frame whose expected
 * value is known exactly rather than to within a rounding error: a pattern
 * written by the CPU into a host-visible buffer, copied into the swapchain image
 * by the GPU, presented, copied back into the arena by the ICD's own present
 * path, and compared **bit for bit**. A single wrong byte is a failure, where a
 * rendered frame can only ever be compared to a tolerance.
 *
 * The pattern is chosen so that every way of getting presentation wrong looks
 * different from every other:
 *
 *   - a per-pixel value derived from both x and y, so a wrong row stride shears
 *     it and a transposed copy is obvious;
 *   - four quadrants with different dominant channels, so a channel swap is not
 *     a subtle shift but a wrong colour;
 *   - a one-pixel border and a distinct first row, so an off-by-one origin or a
 *     vertical flip shows at a glance;
 *   - a frame counter baked into the top-left pixel, so presenting the *same*
 *     image twice — which a broken acquire would do — is caught rather than
 *     passing twice.
 *
 * WHAT IT PROVES
 *
 * That `vkAcquireNextImageKHR` hands out a usable image and signals what it was
 * asked to signal; that `vkQueuePresentKHR` waits for the application's
 * rendering; that the GPU-side copy lands in arena memory the app side has
 * mapped; and that the bytes are the ones that went in. It does **not** prove
 * anything about a window, which is what `glxgears` under Xvfb is for.
 *
 * Build (from the dev shell):
 *   cc -O1 -o vk-present-probe scripts/vk-present-probe.c -lvulkan -I$VULKAN_SDK/include
 *
 * Run:
 *   SV_PRESENT_DUMP=/tmp/frame- VK_PRESENT_PROBE_EXPECT=/tmp/expect- ./vk-present-probe
 *
 * It writes `<expect->NNNN.ppm` for each frame it presented; the ICD writes
 * `<SV_PRESENT_DUMP>NNNN.ppm`, and the two must be identical.
 */
#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <dlfcn.h>

/* Overridable so that the same program is both the correctness check (small,
 * three frames, compared byte for byte) and the cost measurement docs/design.md §10
 * asks for (1080p, many frames, timed). `SV_PRESENT_DUMP` should be unset for
 * the second. */
#define MAX_FRAMES 512
static unsigned W = 256, H = 256, FRAMES = 3;

static unsigned env_u(const char *name, unsigned dflt) {
    const char *v = getenv(name);
    if (!v || !*v)
        return dflt;
    unsigned long n = strtoul(v, NULL, 10);
    return n ? (unsigned)n : dflt;
}

static double now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e3 + t.tv_nsec / 1e6;
}

static void *loader;
#define VKFN(name) static PFN_##name name
VKFN(vkGetInstanceProcAddr);
VKFN(vkCreateInstance);
VKFN(vkDestroyInstance);
VKFN(vkEnumeratePhysicalDevices);
VKFN(vkGetPhysicalDeviceQueueFamilyProperties);
VKFN(vkGetPhysicalDeviceMemoryProperties);
VKFN(vkCreateDevice);
VKFN(vkDestroyDevice);
VKFN(vkGetDeviceProcAddr);
VKFN(vkGetDeviceQueue);
VKFN(vkGetPhysicalDeviceSurfaceSupportKHR);
VKFN(vkGetPhysicalDeviceSurfaceCapabilitiesKHR);
VKFN(vkGetPhysicalDeviceSurfaceFormatsKHR);
VKFN(vkGetPhysicalDeviceSurfacePresentModesKHR);
VKFN(vkCreateHeadlessSurfaceEXT);
VKFN(vkDestroySurfaceKHR);
VKFN(vkCreateSwapchainKHR);
VKFN(vkDestroySwapchainKHR);
VKFN(vkGetSwapchainImagesKHR);
VKFN(vkAcquireNextImageKHR);
VKFN(vkQueuePresentKHR);
VKFN(vkCreateBuffer);
VKFN(vkDestroyBuffer);
VKFN(vkGetBufferMemoryRequirements);
VKFN(vkAllocateMemory);
VKFN(vkFreeMemory);
VKFN(vkBindBufferMemory);
VKFN(vkMapMemory);
VKFN(vkCreateCommandPool);
VKFN(vkDestroyCommandPool);
VKFN(vkAllocateCommandBuffers);
VKFN(vkBeginCommandBuffer);
VKFN(vkEndCommandBuffer);
VKFN(vkCmdPipelineBarrier);
VKFN(vkCmdCopyBufferToImage);
VKFN(vkQueueSubmit);
VKFN(vkQueueWaitIdle);
VKFN(vkDeviceWaitIdle);
VKFN(vkCreateSemaphore);
VKFN(vkDestroySemaphore);

#define CHECK(expr)                                                                      \
    do {                                                                                 \
        VkResult _r = (expr);                                                            \
        if (_r != VK_SUCCESS) {                                                          \
            printf("FAIL: %s -> %d\n", #expr, (int)_r);                                  \
            return 1;                                                                    \
        }                                                                                \
    } while (0)

/* The pattern, as BGRA. `frame` is baked into the first pixel so that two
 * presents of the same contents cannot both pass. */
static void pattern(unsigned char *px, unsigned frame) {
    for (unsigned y = 0; y < H; y++) {
        for (unsigned x = 0; x < W; x++) {
            unsigned char *p = px + ((size_t)y * W + x) * 4;
            int border = (x == 0 || y == 0 || x == W - 1 || y == H - 1);
            unsigned char v = (unsigned char)((x * 7 + y * 3 + frame * 29) & 0xff);
            unsigned char q = (unsigned char)(((x < W / 2) ? 0x40 : 0xc0) ^ ((y < H / 2) ? 0 : 0x20));
            p[0] = border ? 255 : (unsigned char)((y < H / 2) ? v : q);       /* B */
            p[1] = border ? 0 : (unsigned char)((x < W / 2) ? q : v);         /* G */
            p[2] = border ? 0 : (unsigned char)(v ^ q);                       /* R */
            p[3] = 255;                                                      /* A */
        }
    }
    /* The frame number, unmistakably, in the top-left 4x4 block. */
    for (unsigned y = 1; y < 5; y++)
        for (unsigned x = 1; x < 5; x++) {
            unsigned char *p = px + ((size_t)y * W + x) * 4;
            p[0] = p[1] = p[2] = (unsigned char)(frame * 61 + 7);
        }
}

static int write_ppm(const char *path, const unsigned char *bgra) {
    FILE *f = fopen(path, "wb");
    if (!f)
        return 0;
    fprintf(f, "P6\n%u %u\n255\n", W, H);
    for (size_t i = 0; i < (size_t)W * H; i++) {
        fputc(bgra[i * 4 + 2], f);
        fputc(bgra[i * 4 + 1], f);
        fputc(bgra[i * 4 + 0], f);
    }
    fclose(f);
    return 1;
}

int main(void) {
    W = env_u("VK_PRESENT_PROBE_W", W);
    H = env_u("VK_PRESENT_PROBE_H", H);
    FRAMES = env_u("VK_PRESENT_PROBE_FRAMES", FRAMES);
    if (FRAMES > MAX_FRAMES)
        FRAMES = MAX_FRAMES;
    loader = dlopen("libvulkan.so.1", RTLD_NOW);
    if (!loader) {
        printf("SKIP: no libvulkan.so.1: %s\n", dlerror());
        return 7;
    }
    vkGetInstanceProcAddr = (PFN_vkGetInstanceProcAddr)dlsym(loader, "vkGetInstanceProcAddr");
    if (!vkGetInstanceProcAddr) {
        printf("SKIP: no vkGetInstanceProcAddr\n");
        return 7;
    }
#define LOADI(name) name = (PFN_##name)vkGetInstanceProcAddr(inst, #name)
    VkInstance inst = VK_NULL_HANDLE;
    LOADI(vkCreateInstance);

    const char *inst_exts[] = {"VK_KHR_surface", "VK_EXT_headless_surface"};
    VkApplicationInfo app = {VK_STRUCTURE_TYPE_APPLICATION_INFO};
    app.pApplicationName = "vk-present-probe";
    app.apiVersion = VK_MAKE_API_VERSION(0, 1, 1, 0);
    VkInstanceCreateInfo ici = {VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO};
    ici.pApplicationInfo = &app;
    ici.enabledExtensionCount = 2;
    ici.ppEnabledExtensionNames = inst_exts;
    CHECK(vkCreateInstance(&ici, NULL, &inst));

    LOADI(vkDestroyInstance);
    LOADI(vkEnumeratePhysicalDevices);
    LOADI(vkGetPhysicalDeviceQueueFamilyProperties);
    LOADI(vkGetPhysicalDeviceMemoryProperties);
    LOADI(vkCreateDevice);
    LOADI(vkGetDeviceProcAddr);
    LOADI(vkGetPhysicalDeviceSurfaceSupportKHR);
    LOADI(vkGetPhysicalDeviceSurfaceCapabilitiesKHR);
    LOADI(vkGetPhysicalDeviceSurfaceFormatsKHR);
    LOADI(vkGetPhysicalDeviceSurfacePresentModesKHR);
    LOADI(vkCreateHeadlessSurfaceEXT);
    LOADI(vkDestroySurfaceKHR);
    if (!vkCreateHeadlessSurfaceEXT) {
        printf("FAIL: the loader has no vkCreateHeadlessSurfaceEXT\n");
        return 1;
    }

    uint32_t n = 0;
    CHECK(vkEnumeratePhysicalDevices(inst, &n, NULL));
    if (!n) {
        printf("FAIL: no physical device\n");
        return 1;
    }
    VkPhysicalDevice pds[8];
    n = n > 8 ? 8 : n;
    CHECK(vkEnumeratePhysicalDevices(inst, &n, pds));
    VkPhysicalDevice pd = pds[0];

    VkHeadlessSurfaceCreateInfoEXT hci = {VK_STRUCTURE_TYPE_HEADLESS_SURFACE_CREATE_INFO_EXT};
    VkSurfaceKHR surface = VK_NULL_HANDLE;
    CHECK(vkCreateHeadlessSurfaceEXT(inst, &hci, NULL, &surface));

    VkBool32 supported = VK_FALSE;
    CHECK(vkGetPhysicalDeviceSurfaceSupportKHR(pd, 0, surface, &supported));
    printf("surface support family 0: %s\n", supported ? "yes" : "no");
    if (!supported) {
        printf("FAIL: family 0 cannot present\n");
        return 1;
    }

    VkSurfaceCapabilitiesKHR caps;
    CHECK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(pd, surface, &caps));
    printf("caps: images %u..%u  current %ux%u  usage 0x%x\n", caps.minImageCount,
           caps.maxImageCount, caps.currentExtent.width, caps.currentExtent.height,
           (unsigned)caps.supportedUsageFlags);

    uint32_t nf = 0;
    CHECK(vkGetPhysicalDeviceSurfaceFormatsKHR(pd, surface, &nf, NULL));
    VkSurfaceFormatKHR fmts[16];
    nf = nf > 16 ? 16 : nf;
    CHECK(vkGetPhysicalDeviceSurfaceFormatsKHR(pd, surface, &nf, fmts));
    printf("formats: %u, first %d\n", nf, (int)fmts[0].format);
    VkFormat format = VK_FORMAT_UNDEFINED;
    for (uint32_t i = 0; i < nf; i++)
        if (fmts[i].format == VK_FORMAT_B8G8R8A8_UNORM)
            format = fmts[i].format;
    if (format == VK_FORMAT_UNDEFINED) {
        printf("FAIL: no VK_FORMAT_B8G8R8A8_UNORM in the surface format list\n");
        return 1;
    }

    uint32_t nm = 0;
    CHECK(vkGetPhysicalDeviceSurfacePresentModesKHR(pd, surface, &nm, NULL));
    VkPresentModeKHR modes[16];
    nm = nm > 16 ? 16 : nm;
    CHECK(vkGetPhysicalDeviceSurfacePresentModesKHR(pd, surface, &nm, modes));
    printf("present modes: %u\n", nm);

    float prio = 1.f;
    VkDeviceQueueCreateInfo qci = {VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO};
    qci.queueFamilyIndex = 0;
    qci.queueCount = 1;
    qci.pQueuePriorities = &prio;
    const char *dev_exts[] = {"VK_KHR_swapchain"};
    VkDeviceCreateInfo dci = {VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO};
    dci.queueCreateInfoCount = 1;
    dci.pQueueCreateInfos = &qci;
    dci.enabledExtensionCount = 1;
    dci.ppEnabledExtensionNames = dev_exts;
    VkDevice dev = VK_NULL_HANDLE;
    CHECK(vkCreateDevice(pd, &dci, NULL, &dev));

#define LOADD(name) name = (PFN_##name)vkGetDeviceProcAddr(dev, #name)
    LOADD(vkDestroyDevice);
    LOADD(vkGetDeviceQueue);
    LOADD(vkCreateSwapchainKHR);
    LOADD(vkDestroySwapchainKHR);
    LOADD(vkGetSwapchainImagesKHR);
    LOADD(vkAcquireNextImageKHR);
    LOADD(vkQueuePresentKHR);
    LOADD(vkCreateBuffer);
    LOADD(vkDestroyBuffer);
    LOADD(vkGetBufferMemoryRequirements);
    LOADD(vkAllocateMemory);
    LOADD(vkFreeMemory);
    LOADD(vkBindBufferMemory);
    LOADD(vkMapMemory);
    LOADD(vkCreateCommandPool);
    LOADD(vkDestroyCommandPool);
    LOADD(vkAllocateCommandBuffers);
    LOADD(vkBeginCommandBuffer);
    LOADD(vkEndCommandBuffer);
    LOADD(vkCmdPipelineBarrier);
    LOADD(vkCmdCopyBufferToImage);
    LOADD(vkQueueSubmit);
    LOADD(vkQueueWaitIdle);
    LOADD(vkDeviceWaitIdle);
    LOADD(vkCreateSemaphore);
    LOADD(vkDestroySemaphore);
    if (!vkCreateSwapchainKHR) {
        printf("FAIL: the device has no vkCreateSwapchainKHR\n");
        return 1;
    }

    VkQueue queue;
    vkGetDeviceQueue(dev, 0, 0, &queue);

    VkSwapchainCreateInfoKHR sci = {VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR};
    sci.surface = surface;
    sci.minImageCount = caps.minImageCount < 2 ? 2 : caps.minImageCount;
    sci.imageFormat = format;
    sci.imageColorSpace = VK_COLOR_SPACE_SRGB_NONLINEAR_KHR;
    sci.imageExtent.width = W;
    sci.imageExtent.height = H;
    sci.imageArrayLayers = 1;
    sci.imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT;
    sci.imageSharingMode = VK_SHARING_MODE_EXCLUSIVE;
    sci.preTransform = VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR;
    sci.compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR;
    sci.presentMode = VK_PRESENT_MODE_FIFO_KHR;
    sci.clipped = VK_TRUE;
    VkSwapchainKHR swapchain = VK_NULL_HANDLE;
    CHECK(vkCreateSwapchainKHR(dev, &sci, NULL, &swapchain));
    printf("swapchain created\n");

    uint32_t ni = 0;
    CHECK(vkGetSwapchainImagesKHR(dev, swapchain, &ni, NULL));
    VkImage images[8];
    ni = ni > 8 ? 8 : ni;
    CHECK(vkGetSwapchainImagesKHR(dev, swapchain, &ni, images));
    printf("swapchain images: %u\n", ni);

    /* The staging buffer, in arena memory: the pattern is written here by the
     * CPU with no copy at all, which is the property the whole project exists
     * for, and the GPU reads it straight out of the arena. */
    VkBufferCreateInfo bci = {VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO};
    bci.size = (VkDeviceSize)W * H * 4;
    bci.usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT;
    bci.sharingMode = VK_SHARING_MODE_EXCLUSIVE;
    VkBuffer staging;
    CHECK(vkCreateBuffer(dev, &bci, NULL, &staging));
    VkMemoryRequirements mr;
    vkGetBufferMemoryRequirements(dev, staging, &mr);
    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    uint32_t type = UINT32_MAX;
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++) {
        if (!(mr.memoryTypeBits & (1u << i)))
            continue;
        VkMemoryPropertyFlags f = mp.memoryTypes[i].propertyFlags;
        if ((f & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) &&
            (f & VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)) {
            type = i;
            break;
        }
    }
    if (type == UINT32_MAX) {
        printf("FAIL: no host-visible memory type for the staging buffer\n");
        return 1;
    }
    VkMemoryAllocateInfo mai = {VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO};
    mai.allocationSize = mr.size;
    mai.memoryTypeIndex = type;
    VkDeviceMemory mem;
    CHECK(vkAllocateMemory(dev, &mai, NULL, &mem));
    CHECK(vkBindBufferMemory(dev, staging, mem, 0));
    void *mapped = NULL;
    CHECK(vkMapMemory(dev, mem, 0, VK_WHOLE_SIZE, 0, &mapped));

    VkCommandPoolCreateInfo pci = {VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO};
    pci.queueFamilyIndex = 0;
    pci.flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT;
    VkCommandPool pool;
    CHECK(vkCreateCommandPool(dev, &pci, NULL, &pool));
    VkCommandBufferAllocateInfo cbai = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO};
    cbai.commandPool = pool;
    cbai.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
    cbai.commandBufferCount = FRAMES;
    VkCommandBuffer cbs[MAX_FRAMES];
    CHECK(vkAllocateCommandBuffers(dev, &cbai, cbs));

    VkSemaphoreCreateInfo semi = {VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO};
    VkSemaphore acquired[MAX_FRAMES], rendered[MAX_FRAMES];
    for (unsigned i = 0; i < FRAMES; i++) {
        CHECK(vkCreateSemaphore(dev, &semi, NULL, &acquired[i]));
        CHECK(vkCreateSemaphore(dev, &semi, NULL, &rendered[i]));
    }

    const char *expect = getenv("VK_PRESENT_PROBE_EXPECT");
    double t_frames = 0, t_present = 0, t_acquire = 0, t0;
    for (unsigned frame = 0; frame < FRAMES; frame++) {
        double t_frame0 = now_ms();
        pattern((unsigned char *)mapped, frame);
        if (expect) {
            char path[512];
            snprintf(path, sizeof path, "%s%04u.ppm", expect, frame);
            if (!write_ppm(path, (const unsigned char *)mapped)) {
                printf("FAIL: cannot write %s\n", path);
                return 1;
            }
        }

        uint32_t index = 0;
        t0 = now_ms();
        CHECK(vkAcquireNextImageKHR(dev, swapchain, UINT64_MAX, acquired[frame], VK_NULL_HANDLE,
                                    &index));
        t_acquire += now_ms() - t0;
        if (index >= ni) {
            printf("FAIL: acquire returned index %u of %u\n", index, ni);
            return 1;
        }

        VkCommandBuffer cb = cbs[frame];
        VkCommandBufferBeginInfo bi = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
        bi.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
        CHECK(vkBeginCommandBuffer(cb, &bi));
        VkImageMemoryBarrier to_dst = {VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER};
        to_dst.srcAccessMask = 0;
        to_dst.dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
        to_dst.oldLayout = VK_IMAGE_LAYOUT_UNDEFINED;
        to_dst.newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
        to_dst.srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
        to_dst.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
        to_dst.image = images[index];
        to_dst.subresourceRange.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT;
        to_dst.subresourceRange.levelCount = 1;
        to_dst.subresourceRange.layerCount = 1;
        vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
                             0, 0, NULL, 0, NULL, 1, &to_dst);
        VkBufferImageCopy region;
        memset(&region, 0, sizeof region);
        region.imageSubresource.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT;
        region.imageSubresource.layerCount = 1;
        region.imageExtent.width = W;
        region.imageExtent.height = H;
        region.imageExtent.depth = 1;
        vkCmdCopyBufferToImage(cb, staging, images[index],
                               VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &region);
        VkImageMemoryBarrier to_present = to_dst;
        to_present.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
        to_present.dstAccessMask = VK_ACCESS_MEMORY_READ_BIT;
        to_present.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
        to_present.newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
        vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT,
                             VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, NULL, 0, NULL, 1,
                             &to_present);
        CHECK(vkEndCommandBuffer(cb));

        VkPipelineStageFlags wait_stage = VK_PIPELINE_STAGE_TRANSFER_BIT;
        VkSubmitInfo si = {VK_STRUCTURE_TYPE_SUBMIT_INFO};
        si.waitSemaphoreCount = 1;
        si.pWaitSemaphores = &acquired[frame];
        si.pWaitDstStageMask = &wait_stage;
        si.commandBufferCount = 1;
        si.pCommandBuffers = &cb;
        si.signalSemaphoreCount = 1;
        si.pSignalSemaphores = &rendered[frame];
        CHECK(vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE));

        VkPresentInfoKHR pi = {VK_STRUCTURE_TYPE_PRESENT_INFO_KHR};
        pi.waitSemaphoreCount = 1;
        pi.pWaitSemaphores = &rendered[frame];
        pi.swapchainCount = 1;
        pi.pSwapchains = &swapchain;
        pi.pImageIndices = &index;
        t0 = now_ms();
        VkResult pr = vkQueuePresentKHR(queue, &pi);
        t_present += now_ms() - t0;
        if (pr != VK_SUCCESS && pr != VK_SUBOPTIMAL_KHR) {
            printf("FAIL: vkQueuePresentKHR -> %d\n", (int)pr);
            return 1;
        }
        t_frames += now_ms() - t_frame0;
        if (FRAMES <= 8)
            printf("frame %u presented from image %u\n", frame, index);
    }
    /* docs/design.md §10's "measure what the blit costs". `vkQueuePresentKHR` here is
     * the whole of it: one `vkQueueSubmit` round trip carrying a pre-recorded
     * `vkCmdCopyImageToBuffer`, a `vkWaitForFences`, and the app side reading
     * the arena. Everything else in a frame belongs to the application. */
    printf("timing %ux%u over %u frames: %.2f ms/frame total, "
           "%.2f ms/frame in vkQueuePresentKHR, %.3f ms/frame in vkAcquireNextImageKHR "
           "(%.1f MB/frame, %.0f MB/s presented)\n",
           W, H, FRAMES, t_frames / FRAMES, t_present / FRAMES, t_acquire / FRAMES,
           (double)W * H * 4 / (1 << 20),
           t_present > 0 ? (double)W * H * 4 * FRAMES / (t_present / 1e3) / (1 << 20) : 0.0);

    CHECK(vkQueueWaitIdle(queue));
    CHECK(vkDeviceWaitIdle(dev));
    for (unsigned i = 0; i < FRAMES; i++) {
        vkDestroySemaphore(dev, acquired[i], NULL);
        vkDestroySemaphore(dev, rendered[i], NULL);
    }
    vkDestroyCommandPool(dev, pool, NULL);
    vkFreeMemory(dev, mem, NULL);
    vkDestroyBuffer(dev, staging, NULL);
    vkDestroySwapchainKHR(dev, swapchain, NULL);
    vkDestroyDevice(dev, NULL);
    vkDestroySurfaceKHR(inst, surface, NULL);
    vkDestroyInstance(inst, NULL);
    printf("PASS: %d frames presented\n", FRAMES);
    return 0;
}
