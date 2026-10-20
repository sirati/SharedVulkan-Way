/* SPDX-License-Identifier: MIT */
#define _GNU_SOURCE
#include <assert.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>
int os_create_anonymous_file(off_t);
int wc_cygwin_fd_words(int, uint32_t *);
int wc_cygwin_fd_from_words(const uint32_t *, size_t);
static _Alignas(4096) uint8_t bar[262144];
static volatile unsigned fallback_maps, fallback_unmaps;
static volatile unsigned live, drops;
int wc_bridge_lease_create(size_t len, uintptr_t *address, uint64_t *offset, size_t *capacity, uint64_t *lease)
{
    if (len > 65536 || live) return -1;
    ++live; *address = (uintptr_t)bar; *offset = 0; *capacity = 65536; *lease = 1;
    return 0;
}
int wc_bridge_lease_words(uint64_t lease, uint32_t *words)
{
    assert(lease == 1 && live);
    uint32_t value[7] = {24, 6, 42, 0, 0, 1, 0}; memcpy(words, value, sizeof(value)); return 0;
}
int wc_bridge_lease_import(const uint32_t *words, uintptr_t *address, size_t *length, uint64_t *lease)
{
    if (words[0] != 24 || words[1] != 6 || words[2] != 42) return -1;
    ++live; *address = (uintptr_t)(bar + 131072); *length = 4096; *lease = 2; return 0;
}
int wc_bridge_lease_grow(uint64_t lease, size_t len) { return lease != 1 || len > 65536 ? -1 : 0; }
int wc_bridge_lease_drop(uint64_t lease) { assert((lease == 1 || lease == 2) && live); --live; ++drops; return 0; }
int wc_bridge_bar_overlap(uintptr_t start, size_t length)
{
    if (length > UINTPTR_MAX - start) return 1;
    return length && start < (uintptr_t)bar + sizeof(bar) && (uintptr_t)bar < start + length;
}
static void *original_mmap(void *a, size_t n, int p, int f, int fd, off_t o)
{
    (void)a; (void)n; (void)p; (void)f; (void)fd; (void)o;
    ++fallback_maps; errno = EAGAIN; return MAP_FAILED;
}
static int original_munmap(void *a, size_t n) { (void)a; (void)n; ++fallback_unmaps; errno = EAGAIN; return -1; }
static int original_close(int fd) { return syscall(SYS_close, fd); }
static int original_ftruncate(int fd, off_t size) { return syscall(SYS_ftruncate, fd, size); }
static int original_dup(int fd) { return syscall(SYS_dup, fd); }
static int original_dup2(int fd, int target) { return syscall(SYS_dup2, fd, target); }
static int original_fcntl(int fd, int command, ...)
{
    va_list args; va_start(args, command);
    int result;
    if (command == F_GETFD || command == F_GETFL || command == F_GETOWN) result = syscall(SYS_fcntl, fd, command);
    else result = syscall(SYS_fcntl, fd, command, va_arg(args, int));
    va_end(args); return result;
}
void *dlopen(const char *name, int flags) { (void)flags; assert(!strcmp(name, "cygwin1.dll")); return bar; }
void *dlsym(void *handle, const char *name)
{
    assert(handle == bar);
#define SYMBOL(n) if (!strcmp(name, #n)) return (void *)original_##n
    SYMBOL(mmap); SYMBOL(munmap); SYMBOL(close); SYMBOL(ftruncate); SYMBOL(dup); SYMBOL(dup2); SYMBOL(fcntl);
    abort();
}
int dlclose(void *handle) { assert(handle == bar); return 0; }
int main(int argc, char **argv)
{
    assert(argc == 2);
    assert(!setenv("XDG_RUNTIME_DIR", argv[1], 1));
    assert(!setenv("WC_WAYLAND_RING", "1", 1));
    int fd = os_create_anonymous_file(4096);
    assert(fd >= 0);
    uint8_t *pixels = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    assert(pixels == bar); pixels[7] = 0xb3; assert(bar[7] == 0xb3);
    int duplicate = fcntl(fd, F_DUPFD_CLOEXEC, 0);
    assert(duplicate >= 0 && !close(fd));
    uint32_t words[7];
    assert(!wc_cygwin_fd_words(duplicate, words));
    assert(words[0] == 24 && words[1] == 6 && words[2] == 42);
    assert(mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, duplicate, 0) == MAP_FAILED && errno == ENOTSUP);
    assert(!ftruncate(duplicate, 16384));
    uint8_t *grown = mmap(NULL, 16384, PROT_READ | PROT_WRITE, MAP_SHARED, duplicate, 0);
    assert(grown == pixels && grown[7] == 0xb3);
    assert(ftruncate(duplicate, 65537) == -1 && errno == ENOTSUP);
    assert(munmap(pixels, 4095) == -1 && errno == EINVAL);
    assert(munmap(bar + 1024, 1024) == -1 && errno == EINVAL);
    assert(munmap((void *)((uintptr_t)bar - 4096), 8192) == -1 && errno == EINVAL);
    assert(munmap(bar + 8192, 4096) == -1 && errno == EINVAL);
    assert(munmap((void *)(UINTPTR_MAX - 1), 4096) == -1 && errno == EINVAL);
    assert(mmap(bar + 4096, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_FIXED, -1, 0) == MAP_FAILED && errno == ENOTSUP);
    assert(mmap((void *)(UINTPTR_MAX - 1), 4096, PROT_READ, MAP_PRIVATE, -1, 0) == MAP_FAILED && errno == ENOTSUP);
    assert(fallback_maps == 0 && fallback_unmaps == 0);
    assert(!munmap(pixels, 4096) && !munmap(grown, 16384));
    assert(!close(duplicate) && live == 0 && drops == 1);
    assert(munmap(bar + 1024, 1024) == -1 && errno == EINVAL);
    assert(mmap(bar, 4096, PROT_READ, MAP_PRIVATE | MAP_FIXED, -1, 0) == MAP_FAILED && errno == ENOTSUP);
    assert(fallback_maps == 0 && fallback_unmaps == 0);
    assert(mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, -1, 0) == MAP_FAILED && errno == EAGAIN);
    assert(munmap((void *)65536, 4096) == -1 && errno == EAGAIN);
    assert(fallback_maps == 1 && fallback_unmaps == 1);
    int incoming = wc_cygwin_fd_from_words(words, 7);
    assert(incoming >= 0);
    void *keymap = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, incoming, 0);
    assert(keymap == bar + 131072);
    assert(mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, incoming, 0) == MAP_FAILED);
    assert(!close(incoming) && live == 1);
    assert(!munmap(keymap, 4096) && live == 0 && drops == 2);
    for (unsigned i = 0; i < 128; ++i) {
        int recycled = os_create_anonymous_file(4096);
        assert(recycled >= 0 && !close(recycled) && live == 0);
    }
    int unknown = open("/dev/null", O_RDONLY);
    assert(unknown >= 0 && wc_cygwin_fd_words(unknown, words) == -1 && errno == ENOTSUP);
    close(unknown);
    assert(os_create_anonymous_file(65537) == -1 && errno == ENOSPC);
    return 0;
}
