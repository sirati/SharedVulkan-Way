/* probe-inhibit.c — a client that exercises the four protocols stage 21 touched.
 *
 * WHY A C CLIENT AND NOT A RUST TEST
 *
 * docs/design.md §14.9 records two occasions on this project where
 * "implemented" turned out to mean "compiled". A unit test in `wc-proxy`
 * proves the wire table; `wayland-info` proves the globals are advertised.
 * Neither proves that a client which *binds* one of these and then uses it
 * gets what the protocol promises — and "binds, then misbehaves" is the exact
 * failure this stage exists to avoid, because that is the shape a client
 * cannot feature-detect its way around.
 *
 * So this is libwayland-client, driven the way a toolkit drives it, against a
 * live compositor. It is built from the same XML in `crates/wc-proxy/protocols`
 * that the compositor's own implementations are generated from, so a
 * disagreement about an opcode would show up here rather than in production.
 *
 * WHAT IT CHECKS, AND WHAT IT CANNOT
 *
 * It maps a real window, because an idle inhibitor on an unmapped surface is
 * one the compositor is supposed to ignore, and a test that never mapped would
 * pass against a compositor that ignored every inhibitor.
 *
 *   1. zwp_keyboard_shortcuts_inhibit — the client is told `active`. This one
 *      is fully checkable from here: the grant is an event, so the assertion is
 *      the client receiving it and not a log line the compositor wrote about
 *      itself.
 *   2. xdg_activation — a token arrives (`done`), and the client spends it.
 *      Whether the window is *raised* is Windows' decision and there is no
 *      event either way; see win32::activate_window. So what is checkable here
 *      is the token dance, which is the half that can be got wrong silently.
 *   3. zwp_idle_inhibit — an inhibitor is created and later destroyed. The
 *      protocol has no events at all, so the client cannot observe the effect
 *      even in principle; the compositor's log is the only witness, and
 *      `SetThreadExecutionState` needs a real guest on top of that.
 *
 * Exit status is 0 only if every step it *can* observe happened.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#include <wayland-client.h>

#include "xdg-shell-client-protocol.h"
#include "idle-inhibit-unstable-v1-client-protocol.h"
#include "xdg-activation-v1-client-protocol.h"
#include "keyboard-shortcuts-inhibit-unstable-v1-client-protocol.h"
#include "xdg-toplevel-icon-v1-client-protocol.h"

#define W 200
#define H 150

static struct wl_compositor *compositor;
static struct wl_shm *shm;
static struct xdg_wm_base *wm_base;
static struct wl_seat *seat;
static struct zwp_idle_inhibit_manager_v1 *idle_mgr;
static struct xdg_activation_v1 *activation;
static struct zwp_keyboard_shortcuts_inhibit_manager_v1 *shortcuts_mgr;
static struct xdg_toplevel_icon_manager_v1 *icon_mgr;

static struct wl_surface *surface;
static struct xdg_surface *xdg_surf;
static struct xdg_toplevel *toplevel;

static int configured;
static int mapped;
static int shortcuts_active;
static int token_done;
static int activated_sent;
static char token_text[512];

/* --- registry ----------------------------------------------------------- */

static void global(void *d, struct wl_registry *r, uint32_t name, const char *iface, uint32_t ver)
{
	(void)d;
	(void)ver;
	if (!strcmp(iface, "wl_compositor"))
		compositor = wl_registry_bind(r, name, &wl_compositor_interface, 4);
	else if (!strcmp(iface, "wl_shm"))
		shm = wl_registry_bind(r, name, &wl_shm_interface, 1);
	else if (!strcmp(iface, "xdg_wm_base"))
		wm_base = wl_registry_bind(r, name, &xdg_wm_base_interface, 1);
	else if (!strcmp(iface, "wl_seat"))
		seat = wl_registry_bind(r, name, &wl_seat_interface, 1);
	else if (!strcmp(iface, "zwp_idle_inhibit_manager_v1"))
		idle_mgr = wl_registry_bind(r, name, &zwp_idle_inhibit_manager_v1_interface, 1);
	else if (!strcmp(iface, "xdg_activation_v1"))
		activation = wl_registry_bind(r, name, &xdg_activation_v1_interface, 1);
	else if (!strcmp(iface, "zwp_keyboard_shortcuts_inhibit_manager_v1"))
		shortcuts_mgr = wl_registry_bind(r, name,
			&zwp_keyboard_shortcuts_inhibit_manager_v1_interface, 1);
	else if (!strcmp(iface, "xdg_toplevel_icon_manager_v1"))
		icon_mgr = wl_registry_bind(r, name, &xdg_toplevel_icon_manager_v1_interface, 1);
}

static void global_remove(void *d, struct wl_registry *r, uint32_t name)
{
	(void)d; (void)r; (void)name;
}

static const struct wl_registry_listener registry_listener = { global, global_remove };

/* --- xdg-shell ---------------------------------------------------------- */

static void wm_ping(void *d, struct xdg_wm_base *b, uint32_t serial)
{
	(void)d;
	xdg_wm_base_pong(b, serial);
}
static const struct xdg_wm_base_listener wm_listener = { wm_ping };

static void surf_configure(void *d, struct xdg_surface *s, uint32_t serial)
{
	(void)d;
	xdg_surface_ack_configure(s, serial);
	configured = 1;
}
static const struct xdg_surface_listener surf_listener = { surf_configure };

static void top_configure(void *d, struct xdg_toplevel *t, int32_t w, int32_t h,
			  struct wl_array *states)
{
	(void)d; (void)t; (void)w; (void)h; (void)states;
}
static void top_close(void *d, struct xdg_toplevel *t) { (void)d; (void)t; }
static void top_bounds(void *d, struct xdg_toplevel *t, int32_t w, int32_t h)
{
	(void)d; (void)t; (void)w; (void)h;
}
static void top_caps(void *d, struct xdg_toplevel *t, struct wl_array *c)
{
	(void)d; (void)t; (void)c;
}
static const struct xdg_toplevel_listener top_listener = {
	top_configure, top_close, top_bounds, top_caps
};

/* --- the two protocols that answer back ---------------------------------- */

static void shortcuts_on(void *d, struct zwp_keyboard_shortcuts_inhibitor_v1 *i)
{
	(void)d; (void)i;
	shortcuts_active = 1;
	printf("probe: keyboard shortcuts inhibitor is ACTIVE\n");
}
static void shortcuts_off(void *d, struct zwp_keyboard_shortcuts_inhibitor_v1 *i)
{
	(void)d; (void)i;
	shortcuts_active = 0;
	printf("probe: keyboard shortcuts inhibitor is inactive\n");
}
static const struct zwp_keyboard_shortcuts_inhibitor_v1_listener shortcuts_listener = {
	shortcuts_on, shortcuts_off
};

static void token_ready(void *d, struct xdg_activation_token_v1 *t, const char *token)
{
	(void)d; (void)t;
	snprintf(token_text, sizeof token_text, "%s", token);
	token_done = 1;
	printf("probe: activation token: %.16s...\n", token);
}
static const struct xdg_activation_token_v1_listener token_listener = { token_ready };

/* The manager announces the sizes it wants before it announces it is finished.
 * Listened to rather than ignored because the sizes are the compositor telling
 * the client which images will actually be used, and a client that sends the
 * others is wasting the boundary crossing they cost. */
static int icon_sizes;
static void icon_size(void *d, struct xdg_toplevel_icon_manager_v1 *m, int32_t size)
{
	(void)d; (void)m;
	icon_sizes++;
	printf("probe: the compositor wants icons at %dx%d\n", size, size);
}
static void icon_done(void *d, struct xdg_toplevel_icon_manager_v1 *m) { (void)d; (void)m; }
static const struct xdg_toplevel_icon_manager_v1_listener icon_listener = {
	icon_size, icon_done
};

/* --- a buffer, so the window actually maps -------------------------------- */

static struct wl_buffer *make_buffer_sized(int w, int h, uint32_t colour)
{
	int stride = w * 4, size = stride * h;
	int fd = memfd_create("wc-probe-inhibit", MFD_CLOEXEC);
	if (fd < 0 || ftruncate(fd, size) < 0) {
		fprintf(stderr, "probe: memfd: %s\n", strerror(errno));
		return NULL;
	}
	uint32_t *px = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	if (px == MAP_FAILED) {
		fprintf(stderr, "probe: mmap: %s\n", strerror(errno));
		close(fd);
		return NULL;
	}
	for (int i = 0; i < w * h; i++)
		px[i] = colour;
	struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, size);
	struct wl_buffer *b = wl_shm_pool_create_buffer(pool, 0, w, h, stride,
						        WL_SHM_FORMAT_ARGB8888);
	wl_shm_pool_destroy(pool);
	close(fd);
	return b;
}

int main(void)
{
	struct wl_display *display = wl_display_connect(NULL);
	if (!display) {
		fprintf(stderr, "probe: cannot connect to the compositor\n");
		return 2;
	}
	struct wl_registry *registry = wl_display_get_registry(display);
	wl_registry_add_listener(registry, &registry_listener, NULL);
	wl_display_roundtrip(display);

	/* Reported one by one: an absent global is a different bug from a
	 * present one that misbehaves, and this is the line that tells them
	 * apart. */
	int missing = 0;
#define NEED(p, n) do { if (!(p)) { fprintf(stderr, "probe: no %s\n", n); missing = 1; } \
		        else printf("probe: bound %s\n", n); } while (0)
	NEED(compositor, "wl_compositor");
	NEED(shm, "wl_shm");
	NEED(wm_base, "xdg_wm_base");
	NEED(seat, "wl_seat");
	NEED(idle_mgr, "zwp_idle_inhibit_manager_v1");
	NEED(activation, "xdg_activation_v1");
	NEED(shortcuts_mgr, "zwp_keyboard_shortcuts_inhibit_manager_v1");
	NEED(icon_mgr, "xdg_toplevel_icon_manager_v1");
#undef NEED
	if (missing)
		return 3;

	/* The listener has to be attached before the roundtrip that lets the
	 * manager's `icon_size` events be delivered — they are sent at bind. */
	xdg_toplevel_icon_manager_v1_add_listener(icon_mgr, &icon_listener, NULL);
	wl_display_roundtrip(display);

	xdg_wm_base_add_listener(wm_base, &wm_listener, NULL);
	surface = wl_compositor_create_surface(compositor);
	xdg_surf = xdg_wm_base_get_xdg_surface(wm_base, surface);
	xdg_surface_add_listener(xdg_surf, &surf_listener, NULL);
	toplevel = xdg_surface_get_toplevel(xdg_surf);
	xdg_toplevel_add_listener(toplevel, &top_listener, NULL);
	xdg_toplevel_set_title(toplevel, "wc probe: inhibit");
	xdg_toplevel_set_app_id(toplevel, "wc-probe-inhibit");
	wl_surface_commit(surface);

	while (!configured && wl_display_dispatch(display) != -1)
		;
	struct wl_buffer *buffer = make_buffer_sized(W, H, 0xff2060a0);
	if (!buffer)
		return 4;
	wl_surface_attach(surface, buffer, 0, 0);
	wl_surface_damage(surface, 0, 0, W, H);
	wl_surface_commit(surface);
	wl_display_roundtrip(display);
	mapped = 1;
	printf("probe: window mapped %dx%d\n", W, H);

	/* xdg-toplevel-icon. This is the one that used to be a wall rather than
	 * a missing feature: the compositor advertised the global, the proxy's
	 * table had never heard of the interface, and the *first* request on the
	 * manager ended the connection. Through the proxy (`wc-inhibit.sh
	 * wine`) this is therefore the assertion that stage 21's first item was
	 * a real bug and is fixed; on the Linux compositor it is a plain
	 * exercise of the icon path. */
	struct xdg_toplevel_icon_v1 *icon = xdg_toplevel_icon_manager_v1_create_icon(icon_mgr);
	xdg_toplevel_icon_v1_set_name(icon, "wc-probe-inhibit");
	struct wl_buffer *icon_px = make_buffer_sized(32, 32, 0xffcc4020);
	if (!icon_px)
		return 4;
	xdg_toplevel_icon_v1_add_buffer(icon, icon_px, 1);
	xdg_toplevel_icon_manager_v1_set_icon(icon_mgr, toplevel, icon);
	wl_surface_commit(surface);
	wl_display_roundtrip(display);
	printf("probe: an icon was set through xdg_toplevel_icon_v1\n");

	/* Shortcuts next: it is the one with an answer, so a compositor that
	 * accepted the object and never activated it is caught here rather
	 * than being read off a log. */
	struct zwp_keyboard_shortcuts_inhibitor_v1 *grab =
		zwp_keyboard_shortcuts_inhibit_manager_v1_inhibit_shortcuts(shortcuts_mgr,
									   surface, seat);
	zwp_keyboard_shortcuts_inhibitor_v1_add_listener(grab, &shortcuts_listener, NULL);
	wl_display_roundtrip(display);

	/* Activation: mint a token against this surface, then spend it. A real
	 * launcher passes the string to another process; there is only one
	 * process here, which changes nothing about the exchange. */
	struct xdg_activation_token_v1 *tok = xdg_activation_v1_get_activation_token(activation);
	xdg_activation_token_v1_add_listener(tok, &token_listener, NULL);
	xdg_activation_token_v1_set_surface(tok, surface);
	xdg_activation_token_v1_set_app_id(tok, "wc-probe-inhibit");
	xdg_activation_token_v1_commit(tok);
	wl_display_roundtrip(display);
	if (token_done) {
		xdg_activation_v1_activate(activation, token_text, surface);
		activated_sent = 1;
		wl_display_roundtrip(display);
	}

	/* Idle inhibit, held long enough that the compositor's per-turn sweep
	 * has run several times over it. */
	struct zwp_idle_inhibitor_v1 *inhibitor =
		zwp_idle_inhibit_manager_v1_create_inhibitor(idle_mgr, surface);
	wl_display_roundtrip(display);
	printf("probe: idle inhibitor created\n");
	struct timespec ts = { 0, 400 * 1000 * 1000 };
	nanosleep(&ts, NULL);
	wl_display_roundtrip(display);

	zwp_idle_inhibitor_v1_destroy(inhibitor);
	wl_display_roundtrip(display);
	printf("probe: idle inhibitor destroyed\n");
	nanosleep(&ts, NULL);
	wl_display_roundtrip(display);

	/* And the case nothing else covers: an inhibitor that is *never*
	 * destroyed, abandoned by a client that simply disconnects. smithay
	 * calls `uninhibit` only for an explicit destroy, so this is the path
	 * that leaves a compositor keeping the display awake for ever if the
	 * sweep in `State::tick_idle_inhibit` is not there. */
	zwp_idle_inhibit_manager_v1_create_inhibitor(idle_mgr, surface);
	wl_display_roundtrip(display);
	printf("probe: a second inhibitor created and deliberately abandoned\n");
	nanosleep(&ts, NULL);
	wl_display_roundtrip(display);

	int ok = mapped && shortcuts_active && token_done && activated_sent && icon_sizes > 0;
	printf("probe: mapped=%d shortcuts_active=%d token=%d activate_sent=%d icon_sizes=%d\n",
	       mapped, shortcuts_active, token_done, activated_sent, icon_sizes);
	/* Disconnect without destroying anything. */
	wl_display_disconnect(display);
	return ok ? 0 : 5;
}
