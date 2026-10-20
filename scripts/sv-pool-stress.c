/* sv-pool-stress: fill the leased arena from one Vulkan application, and
 * check that running out is an ordinary VK_ERROR_OUT_OF_DEVICE_MEMORY, that
 * memory freed is memory allocatable again, and that every byte mapped is the
 * application's (waycompose-design.md §97).
 *
 *   sv-pool-stress <block MiB> [hold seconds] [label] [linger seconds]
 *
 * 1. Allocate host-visible blocks of <block MiB> until vkAllocateMemory
 *    refuses; the refusal must be VK_ERROR_OUT_OF_DEVICE_MEMORY. Every block is
 *    mapped and filled with a pattern naming the block.
 * 2. Hold for [hold seconds] (so a second process can run against the same
 *    pool), then check every block's pattern: nothing another process did may
 *    have touched it.
 * 3. Free every second block, allocate again until refused: at least as many
 *    must succeed as were freed.
 * 4. Free everything and exit 0. Any other outcome exits 1 and says why.
 *
 * Prints one machine-readable line at the end:
 *   RESULT label=<l> block=<MiB> first=<n> refused=<VkResult> again=<n> ok=1
 */
#include <vulkan/vulkan.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define MAXB 4096

static void fail(const char *what, VkResult r) {
    printf("FAIL %s: VkResult %d\n", what, r);
    exit(1);
}

int main(int argc, char **argv) {
    uint64_t block = (argc > 1 ? strtoull(argv[1], 0, 10) : 8) << 20;
    int hold = argc > 2 ? atoi(argv[2]) : 0;
    const char *label = argc > 3 ? argv[3] : "stress";

    VkApplicationInfo app = { VK_STRUCTURE_TYPE_APPLICATION_INFO, 0, label, 1, 0, 0, VK_API_VERSION_1_1 };
    VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, 0, 0, &app };
    VkInstance inst;
    VkResult r = vkCreateInstance(&ici, 0, &inst);
    if (r) fail("vkCreateInstance", r);
    uint32_t n = 1;
    VkPhysicalDevice pd;
    r = vkEnumeratePhysicalDevices(inst, &n, &pd);
    if (r < 0 || n < 1) fail("vkEnumeratePhysicalDevices", r);
    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    uint32_t type = UINT32_MAX;
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++) {
        VkMemoryPropertyFlags f = mp.memoryTypes[i].propertyFlags;
        if ((f & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) && (f & VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)) { type = i; break; }
    }
    if (type == UINT32_MAX) fail("no HOST_VISIBLE|HOST_COHERENT type", 0);
    printf("%s: heap of type %u is %llu MiB\n", label, type,
           (unsigned long long)(mp.memoryHeaps[mp.memoryTypes[type].heapIndex].size >> 20));

    float prio = 1.0f;
    VkDeviceQueueCreateInfo q = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, 0, 0, 0, 1, &prio };
    VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, 0, 0, 1, &q };
    VkDevice dev;
    r = vkCreateDevice(pd, &dci, 0, &dev);
    if (r) fail("vkCreateDevice", r);

    static VkDeviceMemory mem[MAXB];
    static uint32_t *ptr[MAXB];
    int count = 0;
    VkResult refused = VK_SUCCESS;
    for (; count < MAXB; count++) {
        VkMemoryAllocateInfo ai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, 0, block, type };
        r = vkAllocateMemory(dev, &ai, 0, &mem[count]);
        if (r) { refused = r; break; }
        r = vkMapMemory(dev, mem[count], 0, VK_WHOLE_SIZE, 0, (void **)&ptr[count]);
        if (r) fail("vkMapMemory", r);
        for (uint64_t w = 0; w < block / 4; w += 1024) ptr[count][w] = 0x50000000u | (uint32_t)count;
    }
    int first = count;
    printf("%s: %d block(s) of %llu MiB, then %d\n", label, first, (unsigned long long)(block >> 20), refused);
    if (refused != VK_ERROR_OUT_OF_DEVICE_MEMORY) fail("the refusal is not VK_ERROR_OUT_OF_DEVICE_MEMORY", refused);
    if (first == 0) fail("not one block", refused);

    if (hold > 0) sleep(hold);
    for (int i = 0; i < first; i++)
        for (uint64_t w = 0; w < block / 4; w += 1024)
            if (ptr[i][w] != (0x50000000u | (uint32_t)i)) {
                printf("FAIL block %d word %llu is %08x: another process wrote this application's memory\n", i,
                       (unsigned long long)w, ptr[i][w]);
                return 1;
            }

    int freed = 0;
    for (int i = 0; i < first; i += 2) {
        vkUnmapMemory(dev, mem[i]);
        vkFreeMemory(dev, mem[i], 0);
        mem[i] = VK_NULL_HANDLE;
        freed++;
    }
    int again = 0;
    for (int i = 0; i < first; i += 2) {
        VkMemoryAllocateInfo ai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, 0, block, type };
        r = vkAllocateMemory(dev, &ai, 0, &mem[i]);
        if (r) break;
        again++;
    }
    printf("%s: freed %d, allocated %d again\n", label, freed, again);
    if (again < freed) fail("freed memory did not come back", r);

    for (int i = 0; i < first; i++)
        if (mem[i]) vkFreeMemory(dev, mem[i], 0);
    /* [linger seconds]: stay alive with the device open after freeing
     * everything, so a pool that shrinks (§108) is seen giving memory back
     * while this process still lives. */
    if (argc > 4) {
        printf("%s: everything freed; lingering %d s\n", label, atoi(argv[4]));
        fflush(stdout);
        sleep(atoi(argv[4]));
    }
    vkDestroyDevice(dev, 0);
    vkDestroyInstance(inst, 0);
    printf("RESULT label=%s block=%llu first=%d refused=%d again=%d ok=1\n", label,
           (unsigned long long)(block >> 20), first, refused, again);
    return 0;
}
