/* Unmodified Linux APIs. Cross-build for ARM64, run under integrated hl.
 * Payload is one shared atomic word; sockets carry notifications only. */
#define _GNU_SOURCE
#include <assert.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <sys/mman.h>
#include <sys/wait.h>

static void exchange(int bell, _Atomic uint32_t *word, uint32_t in, uint32_t out) {
    char wake = 1;
    atomic_store_explicit(word, in, memory_order_release);
    assert(write(bell, &wake, 1) == 1);
    assert(read(bell, &wake, 1) == 1);
    assert(atomic_load_explicit(word, memory_order_acquire) == out);
}
int main(int argc, char **argv) {
    if (argc == 5 && !strcmp(argv[1], "--exec")) {
        int fd = atoi(argv[2]), bell = atoi(argv[3]);
        int cloexec = atoi(argv[4]);
        assert(fcntl(cloexec, F_GETFD) == -1);
        void *map = mmap(NULL, 65536, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        assert(map != MAP_FAILED);
        close(fd);
        exchange(bell, (_Atomic uint32_t *)((char *)map + 4096), 0x57434c35, 0x57434c36);
        assert(munmap(map, 65536) == 0);
        close(bell);
        return 0;
    }
    if (argc != 3) { fprintf(stderr, "usage: %s BACKING SOCKET\n", argv[0]); return 2; }
    int fd = open(argv[1], O_RDWR);
    assert(fd >= 0);
    int alias = dup(fd);
    assert(alias >= 0);
    void *map = mmap(NULL, 65536, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    assert(map != MAP_FAILED);
    close(fd);
    int bell = socket(AF_UNIX, SOCK_STREAM, 0);
    assert(bell >= 0);
    struct sockaddr_un addr = { .sun_family = AF_UNIX };
    assert(strlen(argv[2]) < sizeof(addr.sun_path));
    strcpy(addr.sun_path, argv[2]);
    assert(connect(bell, (struct sockaddr *)&addr, sizeof(addr)) == 0);
    char byte = 0;
    union { struct cmsghdr align; char bytes[CMSG_SPACE(sizeof(int))]; } control;
    struct iovec iov = { &byte, 1 };
    struct msghdr msg = { .msg_iov = &iov, .msg_iovlen = 1,
        .msg_control = control.bytes, .msg_controllen = sizeof(control.bytes) };
    assert(recvmsg(bell, &msg, MSG_CMSG_CLOEXEC) == 1 && (unsigned char)byte == 0xfd);
    struct cmsghdr *header = CMSG_FIRSTHDR(&msg);
    assert(header && header->cmsg_level == SOL_SOCKET && header->cmsg_type == SCM_RIGHTS);
    int imported;
    memcpy(&imported, CMSG_DATA(header), sizeof(imported));
    assert(fcntl(imported, F_GETFD) & FD_CLOEXEC);
    struct stat original_stat, imported_stat;
    assert(fstat(alias, &original_stat) == 0 && fstat(imported, &imported_stat) == 0);
    assert(original_stat.st_ino == imported_stat.st_ino && original_stat.st_dev == imported_stat.st_dev);
    void *import_view = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, imported, 4096);
    assert(import_view != MAP_FAILED);
    close(imported);
    memset(&control, 0, sizeof(control));
    byte = (char)0xfd;
    msg.msg_controllen = sizeof(control.bytes);
    header = CMSG_FIRSTHDR(&msg);
    header->cmsg_level = SOL_SOCKET; header->cmsg_type = SCM_RIGHTS;
    header->cmsg_len = CMSG_LEN(sizeof(alias));
    memcpy(CMSG_DATA(header), &alias, sizeof(alias));
    assert(sendmsg(bell, &msg, MSG_NOSIGNAL) == 1);
    _Atomic uint32_t *word = (_Atomic uint32_t *)((char *)map + 4096);
    exchange(bell, word, 0x57434c31, 0x57434c32);
    assert(atomic_load_explicit((_Atomic uint32_t *)import_view, memory_order_acquire) == 0x57434c32);
    /* Protection changes and partial unmaps must retain the same allocation. */
    assert(mprotect(map, 8192, PROT_READ) == 0);
    assert(atomic_load_explicit(word, memory_order_acquire) == 0x57434c32);
    assert(mprotect(map, 8192, PROT_NONE) == 0);
    assert(mprotect(map, 8192, PROT_READ | PROT_WRITE) == 0);
    assert(atomic_load_explicit(word, memory_order_acquire) == 0x57434c32);
    assert(msync(map, 8192, MS_SYNC) == 0);
    assert(munmap((char *)map + 32768, 4096) == 0);
    void *hole = mmap((char *)map + 32768, 4096, PROT_READ | PROT_WRITE,
        MAP_FIXED | MAP_ANONYMOUS | MAP_PRIVATE, -1, 0);
    assert(hole == (char *)map + 32768);
    *(volatile uint32_t *)hole = 0xaabbccdd;
    uint32_t unchanged = 1;
    assert(pread(alias, &unchanged, sizeof(unchanged), 32768) == sizeof(unchanged));
    assert(unchanged == 0);
    pid_t child = fork();
    assert(child >= 0);
    if (!child) { exchange(bell, word, 0x57434c33, 0x57434c34); _exit(0); }
    int status;
    assert(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    assert(atomic_load_explicit(word, memory_order_acquire) == 0x57434c34);
    int cloexec = fcntl(alias, F_DUPFD_CLOEXEC, 100);
    assert(cloexec >= 100);
    char a[24], b[24], c[24];
    snprintf(a, sizeof(a), "%d", alias);
    snprintf(b, sizeof(b), "%d", bell);
    snprintf(c, sizeof(c), "%d", cloexec);
    execl(argv[0], argv[0], "--exec", a, b, c, (char *)NULL);
    perror("exec");
    return 1;
}
