/* SPDX-License-Identifier: MIT */
#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <sys/socket.h>
#include <unistd.h>
ssize_t wc_cygwin_sendmsg(int, const struct msghdr *, int);
int main(void)
{
    int sockets[2];
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) == 0);
    char data = 'x';
    struct iovec iov = {&data, 1};
    struct msghdr message = {0};
    message.msg_iov = &iov;
    message.msg_iovlen = 1;
    assert(wc_cygwin_sendmsg(sockets[0], &message, 0) == 1);
    assert(read(sockets[1], &data, 1) == 1 && data == 'x');
    char ancillary = 0;
    message.msg_control = &ancillary;
    message.msg_controllen = 1;
    assert(wc_cygwin_sendmsg(sockets[0], &message, 0) == -1 && errno == ENOTSUP);
    assert(recv(sockets[1], &data, 1, MSG_DONTWAIT) == -1 &&
           (errno == EAGAIN || errno == EWOULDBLOCK));
    close(sockets[0]); close(sockets[1]);
    return 0;
}
