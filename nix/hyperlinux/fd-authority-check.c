/* test-fd-object-lite.c — open-file API without full hl link
 *
 * Copyright 2025 Moritz Angermann <moritz@zw3rk.com>, zw3rk pte. ltd.
 * SPDX-License-Identifier: Apache-2.0
 *
 * Provides fd_table/fd_alloc/fd_lock stubs so we exercise the real
 * fd_object.c entry points (create/install/get/put/dup/remove).
 */
#include "fd_object.h"
#include "syscall_internal.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <pthread.h>
#include <stdatomic.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
uint32_t hl_trace_mask;
void hl_trace(uint32_t cat, const char *fmt, ...) { (void)cat; (void)fmt; }
void hl_wc_socket_drop(hl_open_file_t *of) { (void)of; }

fd_entry_t fd_table[FD_TABLE_SIZE];
pthread_mutex_t fd_lock = PTHREAD_MUTEX_INITIALIZER;
int hl_verbose = 0;

static uint64_t free_bits[(FD_TABLE_SIZE + 63) / 64];

static void bitmap_init(void) {
    memset(free_bits, 0xFF, sizeof(free_bits));
}
static void set_used(int fd) {
    free_bits[fd / 64] &= ~(1ULL << (fd % 64));
}
static void set_free(int fd) {
    free_bits[fd / 64] |= (1ULL << (fd % 64));
}
static int find_free(void) {
    for (int w = 0; w < (int)(sizeof(free_bits) / sizeof(free_bits[0])); w++) {
        if (free_bits[w]) {
            int b = __builtin_ctzll(free_bits[w]);
            int fd = w * 64 + b;
            if (fd < FD_TABLE_SIZE) return fd;
        }
    }
    return -1;
}

int fd_alloc(int type, int host_fd) {
    pthread_mutex_lock(&fd_lock);
    int fd = find_free();
    if (fd >= 0) {
        set_used(fd);
        fd_table[fd].type = type;
        fd_table[fd].host_fd = host_fd;
        fd_table[fd].linux_flags = 0;
        fd_table[fd].dir = NULL;
        fd_table[fd].of = NULL;
        fd_table[fd].desc = NULL;
    }
    pthread_mutex_unlock(&fd_lock);
    return fd;
}
int fd_alloc_from(int minfd, int type, int host_fd) {
    (void)minfd;
    return fd_alloc(type, host_fd);
}
int fd_alloc_at(int fd, int type, int host_fd) {
    if (fd < 0 || fd >= FD_TABLE_SIZE) return -1;
    pthread_mutex_lock(&fd_lock);
    set_used(fd);
    fd_table[fd].type = type;
    fd_table[fd].host_fd = host_fd;
    fd_table[fd].linux_flags = 0;
    fd_table[fd].dir = NULL;
    fd_table[fd].of = NULL;
    fd_table[fd].desc = NULL;
    pthread_mutex_unlock(&fd_lock);
    return fd;
}
int fd_alloc_descriptor(hl_descriptor_t *d) {
    pthread_mutex_lock(&fd_lock);
    int fd = find_free();
    if (fd >= 0) { set_used(fd); fd_table[fd].desc = d; hl_fd_sync_legacy(fd); }
    pthread_mutex_unlock(&fd_lock); return fd;
}
int fd_alloc_descriptor_from(hl_descriptor_t *d, int minimum) {
    (void)minimum; return fd_alloc_descriptor(d);
}
int fd_alloc_descriptor_at(int fd, hl_descriptor_t *d, int replace) {
    hl_fd_detached_t old;
    if (fd_table[fd].type != FD_CLOSED) {
        if (!replace) return -1;
        if (hl_fd_remove(fd, &old) == 0) hl_fd_detached_finish(&old);
    }
    pthread_mutex_lock(&fd_lock); set_used(fd);
    fd_table[fd].desc = d; hl_fd_sync_legacy(fd);
    pthread_mutex_unlock(&fd_lock); return fd;
}
int fd_to_host(int guest_fd) {
    if (guest_fd < 0 || guest_fd >= FD_TABLE_SIZE) return -1;
    if (fd_table[guest_fd].type == FD_CLOSED) return -1;
    return fd_table[guest_fd].host_fd;
}
void fd_mark_closed_unlocked(int fd) {
    fd_table[fd].type = FD_CLOSED;
    fd_table[fd].host_fd = -1;
    fd_table[fd].dir = NULL;
    fd_table[fd].linux_flags = 0;
    fd_table[fd].of = NULL;
    fd_table[fd].desc = NULL;
    set_free(fd);
}
void fd_mark_closed(int fd) {
    pthread_mutex_lock(&fd_lock);
    fd_mark_closed_unlocked(fd);
    pthread_mutex_unlock(&fd_lock);
}
int fd_snapshot_and_close(int fd, fd_entry_t *out) {
    if (fd < 0 || fd >= FD_TABLE_SIZE) return 0;
    pthread_mutex_lock(&fd_lock);
    if (fd_table[fd].type == FD_CLOSED) {
        pthread_mutex_unlock(&fd_lock);
        return 0;
    }
    *out = fd_table[fd];
    fd_mark_closed_unlocked(fd);
    pthread_mutex_unlock(&fd_lock);
    return 1;
}
int64_t linux_errno(void) { return -1; }

/* The eventfd/signalfd/timerfd/inotify subsystems are not linked into this
 * lite harness; hl_fd_dup3() calls this to retire their fd-keyed state. */
/* hl_vdir_release/retain live in syscall_fs.c (not linked here). */
void hl_vdir_release(void *p) { (void)p; }
void hl_vdir_retain(void *p) { (void)p; }

void fd_special_subsystem_close(int guest_fd, int type) {
    (void)guest_fd; (void)type;
}

void epoll_instance_destroy(void *instance) { free(instance); }

static int fails = 0;
#define CHECK(c, m) do { if (!(c)) { printf("FAIL %s\n", m); fails++; } \
    else printf("OK %s\n", m); } while (0)

static unsigned destroyed;
static void destroy_cb(hl_open_file_t *of) {
    destroyed++;
    free(of->state);
    of->state = NULL;
}
static const hl_fd_ops_t ops = { .destroy = destroy_cb };

int main(void) {
    bitmap_init();
    memset(fd_table, 0, sizeof(fd_table));

    /* Native descriptor numbers confer no guest authority, even while an
     * unlinked private backing file occupies that native number. */
    char private_path[] = "/tmp/hl-fd-authority.XXXXXX";
    int private_fd = mkstemp(private_path);
    CHECK(private_fd >= 0, "private backing created");
    CHECK(unlink(private_path) == 0, "private backing unlinked");
    CHECK(write(private_fd, "secret", 6) == 6, "private backing initialized");
    hl_fd_ref_t invalid = {0};
    CHECK(hl_fd_get_pinned(-1, &invalid) < 0, "negative guest descriptor rejected");
    CHECK(hl_fd_get_pinned(FD_TABLE_SIZE, &invalid) < 0, "out of range guest descriptor rejected");
    CHECK(private_fd < FD_TABLE_SIZE && fd_table[private_fd].type == FD_CLOSED &&
          hl_fd_get_pinned(private_fd, &invalid) < 0,
          "guessed live native backing number rejected as closed guest slot");
    char private_bytes[6];
    CHECK(pread(private_fd, private_bytes, sizeof(private_bytes), 0) == 6 &&
          memcmp(private_bytes, "secret", 6) == 0,
          "invalid guest lookup leaves backing native file open and unchanged");
    close(private_fd);
    int *state = malloc(sizeof(int));
    *state = 7;
    hl_open_file_t *of = hl_open_file_create(FD_REGULAR, &ops, 0, state);
    CHECK(of && atomic_load(&of->refcount) == 1, "create");

    int p[2];
    CHECK(pipe(p) == 0, "pipe");
    int a = hl_fd_install(of, p[0], 0);
    CHECK(a >= 0, "install");
    CHECK(fd_table[a].of == of && fd_table[a].desc, "table wired");

    hl_fd_ref_t ref;
    CHECK(hl_fd_get(a, &ref) == 0 && ref.of == of, "get");
    hl_fd_put(&ref);

    int b = hl_fd_dup(a);
    CHECK(b >= 0 && b != a && fd_table[b].of == of, "dup shares of");
    CHECK(atomic_load(&of->refcount) >= 2, "refcount>=2");

    hl_fd_detached_t d;
    CHECK(hl_fd_remove(a, &d) == 0, "remove a");
    hl_fd_detached_finish(&d);
    CHECK(fd_table[a].type == FD_CLOSED, "a closed");
    CHECK(fd_table[b].of == of, "b still live");

    CHECK(hl_fd_remove(b, &d) == 0, "remove b");
    hl_fd_detached_finish(&d);
    CHECK(fd_table[b].type == FD_CLOSED, "b closed");

    /* Retained identities must survive slot reuse without acquiring authority
     * over the replacement descriptor. */
    int legacy_native = dup(p[1]);
    int slot = fd_alloc(FD_REGULAR, legacy_native);
    fd_table[slot].linux_flags = LINUX_O_CLOEXEC;
    hl_fd_ref_t expected = {0};
    CHECK(hl_fd_get_replaceable(slot, &expected) == 0 && expected.desc && expected.of,
          "legacy promotion creates stable identity and open file");
    int installed_native = dup(p[1]);
    hl_open_file_t *installed_of = hl_open_file_create(FD_REGULAR, &ops, 0, NULL);
    CHECK(hl_fd_install_if_current(&expected, installed_of, installed_native, 0) == slot,
          "expected identity replacement succeeds");
    CHECK(fd_table[slot].of == installed_of && atomic_load(&expected.desc->removed),
          "old identity removed and replacement published");
    hl_descriptor_t *occupant = fd_table[slot].desc;
    int rejected_native = dup(p[1]);
    int *rejected_state = malloc(sizeof(int));
    hl_open_file_t *rejected_of = hl_open_file_create(FD_REGULAR, &ops, 0, rejected_state);
    unsigned before_destroy = destroyed;
    CHECK(hl_fd_install_if_current(&expected, rejected_of, rejected_native, 0) < 0 && errno == EAGAIN,
          "stale replacement rejected");
    CHECK(destroyed == before_destroy + 1, "rejected replacement open file destroyed");
    CHECK(fcntl(rejected_native, F_GETFD) < 0 && errno == EBADF,
          "rejected replacement native resource released");
    CHECK(fd_table[slot].desc == occupant, "stale replacement preserves current occupant");
    CHECK(hl_fd_remove_exact(slot, expected.desc, &d) < 0 && fd_table[slot].desc == occupant,
          "stale failure cleanup preserves current occupant");
    hl_fd_put(&expected);
    CHECK(fcntl(legacy_native, F_GETFD) < 0 && errno == EBADF,
          "final old identity reference closes original native alias");
    CHECK(hl_fd_remove_exact(slot, occupant, &d) == 0, "current occupant exact cleanup");
    hl_fd_detached_finish(&d);
    CHECK(fcntl(installed_native, F_GETFD) < 0 && errno == EBADF,
          "final replacement cleanup closes native alias");
    close(p[1]);
    printf("%s\n", fails ? "FAIL" : "PASS");
    return fails ? 1 : 0;
}
