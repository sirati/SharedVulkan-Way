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
#define ATTR HL_WC_XATTR_PREFIX "socket.v1"
#define MAGIC UINT64_C(0x5743534f434b3031)
typedef struct { uint64_t magic; _Atomic uint32_t passcred; _Atomic uint32_t seqpacket; } shared_option;
typedef struct { int fd; shared_option *option; } socket_state;
static pthread_mutex_t state_lock = PTHREAD_MUTEX_INITIALIZER;
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
        st.st_nlink == 0 && (st.st_mode & 0777) == 0 && st.st_size == sizeof(shared_option) &&
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
void hl_wc_socket_drop(hl_open_file_t *of) {
    socket_state *s = of->socket_state;
    if (!s) return;
    munmap(s->option, sizeof(*s->option)); close(s->fd); free(s);
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
    int *native, unsigned *out, hl_wc_socket_transfers *t) {
    unsigned n = 0;
    for (unsigned i = 0; i < count; i++) {
        hl_fd_ref_t ref;
        if (hl_fd_get(guest[i], &ref) < 0) return -1;
        int alias = fcntl(ref.host_fd, F_DUPFD_CLOEXEC, 0);
        int state = alias >= 0 ? export_option(ref.of) : -1;
        hl_fd_put(&ref);
        if (alias < 0) return -1;
        if (t->count == 64 || n == 64) { close(alias); if (state >= 0) close(state); errno = EMSGSIZE; return -1; }
        native[n++] = alias; t->fds[t->count++] = alias;

        if (state == -2) return -1;
        if (state < 0) continue;
        /* A native consumer cannot preserve our option's shared identity.
         * Refuse rather than silently returning a false option on reimport. */
        if (!hl_wc_socket_same_runtime(channel)) { close(state); errno = EOPNOTSUPP; return -1; }
        if (t->count == 64 || n == 64) { close(state); errno = EMSGSIZE; return -1; }
        native[n++] = state; t->fds[t->count++] = state;
    }
    *out = n; return 0;
}
static int alloc_socket(int, int, int);
int hl_wc_socket_receive(int channel, int *native, unsigned count, int *guest, unsigned *out, int cloexec) {
    unsigned n = 0;
    for (unsigned i = 0; i < count; i++) {
        int fd = native[i]; native[i] = -1;
        if (hl_wc_socket_is_state(fd)) { close(fd); goto fail; }
        int type; socklen_t len = sizeof(type);
        int is_socket = getsockopt(fd, SOL_SOCKET, SO_TYPE, &type, &len) == 0;
        if (fcntl(fd, F_SETFD, cloexec ? FD_CLOEXEC : 0) < 0) { close(fd); goto fail; }
        int gfd;
        if (is_socket) {
            int backing = -1;
            if (i + 1 < count && hl_wc_socket_is_state(native[i + 1])) {
                if (!hl_wc_socket_same_runtime(channel)) { close(fd); goto fail; }
                backing = native[++i]; native[i] = -1;
            }
            gfd = alloc_socket(fd, 0, backing);
            if (backing >= 0) close(backing);
        }
        else {
            int native_flags = fcntl(fd, F_GETFL);
            if (native_flags < 0) { close(fd); goto fail; }
            hl_open_file_t *of = hl_open_file_create(FD_REGULAR, &hl_fd_ops_host_file, mac_to_linux_status_flags(native_flags), NULL);
            if (!of) { close(fd); goto fail; }
            if (hl_wc_memfd_adopt(of, fd) < 0) { hl_open_file_release(of); close(fd); goto fail; }
            gfd = hl_fd_install(of, fd, cloexec ? LINUX_O_CLOEXEC : 0);
        }
        if (gfd < 0) goto fail;
        guest[n++] = gfd;
    }
    *out = n; return 0;
fail:
    for (unsigned i = 0; i < count; i++) if (native[i] >= 0) close(native[i]);
    for (unsigned i = 0; i < n; i++) sys_close(guest[i]);
    errno = EINVAL; return -1;
}

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
static int alloc_socket(int native, int seqpacket, int backing) {
    hl_open_file_t *of = hl_open_file_create(FD_SOCKET, &hl_fd_ops_host_file, 0, NULL);
    if (!of) { close(native); return -1; }
    int flags = fcntl(native, F_GETFL);
    if (flags < 0) { hl_open_file_release(of); close(native); return -1; }
    atomic_store(&of->status_flags, mac_to_linux_status_flags(flags));
    if (stream_socket(native) == 0) {
        socket_state *s = backing >= 0 ? adopt(backing) : new_state();
        if (!s) { hl_open_file_release(of); close(native); return -1; }
        of->socket_state = s;
        if (seqpacket) atomic_store(&s->option->seqpacket, 1);
    } else if (seqpacket || backing >= 0) {
        hl_open_file_release(of); close(native); errno = EINVAL; return -1;
    }
    int cloexec = fcntl(native, F_GETFD);
    return hl_fd_install(of, native, cloexec >= 0 && (cloexec & FD_CLOEXEC) ? LINUX_O_CLOEXEC : 0);
}
int hl_wc_socket_alloc(int native) { return alloc_socket(native, 0, -1); }
int hl_wc_socket_alloc_seqpacket(int native) { return alloc_socket(native, 1, -1); }

void hl_wc_socket_received_end(hl_wc_socket_transfers *t) {
    for (unsigned i = 0; i < t->count; i++) if (t->fds[i] >= 0) sys_close(t->fds[i]);
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
        return fd;
    }
    errno = EEXIST; return -1;
}
