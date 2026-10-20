/* SPDX-License-Identifier: MIT
 * Records the real Cygwin AF_LOCAL descriptor semantics on a disposable VM.
 * Not a Wayland application and not proof of rendered frames.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void)
{
    int sockets[2];
    if (socketpair(AF_LOCAL, SOCK_STREAM, 0, sockets) < 0) {
        perror("socketpair");
        return 1;
    }
    int fd = open("/dev/null", O_RDONLY);
    if (fd < 0) { perror("open"); return 1; }
    char byte = 'x';
    struct iovec iov = {&byte, 1};
    union { struct cmsghdr align; char bytes[CMSG_SPACE(sizeof(int))]; } control;
    memset(&control, 0, sizeof(control));
    struct msghdr message = {0};
    message.msg_iov = &iov;
    message.msg_iovlen = 1;
    message.msg_control = control.bytes;
    message.msg_controllen = sizeof(control.bytes);
    struct cmsghdr *header = CMSG_FIRSTHDR(&message);
    header->cmsg_level = SOL_SOCKET;
    header->cmsg_type = SCM_RIGHTS;
    header->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(header), &fd, sizeof(fd));
    ssize_t sent = sendmsg(sockets[0], &message, 0);
    int send_errno = errno;
    memset(&control, 0, sizeof(control));
    message.msg_controllen = sizeof(control.bytes);
    ssize_t got = recvmsg(sockets[1], &message, MSG_DONTWAIT);
    int recv_errno = errno;
    unsigned count = 0;
    if (got >= 0) {
        for (header = CMSG_FIRSTHDR(&message); header; header = CMSG_NXTHDR(&message, header)) {
            if (header->cmsg_level == SOL_SOCKET && header->cmsg_type == SCM_RIGHTS &&
                header->cmsg_len >= CMSG_LEN(sizeof(int))) {
                int received;
                memcpy(&received, CMSG_DATA(header), sizeof(received));
                if (fcntl(received, F_GETFD) >= 0) { count++; close(received); }
            }
        }
    }
    printf("CYGWIN_FD send=%ld send_errno=%d recv=%ld recv_errno=%d valid_received_fds=%u\n",
           (long)sent, sent < 0 ? send_errno : 0, (long)got, got < 0 ? recv_errno : 0, count);
    close(fd);
    close(sockets[0]); close(sockets[1]);
    return sent == 1 && got == 1 && count == 1 ? 0 : 2;
}
