#include "hl_copy.h"
#include <stdint.h>
#include <mach/mach.h>
#include <mach/mach_vm.h>
int hl_wc_copy(void *destination, const void *source, size_t length) {
    if (!length) return 0;
    mach_vm_size_t copied = 0;
    kern_return_t rc = mach_vm_read_overwrite(mach_task_self(),
        (mach_vm_address_t)(uintptr_t)source, (mach_vm_size_t)length,
        (mach_vm_address_t)(uintptr_t)destination, &copied);
    return rc == KERN_SUCCESS && copied == length ? 0 : -1;
}
