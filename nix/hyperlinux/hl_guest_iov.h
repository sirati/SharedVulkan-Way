#ifndef HL_GUEST_IOV_H
#define HL_GUEST_IOV_H
#include "guest.h"
#include <sys/uio.h>
#include <errno.h>
#include <stdint.h>
#define HL_GUEST_IOV_MAX 1024
/* Preflight the entire guest range before record/datagram consumption. Every
 * chunk carries actual PTE permission and external-backing lifetime checks.
 * Native vectors preserve valid buffers spanning noncontiguous host mappings. */
static int hl_guest_iov_append(guest_t *g, uint64_t va, uint64_t length,
                               struct iovec *iov, int *count, int write) {
    if (length > UINT64_C(0x7ffff000)) { errno = EMSGSIZE; return -1; }
    uint64_t total = length;
    for (int i = 0; i < *count; i++) {
        if (iov[i].iov_len > UINT64_C(0x7ffff000) - total) { errno = EMSGSIZE; return -1; }
        total += iov[i].iov_len;
    }
    if (length > UINT64_C(0x7ffff000)) { errno = EMSGSIZE; return -1; }
    if (length && length - 1 > UINT64_MAX - va) { errno = EFAULT; return -1; }
    while (length) {
        if (*count == HL_GUEST_IOV_MAX) { errno = EOPNOTSUPP; return -1; }
        uint64_t available = 0;
        void *ptr = guest_ptr_range_access(g, va, length, &available, write);
        if (!ptr || !available || available > length) { errno = EFAULT; return -1; }
        iov[*count] = (struct iovec){ ptr, (size_t)available };
        ++*count; length -= available;
        if (length) va += available;
    }
    return 0;
}
#endif
