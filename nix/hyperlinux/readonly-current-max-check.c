/* Empty-VM diagnostic on original shared pages. No vCPU, maximum-rights
 * changes, writable consumer alias, remapping, COW or copied fallback. */
#include <Hypervisor/Hypervisor.h>
#include <mach/mach.h>
#include <mach/mach_vm.h>
#include <assert.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>
#define LEN 65536
static vm_region_basic_info_data_64_t rights(void *view) {
    vm_region_basic_info_data_64_t info;
    mach_msg_type_number_t count = VM_REGION_BASIC_INFO_COUNT_64;
    mach_vm_address_t address = (mach_vm_address_t)(uintptr_t)view;
    mach_vm_size_t size = 0;
    mach_port_t object = MACH_PORT_NULL;
    assert(mach_vm_region(mach_task_self(), &address, &size,
        VM_REGION_BASIC_INFO_64, (vm_region_info_t)&info, &count, &object) == KERN_SUCCESS);
    if (object) mach_port_deallocate(mach_task_self(), object);
    assert(address == (mach_vm_address_t)(uintptr_t)view && size >= LEN);
    assert(info.shared);
    return info;
}
static void check(const char *label, void *view, void *consumer, int rw,
                  unsigned serial, vm_prot_t current, vm_prot_t maximum) {
    vm_region_basic_info_data_64_t info = rights(view);
    assert(info.protection == current && info.max_protection == maximum);
    uint32_t value = 0x56780000 + serial;
    /* Only the original producer fd updates the original backing pages.
     * This remains valid while its existing mapping is read-only. */
    assert(pwrite(rw, &value, sizeof(value), 0) == sizeof(value));
    assert(*(volatile uint32_t *)view == value);
    assert(*(volatile uint32_t *)consumer == value);
    hv_return_t rc = hv_vm_map(view, 0x1000000, LEN, HV_MEMORY_READ);
    printf("RO_CURRENT_MAX view=%s current=%d maximum=%d stage2=R hv=0x%08x original_page_visible=1\n",
        label, info.protection, info.max_protection, (unsigned)rc);
    fflush(stdout);
    if (rc == HV_SUCCESS) assert(hv_vm_unmap(0x1000000, LEN) == HV_SUCCESS);
    info = rights(view);
    assert(info.protection == current && info.max_protection == maximum);
}
int main(void) {
    char path[] = "/tmp/hl-ro-current-max-XXXXXX";
    int rw = mkstemp(path);
    assert(rw >= 0 && !ftruncate(rw, LEN));
    int ro = open(path, O_RDONLY);
    assert(ro >= 0 && !unlink(path));
    assert((fcntl(rw, F_GETFL) & O_ACCMODE) == O_RDWR);
    assert((fcntl(ro, F_GETFL) & O_ACCMODE) == O_RDONLY);
    void *producer = mmap(NULL, LEN, PROT_READ | PROT_WRITE, MAP_SHARED, rw, 0);
    void *consumer = mmap(NULL, LEN, PROT_READ, MAP_SHARED, ro, 0);
    assert(producer != MAP_FAILED && consumer != MAP_FAILED);
    vm_region_basic_info_data_64_t initial = rights(producer), ci = rights(consumer);
    assert(initial.protection == (VM_PROT_READ | VM_PROT_WRITE));
    assert(initial.max_protection & VM_PROT_WRITE);
    assert(ci.protection == VM_PROT_READ && !(ci.max_protection & VM_PROT_WRITE));
    assert(hv_vm_create(NULL) == HV_SUCCESS);
    check("producer-original-rw", producer, consumer, rw, 0, initial.protection, initial.max_protection);
    check("consumer-original-r", consumer, consumer, rw, 1, ci.protection, ci.max_protection);
    assert(!mprotect(producer, LEN, PROT_READ));
    check("producer-demoted-r", producer, consumer, rw, 2, VM_PROT_READ, initial.max_protection);
    assert(!mprotect(producer, LEN, PROT_READ | PROT_WRITE));
    check("producer-restored-rw", producer, consumer, rw, 3, initial.protection, initial.max_protection);
    check("consumer-unchanged-r", consumer, consumer, rw, 4, ci.protection, ci.max_protection);
    assert(hv_vm_destroy() == HV_SUCCESS);
    assert(!munmap(consumer, LEN) && !munmap(producer, LEN));
    assert(!close(ro) && !close(rw));
    puts("RO_CURRENT_MAX cleanup=ok vcpu_created=0");
    return 0;
}
