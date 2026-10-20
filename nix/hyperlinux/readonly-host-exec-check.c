/* Empty-VM admission diagnostic. The consumer keeps the original O_RDONLY
 * MAP_SHARED pages. Host RX stays within the file mapping's maximum rights.
 * Stage 2 is READ only and no vCPU is created. */
#include <Hypervisor/Hypervisor.h>
#include <mach/mach.h>
#include <mach/mach_vm.h>
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>
#define LEN 65536
static void check(void *consumer, volatile uint32_t *producer, unsigned serial, int readonly) {
    vm_region_basic_info_data_64_t info;
    mach_msg_type_number_t count = VM_REGION_BASIC_INFO_COUNT_64;
    mach_vm_address_t address = (mach_vm_address_t)(uintptr_t)consumer;
    mach_vm_size_t size = 0;
    mach_port_t object = MACH_PORT_NULL;
    assert(mach_vm_region(mach_task_self(), &address, &size,
        VM_REGION_BASIC_INFO_64, (vm_region_info_t)&info, &count, &object) == KERN_SUCCESS);
    if (object) mach_port_deallocate(mach_task_self(), object);
    assert(info.protection & VM_PROT_READ);
    if (readonly) {
        assert(!(info.protection & VM_PROT_WRITE));
        assert(!(info.max_protection & VM_PROT_WRITE));
    }
    *producer = 0x12340000 + serial;
    assert(*(volatile uint32_t *)consumer == *producer);
    hv_return_t rc = hv_vm_map(consumer, 0x1000000, LEN, HV_MEMORY_READ);
    printf("RO_HOST_EXEC view=%s current=%d maximum=%d stage2=R hv=0x%08x original_page_visible=1\n",
        readonly ? "original-ro" : "authorized-producer-control", info.protection, info.max_protection, (unsigned)rc);
    if (rc == HV_SUCCESS) assert(hv_vm_unmap(0x1000000, LEN) == HV_SUCCESS);
}
int main(void) {
    char path[] = "/tmp/hl-ro-host-exec-XXXXXX";
    int rw = mkstemp(path);
    assert(rw >= 0 && !ftruncate(rw, LEN));
    int ro = open(path, O_RDONLY);
    assert(ro >= 0 && !unlink(path));
    void *producer = mmap(NULL, LEN, PROT_READ | PROT_WRITE, MAP_SHARED, rw, 0);
    void *consumer = mmap(NULL, LEN, PROT_READ, MAP_SHARED, ro, 0);
    assert(producer != MAP_FAILED && consumer != MAP_FAILED);
    assert(hv_vm_create(NULL) == HV_SUCCESS);
    /* Positive setup control, same IPA/length and READ-only stage 2. The
     * producer already owns this original RW view. No consumer right changes. */
    check(producer, producer, 0, 0);
    check(consumer, producer, 1, 1);
    int rc = mprotect(consumer, LEN, PROT_READ | PROT_EXEC);
    printf("RO_HOST_EXEC protect_rx=%d errno=%d\n", rc, rc ? errno : 0);
    if (!rc) {
        check(consumer, producer, 2, 1);
        assert(!mprotect(consumer, LEN, PROT_READ));
        check(consumer, producer, 3, 1);
    }
    assert(hv_vm_destroy() == HV_SUCCESS);
    assert(!munmap(consumer, LEN) && !munmap(producer, LEN));
    close(ro); close(rw);
    return 0;
}
