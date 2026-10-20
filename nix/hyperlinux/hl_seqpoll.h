#ifndef HL_SEQPOLL_H
#define HL_SEQPOLL_H
#include "hl_socket.h"
#include <errno.h>
#include <fcntl.h>
#include <sys/event.h>
#include <sys/select.h>
#include <poll.h>
#include <time.h>
#include <limits.h>
#define HL_SEQ_RDHUP 0x2000
static uint16_t hl_seqpoll_flags(hl_open_file_t *of, uint16_t requested) {
    if (!of || !hl_wc_socket_read_closed(of, 0)) return 0;
    uint16_t result = requested & (POLLIN | HL_SEQ_RDHUP);
    if (hl_wc_socket_read_closed(of, 1)) result |= POLLHUP;
    return result;
}
static int64_t hl_seqpoll_now(void) {
    struct timespec now; clock_gettime(CLOCK_MONOTONIC, &now);
    return (int64_t)now.tv_sec * 1000000000 + now.tv_nsec;
}
/* Register before sampling shared metadata. EV_CLEAR ensures an unrelated
 * native edge cannot repeatedly wake a waiter whose requested bits are empty.
 * The final readiness sample uses native poll, preserving native OUT/errors. */
static int hl_seqpoll_wait(struct pollfd *fds, nfds_t count,
                           hl_open_file_t *const *options, int timeout_ms) {
    int has_seq = 0;
    for (nfds_t i = 0; i < count; i++) if (options[i]) has_seq = 1;
    if (!has_seq) return poll(fds, count, timeout_ms);
    if (count > FD_TABLE_SIZE) { errno = EINVAL; return -1; }
    struct pollfd native[FD_TABLE_SIZE];
    int watches[FD_TABLE_SIZE];
    int queue = -1, result = -1;
    for (nfds_t i = 0; i < count; i++) { native[i] = fds[i]; native[i].events &= ~HL_SEQ_RDHUP; watches[i] = -1; }
    int64_t deadline = timeout_ms < 0 ? INT64_MAX : hl_seqpoll_now() + (int64_t)timeout_ms * 1000000;
    /* Immediate readiness, including invalid native aliases, needs no watch.
     * A blocking path registers first and samples again below. */
    if (poll(native, count, 0) < 0) goto done;
    result = 0;
    for (nfds_t i = 0; i < count; i++) {
        fds[i].revents = native[i].revents | hl_seqpoll_flags(options[i], fds[i].events);
        if (fds[i].revents) result++;
    }
    if (result || timeout_ms == 0) goto done;
    queue = kqueue(); if (queue < 0) { result = -1; goto done; }
    if (fcntl(queue, F_SETFD, FD_CLOEXEC) < 0) { result = -1; goto done; }
    for (nfds_t i = 0; i < count; i++) {
        if (fds[i].fd < 0) continue;
        struct kevent changes[3]; int n = 0;
        EV_SET(&changes[n++], fds[i].fd, EVFILT_READ, EV_ADD | EV_CLEAR, 0, 0, NULL);
        if (fds[i].events & POLLOUT)
            EV_SET(&changes[n++], fds[i].fd, EVFILT_WRITE, EV_ADD | EV_CLEAR, 0, 0, NULL);
        if (fds[i].events & POLLPRI) {
#ifdef EVFILT_EXCEPT
            EV_SET(&changes[n++], fds[i].fd, EVFILT_EXCEPT, EV_ADD | EV_CLEAR, NOTE_OOB, 0, NULL);
#else
            errno = EOPNOTSUPP; result = -1; goto done;
#endif
        }
        if (kevent(queue, changes, n, NULL, 0, NULL) < 0) { result = -1; goto done; }
        if (options[i]) {
            watches[i] = hl_wc_socket_state_watch(options[i]);
            if (watches[i] < 0) { result = -1; goto done; }
            struct kevent change;
            EV_SET(&change, watches[i], EVFILT_VNODE, EV_ADD | EV_CLEAR, NOTE_WRITE, 0, NULL);
            if (kevent(queue, &change, 1, NULL, 0, NULL) < 0) { result = -1; goto done; }
        }
    }
    for (;;) {
        result = poll(native, count, 0);
        if (result < 0) break;
        result = 0;
        for (nfds_t i = 0; i < count; i++) {
            fds[i].revents = native[i].revents | hl_seqpoll_flags(options[i], fds[i].events);
            if (fds[i].revents) result++;
        }
        if (result) break;
        int64_t remaining = deadline - hl_seqpoll_now();
        if (remaining <= 0) break;
        struct timespec wait = { remaining / 1000000000, remaining % 1000000000 };
        struct kevent event;
        int n = kevent(queue, NULL, 0, &event, 1, timeout_ms < 0 ? NULL : &wait);
        if (n < 0) { result = -1; break; }
        if (!n) break;
        if (event.flags & EV_ERROR) { errno = event.data ? (int)event.data : EIO; result = -1; break; }
    }
done: {
    int saved = errno;
    if (queue >= 0) hl_wc_socket_native_close(queue);
    for (nfds_t i = 0; i < count; i++) if (watches[i] >= 0) hl_wc_socket_native_close(watches[i]);
    errno = saved; return result;
}}
static int hl_seqpoll_select(int nfds, fd_set *reads, fd_set *writes, fd_set *excepts,
                             const struct timespec *timeout, hl_open_file_t *const *options) {
    struct pollfd fds[FD_SETSIZE]; hl_open_file_t *selected[FD_SETSIZE];
    int count = 0;
    for (int fd = 0; fd < nfds; fd++) {
        short events = (reads && FD_ISSET(fd, reads) ? POLLIN : 0) |
            (writes && FD_ISSET(fd, writes) ? POLLOUT : 0) |
            (excepts && FD_ISSET(fd, excepts) ? POLLPRI : 0);
        if (!events) continue;
        fds[count] = (struct pollfd){ fd, events, 0 }; selected[count++] = options[fd];
    }
    int milliseconds = -1;
    if (timeout) {
        if (timeout->tv_sec > INT_MAX / 1000) milliseconds = INT_MAX;
        else {
            int64_t value = timeout->tv_sec * (int64_t)1000 + (timeout->tv_nsec + 999999) / 1000000;
            milliseconds = value > INT_MAX ? INT_MAX : (int)value;
        }
    }
    int result = hl_seqpoll_wait(fds, count, selected, milliseconds);
    if (result < 0) return result;
    if (reads) FD_ZERO(reads); if (writes) FD_ZERO(writes); if (excepts) FD_ZERO(excepts);
    result = 0;
    for (int i = 0; i < count; i++) {
        if (fds[i].revents & POLLNVAL) { errno = EBADF; return -1; }
        if (reads && (fds[i].events & POLLIN) &&
            (fds[i].revents & (POLLIN | POLLHUP | POLLERR))) { FD_SET(fds[i].fd, reads); result++; }
        if (writes && (fds[i].events & POLLOUT) &&
            (fds[i].revents & (POLLOUT | POLLERR))) { FD_SET(fds[i].fd, writes); result++; }
        if (excepts && (fds[i].events & POLLPRI) &&
            (fds[i].revents & POLLPRI)) { FD_SET(fds[i].fd, excepts); result++; }
    }
    return result;
}
typedef struct { hl_fd_ref_t refs[FD_TABLE_SIZE]; } hl_seqpoll_refs;
static void hl_seqpoll_refs_end(hl_seqpoll_refs *refs) {
    for (int i = 0; i < FD_TABLE_SIZE; i++) hl_fd_put(&refs->refs[i]);
}
#endif
