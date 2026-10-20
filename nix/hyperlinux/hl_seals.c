/* Guest seal enforcement. Darwin does NOT provide Linux kernel file seals.
 * The immutable token capability is separate bounded metadata; native importers
 * use its retained registry owner, not a synthetic F_GET_SEALS assertion.
 * Reserved xattrs live on the backing inode, so dup, SCM_RIGHTS and sibling hl
 * processes consult the same state. A native global flock serializes mutations.
 * Its path is outside the rooted guest's binds. No frame payload is copied. */
#include "hl_seals.h"
#include "wc_hyperlinux.h"
#ifndef HL_WC_SEAL_TEST
#include "hl_shared.h"
#endif
#ifndef HL_WC_SEAL_TEST
#include "syscall_internal.h"
#include "fd_object.h"
#else
#include <errno.h>
#define LINUX_EINVAL EINVAL
#define LINUX_EPERM EPERM
#define LINUX_EBUSY EBUSY
static int64_t linux_errno(void) { return -errno; }
#endif
#include <sys/file.h>
#include <sys/stat.h>
#include <sys/xattr.h>
#include <fcntl.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#ifdef __APPLE__
#define ATTR HL_WC_XATTR_PREFIX "seals.v1"
#define native_getattr(fd, ptr, len) fgetxattr(fd, ATTR, ptr, len, 0, 0)
#define native_setattr(fd, ptr, len, options) fsetxattr(fd, ATTR, ptr, len, 0, options)
#else
#define ATTR "user." HL_WC_XATTR_PREFIX "seals.v1"
#define ENOATTR ENODATA
#define native_getattr(fd, ptr, len) fgetxattr(fd, ATTR, ptr, len)
#define native_setattr(fd, ptr, len, options) fsetxattr(fd, ATTR, ptr, len, options)
#endif
#define MAGIC 0x314c4145534c4857ULL
#define SEAL_SEAL 1u
#define SEAL_SHRINK 2u
#define SEAL_GROW 4u
#define SEAL_WRITE 8u
#define SEAL_FUTURE_WRITE 16u
#define MAX_MAPS 128
#define TOKEN_MAX 448
struct range { uint64_t id, offset, len; };
typedef struct {
    uint64_t magic, next_id;
    uint32_t seals, token_len;
    uint8_t token[TOKEN_MAX];
    struct range writable[MAX_MAPS];
} metadata;
static int load(int fd, metadata *m) {
    struct stat st;
    if (fstat(fd, &st) < 0) return -1;
    if ((st.st_mode & S_IFMT) != S_IFREG) return 0;
    ssize_t n = native_getattr(fd, m, sizeof(*m));
    if (n < 0 && (errno == ENOATTR || errno == ENOTSUP)) return 0;
    if (n != (ssize_t)sizeof(*m) || m->magic != MAGIC || m->token_len > TOKEN_MAX || m->seals & ~31u) {
        if (n >= 0) errno = EINVAL;
        return -1;
    }
    return 1;
}
int hl_wc_seal_init(int fd, int allow) {
    metadata m = { .magic = MAGIC, .seals = allow ? 0 : SEAL_SEAL };
    return native_setattr(fd, &m, sizeof(m), XATTR_CREATE);
}
#ifndef HL_WC_SEAL_TEST
int hl_wc_seal_guest(int guestfd, int op, uint64_t value, hl_wc_seal_guard *g) {
    pthread_mutex_lock(&fd_lock);
    int fd = guestfd >= 0 && guestfd < FD_TABLE_SIZE && fd_table[guestfd].type != FD_CLOSED
        ? fcntl(fd_table[guestfd].host_fd, F_DUPFD_CLOEXEC, 0) : -1;
    pthread_mutex_unlock(&fd_lock);
    if (fd < 0) { errno = EBADF; return -1; }
    if (op == HL_WC_SEAL_PIN) {
        g->lockfd = -1; g->fd = fd; g->metadata = NULL; g->dirty = 0; g->ownfd = 1;
        return 0;
    }
    int result = hl_wc_seal_begin(fd, op, value, g);
    g->ownfd = 1;
    return result;
}
#endif
int hl_wc_seal_begin(int fd, int op, uint64_t value, hl_wc_seal_guard *guard) {
    guard->lockfd = -1; guard->metadata = NULL; guard->fd = fd; guard->dirty = 0; guard->ownfd = 0;
    metadata *m = calloc(1, sizeof(*m));
    if (!m) { errno = ENOMEM; return -1; }
    int present = load(fd, m);
    if (present <= 0) { free(m); return present < 0 ? -1 : 0; }
    char path[128];
#ifdef HL_WC_SEAL_TEST
    const char *base = getenv("HL_SEAL_TEST_DIR");
    if (!base) { free(m); errno = EINVAL; return -1; }
#else
    const char *base = "/var/tmp";
#endif
    snprintf(path, sizeof(path), "%s/.waycompose-hl-seals-%u.lock", base, (unsigned)geteuid());
    int lockfd = open(path, O_RDWR | O_CREAT | O_CLOEXEC | O_NOFOLLOW, 0600);
    struct stat st;
    if (lockfd < 0 || fstat(lockfd, &st) < 0 || st.st_uid != geteuid() ||
        (st.st_mode & S_IFMT) != S_IFREG || st.st_mode & 077 || flock(lockfd, LOCK_EX) < 0) {
        int e = lockfd < 0 ? errno : EACCES;
        if (lockfd >= 0) close(lockfd);
        free(m); errno = e; return -1;
    }
    guard->lockfd = lockfd; guard->metadata = m;
    if (load(fd, m) != 1) { errno = EINVAL; return -1; }
    if ((op == HL_WC_SEAL_WRITE || (op == HL_WC_SEAL_MAP && value)) &&
        (m->seals & (SEAL_WRITE | SEAL_FUTURE_WRITE))) { errno = EPERM; return -1; }
    if (op == HL_WC_SEAL_RESIZE) {
        if (fstat(fd, &st) < 0) return -1;
        if ((value < (uint64_t)st.st_size && (m->seals & SEAL_SHRINK)) ||
            (value > (uint64_t)st.st_size && (m->seals & SEAL_GROW))) { errno = EPERM; return -1; }
    }
    return 0;
}
void hl_wc_seal_end(hl_wc_seal_guard *g) {
    int saved = errno;
    if (g->metadata && g->dirty) {
        /* A metadata persistence failure cannot silently enable mutations. */
        if (native_setattr(g->fd, g->metadata, sizeof(metadata), XATTR_REPLACE) < 0) abort();
    }
    free(g->metadata); g->metadata = NULL;
    if (g->lockfd >= 0) close(g->lockfd);
    g->lockfd = -1;
    if (g->ownfd) close(g->fd);
    g->ownfd = 0;
    errno = saved;
}
int hl_wc_seal_writable(const hl_wc_seal_guard *g) {
    const metadata *m = g->metadata;
    return !m || !(m->seals & (SEAL_WRITE | SEAL_FUTURE_WRITE));
}
int hl_wc_seal_track(hl_wc_seal_guard *g, uint64_t offset, uint64_t len, int write, uint64_t *id) {
    *id = 0;
    if (!write || !g->metadata) return 0;
    metadata *m = g->metadata;
    for (int i = 0; i < MAX_MAPS; i++) if (!m->writable[i].id) {
        if (++m->next_id == 0) { errno = EOVERFLOW; return -1; }
        *id = m->next_id;
        m->writable[i] = (struct range){ *id, offset, len };
        g->dirty = 1; return 0;
    }
    /* A crashed mapper can conservatively leave a reservation. Never erase
     * it based only on PID reuse or a guess that the backing is no longer live. */
    errno = ENOMEM; return -1;
}
void hl_wc_seal_untrack(int fd, uint64_t id) {
    if (!id) return;
    hl_wc_seal_guard guard __attribute__((cleanup(hl_wc_seal_end))) = { .lockfd = -1 };
    if (hl_wc_seal_begin(fd, 0, 0, &guard) < 0 || !guard.metadata) return;
    metadata *m = guard.metadata;
    for (int i = 0; i < MAX_MAPS; i++) if (m->writable[i].id == id) {
        memset(&m->writable[i], 0, sizeof(m->writable[i])); guard.dirty = 1; break;
    }
}
int hl_wc_token_snapshot(int fd, uint8_t *out, uint64_t cap) {
    hl_wc_seal_guard guard __attribute__((cleanup(hl_wc_seal_end))) = { .lockfd = -1 };
    if (hl_wc_seal_begin(fd, 0, 0, &guard) < 0) return -errno;
    metadata *m = guard.metadata;
    if (!m || !m->token_len || cap < m->token_len) return -EINVAL;
    uint8_t current[TOKEN_MAX];
    if (pread(fd, current, m->token_len, 0) != m->token_len || memcmp(current, m->token, m->token_len)) return -EINVAL;
    memcpy(out, m->token, m->token_len);
    return (int)m->token_len;
}
int64_t hl_wc_seal_fcntl(int fd, int cmd, uint64_t arg) {
    hl_wc_seal_guard guard __attribute__((cleanup(hl_wc_seal_end))) = { .lockfd = -1 };
    if (hl_wc_seal_begin(fd, 0, 0, &guard) < 0) return linux_errno();
    metadata *m = guard.metadata;
    if (!m) return -LINUX_EINVAL;
    if (cmd == 1034) return m->seals;
    if (arg & ~31ULL) return -LINUX_EINVAL;
    if (m->seals & SEAL_SEAL) return -LINUX_EPERM;
    if (arg & SEAL_WRITE) for (int i = 0; i < MAX_MAPS; i++)
        if (m->writable[i].id) return -LINUX_EBUSY;
    /* Canonicalize only actual PRIME headers. Generic memfds retain seal
     * enforcement without being accepted as a graphics capability. */
    unsigned char bytes[TOKEN_MAX];
    ssize_t n = pread(fd, bytes, sizeof(bytes), 0);
    uint32_t token_len = n >= 16 ? ((uint32_t)bytes[12] | (uint32_t)bytes[13]<<8 |
        (uint32_t)bytes[14]<<16 | (uint32_t)bytes[15]<<24) : 0;
    unsigned resulting = m->seals | (unsigned)arg;
    if ((resulting & 7u) == 7u && (resulting & (SEAL_WRITE | SEAL_FUTURE_WRITE)) && n >= 44 &&
        memcmp(bytes, "SVDMABF1", 8) == 0 && token_len <= (uint32_t)n && token_len <= TOKEN_MAX) {
        for (int i = 0; i < MAX_MAPS; i++) if (m->writable[i].id && m->writable[i].offset < token_len)
            return -LINUX_EBUSY;
        /* Rust validates every encoded field and canonical byte exactly. */
        if (wc_hl_token_validate(bytes, token_len) < 0) return -LINUX_EINVAL;
        memcpy(m->token, bytes, token_len); m->token_len = token_len;
    }
    m->seals |= (uint32_t)arg; guard.dirty = 1;
    return 0;
}
/* An open description owns a local native registry reference. Adopt is also
 * used after SCM_RIGHTS and fork to rebuild process-local state from the same
 * protected inode metadata. No Rust pointer crosses processes. */
#ifndef HL_WC_SEAL_TEST
static void seal_file_destroy(hl_open_file_t *of) {
    if (of->state) wc_hl_prime_release(of->state);
    free(of->guest_path_hint);
}
static const hl_fd_ops_t seal_file_ops = { .destroy = seal_file_destroy };
int hl_wc_is_memfd(const hl_open_file_t *of) { return of && of->ops == &seal_file_ops; }
int hl_wc_memfd_adopt(hl_open_file_t *of, int fd) {
    metadata m;
    int present = load(fd, &m);
    if (present <= 0) return present;
    void *prime = NULL;
    if (m.token_len) {
        uint8_t bytes[TOKEN_MAX];
        int n = hl_wc_token_snapshot(fd, bytes, sizeof(bytes));
        if (n < 0) { errno = -n; return -1; }
        int rc = wc_hl_prime_register(fd, bytes, (uint64_t)n, &prime);
        if (rc < 0) { errno = -rc; return -1; }
    }
    pthread_mutex_lock(&fd_lock);
    void *old = of->state;
    of->ops = &seal_file_ops;
    of->state = prime;
    pthread_mutex_unlock(&fd_lock);
    if (prime) hl_wc_prime_attach(fd, prime);
    wc_hl_prime_release(old);
    return 1;
}
int hl_wc_memfd_restore(int gfd) {
    int fd = fd_to_host(gfd);
    if (fd < 0) return -1;
    metadata m;
    int present = load(fd, &m);
    if (present <= 0) return present;
    int alias = fcntl(fd, F_DUPFD_CLOEXEC, 0);
    if (alias < 0) return -1;
    int flags = fd_table[gfd].linux_flags;
    hl_open_file_t *of = hl_open_file_create(FD_REGULAR, &seal_file_ops, flags, NULL);
    if (!of) { close(alias); errno = ENOMEM; return -1; }
    if (hl_wc_memfd_adopt(of, alias) < 0) { hl_open_file_release(of); close(alias); return -1; }
    return hl_fd_install_at(gfd, of, alias, flags & LINUX_O_CLOEXEC, true) < 0 ? -1 : 1;
}

#endif

/* Lock only the native mutation, never guest memory resolution. Resolver
 * readers take the mapping-view lock, while unmap takes that lock before
 * retiring its writable reservation. Holding the seal lock across a guest
 * read/write would invert those locks. */
ssize_t hl_wc_write(int fd, const void *data, size_t len) {
    hl_wc_seal_guard g __attribute__((cleanup(hl_wc_seal_end))) = { .lockfd = -1 };
    if (hl_wc_seal_begin(fd, HL_WC_SEAL_WRITE, 0, &g) < 0) return -1;
    return write(fd, data, len);
}
ssize_t hl_wc_pwrite(int fd, const void *data, size_t len, off_t offset) {
    hl_wc_seal_guard g __attribute__((cleanup(hl_wc_seal_end))) = { .lockfd = -1 };
    if (hl_wc_seal_begin(fd, HL_WC_SEAL_WRITE, 0, &g) < 0) return -1;
    return pwrite(fd, data, len, offset);
}
ssize_t hl_wc_writev(int fd, const struct iovec *iov, int count) {
    hl_wc_seal_guard g __attribute__((cleanup(hl_wc_seal_end))) = { .lockfd = -1 };
    if (hl_wc_seal_begin(fd, HL_WC_SEAL_WRITE, 0, &g) < 0) return -1;
    return writev(fd, iov, count);
}
ssize_t hl_wc_pwritev(int fd, const struct iovec *iov, int count, off_t offset) {
    hl_wc_seal_guard g __attribute__((cleanup(hl_wc_seal_end))) = { .lockfd = -1 };
    if (hl_wc_seal_begin(fd, HL_WC_SEAL_WRITE, 0, &g) < 0) return -1;
    return pwritev(fd, iov, count, offset);
}
int hl_wc_ftruncate(int fd, off_t length) {
    hl_wc_seal_guard g __attribute__((cleanup(hl_wc_seal_end))) = { .lockfd = -1 };
    if (hl_wc_seal_begin(fd, HL_WC_SEAL_RESIZE, (uint64_t)length, &g) < 0) return -1;
    return ftruncate(fd, length);
}
