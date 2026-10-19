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

#define FMT_XRGB8888 0x34325258u /* 'XR24' */

static struct wl_compositor *compositor;
static struct xdg_wm_base *wm;
static struct zwp_linux_dmabuf_v1 *dmabuf;
static int configured, failed, closed;
static uint32_t *px;
static int W = 320, H = 200;
static unsigned frames;
static int frame_pending;

static void reg_global(void *d, struct wl_registry *r, uint32_t name, const char *iface, uint32_t v) {
	(void)d;
	if (!strcmp(iface, "wl_compositor"))
		compositor = wl_registry_bind(r, name, &wl_compositor_interface, v < 4 ? v : 4);
	else if (!strcmp(iface, "xdg_wm_base"))
		wm = wl_registry_bind(r, name, &xdg_wm_base_interface, 1);
	else if (!strcmp(iface, "zwp_linux_dmabuf_v1") && v >= 4)
		dmabuf = wl_registry_bind(r, name, &zwp_linux_dmabuf_v1_interface, 4);
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

/* WC_DMABUF_FENCE=1 (§104.11): a version-2 token whose sync page carries
 * the number of the "GPU work" that writes each frame, and a fence record
 * this client completes itself, a few milliseconds *after* it committed the
 * frame and only then draws it -- as a GPU would. The compositor shows the
 * right frame only if it waits on the record before reading. */
static int fence;
static volatile uint64_t *sync_seq, *rec_done;
static uint64_t seq;
static char rec_path[128];

/* sv_drm::token: magic, version, hdr_len, size, nonce, name_len, name
 * [, rec_len, record]; all LE. */
static int make_token(const char *name, uint64_t size, const char *rec) {
	int fd = memfd_create("sv-dmabuf", MFD_CLOEXEC | MFD_ALLOW_SEALING);
	if (fd < 0) return -1;
	uint32_t nlen = (uint32_t)strlen(name), rlen = rec ? (uint32_t)strlen(rec) : 0;
	uint32_t hdr = 8 + 4 + 4 + 8 + 16 + 4 + nlen + (rec ? 4 + rlen : 0), ver = rec ? 2 : 1;
	unsigned char b[512];
	size_t o = 0;
	memcpy(b + o, "SVDMABF1", 8); o += 8;
	memcpy(b + o, &ver, 4); o += 4;
	memcpy(b + o, &hdr, 4); o += 4;
	memcpy(b + o, &size, 8); o += 8;
	for (int i = 0; i < 16; i++) b[o++] = (unsigned char)(rand() & 0xff);
	memcpy(b + o, &nlen, 4); o += 4;
	memcpy(b + o, name, nlen); o += nlen;
	if (rec) { memcpy(b + o, &rlen, 4); o += 4; memcpy(b + o, rec, rlen); o += rlen; }
	if (pwrite(fd, b, o, 0) != (ssize_t)o) return -1;
	uint64_t len = size > o ? size : o;
	if (rec && len < 8192) len = 8192;
	if (ftruncate(fd, (off_t)len) != 0) return -1;
	if (rec) {
		unsigned char *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 4096);
		if (p == MAP_FAILED) return -1;
		memcpy(p, "SVSYNC01", 8);
		sync_seq = (volatile uint64_t *)(p + 8);
	}
	int seals = F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL | (rec ? 0x0010 /* F_SEAL_FUTURE_WRITE */ : F_SEAL_WRITE);
	if (fcntl(fd, F_ADD_SEALS, seals) != 0) return -1;
	return fd;
}

/* The "GPU": the commit is already on its way; 15 ms later the frame is
 * drawn and only then is its work number completed in the record. */
static void gpu_frame(struct wl_display *dpy, unsigned tick) {
	wl_display_flush(dpy);
	usleep(15 * 1000);
	paint(tick);
	__atomic_store_n(rec_done, seq, __ATOMIC_RELEASE);
}

int main(int argc, char **argv) {
	int secs = argc > 1 ? atoi(argv[1]) : 5;
	if (argc > 3) { W = atoi(argv[2]); H = atoi(argv[3]); }
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

	/* The shared allocation, as the GPU side would make it locally. */
	char path[128];
	snprintf(path, sizeof path, "/dev/shm/sv-dmabuf-test-%d", (int)getpid());
	size_t size = (size_t)W * H * 4;
	int shm = open(path, O_RDWR | O_CREAT | O_EXCL, 0600);
	if (shm < 0 || ftruncate(shm, (off_t)size) != 0) { perror(path); return 1; }
	px = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, shm, 0);
	if (px == MAP_FAILED) { perror("mmap"); return 1; }
	paint(0);
	fence = getenv("WC_DMABUF_FENCE") && atoi(getenv("WC_DMABUF_FENCE"));
	if (fence) {
		snprintf(rec_path, sizeof rec_path, "/dev/shm/sv-fence-%d-0-%08x%08x", (int)getpid(), (unsigned)rand(), (unsigned)rand());
		int rf = open(rec_path, O_RDWR | O_CREAT | O_EXCL, 0600);
		if (rf < 0 || ftruncate(rf, 4096) != 0) { perror(rec_path); return 1; }
		unsigned char *r = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, rf, 0);
		if (r == MAP_FAILED) { perror("mmap record"); return 1; }
		memcpy(r, "SVFENCE1", 8);
		rec_done = (volatile uint64_t *)(r + 8);
		close(rf);
		printf("record %s\n", rec_path);
	}
	int tok = make_token(path, size, fence ? rec_path : NULL);
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
		if (!fence) paint(tick);
		else __atomic_store_n(sync_seq, ++seq, __ATOMIC_RELEASE);
		struct wl_callback *cb = wl_surface_frame(surf);
		wl_callback_add_listener(cb, &frame_l, NULL);
		frame_pending = 1;
		wl_surface_attach(surf, buf, 0, 0);
		wl_surface_damage_buffer(surf, 0, 0, W, H);
		wl_surface_commit(surf);
		frames++;
		if (fence) gpu_frame(dpy, tick);
		while (frame_pending && wl_display_dispatch(dpy) >= 0) {}
	}
	/* Leave a still picture behind for the frame dump: the bar at the top. */
	if (!fence) paint(0);
	else __atomic_store_n(sync_seq, ++seq, __ATOMIC_RELEASE);
	wl_surface_attach(surf, buf, 0, 0);
	wl_surface_damage_buffer(surf, 0, 0, W, H);
	wl_surface_commit(surf);
	if (fence) gpu_frame(dpy, 0);
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
	if (fence) unlink(rec_path);
	return 0;
}
