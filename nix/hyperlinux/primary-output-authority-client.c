/* Ordinary anonymous mappings: syscall output must obey guest write permission.
 * Direct syscalls avoid libc/vDSO stores. Protected suffixes must stay intact
 * even when Linux permits a partial copy into the writable prefix. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>
#define CHECK(x) do { if (!(x)) { fprintf(stderr, "primary output line %d: %s errno=%d\n", __LINE__, #x, errno); _exit(90); } } while (0)
union ancillary { struct cmsghdr align; unsigned char bytes[CMSG_SPACE(sizeof(int))]; };
static size_t page;
static unsigned char *mapping(void) {
    unsigned char *p = mmap(NULL, page * 2, PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(p != MAP_FAILED); memset(p, 0x5a, page * 2); return p;
}
static void finish(const char *name, long result, int error,
                   unsigned char *protected, unsigned char *snapshot) {
    int unchanged = !memcmp(protected, snapshot, page);
    printf("primary output %s: result=%ld errno=%d protected-page-unchanged=%d\n", name, result, error, unchanged);
    fflush(stdout);
    CHECK(result == -1 && error == EFAULT && unchanged);
}
static void time_output(int crossing) {
    unsigned char *p = mapping(), *protected = p + page;
    struct timeval *output = (struct timeval *)(crossing ? protected - 8 : protected);
    unsigned char *snapshot = malloc(page); CHECK(snapshot);
    memcpy(snapshot, protected, page); CHECK(mprotect(protected, page, PROT_READ) == 0);
    errno = 0; long result = syscall(SYS_gettimeofday, output, NULL); int error = errno;
    finish(crossing ? "gettimeofday writable-to-readonly" : "gettimeofday readonly", result, error, protected, snapshot);
}
static void send_right(int socket, int file) {
    union ancillary control = {0}; char byte = 'R'; struct iovec io = { &byte, 1 };
    struct msghdr message = { .msg_iov = &io, .msg_iovlen = 1,
        .msg_control = control.bytes, .msg_controllen = sizeof(control.bytes) };
    struct cmsghdr *c = CMSG_FIRSTHDR(&message);
    c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS; c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &file, sizeof(file));
    CHECK(sendmsg(socket, &message, MSG_NOSIGNAL) == 1);
}
static void message_output(int header, int crossing) {
    unsigned char *p = mapping(), *protected = p + page;
    struct msghdr local = {0}; union ancillary control = {0};
    char byte = 0; struct iovec io = { &byte, 1 };
    struct msghdr *message = &local;
    if (header) message = (struct msghdr *)(crossing ? protected - 32 : protected);
    memset(message, 0, sizeof(*message)); message->msg_iov = &io; message->msg_iovlen = 1;
    message->msg_control = header ? control.bytes : (crossing ? protected - 8 : protected);
    message->msg_controllen = sizeof(control.bytes);
    message->msg_flags = 0x40000000; /* Input field ignored; output store must visibly change it. */
    unsigned char *snapshot = malloc(page); CHECK(snapshot);
    memcpy(snapshot, protected, page); CHECK(mprotect(protected, page, PROT_READ) == 0);
    int sockets[2]; CHECK(socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sockets) == 0);
    int file = open("/dev/null", O_RDONLY | O_CLOEXEC); CHECK(file >= 0); send_right(sockets[0], file);
    errno = 0; long result = syscall(SYS_recvmsg, sockets[1], message, MSG_CMSG_CLOEXEC); int error = errno;
    const char *name = header ? (crossing ? "recvmsg header writable-to-readonly" : "recvmsg header readonly")
        : (crossing ? "recvmsg control writable-to-readonly" : "recvmsg control readonly");
    finish(name, result, error, protected, snapshot);
}
static void run(int mode) {
    pid_t child = fork(); CHECK(child >= 0);
    if (!child) {
        if (mode < 2) time_output(mode);
        else message_output(mode < 4, mode & 1);
        _exit(0);
    }
    int status; CHECK(waitpid(child, &status, 0) == child);
    if (!WIFEXITED(status) || WEXITSTATUS(status)) {
        fprintf(stderr, "primary output mode %d failed wait-status=%d\n", mode, status);
        exit(1);
    }
}
int main(int argc, char **argv) {
    page = (size_t)sysconf(_SC_PAGESIZE); CHECK(page >= 4096);
    /* Optional individual mode keeps a failing baseline independently useful. */
    if (argc == 2) { int mode = atoi(argv[1]); CHECK(mode >= 0 && mode < 6); run(mode); }
    else { CHECK(argc == 1); for (int mode = 0; mode < 6; mode++) run(mode); }
    puts("PRIMARY anonymous readonly syscall output authority PASS"); return 0;
}
