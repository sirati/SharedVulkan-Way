// §105.13: round trips on a waycompose control socket (the compositor's,
// which carries each request over the session's control ring to the host).
//   probe-ctl-rtt SOCKET N [WORDS]   N requests "raw WORDS" (default rtt) on
//   one connection; prints mean, p50, p90, p99 and max in microseconds.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/socket.h>
#include <sys/un.h>

static double now_us(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec * 1e6 + t.tv_nsec / 1e3;
}
static int cmp(const void *a, const void *b) {
  double x = *(const double *)a, y = *(const double *)b;
  return x < y ? -1 : x > y;
}
int main(int argc, char **argv) {
  if (argc < 3) { fprintf(stderr, "usage: %s SOCKET N [WORDS]\n", argv[0]); return 2; }
  int n = atoi(argv[2]);
  const char *words = argc > 3 ? argv[3] : "rtt";
  int fd = socket(AF_UNIX, SOCK_STREAM, 0);
  struct sockaddr_un a = { .sun_family = AF_UNIX };
  strncpy(a.sun_path, argv[1], sizeof a.sun_path - 1);
  if (connect(fd, (struct sockaddr *)&a, sizeof a) != 0) { perror("connect"); return 1; }
  char req[512];
  int rl = snprintf(req, sizeof req, "raw %s\n", words);
  double *us = malloc(sizeof(double) * n);
  static char buf[1 << 20];
  for (int i = 0; i < n; i++) {
    double t0 = now_us();
    if (write(fd, req, rl) != rl) { perror("write"); return 1; }
    size_t have = 0;
    long body = -1;
    for (;;) {
      ssize_t r = read(fd, buf + have, sizeof buf - have);
      if (r <= 0) { fprintf(stderr, "closed after %d\n", i); return 1; }
      have += r;
      char *nl = memchr(buf, '\n', have);
      if (!nl) continue;
      if (strncmp(buf, "ok-raw ", 7) == 0) body = atol(buf + 7); else if (strncmp(buf, "err", 3) == 0) { fprintf(stderr, "%.*s\n", (int)(nl - buf), buf); return 1; } else body = 0;
      if ((long)(have - (nl + 1 - buf)) >= body) break;
    }
    us[i] = now_us() - t0;
  }
  double sum = 0;
  for (int i = 0; i < n; i++) sum += us[i];
  qsort(us, n, sizeof *us, cmp);
  printf("%s x%d: mean %.1f us, p50 %.1f, p90 %.1f, p99 %.1f, max %.1f\n", words, n, sum / n,
         us[n / 2], us[(int)(n * 0.9)], us[(int)(n * 0.99)], us[n - 1]);
  return 0;
}
