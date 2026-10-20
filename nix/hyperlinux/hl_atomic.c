#include "hl_atomic.h"
#include <mach/arm/thread_status.h>
#include <mach/ndr.h>
#include <mach/mig_errors.h>
#include <mach/exc.h>
#include <pthread.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>

/* Every recoverable PC is one guest-address memory instruction in a leaf.
 * Native output stores, stack operations and all surrounding C are excluded.
 * The recovery is in the same leaf and returns normally through its own LR.
 * No Rust frame, C lock cleanup or exception unwind is skipped. */
extern void hl_wc_load_access(void), hl_wc_load_recover(void);
extern void hl_wc_store_access(void), hl_wc_store_recover(void);
extern void hl_wc_cas_load_access(void), hl_wc_cas_store_access(void), hl_wc_cas_recover(void);
__asm__(
".text\n.p2align 2\n"
".globl _hl_wc_atomic_load, _hl_wc_load_access, _hl_wc_load_recover\n"
"_hl_wc_atomic_load:\n_hl_wc_load_access:\n ldar w2, [x0]\n dmb ish\n str w2, [x1]\n mov w0, #0\n ret\n"
"_hl_wc_load_recover:\n mov w0, #-14\n ret\n"
".p2align 2\n.globl _hl_wc_atomic_store, _hl_wc_store_access, _hl_wc_store_recover\n"
"_hl_wc_atomic_store:\n dmb ish\n_hl_wc_store_access:\n stlr w1, [x0]\n dmb ish\n mov w0, #0\n ret\n"
"_hl_wc_store_recover:\n mov w0, #-14\n ret\n"
".p2align 2\n.globl _hl_wc_atomic_cas, _hl_wc_cas_load_access, _hl_wc_cas_store_access, _hl_wc_cas_recover\n"
"_hl_wc_atomic_cas:\n ldr w3, [x1]\n dmb ish\n"
"_hl_wc_cas_load_access:\n1:\n ldaxr w4, [x0]\n cmp w4, w3\n b.ne 2f\n"
"_hl_wc_cas_store_access:\n stlxr w5, w2, [x0]\n cbnz w5, 1b\n dmb ish\n mov w0, #1\n ret\n"
"2:\n clrex\n dmb ish\n str w4, [x1]\n mov w0, #0\n ret\n"
"_hl_wc_cas_recover:\n clrex\n mov w0, #-14\n ret\n");

_Static_assert(EFAULT == 14, "leaf error ABI");
_Static_assert(sizeof(arm_thread_state64_t) == ARM_THREAD_STATE64_COUNT * sizeof(natural_t), "ARM state ABI");
_Static_assert(offsetof(__Request__exception_raise_state_t, old_state) == 56, "public request ABI");
_Static_assert(offsetof(__Reply__exception_raise_state_t, new_state) == 44, "public reply ABI");

static mach_port_t fault_port;
static int init_error;
static pthread_once_t once = PTHREAD_ONCE_INIT;
static _Thread_local hl_wc_atomic_scope *current_scope;
static void (*recovery(uint64_t pc))(void) {
    if (pc == (uintptr_t)hl_wc_load_access) return hl_wc_load_recover;
    if (pc == (uintptr_t)hl_wc_store_access) return hl_wc_store_recover;
    if (pc == (uintptr_t)hl_wc_cas_load_access || pc == (uintptr_t)hl_wc_cas_store_access)
        return hl_wc_cas_recover;
    return NULL;
}
static void *fault_worker(void *unused) {
    (void)unused;
    for (;;) {
        union { mach_msg_header_t head; unsigned char bytes[4096]; } message;
        kern_return_t rc = mach_msg(&message.head, MACH_RCV_MSG, 0, sizeof(message),
            fault_port, MACH_MSG_TIMEOUT_NONE, MACH_PORT_NULL);
        if (rc != MACH_MSG_SUCCESS) abort();
        struct {
            mach_msg_header_t head;
            NDR_record_t ndr;
            kern_return_t result;
            int flavor;
            mach_msg_type_number_t count;
            natural_t state[ARM_THREAD_STATE64_COUNT];
        } reply;
        _Static_assert(offsetof(__typeof__(reply), state) == 44, "reply has no alignment hole");
        memset(&reply, 0, sizeof(reply));
        reply.head.msgh_bits = MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0);
        reply.head.msgh_remote_port = message.head.msgh_remote_port;
        reply.head.msgh_id = message.head.msgh_id + 100;
        reply.ndr = NDR_record;
        reply.result = KERN_FAILURE;
        reply.head.msgh_size = offsetof(__typeof__(reply), flavor);
        /* Public exc.defs EXCEPTION_STATE request 2402. All variable fields
         * are read only after exact length/count checks. No descriptors exist. */
        const size_t prefix = sizeof(mach_msg_header_t) + sizeof(NDR_record_t);
        if (!(message.head.msgh_bits & MACH_MSGH_BITS_COMPLEX) &&
            message.head.msgh_id == 2402 &&
            MACH_MSGH_BITS_REMOTE(message.head.msgh_bits) == MACH_MSG_TYPE_PORT_SEND_ONCE &&
            message.head.msgh_size >= prefix + 4 * sizeof(uint32_t)) {
            uint32_t fields[2];
            memcpy(fields, message.bytes + prefix, sizeof(fields));
            uint32_t count = fields[1];
            if (fields[0] == EXC_BAD_ACCESS && count == 2 &&
                !memcmp(message.bytes + sizeof(mach_msg_header_t), &NDR_record, sizeof(NDR_record_t))) {
                const size_t at = prefix + (2 + count) * sizeof(uint32_t);
                if (message.head.msgh_size >= at + 2 * sizeof(uint32_t)) {
                    memcpy(fields, message.bytes + at, sizeof(fields));
                    if (fields[0] == ARM_THREAD_STATE64 && fields[1] == ARM_THREAD_STATE64_COUNT &&
                        message.head.msgh_size == at + sizeof(fields) + sizeof(reply.state)) {
                        arm_thread_state64_t state;
                        memcpy(&state, message.bytes + at + sizeof(fields), sizeof(state));
                        void (*resume)(void) = recovery(arm_thread_state64_get_pc(state));
                        if (resume) {
                            arm_thread_state64_set_pc_fptr(state, resume);
                            memcpy(reply.state, &state, sizeof(state));
                            reply.result = KERN_SUCCESS;
                            reply.flavor = ARM_THREAD_STATE64;
                            reply.count = ARM_THREAD_STATE64_COUNT;
                            reply.head.msgh_size = offsetof(__typeof__(reply), state) + sizeof(reply.state);
                        }
                    }
                }
            }
        }
        /* Preserve the send-once reply right, and destroy any unexpected
         * descriptor rights. Unknown PCs refuse so untouched task/system
         * handlers receive the fault normally. */
        message.head.msgh_remote_port = MACH_PORT_NULL;
        mach_msg_destroy(&message.head);
        if (reply.head.msgh_remote_port != MACH_PORT_NULL &&
            mach_msg(&reply.head, MACH_SEND_MSG, reply.head.msgh_size, 0,
                MACH_PORT_NULL, MACH_MSG_TIMEOUT_NONE, MACH_PORT_NULL) != MACH_MSG_SUCCESS)
            mach_msg_destroy(&reply.head);
    }
}
static void initialize(void) {
    if (mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_RECEIVE, &fault_port) != KERN_SUCCESS) {
        init_error = 1; return;
    }
    if (mach_port_insert_right(mach_task_self(), fault_port, fault_port, MACH_MSG_TYPE_MAKE_SEND) != KERN_SUCCESS) {
        mach_port_mod_refs(mach_task_self(), fault_port, MACH_PORT_RIGHT_RECEIVE, -1); init_error = 1; return;
    }
    pthread_t worker;
    if (pthread_create(&worker, NULL, fault_worker, NULL) != 0) {
        mach_port_deallocate(mach_task_self(), fault_port);
        mach_port_mod_refs(mach_task_self(), fault_port, MACH_PORT_RIGHT_RECEIVE, -1); init_error = 1; return;
    }
    if (pthread_detach(worker) != 0) abort();
}
void hl_wc_atomic_end(hl_wc_atomic_scope *scope) {
    if (!scope->active) return;
    if (current_scope != scope) abort();
    /* Only one exception-mask bit was swapped, so at most one previous
     * row exists. Restore that entire original row in a single kernel call. */
    if (scope->count > 1 || (scope->count && scope->masks[0] != EXC_MASK_BAD_ACCESS)) abort();
    mach_port_t port = scope->count ? scope->ports[0] : MACH_PORT_NULL;
    exception_behavior_t behavior = scope->count ? scope->behaviors[0] : EXCEPTION_DEFAULT;
    thread_state_flavor_t flavor = scope->count ? scope->flavors[0] : THREAD_STATE_NONE;
    if (thread_set_exception_ports(scope->thread, EXC_MASK_BAD_ACCESS, port,
            behavior, flavor) != KERN_SUCCESS) abort();
    if (port != MACH_PORT_NULL) mach_port_deallocate(mach_task_self(), port);
    mach_port_deallocate(mach_task_self(), scope->thread);
    scope->active = 0; current_scope = NULL;
}
int hl_wc_atomic_begin(hl_wc_atomic_scope *scope) {
    if (current_scope) return -1; /* Never erase a live nested same-pointer scope. */
    memset(scope, 0, sizeof(*scope));
    if (pthread_once(&once, initialize) != 0 || init_error) return -1;
    scope->thread = mach_thread_self();
    scope->count = EXC_TYPES_COUNT;
    if (thread_swap_exception_ports(scope->thread, EXC_MASK_BAD_ACCESS, fault_port,
            EXCEPTION_STATE, ARM_THREAD_STATE64, scope->masks, &scope->count,
            scope->ports, scope->behaviors, scope->flavors) != KERN_SUCCESS) {
        mach_port_deallocate(mach_task_self(), scope->thread); return -1;
    }
    scope->active = 1; current_scope = scope;
    for (mach_msg_type_number_t i = 0; i < scope->count; i++)
        if (scope->ports[i] != MACH_PORT_NULL) {
            hl_wc_atomic_end(scope); return -1;
        }
    return 0;
}
