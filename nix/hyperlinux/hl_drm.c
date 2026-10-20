/* Native calls into the existing portable sv-drm ioctl state machine. */
#include "hl_drm.h"
#include "wc_hyperlinux.h"
#include "device.h"
#include "syscall_internal.h"
#include "syscall_proc.h"
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <stdatomic.h>
#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <stdio.h>

/* Implemented by the shared native-file seal adapter. */
extern int hl_wc_token_snapshot(int, uint8_t *, uint64_t);
static atomic_int open_count;
typedef struct { wc_hl_drm *drm; int peer; } drm_open;
int hl_wc_drm_has_open(void) { return atomic_load(&open_count) != 0; }
static int guest_range(const guest_t *g, uint64_t addr, uint64_t len, int prot) {
    if (!addr || len > UINT64_MAX - addr) return 0;
    uint64_t end = addr + len;
    while (addr < end) {
        uint64_t next = addr;
        for (int i = 0; i < g->nregions; i++) {
            const guest_region_t *r = &g->regions[i];
            if (r->start <= addr && addr < r->end && (r->prot & prot) == prot) {
                next = r->end < end ? r->end : end; break;
            }
        }
        if (next == addr) return 0;
        addr = next;
    }
    return 1;
}
static int32_t read_guest(void *context, uint64_t addr, uint8_t *out, uint64_t len) {
    guest_t *g = context;
    pthread_mutex_lock(&mmap_lock);
    int ok = guest_range(g, addr, len, LINUX_PROT_READ) && guest_read(g, addr, out, len) == 0;
    pthread_mutex_unlock(&mmap_lock);
    return ok ? 0 : -EFAULT;
}
static int32_t write_guest(void *context, uint64_t addr, const uint8_t *data, uint64_t len) {
    guest_t *g = context;
    pthread_mutex_lock(&mmap_lock);
    int ok = guest_range(g, addr, len, LINUX_PROT_WRITE) && guest_write(g, addr, data, len) == 0;
    pthread_mutex_unlock(&mmap_lock);
    return ok ? 0 : -EFAULT;
}
static int32_t token_guest(void *g, int32_t fd, uint8_t *out, uint64_t cap) {
    (void)g;
    pthread_mutex_lock(&fd_lock);
    int native = fd >= 0 && fd < FD_TABLE_SIZE && fd_table[fd].type != FD_CLOSED
        ? fcntl(fd_table[fd].host_fd, F_DUPFD_CLOEXEC, 0) : -1;
    pthread_mutex_unlock(&fd_lock);
    if (native < 0) return -EBADF;
    int result = hl_wc_token_snapshot(native, out, cap);
    close(native);
    return result;
}
/* Same algorithm as sv-drm relay_api::service_name, including long-path hash. */
static int service_matches(const uint8_t *name, uint64_t len, const char *prefix, const char *path) {
    if (!path) return 0;
    char expected[512];
    int n = snprintf(expected, sizeof(expected), "%s%s", prefix, path);
    if (n >= 64) {
        uint64_t hash = 0xcbf29ce484222325ULL;
        for (const unsigned char *p = (const unsigned char *)path; *p; p++) {
            hash ^= *p; hash *= 0x100000001b3ULL;
        }
        n = snprintf(expected, sizeof(expected), "%s#%016llx", prefix, (unsigned long long)hash);
    }
    return n > 0 && (uint64_t)n == len && memcmp(name, expected, len) == 0;
}
static int32_t relay_guest(void *g, int32_t fd, const uint8_t *name, uint64_t len) {
    (void)g;
    const char *gpu = getenv("SV_SOCKET"), *bell = getenv("SV_BELL");
    unsigned char tag;
    if (service_matches(name, len, "sv-gpu:", gpu)) tag = 1;
    else if (service_matches(name, len, "sv-bell:", bell)) tag = 2;
    else return -ENOENT;
    const char *endpoint = getenv("WC_HL_RELAY");
    if (!endpoint || !*endpoint) return -ENOENT;
    pthread_mutex_lock(&fd_lock);
    int native = fd >= 0 && fd < FD_TABLE_SIZE && fd_table[fd].type == FD_SOCKET
        ? fcntl(fd_table[fd].host_fd, F_DUPFD_CLOEXEC, 0) : -1;
    pthread_mutex_unlock(&fd_lock);
    if (native < 0) return -EBADF;
    uid_t uid; gid_t gid; int type = 0; socklen_t size = sizeof(type);
    struct sockaddr_storage addr; socklen_t addrlen = sizeof(addr);
    if (getpeereid(native, &uid, &gid) != 0 || uid != geteuid() ||
        getsockopt(native, SOL_SOCKET, SO_TYPE, &type, &size) != 0 || type != SOCK_STREAM ||
        getsockname(native, (struct sockaddr *)&addr, &addrlen) != 0 || addr.ss_family != AF_UNIX) {
        close(native); return -EPERM;
    }
    int control = socket(AF_UNIX, SOCK_STREAM, 0);
    if (control < 0) { int e = errno; close(native); return -e; }
    fcntl(control, F_SETFD, FD_CLOEXEC);
    int no_sigpipe = 1;
    setsockopt(control, SOL_SOCKET, SO_NOSIGPIPE, &no_sigpipe, sizeof(no_sigpipe));
    struct sockaddr_un un = { .sun_family = AF_UNIX };
    if (strlen(endpoint) >= sizeof(un.sun_path)) { close(control); close(native); return -ENAMETOOLONG; }
    strcpy(un.sun_path, endpoint);
    if (connect(control, (struct sockaddr *)&un, sizeof(un)) < 0) {
        int e = errno; close(control); close(native); return -e;
    }
    struct iovec iov = { .iov_base = &tag, .iov_len = 1 };
    union { struct cmsghdr align; unsigned char bytes[CMSG_SPACE(sizeof(int))]; } ancillary = {0};
    struct msghdr message = { .msg_iov = &iov, .msg_iovlen = 1,
        .msg_control = ancillary.bytes, .msg_controllen = sizeof(ancillary.bytes) };
    struct cmsghdr *c = CMSG_FIRSTHDR(&message);
    c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS; c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &native, sizeof(native));
    struct timeval timeout = { .tv_sec = 5 };
    setsockopt(control, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
    unsigned char ack = 1;
    int result = sendmsg(control, &message, 0) == 1 ? 0 : -errno;
    if (result == 0 && (read(control, &ack, 1) != 1 || ack != 0)) result = -EIO;
    close(control); close(native);
    return result;
}
static int64_t drm_ioctl(hl_open_file_t *of, int host, guest_t *g, uint64_t cmd, uint64_t arg) {
    (void)host;
    drm_open *state = of->state;
    wc_hl_drm_callbacks callbacks = { g, read_guest, write_guest, token_guest, relay_guest };
    int rc = wc_hl_drm_ioctl(state->drm, (uint32_t)geteuid(), (uint32_t)proc_get_pid(), (uint32_t)cmd, arg, callbacks);
    if (rc >= 0) return rc;
    errno = -rc; return linux_errno();
}
static int64_t drm_io(hl_open_file_t *of, int host, guest_t *g, uint64_t addr, uint64_t count) {
    (void)of; (void)host; (void)g; (void)addr; (void)count;
    return -LINUX_EINVAL;
}
static void drm_destroy(hl_open_file_t *of) {
    drm_open *state = of->state;
    wc_hl_drm_release(state->drm); close(state->peer); free(state);
    atomic_fetch_sub(&open_count, 1);
}
static const hl_fd_ops_t drm_ops = { .ioctl = drm_ioctl, .read = drm_io, .write = drm_io, .destroy = drm_destroy };
static int64_t drm_open_node(const char *name, int flags, int mode) {
    (void)name; (void)mode;
    if ((flags & 3) == 1) return -LINUX_EACCES;
    drm_open *state = calloc(1, sizeof(*state));
    if (!state) return -LINUX_ENOMEM;
    int pair[2];
    if (pipe(pair) < 0) { free(state); return linux_errno(); }
    fcntl(pair[0], F_SETFD, FD_CLOEXEC); fcntl(pair[1], F_SETFD, FD_CLOEXEC);
    int rc = wc_hl_drm_open((uint32_t)geteuid(), (uint32_t)proc_get_pid(), &state->drm);
    if (rc < 0) { close(pair[0]); close(pair[1]); free(state); errno = -rc; return linux_errno(); }
    state->peer = pair[1];
    hl_open_file_t *of = hl_open_file_create(FD_DEVICE, &drm_ops, (uint32_t)flags, state);
    if (!of) { wc_hl_drm_release(state->drm); close(pair[0]); close(pair[1]); free(state); return -LINUX_ENOMEM; }
    atomic_fetch_add(&open_count, 1);
    int fd = hl_fd_install(of, pair[0], flags & LINUX_O_CLOEXEC);
    if (fd >= 0) {
        const hl_device_node_t *node = hl_device_lookup("/dev/dri/renderD128");
        pthread_mutex_lock(&fd_lock);
        if (fd_table[fd].of == of) {
            fd_table[fd].dir = (void *)node;
            if (fd_table[fd].desc) fd_table[fd].desc->dir = (void *)node;
        }
        pthread_mutex_unlock(&fd_lock);
    }
    return fd < 0 ? -LINUX_EMFILE : fd;
}
static const hl_device_ops_t node_ops = { .open = drm_open_node };
void hl_wc_drm_register(void) {
    static const hl_device_node_t node = { "dri/renderD128", &node_ops, S_IFCHR | 0666, 226, 128 };
    hl_device_register(&node);
}
