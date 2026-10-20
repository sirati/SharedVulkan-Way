/* SPDX-License-Identifier: MIT
 * Cygwin POSIX boundary for BAR-backed anonymous shared mappings. The local
 * file is an identity token only: its contents are never mapped or copied.
 * Unknown mappings retain Cygwin's ordinary mmap/munmap semantics.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#ifdef __CYGWIN__
__declspec(dllimport)
#endif
int wc_bridge_pool_alloc(size_t, uintptr_t *, uint64_t *, uint32_t *);

#define MAX_POOLS 64
struct pool {
    int hold;
    dev_t device;
    ino_t inode;
    size_t size;
    void *address;
    uint64_t offset;
    uint32_t arena;
    int mapped;
};
static struct pool pools[MAX_POOLS];
static unsigned count;
static pthread_mutex_t pool_lock = PTHREAD_MUTEX_INITIALIZER;

int wc_cygwin_ring_enabled(void)
{
    const char *value = getenv("WC_WAYLAND_RING");
    return value && !strcmp(value, "1");
}

static struct pool *find_pool(int fd)
{
    struct stat st;
    if (fstat(fd, &st)) return NULL;
    for (unsigned i = 0; i < count; ++i)
        if (pools[i].device == st.st_dev && pools[i].inode == st.st_ino)
            return &pools[i];
    return NULL;
}

/* The upstream application's OS compatibility library entry point. */
int os_create_anonymous_file(off_t size)
{
    if (!wc_cygwin_ring_enabled() || size <= 0 || (uint64_t)size > SIZE_MAX) {
        errno = ENOTSUP;
        return -1;
    }
    const char *runtime = getenv("XDG_RUNTIME_DIR");
    if (!runtime || !*runtime) { errno = ENOENT; return -1; }
    char *name;
    if (asprintf(&name, "%s/wc-bar-token-XXXXXX", runtime) < 0) return -1;
    int fd = mkostemp(name, O_CLOEXEC);
    if (fd >= 0) unlink(name);
    free(name);
    if (fd < 0) return -1;
    if (ftruncate(fd, size)) { close(fd); return -1; }
    struct stat st;
    if (fstat(fd, &st)) { close(fd); return -1; }
    int hold = fcntl(fd, F_DUPFD_CLOEXEC, 0);
    if (hold < 0) { close(fd); return -1; }
    pthread_mutex_lock(&pool_lock);
    if (count == MAX_POOLS) {
        pthread_mutex_unlock(&pool_lock); close(hold); close(fd);
        errno = ENOSPC; return -1;
    }
    uintptr_t address;
    uint64_t offset;
    uint32_t arena;
    if (wc_bridge_pool_alloc((size_t)size, &address, &offset, &arena)) {
        pthread_mutex_unlock(&pool_lock); close(hold); close(fd);
        errno = ENOSPC; return -1;
    }
    pools[count++] = (struct pool) {
        .hold = hold, .device = st.st_dev, .inode = st.st_ino,
        .size = (size_t)size, .address = (void *)address, .offset = offset,
        .arena = arena, .mapped = 0
    };
    pthread_mutex_unlock(&pool_lock);
    return fd;
}

/* This symbol is resolved from the shipped companion DLL ahead of cygwin1
 * when linking a Cygwin application. No application source is changed. */
void *mmap(void *address, size_t length, int prot, int flags, int fd, off_t offset)
{
    pthread_mutex_lock(&pool_lock);
    struct pool *p = find_pool(fd);
    if (p) {
        if (address || offset || length != p->size ||
            prot != (PROT_READ | PROT_WRITE) || flags != MAP_SHARED || p->mapped) {
            pthread_mutex_unlock(&pool_lock); errno = ENOTSUP; return MAP_FAILED;
        }
        p->mapped = 1;
        void *result = p->address;
        pthread_mutex_unlock(&pool_lock);
        return result;
    }
    pthread_mutex_unlock(&pool_lock);
    void *runtime = dlopen("cygwin1.dll", RTLD_NOW);
    if (!runtime) { errno = ENOSYS; return MAP_FAILED; }
    void *(*original)(void *, size_t, int, int, int, off_t) = dlsym(runtime, "mmap");
    if (!original) { dlclose(runtime); errno = ENOSYS; return MAP_FAILED; }
    void *result = original(address, length, prot, flags, fd, offset);
    dlclose(runtime);
    return result;
}

int munmap(void *address, size_t length)
{
    pthread_mutex_lock(&pool_lock);
    for (unsigned i = 0; i < count; ++i) {
        struct pool *p = &pools[i];
        if (p->address == address) {
            if (!p->mapped || length != p->size) {
                pthread_mutex_unlock(&pool_lock); errno = EINVAL; return -1;
            }
            p->mapped = 0;
            /* Do not recycle while a compositor buffer can retain the slice.
             * The native mapping owner lives until process teardown. */
            pthread_mutex_unlock(&pool_lock);
            return 0;
        }
    }
    pthread_mutex_unlock(&pool_lock);
    void *runtime = dlopen("cygwin1.dll", RTLD_NOW);
    if (!runtime) { errno = ENOSYS; return -1; }
    int (*original)(void *, size_t) = dlsym(runtime, "munmap");
    if (!original) { dlclose(runtime); errno = ENOSYS; return -1; }
    int result = original(address, length);
    dlclose(runtime);
    return result;
}

/* Seven native-endian Wayland words: byte-array length then the existing
 * little-endian ArenaSlice descriptor. Both supported peers are x86-64. */
int wc_cygwin_fd_words(int fd, uint32_t words[7])
{
    pthread_mutex_lock(&pool_lock);
    struct pool *p = find_pool(fd);
    if (!p || !p->mapped) {
        pthread_mutex_unlock(&pool_lock); errno = ENOTSUP; return -1;
    }
    fprintf(stderr, "WC_CYGWIN_BAR_DESCRIPTOR arena=%u offset=%llu bytes=%zu mapped_directly=1\n",
            p->arena, (unsigned long long)p->offset, p->size);
    words[0] = 24;
    words[1] = 4; /* wc_handle::Descriptor::ArenaSlice */
    words[2] = p->arena;
    words[3] = (uint32_t)p->offset;
    words[4] = (uint32_t)(p->offset >> 32);
    words[5] = (uint32_t)p->size;
    words[6] = (uint32_t)((uint64_t)p->size >> 32);
    pthread_mutex_unlock(&pool_lock);
    return 0;
}

__attribute__((destructor)) static void close_pool_tokens(void)
{
    pthread_mutex_lock(&pool_lock);
    for (unsigned i = 0; i < count; ++i) close(pools[i].hold);
    if (count) fprintf(stderr, "WC_CYGWIN_BAR_TEARDOWN pools=%u token_holds_closed=%u extents_recycled=0\n", count, count);
    pthread_mutex_unlock(&pool_lock);
}
