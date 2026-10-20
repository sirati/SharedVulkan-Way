/* Hyperlinux seqpacket cleanup regression. Run only against the adapter:
 * Linux does not promise identical descriptor rollback after recvmsg EFAULT.
 * All inputs remain readable; output-only msghdr stores deliberately fault. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <unistd.h>
#define CHECK(x) do { if (!(x)) { fprintf(stderr, "seqpacket cleanup line %d: %s errno=%d\n", __LINE__, #x, errno); exit(1); } } while (0)
#define RIGHTS 8
#define ITERATIONS 4096
union ancillary { struct cmsghdr align; char bytes[CMSG_SPACE(RIGHTS * sizeof(int))]; };
static void send_rights(int socket, int file) {
    union ancillary control = {0};
    char byte = 'R'; struct iovec io = { &byte, 1 };
    struct msghdr message = { .msg_iov = &io, .msg_iovlen = 1,
        .msg_control = control.bytes, .msg_controllen = sizeof(control.bytes) };
    struct cmsghdr *c = CMSG_FIRSTHDR(&message);
    c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(RIGHTS * sizeof(int));
    int descriptors[RIGHTS]; for (int i = 0; i < RIGHTS; i++) descriptors[i] = file;
    memcpy(CMSG_DATA(c), descriptors, sizeof(descriptors));
    CHECK(sendmsg(socket, &message, MSG_NOSIGNAL) == 1);
}
static void normal_receive(int socket) {
    union ancillary control = {0}; char byte = 0;
    struct iovec io = { &byte, 1 };
    struct msghdr message = { .msg_iov = &io, .msg_iovlen = 1,
        .msg_control = control.bytes, .msg_controllen = sizeof(control.bytes) };
    CHECK(recvmsg(socket, &message, MSG_CMSG_CLOEXEC) == 1 && byte == 'R');
    struct cmsghdr *c = CMSG_FIRSTHDR(&message);
    CHECK(c && c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS);
    CHECK(c->cmsg_len == CMSG_LEN(RIGHTS * sizeof(int)));
    int descriptors[RIGHTS]; memcpy(descriptors, CMSG_DATA(c), sizeof(descriptors));
    for (int i = 0; i < RIGHTS; i++) {
        CHECK(fcntl(descriptors[i], F_GETFD) & FD_CLOEXEC);
        char value; CHECK(pread(descriptors[i], &value, 1, 0) == 1 && value == 'F');
        CHECK(close(descriptors[i]) == 0);
    }
}
static void fault_receives(int socket, int sender, int file, int before_import) {
    size_t page = (size_t)sysconf(_SC_PAGESIZE);
    struct msghdr *message = mmap(NULL, page, PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(message != MAP_FAILED);
    union ancillary control;
    char byte = 0, name[128]; struct iovec io = { &byte, 1 };
    memset(message, 0, sizeof(*message));
    message->msg_iov = &io; message->msg_iovlen = 1;
    message->msg_control = control.bytes; message->msg_controllen = sizeof(control.bytes);
    if (before_import) { message->msg_name = name; message->msg_namelen = sizeof(name); }
    CHECK(mprotect(message, page, PROT_READ) == 0);
    for (int iteration = 0; iteration < ITERATIONS; iteration++) {
        memset(&control, 0, sizeof(control)); byte = 0;
        send_rights(sender, file);
        CHECK(recvmsg(socket, message, MSG_CMSG_CLOEXEC) == -1 && errno == EFAULT);
        CHECK(byte == 'R'); /* Record was consumed before the output store. */
        if (!before_import) {
            /* Writable control was copied before readonly msg_controllen.
             * Descriptor identities must be rolled back after that failure. */
            struct cmsghdr *c = (struct cmsghdr *)control.bytes;
            CHECK(c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS);
            CHECK(c->cmsg_len == CMSG_LEN(RIGHTS * sizeof(int)));
            int descriptors[RIGHTS]; memcpy(descriptors, CMSG_DATA(c), sizeof(descriptors));
            for (int i = 0; i < RIGHTS; i++)
                CHECK(fcntl(descriptors[i], F_GETFD) == -1 && errno == EBADF);
        }
        if (iteration % 128 == 0) { send_rights(sender, file); normal_receive(socket); }
    }
    CHECK(munmap(message, page) == 0);
    printf("SEQPACKET %s EFAULT: %d receives, %d rights, normal receive remains usable PASS\n",
        before_import ? "before-import native cleanup" : "after-import descriptor rollback",
        ITERATIONS, ITERATIONS * RIGHTS);
}
int main(void) {
    int pair[2]; CHECK(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC | SOCK_NONBLOCK, 0, pair) == 0);
    char path[] = "/tmp/seqpacket-cleanup-XXXXXX";
    int file = mkstemp(path); CHECK(file >= 0 && unlink(path) == 0);
    CHECK(write(file, "F", 1) == 1);
    fault_receives(pair[1], pair[0], file, 1);
    fault_receives(pair[1], pair[0], file, 0);
    send_rights(pair[0], file); normal_receive(pair[1]);
    CHECK(close(file) == 0 && close(pair[0]) == 0 && close(pair[1]) == 0);
    puts("SEQPACKET repeated ancillary EFAULT cleanup PASS");
    return 0;
}
