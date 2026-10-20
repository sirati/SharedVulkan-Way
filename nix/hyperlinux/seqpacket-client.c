/* Guest acceptance for the bounded AF_UNIX SOCK_SEQPACKET pair ABI. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/select.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/sendfile.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

#define CHECK(test) do { if (!(test)) { \
    fprintf(stderr, "seqpacket line %d: %s, errno=%d\n", __LINE__, #test, errno); \
    exit(1); } } while (0)
#define MAX_RECORD 16384
static int linux_reference;
static void pair(int fd[2]) {
    CHECK(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC | SOCK_NONBLOCK,
                     0, fd) == 0);
    CHECK(fcntl(fd[0], F_GETFD) & FD_CLOEXEC);
    CHECK(fcntl(fd[0], F_GETFL) & O_NONBLOCK);
    int type = 0; socklen_t length = sizeof(type);
    CHECK(getsockopt(fd[0], SOL_SOCKET, SO_TYPE, &type, &length) == 0);
    CHECK(length == sizeof(type) && type == SOCK_SEQPACKET);
}
static void close_pair(int fd[2]) { CHECK(close(fd[0]) == 0); CHECK(close(fd[1]) == 0); }
static void send_fd(int socket, const char *data, size_t length, int fd) {
    union { struct cmsghdr alignment; char bytes[CMSG_SPACE(sizeof(int))]; } control;
    memset(&control, 0, sizeof(control));
    struct iovec io = { (void *)data, length };
    struct msghdr message = { .msg_iov = &io, .msg_iovlen = 1 };
    if (fd >= 0) {
        message.msg_control = control.bytes;
        message.msg_controllen = sizeof(control.bytes);
        struct cmsghdr *header = CMSG_FIRSTHDR(&message);
        header->cmsg_level = SOL_SOCKET;
        header->cmsg_type = SCM_RIGHTS;
        header->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(header), &fd, sizeof(fd));
    }
    CHECK(sendmsg(socket, &message, MSG_NOSIGNAL) == (ssize_t)length);
}
static ssize_t receive(int socket, char *data, size_t length, int flags,
                       int *received, int *output_flags, int with_control) {
    union { struct cmsghdr alignment; char bytes[CMSG_SPACE(sizeof(int))]; } control;
    memset(&control, 0, sizeof(control));
    struct iovec io = { data, length };
    struct msghdr message = { .msg_iov = &io, .msg_iovlen = 1 };
    if (with_control) {
        message.msg_control = control.bytes;
        message.msg_controllen = sizeof(control.bytes);
    }
    *received = -1;
    ssize_t result = recvmsg(socket, &message, flags);
    *output_flags = message.msg_flags;
    if (result >= 0 && message.msg_controllen && with_control) {
        struct cmsghdr *header = CMSG_FIRSTHDR(&message);
        if (header) {
            CHECK(header->cmsg_level == SOL_SOCKET && header->cmsg_type == SCM_RIGHTS);
            CHECK(header->cmsg_len == CMSG_LEN(sizeof(int)));
            memcpy(received, CMSG_DATA(header), sizeof(*received));
        }
    }
    return result;
}
static void empty(int fd) {
    char byte;
    CHECK(recv(fd, &byte, 1, MSG_DONTWAIT) == -1 && errno == EAGAIN);
}
static void check_peek(int fd, const char *expected, int with_control) {
    char bytes[32] = {0}; int passed, flags;
    ssize_t result = receive(fd, bytes, sizeof(bytes), MSG_PEEK | MSG_CMSG_CLOEXEC,
                             &passed, &flags, with_control);
    if (linux_reference) {
        CHECK(result == (ssize_t)strlen(expected));
        CHECK(!memcmp(bytes, expected, strlen(expected)));
        if (passed >= 0) CHECK(close(passed) == 0);
    } else {
        CHECK(result == -1 && errno == EOPNOTSUPP);
        CHECK(passed == -1);
    }
}
static void records(void) {
    int fd[2]; char bytes[64] = {0}; pair(fd);
    CHECK(write(fd[0], "abcdef", 6) == 6);
    struct iovec output[] = { {"GH", 2}, {"ijk", 3} };
    CHECK(writev(fd[0], output, 2) == 5);
    CHECK(read(fd[1], bytes, 3) == 3 && !memcmp(bytes, "abc", 3));
    struct iovec input[] = { {bytes, 1}, {bytes + 1, sizeof(bytes) - 1} };
    CHECK(readv(fd[1], input, 2) == 5 && !memcmp(bytes, "GHijk", 5));
    empty(fd[1]);
    struct msghdr message = { .msg_iov = output, .msg_iovlen = 2 };
    CHECK(sendmsg(fd[0], &message, MSG_NOSIGNAL) == 5);
    message.msg_iov = input; message.msg_iovlen = 2;
    CHECK(recvmsg(fd[1], &message, 0) == 5 && !memcmp(bytes, "GHijk", 5));
    CHECK(!(message.msg_flags & (MSG_TRUNC | MSG_CTRUNC)));
    int passed, flags;
    send_fd(fd[0], "longer", 6, -1);
    CHECK(receive(fd[1], bytes, 2, 0, &passed, &flags, 0) == 2);
    CHECK((flags & MSG_TRUNC) && !memcmp(bytes, "lo", 2)); empty(fd[1]);
    send_fd(fd[0], "longer", 6, -1);
    CHECK(receive(fd[1], bytes, 2, MSG_TRUNC, &passed, &flags, 0) == 6);
    CHECK((flags & MSG_TRUNC) && !memcmp(bytes, "lo", 2)); empty(fd[1]);
    send_fd(fd[0], "peek", 4, -1);
    check_peek(fd[1], "peek", 0); check_peek(fd[1], "peek", 1);
    CHECK(read(fd[1], bytes, sizeof(bytes)) == 4 && !memcmp(bytes, "peek", 4));
    empty(fd[1]);
    CHECK(write(fd[0], "", 0) == 0);
    struct pollfd pollfd = { fd[1], POLLIN, 0 };
    CHECK(poll(&pollfd, 1, 0) == 1 && (pollfd.revents & POLLIN));
    CHECK(read(fd[1], bytes, sizeof(bytes)) == 0); empty(fd[1]);
    int duplicate = fcntl(fd[0], F_DUPFD_CLOEXEC, 300); CHECK(duplicate >= 300);
    CHECK(dup3(fd[0], duplicate, O_CLOEXEC) == duplicate);
    CHECK(fcntl(duplicate, F_GETFD) & FD_CLOEXEC);
    CHECK(dup2(fd[0], duplicate) == duplicate && !(fcntl(duplicate, F_GETFD) & FD_CLOEXEC));
    int type = 0; socklen_t type_length = sizeof(type);
    CHECK(getsockopt(duplicate, SOL_SOCKET, SO_TYPE, &type, &type_length) == 0 && type == SOCK_SEQPACKET);
    CHECK(write(duplicate, "dup", 3) == 3 && read(fd[1], bytes, sizeof(bytes)) == 3 && !memcmp(bytes, "dup", 3));
    CHECK(close(duplicate) == 0);
    close_pair(fd); puts("SEQPACKET records PASS");
}
static void rights(void) {
    int fd[2]; pair(fd);
    int file = memfd_create("seqpacket-identity", MFD_CLOEXEC); CHECK(file >= 0);
    CHECK(write(file, "0123456789", 10) == 10 && lseek(file, 2, SEEK_SET) == 2);
    struct stat original, received; CHECK(fstat(file, &original) == 0);
    send_fd(fd[0], "rights", 6, file);
    check_peek(fd[1], "rights", 0); check_peek(fd[1], "rights", 1);
    char bytes[32]; int passed, flags;
    CHECK(receive(fd[1], bytes, sizeof(bytes), MSG_CMSG_CLOEXEC,
                  &passed, &flags, 1) == 6 && passed >= 0);
    CHECK(!memcmp(bytes, "rights", 6) && !(flags & (MSG_TRUNC | MSG_CTRUNC)));
    CHECK(fcntl(passed, F_GETFD) & FD_CLOEXEC);
    CHECK((fcntl(passed, F_GETFL) & O_ACCMODE) == (fcntl(file, F_GETFL) & O_ACCMODE));
    CHECK(fstat(passed, &received) == 0 && original.st_dev == received.st_dev &&
          original.st_ino == received.st_ino && original.st_size == received.st_size);
    CHECK(read(passed, bytes, 1) == 1 && bytes[0] == '2');
    CHECK(lseek(file, 0, SEEK_CUR) == 3);
    int alias = dup(passed); CHECK(alias >= 0 && !(fcntl(alias, F_GETFD) & FD_CLOEXEC));
    CHECK(close(file) == 0 && close(passed) == 0);
    CHECK(read(alias, bytes, 1) == 1 && bytes[0] == '3');
    /* Omitted ancillary storage must discard rights while consuming the record. */
    send_fd(fd[0], "discard", 7, alias);
    CHECK(receive(fd[1], bytes, sizeof(bytes), 0, &passed, &flags, 0) == 7);
    CHECK((flags & MSG_CTRUNC) && passed == -1); empty(fd[1]);
    send_fd(fd[0], "truncated", 9, alias);
    CHECK(receive(fd[1], bytes, 1, MSG_CMSG_CLOEXEC, &passed, &flags, 1) == 1);
    CHECK((flags & MSG_TRUNC) && passed >= 0); CHECK(close(passed) == 0);
    union { struct cmsghdr align; char bytes[CMSG_SPACE(2 * sizeof(int))]; } control;
    memset(&control, 0, sizeof(control));
    struct iovec io = { (void *)"partial", 7 };
    struct msghdr message = { .msg_iov = &io, .msg_iovlen = 1, .msg_control = control.bytes, .msg_controllen = sizeof(control.bytes) };
    struct cmsghdr *header = CMSG_FIRSTHDR(&message);
    header->cmsg_level = SOL_SOCKET; header->cmsg_type = SCM_RIGHTS; header->cmsg_len = CMSG_LEN(2 * sizeof(int));
    int two_rights[2] = { alias, alias }; memcpy(CMSG_DATA(header), two_rights, sizeof(two_rights));
    CHECK(sendmsg(fd[0], &message, MSG_NOSIGNAL) == 7);
    memset(&control, 0, sizeof(control)); io.iov_base = bytes; io.iov_len = sizeof(bytes);
    message.msg_controllen = CMSG_LEN(sizeof(int)); message.msg_flags = 0;
    CHECK(recvmsg(fd[1], &message, MSG_CMSG_CLOEXEC) == 7 && (message.msg_flags & MSG_CTRUNC));
    header = CMSG_FIRSTHDR(&message); CHECK(header && header->cmsg_len == CMSG_LEN(sizeof(int)));
    memcpy(&passed, CMSG_DATA(header), sizeof(passed)); CHECK(fcntl(passed, F_GETFD) & FD_CLOEXEC);
    CHECK(close(passed) == 0); empty(fd[1]);
    CHECK(close(alias) == 0); close_pair(fd);
    puts("SEQPACKET rights identity CLOEXEC offsets dup PASS");
}
static void endpoints(void) {
    int channel[2], response[2]; pair(channel); pair(response);
    send_fd(channel[0], "request", 7, response[1]); CHECK(close(response[1]) == 0);
    char bytes[32]; int endpoint, flags;
    CHECK(receive(channel[1], bytes, sizeof(bytes), MSG_CMSG_CLOEXEC,
                  &endpoint, &flags, 1) == 7 && endpoint >= 0);
    send_fd(endpoint, "real", 4, -1);
    /* Firefox keeps the sending endpoint alive in its trailing fake response. */
    send_fd(endpoint, "fake", 4, endpoint); CHECK(close(endpoint) == 0);
    CHECK(read(response[0], bytes, sizeof(bytes)) == 4 && !memcmp(bytes, "real", 4));
    CHECK(receive(response[0], bytes, sizeof(bytes), MSG_CMSG_CLOEXEC,
                  &endpoint, &flags, 1) == 4 && endpoint >= 0);
    CHECK(!memcmp(bytes, "fake", 4)); empty(response[0]);
    CHECK(close(endpoint) == 0); CHECK(read(response[0], bytes, sizeof(bytes)) == 0);
    CHECK(close(response[0]) == 0); close_pair(channel);
    puts("SEQPACKET endpoint transfer and queued self-reference PASS");
}
static void shutdowns(void) {
    int fd[2]; pair(fd); char bytes[32];
    send_fd(fd[0], "before", 6, -1); CHECK(shutdown(fd[0], SHUT_WR) == 0);
    struct pollfd pollfd = { fd[1], POLLIN | POLLRDHUP, 0 };
    CHECK(poll(&pollfd, 1, 0) == 1 && (pollfd.revents & POLLIN));
    CHECK(read(fd[1], bytes, sizeof(bytes)) == 6 && !memcmp(bytes, "before", 6));
    CHECK(read(fd[1], bytes, sizeof(bytes)) == 0);
    CHECK(send(fd[0], "x", 1, MSG_NOSIGNAL) == -1 && errno == EPIPE);
    send_fd(fd[1], "reverse", 7, -1);
    CHECK(read(fd[0], bytes, sizeof(bytes)) == 7 && !memcmp(bytes, "reverse", 7));
    close_pair(fd); pair(fd);
    CHECK(shutdown(fd[1], SHUT_RD) == 0);
    CHECK(read(fd[1], bytes, sizeof(bytes)) == 0);
    CHECK(send(fd[0], "x", 1, MSG_NOSIGNAL) == -1 && errno == EPIPE);
    close_pair(fd); puts("SEQPACKET shutdown EOF NOSIGNAL PASS");
}
static void shared_shutdown(void) {
    int channel[2], fd[2]; pair(channel); pair(fd);
    send_fd(channel[0], "endpoint", 8, fd[1]); CHECK(close(fd[1]) == 0);
    char bytes[32]; int endpoint, flags;
    CHECK(receive(channel[1], bytes, sizeof(bytes), MSG_CMSG_CLOEXEC, &endpoint, &flags, 1) == 8 && endpoint >= 0);
    CHECK(shutdown(endpoint, SHUT_RD) == 0);
    CHECK(send(fd[0], "closed", 6, MSG_NOSIGNAL) == -1 && errno == EPIPE);
    send_fd(endpoint, "reverse", 7, -1);
    CHECK(read(fd[0], bytes, sizeof(bytes)) == 7 && !memcmp(bytes, "reverse", 7));
    pid_t child = fork(); CHECK(child >= 0);
    if (!child) { CHECK(shutdown(fd[0], SHUT_RD) == 0); _exit(0); }
    int status; CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    CHECK(send(endpoint, "closed", 6, MSG_NOSIGNAL) == -1 && errno == EPIPE);
    CHECK(read(endpoint, bytes, sizeof(bytes)) == 0);
    CHECK(close(endpoint) == 0 && close(fd[0]) == 0); close_pair(channel);
    puts("SEQPACKET shared shutdown through rights and fork PASS");
}
static void queued_read_shutdown(void) {
    for (int writer_closed = 0; writer_closed < 2; writer_closed++) {
        int fd[2], pipefd[2]; pair(fd); CHECK(pipe(pipefd) == 0);
        CHECK(write(pipefd[1], "R", 1) == 1);
        send_fd(fd[0], "queued", 6, pipefd[0]);
        CHECK(close(pipefd[0]) == 0 && close(pipefd[1]) == 0);
        send_fd(fd[0], "second", 6, -1);
        if (writer_closed) CHECK(shutdown(fd[0], SHUT_WR) == 0);
        int alias = dup(fd[1]); CHECK(alias >= 0);
        CHECK(shutdown(alias, SHUT_RD) == 0 && close(alias) == 0);
        CHECK(send(fd[0], "late", 4, MSG_NOSIGNAL) == -1 && errno == EPIPE);
        char bytes[32]; int passed, flags;
        CHECK(receive(fd[1], bytes, sizeof(bytes), MSG_CMSG_CLOEXEC, &passed, &flags, 1) == 6);
        CHECK(!memcmp(bytes, "queued", 6) && passed >= 0);
        CHECK(read(passed, bytes, sizeof(bytes)) == 1 && bytes[0] == 'R');
        CHECK(close(passed) == 0);
        CHECK(read(fd[1], bytes, sizeof(bytes)) == 6 && !memcmp(bytes, "second", 6));
        CHECK(read(fd[1], bytes, sizeof(bytes)) == 0);
        CHECK(read(fd[1], bytes, sizeof(bytes)) == 0);
        close_pair(fd);
    }
    int fd[2]; pair(fd); send_fd(fd[0], "forkqueue", 9, -1);
    pid_t child = fork(); CHECK(child >= 0);
    if (!child) { CHECK(shutdown(fd[1], SHUT_RD) == 0); _exit(0); }
    int status; CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    char bytes[32]; CHECK(read(fd[1], bytes, sizeof(bytes)) == 9 && !memcmp(bytes, "forkqueue", 9));
    CHECK(read(fd[1], bytes, sizeof(bytes)) == 0); close_pair(fd);
    /* Firefox's receiver blocks before another thread/process calls SHUT_RD. */
    pair(fd); CHECK(fcntl(fd[1], F_SETFL, 0) == 0);
    child = fork(); CHECK(child >= 0);
    if (!child) { usleep(50000); CHECK(shutdown(fd[1], SHUT_RD) == 0); _exit(0); }
    CHECK(read(fd[1], bytes, sizeof(bytes)) == 0);
    CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    close_pair(fd);
    puts("SEQPACKET queued SHUT_RD rights dup fork and blocking wake PASS");
}
static void shutdown_readiness(void) {
    for (int kind = 0; kind < 3; kind++) {
        int fd[2]; pair(fd); int ep = -1;
        struct epoll_event event = { .events = EPOLLIN | EPOLLRDHUP, .data.u64 = 19 }, result;
        if (kind == 1) {
            ep = epoll_create1(EPOLL_CLOEXEC); CHECK(ep >= 0);
            CHECK(epoll_ctl(ep, EPOLL_CTL_ADD, fd[1], &event) == 0);
        }
        pid_t child = fork(); CHECK(child >= 0);
        if (!child) { usleep(50000); CHECK(shutdown(fd[1], SHUT_RD) == 0); _exit(0); }
        if (kind == 0) {
            struct pollfd p = { fd[1], POLLIN | POLLRDHUP, 0 };
            CHECK(poll(&p, 1, 2000) == 1 && (p.revents & POLLIN) && (p.revents & POLLRDHUP));
            CHECK(poll(&p, 1, 0) == 1 && (p.revents & POLLIN) && (p.revents & POLLRDHUP));
        } else if (kind == 1) {
            CHECK(epoll_wait(ep, &result, 1, 2000) == 1 && result.data.u64 == 19 &&
                  (result.events & EPOLLIN) && (result.events & EPOLLRDHUP));
            CHECK(epoll_wait(ep, &result, 1, 0) == 1 && (result.events & EPOLLIN) &&
                  (result.events & EPOLLRDHUP));
            event.events |= EPOLLOUT;
            CHECK(epoll_ctl(ep, EPOLL_CTL_MOD, fd[1], &event) == 0);
            CHECK(epoll_wait(ep, &result, 1, 0) == 1 &&
                  (result.events & EPOLLIN) && (result.events & EPOLLOUT));
        } else {
            fd_set r, w; FD_ZERO(&r); FD_SET(fd[1], &r);
            struct timespec bounded = {2, 0};
            CHECK(pselect(fd[1] + 1, &r, NULL, NULL, &bounded, NULL) == 1 && FD_ISSET(fd[1], &r));
            FD_ZERO(&r); FD_ZERO(&w); FD_SET(fd[1], &r); FD_SET(fd[1], &w);
            struct timespec zero = {0};
            CHECK(pselect(fd[1] + 1, &r, &w, NULL, &zero, NULL) == 2 &&
                  FD_ISSET(fd[1], &r) && FD_ISSET(fd[1], &w));
        }
        int status; CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
        CHECK(shutdown(fd[0], SHUT_RD) == 0);
        struct pollfd p = {fd[1], POLLIN | POLLRDHUP, 0};
        CHECK(poll(&p, 1, 0) == 1 && (p.revents & POLLHUP));
        if (ep >= 0) CHECK(close(ep) == 0);
        close_pair(fd);
    }
    /* A read-closed peer cannot fabricate writable readiness on a full queue. */
    int fd[2]; pair(fd); unsigned count = 0; char byte;
    for (;;) {
        ssize_t n = send(fd[0], "B", 1, MSG_NOSIGNAL);
        if (n < 0) { CHECK(errno == EAGAIN); break; }
        CHECK(n == 1); CHECK(++count < 65536);
    }
    CHECK(count > 0 && shutdown(fd[1], SHUT_RD) == 0);
    struct pollfd p = {fd[0], POLLOUT, 0}; CHECK(poll(&p, 1, 0) == 0);
    CHECK(send(fd[0], "late", 4, MSG_NOSIGNAL) == -1 && errno == EPIPE);
    for (unsigned i = 0; i < count; i++) CHECK(read(fd[1], &byte, 1) == 1 && byte == 'B');
    CHECK(read(fd[1], &byte, 1) == 0); close_pair(fd);
    /* Registration metadata must not retain the actual endpoint transport. */
    pair(fd); int ep = epoll_create1(EPOLL_CLOEXEC); CHECK(ep >= 0);
    struct epoll_event event = { .events = EPOLLIN, .data.u64 = 29 }, result;
    CHECK(epoll_ctl(ep, EPOLL_CTL_ADD, fd[1], &event) == 0 && close(fd[1]) == 0);
    CHECK(recv(fd[0], &byte, 1, MSG_DONTWAIT) == 0);
    CHECK(close(fd[0]) == 0 && close(ep) == 0);
    /* Preserve ordinary legacy pipe epoll behavior while adding metadata. */
    int pipefd[2]; CHECK(pipe(pipefd) == 0); ep = epoll_create1(EPOLL_CLOEXEC); CHECK(ep >= 0);
    CHECK(epoll_ctl(ep, EPOLL_CTL_ADD, pipefd[0], &event) == 0 && close(pipefd[1]) == 0);
    CHECK(epoll_wait(ep, &result, 1, 1000) == 1 && (result.events & EPOLLHUP));
    CHECK(close(pipefd[0]) == 0 && close(ep) == 0);
    /* The child's epoll imports precede socket metadata in the fork protocol. */
    pair(fd); ep = epoll_create1(EPOLL_CLOEXEC); CHECK(ep >= 0);
    CHECK(epoll_ctl(ep, EPOLL_CTL_ADD, fd[1], &event) == 0);
    pid_t child = fork(); CHECK(child >= 0);
    if (!child) {
        CHECK(epoll_wait(ep, &result, 1, 2000) == 1 && (result.events & EPOLLIN));
        _exit(0);
    }
    usleep(50000); CHECK(shutdown(fd[1], SHUT_RD) == 0);
    int status; CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    CHECK(close(ep) == 0); close_pair(fd);
    if (!linux_reference) {
        pair(fd); ep = epoll_create1(EPOLL_CLOEXEC); CHECK(ep >= 0);
        event.events = EPOLLIN | EPOLLET;
        CHECK(epoll_ctl(ep, EPOLL_CTL_ADD, fd[1], &event) == -1 && errno == EOPNOTSUPP);
        event.events = EPOLLIN; CHECK(epoll_ctl(ep, EPOLL_CTL_ADD, fd[1], &event) == 0);
        event.events |= EPOLLONESHOT;
        CHECK(epoll_ctl(ep, EPOLL_CTL_MOD, fd[1], &event) == -1 && errno == EOPNOTSUPP);
        send_fd(fd[0], "kept", 4, -1);
        CHECK(epoll_wait(ep, &result, 1, 0) == 1 && (result.events & EPOLLIN));
        CHECK(close(ep) == 0); close_pair(fd);
    }
    puts("SEQPACKET event-driven shutdown poll epoll pselect lifetime PASS");
}
static void shutdown_send_race(void) {
    int fd[2], report[2]; pair(fd); CHECK(pipe(report) == 0);
    pid_t child = fork(); CHECK(child >= 0);
    if (!child) {
        CHECK(close(report[0]) == 0);
        unsigned accepted = 0;
        for (;;) {
            ssize_t n = send(fd[0], "R", 1, MSG_NOSIGNAL);
            if (n == 1) { accepted++; continue; }
            CHECK(n == -1);
            if (errno == EPIPE) break;
            CHECK(errno == EAGAIN); usleep(1000);
        }
        CHECK(write(report[1], &accepted, sizeof(accepted)) == sizeof(accepted));
        _exit(0);
    }
    CHECK(close(report[1]) == 0); usleep(50000);
    CHECK(shutdown(fd[1], SHUT_RD) == 0);
    unsigned drained = 0; char byte;
    for (;;) {
        ssize_t n = recv(fd[1], &byte, 1, MSG_DONTWAIT);
        if (!n) break;
        CHECK(n == 1 && byte == 'R'); drained++;
    }
    unsigned accepted = 0;
    CHECK(read(report[0], &accepted, sizeof(accepted)) == sizeof(accepted));
    CHECK(accepted > 0 && drained == accepted);
    int status; CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    CHECK(read(fd[1], &byte, 1) == 0);
    CHECK(close(report[0]) == 0); close_pair(fd);
    puts("SEQPACKET concurrent accepted records preserved across SHUT_RD PASS");
}
static void readiness(void) {
    int fd[2]; pair(fd);
    struct pollfd p = { fd[0], POLLIN | POLLOUT, 0 };
    CHECK(poll(&p, 1, 0) == 1 && (p.revents & POLLOUT) && !(p.revents & POLLIN));
    int ep = epoll_create1(EPOLL_CLOEXEC); CHECK(ep >= 0);
    struct epoll_event event = { .events = EPOLLIN | EPOLLOUT, .data.u64 = 7 }, result;
    CHECK(epoll_ctl(ep, EPOLL_CTL_ADD, fd[1], &event) == 0);
    CHECK(epoll_wait(ep, &result, 1, 0) == 1 && result.data.u64 == 7 &&
          (result.events & EPOLLOUT) && !(result.events & EPOLLIN));
    send_fd(fd[0], "ready", 5, -1);
    CHECK(epoll_wait(ep, &result, 1, 0) == 1 && (result.events & EPOLLIN));
    char bytes[MAX_RECORD + 1]; memset(bytes, 'Q', sizeof(bytes));
    CHECK(read(fd[1], bytes, sizeof(bytes)) == 5); empty(fd[1]);
    memset(bytes, 'Q', sizeof(bytes));
    CHECK(send(fd[0], bytes, MAX_RECORD, MSG_NOSIGNAL) == MAX_RECORD);
    CHECK(read(fd[1], bytes, sizeof(bytes)) == MAX_RECORD);
    for (unsigned i = 0; i < MAX_RECORD; i++) CHECK(bytes[i] == 'Q');
    if (!linux_reference) {
        CHECK(send(fd[0], bytes, sizeof(bytes), MSG_NOSIGNAL) == -1 && errno == EMSGSIZE);
        empty(fd[1]);
    }
    unsigned count;
    for (count = 0; count < 65536; count++) {
        ssize_t sent = send(fd[0], bytes, 1, MSG_NOSIGNAL);
        if (sent < 0) { CHECK(errno == EAGAIN); break; }
        CHECK(sent == 1);
    }
    CHECK(count > 0 && count < 65536);
    if (!linux_reference) {
        CHECK(count <= 256);
        printf("SEQPACKET queued records=%u maximum=256 payload_limit=%d\n", count, MAX_RECORD);
    }
    p = (struct pollfd){ fd[0], POLLOUT, 0 };
    CHECK(poll(&p, 1, 0) == 0);
    for (unsigned i = 0; i < count; i++) CHECK(read(fd[1], bytes, sizeof(bytes)) == 1);
    CHECK(poll(&p, 1, 0) == 1 && (p.revents & POLLOUT));
    CHECK(epoll_wait(ep, &result, 1, 0) == 1 && !(result.events & EPOLLIN));
    CHECK(close(ep) == 0); close_pair(fd);
    puts("SEQPACKET bounded backpressure poll epoll PASS");
}
static void unsupported(void) {
    if (linux_reference) return;
    CHECK(socket(AF_UNIX, SOCK_SEQPACKET, 0) == -1 && errno == EOPNOTSUPP);
    int fd[2]; pair(fd); int value = 8192;
    CHECK(setsockopt(fd[0], SOL_SOCKET, SO_SNDBUF, &value, sizeof(value)) == -1 && errno == EOPNOTSUPP);
    CHECK(setsockopt(fd[0], SOL_SOCKET, SO_RCVBUF, &value, sizeof(value)) == -1 && errno == EOPNOTSUPP);
    value = 1;
    CHECK(setsockopt(fd[0], SOL_SOCKET, SO_PASSCRED, &value, sizeof(value)) == -1 && errno == EOPNOTSUPP);
    struct sockaddr_un address = { .sun_family = AF_UNIX };
    memcpy(address.sun_path, "seqpacket-unsupported", sizeof("seqpacket-unsupported"));
    CHECK(bind(fd[0], (struct sockaddr *)&address, sizeof(address)) == -1 && errno == EOPNOTSUPP);
    CHECK(connect(fd[0], (struct sockaddr *)&address, sizeof(address)) == -1 && errno == EOPNOTSUPP);
    CHECK(listen(fd[0], 1) == -1 && errno == EOPNOTSUPP);
    send_fd(fd[0], "not-error-queue", 15, -1);
    char bytes[32]; int passed, flags;
    CHECK(receive(fd[1], bytes, sizeof(bytes), MSG_ERRQUEUE, &passed, &flags, 0) == -1 && errno == EOPNOTSUPP);
    CHECK(read(fd[1], bytes, sizeof(bytes)) == 15 && !memcmp(bytes, "not-error-queue", 15));
    CHECK(send(fd[0], "oob", 3, MSG_OOB | MSG_NOSIGNAL) == -1 && errno == EOPNOTSUPP); empty(fd[1]);
    int special = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC); CHECK(special >= 0);
    CHECK(dup3(fd[0], special, O_CLOEXEC) == -1 && errno == EOPNOTSUPP);
    unsigned long long counter = 1, output = 0;
    CHECK(write(special, &counter, sizeof(counter)) == sizeof(counter));
    CHECK(read(special, &output, sizeof(output)) == sizeof(output) && output == 1);
    union { struct cmsghdr align; char bytes[CMSG_SPACE(sizeof(int))]; } control;
    memset(&control, 0, sizeof(control)); struct iovec io = { (void *)"synthetic", 9 };
    struct msghdr message = { .msg_iov = &io, .msg_iovlen = 1, .msg_control = control.bytes, .msg_controllen = sizeof(control.bytes) };
    struct cmsghdr *header = CMSG_FIRSTHDR(&message);
    header->cmsg_level = SOL_SOCKET; header->cmsg_type = SCM_RIGHTS; header->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(header), &special, sizeof(special));
    CHECK(sendmsg(fd[0], &message, MSG_NOSIGNAL) == -1 && errno == EOPNOTSUPP); empty(fd[1]);
    CHECK(close(special) == 0);
    int file = memfd_create("seqpacket-control-limit", MFD_CLOEXEC); CHECK(file >= 0);
    union { struct cmsghdr align; char bytes[CMSG_SPACE(60 * sizeof(int))]; } large_control;
    memset(&large_control, 0, sizeof(large_control));
    int many[60]; for (unsigned i = 0; i < 60; i++) many[i] = file;
    message.msg_control = large_control.bytes; message.msg_controllen = sizeof(large_control.bytes);
    header = CMSG_FIRSTHDR(&message); header->cmsg_level = SOL_SOCKET; header->cmsg_type = SCM_RIGHTS;
    header->cmsg_len = CMSG_LEN(sizeof(many)); memcpy(CMSG_DATA(header), many, sizeof(many));
    CHECK(sendmsg(fd[0], &message, MSG_NOSIGNAL) == -1 && errno == EMSGSIZE); empty(fd[1]);
    CHECK(fcntl(file, F_GETFD) >= 0 && close(file) == 0);
    close_pair(fd); puts("SEQPACKET unsupported operations PASS");
}
static void byte_copy_refusal(void) {
    if (linux_reference) return;
    int fd[2], pipefd[2]; pair(fd); CHECK(pipe(pipefd) == 0);
    int file = memfd_create("seqpacket-copy-refusal", MFD_CLOEXEC); CHECK(file >= 0);
    CHECK(write(file, "input", 5) == 5 && lseek(file, 0, SEEK_SET) == 0);
    off_t offset = 0;
    CHECK(sendfile(fd[0], file, &offset, 5) == -1 && errno == EOPNOTSUPP);
    CHECK(offset == 0 && lseek(file, 0, SEEK_CUR) == 0); empty(fd[1]);
    struct iovec io = { (void *)"raw", 3 };
    CHECK(vmsplice(fd[0], &io, 1, 0) == -1 && errno == EOPNOTSUPP); empty(fd[1]);
    send_fd(fd[0], "preserved", 9, -1);
    CHECK(splice(fd[1], NULL, pipefd[1], NULL, 9, SPLICE_F_NONBLOCK) == -1 && errno == EOPNOTSUPP);
    char bytes[32]; CHECK(read(fd[1], bytes, sizeof(bytes)) == 9 && !memcmp(bytes, "preserved", 9));
    send_fd(fd[0], "record", 6, -1);
    CHECK(copy_file_range(fd[1], NULL, file, NULL, 6, 0) == -1 && errno == EOPNOTSUPP);
    CHECK(read(fd[1], bytes, sizeof(bytes)) == 6 && !memcmp(bytes, "record", 6));
    CHECK(lseek(file, 0, SEEK_CUR) == 0);
    CHECK(close(file) == 0 && close(pipefd[0]) == 0 && close(pipefd[1]) == 0); close_pair(fd);
    puts("SEQPACKET byte copy refusal preserves input PASS");
}
static void sender_exit(void) {
    int channel[2]; pair(channel);
    pid_t child = fork(); CHECK(child >= 0);
    if (!child) {
        CHECK(close(channel[1]) == 0);
        int response[2]; pair(response);
        send_fd(response[0], "after-exit", 10, -1);
        send_fd(channel[0], "endpoint", 8, response[1]);
        close_pair(response); CHECK(close(channel[0]) == 0); _exit(0);
    }
    CHECK(close(channel[0]) == 0);
    int status; CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    char bytes[32]; int endpoint, flags;
    CHECK(receive(channel[1], bytes, sizeof(bytes), MSG_CMSG_CLOEXEC, &endpoint, &flags, 1) == 8 && endpoint >= 0);
    int type = 0; socklen_t length = sizeof(type);
    CHECK(getsockopt(endpoint, SOL_SOCKET, SO_TYPE, &type, &length) == 0 && type == SOCK_SEQPACKET);
    CHECK(read(endpoint, bytes, sizeof(bytes)) == 10 && !memcmp(bytes, "after-exit", 10));
    CHECK(read(endpoint, bytes, sizeof(bytes)) == 0);
    CHECK(close(endpoint) == 0 && close(channel[1]) == 0);
    puts("SEQPACKET queued endpoint after sender exit PASS");
}
static void fork_exec(const char *self) {
    int fd[2]; pair(fd);
    int keep = fcntl(fd[1], F_DUPFD, 100); CHECK(keep >= 100);
    int cloexec = fcntl(fd[1], F_DUPFD_CLOEXEC, 200); CHECK(cloexec >= 200);
    send_fd(fd[0], "exec", 4, -1);
    pid_t child = fork(); CHECK(child >= 0);
    if (!child) {
        char keep_arg[24], close_arg[24];
        snprintf(keep_arg, sizeof(keep_arg), "%d", keep);
        snprintf(close_arg, sizeof(close_arg), "%d", cloexec);
        execl(self, self, "--exec-check", keep_arg, close_arg, (char *)NULL);
        perror("seqpacket exec"); _exit(1);
    }
    int status; CHECK(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    char bytes[8]; CHECK(read(fd[0], bytes, sizeof(bytes)) == 5 && !memcmp(bytes, "child", 5));
    CHECK(close(keep) == 0 && close(cloexec) == 0); close_pair(fd);
    puts("SEQPACKET fork exec inherited endpoint CLOEXEC PASS");
}
int main(int argc, char **argv) {
    alarm(30);
    if (argc == 4 && !strcmp(argv[1], "--exec-check")) {
        int keep = atoi(argv[2]), cloexec = atoi(argv[3]); char bytes[8];
        CHECK(fcntl(cloexec, F_GETFD) == -1 && errno == EBADF);
        CHECK(fcntl(keep, F_GETFD) >= 0);
        CHECK(fcntl(keep, F_GETFL) & O_NONBLOCK);
        CHECK((fcntl(keep, F_GETFL) & O_ACCMODE) == O_RDWR);
        CHECK(read(keep, bytes, sizeof(bytes)) == 4 && !memcmp(bytes, "exec", 4));
        send_fd(keep, "child", 5, -1); CHECK(close(keep) == 0); return 0;
    }
    CHECK(argc == 1 || (argc == 2 && !strcmp(argv[1], "--linux-reference")));
    linux_reference = argc == 2;
    records(); rights(); endpoints(); shutdowns(); shared_shutdown(); queued_read_shutdown(); shutdown_readiness(); shutdown_send_race(); readiness(); unsupported(); byte_copy_refusal(); fork_exec(argv[0]); sender_exit();
    puts("SEQPACKET ALL PASS"); return 0;
}
