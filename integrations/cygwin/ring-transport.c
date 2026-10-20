/* SPDX-License-Identifier: MIT
 * In-process Cygwin/libwayland ring adapter. Application source and ABI stay
 * unchanged. The socketpair preserves POSIX poll/readiness within Cygwin;
 * only the native broker-backed DLL touches the shared-memory BAR.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <unistd.h>

__declspec(dllimport) int wc_bridge_open(void);
__declspec(dllimport) intptr_t wc_bridge_read(uint8_t *, size_t);
__declspec(dllimport) intptr_t wc_bridge_write(const uint8_t *, size_t);
__declspec(dllimport) void wc_bridge_close(void);

struct pump { int socket; unsigned references; pthread_mutex_t lock; };

static void release(struct pump *p)
{
    pthread_mutex_lock(&p->lock);
    unsigned left = --p->references;
    pthread_mutex_unlock(&p->lock);
    if (!left) { close(p->socket); pthread_mutex_destroy(&p->lock); free(p); }
}

static void *to_ring(void *opaque)
{
    struct pump *p = opaque;
    uint8_t bytes[65536];
    for (;;) {
        ssize_t count = read(p->socket, bytes, sizeof(bytes));
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) break;
        size_t offset = 0;
        while (offset < (size_t)count) {
            intptr_t sent = wc_bridge_write(bytes + offset, (size_t)count - offset);
            if (sent <= 0) goto done;
            offset += (size_t)sent;
        }
    }
done:
    wc_bridge_close();
    shutdown(p->socket, SHUT_RDWR);
    release(p);
    return NULL;
}

static void *from_ring(void *opaque)
{
    struct pump *p = opaque;
    uint8_t bytes[65536];
    for (;;) {
        intptr_t count = wc_bridge_read(bytes, sizeof(bytes));
        if (count <= 0) break;
        size_t offset = 0;
        while (offset < (size_t)count) {
            /* MSG_NOSIGNAL avoids changing the application's signal policy. */
            ssize_t sent = send(p->socket, bytes + offset, (size_t)count - offset, MSG_NOSIGNAL);
            if (sent < 0 && errno == EINTR) continue;
            if (sent <= 0) goto done;
            offset += (size_t)sent;
        }
    }
done:
    wc_bridge_close();
    shutdown(p->socket, SHUT_RDWR);
    release(p);
    return NULL;
}

int wc_cygwin_ring_socket(void)
{
    int sockets[2];
    if (socketpair(AF_LOCAL, SOCK_STREAM | SOCK_CLOEXEC, 0, sockets) < 0) return -1;
    struct pump *p = calloc(1, sizeof(*p));
    if (!p) { close(sockets[0]); close(sockets[1]); return -1; }
    p->socket = sockets[1];
    p->references = 2;
    pthread_mutex_init(&p->lock, NULL);
    if (wc_bridge_open() != 0) {
        pthread_mutex_destroy(&p->lock); free(p); close(sockets[0]); close(sockets[1]);
        errno = ENODEV; return -1;
    }
    pthread_t tx, rx;
    if (pthread_create(&tx, NULL, to_ring, p) != 0) {
        wc_bridge_close(); pthread_mutex_destroy(&p->lock); free(p);
        close(sockets[0]); close(sockets[1]); errno = EAGAIN; return -1;
    }
    if (pthread_create(&rx, NULL, from_ring, p) != 0) {
        wc_bridge_close(); shutdown(sockets[0], SHUT_RDWR); close(sockets[0]);
        release(p); pthread_join(tx, NULL); errno = EAGAIN; return -1;
    }
    pthread_detach(tx); pthread_detach(rx);
    return sockets[0];
}
