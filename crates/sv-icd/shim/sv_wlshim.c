/*
 * libsv_wlshim.so: lets the ICD finish a Wayland present after
 * vkQueuePresentKHR has returned (docs/waycompose-design.md §100).
 *
 * The ICD shows a frame with attach, damage and commit on the application's
 * wl_surface. Doing that on its own thread raced the application destroying
 * that surface (§98: Firefox crashed at quit). This library is preloaded into
 * the application (LD_PRELOAD) and stands in front of libwayland's request
 * entry points. For a surface the ICD has registered, it does two things,
 * both while holding the one lock the ICD's present thread holds around its
 * own attach, damage and commit:
 *
 *  - the application's wl_surface.destroy (or wl_proxy_destroy) marks the
 *    surface dead before it is forwarded, so no request of ours follows it;
 *  - the application's attach of a NULL buffer (an unmap) bumps the surface's
 *    generation, so a frame the application presented before the unmap is
 *    not shown after it.
 *
 * Every other request, and every request on any other object, is forwarded
 * unchanged. Without this library the ICD presents inside vkQueuePresentKHR
 * (§98), so it is an optimisation that can never be needed for correctness.
 *
 * No DT_NEEDED on libwayland: the real entry points are looked up in the
 * libwayland-client the process already has loaded, by its handle, never by
 * RTLD_NEXT, which in Firefox would find libmozwayland's link-time stubs.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

/* The libwayland ABI this needs, which has not changed since 1.0. */
struct wl_message { const char *name; const char *signature; const void **types; };
struct wl_interface {
    const char *name; int version;
    int method_count; const struct wl_message *methods;
    int event_count; const struct wl_message *events;
};
union wl_argument { int32_t i; uint32_t u; int32_t f; const char *s; void *o; uint32_t n; void *a; int32_t h; };
/* struct wl_proxy begins with struct wl_object, which begins with its interface. */
struct wl_object_head { const struct wl_interface *interface; };

#define MAX_ARGS 20
#define WL_SURFACE_DESTROY 0
#define WL_SURFACE_ATTACH 1
#define WL_MARSHAL_FLAG_DESTROY 1u
#define SLOTS 64

typedef void *(*array_flags_fn)(void *, uint32_t, const struct wl_interface *, uint32_t, uint32_t, union wl_argument *);
typedef void (*array_fn)(void *, uint32_t, union wl_argument *);
typedef void (*destroy_fn)(void *);

static array_flags_fn real_array_flags;
static array_fn real_array;
static destroy_fn real_destroy;
static pthread_once_t once = PTHREAD_ONCE_INIT;

static void resolve(void) {
    void *h = dlopen("libwayland-client.so.0", RTLD_LAZY | RTLD_NOLOAD);
    if (!h) h = dlopen("libwayland-client.so.0", RTLD_LAZY);
    if (!h) { fprintf(stderr, "sv_wlshim: libwayland-client.so.0 is not loadable\n"); abort(); }
    real_array_flags = (array_flags_fn)dlsym(h, "wl_proxy_marshal_array_flags");
    real_array = (array_fn)dlsym(h, "wl_proxy_marshal_array");
    real_destroy = (destroy_fn)dlsym(h, "wl_proxy_destroy");
    if (!real_array_flags || !real_array || !real_destroy) {
        fprintf(stderr, "sv_wlshim: libwayland-client lacks wl_proxy_marshal_array_flags\n");
        abort();
    }
}

/* The registered surfaces. `lock` also serialises the ICD's frame against the
 * application's destroy and unmap of the same surface. */
struct slot { void *proxy; int used; int alive; uint32_t gen; };
static struct slot slots[SLOTS];
static int tracked;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;

static struct slot *find_locked(void *proxy) {
    for (int i = 0; i < SLOTS; i++)
        if (slots[i].used && slots[i].alive && slots[i].proxy == proxy) return &slots[i];
    return NULL;
}

/* What a request does to a registered surface. Called with `lock` held. */
static void note_locked(struct slot *s, uint32_t opcode, uint32_t flags, const union wl_argument *args) {
    if (opcode == WL_SURFACE_DESTROY || (flags & WL_MARSHAL_FLAG_DESTROY)) s->alive = 0;
    else if (opcode == WL_SURFACE_ATTACH && args && args[0].o == NULL) s->gen++;
}

/* Arguments from a va_list, as libwayland's wl_argument_from_va_list does. */
static int from_va(const char *sig, union wl_argument *args, va_list ap) {
    int n = 0;
    for (const char *c = sig; *c; c++) {
        if (*c == '?' || (*c >= '0' && *c <= '9')) continue;
        if (n == MAX_ARGS) return -1;
        switch (*c) {
        case 'i': args[n].i = va_arg(ap, int32_t); break;
        case 'u': args[n].u = va_arg(ap, uint32_t); break;
        case 'f': args[n].f = va_arg(ap, int32_t); break;
        case 's': args[n].s = va_arg(ap, const char *); break;
        case 'o': args[n].o = va_arg(ap, void *); break;
        case 'n': args[n].o = va_arg(ap, void *); break;
        case 'a': args[n].a = va_arg(ap, void *); break;
        case 'h': args[n].h = va_arg(ap, int32_t); break;
        default: return -1;
        }
        n++;
    }
    return n;
}

static const char *signature(void *proxy, uint32_t opcode) {
    const struct wl_interface *i = ((struct wl_object_head *)proxy)->interface;
    if (!i || opcode >= (uint32_t)i->method_count) return NULL;
    return i->methods[opcode].signature;
}

static void *marshal(void *proxy, uint32_t opcode, const struct wl_interface *iface, uint32_t version,
                     uint32_t flags, union wl_argument *args, int legacy) {
    pthread_once(&once, resolve);
    if (__atomic_load_n(&tracked, __ATOMIC_ACQUIRE) > 0) {
        pthread_mutex_lock(&lock);
        struct slot *s = find_locked(proxy);
        if (s) {
            note_locked(s, opcode, flags, args);
            void *r = NULL;
            if (legacy) real_array(proxy, opcode, args);
            else r = real_array_flags(proxy, opcode, iface, version, flags, args);
            pthread_mutex_unlock(&lock);
            return r;
        }
        pthread_mutex_unlock(&lock);
    }
    if (legacy) { real_array(proxy, opcode, args); return NULL; }
    return real_array_flags(proxy, opcode, iface, version, flags, args);
}

__attribute__((visibility("default")))
void *wl_proxy_marshal_flags(void *proxy, uint32_t opcode, const struct wl_interface *iface,
                             uint32_t version, uint32_t flags, ...) {
    union wl_argument args[MAX_ARGS];
    const char *sig = signature(proxy, opcode);
    va_list ap;
    va_start(ap, flags);
    int n = sig ? from_va(sig, args, ap) : -1;
    va_end(ap);
    if (n < 0) { fprintf(stderr, "sv_wlshim: cannot read the signature of request %u\n", opcode); abort(); }
    return marshal(proxy, opcode, iface, version, flags, args, 0);
}

__attribute__((visibility("default")))
void wl_proxy_marshal(void *proxy, uint32_t opcode, ...) {
    union wl_argument args[MAX_ARGS];
    const char *sig = signature(proxy, opcode);
    va_list ap;
    va_start(ap, opcode);
    int n = sig ? from_va(sig, args, ap) : -1;
    va_end(ap);
    if (n < 0) { fprintf(stderr, "sv_wlshim: cannot read the signature of request %u\n", opcode); abort(); }
    marshal(proxy, opcode, NULL, 0, 0, args, 1);
}

__attribute__((visibility("default")))
void *wl_proxy_marshal_array_flags(void *proxy, uint32_t opcode, const struct wl_interface *iface,
                                   uint32_t version, uint32_t flags, union wl_argument *args) {
    return marshal(proxy, opcode, iface, version, flags, args, 0);
}

__attribute__((visibility("default")))
void wl_proxy_marshal_array(void *proxy, uint32_t opcode, union wl_argument *args) {
    marshal(proxy, opcode, NULL, 0, 0, args, 1);
}

__attribute__((visibility("default")))
void wl_proxy_destroy(void *proxy) {
    pthread_once(&once, resolve);
    if (__atomic_load_n(&tracked, __ATOMIC_ACQUIRE) > 0) {
        pthread_mutex_lock(&lock);
        struct slot *s = find_locked(proxy);
        if (s) s->alive = 0;
        real_destroy(proxy);
        pthread_mutex_unlock(&lock);
        return;
    }
    real_destroy(proxy);
}

/* ------------------------------------------------------------ the ICD's side.
 * Called by libsv_icd.so, found with dlsym(RTLD_DEFAULT). The version is in
 * the names: a newer ICD with an older shim finds nothing and presents in the
 * call, as without the shim. */

/* Register a surface. Must be called while the surface is known to be alive,
 * i.e. inside the application's Vulkan call. Returns a slot, or -1. */
__attribute__((visibility("default")))
int svwl_shim_v1_track(void *surface) {
    pthread_mutex_lock(&lock);
    int r = -1;
    for (int i = 0; i < SLOTS; i++) {
        if (!slots[i].used) {
            slots[i] = (struct slot){ .proxy = surface, .used = 1, .alive = 1, .gen = 0 };
            __atomic_add_fetch(&tracked, 1, __ATOMIC_RELEASE);
            r = i;
            break;
        }
    }
    pthread_mutex_unlock(&lock);
    return r;
}

__attribute__((visibility("default")))
void svwl_shim_v1_untrack(int slot) {
    if (slot < 0 || slot >= SLOTS) return;
    pthread_mutex_lock(&lock);
    if (slots[slot].used) {
        slots[slot].used = 0;
        __atomic_sub_fetch(&tracked, 1, __ATOMIC_RELEASE);
    }
    pthread_mutex_unlock(&lock);
}

/* The surface's generation now; read inside vkQueuePresentKHR. */
__attribute__((visibility("default")))
uint32_t svwl_shim_v1_generation(int slot) {
    if (slot < 0 || slot >= SLOTS) return 0;
    pthread_mutex_lock(&lock);
    uint32_t g = slots[slot].gen;
    pthread_mutex_unlock(&lock);
    return g;
}

/* Begin showing a frame presented at generation `gen`. Returns 1 with the lock
 * held when the surface is alive and has not been unmapped since; the caller
 * then makes its requests and calls svwl_shim_v1_end. Returns 0, lock not
 * held, when the frame must be dropped. */
__attribute__((visibility("default")))
int svwl_shim_v1_begin(int slot, uint32_t gen) {
    if (slot < 0 || slot >= SLOTS) return 0;
    pthread_mutex_lock(&lock);
    if (slots[slot].used && slots[slot].alive && slots[slot].gen == gen) return 1;
    pthread_mutex_unlock(&lock);
    return 0;
}

__attribute__((visibility("default")))
void svwl_shim_v1_end(void) { pthread_mutex_unlock(&lock); }
