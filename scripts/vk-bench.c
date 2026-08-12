/* vk-bench — what a forwarded Vulkan costs, measured through the real loader.
 *
 * design.md §13 step 1 asks for "a throughput/latency benchmark" and there was
 * never one. This is it, written at the *application* level rather than at the
 * transport level, for the reason §0.6 item 7 gives about control runs: the only
 * number that means anything is the one an application can observe, and the same
 * program run against the real driver with no forwarding at all is the yardstick
 * that says how much of it is ours.
 *
 * Build (from the dev shell):
 *   cc -O2 -o vk-bench scripts/vk-bench.c -ldl
 *
 * Run:
 *   VK_DRIVER_FILES=<manifest> SV_SOCKET=<sock> ./vk-bench
 *   VK_DRIVER_FILES=/run/opengl-driver/share/vulkan/icd.d/radeon_icd.x86_64.json ./vk-bench
 *
 * It prints one `bench <name> <median-us> <mean-us> <n>` line per case, so the
 * harness can diff two runs without parsing prose.
 *
 * THE CASES, and what each isolates:
 *
 *   roundtrip   `vkGetFenceStatus` in a loop. One command, one reply, nothing
 *               else: this is the cost of the ring itself, and every other
 *               number here is some multiple of it.
 *   create      `vkCreateBuffer` + `vkDestroyBuffer`. §6.4 claims object
 *               creation costs no round trip because the app side assigns the
 *               id; this is the check on that claim.
 *   record-N    N `vkCmdSetViewport` between Begin and End, no submit. This is
 *               §6.5's target: with one packet per command it is N ring pushes,
 *               and with a blob it should be N memcpys into arena memory.
 *   submit0     Begin/End of an *empty* command buffer, submit, wait the fence.
 *               The floor: a submission that does nothing still pays the GPU's
 *               own submit cost (1.347 ms on the Intel iGPU) plus one round
 *               trip.
 *   frame-N     Begin, N `vkCmdFillBuffer`, End, submit, wait. A frame's shape:
 *               recording and submission together, which is what an application
 *               actually does and the only case whose improvement is allowed to
 *               be claimed as a frame-rate improvement.
 *   submitq-N   The same recording, submitted, with the *fence wait deferred* to
 *               the next iteration. This is what an application does when it is
 *               pipelining, and it is the case a synchronous `vkQueueSubmit`
 *               destroys: with a blocking submit it is identical to frame-N, and
 *               with an asynchronous one it is bounded by the GPU rather than by
 *               the wire.
 */
#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static double now_us(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e6 + t.tv_nsec / 1e3;
}

static unsigned env_u(const char *name, unsigned dflt) {
    const char *v = getenv(name);
    if (!v || !*v)
        return dflt;
    unsigned long n = strtoul(v, NULL, 10);
    return n ? (unsigned)n : dflt;
}

#define MAXS 4096
static double samples[MAXS];

static int cmp_d(const void *a, const void *b) {
    double x = *(const double *)a, y = *(const double *)b;
    return x < y ? -1 : x > y ? 1 : 0;
}

/* Report a case. `per` divides the median so that a per-command figure can be
 * printed beside the per-iteration one — a 1000-command recording whose total is
 * 4 ms is 4 us a command, and that is the number §6.5 is about. */
static void report(const char *name, unsigned n, unsigned per) {
    if (!n)
        return;
    double sum = 0;
    for (unsigned i = 0; i < n; i++)
        sum += samples[i];
    qsort(samples, n, sizeof(double), cmp_d);
    double med = samples[n / 2];
    printf("bench %-14s median %10.3f us   mean %10.3f us   n %4u", name, med, sum / n, n);
    if (per > 1)
        printf("   per-op %8.3f us", med / per);
    printf("\n");
    fflush(stdout);
}

static void *loader;
#define VKFN(name) static PFN_##name name
VKFN(vkGetInstanceProcAddr);
VKFN(vkCreateInstance);
VKFN(vkDestroyInstance);
VKFN(vkEnumeratePhysicalDevices);
VKFN(vkGetPhysicalDeviceProperties);
VKFN(vkGetPhysicalDeviceQueueFamilyProperties);
VKFN(vkGetPhysicalDeviceMemoryProperties);
VKFN(vkCreateDevice);
VKFN(vkDestroyDevice);
VKFN(vkGetDeviceProcAddr);
VKFN(vkGetDeviceQueue);
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
VKFN(vkResetCommandBuffer);
VKFN(vkBeginCommandBuffer);
VKFN(vkEndCommandBuffer);
VKFN(vkCmdSetViewport);
VKFN(vkCmdFillBuffer);
VKFN(vkQueueSubmit);
VKFN(vkQueueWaitIdle);
VKFN(vkDeviceWaitIdle);
VKFN(vkCreateFence);
VKFN(vkDestroyFence);
VKFN(vkResetFences);
VKFN(vkWaitForFences);
VKFN(vkGetFenceStatus);

#define CHECK(expr)                                                                      \
    do {                                                                                 \
        VkResult _r = (expr);                                                            \
        if (_r != VK_SUCCESS) {                                                          \
            printf("FAIL: %s -> %d\n", #expr, (int)_r);                                  \
            return 1;                                                                    \
        }                                                                                \
    } while (0)

int main(void) {
    unsigned iters = env_u("VK_BENCH_ITERS", 200);
    unsigned cmds = env_u("VK_BENCH_CMDS", 1000);
    unsigned warm = env_u("VK_BENCH_WARMUP", 20);
    if (iters > MAXS)
        iters = MAXS;

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

    VkApplicationInfo app = {VK_STRUCTURE_TYPE_APPLICATION_INFO};
    app.pApplicationName = "vk-bench";
    app.apiVersion = VK_MAKE_API_VERSION(0, 1, 1, 0);
    VkInstanceCreateInfo ici = {VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO};
    ici.pApplicationInfo = &app;
    CHECK(vkCreateInstance(&ici, NULL, &inst));

    LOADI(vkDestroyInstance);
    LOADI(vkEnumeratePhysicalDevices);
    LOADI(vkGetPhysicalDeviceProperties);
    LOADI(vkGetPhysicalDeviceQueueFamilyProperties);
    LOADI(vkGetPhysicalDeviceMemoryProperties);
    LOADI(vkCreateDevice);
    LOADI(vkGetDeviceProcAddr);

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
    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(pd, &props);
    printf("device %s\n", props.deviceName);

    uint32_t qn = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &qn, NULL);
    VkQueueFamilyProperties qf[16];
    qn = qn > 16 ? 16 : qn;
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &qn, qf);
    uint32_t family = 0;
    for (uint32_t i = 0; i < qn; i++)
        if (qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) {
            family = i;
            break;
        }

    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = {VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO};
    qci.queueFamilyIndex = family;
    qci.queueCount = 1;
    qci.pQueuePriorities = &prio;
    VkDeviceCreateInfo dci = {VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO};
    dci.queueCreateInfoCount = 1;
    dci.pQueueCreateInfos = &qci;
    VkDevice dev = VK_NULL_HANDLE;
    CHECK(vkCreateDevice(pd, &dci, NULL, &dev));

#define LOADD(name) name = (PFN_##name)vkGetDeviceProcAddr(dev, #name)
    LOADD(vkDestroyDevice);
    LOADD(vkGetDeviceQueue);
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
    LOADD(vkResetCommandBuffer);
    LOADD(vkBeginCommandBuffer);
    LOADD(vkEndCommandBuffer);
    LOADD(vkCmdSetViewport);
    LOADD(vkCmdFillBuffer);
    LOADD(vkQueueSubmit);
    LOADD(vkQueueWaitIdle);
    LOADD(vkDeviceWaitIdle);
    LOADD(vkCreateFence);
    LOADD(vkDestroyFence);
    LOADD(vkResetFences);
    LOADD(vkWaitForFences);
    LOADD(vkGetFenceStatus);

    VkQueue queue;
    vkGetDeviceQueue(dev, family, 0, &queue);

    VkCommandPoolCreateInfo pci = {VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO};
    pci.flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT;
    pci.queueFamilyIndex = family;
    VkCommandPool pool;
    CHECK(vkCreateCommandPool(dev, &pci, NULL, &pool));

    VkCommandBufferAllocateInfo cbi = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO};
    cbi.commandPool = pool;
    cbi.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
    cbi.commandBufferCount = 2;
    VkCommandBuffer cb[2];
    CHECK(vkAllocateCommandBuffers(dev, &cbi, cb));

    VkFenceCreateInfo fci = {VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    VkFence fence[2];
    CHECK(vkCreateFence(dev, &fci, NULL, &fence[0]));
    CHECK(vkCreateFence(dev, &fci, NULL, &fence[1]));

    /* A device-local buffer for `vkCmdFillBuffer` to have somewhere to write.
     * Device-local so that the fill is real GPU work rather than a write into
     * the arena, which would measure the arena instead of the submission. */
    VkBufferCreateInfo bci = {VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO};
    bci.size = 1 << 20;
    bci.usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT;
    bci.sharingMode = VK_SHARING_MODE_EXCLUSIVE;
    VkBuffer buf;
    CHECK(vkCreateBuffer(dev, &bci, NULL, &buf));
    VkMemoryRequirements mr;
    vkGetBufferMemoryRequirements(dev, buf, &mr);
    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    uint32_t type = 0;
    int found = 0;
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
        if ((mr.memoryTypeBits & (1u << i)) &&
            (mp.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)) {
            type = i;
            found = 1;
            break;
        }
    if (!found)
        for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
            if (mr.memoryTypeBits & (1u << i)) {
                type = i;
                break;
            }
    VkMemoryAllocateInfo mai = {VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO};
    mai.allocationSize = mr.size;
    mai.memoryTypeIndex = type;
    VkDeviceMemory mem;
    CHECK(vkAllocateMemory(dev, &mai, NULL, &mem));
    CHECK(vkBindBufferMemory(dev, buf, mem, 0));

    VkCommandBufferBeginInfo bi = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
    bi.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
    VkViewport vp = {0, 0, 64, 64, 0.0f, 1.0f};

    /* ---- arena-write: what a host-visible mapping costs to dribble into ----
     *
     * design.md §6.5 puts a command buffer's recording in *arena* memory, and
     * `measured-gpu-side.md` says the arena costs nothing — 34.08 GB/s imported
     * against 33.56 for a driver allocation. That was one large sequential copy.
     * Recording is the opposite shape: a few dozen bytes at a time, with an
     * encode between each, and on a mapping that is write-combined rather than
     * write-back the two numbers have nothing to do with each other.
     *
     * So this writes the same total, in the same 48-byte pieces, into a
     * host-visible `vkMapMemory` pointer and into ordinary process memory, and
     * prints both. The ratio is the answer to "is the arena a fine place to
     * record into", and it is a property of the memory type the *provider*
     * chose rather than of anything this project decides. */
    {
        VkBufferCreateInfo sbi = {VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO};
        sbi.size = 4 << 20;
        sbi.usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT;
        VkBuffer sb;
        VkDeviceMemory sm = VK_NULL_HANDLE;
        void *host = NULL;
        if (vkCreateBuffer(dev, &sbi, NULL, &sb) == VK_SUCCESS) {
            VkMemoryRequirements smr;
            vkGetBufferMemoryRequirements(dev, sb, &smr);
            uint32_t st = UINT32_MAX;
            for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
                if ((smr.memoryTypeBits & (1u << i)) &&
                    (mp.memoryTypes[i].propertyFlags &
                     VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT)) {
                    st = i;
                    break;
                }
            if (st != UINT32_MAX) {
                VkMemoryAllocateInfo smai = {VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO};
                smai.allocationSize = smr.size;
                smai.memoryTypeIndex = st;
                if (vkAllocateMemory(dev, &smai, NULL, &sm) == VK_SUCCESS)
                    vkMapMemory(dev, sm, 0, VK_WHOLE_SIZE, 0, &host);
            }
        }
        unsigned reps = iters > 60 ? 60 : iters;
        unsigned pieces = 1000;
        unsigned char piece[48];
        memset(piece, 0xa5, sizeof piece);
        void *plain = malloc((size_t)pieces * sizeof piece);
        if (host) {
            for (unsigned i = 0; i < reps; i++) {
                double t0 = now_us();
                for (unsigned c = 0; c < pieces; c++)
                    memcpy((unsigned char *)host + (size_t)c * sizeof piece, piece,
                           sizeof piece);
                samples[i] = now_us() - t0;
            }
            report("arena-write", reps, pieces);
        }
        if (plain) {
            for (unsigned i = 0; i < reps; i++) {
                double t0 = now_us();
                for (unsigned c = 0; c < pieces; c++)
                    memcpy((unsigned char *)plain + (size_t)c * sizeof piece, piece,
                           sizeof piece);
                samples[i] = now_us() - t0;
            }
            report("plain-write", reps, pieces);
        }

        /* The same two, with work in between — which is the shape recording
         * actually has, because every `vkCmd*` encodes into a scratch buffer
         * before it copies. Back-to-back `memcpy`s let a write-combining buffer
         * fill and flush a whole cache line at a time; spaced ones do not, and
         * a partially-filled WC buffer flushing is an uncached write.
         *
         * `spaced-arena` minus `spaced-plain` is therefore the real per-command
         * price of putting a command blob in *this* arena, and it is a property
         * of the memory type the provider chose. On the `testhost` topology
         * the arena is ordinary guest RAM (design.md §5.6 makes it write-back)
         * and this difference should vanish; on the Linux GPU-export provider
         * the arena is the driver's, and it does not. */
        volatile unsigned sink = 0;
        if (host) {
            for (unsigned i = 0; i < reps; i++) {
                double t0 = now_us();
                for (unsigned c = 0; c < pieces; c++) {
                    unsigned x = c;
                    for (unsigned k = 0; k < 24; k++)
                        x = x * 1664525u + 1013904223u;
                    sink = x;
                    memcpy((unsigned char *)host + (size_t)c * sizeof piece, piece,
                           sizeof piece);
                }
                samples[i] = now_us() - t0;
            }
            report("spaced-arena", reps, pieces);
        }
        if (plain) {
            for (unsigned i = 0; i < reps; i++) {
                double t0 = now_us();
                for (unsigned c = 0; c < pieces; c++) {
                    unsigned x = c;
                    for (unsigned k = 0; k < 24; k++)
                        x = x * 1664525u + 1013904223u;
                    sink = x;
                    memcpy((unsigned char *)plain + (size_t)c * sizeof piece, piece,
                           sizeof piece);
                }
                samples[i] = now_us() - t0;
            }
            report("spaced-plain", reps, pieces);
            free(plain);
        }
        (void)sink;
        if (host)
            vkFreeMemory(dev, sm, NULL);
    }

    /* ---- roundtrip: one command, one reply -------------------------------- */
    for (unsigned i = 0; i < warm; i++)
        vkGetFenceStatus(dev, fence[0]);
    for (unsigned i = 0; i < iters; i++) {
        double t0 = now_us();
        vkGetFenceStatus(dev, fence[0]);
        samples[i] = now_us() - t0;
    }
    report("roundtrip", iters, 1);

    /* ---- create: §6.4's claim that an id costs no round trip -------------- */
    for (unsigned i = 0; i < iters; i++) {
        VkBuffer b;
        double t0 = now_us();
        vkCreateBuffer(dev, &bci, NULL, &b);
        samples[i] = now_us() - t0;
        vkDestroyBuffer(dev, b, NULL);
    }
    report("create", iters, 1);

    /* ---- record: N vkCmd* between Begin and End, no submit ---------------- */
    {
        unsigned reps = iters > 60 ? 60 : iters;
        for (unsigned i = 0; i < reps; i++) {
            vkResetCommandBuffer(cb[0], 0);
            vkBeginCommandBuffer(cb[0], &bi);
            double t0 = now_us();
            for (unsigned c = 0; c < cmds; c++)
                vkCmdSetViewport(cb[0], 0, 1, &vp);
            samples[i] = now_us() - t0;
            vkEndCommandBuffer(cb[0]);
        }
        char name[32];
        snprintf(name, sizeof name, "record-%u", cmds);
        report(name, reps, cmds);
    }

    /* ---- submit0: the empty submission floor ------------------------------ */
    for (unsigned i = 0; i < warm; i++) {
        vkResetCommandBuffer(cb[0], 0);
        vkBeginCommandBuffer(cb[0], &bi);
        vkEndCommandBuffer(cb[0]);
        VkSubmitInfo si = {VK_STRUCTURE_TYPE_SUBMIT_INFO};
        si.commandBufferCount = 1;
        si.pCommandBuffers = &cb[0];
        vkResetFences(dev, 1, &fence[0]);
        vkQueueSubmit(queue, 1, &si, fence[0]);
        vkWaitForFences(dev, 1, &fence[0], VK_TRUE, ~0ull);
    }
    for (unsigned i = 0; i < iters; i++) {
        double t0 = now_us();
        vkResetCommandBuffer(cb[0], 0);
        vkBeginCommandBuffer(cb[0], &bi);
        vkEndCommandBuffer(cb[0]);
        VkSubmitInfo si = {VK_STRUCTURE_TYPE_SUBMIT_INFO};
        si.commandBufferCount = 1;
        si.pCommandBuffers = &cb[0];
        vkResetFences(dev, 1, &fence[0]);
        vkQueueSubmit(queue, 1, &si, fence[0]);
        vkWaitForFences(dev, 1, &fence[0], VK_TRUE, ~0ull);
        samples[i] = now_us() - t0;
    }
    report("submit0", iters, 1);

    /* ---- submit0's parts, one line each ----------------------------------
     *
     * `submit0` is six forwarded commands and a wait, and until each is timed
     * separately "a submission costs 175 us" is not a finding, it is a number
     * with nowhere to put it. */
    {
        const char *names[6] = {"p-reset-cb", "p-begin", "p-end",
                                "p-reset-fence", "p-submit", "p-wait"};
        static double parts[6][MAXS];
        for (unsigned i = 0; i < iters; i++) {
            double t[7];
            t[0] = now_us();
            vkResetCommandBuffer(cb[0], 0);
            t[1] = now_us();
            vkBeginCommandBuffer(cb[0], &bi);
            t[2] = now_us();
            vkEndCommandBuffer(cb[0]);
            t[3] = now_us();
            vkResetFences(dev, 1, &fence[0]);
            t[4] = now_us();
            VkSubmitInfo si = {VK_STRUCTURE_TYPE_SUBMIT_INFO};
            si.commandBufferCount = 1;
            si.pCommandBuffers = &cb[0];
            vkQueueSubmit(queue, 1, &si, fence[0]);
            t[5] = now_us();
            vkWaitForFences(dev, 1, &fence[0], VK_TRUE, ~0ull);
            t[6] = now_us();
            for (unsigned k = 0; k < 6; k++)
                parts[k][i] = t[k + 1] - t[k];
        }
        for (unsigned k = 0; k < 6; k++) {
            memcpy(samples, parts[k], iters * sizeof(double));
            report(names[k], iters, 1);
        }
    }

    /* ---- frame: record N, submit, wait ------------------------------------ */
    {
        unsigned reps = iters > 60 ? 60 : iters;
        unsigned fc = cmds > 200 ? 200 : cmds;
        for (unsigned i = 0; i < reps; i++) {
            double t0 = now_us();
            vkResetCommandBuffer(cb[0], 0);
            vkBeginCommandBuffer(cb[0], &bi);
            for (unsigned c = 0; c < fc; c++)
                vkCmdFillBuffer(cb[0], buf, (c % 64) * 4096, 4096, c);
            vkEndCommandBuffer(cb[0]);
            VkSubmitInfo si = {VK_STRUCTURE_TYPE_SUBMIT_INFO};
            si.commandBufferCount = 1;
            si.pCommandBuffers = &cb[0];
            vkResetFences(dev, 1, &fence[0]);
            vkQueueSubmit(queue, 1, &si, fence[0]);
            vkWaitForFences(dev, 1, &fence[0], VK_TRUE, ~0ull);
            samples[i] = now_us() - t0;
        }
        char name[32];
        snprintf(name, sizeof name, "frame-%u", fc);
        report(name, reps, 1);
    }

    /* ---- submitq: the same, pipelined one deep ---------------------------- */
    {
        unsigned reps = iters > 60 ? 60 : iters;
        unsigned fc = cmds > 200 ? 200 : cmds;
        int armed = 0;
        unsigned slot = 0;
        vkDeviceWaitIdle(dev);
        for (unsigned i = 0; i < reps; i++) {
            double t0 = now_us();
            slot = i & 1;
            if (armed)
                vkWaitForFences(dev, 1, &fence[slot], VK_TRUE, ~0ull);
            vkResetCommandBuffer(cb[slot], 0);
            vkBeginCommandBuffer(cb[slot], &bi);
            for (unsigned c = 0; c < fc; c++)
                vkCmdFillBuffer(cb[slot], buf, (c % 64) * 4096, 4096, c);
            vkEndCommandBuffer(cb[slot]);
            VkSubmitInfo si = {VK_STRUCTURE_TYPE_SUBMIT_INFO};
            si.commandBufferCount = 1;
            si.pCommandBuffers = &cb[slot];
            vkResetFences(dev, 1, &fence[slot]);
            vkQueueSubmit(queue, 1, &si, fence[slot]);
            samples[i] = now_us() - t0;
            if (i >= 1)
                armed = 1;
        }
        vkDeviceWaitIdle(dev);
        char name[32];
        snprintf(name, sizeof name, "submitq-%u", fc);
        report(name, reps, 1);
    }

    vkDeviceWaitIdle(dev);
    vkDestroyFence(dev, fence[0], NULL);
    vkDestroyFence(dev, fence[1], NULL);
    vkDestroyBuffer(dev, buf, NULL);
    vkFreeMemory(dev, mem, NULL);
    vkDestroyCommandPool(dev, pool, NULL);
    vkDestroyDevice(dev, NULL);
    vkDestroyInstance(inst, NULL);
    printf("ok\n");
    return 0;
}
