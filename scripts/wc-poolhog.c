/* wc-poolhog — a Wayland client that takes arena memory and then does not give
 * it back politely.
 *
 * WHY THIS EXISTS, AND WHY IT IS NOT `wc-pooltest`
 *
 * `wc-pooltest` exists to *end* pools: it creates and destroys them so that the
 * shim's `wl_shm_pool.destroy` path gets run. This is the opposite client, and
 * it is the one the release question needs.
 *
 * docs/waycompose-design.md §72 is about the pools a client is still holding
 * when it **exits**. `Pool::releasable` is `zombie && acked && buffers == 0`,
 * and `zombie` is set in exactly one place: the client sending
 * `wl_shm_pool.destroy`. A client that is killed never sends it, so before
 * `Drop for Session` those bytes stayed `published` on an arena that outlives
 * the connection. Reproducing that needs a client that
 *
 *   1. creates pools big enough to matter,
 *   2. *keeps* them — no destroy, ever, and
 *   3. can be killed rather than asked to leave.
 *
 * So: create N pools of M bytes, say so on stdout, and then sit in
 * `wl_display_dispatch` forever waiting to be shot. `SIGKILL` is the point —
 * a client that unwinds might tidy up, and tidying up is the path that already
 * worked.
 *
 * With `N = 0` it is the **keepalive**: a connection that holds no arena bytes
 * and never draws. That is not a convenience, it is load-bearing. The reclaim
 * clock turns on a *session's* poll timeout, so with zero sessions nothing
 * ticks at all — see §72.5. The keepalive is the session whose idle poll turns
 * the clock for the arena the killed client's pools were in.
 *
 *   gcc -O2 -o wc-poolhog scripts/wc-poolhog.c -lwayland-client
 *   wc-poolhog <pools> <bytes-per-pool> [tag]
 *
 * No surface, no seat, no shell: `wl_shm.create_pool` is the whole experiment,
 * and the proxy publishes a pool into the arena when the client creates it
 * (`Pool::wanted` is true at `create_pool`), so nothing has to be drawn for the
 * bytes to be taken.
 */

#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#include <wayland-client.h>

static struct wl_shm *shm;

static void reg_global(void *data, struct wl_registry *r, uint32_t name,
                       const char *iface, uint32_t ver) {
    (void)data;
    (void)ver;
    if (!strcmp(iface, "wl_shm"))
        shm = wl_registry_bind(r, name, &wl_shm_interface, 1);
}
static void reg_remove(void *d, struct wl_registry *r, uint32_t n) {
    (void)d;
    (void)r;
    (void)n;
}
static const struct wl_registry_listener reg_listener = {reg_global, reg_remove};

/* An anonymous, sealed-off scratch file. `memfd_create` rather than a file in
 * the runtime directory because the pool's memory must not be reachable by
 * name: the whole point of the arena is that what crosses is three integers
 * and not a path. */
static int scratch(size_t size) {
    int fd = memfd_create("wc-poolhog", MFD_CLOEXEC);
    if (fd < 0)
        return -1;
    if (ftruncate(fd, (off_t)size) < 0) {
        close(fd);
        return -1;
    }
    return fd;
}

int main(int argc, char **argv) {
    int pools = argc > 1 ? atoi(argv[1]) : 1;
    size_t size = argc > 2 ? (size_t)strtoull(argv[2], NULL, 10) : (size_t)(4 << 20);
    const char *tag = argc > 3 ? argv[3] : "hog";

    struct wl_display *dpy = wl_display_connect(NULL);
    if (!dpy) {
        fprintf(stderr, "wc-poolhog[%s]: no display\n", tag);
        return 1;
    }
    struct wl_registry *reg = wl_display_get_registry(dpy);
    wl_registry_add_listener(reg, &reg_listener, NULL);
    wl_display_roundtrip(dpy);
    if (!shm && pools > 0) {
        fprintf(stderr, "wc-poolhog[%s]: no wl_shm\n", tag);
        return 1;
    }

    /* All of them, then one flush, and **no round trip in between**.
     *
     * The round trip is what a normal client would do and it is exactly what
     * this one must not do, for a reason worth writing down because it took a
     * run to find: `wc-proxy` writes its upstream stream in the in-band-`h`
     * dialect (docs/waycompose-design.md §2.3), which only the **Windows**
     * build of the compositor can read -- on Unix the vendored backend's
     * `Argument::Fd` is a raw descriptor and there is nowhere to put a
     * `wc_handle::Descriptor`. So against a *Linux* compositor the rewritten
     * `wl_shm.create_pool` desynchronises the server's reader, and a client
     * that waits for a reply after its first pool waits forever. See §72.3.
     *
     * That stall is upstream of nothing this client is for. The proxy's
     * allocator takes the arena bytes when the request is *parsed*, so all
     * three pools land in guest RAM either way; what is missing is the
     * compositor's Wayland-object view of them, which no part of the memory
     * question depends on. */
    int *fds = calloc(pools > 0 ? (size_t)pools : 1, sizeof(int));
    for (int i = 0; i < pools; i++) {
        fds[i] = scratch(size);
        if (fds[i] < 0) {
            fprintf(stderr, "wc-poolhog[%s]: no memfd for pool %d\n", tag, i);
            return 1;
        }
        /* Touch it, so that these are real pages on this side too and a
         * harness reading the *guest's* numbers is not reading a hole. */
        void *p = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fds[i], 0);
        if (p != MAP_FAILED) {
            memset(p, 0x40 + i, size);
            munmap(p, size);
        }
        struct wl_shm_pool *pool = wl_shm_create_pool(shm, fds[i], (int32_t)size);
        if (!pool) {
            fprintf(stderr, "wc-poolhog[%s]: create_pool %d failed\n", tag, i);
            return 1;
        }
        /* Deliberately never `wl_shm_pool_destroy(pool)`. That is the whole
         * experiment: this client leaks its pools and is then killed. */
    }
    /* One more message, and it is required rather than tidy. `create_pool` is
     * one of the two requests a pool is allowed to be *deferred* through
     * (`Session::client_message`): the proxy sizes the pool and publishes it
     * into the arena at the top of the **next** message, so a client that stops
     * after its last `create_pool` has taken no arena bytes at all. A `sync` is
     * the smallest thing that is not deferrable.
     *
     * Sent and not waited for. Waiting is what a normal client does and what
     * this one cannot do; see the note above. */
    wl_display_sync(dpy);
    /* Flush first, then close. libwayland queues the `h` argument as a bare
     * descriptor number and sends it at flush, so closing between the request
     * and the flush hands the far side a closed descriptor. */
    wl_display_flush(dpy);
    for (int i = 0; i < pools; i++)
        close(fds[i]);

    printf("wc-poolhog[%s]: %d pool(s) of %zu bytes created and held; pid %d\n", tag, pools,
           size, (int)getpid());
    fflush(stdout);

    /* Wait to be killed. `dispatch` rather than `pause` so the connection is
     * live -- a client that stops reading its socket is a client the proxy is
     * blocked writing to, which is a different experiment. */
    while (wl_display_dispatch(dpy) != -1) {
    }
    printf("wc-poolhog[%s]: the connection ended\n", tag);
    return 0;
}
