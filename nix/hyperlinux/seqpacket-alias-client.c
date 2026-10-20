/* Proc/dev fd aliases must never turn a protocol socket into a regular fd. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>
#define CHECK(x) do { if (!(x)) { fprintf(stderr, "seq alias line %d: %s errno=%d\n", __LINE__, #x, errno); exit(1); } } while (0)
static void refused(const char *path) {
    const int access[] = { O_RDONLY, O_WRONLY, O_RDWR };
    for (unsigned i = 0; i < sizeof(access) / sizeof(access[0]); i++) {
        errno = 0; int fd = open(path, access[i] | O_CLOEXEC);
        CHECK(fd == -1 && errno == ENXIO);
    }
}
int main(void) {
    int pair[2]; CHECK(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC | SOCK_NONBLOCK, 0, pair) == 0);
    CHECK(send(pair[0], "preserved", 9, MSG_NOSIGNAL) == 9);
    char path[64];
    snprintf(path, sizeof(path), "/proc/self/fd/%d", pair[1]); refused(path);
    snprintf(path, sizeof(path), "/dev/fd/%d", pair[1]); refused(path);
    int saved = dup(STDOUT_FILENO); CHECK(saved >= 0);
    CHECK(dup2(pair[1], STDOUT_FILENO) == STDOUT_FILENO);
    refused("/dev/stdout"); refused("/proc/self/fd/1"); refused("/dev/fd/1");
    CHECK(dup2(saved, STDOUT_FILENO) == STDOUT_FILENO && close(saved) == 0);
    int type = 0; socklen_t size = sizeof(type);
    CHECK(getsockopt(pair[1], SOL_SOCKET, SO_TYPE, &type, &size) == 0 && type == SOCK_SEQPACKET);
    char bytes[16]; CHECK(recv(pair[1], bytes, sizeof(bytes), 0) == 9 && !memcmp(bytes, "preserved", 9));
    CHECK(recv(pair[1], bytes, sizeof(bytes), MSG_DONTWAIT) == -1 && errno == EAGAIN);
    CHECK(close(pair[0]) == 0 && close(pair[1]) == 0);
    puts("SEQPACKET proc/dev/stdout alias opens refuse without protocol downgrade PASS"); return 0;
}
