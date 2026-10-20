/* SPDX-License-Identifier: MIT */
#include "wayland-os.h"
#include "wc-native.h"
#include <errno.h>
#include <sys/mman.h>
extern int wc_native_socket(void);
int wl_os_socket_cloexec(int domain, int type, int protocol) {
  if (domain != AF_UNIX || type != SOCK_STREAM || protocol) {
    errno = ENOTSUP;
    return -1;
  }
  return wc_native_socket();
}
int wl_os_socket_peercred(int fd, uid_t *u, gid_t *g, pid_t *p) {
  (void)fd;
  (void)u;
  (void)g;
  (void)p;
  errno = ENOTSUP;
  return -1;
}
int wl_os_dupfd_cloexec(int fd, int minimum) {
  (void)minimum;
  return wc_native_dup(fd);
}
ssize_t wl_os_recvmsg_cloexec(int fd, struct msghdr *message, int flags) {
  return wc_native_recvmsg(fd, message, flags);
}
int wl_os_epoll_create_cloexec(void) {
  errno = ENOTSUP;
  return -1;
}
int wl_os_accept_cloexec(int fd, struct sockaddr *address, socklen_t *size) {
  (void)fd;
  (void)address;
  (void)size;
  errno = ENOTSUP;
  return -1;
}
void *wl_os_mremap_maymove(int fd, void *old, ssize_t *old_size,
                           ssize_t new_size, int prot, int flags) {
  (void)fd;
  (void)old;
  (void)old_size;
  (void)new_size;
  (void)prot;
  (void)flags;
  errno = ENOTSUP;
  return MAP_FAILED;
}
