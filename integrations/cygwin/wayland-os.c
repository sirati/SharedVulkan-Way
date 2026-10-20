/* SPDX-License-Identifier: MIT
 * Client-only Cygwin OS integration for upstream libwayland.
 * Cygwin's AF_LOCAL stream sockets discard SCM_RIGHTS. Refuse outbound
 * descriptors explicitly instead of claiming that a wl_shm pool arrived.
 * A BAR-backed descriptor transport is still required for rendered clients.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <unistd.h>
#include "wayland-os.h"

int wl_os_socket_cloexec(int domain, int type, int protocol)
{
    return socket(domain, type | SOCK_CLOEXEC, protocol);
}

int wl_os_socket_peercred(int fd, uid_t *uid, gid_t *gid, pid_t *pid)
{
    (void)fd; (void)uid; (void)gid; (void)pid;
    errno = ENOTSUP;
    return -1;
}

int wl_os_dupfd_cloexec(int fd, int minimum)
{
    return fcntl(fd, F_DUPFD_CLOEXEC, minimum);
}

ssize_t wc_cygwin_sendmsg(int fd, const struct msghdr *msg, int flags)
{
    if (msg->msg_controllen) {
        errno = ENOTSUP;
        return -1;
    }
    return sendmsg(fd, msg, flags);
}

ssize_t wl_os_recvmsg_cloexec(int fd, struct msghdr *msg, int flags)
{
    /* There are no received descriptors in the Cygwin AF_LOCAL backend.
     * Do not let a stale input capacity masquerade as ancillary output. */
    msg->msg_controllen = 0;
    return recvmsg(fd, msg, flags & ~MSG_CMSG_CLOEXEC);
}

int wl_os_epoll_create_cloexec(void)
{
    errno = ENOSYS; /* This build does not include libwayland-server. */
    return -1;
}

int wl_os_accept_cloexec(int fd, struct sockaddr *addr, socklen_t *length)
{
    return accept4(fd, addr, length, SOCK_CLOEXEC);
}

void *wl_os_mremap_maymove(int fd, void *old, ssize_t *old_size,
                         ssize_t new_size, int prot, int flags)
{
    if (msync(old, *old_size, MS_SYNC) != 0)
        return MAP_FAILED;
    void *next = mmap(NULL, new_size, prot, flags, fd, 0);
    if (next != MAP_FAILED && munmap(old, *old_size) == 0)
        *old_size = 0;
    return next;
}
