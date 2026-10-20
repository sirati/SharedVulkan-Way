/* Included by bar-posix.c. OS pipes provide local POSIX readiness only;
 * clipboard bytes cross the VM solely through the original BAR byte rings. */
#include <poll.h>
#include <signal.h>
/* Cygwin 3.6.11's stdatomic.h names this private wchar type without
 * declaring it. Keep the compatibility typedef inside our companion only. */
#if defined(__CYGWIN__) && defined(__clang__)
typedef __WCHAR_TYPE__ ___wchar_t;
#endif
#include <stdatomic.h>
BRIDGE int wc_bridge_stream_pair(uint64_t *, uint64_t *);
BRIDGE int wc_bridge_stream_import(const uint32_t *, int *, uint64_t *);
BRIDGE intptr_t wc_bridge_stream_read(uint64_t, uint8_t *, size_t);
BRIDGE intptr_t wc_bridge_stream_write(uint64_t, const uint8_t *, size_t);
BRIDGE int wc_bridge_stream_epoch(uint64_t, uint32_t *);
BRIDGE int wc_bridge_stream_watch(uint64_t, uint32_t, int, uint32_t);
BRIDGE int wc_bridge_stream_state(uint64_t);
BRIDGE int wc_bridge_stream_kick(uint64_t);
struct stream_token {
    uint64_t lease;
    int writer, pump, notify[2], running, joining, joined, fork_shared;
    dev_t device;
    ino_t inode;
    _Atomic int stop, want;
    struct fd_ref *fds;
    pthread_t watcher, worker;
    struct stream_token *next;
};
static struct stream_token *streams;
static unsigned streams_created, streams_retired;
static int stream_add_fd(struct stream_token *stream, int fd)
{
    for (struct fd_ref *r = stream->fds; r; r = r->next) if (r->fd == fd) return 0;
    struct fd_ref *r = malloc(sizeof(*r));
    if (!r) return -1;
    *r = (struct fd_ref){fd, stream->fds}; stream->fds = r; return 0;
}
static void stream_remove_fd(struct stream_token *stream, int fd)
{
    struct fd_ref **next = &stream->fds;
    while (*next) {
        if ((*next)->fd == fd) { struct fd_ref *old = *next; *next = old->next; free(old); return; }
        next = &(*next)->next;
    }
}
static struct stream_token *find_stream(int fd)
{
    struct stat st;
    if (fstat(fd, &st)) return NULL;
    for (struct stream_token *s = streams; s; s = s->next) {
        if (s->device != st.st_dev || s->inode != st.st_ino) continue;
        for (struct fd_ref *r = s->fds; r; r = r->next) if (r->fd == fd) return s;
    }
    return NULL;
}
static int runtime_pipe(int fds[2], int flags)
{
    int (*function)(int *, int) = runtime_symbol("pipe2");
    return function ? function(fds, flags) : -1;
}
static void stream_notify(struct stream_token *stream)
{
    int saved = errno;
    unsigned char byte = 1;
    ssize_t result;
    do { result = write(stream->notify[1], &byte, 1); } while (result < 0 && errno == EINTR);
    errno = saved;
}
/* pool_lock protects fd membership and final actual pump completion. */
static void stream_retire(struct stream_token *stream)
{
    if (stream->fds || stream->running || !stream->joined) return;
    if (wc_bridge_lease_drop(stream->lease)) return;
    struct stream_token **next = &streams;
    while (*next && *next != stream) next = &(*next)->next;
    if (*next) *next = stream->next;
    ++streams_retired;
    fprintf(stderr, "WC_CYGWIN_STREAM_LOCAL_RELEASE lease=%llu actual_fds_closed=1 pumps_joined=1 remote_completion_required=1\n", (unsigned long long)stream->lease);
    free(stream);
}
static int stream_fd_closed(struct stream_token *stream, int fd)
{
    stream_remove_fd(stream, fd);
    /* The last real read fd closing cancels its local reader pump. A writer
     * pump must still drain already queued pipe bytes before its own EOF. */
    if (!stream->fds && !stream->writer && stream->running && !stream->fork_shared) {
        atomic_store_explicit(&stream->stop, 1, memory_order_release);
        stream_notify(stream);
        wc_bridge_stream_kick(stream->lease);
    }
    if (!stream->fds && !stream->joining && !stream->joined) {
        stream->joining = 1;
        return 1;
    }
    return 0;
}
/* Called without pool_lock. A last explicit close waits for real local drain
 * and both workers, so process exit cannot silently abandon queued bytes. */
static void stream_join(struct stream_token *stream)
{
    int result = pthread_join(stream->worker, NULL);
    pthread_mutex_lock(&pool_lock);
    if (!result) stream->joined = 1;
    stream_retire(stream);
    pthread_mutex_unlock(&pool_lock);
}
static void *stream_watch(void *opaque)
{
    struct stream_token *stream = opaque;
    for (;;) {
        uint32_t epoch;
        if (wc_bridge_stream_epoch(stream->lease, &epoch)) break;
        /* Capture the epoch before this final local check. A concurrent last
         * fd close kicks the retained native waiter, including before park. */
        if (atomic_load_explicit(&stream->stop, memory_order_acquire)) return NULL;
        int wanted = atomic_load_explicit(&stream->want, memory_order_acquire);
        int state = wc_bridge_stream_watch(stream->lease, epoch, wanted, 3600000);
        if (atomic_load_explicit(&stream->stop, memory_order_acquire)) return NULL;
        if (state < 0) break;
        if (state & 2) { stream_notify(stream); return NULL; }
        if (wanted && (state & 1)) {
            atomic_store_explicit(&stream->want, 0, memory_order_release);
            stream_notify(stream);
        }
    }
    atomic_store_explicit(&stream->stop, 1, memory_order_release);
    stream_notify(stream);
    return NULL;
}
static void stream_want(struct stream_token *stream)
{
    atomic_store_explicit(&stream->want, 1, memory_order_release);
    wc_bridge_stream_kick(stream->lease);
}
static void *stream_pump(void *opaque)
{
    struct stream_token *stream = opaque;
    /* An application still gets ordinary SIGPIPE on its own write. Only this
     * internal pipe writer blocks that signal on this one worker thread. */
    sigset_t signals; sigemptyset(&signals); sigaddset(&signals, SIGPIPE);
    pthread_sigmask(SIG_BLOCK, &signals, NULL);
    uint8_t bytes[16384];
    size_t length = 0, offset = 0;
    int input_eof = 0;
    for (;;) {
        if (atomic_load_explicit(&stream->stop, memory_order_acquire)) break;
        if (stream->writer) {
            int state = wc_bridge_stream_state(stream->lease);
            if (state < 0 || (state & 2)) break;
            if (offset == length && !input_eof) {
                ssize_t count = read(stream->pump, bytes, sizeof(bytes));
                if (count > 0) { length = (size_t)count; offset = 0; }
                else if (!count) input_eof = 1;
                else if (errno == EINTR) continue;
                else if (errno != EAGAIN && errno != EWOULDBLOCK) break;
            }
            if (offset < length) {
                intptr_t count = wc_bridge_stream_write(stream->lease, bytes + offset, length - offset);
                if (count > 0) { offset += (size_t)count; continue; }
                if (count != -2) break;
                stream_want(stream);
            } else if (input_eof) break;
        } else {
            if (offset == length && !input_eof) {
                intptr_t count = wc_bridge_stream_read(stream->lease, bytes, sizeof(bytes));
                if (count > 0) { length = (size_t)count; offset = 0; }
                else if (!count) input_eof = 1;
                else if (count == -2) stream_want(stream);
                else break;
            }
            if (offset < length) {
                ssize_t count = write(stream->pump, bytes + offset, length - offset);
                if (count > 0) { offset += (size_t)count; continue; }
                if (count < 0 && errno == EINTR) continue;
                if (count <= 0 && errno != EAGAIN && errno != EWOULDBLOCK) break;
            } else if (input_eof) break;
        }
        struct pollfd fds[2] = {
            {stream->notify[0], POLLIN, 0},
            {stream->writer ? (offset == length ? stream->pump : -1) : stream->pump, stream->writer ? POLLIN : (offset < length ? POLLOUT : 0), 0}
        };
        int result;
        do { result = poll(fds, 2, -1); } while (result < 0 && errno == EINTR);
        if (result < 0) break;
        if (fds[0].revents) {
            unsigned char discard[64];
            while (read(stream->notify[0], discard, sizeof(discard)) > 0) {}
        }
        if (fds[1].revents & (POLLERR | POLLNVAL)) break;
    }
    atomic_store_explicit(&stream->stop, 1, memory_order_release);
    wc_bridge_stream_kick(stream->lease);
    pthread_join(stream->watcher, NULL);
    real_close(stream->pump);
    pthread_mutex_lock(&pool_lock);
    /* Only after both actual workers have ceased using the native capability
     * may close() or this completion release its final local reference. */
    stream->running = 0;
    real_close(stream->notify[0]); real_close(stream->notify[1]);
    stream_retire(stream);
    pthread_mutex_unlock(&pool_lock);
    return NULL;
}
static int stream_token(uint64_t lease, int writer, int flags)
{
    struct stream_token *stream = calloc(1, sizeof(*stream));
    if (!stream) return -1;
    int fds[2];
    if (runtime_pipe(fds, O_CLOEXEC | O_NONBLOCK)) { free(stream); return -1; }
    if (runtime_pipe(stream->notify, O_CLOEXEC | O_NONBLOCK)) {
        real_close(fds[0]); real_close(fds[1]); free(stream); return -1;
    }
    int application = fds[writer ? 1 : 0];
    stream->pump = fds[writer ? 0 : 1];
    stream->lease = lease; stream->writer = writer; stream->running = 1;
    int (*runtime_fcntl)(int, int, ...) = runtime_symbol("fcntl");
    struct stat st;
    if (!runtime_fcntl || fstat(application, &st) ||
        runtime_fcntl(application, F_SETFL, flags & O_NONBLOCK) ||
        runtime_fcntl(application, F_SETFD, flags & O_CLOEXEC ? FD_CLOEXEC : 0) ||
        stream_add_fd(stream, application)) goto failed;
    stream->device = st.st_dev; stream->inode = st.st_ino;
    pthread_mutex_lock(&pool_lock);
    stream->next = streams; streams = stream; ++streams_created;
    pthread_mutex_unlock(&pool_lock);
    if (pthread_create(&stream->watcher, NULL, stream_watch, stream)) goto unlist;
    if (pthread_create(&stream->worker, NULL, stream_pump, stream)) {
        atomic_store_explicit(&stream->stop, 1, memory_order_release);
        wc_bridge_stream_kick(lease); pthread_join(stream->watcher, NULL); goto unlist;
    }
    fprintf(stderr, "WC_CYGWIN_STREAM_FD lease=%llu writer=%d original_bar=1\n", (unsigned long long)lease, writer);
    return application;
unlist:
    pthread_mutex_lock(&pool_lock);
    struct stream_token **next = &streams;
    while (*next && *next != stream) next = &(*next)->next;
    if (*next) *next = stream->next;
    pthread_mutex_unlock(&pool_lock);
failed:
    while (stream->fds) { struct fd_ref *next = stream->fds->next; free(stream->fds); stream->fds = next; }
    real_close(fds[0]); real_close(fds[1]);
    real_close(stream->notify[0]); real_close(stream->notify[1]); free(stream); return -1;
}
int pipe2(int fds[2], int flags)
{
    if (!wc_cygwin_ring_enabled()) return runtime_pipe(fds, flags);
    if (!fds || flags & ~(O_CLOEXEC | O_NONBLOCK)) { errno = EINVAL; return -1; }
    uint64_t reader, writer;
    if (wc_bridge_stream_pair(&reader, &writer)) { errno = ENOSPC; return -1; }
    int read_fd = stream_token(reader, 0, flags);
    if (read_fd < 0) { wc_bridge_lease_drop(reader); wc_bridge_lease_drop(writer); return -1; }
    int write_fd = stream_token(writer, 1, flags);
    if (write_fd < 0) { close(read_fd); wc_bridge_lease_drop(writer); return -1; }
    fds[0] = read_fd; fds[1] = write_fd; return 0;
}
int pipe(int fds[2]) { return pipe2(fds, 0); }
