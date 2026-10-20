/* SPDX-License-Identifier: MIT
 * Local native socket transport behind a genuine Cygwin socketpair.
 * Shared pump code is taken from the shipped Cygwin byte adapter at build.
 */
#define _GNU_SOURCE
#include "wc-local-client.h"
#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/cygwin.h>
static int local_child;
static uint64_t connection;
static const char *connecting_name;
static pthread_mutex_t connect_lock = PTHREAD_MUTEX_INITIALIZER;
int wc_local_transport_open(void) {
  if (local_child)
    return -1;
  const char *name =
      connecting_name ? connecting_name : getenv("WAYLAND_DISPLAY");
  if (!name)
    name = "wayland-0";
  char *path = NULL;
  if (name[0] == '/')
    path = strdup(name);
  else {
    const char *runtime = getenv("XDG_RUNTIME_DIR");
    if (!runtime || !*runtime)
      return -1;
    if (asprintf(&path, "%s/%s", runtime, name) < 0)
      return -1;
  }
  if (!path)
    return -1;
  ssize_t size = cygwin_conv_path(CCP_POSIX_TO_WIN_A, path, NULL, 0);
  if (size <= 0) {
    free(path);
    return -1;
  }
  char *native = malloc((size_t)size);
  if (!native) {
    free(path);
    return -1;
  }
  int result = cygwin_conv_path(CCP_POSIX_TO_WIN_A, path, native, (size_t)size)
                   ? -1
                   : wc_local_connect((const uint8_t *)native,
                                      (uint32_t)strlen(native), &connection);
  free(native);
  free(path);
  return result;
}
intptr_t wc_local_transport_read(uint8_t *bytes, size_t capacity) {
  if (capacity > 65536)
    capacity = 65536;
  return local_child ? -1
                     : wc_local_read(connection, bytes, (uint32_t)capacity);
}
intptr_t wc_local_transport_write(const uint8_t *bytes, size_t length) {
  if (length > 65536)
    length = 65536;
  return local_child ? -1 : wc_local_write(connection, bytes, (uint32_t)length);
}
void wc_local_transport_close(void) {
  if (!local_child)
    wc_local_disconnect(connection);
}
static void mark_child(void) { local_child = 1; }
__attribute__((constructor)) static void prepare_fork(void) {
  pthread_atfork(NULL, NULL, mark_child);
}
/* Shared pump implementation is appended by the build script. */
