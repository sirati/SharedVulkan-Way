#define _GNU_SOURCE
#include "wc_hyperlinux.h"
#include <assert.h>
#include <stddef.h>
#include <stdint.h>
#include <stdatomic.h>
#include <sys/mman.h>
#include <fcntl.h>
#include <unistd.h>

_Static_assert(sizeof(wc_hl_mapping) == 40, "native ABI layout");
_Static_assert(offsetof(wc_hl_mapping, file_offset) == 32, "native ABI offset");
int main(void) {
    int fd = memfd_create("hl-ffi-check", MFD_CLOEXEC);
    assert(fd >= 0 && ftruncate(fd, 65536) == 0);
    wc_hl_backing *a = NULL, *b = NULL;
    wc_hl_mapping ai, bi;
    assert(wc_hl_backing_map(fd, 4096, 8192, 1, &a, &ai) == 0);
    int transferred = -1;
    assert(wc_hl_backing_export_fd(a, &transferred) == 0);
    assert(fcntl(transferred, F_GETFD) & FD_CLOEXEC);
    assert(wc_hl_backing_map(transferred, 4096, 8192, 1, &b, &bi) == 0);
    close(fd); close(transferred);
    _Atomic uint32_t *aw = (_Atomic uint32_t *)((char *)ai.base + ai.data_offset);
    _Atomic uint32_t *bw = (_Atomic uint32_t *)((char *)bi.base + bi.data_offset);
    atomic_store_explicit(aw, 27, memory_order_release);
    assert(atomic_load_explicit(bw, memory_order_acquire) == 27);
    wc_hl_backing_release(a);
    atomic_store_explicit(bw, 29, memory_order_release);
    assert(atomic_load_explicit(bw, memory_order_acquire) == 29);
    wc_hl_backing_release(b);
    return 0;
}
