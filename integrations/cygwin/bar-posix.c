/* SPDX-License-Identifier: MIT
 * POSIX identities and lifetime references for original-BAR allocations.
 * Token files contain no pixels. mmap returns the broker mapping directly.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>
#ifdef __CYGWIN__
#define BRIDGE __declspec(dllimport)
#else
#define BRIDGE
#endif
BRIDGE int wc_bridge_bar_overlap(uintptr_t, size_t);
BRIDGE int wc_bridge_lease_create(size_t, uintptr_t *, uint64_t *, size_t *, uint64_t *);
BRIDGE int wc_bridge_lease_words(uint64_t, uint32_t *);
BRIDGE int wc_bridge_lease_import(const uint32_t *, uintptr_t *, size_t *, uint64_t *);
BRIDGE int wc_bridge_lease_grow(uint64_t, size_t);
BRIDGE int wc_bridge_lease_drop(uint64_t);

struct fd_ref { int fd; struct fd_ref *next; };
struct map_ref { size_t length; struct map_ref *next; };
struct pool {
    int hold, producer;
    dev_t device;
    ino_t inode;
    size_t size, capacity;
    void *address;
    uint64_t offset, lease;
    struct fd_ref *fds;
    struct map_ref *maps;
    struct pool *next;
};
static struct pool *pools;
static unsigned created, retired;
static pid_t bridge_process;
static pthread_mutex_t pool_lock = PTHREAD_MUTEX_INITIALIZER;

static void *runtime_symbol(const char *name)
{
    /* cygwin1 is the process runtime and remains loaded for the process life. */
    void *runtime = dlopen("cygwin1.dll", RTLD_NOW);
    if (!runtime) { errno = ENOSYS; return NULL; }
    void *symbol = dlsym(runtime, name);
    dlclose(runtime);
    if (!symbol) errno = ENOSYS;
    return symbol;
}
static int real_close(int fd)
{
    int (*function)(int) = runtime_symbol("close");
    return function ? function(fd) : -1;
}
static int real_ftruncate(int fd, off_t size)
{
    int (*function)(int, off_t) = runtime_symbol("ftruncate");
    return function ? function(fd, size) : -1;
}
static int real_dupfd(int fd)
{
    int (*function)(int, int, ...) = runtime_symbol("fcntl");
    return function ? function(fd, F_DUPFD_CLOEXEC, 0) : -1;
}
int wc_cygwin_ring_enabled(void)
{
    const char *value = getenv("WC_WAYLAND_RING");
    return getpid() == bridge_process && value && !strcmp(value, "1");
}
static struct pool *find_pool(int fd)
{
    struct stat st;
    if (fstat(fd, &st)) return NULL;
    for (struct pool *p = pools; p; p = p->next)
        if (p->device == st.st_dev && p->inode == st.st_ino) return p;
    return NULL;
}
static int add_fd(struct pool *p, int fd)
{
    for (struct fd_ref *r = p->fds; r; r = r->next) if (r->fd == fd) return 0;
    struct fd_ref *r = malloc(sizeof(*r));
    if (!r) return -1;
    *r = (struct fd_ref){fd, p->fds}; p->fds = r;
    return 0;
}
static void remove_fd(struct pool *p, int fd)
{
    struct fd_ref **next = &p->fds;
    while (*next) {
        if ((*next)->fd == fd) { struct fd_ref *old = *next; *next = old->next; free(old); return; }
        next = &(*next)->next;
    }
}
static void retire(struct pool *p)
{
    if (p->fds || p->maps) return;
    /* Native release drops only this local capability. Pending and remote
     * consumer references remain until actual receiver completion. */
    if (wc_bridge_lease_drop(p->lease)) return;
    struct pool **next = &pools;
    while (*next && *next != p) next = &(*next)->next;
    if (*next) *next = p->next;
    real_close(p->hold);
    ++retired;
    fprintf(stderr, "WC_CYGWIN_BAR_LOCAL_RELEASE lease=%llu consumer_completion_required=1\n", (unsigned long long)p->lease);
    free(p);
}
static int make_token(struct pool *p)
{
    const char *runtime = getenv("XDG_RUNTIME_DIR");
    if (!runtime || !*runtime) { errno = ENOENT; return -1; }
    char *name;
    if (asprintf(&name, "%s/wc-bar-token-XXXXXX", runtime) < 0) return -1;
    int fd = mkostemp(name, O_CLOEXEC);
    if (fd >= 0) unlink(name);
    free(name);
    if (fd < 0) return -1;
    if (real_ftruncate(fd, (off_t)p->size)) { real_close(fd); return -1; }
    struct stat st;
    if (fstat(fd, &st)) { real_close(fd); return -1; }
    p->hold = real_dupfd(fd);
    if (p->hold < 0) { real_close(fd); return -1; }
    p->device = st.st_dev; p->inode = st.st_ino;
    if (add_fd(p, fd)) { real_close(p->hold); real_close(fd); return -1; }
    p->next = pools; pools = p; ++created;
    return fd;
}
int os_create_anonymous_file(off_t size)
{
    if (!wc_cygwin_ring_enabled() || size <= 0 || (uint64_t)size > SIZE_MAX) { errno = ENOTSUP; return -1; }
    struct pool *p = calloc(1, sizeof(*p));
    if (!p) return -1;
    uintptr_t address;
    if (wc_bridge_lease_create((size_t)size, &address, &p->offset, &p->capacity, &p->lease)) {
        free(p); errno = ENOSPC; return -1;
    }
    p->address = (void *)address; p->size = (size_t)size; p->producer = 1;
    pthread_mutex_lock(&pool_lock);
    int fd = make_token(p);
    pthread_mutex_unlock(&pool_lock);
    if (fd < 0) { wc_bridge_lease_drop(p->lease); free(p); }
    return fd;
}
/* Invoked by libwayland's signature-aware decoder before event dispatch. */
#include "stream-posix.inc"

/* Fork children may inherit real local pipe fds for immediate dup/exec. They
 * never use copied native DLL pointers or acquire a BAR capability implicitly. */
static void posix_before_fork(void)
{
    pthread_mutex_lock(&pool_lock);
    for (struct stream_token *stream = streams; stream; stream = stream->next) stream->fork_shared = 1;
}
static void posix_parent_after_fork(void) { pthread_mutex_unlock(&pool_lock); }
static void posix_child_after_fork(void)
{
    pools = NULL; streams = NULL;
    created = retired = streams_created = streams_retired = 0;
    pthread_mutex_unlock(&pool_lock);
}
__attribute__((constructor)) static void initialize_posix_bridge(void)
{
    bridge_process = getpid();
    pthread_atfork(posix_before_fork, posix_parent_after_fork, posix_child_after_fork);
}

int wc_cygwin_fd_from_words(const uint32_t *words, size_t count)
{
    if (words && count >= 7 && words[0] == 24 && words[1] == 7) {
        int writer; uint64_t lease;
        if (wc_bridge_stream_import(words, &writer, &lease)) { errno = EPROTO; return -1; }
        int fd = stream_token(lease, writer, O_CLOEXEC);
        if (fd < 0) wc_bridge_lease_drop(lease);
        return fd;
    }

    if (count < 7 || words[0] != 24) { errno = EPROTO; return -1; }
    struct pool *p = calloc(1, sizeof(*p));
    if (!p) return -1;
    uintptr_t address;
    if (wc_bridge_lease_import(words, &address, &p->size, &p->lease)) { free(p); errno = EPROTO; return -1; }
    p->address = (void *)address; p->capacity = p->size;
    pthread_mutex_lock(&pool_lock);
    int fd = make_token(p);
    pthread_mutex_unlock(&pool_lock);
    if (fd < 0) { wc_bridge_lease_drop(p->lease); free(p); }
    else fprintf(stderr, "WC_CYGWIN_BAR_INCOMING lease=%llu bytes=%zu mapped_directly=1\n", (unsigned long long)p->lease, p->size);
    return fd;
}
static int bar_overlap(void *address, size_t length)
{
    uintptr_t start = (uintptr_t)address;
    if (length > UINTPTR_MAX - start) return 1;
    if (wc_bridge_bar_overlap(start, length)) return 1;
    uintptr_t end = start + length;
    for (struct pool *p = pools; p; p = p->next) {
        uintptr_t first = (uintptr_t)p->address;
        if (p->capacity > UINTPTR_MAX - first) return 1;
        if (length && start < first + p->capacity && first < end) return 1;
    }
    return 0;
}
void *mmap(void *address, size_t length, int prot, int flags, int fd, off_t offset)
{
    if (getpid() != bridge_process) { errno = ENOTSUP; return MAP_FAILED; }
    pthread_mutex_lock(&pool_lock);
    struct pool *p = find_pool(fd);
    if (p) {
        int shape = p->producer ? prot == (PROT_READ | PROT_WRITE) && flags == MAP_SHARED
            : prot == PROT_READ && (flags == MAP_SHARED || flags == MAP_PRIVATE);
        if (address || offset || !length || length > p->size || !shape || add_fd(p, fd)) {
            pthread_mutex_unlock(&pool_lock); errno = ENOTSUP; return MAP_FAILED;
        }
        struct map_ref *r = malloc(sizeof(*r));
        if (!r) { pthread_mutex_unlock(&pool_lock); return MAP_FAILED; }
        *r = (struct map_ref){length, p->maps}; p->maps = r;
        void *result = p->address;
        pthread_mutex_unlock(&pool_lock); return result;
    }
    if (bar_overlap(address, length)) { pthread_mutex_unlock(&pool_lock); errno = ENOTSUP; return MAP_FAILED; }
    pthread_mutex_unlock(&pool_lock);
    void *(*function)(void *, size_t, int, int, int, off_t) = runtime_symbol("mmap");
    return function ? function(address, length, prot, flags, fd, offset) : MAP_FAILED;
}
int munmap(void *address, size_t length)
{
    if (getpid() != bridge_process) { errno = ENOTSUP; return -1; }
    pthread_mutex_lock(&pool_lock);
    for (struct pool *p = pools; p; p = p->next) {
        if (p->address != address) continue;
        struct map_ref **next = &p->maps;
        while (*next && (*next)->length != length) next = &(*next)->next;
        if (!*next) { pthread_mutex_unlock(&pool_lock); errno = EINVAL; return -1; }
        struct map_ref *old = *next; *next = old->next; free(old);
        retire(p);
        pthread_mutex_unlock(&pool_lock); return 0;
    }
    if (bar_overlap(address, length)) { pthread_mutex_unlock(&pool_lock); errno = EINVAL; return -1; }
    pthread_mutex_unlock(&pool_lock);
    int (*function)(void *, size_t) = runtime_symbol("munmap");
    return function ? function(address, length) : -1;
}
int close(int fd)
{
    pthread_mutex_lock(&pool_lock);
    struct pool *p = find_pool(fd);
    struct stream_token *stream = find_stream(fd);
    int result = real_close(fd);
    if (!result && p) { remove_fd(p, fd); retire(p); }
    int join = !result && stream && stream_fd_closed(stream, fd);
    pthread_mutex_unlock(&pool_lock);
    if (join) stream_join(stream);
    return result;
}
int fclose(FILE *file)
{
    int (*function)(FILE *) = runtime_symbol("fclose");
    if (!function) return EOF;
    int fd = fileno(file);
    pthread_mutex_lock(&pool_lock);
    struct pool *p = find_pool(fd);
    struct stream_token *stream = find_stream(fd);
    pthread_mutex_unlock(&pool_lock);
    /* fclose may flush through a blocking pipe. Do not hold pool_lock while
     * the actual runtime drains stdio and closes its kernel descriptor. */
    int result = function(file);
    pthread_mutex_lock(&pool_lock);
    if (p) { remove_fd(p, fd); retire(p); }
    int join = stream && stream_fd_closed(stream, fd);
    pthread_mutex_unlock(&pool_lock);
    if (join) stream_join(stream);
    return result;
}
int ftruncate(int fd, off_t size)
{
    pthread_mutex_lock(&pool_lock);
    struct pool *p = find_pool(fd);
    if (!p) { pthread_mutex_unlock(&pool_lock); return real_ftruncate(fd, size); }
    if (!p->producer || size <= 0 || (uint64_t)size < p->size || (uint64_t)size > p->capacity) {
        pthread_mutex_unlock(&pool_lock); errno = ENOTSUP; return -1;
    }
    if (wc_bridge_lease_grow(p->lease, (size_t)size)) { pthread_mutex_unlock(&pool_lock); errno = ENOSPC; return -1; }
    int result = real_ftruncate(fd, size);
    if (!result) p->size = (size_t)size;
    pthread_mutex_unlock(&pool_lock); return result;
}
int dup(int fd)
{
    int (*function)(int) = runtime_symbol("dup");
    if (!function) return -1;
    pthread_mutex_lock(&pool_lock);
    struct pool *p = find_pool(fd);
    struct stream_token *stream = find_stream(fd);
    int result = function(fd);
    if (result >= 0 && p && add_fd(p, result)) { real_close(result); result = -1; }
    if (result >= 0 && stream && stream_add_fd(stream, result)) { real_close(result); result = -1; }
    pthread_mutex_unlock(&pool_lock); return result;
}
int dup2(int fd, int target)
{
    int (*function)(int, int) = runtime_symbol("dup2");
    if (!function) return -1;
    pthread_mutex_lock(&pool_lock);
    struct pool *source = find_pool(fd), *old = find_pool(target);
    struct stream_token *stream = find_stream(fd), *old_stream = find_stream(target);
    int result = function(fd, target);
    struct stream_token *join = NULL;
    if (result >= 0 && fd != target) {
        if (old) remove_fd(old, target);
        if (source && add_fd(source, result)) { real_close(result); result = -1; }
        if (old) retire(old);
        if (old_stream) stream_remove_fd(old_stream, target);
        if (stream && stream_add_fd(stream, target)) { real_close(target); result = -1; }
        if (old_stream) {
            if (old_stream != stream && stream_fd_closed(old_stream, target)) join = old_stream;
        }
    }
    pthread_mutex_unlock(&pool_lock);
    if (join) stream_join(join);
    return result;
}
int fcntl(int fd, int command, ...)
{
    int (*function)(int, int, ...) = runtime_symbol("fcntl");
    if (!function) return -1;
    int result;
    va_list args; va_start(args, command);
    pthread_mutex_lock(&pool_lock);
    struct pool *p = find_pool(fd);
    struct stream_token *stream = find_stream(fd);
    switch (command) {
    case F_GETFD: case F_GETFL: case F_GETOWN:
        result = function(fd, command); break;
    case F_SETFD: case F_SETFL: case F_SETOWN: case F_DUPFD: case F_DUPFD_CLOEXEC:
        result = function(fd, command, va_arg(args, int)); break;
    case F_GETLK: case F_SETLK: case F_SETLKW:
        result = function(fd, command, va_arg(args, struct flock *)); break;
    default:
        errno = ENOTSUP; result = -1; break;
    }
    if (result >= 0 && p && (command == F_DUPFD || command == F_DUPFD_CLOEXEC) && add_fd(p, result)) {
        real_close(result); result = -1;
    }
    if (result >= 0 && stream && (command == F_DUPFD || command == F_DUPFD_CLOEXEC) && stream_add_fd(stream, result)) {
        real_close(result); result = -1;
    }
    pthread_mutex_unlock(&pool_lock); va_end(args); return result;
}
int wc_cygwin_fd_words(int fd, uint32_t *words)
{
    pthread_mutex_lock(&pool_lock);
    struct pool *p = find_pool(fd);
    struct stream_token *stream = find_stream(fd);
    if (stream && words) {
        int result = wc_bridge_lease_words(stream->lease, words);
        pthread_mutex_unlock(&pool_lock);
        if (result) errno = ENOTSUP;
        return result;
    }
    if (!p || !words || add_fd(p, fd) || wc_bridge_lease_words(p->lease, words)) {
        pthread_mutex_unlock(&pool_lock); errno = ENOTSUP; return -1;
    }
    fprintf(stderr, "WC_CYGWIN_BAR_DESCRIPTOR offset=%llu bytes=%zu mapped_directly=1 leased=1\n", (unsigned long long)p->offset, p->size);
    pthread_mutex_unlock(&pool_lock); return 0;
}
__attribute__((destructor)) static void close_tokens(void)
{
    pthread_mutex_lock(&pool_lock);
    unsigned retained = 0, mappings = 0;
    /* DLL teardown precedes authoritative kernel process death. Other local
     * consumers may still exist. Only explicit close+munmap retired above;
     * the broker's retained process handle must retire abandoned producers.
     * Neither path is evidence that a remote consumer has drained. */
    for (struct pool *p = pools; p; p = p->next) {
        ++retained;
        for (struct map_ref *r = p->maps; r; r = r->next) ++mappings;
    }
    unsigned retained_streams = 0;
    for (struct stream_token *stream = streams; stream; stream = stream->next) ++retained_streams;
    fprintf(stderr, "WC_CYGWIN_STREAM_TEARDOWN created=%u retired=%u retained=%u actual_endpoint_completion_required=1\n",
            streams_created, streams_retired, retained_streams);
    fprintf(stderr, "WC_CYGWIN_BAR_TEARDOWN pools=%u local_retired=%u retained_capabilities=%u retained_maps=%u kernel_death_required=1 consumer_completion_required=1\n",
            created, retired, retained, mappings);
    pthread_mutex_unlock(&pool_lock);
}
