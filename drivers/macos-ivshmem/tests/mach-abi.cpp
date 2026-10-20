// Public SDK ABI checks for the Rust Mach lender and IRQ adapter. Compile only.
#include <stddef.h>
#include <mach/mach.h>
#include <mach/mach_vm.h>
#include <IOKit/IOKitLib.h>
#include <IOKit/OSMessageNotification.h>
static_assert(sizeof(mach_msg_header_t) == 24);
static_assert(sizeof(mach_msg_port_descriptor_t) == 12);
static_assert(offsetof(mach_msg_port_descriptor_t, name) == 0);
static_assert(sizeof(mach_msg_audit_trailer_t) == 52);
static_assert(offsetof(mach_msg_audit_trailer_t, msgh_audit) == 20);
static_assert(sizeof(audit_token_t) == 32);
static_assert(MACH_MSG_TYPE_MOVE_SEND == 17);
static_assert(MACH_MSG_TYPE_MOVE_SEND_ONCE == 18);
static_assert(MACH_MSG_TYPE_COPY_SEND == 19);
static_assert(MACH_MSG_TYPE_MAKE_SEND_ONCE == 21);
static_assert(MACH_RCV_TRAILER_ELEMENTS(MACH_RCV_TRAILER_AUDIT) == (3u << 24));
static_assert(MACH_RCV_TIMED_OUT == 0x10004003);
static_assert(kIOAsyncCalloutFuncIndex == 1 && kIOAsyncCalloutRefconIndex == 2);
static_assert(kOSAsyncRef64Count == 8);
static_assert(VM_INHERIT_NONE == 2 && VM_FLAGS_ANYWHERE == 1);
static_assert(VM_PROT_READ == 1 && VM_PROT_WRITE == 2);
