#ifndef WC_NATIVE_H
#define WC_NATIVE_H
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
static inline int wc_native_path_is_absolute(const char *path) {
  return path && ((path[0] >= 'A' && path[0] <= 'Z') ||
                  (path[0] >= 'a' && path[0] <= 'z')) &&
         path[1] == ':' && (path[2] == '/' || path[2] == '\\');
}
static inline const char *wc_native_program_name(void) {
  const char *name = _pgmptr;
  const char *slash = name ? strrchr(name, '\\') : NULL;
  return slash ? slash + 1 : name ? name : "wayland-client";
}
#define program_invocation_short_name (wc_native_program_name())
static inline int unsetenv(const char *name) { return _putenv_s(name, ""); }
#include <fcntl.h>
#define F_GETFD 1
#define F_SETFD 2
#define FD_CLOEXEC 1
#define F_DUPFD_CLOEXEC 1030
#define F_GETFL 3
#define F_SETFL 4
#ifndef O_CLOEXEC
#define O_CLOEXEC 0
#endif
#define O_NONBLOCK 04000
void wc_native_log_lock(void);
void wc_native_log_unlock(void);
int wc_native_close(int);
int wc_native_fcntl(int, int, ...);
int wc_native_dup(int);
int wc_native_ftruncate(int, off_t);
size_t wc_native_fd_word_count(int);
int wc_native_fd_words(int, uint32_t *, size_t);
int wc_native_fd_from_words(const uint32_t *, size_t);
#define close wc_native_close
#define fcntl wc_native_fcntl
#define dup wc_native_dup
#define ftruncate wc_native_ftruncate
/* MinGW lacks POSIX sigaction. The tested client installs SIGINT only. */
struct wc_native_sigaction {
  void (*sa_handler)(int);
  int sa_mask, sa_flags;
};
#define sigaction wc_native_sigaction
#define sigemptyset(set) (*(set) = 0)
#define SA_RESETHAND 0
static inline int wc_native_sigaction(int n,
                                      const struct wc_native_sigaction *a,
                                      struct wc_native_sigaction *old) {
  if (old || !a)
    return -1;
  return signal(n, a->sa_handler) == SIG_ERR ? -1 : 0;
}
#endif
