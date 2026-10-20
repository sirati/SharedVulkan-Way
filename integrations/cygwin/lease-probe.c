/* SPDX-License-Identifier: MIT
 * Real Cygwin acceptance for incoming keymaps and shared allocation lifetimes.
 * This fixture is separate from the unchanged upstream simple-shm application.
 */
#define _GNU_SOURCE
#include <assert.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#include "wayland-client.h"
#include "xdg-shell-client-protocol.h"
int os_create_anonymous_file(off_t);
static struct wl_display *display;
static struct wl_compositor *compositor;
static struct wl_shm *shm;
static struct xdg_wm_base *wm;
static struct wl_seat *seat;
static struct wl_keyboard *keyboard;
static unsigned keymap_size, configured, released, framed;

static void keymap(void *data, struct wl_keyboard *object, uint32_t format, int fd, uint32_t size)
{
    (void)data; (void)object;
    assert(format == WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1 && size > 32);
    const char *bytes = mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0);
    assert(bytes != MAP_FAILED && bytes[size - 1] == 0 && strstr(bytes, "xkb_keymap"));
    keymap_size = size;
    assert(!close(fd));
    assert(!munmap((void *)bytes, size));
}
static void enter(void *d, struct wl_keyboard *k, uint32_t s, struct wl_surface *w, struct wl_array *a)
{ (void)d; (void)k; (void)s; (void)w; (void)a; }
static void leave(void *d, struct wl_keyboard *k, uint32_t s, struct wl_surface *w)
{ (void)d; (void)k; (void)s; (void)w; }
static void key(void *d, struct wl_keyboard *k, uint32_t s, uint32_t t, uint32_t c, uint32_t state)
{ (void)d; (void)k; (void)s; (void)t; (void)c; (void)state; }
static void modifiers(void *d, struct wl_keyboard *k, uint32_t s, uint32_t a, uint32_t b, uint32_t c, uint32_t e)
{ (void)d; (void)k; (void)s; (void)a; (void)b; (void)c; (void)e; }
static void repeat(void *d, struct wl_keyboard *k, int32_t rate, int32_t delay)
{ (void)d; (void)k; (void)rate; (void)delay; }
static const struct wl_keyboard_listener keyboard_listener = { keymap, enter, leave, key, modifiers, repeat };
static void capabilities(void *d, struct wl_seat *s, uint32_t caps)
{
    (void)d;
    if ((caps & WL_SEAT_CAPABILITY_KEYBOARD) && !keyboard) {
        keyboard = wl_seat_get_keyboard(s);
        assert(!wl_keyboard_add_listener(keyboard, &keyboard_listener, NULL));
    }
}
static void seat_name(void *d, struct wl_seat *s, const char *name) { (void)d; (void)s; (void)name; }
static const struct wl_seat_listener seat_listener = { capabilities, seat_name };
static void ping(void *d, struct xdg_wm_base *object, uint32_t serial) { (void)d; xdg_wm_base_pong(object, serial); }
static const struct xdg_wm_base_listener wm_listener = { ping };
static void global(void *d, struct wl_registry *registry, uint32_t name, const char *interface, uint32_t version)
{
    (void)d;
    if (!strcmp(interface, "wl_compositor")) compositor = wl_registry_bind(registry, name, &wl_compositor_interface, version < 4 ? version : 4);
    else if (!strcmp(interface, "wl_shm")) shm = wl_registry_bind(registry, name, &wl_shm_interface, 1);
    else if (!strcmp(interface, "xdg_wm_base")) { wm = wl_registry_bind(registry, name, &xdg_wm_base_interface, 1); xdg_wm_base_add_listener(wm, &wm_listener, NULL); }
    else if (!strcmp(interface, "wl_seat")) { seat = wl_registry_bind(registry, name, &wl_seat_interface, version < 5 ? version : 5); wl_seat_add_listener(seat, &seat_listener, NULL); }
}
static void global_remove(void *d, struct wl_registry *r, uint32_t n) { (void)d; (void)r; (void)n; }
static const struct wl_registry_listener registry_listener = { global, global_remove };
static void configure(void *d, struct xdg_surface *surface, uint32_t serial)
{ (void)d; xdg_surface_ack_configure(surface, serial); configured = 1; }
static const struct xdg_surface_listener surface_listener = { configure };
static void top_configure(void *d, struct xdg_toplevel *t, int32_t w, int32_t h, struct wl_array *s)
{ (void)d; (void)t; (void)w; (void)h; (void)s; }
static void top_close(void *d, struct xdg_toplevel *t) { (void)d; (void)t; }
static const struct xdg_toplevel_listener toplevel_listener = { .configure = top_configure, .close = top_close };
static void release(void *d, struct wl_buffer *b) { (void)d; (void)b; released = 1; }
static const struct wl_buffer_listener buffer_listener = { release };
static void frame(void *d, struct wl_callback *callback, uint32_t time)
{ (void)d; (void)time; wl_callback_destroy(callback); framed = 1; }
static const struct wl_callback_listener frame_listener = { frame };
static void roundtrip(void) { assert(wl_display_roundtrip(display) >= 0); }
int main(void)
{
    display = wl_display_connect(NULL); assert(display);
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, NULL);
    roundtrip(); roundtrip(); roundtrip();
    assert(compositor && shm && wm && keyboard && keymap_size);
    struct wl_surface *surface = wl_compositor_create_surface(compositor);
    struct xdg_surface *xdg = xdg_wm_base_get_xdg_surface(wm, surface);
    xdg_surface_add_listener(xdg, &surface_listener, NULL);
    struct xdg_toplevel *top = xdg_surface_get_toplevel(xdg);
    xdg_toplevel_add_listener(top, &toplevel_listener, NULL);
    xdg_toplevel_set_title(top, "Cygwin direct BAR lease acceptance");
    wl_surface_commit(surface);
    while (!configured) assert(wl_display_dispatch(display) >= 0);
    int fd = os_create_anonymous_file(4096); assert(fd >= 0);
    uint8_t *small = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0); assert(small != MAP_FAILED);
    small[7] = 0xb3;
    struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, 4096);
    assert(!ftruncate(fd, 16384));
    uint8_t *grown = mmap(NULL, 16384, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    assert(grown == small && grown[7] == 0xb3);
    wl_shm_pool_resize(pool, 16384);
    struct wl_buffer *buffer = wl_shm_pool_create_buffer(pool, 8192, 32, 32, 128, WL_SHM_FORMAT_XRGB8888);
    wl_buffer_add_listener(buffer, &buffer_listener, NULL);
    for (unsigned index = 0; index < 12; ++index) {
        uint32_t *pixels = (uint32_t *)(grown + 8192);
        for (unsigned pixel = 0; pixel < 1024; ++pixel) pixels[pixel] = 0xff000000u | (index * 17u << 16) | ((pixel + index * 11u) & 255u);
        released = framed = 0;
        wl_surface_attach(surface, buffer, 0, 0);
        wl_surface_damage(surface, 0, 0, 32, 32);
        struct wl_callback *callback = wl_surface_frame(surface);
        wl_callback_add_listener(callback, &frame_listener, NULL);
        wl_surface_commit(surface);
        while (!released || !framed) assert(wl_display_dispatch(display) >= 0);
    }
    wl_surface_attach(surface, NULL, 0, 0); wl_surface_commit(surface);
    wl_buffer_destroy(buffer); wl_shm_pool_destroy(pool); roundtrip();
    assert(!munmap(small, 4096) && !munmap(grown, 16384) && !close(fd));
    for (unsigned index = 0; index < 128; ++index) {
        int first = os_create_anonymous_file(4096); assert(first >= 0);
        void *old = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, first, 0); assert(old != MAP_FAILED);
        struct wl_shm_pool *held_pool = wl_shm_create_pool(shm, first, 4096);
        struct wl_buffer *held = wl_shm_pool_create_buffer(held_pool, 0, 32, 32, 128, WL_SHM_FORMAT_XRGB8888);
        wl_shm_pool_destroy(held_pool); roundtrip();
        assert(!close(first) && !munmap(old, 4096));
        int second = os_create_anonymous_file(4096); assert(second >= 0);
        void *other = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, second, 0);
        assert(other != MAP_FAILED && other != old);
        wl_buffer_destroy(held); roundtrip();
        assert(!munmap(other, 4096) && !close(second));
        unsigned attempts;
        for (attempts = 0; attempts < 64; ++attempts) {
            int third = os_create_anonymous_file(4096); assert(third >= 0);
            void *reused = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, third, 0);
            assert(reused != MAP_FAILED);
            int matched = reused == old;
            assert(!munmap(reused, 4096) && !close(third));
            if (matched) break;
            /* A sync reply is not resource-completion authority. Retry actual
             * allocations while the server finishes dropping its references;
             * the allocator may reuse only after its real consumer is gone. */
            roundtrip();
        }
        if (attempts) fprintf(stderr, "WC_CYGWIN_REUSE_WAIT iteration=%u attempts=%u\n", index, attempts);
        assert(attempts < 64);
    }
    xdg_toplevel_destroy(top); xdg_surface_destroy(xdg); wl_surface_destroy(surface);
    wl_keyboard_destroy(keyboard); wl_seat_destroy(seat); xdg_wm_base_destroy(wm);
    wl_shm_destroy(shm); wl_compositor_destroy(compositor); wl_registry_destroy(registry);
    roundtrip(); wl_display_disconnect(display);
    fprintf(stderr, "WC_CYGWIN_LEASE_ACCEPTANCE keymap_bytes=%u grown_bytes=16384 address_preserved=1 remote_hold_blocks_reuse=1 recycled_after_consumer=128 frames=12\n", keymap_size);
    return 0;
}
