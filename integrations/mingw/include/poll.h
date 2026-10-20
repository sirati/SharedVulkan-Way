#ifndef WC_MINGW_POLL_H
#define WC_MINGW_POLL_H
#include <time.h>
struct pollfd {
  int fd;
  short events, revents;
};
#define POLLIN 1
#define POLLOUT 2
#define POLLERR 4
#define POLLHUP 8
#define POLLNVAL 16
int wc_native_poll(struct pollfd *, unsigned long, int);
int wc_native_ppoll(struct pollfd *, unsigned long, const struct timespec *,
                    const void *);
#define poll wc_native_poll
#define ppoll wc_native_ppoll
#endif
