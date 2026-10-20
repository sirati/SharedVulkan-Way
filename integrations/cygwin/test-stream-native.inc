/* Native stream mock for testing only the local POSIX fd adapter. Canonical
 * BAR ring framing, credits and allocation generations have separate Rust tests. */
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <time.h>
#include <sys/wait.h>
struct mock_stream { unsigned char bytes[64]; size_t head, count; unsigned readers, writers, pending[2]; uint32_t epoch; };
struct mock_endpoint { struct mock_stream *stream; int writer, live; };
static struct mock_stream mock_streams[8];
static struct mock_endpoint mock_endpoints[256];
static uint64_t next_mock_endpoint = 100;
static pthread_mutex_t mock_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t mock_condition = PTHREAD_COND_INITIALIZER;
static uint64_t mock_endpoint(struct mock_stream *stream, int writer)
{
    uint64_t id = next_mock_endpoint++; assert(id < 256);
    mock_endpoints[id] = (struct mock_endpoint){stream, writer, 1}; return id;
}
static void mock_wake(struct mock_stream *stream) { ++stream->epoch; pthread_cond_broadcast(&mock_condition); }
static int mock_state(struct mock_endpoint *endpoint)
{
    struct mock_stream *stream = endpoint->stream;
    int closed = endpoint->writer ? !stream->readers && !stream->pending[0] : !stream->writers && !stream->pending[1];
    int ready = endpoint->writer ? stream->count < sizeof(stream->bytes) : stream->count > 0;
    return ready | (closed << 1);
}
int wc_bridge_stream_pair(uint64_t *reader, uint64_t *writer)
{
    pthread_mutex_lock(&mock_lock);
    struct mock_stream *stream = NULL;
    for (unsigned index = 0; index < 8; ++index)
        if (!mock_streams[index].readers && !mock_streams[index].writers && !mock_streams[index].pending[0] && !mock_streams[index].pending[1]) { stream = &mock_streams[index]; break; }
    assert(stream); memset(stream, 0, sizeof(*stream)); stream->readers = stream->writers = 1;
    *reader = mock_endpoint(stream, 0); *writer = mock_endpoint(stream, 1);
    pthread_mutex_unlock(&mock_lock); return 0;
}
static int mock_words(uint64_t id, uint32_t *words)
{
    pthread_mutex_lock(&mock_lock);
    struct mock_endpoint *endpoint = &mock_endpoints[id]; assert(endpoint->live);
    struct mock_stream *stream = endpoint->stream; ++stream->pending[endpoint->writer];
    uint32_t value[7] = {24, 7, 42, (uint32_t)(stream - mock_streams), (uint32_t)endpoint->writer, 1, 0};
    memcpy(words, value, sizeof(value)); pthread_mutex_unlock(&mock_lock); return 0;
}
int wc_bridge_stream_import(const uint32_t *words, int *writer, uint64_t *id)
{
    if (words[0] != 24 || words[1] != 7 || words[2] != 42 || words[3] >= 8 || words[4] > 1) return -1;
    pthread_mutex_lock(&mock_lock);
    struct mock_stream *stream = &mock_streams[words[3]]; *writer = (int)words[4];
    assert(stream->pending[*writer]); --stream->pending[*writer];
    if (*writer) ++stream->writers; else ++stream->readers;
    *id = mock_endpoint(stream, *writer); mock_wake(stream);
    pthread_mutex_unlock(&mock_lock); return 0;
}
static int mock_drop(uint64_t id)
{
    pthread_mutex_lock(&mock_lock);
    struct mock_endpoint *endpoint = &mock_endpoints[id]; assert(endpoint->live); endpoint->live = 0;
    if (endpoint->writer) --endpoint->stream->writers; else --endpoint->stream->readers;
    mock_wake(endpoint->stream); pthread_mutex_unlock(&mock_lock); return 0;
}
intptr_t wc_bridge_stream_read(uint64_t id, uint8_t *bytes, size_t length)
{
    pthread_mutex_lock(&mock_lock);
    struct mock_endpoint *endpoint = &mock_endpoints[id]; assert(endpoint->live && !endpoint->writer);
    struct mock_stream *stream = endpoint->stream;
    size_t count = length < stream->count ? length : stream->count;
    for (size_t index = 0; index < count; ++index) bytes[index] = stream->bytes[(stream->head + index) % sizeof(stream->bytes)];
    stream->head = (stream->head + count) % sizeof(stream->bytes); stream->count -= count;
    intptr_t result = count ? (intptr_t)count : (mock_state(endpoint) & 2 ? 0 : -2);
    if (count) mock_wake(stream);
    pthread_mutex_unlock(&mock_lock); return result;
}
intptr_t wc_bridge_stream_write(uint64_t id, const uint8_t *bytes, size_t length)
{
    pthread_mutex_lock(&mock_lock);
    struct mock_endpoint *endpoint = &mock_endpoints[id]; assert(endpoint->live && endpoint->writer);
    if (mock_state(endpoint) & 2) { pthread_mutex_unlock(&mock_lock); return -3; }
    struct mock_stream *stream = endpoint->stream;
    size_t count = sizeof(stream->bytes) - stream->count; if (count > length) count = length;
    for (size_t index = 0; index < count; ++index) stream->bytes[(stream->head + stream->count + index) % sizeof(stream->bytes)] = bytes[index];
    stream->count += count; if (count) mock_wake(stream);
    pthread_mutex_unlock(&mock_lock); return count ? (intptr_t)count : -2;
}
int wc_bridge_stream_state(uint64_t id)
{
    pthread_mutex_lock(&mock_lock); assert(mock_endpoints[id].live);
    int result = mock_state(&mock_endpoints[id]); pthread_mutex_unlock(&mock_lock); return result;
}
int wc_bridge_stream_epoch(uint64_t id, uint32_t *epoch)
{
    pthread_mutex_lock(&mock_lock); assert(mock_endpoints[id].live);
    *epoch = mock_endpoints[id].stream->epoch; pthread_mutex_unlock(&mock_lock); return 0;
}
int wc_bridge_stream_kick(uint64_t id)
{
    pthread_mutex_lock(&mock_lock); assert(mock_endpoints[id].live);
    mock_wake(mock_endpoints[id].stream); pthread_mutex_unlock(&mock_lock); return 0;
}
int wc_bridge_stream_watch(uint64_t id, uint32_t epoch, int wanted, uint32_t milliseconds)
{
    (void)milliseconds;
    pthread_mutex_lock(&mock_lock); struct mock_endpoint *endpoint = &mock_endpoints[id]; assert(endpoint->live);
    while (epoch == endpoint->stream->epoch && !(mock_state(endpoint) & 2) && !(wanted && (mock_state(endpoint) & 1)))
        pthread_cond_wait(&mock_condition, &mock_lock);
    int result = mock_state(endpoint); pthread_mutex_unlock(&mock_lock); return result;
}
static void *mock_produce(void *opaque)
{
    int fd = *(int *)opaque;
    assert(!fcntl(fd, F_SETFL, 0));
    FILE *file = fdopen(fd, "w"); assert(file);
    unsigned char bytes[4093]; size_t offset = 0;
    while (offset < 256 * 1024) {
        size_t length = sizeof(bytes); if (length > 256 * 1024 - offset) length = 256 * 1024 - offset;
        for (size_t index = 0; index < length; ++index) bytes[index] = (unsigned char)((offset + index) % 251);
        assert(fwrite(bytes, 1, length, file) == length);
        offset += length;
    }
    assert(!fclose(file)); return NULL;
}
static void test_streams(void)
{
    alarm(20);
    int fds[2]; assert(!pipe2(fds, O_CLOEXEC | O_NONBLOCK));
    uint32_t words[7]; assert(!wc_cygwin_fd_words(fds[1], words));
    assert(!close(fds[1]));
    struct pollfd readable = {fds[0], POLLIN, 0}; assert(poll(&readable, 1, 0) == 0);
    int imported = wc_cygwin_fd_from_words(words, 7); assert(imported >= 0);
    int duplicate = fcntl(imported, F_DUPFD_CLOEXEC, 0); assert(duplicate >= 0 && !close(imported));
    int flags = fcntl(duplicate, F_GETFL); assert(flags >= 0 && !fcntl(duplicate, F_SETFL, flags | O_NONBLOCK));
    pthread_t producer; assert(!pthread_create(&producer, NULL, mock_produce, &duplicate));
    size_t offset = 0; unsigned char bytes[997];
    for (;;) {
        ssize_t count = read(fds[0], bytes, sizeof(bytes));
        if (count > 0) {
            for (ssize_t index = 0; index < count; ++index) assert(bytes[index] == (unsigned char)((offset + (size_t)index) % 251));
            offset += (size_t)count; continue;
        }
        if (!count) break;
        assert(errno == EAGAIN || errno == EWOULDBLOCK);
        assert(poll(&readable, 1, 2000) == 1);
    }
    assert(offset == 256 * 1024 && !pthread_join(producer, NULL));
    assert(!close(fds[0]));
    /* wl-paste inherits only a real local pipe into cat. The parent still
     * owns and pumps its BAR consumer until actual local kernel completion. */
    assert(!pipe2(fds, O_CLOEXEC));
    assert(!wc_cygwin_fd_words(fds[1], words));
    assert(!close(fds[1]));
    imported = wc_cygwin_fd_from_words(words, 7); assert(imported >= 0);
    char *name; assert(asprintf(&name, "%s/stream-fork-XXXXXX", getenv("XDG_RUNTIME_DIR")) > 0);
    int output = mkstemp(name); assert(output >= 0); unlink(name); free(name);
    pid_t child = fork(); assert(child >= 0);
    if (!child) {
        assert(!close(imported));
        assert(dup2(fds[0], STDIN_FILENO) == STDIN_FILENO && !close(fds[0]));
        assert(dup2(output, STDOUT_FILENO) == STDOUT_FILENO && !close(output));
        execlp("cat", "cat", NULL); _exit(91);
    }
    assert(!pthread_create(&producer, NULL, mock_produce, &imported));
    assert(!close(fds[0]));
    assert(!pthread_join(producer, NULL));
    int status; assert(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    assert(lseek(output, 0, SEEK_END) == 256 * 1024 && lseek(output, 0, SEEK_SET) == 0);
    offset = 0;
    for (;;) {
        ssize_t count = read(output, bytes, sizeof(bytes)); assert(count >= 0);
        if (!count) break;
        for (ssize_t index = 0; index < count; ++index) assert(bytes[index] == (unsigned char)((offset + (size_t)index) % 251));
        offset += (size_t)count;
    }
    assert(offset == 256 * 1024 && !close(output));
    assert(!pipe2(fds, O_CLOEXEC | O_NONBLOCK));
    assert(!close(fds[0]));
    signal(SIGPIPE, SIG_IGN);
    struct pollfd writable = {fds[1], POLLOUT, 0};
    for (unsigned attempt = 0; attempt < 100; ++attempt) {
        assert(poll(&writable, 1, 2000) == 1);
        if (writable.revents & POLLERR) break;
        struct timespec pause = {0, 1000000}; nanosleep(&pause, NULL);
    }
    assert(writable.revents & POLLERR);
    assert(write(fds[1], "x", 1) < 0 && errno == EPIPE);
    assert(!close(fds[1]));
    /* Failed dup2 must leave the old tracked target live. Successful
     * replacement must close that endpoint and retain only the new alias. */
    int other[2];
    assert(!pipe2(fds, O_CLOEXEC) && !pipe2(other, O_CLOEXEC));
    assert(dup2(-1, other[0]) < 0 && errno == EBADF);
    assert(write(other[1], "a", 1) == 1 && read(other[0], bytes, 1) == 1 && bytes[0] == 'a');
    assert(dup2(fds[0], fds[0]) == fds[0]);
    assert(dup2(fds[0], other[0]) == other[0]);
    assert(!close(fds[0]));
    assert(write(fds[1], "b", 1) == 1 && read(other[0], bytes, 1) == 1 && bytes[0] == 'b');
    assert(!close(other[0]) && !close(fds[1]) && !close(other[1]));
    for (unsigned id = 100; id < next_mock_endpoint; ++id) assert(!mock_endpoints[id].live);
    alarm(0);
    fprintf(stderr, "WC_CYGWIN_STREAM_MOCK bytes=262144 pending_blocks_eof=1 poll=1 duplicate=1 stdio_close=1 fork_exec_local_pipe=1 eof=1 epipe=1 all_workers_joined=1\n");
}
