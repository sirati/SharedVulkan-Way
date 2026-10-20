/* Linux seqpacket socketpair ABI over atomic native stream tokens.
 * The token byte and its SCM_RIGHTS are a single kernel sendmsg operation.
 * A receiver requests exactly one byte, never a byte stream frame.
 *
 * Quota: SO_SNDBUF is fixed at 256 and guest resizing is refused. XNU's
 * AF_UNIX stream send charges each token byte to sender hiwat; receive
 * restores the charge. At most 256 one-byte records can therefore be
 * queued per direction, <=4MiB backing payload and <=16640 native rights.
 * The independent control-mbuf budget makes the actual limit smaller.
 * There can additionally be one <=16KiB temporary per active send syscall.
 * Guest fd lookup never accepts arbitrary native numbers. Protected xattrs
 * cannot be created or changed through guest syscalls. Internal record fds
 * are never installed in the guest table or exported as application rights.
 */
#include "hl_seqpacket.h"
#include "hl_seals.h"
#include <sys/stat.h>
#include <sys/xattr.h>
#include <fcntl.h>
#include <errno.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#define RECORD_ATTR HL_WC_XATTR_PREFIX "seqpacket.record.v1"
#define RECORD_MAGIC UINT64_C(0x5743534551523031)
#define RIGHTS_MAX 64u
typedef struct { uint64_t magic, length; } record_header;
static int marker_set(int fd) {
    uint64_t magic = RECORD_MAGIC;
#ifdef __APPLE__
    return fsetxattr(fd, RECORD_ATTR, &magic, sizeof(magic), 0, XATTR_CREATE);
#else
    return fsetxattr(fd, "user." RECORD_ATTR, &magic, sizeof(magic), XATTR_CREATE);
#endif
}
static int marker_get(int fd) {
    uint64_t magic = 0;
#ifdef __APPLE__
    ssize_t n = fgetxattr(fd, RECORD_ATTR, &magic, sizeof(magic), 0, 0);
#else
    ssize_t n = fgetxattr(fd, "user." RECORD_ATTR, &magic, sizeof(magic));
#endif
    return n == sizeof(magic) && magic == RECORD_MAGIC;
}
static int full_io(int fd, void *buf, size_t size, off_t offset, int writing) {
    size_t done = 0;
    while (done < size) {
        ssize_t n = writing ? pwrite(fd, (char *)buf + done, size - done, offset + done)
                            : pread(fd, (char *)buf + done, size - done, offset + done);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) { if (!n) errno = EIO; return -1; }
        done += (size_t)n;
    }
    return 0;
}
int hl_wc_seqpacket_pair(int pair[2]) {
    if (geteuid() == 0) { errno = EOPNOTSUPP; return -1; }
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) < 0) return -1;
    for (unsigned i = 0; i < 2; i++) {
        int size = HL_WC_SEQPACKET_BUFFER;
        if (setsockopt(pair[i], SOL_SOCKET, SO_SNDBUF, &size, sizeof(size)) < 0) goto fail;
        socklen_t length = sizeof(size);
        if (getsockopt(pair[i], SOL_SOCKET, SO_SNDBUF, &size, &length) < 0) goto fail;
        if (length != sizeof(size) || size > HL_WC_SEQPACKET_BUFFER || size <= 0) { errno = EOPNOTSUPP; goto fail; }
#ifdef SO_NOSIGPIPE
        int one = 1;
        if (setsockopt(pair[i], SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof(one)) < 0) goto fail;
#endif
    }
    return 0;
fail: {
    int saved = errno; close(pair[0]); close(pair[1]); errno = saved; return -1;
}}
ssize_t hl_wc_seqpacket_send(int fd, const struct msghdr *message, int flags) {
    if (flags & (MSG_PEEK | MSG_OOB) || message->msg_name || message->msg_namelen) {
        errno = EOPNOTSUPP; return -1;
    }
    size_t size = 0;
    for (int i = 0; i < message->msg_iovlen; i++) {
        if (message->msg_iov[i].iov_len > HL_WC_SEQPACKET_MAX - size) { errno = EMSGSIZE; return -1; }
        size += message->msg_iov[i].iov_len;
    }
    int rights[RIGHTS_MAX + 1]; unsigned count = 1;
    struct msghdr control = *message;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&control); c; c = CMSG_NXTHDR(&control, c)) {
        if (c->cmsg_level != SOL_SOCKET || c->cmsg_type != SCM_RIGHTS || c->cmsg_len < CMSG_LEN(0)) {
            errno = EOPNOTSUPP; return -1;
        }
        size_t bytes = c->cmsg_len - CMSG_LEN(0);
        if (bytes % sizeof(int) || bytes / sizeof(int) > RIGHTS_MAX + 1 - count) { errno = EMSGSIZE; return -1; }
        memcpy(rights + count, CMSG_DATA(c), bytes); count += bytes / sizeof(int);
    }
    int backing = hl_wc_socket_private_file();
    if (backing < 0) return -1;
    int result_errno = 0;
    record_header header = { .magic = RECORD_MAGIC, .length = size };
    if (fcntl(backing, F_SETFD, FD_CLOEXEC) < 0 ||
        marker_set(backing) < 0 || full_io(backing, &header, sizeof(header), 0, 1) < 0) goto fail;
    off_t offset = sizeof(header);
    for (int i = 0; i < message->msg_iovlen; i++) {
        if (full_io(backing, message->msg_iov[i].iov_base, message->msg_iov[i].iov_len, offset, 1) < 0) goto fail;
        offset += message->msg_iov[i].iov_len;
    }
    rights[0] = backing;
    union { struct cmsghdr align; char bytes[CMSG_SPACE(sizeof(rights))]; } ancillary;
    memset(&ancillary, 0, sizeof(ancillary));
    char token = 1; struct iovec iov = { &token, 1 };
    struct msghdr native = { .msg_iov = &iov, .msg_iovlen = 1,
        .msg_control = ancillary.bytes, .msg_controllen = CMSG_SPACE(count * sizeof(int)) };
    struct cmsghdr *c = CMSG_FIRSTHDR(&native);
    c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS; c->cmsg_len = CMSG_LEN(count * sizeof(int));
    memcpy(CMSG_DATA(c), rights, count * sizeof(int));
    ssize_t n = sendmsg(fd, &native, flags & ~(MSG_EOR | MSG_WAITALL));
    if (n != 1) { if (n >= 0) errno = EIO; goto fail; }
    close(backing); return (ssize_t)size;
fail:
    result_errno = errno; close(backing); errno = result_errno; return -1;
}
ssize_t hl_wc_seqpacket_recv(int fd, struct msghdr *message, int flags, int full_length) {
    /* Darwin MSG_PEEK does not externalize SCM_RIGHTS. Refuse before receive. */
    if (flags & (MSG_PEEK | MSG_OOB)) { errno = EOPNOTSUPP; return -1; }
    int rights[RIGHTS_MAX + 1]; unsigned count = 0;
    union { struct cmsghdr align; char bytes[CMSG_SPACE(sizeof(rights))]; } ancillary;
    memset(&ancillary, 0, sizeof(ancillary));
    char token = 0; struct iovec iov = { &token, 1 };
    struct msghdr native = { .msg_iov = &iov, .msg_iovlen = 1,
        .msg_control = ancillary.bytes, .msg_controllen = sizeof(ancillary.bytes) };
    ssize_t n = recvmsg(fd, &native, flags & ~(MSG_WAITALL | MSG_EOR));
    if (n < 0) return -1;
    message->msg_flags = 0; message->msg_namelen = 0;
    if (!n) { message->msg_controllen = 0; return 0; }
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&native); c; c = CMSG_NXTHDR(&native, c)) {
        if (c->cmsg_level != SOL_SOCKET || c->cmsg_type != SCM_RIGHTS || c->cmsg_len < CMSG_LEN(0)) continue;
        size_t bytes = c->cmsg_len - CMSG_LEN(0);
        if (bytes % sizeof(int) || bytes / sizeof(int) > RIGHTS_MAX + 1 - count) { errno = EPROTO; goto fail; }
        memcpy(rights + count, CMSG_DATA(c), bytes); count += bytes / sizeof(int);
    }
    record_header header; struct stat st;
    if (token != 1 || !count || native.msg_flags & MSG_CTRUNC ||
        fstat(rights[0], &st) < 0 || !S_ISREG(st.st_mode) || st.st_uid != geteuid() || st.st_nlink != 0 || (st.st_mode & 0777) != 0 ||
        !marker_get(rights[0]) || full_io(rights[0], &header, sizeof(header), 0, 0) < 0 ||
        header.magic != RECORD_MAGIC || header.length > HL_WC_SEQPACKET_MAX ||
        st.st_size != (off_t)(sizeof(header) + header.length)) { errno = EPROTO; goto fail; }
    size_t copied = 0;
    for (int i = 0; i < message->msg_iovlen && copied < header.length; i++) {
        size_t bytes = message->msg_iov[i].iov_len;
        if (bytes > header.length - copied) bytes = header.length - copied;
        if (full_io(rights[0], message->msg_iov[i].iov_base, bytes, sizeof(header) + copied, 0) < 0) goto fail;
        copied += bytes;
    }
    if (copied < header.length) message->msg_flags |= MSG_TRUNC;
    close(rights[0]); rights[0] = -1;
    unsigned published = 0;
    if (count > 1 && message->msg_control && message->msg_controllen >= CMSG_LEN(sizeof(int))) {
        size_t room = (message->msg_controllen - CMSG_LEN(0)) / sizeof(int);
        published = count - 1; if (published > room) published = room;
        struct cmsghdr *c = CMSG_FIRSTHDR(message);
        c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS; c->cmsg_len = CMSG_LEN(published * sizeof(int));
        memcpy(CMSG_DATA(c), rights + 1, published * sizeof(int));
        message->msg_controllen = CMSG_SPACE(published * sizeof(int));
        if (message->msg_controllen > CMSG_LEN(room * sizeof(int)))
            message->msg_controllen = CMSG_LEN(published * sizeof(int));
    } else message->msg_controllen = 0;
    if (published < count - 1) message->msg_flags |= MSG_CTRUNC;
    for (unsigned i = 1 + published; i < count; i++) close(rights[i]);
    return full_length ? (ssize_t)header.length : (ssize_t)copied;
fail: {
    int saved = errno; for (unsigned i = 0; i < count; i++) if (rights[i] >= 0) close(rights[i]);
    errno = saved; return -1;
}}
ssize_t hl_wc_seqpacket_io(int fd, const struct iovec *iov, int count, int writing) {
    struct msghdr message = { .msg_iov = (struct iovec *)iov, .msg_iovlen = count };
    return writing ? hl_wc_seqpacket_send(fd, &message, 0) : hl_wc_seqpacket_recv(fd, &message, 0, 0);
}
