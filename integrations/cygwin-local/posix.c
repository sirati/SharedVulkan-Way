/* SPDX-License-Identifier: MIT
 * Cygwin POSIX identities for same-machine native Windows named sections.
 * These token files contain no pixels. mmap returns the native section view.
 */
#define _GNU_SOURCE
#include "wc-local-client.h"
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
struct alias {
  int fd;
  struct alias *next;
};
struct mapped {
  size_t size;
  struct mapped *next;
};
struct pool {
  dev_t device;
  ino_t inode;
  int hold, producer;
  uint64_t native, map;
  void *address;
  size_t size;
  struct alias *fds;
  struct mapped *maps;
  struct pool *next;
};
static struct pool *pools;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static int child;
static void *real(const char *name) {
  void *runtime = dlopen("cygwin1.dll", RTLD_NOW);
  if (!runtime) {
    errno = ENOSYS;
    return NULL;
  }
  void *p = dlsym(runtime, name);
  dlclose(runtime);
  return p;
}
static int realclose(int fd) {
  int (*fn)(int) = real("close");
  return fn ? fn(fd) : -1;
}
static struct pool *find(int fd) {
  struct stat st;
  if (fstat(fd, &st))
    return NULL;
  for (struct pool *p = pools; p; p = p->next)
    if (st.st_dev == p->device && st.st_ino == p->inode)
      return p;
  return NULL;
}
static int add(struct pool *p, int fd) {
  for (struct alias *a = p->fds; a; a = a->next)
    if (a->fd == fd)
      return 0;
  struct alias *a = malloc(sizeof(*a));
  if (!a)
    return -1;
  *a = (struct alias){fd, p->fds};
  p->fds = a;
  return 0;
}
static void remove_alias(struct pool *p, int fd) {
  struct alias **a = &p->fds;
  while (*a) {
    if ((*a)->fd == fd) {
      struct alias *old = *a;
      *a = old->next;
      free(old);
      return;
    }
    a = &(*a)->next;
  }
}
static void retire(struct pool *p) {
  if (p->fds || p->maps)
    return;
  struct pool **next = &pools;
  while (*next && *next != p)
    next = &(*next)->next;
  if (*next)
    *next = p->next;
  wc_local_unmap(p->map);
  wc_local_section_close(p->native);
  realclose(p->hold);
  free(p);
}
static int token(struct pool *p) {
  const char *runtime = getenv("XDG_RUNTIME_DIR");
  if (!runtime || !*runtime) {
    errno = ENOENT;
    return -1;
  }
  char *path;
  if (asprintf(&path, "%s/wc-local-XXXXXX", runtime) < 0)
    return -1;
  int fd = mkostemp(path, O_CLOEXEC);
  if (fd >= 0)
    unlink(path);
  free(path);
  if (fd < 0)
    return -1;
  int (*truncate)(int, off_t) = real("ftruncate");
  int (*control)(int, int, ...) = real("fcntl");
  if (!truncate || !control || truncate(fd, (off_t)p->size)) {
    realclose(fd);
    return -1;
  }
  struct stat st;
  if (fstat(fd, &st) || (p->hold = control(fd, F_DUPFD_CLOEXEC, 0)) < 0) {
    realclose(fd);
    return -1;
  }
  p->device = st.st_dev;
  p->inode = st.st_ino;
  if (add(p, fd)) {
    realclose(p->hold);
    realclose(fd);
    return -1;
  }
  p->next = pools;
  pools = p;
  return fd;
}
static int materialize(uint64_t native, int producer) {
  struct pool *p = calloc(1, sizeof(*p));
  if (!p) {
    wc_local_section_close(native);
    return -1;
  }
  uint64_t address, length;
  p->native = native;
  p->producer = producer;
  if (wc_local_map(native, producer, &p->map, &address, &length)) {
    wc_local_section_close(native);
    free(p);
    errno = EACCES;
    return -1;
  }
  p->address = (void *)(uintptr_t)address;
  p->size = (size_t)length;
  pthread_mutex_lock(&lock);
  int fd = token(p);
  pthread_mutex_unlock(&lock);
  if (fd < 0) {
    wc_local_unmap(p->map);
    wc_local_section_close(native);
    free(p);
  }
  return fd;
}
static int overlaps(void *address, size_t size) {
  uintptr_t start = (uintptr_t)address;
  if (size > UINTPTR_MAX - start)
    return 1;
  for (struct pool *p = pools; p; p = p->next) {
    uintptr_t first = (uintptr_t)p->address;
    if (p->size > UINTPTR_MAX - first)
      return 1;
    if (size && start < first + p->size && first < start + size)
      return 1;
  }
  return 0;
}
int os_create_anonymous_file(off_t size) {
  if (child || size <= 0) {
    errno = ENOTSUP;
    return -1;
  }
  uint64_t native;
  if (wc_local_section_create((uint64_t)size, &native)) {
    errno = ENOMEM;
    return -1;
  }
  return materialize(native, 1);
}
void *mmap(void *address, size_t size, int prot, int flags, int fd,
           off_t offset) {
  pthread_mutex_lock(&lock);
  struct pool *p = find(fd);
  if (!p) {
    if (overlaps(address, size)) {
      pthread_mutex_unlock(&lock);
      errno = ENOTSUP;
      return MAP_FAILED;
    }
    pthread_mutex_unlock(&lock);
    void *(*fn)(void *, size_t, int, int, int, off_t) = real("mmap");
    return fn ? fn(address, size, prot, flags, fd, offset) : MAP_FAILED;
  }
  int shape =
      p->producer
          ? prot == (PROT_READ | PROT_WRITE) && flags == MAP_SHARED
          : prot == PROT_READ && (flags == MAP_SHARED || flags == MAP_PRIVATE);
  if (child || address || offset || !size || size > p->size || !shape ||
      add(p, fd)) {
    pthread_mutex_unlock(&lock);
    errno = ENOTSUP;
    return MAP_FAILED;
  }
  struct mapped *m = malloc(sizeof(*m));
  if (!m) {
    pthread_mutex_unlock(&lock);
    return MAP_FAILED;
  }
  *m = (struct mapped){size, p->maps};
  p->maps = m;
  void *result = p->address;
  pthread_mutex_unlock(&lock);
  return result;
}
int munmap(void *address, size_t size) {
  pthread_mutex_lock(&lock);
  for (struct pool *p = pools; p; p = p->next)
    if (p->address == address) {
      if (child) {
        pthread_mutex_unlock(&lock);
        errno = ENOTSUP;
        return -1;
      }
      struct mapped **next = &p->maps;
      while (*next && (*next)->size != size)
        next = &(*next)->next;
      if (!*next) {
        pthread_mutex_unlock(&lock);
        errno = EINVAL;
        return -1;
      }
      struct mapped *old = *next;
      *next = old->next;
      free(old);
      retire(p);
      pthread_mutex_unlock(&lock);
      return 0;
    }
  if (overlaps(address, size)) {
    pthread_mutex_unlock(&lock);
    errno = EINVAL;
    return -1;
  }
  pthread_mutex_unlock(&lock);
  int (*fn)(void *, size_t) = real("munmap");
  return fn ? fn(address, size) : -1;
}
int close(int fd) {
  pthread_mutex_lock(&lock);
  struct pool *p = find(fd);
  int result = realclose(fd);
  if (!result && p && !child) {
    remove_alias(p, fd);
    retire(p);
  }
  pthread_mutex_unlock(&lock);
  return result;
}
int ftruncate(int fd, off_t size) {
  pthread_mutex_lock(&lock);
  struct pool *p = find(fd);
  if (p) {
    int ok = !child && p->producer && size > 0 && (uint64_t)size == p->size;
    pthread_mutex_unlock(&lock);
    if (!ok)
      errno = ENOTSUP;
    return ok ? 0 : -1;
  }
  pthread_mutex_unlock(&lock);
  int (*fn)(int, off_t) = real("ftruncate");
  return fn ? fn(fd, size) : -1;
}
int dup(int fd) {
  int (*fn)(int) = real("dup");
  if (!fn)
    return -1;
  pthread_mutex_lock(&lock);
  struct pool *p = find(fd);
  int result = fn(fd);
  if (result >= 0 && p && !child && add(p, result)) {
    realclose(result);
    result = -1;
  }
  pthread_mutex_unlock(&lock);
  return result;
}
int dup2(int fd, int target) {
  int (*fn)(int, int) = real("dup2");
  if (!fn)
    return -1;
  pthread_mutex_lock(&lock);
  struct pool *p = find(fd), *old = find(target);
  int result = fn(fd, target);
  if (result >= 0 && fd != target && !child) {
    if (old)
      remove_alias(old, target);
    if (p && add(p, result)) {
      realclose(result);
      result = -1;
    }
    if (old)
      retire(old);
  }
  pthread_mutex_unlock(&lock);
  return result;
}
int fcntl(int fd, int command, ...) {
  int (*fn)(int, int, ...) = real("fcntl");
  if (!fn)
    return -1;
  va_list ap;
  va_start(ap, command);
  pthread_mutex_lock(&lock);
  struct pool *p = find(fd);
  int result;
  switch (command) {
  case F_GETFD:
  case F_GETFL:
  case F_GETOWN:
    result = fn(fd, command);
    break;
  case F_SETFD:
  case F_SETFL:
  case F_SETOWN:
  case F_DUPFD:
  case F_DUPFD_CLOEXEC:
    result = fn(fd, command, va_arg(ap, int));
    break;
  case F_GETLK:
  case F_SETLK:
  case F_SETLKW:
    result = fn(fd, command, va_arg(ap, struct flock *));
    break;
  default:
    errno = ENOTSUP;
    result = -1;
  }
  if (result >= 0 && p && !child &&
      (command == F_DUPFD || command == F_DUPFD_CLOEXEC) && add(p, result)) {
    realclose(result);
    result = -1;
  }
  pthread_mutex_unlock(&lock);
  va_end(ap);
  return result;
}
size_t wc_native_fd_word_count(int fd) {
  pthread_mutex_lock(&lock);
  struct pool *p = child ? NULL : find(fd);
  int length = p ? wc_local_section_export(p->native, NULL, 0) : -1;
  pthread_mutex_unlock(&lock);
  return length > 0 ? 1 + ((size_t)length + 3) / 4 : 0;
}
int wc_native_fd_words(int fd, uint32_t *words, size_t capacity) {
  pthread_mutex_lock(&lock);
  struct pool *p = child ? NULL : find(fd);
  int length = p ? wc_local_section_export(p->native, NULL, 0) : -1;
  if (length <= 0 || length > 4096 || capacity < 1 + ((size_t)length + 3) / 4) {
    pthread_mutex_unlock(&lock);
    errno = ENOTSUP;
    return -1;
  }
  memset(words, 0, (1 + ((size_t)length + 3) / 4) * 4);
  words[0] = (uint32_t)length;
  int result = wc_local_section_export(p->native, (uint8_t *)(words + 1),
                                       (uint32_t)length);
  pthread_mutex_unlock(&lock);
  return result < 0 ? -1 : 0;
}
int wc_native_fd_from_words(const uint32_t *words, size_t count) {
  if (child || !count || !words || !words[0] || words[0] > 4096 ||
      1 + ((size_t)words[0] + 3) / 4 > count) {
    errno = EPROTO;
    return -1;
  }
  uint64_t native;
  if (wc_local_section_import((const uint8_t *)(words + 1), words[0],
                              &native)) {
    errno = EPROTO;
    return -1;
  }
  return materialize(native, 0);
}
static void before_fork(void) { pthread_mutex_lock(&lock); }
static void after_fork(void) { pthread_mutex_unlock(&lock); }
static void in_child(void) {
  child = 1;
  pthread_mutex_unlock(&lock);
}
__attribute__((constructor)) static void register_fork(void) {
  pthread_atfork(before_fork, after_fork, in_child);
}
