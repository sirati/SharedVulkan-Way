/* wc-pooltest — create and destroy `wl_shm` pools in a loop, so that the part
 * of `wc-poolshim` nothing else reaches gets run.
 *
 * WHY THIS EXISTS. §30.11 requires the shim's pool record to end at
 * `wl_shm_pool.destroy` rather than leak, because `(st_dev, st_ino)` is a
 * better identity than a descriptor number but still not eternal: a released
 * memfd's inode can be reissued and a record that outlived its object would
 * match a stranger. The ending was written and then found to be unexercised —
 * Firefox holds its rendering pools for the life of the window, so a 70-second
 * session destroys nothing and reports `destroyed=0`. Code that compiles and
 * binds but never runs is precisely what §30 keeps catching, so here is a
 * client whose entire purpose is to end pools.
 *
 * It draws nothing and needs no surface, seat or shell: `wl_shm.create_pool`
 * and `wl_shm_pool.destroy` are the whole experiment.
 *
 *   gcc -O2 -o wc-pooltest scripts/wc-pooltest.c -lwayland-client
 *   LD_PRELOAD=./wc-poolshim.so WC_POOLSHIM=1 ./wc-pooltest 8
 *
 * Expect `create_pool=N redirected=N destroyed=N` with N pools' worth of
 * "destroyed by the client" lines, and no descriptor left behind.
 */

#define _GNU_SOURCE
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

int main(int argc, char **argv) {
    int rounds = argc > 1 ? atoi(argv[1]) : 8;
    const int size = 640 * 480 * 4;

    struct wl_display *dpy = wl_display_connect(NULL);
    if (!dpy) {
        fprintf(stderr, "wc-pooltest: no display\n");
        return 1;
    }
    struct wl_registry *reg = wl_display_get_registry(dpy);
    wl_registry_add_listener(reg, &reg_listener, NULL);
    wl_display_roundtrip(dpy);
    if (!shm) {
        fprintf(stderr, "wc-pooltest: no wl_shm\n");
        return 1;
    }

    for (int i = 0; i < rounds; i++) {
        int fd = memfd_create("wc-pooltest", 0);
        if (fd < 0 || ftruncate(fd, size) != 0) {
            fprintf(stderr, "wc-pooltest: memfd failed\n");
            return 1;
        }
        /* Map it the way a real client would, so the shim's substitution has
         * something to act on, and touch it so the witness has something to
         * see. */
        void *p = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (p == MAP_FAILED) {
            fprintf(stderr, "wc-pooltest: mmap failed\n");
            return 1;
        }
        memset(p, 0x40 + i, size);

        struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, size);
        struct wl_buffer *buf = wl_shm_pool_create_buffer(pool, 0, 640, 480,
                                                          640 * 4,
                                                          WL_SHM_FORMAT_XRGB8888);
        wl_display_roundtrip(dpy);

        wl_buffer_destroy(buf);
        wl_shm_pool_destroy(pool); /* the line this program exists for */
        wl_display_roundtrip(dpy);

        munmap(p, size);
        close(fd);
        fprintf(stderr, "wc-pooltest: round %d done\n", i);
    }

    wl_display_roundtrip(dpy);
    wl_display_disconnect(dpy);
    fprintf(stderr, "wc-pooltest: %d pools created and destroyed\n", rounds);
    return 0;
}
