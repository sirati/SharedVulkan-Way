/* SPDX-License-Identifier: MIT
 * Allocation integration in the shipped GTK library. Firefox and Mesa are
 * unchanged. Cairo paints directly into the registered transport backing. */
#include "config.h"
#include "gtk-native-shm-helper.h"
#include "wc-native-shm-client-protocol.h"
#include <wc-native-shm.h>
#include <string.h>
#include <limits.h>

typedef struct {
  GdkWaylandDisplay *display;
  struct wl_event_queue *queue;
  struct wc_native_shm_v1 *manager;
  GMutex mutex;
  guint32 next_serial;
  guint32 pending;
  guint32 requested_size;
  struct wc_native_shm_mapping *mapping;
  char *failure;
  gboolean allocated;
} WcNativeState;

G_LOCK_DEFINE_STATIC(wc_native_states);

typedef struct {
  WcNativeState *state;
  GdkWaylandDisplay *display;
  guint32 serial;
  struct wc_native_shm_mapping *mapping;
} WcNativeOwner;

static void allocated(void *opaque, struct wc_native_shm_v1 *manager,
    uint32_t serial, uint32_t kind, uint32_t id_high, uint32_t id_low,
    uint32_t offset_high, uint32_t offset_low, uint32_t size, const char *path)
{
  WcNativeState *state = opaque;
  (void)manager;
  if (serial != state->pending || size != state->requested_size) {
    g_free(state->failure);
    state->failure = g_strdup("allocator response did not match the requested allocation");
    return;
  }
  state->allocated = TRUE;
  state->mapping = wc_native_shm_open(kind, ((uint64_t)id_high << 32) | id_low,
      path, ((uint64_t)offset_high << 32) | offset_low, size);
  if (!state->mapping)
    state->failure = g_strdup("registered original backing could not be opened");
}
static void failed(void *opaque, struct wc_native_shm_v1 *manager,
    uint32_t serial, const char *reason)
{
  WcNativeState *state = opaque;
  (void)manager;
  if (serial == state->pending) {
    g_free(state->failure);
    state->failure = g_strdup(reason);
  }
}
static const struct wc_native_shm_v1_listener manager_listener = { allocated, failed };

static void global(void *opaque, struct wl_registry *registry, uint32_t name,
    const char *interface, uint32_t version)
{
  WcNativeState *state = opaque;
  if (!state->manager && version >= 1 && strcmp(interface, "wc_native_shm_v1") == 0) {
    state->manager = wl_registry_bind(registry, name, &wc_native_shm_v1_interface, 1);
    wc_native_shm_v1_add_listener(state->manager, &manager_listener, state);
  }
}
static void global_remove(void *opaque, struct wl_registry *registry, uint32_t name)
{ (void)opaque; (void)registry; (void)name; }
static const struct wl_registry_listener registry_listener = { global, global_remove };

static void state_free(void *opaque)
{
  WcNativeState *state = opaque;
  if (state->mapping) wc_native_shm_close(state->mapping);
  if (state->manager) wc_native_shm_v1_destroy(state->manager);
  if (state->queue) wl_event_queue_destroy(state->queue);
  g_free(state->failure);
  g_mutex_clear(&state->mutex);
  g_free(state);
}

void wc_gtk_native_shm_display_finish(GdkWaylandDisplay *display)
{
  /* Called before wl_display_disconnect. Live Cairo owners hold a display
   * reference, so finalization cannot invalidate their queue or proxies. */
  WcNativeState *state = g_object_steal_data(G_OBJECT(display), "wc-native-shm");
  if (state) state_free(state);
}

static WcNativeState *state_get(GdkWaylandDisplay *display)
{
  WcNativeState *state = g_object_get_data(G_OBJECT(display), "wc-native-shm");
  if (state) return state;
  state = g_new0(WcNativeState, 1);
  state->display = display;
  g_mutex_init(&state->mutex);
  state->queue = wl_display_create_queue(display->wl_display);
  if (!state->queue) { state_free(state); return NULL; }
  struct wl_proxy *wrapper = wl_proxy_create_wrapper(display->wl_display);
  if (!wrapper) { state_free(state); return NULL; }
  wl_proxy_set_queue(wrapper, state->queue);
  struct wl_registry *registry = wl_display_get_registry((struct wl_display *)wrapper);
  wl_proxy_wrapper_destroy(wrapper);
  wl_registry_add_listener(registry, &registry_listener, state);
  if (wl_display_roundtrip_queue(display->wl_display, state->queue) < 0) {
    wl_registry_destroy(registry); state_free(state); return NULL;
  }
  wl_registry_destroy(registry);
  g_object_set_data_full(G_OBJECT(display), "wc-native-shm", state, state_free);
  return state;
}

int wc_gtk_native_shm_create(GdkWaylandDisplay *display, int size,
    struct wl_shm_pool **pool, void **pixels, void **owner, int *buffer_offset)
{
  const char *strict = g_getenv("WC_NATIVE_SHM_STRICT");
  G_LOCK(wc_native_states);
  WcNativeState *state = state_get(display);
  G_UNLOCK(wc_native_states);
  if (!state || !state->manager) {
    if (strict && strcmp(strict, "1") == 0) {
      g_critical("native GTK shm allocation required but the display has no allocator");
      return -1;
    }
    return 0;
  }
  if (size <= 0) return -1;
  g_mutex_lock(&state->mutex);
  if (++state->next_serial == 0) {
    g_mutex_unlock(&state->mutex);
    g_critical("native GTK shm allocation serial space exhausted");
    return -1;
  }
  state->pending = state->next_serial;
  state->requested_size = size;
  g_clear_pointer(&state->failure, g_free);
  state->mapping = NULL;
  state->allocated = FALSE;
  wc_native_shm_v1_allocate(state->manager, state->pending, size);
  int result = wl_display_roundtrip_queue(display->wl_display, state->queue);
  if (result < 0 || state->failure || !state->mapping) {
    g_critical("native GTK shm allocation refused: %s", state->failure ? state->failure : "display connection failed");
    if (state->mapping) { wc_native_shm_close(state->mapping); state->mapping = NULL; }
    if (state->allocated) wc_native_shm_v1_release(state->manager, state->pending);
    g_mutex_unlock(&state->mutex);
    return -1;
  }
  uint64_t offset = wc_native_shm_offset(state->mapping);
  if (offset > INT_MAX || (uint64_t)size > INT_MAX - offset) {
    wc_native_shm_close(state->mapping); state->mapping = NULL;
    wc_native_shm_v1_release(state->manager, state->pending);
    g_mutex_unlock(&state->mutex);
    return -1;
  }
  WcNativeOwner *hold = g_new0(WcNativeOwner, 1);
  hold->state = state;
  hold->display = g_object_ref(display);
  hold->serial = state->pending;
  hold->mapping = state->mapping;
  state->mapping = NULL;
  *pool = wl_shm_create_pool(display->shm, wc_native_shm_fd(hold->mapping), offset + size);
  if (!*pool) {
    wc_native_shm_close(hold->mapping);
    wc_native_shm_v1_release(state->manager, hold->serial);
    g_object_unref(hold->display);
    g_free(hold);
    g_mutex_unlock(&state->mutex);
    return -1;
  }
  *pixels = wc_native_shm_data(hold->mapping);
  *buffer_offset = offset;
  *owner = hold;
  g_mutex_unlock(&state->mutex);
  return 1;
}

void wc_gtk_native_shm_close(void *opaque)
{
  WcNativeOwner *hold = opaque;
  if (!hold) return;
  /* Original mappings end before our authenticated broker lease closes.
   * The proxy independently waits for compositor release and deletion. */
  wc_native_shm_close(hold->mapping);
  g_mutex_lock(&hold->state->mutex);
  wc_native_shm_v1_release(hold->state->manager, hold->serial);
  g_mutex_unlock(&hold->state->mutex);
  g_object_unref(hold->display);
  g_free(hold);
}
