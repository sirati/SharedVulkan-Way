/* Connected AF_UNIX stream credentials. Identity comes only from Darwin.
 * The option is shared storage owned by the open description, never an fd
 * number. Native fd transfer preserves that storage for fork/hl peers. */
#include "hl_socket.h"
#include "syscall.h"
#include "syscall_internal.h"
#include "syscall_fs.h"
#include "hl_seals.h"
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/xattr.h>
#include <libproc.h>
#include <pthread.h>
#include <fcntl.h>
#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <stdio.h>
#include <stddef.h>
#define ATTR HL_WC_XATTR_PREFIX "socket.v1"
#define MAGIC UINT64_C(0x5743534f434b3031)
typedef struct { uint64_t magic; _Atomic uint32_t passcred; _Atomic uint32_t seqpacket; uint64_t endpoints[2]; _Atomic uint8_t read_shutdown[2]; } shared_option;
typedef struct { int fd; shared_option *option; int side; } socket_state;
static pthread_mutex_t state_lock = PTHREAD_RECURSIVE_MUTEX_INITIALIZER;
/* POSIX inode locks are process-owned and any close releases them. Every
 * private-state close uses this same local mutex, including ancillary cleanup.
 * The inode lock supplies interprocess exclusion and is released on death. */
int hl_wc_socket_native_close(int fd) {
    pthread_mutex_lock(&state_lock);
    int result = close(fd), saved = errno;
    pthread_mutex_unlock(&state_lock);
    errno = saved; return result;
}
#define close hl_wc_socket_native_close
static int pair_lock(socket_state *s, int type) {
    struct flock lock = { .l_type = type, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1 };
    return fcntl(s->fd, type == F_UNLCK ? F_SETLK : F_SETLKW, &lock);
}
static int stream_socket(int fd) {
    int type; socklen_t n = sizeof(type);
    struct sockaddr_storage addr; socklen_t len = sizeof(addr);
    if (getsockopt(fd, SOL_SOCKET, SO_TYPE, &type, &n) < 0 ||
        getsockname(fd, (struct sockaddr *)&addr, &len) < 0) return -1;
    if (type != SOCK_STREAM || addr.ss_family != AF_UNIX) { errno = EOPNOTSUPP; return -1; }
    return 0;
}
int hl_wc_socket_peer(int fd, hl_wc_ucred *cred) {
    uid_t uid; gid_t gid; pid_t pid; socklen_t n = sizeof(pid);
    if (stream_socket(fd) < 0 || getpeereid(fd, &uid, &gid) < 0 ||
        getsockopt(fd, SOL_LOCAL, LOCAL_PEERPID, &pid, &n) < 0) return -1;
    if (n != sizeof(pid) || pid <= 0) { errno = EOPNOTSUPP; return -1; }
    *cred = (hl_wc_ucred){ .pid = pid, .uid = uid, .gid = gid };
    return 0;
}
int hl_wc_socket_same_runtime(int fd) {
    hl_wc_ucred peer;
    char own[PROC_PIDPATHINFO_MAXSIZE], other[PROC_PIDPATHINFO_MAXSIZE];
    struct stat a, b;
    return hl_wc_socket_peer(fd, &peer) == 0 && peer.uid == geteuid() &&
        proc_pidpath(getpid(), own, sizeof(own)) > 0 &&
        proc_pidpath(peer.pid, other, sizeof(other)) > 0 &&
        stat(own, &a) == 0 && stat(other, &b) == 0 &&
        a.st_dev == b.st_dev && a.st_ino == b.st_ino;
}
int hl_wc_socket_is_state(int fd) {
    uint64_t magic = 0, contents = 0;
    struct stat st;
    return fstat(fd, &st) == 0 && S_ISREG(st.st_mode) && st.st_uid == geteuid() &&
        st.st_nlink == 0 && (st.st_mode & 0777) == 0600 && st.st_size == sizeof(shared_option) &&
        pread(fd, &contents, sizeof(contents), 0) == sizeof(contents) && contents == MAGIC &&
        fgetxattr(fd, ATTR, &magic, sizeof(magic), 0, 0) == sizeof(magic) && magic == MAGIC;
}
static socket_state *adopt(int fd) {
    socket_state *s = calloc(1, sizeof(*s));
    if (!s) return NULL;
    s->fd = fcntl(fd, F_DUPFD_CLOEXEC, 0);
    if (s->fd < 0) { free(s); return NULL; }
    s->option = mmap(NULL, sizeof(*s->option), PROT_READ | PROT_WRITE, MAP_SHARED, s->fd, 0);
    if (s->option == MAP_FAILED) { close(s->fd); free(s); return NULL; }
    return s;
}
static int match_side(socket_state *s, int native) {
    if (!atomic_load_explicit(&s->option->seqpacket, memory_order_acquire)) { s->side = 0; return 0; }
    struct stat st;
    if (fstat(native, &st) < 0) return -1;
    for (int side = 0; side < 2; side++) if (s->option->endpoints[side] == (uint64_t)st.st_ino) {
        s->side = side; return 0;
    }
    errno = EPROTO; return -1;
}
static void free_state(socket_state *s) {
    if (!s) return;
    munmap(s->option, sizeof(*s->option)); close(s->fd); free(s);
}
void hl_wc_socket_drop(hl_open_file_t *of) {
    socket_state *s = of->socket_state;
    free_state(s);
}
int hl_wc_socket_set(int fd, int enabled) {
    hl_fd_ref_t ref;
    if (hl_fd_get(fd, &ref) < 0) return -1;
    if (!ref.of || stream_socket(ref.host_fd) < 0) { hl_fd_put(&ref); errno = EOPNOTSUPP; return -1; }
    pthread_mutex_lock(&state_lock);
    socket_state *s = ref.of ? ref.of->socket_state : NULL;
    if (!s) {
        int backing = hl_wc_socket_private_file();
        if (backing >= 0) {
            uint64_t magic = MAGIC;
            if (ftruncate(backing, sizeof(shared_option)) == 0 &&
                fsetxattr(backing, ATTR, &magic, sizeof(magic), 0, XATTR_CREATE) == 0) {
                s = adopt(backing);
                if (s) { s->option->magic = MAGIC; ref.of->socket_state = s; }
            }
            close(backing);
        }
    }
    if (s && atomic_load_explicit(&s->option->seqpacket, memory_order_acquire)) {
        pthread_mutex_unlock(&state_lock); hl_fd_put(&ref); errno = EOPNOTSUPP; return -1;
    }
    if (s) atomic_store_explicit(&s->option->passcred, !!enabled, memory_order_release);
    pthread_mutex_unlock(&state_lock);
    hl_fd_put(&ref);
    return s ? 0 : -1;
}
int hl_wc_socket_get(int fd, int *enabled) {
    hl_fd_ref_t ref;
    if (hl_fd_get(fd, &ref) < 0) return -1;
    if (stream_socket(ref.host_fd) < 0) { hl_fd_put(&ref); return -1; }
    pthread_mutex_lock(&state_lock);
    socket_state *s = ref.of ? ref.of->socket_state : NULL;
    *enabled = s ? atomic_load_explicit(&s->option->passcred, memory_order_acquire) : 0;
    pthread_mutex_unlock(&state_lock);
    hl_fd_put(&ref); return 0;
}
static int export_option(hl_open_file_t *of) {
    pthread_mutex_lock(&state_lock);
    socket_state *s = of ? of->socket_state : NULL;
    int result = s ? fcntl(s->fd, F_DUPFD_CLOEXEC, 0) : -1;
    if (s && result < 0) result = -2;
    pthread_mutex_unlock(&state_lock);
    return result;
}
/* Caller owns this private alias and must use native_close, because closing
 * any alias would otherwise release an active process-owned inode lock. */
int hl_wc_socket_state_watch(hl_open_file_t *of) {
    int result = export_option(of);
    if (result == -2) return -1;
    if (result < 0) errno = EINVAL;
    return result;
}
int hl_wc_socket_export(int fd) {
    hl_fd_ref_t ref;
    if (hl_fd_get(fd, &ref) < 0) return -2;
    int result = export_option(ref.of);
    hl_fd_put(&ref); return result;
}
int hl_wc_socket_import(int fd, int backing) {
    if (!hl_wc_socket_is_state(backing)) { errno = EINVAL; return -1; }
    /* Fork import reconstructs legacy host aliases before option records. */
    pthread_mutex_lock(&fd_lock);
    if (fd >= 0 && fd < FD_TABLE_SIZE && fd_table[fd].type == FD_SOCKET && !fd_table[fd].desc) {
        int flags = fcntl(fd_table[fd].host_fd, F_GETFL);
        if (flags < 0) { pthread_mutex_unlock(&fd_lock); return -1; }
        hl_open_file_t *of = hl_open_file_create(FD_SOCKET, &hl_fd_ops_host_file,
            mac_to_linux_status_flags(flags), NULL);
        hl_descriptor_t *desc = of ? hl_descriptor_create(of, fd_table[fd].host_fd,
            fd_table[fd].linux_flags & LINUX_O_CLOEXEC, FD_SOCKET, NULL) : NULL;
        if (desc) { fd_table[fd].of = of; fd_table[fd].desc = desc; }
        else if (of) hl_open_file_release(of);
    }
    pthread_mutex_unlock(&fd_lock);
    hl_fd_ref_t ref;
    if (hl_fd_get(fd, &ref) < 0) return -1;
    if (!ref.of || stream_socket(ref.host_fd) < 0) { hl_fd_put(&ref); return -1; }
    socket_state *s = adopt(backing);
    if (!s) { hl_fd_put(&ref); return -1; }
    if (match_side(s, ref.host_fd) < 0) { free_state(s); hl_fd_put(&ref); return -1; }
    pthread_mutex_lock(&state_lock);
    hl_wc_socket_drop(ref.of);
    ref.of->socket_state = s;
    pthread_mutex_unlock(&state_lock);
    hl_fd_put(&ref); return 0;
}
void hl_wc_socket_transfer_end(hl_wc_socket_transfers *t) {
    for (unsigned i = 0; i < t->count; i++) close(t->fds[i]);
}
int hl_wc_socket_transfer(int channel, const int *guest, unsigned count,
    int *native, unsigned *out, hl_wc_socket_transfers *t, int trusted) {
    unsigned n = 0;
    for (unsigned i = 0; i < count; i++) {
        pthread_mutex_lock(&fd_lock);
        int gfd = guest[i];
        if (gfd < 0 || gfd >= FD_TABLE_SIZE || fd_table[gfd].type == FD_CLOSED) {
            pthread_mutex_unlock(&fd_lock); errno = EBADF; return -1;
        }
        int kind = fd_table[gfd].type;
        if (trusted && kind != FD_REGULAR && kind != FD_STDIO && kind != FD_PIPE && kind != FD_SOCKET) {
            pthread_mutex_unlock(&fd_lock); errno = EOPNOTSUPP; return -1;
        }
        hl_open_file_t *of = fd_table[gfd].of;
        if (of) hl_open_file_retain(of);
        int alias = fcntl(fd_table[gfd].host_fd, F_DUPFD_CLOEXEC, 0);
        pthread_mutex_unlock(&fd_lock);
        int state = alias >= 0 ? export_option(of) : -1;
        if (of) hl_open_file_release(of);
        if (alias < 0) return -1;
        if (t->count == 64 || n == 64) { close(alias); if (state >= 0) close(state); errno = EMSGSIZE; return -1; }
        native[n++] = alias; t->fds[t->count++] = alias;

        if (state == -2) return -1;
        if (state < 0) continue;
        /* A native consumer cannot preserve our option's shared identity.
         * Refuse rather than silently returning a false option on reimport. */
        if (!trusted && !hl_wc_socket_same_runtime(channel)) { close(state); errno = EOPNOTSUPP; return -1; }
        if (t->count == 64 || n == 64) { close(state); errno = EMSGSIZE; return -1; }
        native[n++] = state; t->fds[t->count++] = state;
    }
    *out = n; return 0;
}
static int alloc_socket(int, int, int, hl_descriptor_t **);
int hl_wc_socket_receive(int channel, int *native, unsigned count, int *guest, unsigned *out,
    int cloexec, int trusted, hl_wc_socket_transfers *received) {
    unsigned n = 0, start = received->count;
    for (unsigned i = 0; i < count; i++) {
        int fd = native[i]; native[i] = -1;
        if (hl_wc_socket_is_state(fd)) { close(fd); errno = EPROTO; goto fail; }
        if (received->count == 64) { close(fd); errno = EMSGSIZE; goto fail; }
        int type; socklen_t len = sizeof(type);
        int is_socket = getsockopt(fd, SOL_SOCKET, SO_TYPE, &type, &len) == 0;
        if (fcntl(fd, F_SETFD, cloexec ? FD_CLOEXEC : 0) < 0) { close(fd); goto fail; }
        int gfd; hl_descriptor_t *identity = NULL;
        if (is_socket) {
            int backing = -1;
            if (i + 1 < count && hl_wc_socket_is_state(native[i + 1])) {
                if (!trusted && !hl_wc_socket_same_runtime(channel)) { close(fd); goto fail; }
                backing = native[++i]; native[i] = -1;
            }
            gfd = alloc_socket(fd, 0, backing, &identity);
            if (backing >= 0) close(backing);
        } else {
            int native_flags = fcntl(fd, F_GETFL);
            if (native_flags < 0) { close(fd); goto fail; }
            struct stat st;
            if (fstat(fd, &st) < 0) { close(fd); goto fail; }
            hl_open_file_t *of = hl_open_file_create(S_ISFIFO(st.st_mode) ? FD_PIPE : FD_REGULAR, &hl_fd_ops_host_file, mac_to_linux_status_flags(native_flags), NULL);
            if (!of) { close(fd); goto fail; }
            if (hl_wc_memfd_adopt(of, fd) < 0) { hl_open_file_release(of); close(fd); goto fail; }
            gfd = hl_fd_install_retained(of, fd, cloexec ? LINUX_O_CLOEXEC : 0, &identity);
        }
        if (gfd < 0) goto fail;
        received->fds[received->count] = gfd;
        received->descriptors[received->count++] = identity;
        guest[n++] = gfd;
    }
    *out = n; return 0;
fail: {
    int saved = errno;
    for (unsigned i = 0; i < count; i++) if (native[i] >= 0) { close(native[i]); native[i] = -1; }
    for (unsigned i = start; i < received->count; i++) hl_wc_socket_received_close(received, received->fds[i]);
    received->count = start;
    errno = saved; return -1;
}}

static socket_state *new_state(void) {
    int backing = hl_wc_socket_private_file();
    if (backing < 0) return NULL;
    uint64_t magic = MAGIC;
    socket_state *state = NULL;
    if (ftruncate(backing, sizeof(shared_option)) == 0 &&
        fsetxattr(backing, ATTR, &magic, sizeof(magic), 0, XATTR_CREATE) == 0) {
        state = adopt(backing);
        if (state) state->option->magic = MAGIC;
    }
    int saved = errno; close(backing); errno = saved; return state;
}
int hl_wc_socket_pair_metadata(int pair[2]) {
    socket_state *s = new_state();
    if (!s) return -1;
    for (int i = 0; i < 2; i++) {
        struct stat st;
        if (fstat(pair[i], &st) < 0) { free_state(s); return -1; }
        /* XNU assigns its first synthetic inode lazily; reject zero. */
        if (!st.st_ino && fstat(pair[i], &st) < 0) { free_state(s); return -1; }
        if (!st.st_ino) { free_state(s); errno = EOPNOTSUPP; return -1; }
        s->option->endpoints[i] = (uint64_t)st.st_ino;
    }
    if (s->option->endpoints[0] == s->option->endpoints[1]) { free_state(s); errno = EOPNOTSUPP; return -1; }
    atomic_store_explicit(&s->option->seqpacket, 1, memory_order_release);
    int backing = s->fd;
    munmap(s->option, sizeof(*s->option)); free(s);
    return backing;
}
static int alloc_socket(int native, int seqpacket, int backing, hl_descriptor_t **identity) {
    hl_open_file_t *of = hl_open_file_create(FD_SOCKET, &hl_fd_ops_host_file, 0, NULL);
    if (!of) { close(native); return -1; }
    int flags = fcntl(native, F_GETFL);
    if (flags < 0) { hl_open_file_release(of); close(native); return -1; }
    atomic_store(&of->status_flags, mac_to_linux_status_flags(flags));
    if (stream_socket(native) == 0) {
        socket_state *s = backing >= 0 ? adopt(backing) : new_state();
        if (!s) { hl_open_file_release(of); close(native); return -1; }
        of->socket_state = s;
        if (seqpacket && !atomic_load(&s->option->seqpacket)) { hl_open_file_release(of); close(native); errno = EPROTO; return -1; }
        if (match_side(s, native) < 0) { hl_open_file_release(of); close(native); return -1; }
    } else if (seqpacket || backing >= 0) {
        hl_open_file_release(of); close(native); errno = EINVAL; return -1;
    }
    int cloexec = fcntl(native, F_GETFD);
    return hl_fd_install_retained(of, native, cloexec >= 0 && (cloexec & FD_CLOEXEC) ? LINUX_O_CLOEXEC : 0, identity);
}
int hl_wc_socket_alloc(int native) { return alloc_socket(native, 0, -1, NULL); }
int hl_wc_socket_alloc_retained(int native, int seqpacket, int backing, hl_descriptor_t **identity) { return alloc_socket(native, seqpacket, backing, identity); }

void hl_wc_socket_received_close(hl_wc_socket_transfers *t, int fd) {
    for (unsigned i = 0; i < t->count; i++) if (t->fds[i] == fd && t->descriptors[i]) {
        hl_fd_detached_t detached = {0};
        if (hl_fd_remove_exact(fd, t->descriptors[i], &detached) == 0) hl_fd_detached_finish(&detached);
        hl_descriptor_release(t->descriptors[i]); t->descriptors[i] = NULL; t->fds[i] = -1;
        return;
    }
}
void hl_wc_socket_received_end(hl_wc_socket_transfers *t) {
    for (unsigned i = 0; i < t->count; i++) hl_wc_socket_received_close(t, t->fds[i]);
    t->count = 0;
}
void hl_wc_socket_received_commit(hl_wc_socket_transfers *t) {
    for (unsigned i = 0; i < t->count; i++) if (t->descriptors[i]) hl_descriptor_release(t->descriptors[i]);
    t->count = 0;
}

int hl_wc_socket_enabled(hl_open_file_t *of) {
    pthread_mutex_lock(&state_lock);
    socket_state *s = of ? of->socket_state : NULL;
    int enabled = s ? atomic_load_explicit(&s->option->passcred, memory_order_acquire) : 0;
    pthread_mutex_unlock(&state_lock);
    return enabled;
}

int hl_wc_socket_seqpacket_of(hl_open_file_t *of) {
    pthread_mutex_lock(&state_lock);
    socket_state *s = of ? of->socket_state : NULL;
    int result = s && atomic_load_explicit(&s->option->seqpacket, memory_order_acquire);
    pthread_mutex_unlock(&state_lock); return result;
}
int hl_wc_socket_seqpacket(int fd) {
    hl_fd_ref_t ref;
    if (hl_fd_get(fd, &ref) < 0) return 0;
    int result = hl_wc_socket_seqpacket_of(ref.of);
    hl_fd_put(&ref); return result;
}

/* Mode zero at creation closes the mkstemp-before-unlink pathname race.
 * Restore owner permissions only after unlink for native xattr authorization.
 * The seqpacket caller refuses native root, which bypasses inode checks.
 * Guest fd tables never
 * expose these fds. No writable pathname alias exists after unlink. */
int hl_wc_socket_private_file(void) {
    char path[96];
    for (unsigned attempt = 0; attempt < 16; attempt++) {
        uint64_t random[2]; arc4random_buf(random, sizeof(random));
        snprintf(path, sizeof(path), "/tmp/hl-private-%016llx%016llx",
            (unsigned long long)random[0], (unsigned long long)random[1]);
        int fd = open(path, O_CREAT | O_EXCL | O_RDWR | O_CLOEXEC, 0);
        if (fd < 0) { if (errno == EEXIST) continue; return -1; }
        if (unlink(path) < 0) { int saved = errno; close(fd); errno = saved; return -1; }
        if (fchmod(fd, 0600) < 0) { int saved = errno; close(fd); errno = saved; return -1; }
        return fd;
    }
    errno = EEXIST; return -1;
}

void hl_wc_socket_native_end(hl_wc_socket_native_guard *guard) {
    if (!guard->message) return;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(guard->message); c; c = CMSG_NXTHDR(guard->message, c)) {
        if (c->cmsg_len < CMSG_LEN(0)) break;
        if (c->cmsg_level != SOL_SOCKET || c->cmsg_type != SCM_RIGHTS) continue;
        unsigned count = (unsigned)((c->cmsg_len - CMSG_LEN(0)) / sizeof(int));
        int *fds = (int *)CMSG_DATA(c);
        for (unsigned i = 0; i < count; i++) if (fds[i] >= 0) { close(fds[i]); fds[i] = -1; }
    }
}

int hl_wc_socket_read_closed(hl_open_file_t *of, int peer) {
    pthread_mutex_lock(&state_lock);
    socket_state *s = of ? of->socket_state : NULL;
    int result = s && atomic_load_explicit(&s->option->seqpacket, memory_order_acquire) &&
        atomic_load_explicit(&s->option->read_shutdown[s->side ^ !!peer], memory_order_acquire);
    pthread_mutex_unlock(&state_lock); return result;
}
/* Each native attempt is nonblocking while holding the pair inode lock.
 * A sender cannot commit after a receiver observes logical shutdown and an
 * empty queue. Blocking callers release the lock before waiting for readiness. */
ssize_t hl_wc_socket_record_attempt(hl_open_file_t *of, int native,
    struct msghdr *message, int flags, int writing) {
    pthread_mutex_lock(&state_lock);
    socket_state *s = of ? of->socket_state : NULL;
    if (!s || !atomic_load(&s->option->seqpacket)) {
        pthread_mutex_unlock(&state_lock); errno = EINVAL; return -1;
    }
    if (pair_lock(s, F_WRLCK) < 0) {
        int saved = errno; pthread_mutex_unlock(&state_lock); errno = saved; return -1;
    }
    ssize_t result;
    if (writing && atomic_load_explicit(&s->option->read_shutdown[s->side ^ 1], memory_order_acquire)) {
        result = -1; errno = EPIPE;
    } else {
        result = writing ? sendmsg(native, message, flags | MSG_DONTWAIT)
                         : recvmsg(native, message, flags | MSG_DONTWAIT);
        if (!writing && result < 0 && (errno == EAGAIN || errno == EWOULDBLOCK) &&
            atomic_load_explicit(&s->option->read_shutdown[s->side], memory_order_acquire)) {
            result = 0;
        }
    }
    int saved = errno;
    pair_lock(s, F_UNLCK);
    pthread_mutex_unlock(&state_lock);
    errno = saved; return result;
}
int hl_wc_socket_shutdown_pair(hl_open_file_t *of, int native, int how) {
    if (how < 0 || how > 2) { errno = EINVAL; return -1; }
    pthread_mutex_lock(&state_lock);
    socket_state *s = of ? of->socket_state : NULL;
    if (!s || !atomic_load(&s->option->seqpacket)) {
        pthread_mutex_unlock(&state_lock); errno = EINVAL; return -1;
    }
    if (pair_lock(s, F_WRLCK) < 0) {
        int saved = errno; pthread_mutex_unlock(&state_lock); errno = saved; return -1;
    }
    /* Darwin SHUT_RD destroys queued records and their rights. Preserve the
     * queue, mark logical shutdown, then let receives drain it before EOF. */
    int result = 0;
    if (how != SHUT_WR) {
        /* A single-byte write both publishes the shared mmap marker and
         * emits NOTE_WRITE. No committed store can precede a failed wake.
         * Byte-sized flags avoid a partially visible multi-byte update.
         * Darwin's coherent MAP_SHARED file mapping is the reader view;
         * no native userspace stores compete with these kernel byte writes. */
        uint8_t closed = 1;
        ssize_t written;
        do { written = pwrite(s->fd, &closed, sizeof(closed),
            offsetof(shared_option, read_shutdown) + s->side); } while (written < 0 && errno == EINTR);
        if (written != sizeof(closed)) { if (written >= 0) errno = EIO; result = -1; }
    }
    if (!result && how != SHUT_RD) result = shutdown(native, SHUT_WR);
    if (result < 0 && errno == ENOTCONN) result = 0;
    int saved = errno;
    pair_lock(s, F_UNLCK);
    pthread_mutex_unlock(&state_lock);
    errno = saved; return result;
}
