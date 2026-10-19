/*
 * wc-dmabuf-client: a window whose pixels are an sv-drm token, not a wl_shm
 * pool (docs/waycompose-design.md §104). Built and run by scripts/wc-dmabuf.sh.
 *
 * It does what a nested KWin does with its GBM buffers, minus the GPU: binds
 * zwp_linux_dmabuf_v1 (v4), reads the default feedback (format table, main
 * device), puts W x H XRGB8888 pixels in /dev/shm/<name>, makes a sealed
 * memfd token naming that file (the sv_drm::token format), hands it over with
 * zwp_linux_buffer_params_v1.add + create_immed, and shows it in an
 * xdg_toplevel. Then, for SECS seconds, it moves a bar *in the shared file*
 * and commits the same buffer each frame: the compositor can only show the
 * bar moving if it reads the file itself, because nothing is copied.
 *
 * Output (stdout, one fact per line, for the harness):
 *   feedback main_device=MAJ:MIN table=N formats=0x...,0x...
 *   token name=/dev/shm/... size=N
 *   buffer created
 *   frames N
 * Exit 0 on success, 2 if the global is not offered, 1 on any failure.
 *
 *   wc-dmabuf-client [SECS] [W] [H]
 *   wc-dmabuf-client SECS W H bench [NBUF] [xrgb|argb|argbt] [SQ] [flat|kwin]
 *
 * `bench` measures the compositor's CPU rather than its correctness, so it
 * behaves like KWin's swapchain and not like a test pattern: NBUF buffers
 * (default 3), each in its own /dev/shm file as the GPU side's sections are;
 * a buffer is reused only after the compositor released it; each frame moves
 * an SQ x SQ square (default 64) over a static gradient, repaints in the
 * chosen buffer what that buffer owes (the union of the damage since it was
 * last drawn, which is what buffer age does) and damages only the square's
 * old and new place. Paced by frame callbacks. Extra output: `released N`,
 * `release_waits N`. `argbt` is ARGB8888 whose gradient is translucent
 * (premultiplied, alpha 0xC0). `kwin` is the shape of KWin's Wayland output
 * (§102): the toplevel's surface is one opaque black single-pixel buffer
 * stretched by a viewport to W x H, and the frames are in a synchronised
 * subsurface at (0, 0) with an identity viewport, committed before the parent
 * every frame.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <time.h>
#include <unistd.h>

#include <wayland-client.h>
#include "xdg-shell-client-protocol.h"
#include "linux-dmabuf-v1-client-protocol.h"
#include "single-pixel-buffer-v1-client-protocol.h"
#include "viewporter-client-protocol.h"

#define FMT_XRGB8888 0x34325258u /* 'XR24' */

static struct wl_compositor *compositor;
static struct xdg_wm_base *wm;
static struct zwp_linux_dmabuf_v1 *dmabuf;
static struct wl_subcompositor *subcompositor;
static struct wp_viewporter *viewporter;
static struct wp_single_pixel_buffer_manager_v1 *single_pixel;
static int configured, failed, closed;
static uint32_t *px;
static int W = 320, H = 200;
static unsigned frames;
static int frame_pending;
static uint32_t fmt = FMT_XRGB8888;

static void reg_global(void *d, struct wl_registry *r, uint32_t name, const char *iface, uint32_t v) {
	(void)d;
	if (!strcmp(iface, "wl_compositor"))
		compositor = wl_registry_bind(r, name, &wl_compositor_interface, v < 4 ? v : 4);
	else if (!strcmp(iface, "xdg_wm_base"))
		wm = wl_registry_bind(r, name, &xdg_wm_base_interface, 1);
	else if (!strcmp(iface, "zwp_linux_dmabuf_v1") && v >= 4)
		dmabuf = wl_registry_bind(r, name, &zwp_linux_dmabuf_v1_interface, 4);
	else if (!strcmp(iface, "wl_subcompositor"))
		subcompositor = wl_registry_bind(r, name, &wl_subcompositor_interface, 1);
	else if (!strcmp(iface, "wp_viewporter"))
		viewporter = wl_registry_bind(r, name, &wp_viewporter_interface, 1);
	else if (!strcmp(iface, "wp_single_pixel_buffer_manager_v1"))
		single_pixel = wl_registry_bind(r, name, &wp_single_pixel_buffer_manager_v1_interface, 1);
}
static void reg_remove(void *d, struct wl_registry *r, uint32_t n) { (void)d; (void)r; (void)n; }
static const struct wl_registry_listener reg_l = { reg_global, reg_remove };

static void wm_ping(void *d, struct xdg_wm_base *w, uint32_t s) { (void)d; xdg_wm_base_pong(w, s); }
static const struct xdg_wm_base_listener wm_l = { wm_ping };

/* --- feedback -------------------------------------------------------- */
static int fb_done;
static unsigned table_n;
static uint32_t table_fmt[16];
static dev_t main_dev;

static void fb_done_ev(void *d, struct zwp_linux_dmabuf_feedback_v1 *f) { (void)d; (void)f; fb_done = 1; }
static void fb_table(void *d, struct zwp_linux_dmabuf_feedback_v1 *f, int32_t fd, uint32_t size) {
	(void)d; (void)f;
	void *m = mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0);
	if (m != MAP_FAILED) {
		table_n = size / 16;
		for (unsigned i = 0; i < table_n && i < 16; i++)
			memcpy(&table_fmt[i], (char *)m + i * 16, 4);
		munmap(m, size);
	}
	close(fd);
}
static void fb_main(void *d, struct zwp_linux_dmabuf_feedback_v1 *f, struct wl_array *dev) {
	(void)d; (void)f;
	if (dev->size == sizeof(dev_t))
		memcpy(&main_dev, dev->data, sizeof(dev_t));
}
static void fb_tdone(void *d, struct zwp_linux_dmabuf_feedback_v1 *f) { (void)d; (void)f; }
static void fb_target(void *d, struct zwp_linux_dmabuf_feedback_v1 *f, struct wl_array *a) { (void)d; (void)f; (void)a; }
static void fb_formats(void *d, struct zwp_linux_dmabuf_feedback_v1 *f, struct wl_array *a) { (void)d; (void)f; (void)a; }
static void fb_flags(void *d, struct zwp_linux_dmabuf_feedback_v1 *f, uint32_t fl) { (void)d; (void)f; (void)fl; }
static const struct zwp_linux_dmabuf_feedback_v1_listener fb_l = {
	fb_done_ev, fb_table, fb_main, fb_tdone, fb_target, fb_formats, fb_flags,
};

/* --- params ---------------------------------------------------------- */
static void p_created(void *d, struct zwp_linux_buffer_params_v1 *p, struct wl_buffer *b) { (void)d; (void)p; (void)b; }
static void p_failed(void *d, struct zwp_linux_buffer_params_v1 *p) { (void)d; (void)p; failed = 1; }
static const struct zwp_linux_buffer_params_v1_listener p_l = { p_created, p_failed };

/* --- xdg -------------------------------------------------------------- */
static void xs_configure(void *d, struct xdg_surface *s, uint32_t serial) {
	(void)d;
	xdg_surface_ack_configure(s, serial);
	configured = 1;
}
static const struct xdg_surface_listener xs_l = { xs_configure };
static void top_configure(void *d, struct xdg_toplevel *t, int32_t w, int32_t h, struct wl_array *s) { (void)d; (void)t; (void)w; (void)h; (void)s; }
static void top_close(void *d, struct xdg_toplevel *t) { (void)d; (void)t; closed = 1; }
static void top_bounds(void *d, struct xdg_toplevel *t, int32_t w, int32_t h) { (void)d; (void)t; (void)w; (void)h; }
static void top_caps(void *d, struct xdg_toplevel *t, struct wl_array *c) { (void)d; (void)t; (void)c; }
static const struct xdg_toplevel_listener top_l = { top_configure, top_close, top_bounds, top_caps };

static void frame_done(void *d, struct wl_callback *cb, uint32_t t) { (void)d; (void)t; wl_callback_destroy(cb); frame_pending = 0; }
static const struct wl_callback_listener frame_l = { frame_done };

/* The picture: four quadrants (red, green, blue, white) and a black bar
 * whose row moves with `tick`. XRGB8888 is B,G,R,X in memory. */
static void paint(unsigned tick) {
	int bar = (int)(tick * 4 % (unsigned)H);
	for (int y = 0; y < H; y++)
		for (int x = 0; x < W; x++) {
			uint32_t c;
			if (y >= bar && y < bar + 8) c = 0x000000;
			else if (y < H / 2) c = x < W / 2 ? 0xff0000 : 0x00ff00;
			else c = x < W / 2 ? 0x0000ff : 0xffffff;
			px[y * W + x] = 0xff000000u | c;
		}
}

/* sv_drm::token: magic, version, hdr_len, size, nonce, name_len, name; all LE. */
static int make_token(const char *name, uint64_t size) {
	int fd = memfd_create("sv-dmabuf", MFD_CLOEXEC | MFD_ALLOW_SEALING);
	if (fd < 0) return -1;
	uint32_t nlen = (uint32_t)strlen(name), hdr = 8 + 4 + 4 + 8 + 16 + 4 + nlen, ver = 1;
	unsigned char b[512];
	size_t o = 0;
	memcpy(b + o, "SVDMABF1", 8); o += 8;
	memcpy(b + o, &ver, 4); o += 4;
	memcpy(b + o, &hdr, 4); o += 4;
	memcpy(b + o, &size, 8); o += 8;
	for (int i = 0; i < 16; i++) b[o++] = (unsigned char)(rand() & 0xff);
	memcpy(b + o, &nlen, 4); o += 4;
	memcpy(b + o, name, nlen); o += nlen;
	if (pwrite(fd, b, o, 0) != (ssize_t)o) return -1;
	if (ftruncate(fd, (off_t)(size > o ? size : o)) != 0) return -1;
	if (fcntl(fd, F_ADD_SEALS, F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_SEAL) != 0) return -1;
	return fd;
}

/* --- bench: a KWin-like swapchain ---------------------------------------- */
#define MAXBUF 4
struct rect { int x0, y0, x1, y1; }; /* half-open; empty when x1 <= x0 */
struct sbuf {
	struct wl_buffer *wl;
	uint32_t *px;
	char path[128];
	int busy;
	struct rect owed;
};
static struct sbuf bufs[MAXBUF];
static unsigned released, release_waits;

static struct rect r_union(struct rect a, struct rect b) {
	if (a.x1 <= a.x0) return b;
	if (b.x1 <= b.x0) return a;
	struct rect r = { a.x0 < b.x0 ? a.x0 : b.x0, a.y0 < b.y0 ? a.y0 : b.y0,
	                  a.x1 > b.x1 ? a.x1 : b.x1, a.y1 > b.y1 ? a.y1 : b.y1 };
	return r;
}
static void buf_release(void *d, struct wl_buffer *b) { (void)b; ((struct sbuf *)d)->busy = 0; released++; }
static const struct wl_buffer_listener buf_l = { buf_release };

/* Where the square is at `tick`: left to right and back, in a band of rows. */
static struct rect square_at(unsigned tick, int sq) {
	int span = W - sq, x = span > 0 ? (int)(tick * 4 % (unsigned)(2 * span)) : 0;
	if (x > span) x = 2 * span - x;
	int y = H / 3 + (int)(tick / 64 % 4) * sq / 2;
	if (y + sq > H) y = H - sq;
	struct rect r = { x, y, x + sq, y + sq };
	return r;
}
/* The picture inside `r`: a gradient, and the square in opaque black. With
 * `alpha` < 255 the gradient is translucent, premultiplied. */
static void paint_rect(uint32_t *p, struct rect r, struct rect sqr, uint32_t alpha) {
	for (int y = r.y0; y < r.y1; y++)
		for (int x = r.x0; x < r.x1; x++) {
			if (x >= sqr.x0 && x < sqr.x1 && y >= sqr.y0 && y < sqr.y1) {
				p[y * W + x] = 0xff000000u;
				continue;
			}
			uint32_t cr = (uint32_t)(x * 255 / W) * alpha / 255;
			uint32_t cg = (uint32_t)(y * 255 / H) * alpha / 255;
			uint32_t cb = 0x80u * alpha / 255;
			p[y * W + x] = alpha << 24 | cr << 16 | cg << 8 | cb;
		}
}

static int bench(struct wl_display *dpy, int secs, int nbuf, int sq, int kwin, uint32_t alpha) {
	size_t size = (size_t)W * H * 4;
	struct rect full = { 0, 0, W, H };
	for (int i = 0; i < nbuf; i++) {
		struct sbuf *b = &bufs[i];
		snprintf(b->path, sizeof b->path, "/dev/shm/sv-dmabuf-test-%d-%d", (int)getpid(), i);
		int shm = open(b->path, O_RDWR | O_CREAT | O_EXCL, 0600);
		if (shm < 0 || ftruncate(shm, (off_t)size) != 0) { perror(b->path); return 1; }
		b->px = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, shm, 0);
		close(shm);
		if (b->px == MAP_FAILED) { perror("mmap"); return 1; }
		b->owed = full;
		int tok = make_token(b->path, size);
		if (tok < 0) { perror("token"); return 1; }
		struct zwp_linux_buffer_params_v1 *params = zwp_linux_dmabuf_v1_create_params(dmabuf);
		zwp_linux_buffer_params_v1_add_listener(params, &p_l, NULL);
		zwp_linux_buffer_params_v1_add(params, tok, 0, 0, (uint32_t)W * 4, 0, 0);
		close(tok);
		b->wl = zwp_linux_buffer_params_v1_create_immed(params, W, H, fmt, 0);
		zwp_linux_buffer_params_v1_destroy(params);
		wl_buffer_add_listener(b->wl, &buf_l, b);
	}
	wl_display_roundtrip(dpy);
	if (failed) { printf("buffer failed\n"); return 1; }
	printf("buffer created\n");

	struct wl_surface *root = wl_compositor_create_surface(compositor);
	struct xdg_surface *xs = xdg_wm_base_get_xdg_surface(wm, root);
	xdg_surface_add_listener(xs, &xs_l, NULL);
	struct xdg_toplevel *top = xdg_surface_get_toplevel(xs);
	xdg_toplevel_add_listener(top, &top_l, NULL);
	xdg_toplevel_set_title(top, "wc-dmabuf-client bench");
	wl_surface_commit(root);
	while (!configured && wl_display_dispatch(dpy) >= 0) {}

	/* `surf` is what the frames are attached to: the toplevel itself, or
	 * (kwin) a subsurface over a black single pixel, as KWin's output is. */
	struct wl_surface *surf = root;
	if (kwin) {
		if (!subcompositor || !viewporter || !single_pixel) {
			printf("kwin shape: no wl_subcompositor/wp_viewporter/single-pixel buffers\n");
			return 1;
		}
		struct wl_buffer *black =
			wp_single_pixel_buffer_manager_v1_create_u32_rgba_buffer(single_pixel, 0, 0, 0, 0xffffffffu);
		struct wp_viewport *rv = wp_viewporter_get_viewport(viewporter, root);
		wp_viewport_set_destination(rv, W, H);
		surf = wl_compositor_create_surface(compositor);
		struct wl_subsurface *sub = wl_subcompositor_get_subsurface(subcompositor, surf, root);
		wl_subsurface_set_position(sub, 0, 0);
		struct wp_viewport *lv = wp_viewporter_get_viewport(viewporter, surf);
		wp_viewport_set_source(lv, 0, 0, wl_fixed_from_int(W), wl_fixed_from_int(H));
		wp_viewport_set_destination(lv, W, H);
		wl_surface_attach(root, black, 0, 0);
		wl_surface_damage_buffer(root, 0, 0, INT32_MAX, INT32_MAX);
	}

	struct timespec t0, now;
	clock_gettime(CLOCK_MONOTONIC, &t0);
	struct rect prev = square_at(0, sq);
	struct sbuf *last = NULL;
	for (unsigned tick = 0;; tick++) {
		clock_gettime(CLOCK_MONOTONIC, &now);
		if (closed || now.tv_sec - t0.tv_sec >= secs) break;
		struct sbuf *b = NULL;
		for (;;) {
			for (int i = 0; i < nbuf && !b; i++)
				if (!bufs[(tick + i) % nbuf].busy) b = &bufs[(tick + i) % nbuf];
			if (b) break;
			release_waits++;
			if (wl_display_dispatch(dpy) < 0) return 1;
		}
		struct rect cur = square_at(tick, sq);
		/* The first frame damages everything; afterwards only the square. */
		struct rect dmg = tick == 0 ? full : r_union(prev, cur);
		for (int i = 0; i < nbuf; i++) bufs[i].owed = r_union(bufs[i].owed, dmg);
		paint_rect(b->px, b->owed, cur, alpha);
		b->owed.x1 = b->owed.x0;
		prev = cur;
		/* KWin asks for the frame callback on its main surface. */
		struct wl_callback *cb = wl_surface_frame(root);
		wl_callback_add_listener(cb, &frame_l, NULL);
		frame_pending = 1;
		b->busy = 1;
		wl_surface_attach(surf, b->wl, 0, 0);
		wl_surface_damage_buffer(surf, dmg.x0, dmg.y0, dmg.x1 - dmg.x0, dmg.y1 - dmg.y0);
		wl_surface_commit(surf);
		if (surf != root) wl_surface_commit(root);
		frames++;
		last = b;
		while (frame_pending && wl_display_dispatch(dpy) >= 0) {}
	}
	/* WC_BENCH_FINAL=path: the last committed buffer as a PPM, as it looks
	 * over black (premultiplied, so its channels are the colour), for the
	 * harness to compare with the compositor's last frame. */
	const char *final = getenv("WC_BENCH_FINAL");
	if (final && last) {
		FILE *f = fopen(final, "wb");
		if (f) {
			fprintf(f, "P6\n%d %d\n255\n", W, H);
			for (int i = 0; i < W * H; i++) {
				uint32_t p = last->px[i];
				unsigned char rgb[3] = { (unsigned char)(p >> 16), (unsigned char)(p >> 8), (unsigned char)p };
				fwrite(rgb, 1, 3, f);
			}
			fclose(f);
		}
		usleep(500 * 1000); /* the compositor's dump of that frame */
	}
	printf("frames %u\n", frames);
	printf("released %u\n", released);
	printf("release_waits %u\n", release_waits);
	xdg_toplevel_destroy(top);
	xdg_surface_destroy(xs);
	if (surf != root) wl_surface_destroy(surf);
	wl_surface_destroy(root);
	for (int i = 0; i < nbuf; i++) {
		wl_buffer_destroy(bufs[i].wl);
		munmap(bufs[i].px, size);
		unlink(bufs[i].path);
	}
	wl_display_roundtrip(dpy);
	wl_display_disconnect(dpy);
	return 0;
}

int main(int argc, char **argv) {
	int secs = argc > 1 ? atoi(argv[1]) : 5;
	if (argc > 3) { W = atoi(argv[2]); H = atoi(argv[3]); }
	int is_bench = argc > 4 && !strcmp(argv[4], "bench");
	int nbuf = argc > 5 ? atoi(argv[5]) : 3;
	if (nbuf < 1 || nbuf > MAXBUF) nbuf = 3;
	uint32_t alpha = 255;
	if (argc > 6 && !strncmp(argv[6], "argb", 4)) fmt = 0x34325241u; /* 'AR24' */
	if (argc > 6 && !strcmp(argv[6], "argbt")) alpha = 0xc0;
	int sq = argc > 7 ? atoi(argv[7]) : 64;
	if (sq < 1 || sq > W || sq > H) sq = W < H ? W : H;
	int kwin = argc > 8 && !strcmp(argv[8], "kwin");
	setvbuf(stdout, NULL, _IOLBF, 0);
	srand((unsigned)getpid() ^ (unsigned)time(NULL));

	struct wl_display *dpy = wl_display_connect(NULL);
	if (!dpy) { fprintf(stderr, "no display\n"); return 1; }
	struct wl_registry *reg = wl_display_get_registry(dpy);
	wl_registry_add_listener(reg, &reg_l, NULL);
	wl_display_roundtrip(dpy);
	if (!compositor || !wm) { fprintf(stderr, "no wl_compositor/xdg_wm_base\n"); return 1; }
	if (!dmabuf) { printf("no zwp_linux_dmabuf_v1 v4\n"); return 2; }
	xdg_wm_base_add_listener(wm, &wm_l, NULL);

	struct zwp_linux_dmabuf_feedback_v1 *fb = zwp_linux_dmabuf_v1_get_default_feedback(dmabuf);
	zwp_linux_dmabuf_feedback_v1_add_listener(fb, &fb_l, NULL);
	while (!fb_done && wl_display_roundtrip(dpy) >= 0) {}
	printf("feedback main_device=%u:%u table=%u formats=", major(main_dev), minor(main_dev), table_n);
	for (unsigned i = 0; i < table_n && i < 16; i++) printf("%s0x%08x", i ? "," : "", table_fmt[i]);
	printf("\n");
	zwp_linux_dmabuf_feedback_v1_destroy(fb);
	if (is_bench) return bench(dpy, secs, nbuf, sq, kwin, alpha);

	/* The shared allocation, as the GPU side would make it locally. */
	char path[128];
	snprintf(path, sizeof path, "/dev/shm/sv-dmabuf-test-%d", (int)getpid());
	size_t size = (size_t)W * H * 4;
	int shm = open(path, O_RDWR | O_CREAT | O_EXCL, 0600);
	if (shm < 0 || ftruncate(shm, (off_t)size) != 0) { perror(path); return 1; }
	px = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, shm, 0);
	if (px == MAP_FAILED) { perror("mmap"); return 1; }
	paint(0);
	int tok = make_token(path, size);
	if (tok < 0) { perror("token"); unlink(path); return 1; }
	printf("token name=%s size=%zu\n", path, size);

	struct zwp_linux_buffer_params_v1 *params = zwp_linux_dmabuf_v1_create_params(dmabuf);
	zwp_linux_buffer_params_v1_add_listener(params, &p_l, NULL);
	zwp_linux_buffer_params_v1_add(params, tok, 0, 0, (uint32_t)W * 4, 0, 0);
	close(tok);
	struct wl_buffer *buf = zwp_linux_buffer_params_v1_create_immed(params, W, H, FMT_XRGB8888, 0);
	zwp_linux_buffer_params_v1_destroy(params);
	wl_display_roundtrip(dpy);
	if (failed) { printf("buffer failed\n"); unlink(path); return 1; }
	printf("buffer created\n");

	struct wl_surface *surf = wl_compositor_create_surface(compositor);
	struct xdg_surface *xs = xdg_wm_base_get_xdg_surface(wm, surf);
	xdg_surface_add_listener(xs, &xs_l, NULL);
	struct xdg_toplevel *top = xdg_surface_get_toplevel(xs);
	xdg_toplevel_add_listener(top, &top_l, NULL);
	xdg_toplevel_set_title(top, "wc-dmabuf-client");
	wl_surface_commit(surf);
	while (!configured && wl_display_dispatch(dpy) >= 0) {}

	struct timespec t0, now;
	clock_gettime(CLOCK_MONOTONIC, &t0);
	for (unsigned tick = 0;; tick++) {
		clock_gettime(CLOCK_MONOTONIC, &now);
		if (closed || now.tv_sec - t0.tv_sec >= secs) break;
		/* Written into the shared file: the compositor sees it by name. */
		paint(tick);
		struct wl_callback *cb = wl_surface_frame(surf);
		wl_callback_add_listener(cb, &frame_l, NULL);
		frame_pending = 1;
		wl_surface_attach(surf, buf, 0, 0);
		wl_surface_damage_buffer(surf, 0, 0, W, H);
		wl_surface_commit(surf);
		frames++;
		while (frame_pending && wl_display_dispatch(dpy) >= 0) {}
	}
	/* Leave a still picture behind for the frame dump: the bar at the top. */
	paint(0);
	wl_surface_attach(surf, buf, 0, 0);
	wl_surface_damage_buffer(surf, 0, 0, W, H);
	wl_surface_commit(surf);
	wl_display_roundtrip(dpy);
	usleep(500 * 1000);
	printf("frames %u\n", frames);

	wl_buffer_destroy(buf);
	xdg_toplevel_destroy(top);
	xdg_surface_destroy(xs);
	wl_surface_destroy(surf);
	wl_display_roundtrip(dpy);
	wl_display_disconnect(dpy);
	munmap(px, size);
	unlink(path);
	return 0;
}
