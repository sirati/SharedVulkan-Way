/* SPDX-License-Identifier: MIT
 * Native LLP64 client runtime for shipped libwayland. fd identities refer to
 * retained native sockets or sections. Pool pixels are never copied here.
 */
#include "wc-local-client.h"
#include "wc-native.h"
#include <errno.h>
#include <limits.h>
#include <poll.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/un.h>
#define COUNT 256
struct resource {
  int fd, socket, flags;
  uint64_t native, size;
};
static struct resource resources[COUNT];
struct mapping {
  void *address;
  size_t size;
  uint64_t native;
};
static struct mapping mappings[COUNT];
static int next_fd = 1000;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_mutex_t log_lock = PTHREAD_MUTEX_INITIALIZER;
void wc_native_log_lock(void) { pthread_mutex_lock(&log_lock); }
void wc_native_log_unlock(void) { pthread_mutex_unlock(&log_lock); }
static struct resource *find(int fd) {
  for (int i = 0; i < COUNT; ++i)
    if (resources[i].fd == fd)
      return resources + i;
  return NULL;
}
static int insert(int socket, uint64_t native, uint64_t size) {
  if (next_fd == INT_MAX) {
    errno = EMFILE;
    return -1;
  }
  for (int i = 0; i < COUNT; ++i)
    if (!resources[i].fd) {
      int fd = next_fd++;
      resources[i] = (struct resource){fd, socket, 0, native, size};
      return fd;
    }
  errno = EMFILE;
  return -1;
}
static int snapshot(int fd, struct resource *out) {
  pthread_mutex_lock(&lock);
  struct resource *r = find(fd);
  if (r)
    *out = *r;
  pthread_mutex_unlock(&lock);
  if (!r) {
    errno = EBADF;
    return -1;
  }
  return 0;
}
int wc_native_socket(void) {
  pthread_mutex_lock(&lock);
  int fd = insert(1, 0, 0);
  pthread_mutex_unlock(&lock);
  return fd;
}
int wc_native_connect(int fd, const struct sockaddr *address,
                      socklen_t length) {
  if (!address || length < sizeof(unsigned short) ||
      address->sa_family != AF_UNIX) {
    errno = EINVAL;
    return -1;
  }
  const struct sockaddr_un *un = (const void *)address;
  size_t cap = length > offsetof(struct sockaddr_un, sun_path)
                   ? length - offsetof(struct sockaddr_un, sun_path)
                   : 0;
  if (cap > sizeof(un->sun_path))
    cap = sizeof(un->sun_path);
  size_t size = strnlen(un->sun_path, cap);
  if (!size || size == cap) {
    errno = EINVAL;
    return -1;
  }
  uint64_t connection;
  if (wc_local_connect((const uint8_t *)un->sun_path, (uint32_t)size,
                       &connection)) {
    errno = ECONNREFUSED;
    return -1;
  }
  pthread_mutex_lock(&lock);
  struct resource *r = find(fd);
  if (!r || !r->socket || r->native) {
    pthread_mutex_unlock(&lock);
    wc_local_disconnect(connection);
    errno = EBADF;
    return -1;
  }
  r->native = connection;
  int flags = r->flags;
  pthread_mutex_unlock(&lock);
  (void)flags;
  wc_local_nonblocking(connection, 1);
  return 0;
}
int wc_native_close(int fd) {
  pthread_mutex_lock(&lock);
  struct resource *r = find(fd);
  if (!r) {
    pthread_mutex_unlock(&lock);
    errno = EBADF;
    return -1;
  }
  struct resource old = *r;
  memset(r, 0, sizeof(*r));
  pthread_mutex_unlock(&lock);
  if (!old.native)
    return 0;
  return old.socket ? wc_local_disconnect(old.native)
                    : wc_local_section_close(old.native);
}
int os_create_anonymous_file(off_t size) {
  if (size <= 0) {
    errno = EINVAL;
    return -1;
  }
  uint64_t native;
  if (wc_local_section_create((uint64_t)size, &native)) {
    errno = ENOMEM;
    return -1;
  }
  pthread_mutex_lock(&lock);
  int fd = insert(0, native, (uint64_t)size);
  pthread_mutex_unlock(&lock);
  if (fd < 0)
    wc_local_section_close(native);
  return fd;
}
int wc_native_dup(int fd) {
  pthread_mutex_lock(&lock);
  struct resource *r = find(fd);
  uint64_t alias;
  if (!r || r->socket) {
    pthread_mutex_unlock(&lock);
    errno = ENOTSUP;
    return -1;
  }
  if (wc_local_section_clone(r->native, &alias)) {
    pthread_mutex_unlock(&lock);
    errno = EBADF;
    return -1;
  }
  int result = insert(0, alias, r->size);
  pthread_mutex_unlock(&lock);
  if (result < 0)
    wc_local_section_close(alias);
  return result;
}
int wc_native_fcntl(int fd, int command, ...) {
  if (command == F_DUPFD_CLOEXEC)
    return wc_native_dup(fd);
  pthread_mutex_lock(&lock);
  struct resource *r = find(fd);
  if (!r) {
    pthread_mutex_unlock(&lock);
    errno = EBADF;
    return -1;
  }
  int result = 0;
  if (command == F_GETFD)
    result = FD_CLOEXEC;
  else if (command == F_GETFL)
    result = r->flags;
  else if (command == F_SETFD) {
  } else if (command == F_SETFL) {
    va_list ap;
    va_start(ap, command);
    r->flags = va_arg(ap, int);
    va_end(ap);
    if (r->socket && r->native)
      result = wc_local_nonblocking(r->native, !!(r->flags & O_NONBLOCK));
  } else {
    errno = EINVAL;
    result = -1;
  }
  pthread_mutex_unlock(&lock);
  return result;
}
int wc_native_ftruncate(int fd, off_t size) {
  struct resource r;
  if (snapshot(fd, &r))
    return -1;
  if (r.socket || size < 0 || (uint64_t)size != r.size) {
    errno = ENOTSUP;
    return -1;
  }
  return 0;
}
void *wc_native_mmap(void *address, size_t size, int prot, int flags, int fd,
                     off_t offset) {
  struct resource r;
  if (snapshot(fd, &r))
    return MAP_FAILED;
  if (r.socket || address || offset || !size || size > r.size ||
      (flags != MAP_SHARED && flags != MAP_PRIVATE) ||
      (prot != PROT_READ && prot != (PROT_READ | PROT_WRITE))) {
    errno = ENOTSUP;
    return MAP_FAILED;
  }
  uint64_t id, pointer, length;
  if (wc_local_map(r.native, !!(prot & PROT_WRITE), &id, &pointer, &length)) {
    errno = EACCES;
    return MAP_FAILED;
  }
  pthread_mutex_lock(&lock);
  for (int i = 0; i < COUNT; ++i)
    if (!mappings[i].address) {
      mappings[i] = (struct mapping){(void *)(uintptr_t)pointer, size, id};
      pthread_mutex_unlock(&lock);
      return (void *)(uintptr_t)pointer;
    }
  pthread_mutex_unlock(&lock);
  wc_local_unmap(id);
  errno = ENOMEM;
  return MAP_FAILED;
}
int wc_native_munmap(void *address, size_t size) {
  pthread_mutex_lock(&lock);
  for (int i = 0; i < COUNT; ++i)
    if (mappings[i].address == address && mappings[i].size == size) {
      uint64_t id = mappings[i].native;
      memset(mappings + i, 0, sizeof(mappings[i]));
      pthread_mutex_unlock(&lock);
      return wc_local_unmap(id);
    }
  pthread_mutex_unlock(&lock);
  errno = EINVAL;
  return -1;
}
ssize_t wc_native_sendmsg(int fd, const struct msghdr *message, int flags) {
  (void)flags;
  struct resource r;
  if (snapshot(fd, &r))
    return -1;
  if (!r.socket || !r.native || !message || message->msg_controllen) {
    errno = ENOTSUP;
    return -1;
  }
  size_t length = 0;
  for (size_t i = 0; i < message->msg_iovlen; ++i) {
    if (message->msg_iov[i].iov_len > 65536 - length) {
      errno = EMSGSIZE;
      return -1;
    }
    length += message->msg_iov[i].iov_len;
  }
  if (!length)
    return 0;
  uint8_t *bytes = malloc(length);
  if (!bytes)
    return -1;
  size_t at = 0;
  for (size_t i = 0; i < message->msg_iovlen; ++i) {
    memcpy(bytes + at, message->msg_iov[i].iov_base,
           message->msg_iov[i].iov_len);
    at += message->msg_iov[i].iov_len;
  }
  int result = wc_local_write(r.native, bytes, (uint32_t)length);
  free(bytes);
  if (result < 0)
    errno = wc_local_error();
  return result;
}
ssize_t wc_native_recvmsg(int fd, struct msghdr *message, int flags) {
  (void)flags;
  struct resource r;
  if (snapshot(fd, &r))
    return -1;
  if (!r.socket || !r.native || !message) {
    errno = EINVAL;
    return -1;
  }
  message->msg_controllen = 0;
  for (size_t i = 0; i < message->msg_iovlen; ++i)
    if (message->msg_iov[i].iov_len) {
      uint32_t n = message->msg_iov[i].iov_len > 65536
                       ? 65536
                       : (uint32_t)message->msg_iov[i].iov_len;
      int result = wc_local_read(r.native, message->msg_iov[i].iov_base, n);
      if (result < 0)
        errno = wc_local_error();
      return result;
    }
  return 0;
}
int wc_native_poll(struct pollfd *fds, unsigned long count, int timeout) {
  if (count != 1 || !fds) {
    errno = ENOTSUP;
    return -1;
  }
  struct resource r;
  if (snapshot(fds[0].fd, &r))
    return -1;
  if (!r.socket || !r.native) {
    errno = ENOTSUP;
    return -1;
  }
  int result = wc_local_wait(r.native,
                             ((fds[0].events & POLLIN) ? 1 : 0) |
                                 ((fds[0].events & POLLOUT) ? 2 : 0),
                             timeout);
  if (result < 0) {
    errno = wc_local_error();
    return -1;
  }
  fds[0].revents =
      (short)((result & 1 ? POLLIN : 0) | (result & 2 ? POLLOUT : 0) |
              (result & 4 ? POLLERR | POLLHUP : 0));
  return !!result;
}
int wc_native_ppoll(struct pollfd *fds, unsigned long count,
                    const struct timespec *timeout, const void *mask) {
  if (mask) {
    errno = ENOTSUP;
    return -1;
  }
  int millis = -1;
  if (timeout) {
    if (timeout->tv_sec < 0 || timeout->tv_nsec < 0 ||
        timeout->tv_nsec >= 1000000000) {
      errno = EINVAL;
      return -1;
    }
    long long n = (long long)timeout->tv_sec * 1000 +
                  (timeout->tv_nsec + 999999) / 1000000;
    millis = n > INT_MAX ? INT_MAX : (int)n;
  }
  return wc_native_poll(fds, count, millis);
}
size_t wc_native_fd_word_count(int fd) {
  struct resource r;
  if (snapshot(fd, &r) || r.socket)
    return 0;
  int bytes = wc_local_section_export(r.native, NULL, 0);
  return bytes > 0 ? 1 + ((size_t)bytes + 3) / 4 : 0;
}
int wc_native_fd_words(int fd, uint32_t *words, size_t capacity) {
  struct resource r;
  if (snapshot(fd, &r) || r.socket || !words)
    return -1;
  int length = wc_local_section_export(r.native, NULL, 0);
  if (length <= 0 || length > 4096 || capacity < 1 + ((size_t)length + 3) / 4) {
    errno = EINVAL;
    return -1;
  }
  memset(words, 0, (1 + ((size_t)length + 3) / 4) * 4);
  words[0] = (uint32_t)length;
  return wc_local_section_export(r.native, (uint8_t *)(words + 1),
                                 (uint32_t)length) < 0
             ? -1
             : 0;
}
int wc_native_fd_from_words(const uint32_t *words, size_t count) {
  if (!words || !count || !words[0] || words[0] > 4096 ||
      1 + ((size_t)words[0] + 3) / 4 > count) {
    errno = EPROTO;
    return -1;
  }
  uint64_t id;
  if (wc_local_section_import((const uint8_t *)(words + 1), words[0], &id)) {
    errno = EPROTO;
    return -1;
  }
  uint64_t map, pointer, length;
  if (wc_local_map(id, 0, &map, &pointer, &length)) {
    wc_local_section_close(id);
    errno = EPROTO;
    return -1;
  }
  wc_local_unmap(map);
  pthread_mutex_lock(&lock);
  int fd = insert(0, id, length);
  pthread_mutex_unlock(&lock);
  if (fd < 0)
    wc_local_section_close(id);
  return fd;
}
