#include <mach/kmod.h>
extern "C" {
// Modern IOKit initializes/finalizes C++ metaclasses around module start/stop.
// Matching and instance teardown use the IOService lifecycle in WCIVSHMEM.
kern_return_t WCIVSHMEM_start(kmod_info_t *, void *) { return KERN_SUCCESS; }
kern_return_t WCIVSHMEM_stop(kmod_info_t *, void *) { return KERN_SUCCESS; }
KMOD_EXPLICIT_DECL(org.waycompose.ivshmem, "0.1.0", WCIVSHMEM_start, WCIVSHMEM_stop)
}
