/* SPDX-License-Identifier: MIT */
#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>

int os_create_anonymous_file(off_t);
int wc_cygwin_fd_words(int, uint32_t *);
static _Alignas(4096) uint8_t bar[65536];
static unsigned allocations;
int wc_bridge_pool_alloc(size_t len, uintptr_t *address, uint64_t *offset, uint32_t *arena)
{
    if (len > sizeof(bar) || allocations++) return -1;
    *address = (uintptr_t)bar;
    *offset = 131072;
    *arena = 42;
    return 0;
}
int main(int argc, char **argv)
{
    assert(argc == 2);
    assert(!setenv("XDG_RUNTIME_DIR", argv[1], 1));
    assert(!setenv("WC_WAYLAND_RING", "1", 1));
    int fd = os_create_anonymous_file(4096);
    assert(fd >= 0);
    void *pixels = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    assert(pixels == bar);
    ((uint8_t *)pixels)[7] = 0xb3;
    assert(bar[7] == 0xb3);
    int duplicate = fcntl(fd, F_DUPFD_CLOEXEC, 0);
    assert(duplicate >= 0);
    close(fd);
    uint32_t words[7];
    assert(!wc_cygwin_fd_words(duplicate, words));
    assert(words[0] == 24 && words[1] == 4 && words[2] == 42);
    assert(words[3] == 131072 && words[4] == 0 && words[5] == 4096 && words[6] == 0);
    assert(mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, duplicate, 0) == MAP_FAILED);
    assert(errno == ENOTSUP);
    assert(munmap(pixels, 4095) == -1 && errno == EINVAL);
    assert(!munmap(pixels, 4096));
    assert(wc_cygwin_fd_words(duplicate, words) == -1 && errno == ENOTSUP);
    close(duplicate);
    int unknown = open("/dev/null", O_RDONLY);
    assert(unknown >= 0 && wc_cygwin_fd_words(unknown, words) == -1 && errno == ENOTSUP);
    close(unknown);
    assert(os_create_anonymous_file(65537) == -1 && errno == ENOSPC);
    return 0;
}
