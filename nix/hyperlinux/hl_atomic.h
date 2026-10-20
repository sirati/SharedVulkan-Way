#ifndef HL_WC_ATOMIC_H
#define HL_WC_ATOMIC_H
#include <stdint.h>
#include <mach/mach.h>
/* Only the calling thread's BAD_ACCESS port changes. Existing non-null
 * thread handlers cause refusal and immediate restoration. Task ports never
 * change. A leaf-PC-only recovery returns EFAULT without native unwinding. */
typedef struct {
    thread_t thread;
    exception_mask_t masks[EXC_TYPES_COUNT];
    mach_port_t ports[EXC_TYPES_COUNT];
    exception_behavior_t behaviors[EXC_TYPES_COUNT];
    thread_state_flavor_t flavors[EXC_TYPES_COUNT];
    mach_msg_type_number_t count;
    int active;
} hl_wc_atomic_scope;
int hl_wc_atomic_begin(hl_wc_atomic_scope *);
void hl_wc_atomic_end(hl_wc_atomic_scope *);
/* load/store return 0 or -EFAULT. CAS returns 1 matched, 0 mismatch,
 * or -EFAULT. Output/expected pointers must be trusted native storage. */
int hl_wc_atomic_load(const uint32_t *, uint32_t *);
int hl_wc_atomic_store(uint32_t *, uint32_t);
int hl_wc_atomic_cas(uint32_t *, uint32_t *, uint32_t);
#endif
