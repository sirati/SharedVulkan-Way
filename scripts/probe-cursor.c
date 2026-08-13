/* Say what the mouse cursor actually is, over the compositor's window.
 *
 * Stage 12 claims that a client's `wl_pointer.set_cursor` and its
 * `wp_cursor_shape_v1.set_shape` reach Win32 as a real `HCURSOR`
 * (docs/design.md §23). A screenshot cannot settle that: a cursor is drawn by
 * the window server above every window, so it is usually *absent* from a
 * screen capture, and where it is present it is a handful of antialiased
 * pixels that a human has to squint at. `GetCursorInfo` returns the handle
 * itself, and a handle can be compared.
 *
 *   x86_64-w64-mingw32-gcc -O1 -mwindows -o probe-cursor.exe probe-cursor.c \
 *       -luser32 -lgdi32
 *   wine probe-cursor.exe --class WaycomposeToplevel --log cursor.log
 *
 * Follows the convention of the other probes here (probe-input.c,
 * probe-window.c): one translation unit, linked by hand, no cargo.
 *
 * WHAT IT MEASURES, AND WHY EACH PART IS NECESSARY
 *
 * 1. It moves the pointer into the client area with `SendInput`, for the
 *    reason probe-input.c gives at length: a posted `WM_MOUSEMOVE` does not
 *    move the cursor, and Win32's own tracking follows the real one. Here it
 *    is worse than inaccurate, it is the entire experiment — `WM_SETCURSOR` is
 *    only sent because the pointer is somewhere, and the compositor only sends
 *    `wl_pointer.enter` because the pointer arrived.
 *
 * 2. It reads `GetCursorInfo().hCursor` and compares it against every stock
 *    cursor `LoadCursorW(NULL, IDC_*)` returns. Stock cursors are shared
 *    per-session handles, so an exact match is proof of identity rather than
 *    resemblance: "this is IDC_IBEAM" and not "this looks like an I-beam".
 *
 * 3. If it matches none, the cursor was built by the process under test, and
 *    `GetIconInfo` says so with numbers — `fIcon == 0`, a hotspot, and two
 *    bitmaps whose dimensions are the client's cursor size. That is the
 *    `wl_pointer.set_cursor` buffer path, and the hotspot is the part that
 *    would be silently wrong if the surface-to-buffer arithmetic were.
 *
 * WHY IT ALSO REPORTS THE NON-CLIENT AREA
 *
 * The invariant the user asked for is *one* cursor, and the other half of it
 * is that the window frame keeps the system's own: a resize arrow on the
 * border, an arrow over the caption. `WM_SETCURSOR` is claimed only for
 * `HTCLIENT`, and `--edge` checks that by parking the pointer on the window's
 * right border and reading the cursor there. A run where the client area and
 * the border report the same handle is a run where the hit-test guard was
 * dropped.
 *
 * WHAT IT CANNOT SAY
 *
 * Whether the cursor is *drawn* correctly. `GetCursorInfo` reports what the
 * window server was told to show; the pixels that reach the screen are the
 * window server's business, and under Wine that window server is Wine
 * (docs/design.md §13.6 — four lies so far, and cursors are exactly the kind
 * of thing it emulates rather than implements). A pass here means the
 * compositor installed the right handle. Whether Windows then draws it is a
 * real-guest question.
 */
#include <windows.h>
#include <stdio.h>
#include <string.h>

/* The stock cursors, by ordinal. Written out rather than using the `IDC_*`
 * macros so that the numbers in this file and the numbers in
 * `crates/wc-compositor/src/cursor.rs` are two independent transcriptions of
 * `winuser.h`: agreement between them is evidence, and a shared typo needs two
 * people to make it. */
struct stock {
	const char *name;
	WORD idc;
};

static const struct stock STOCK[] = {
	{ "IDC_ARROW",       32512 },
	{ "IDC_IBEAM",       32513 },
	{ "IDC_WAIT",        32514 },
	{ "IDC_CROSS",       32515 },
	{ "IDC_UPARROW",     32516 },
	{ "IDC_SIZE",        32640 },
	{ "IDC_ICON",        32641 },
	{ "IDC_SIZENWSE",    32642 },
	{ "IDC_SIZENESW",    32643 },
	{ "IDC_SIZEWE",      32644 },
	{ "IDC_SIZENS",      32645 },
	{ "IDC_SIZEALL",     32646 },
	{ "IDC_NO",          32648 },
	{ "IDC_HAND",        32649 },
	{ "IDC_APPSTARTING", 32650 },
	{ "IDC_HELP",        32651 },
	{ NULL, 0 },
};

static const char *stock_name(HCURSOR h)
{
	if (!h)
		return "(none: the pointer is hidden)";
	for (int i = 0; STOCK[i].name; i++)
		if (LoadCursorW(NULL, MAKEINTRESOURCEW(STOCK[i].idc)) == h)
			return STOCK[i].name;
	return NULL;
}

/* Absolute mouse coordinates are normalised over the whole virtual screen in
 * units of 1/65535 — not pixels, and the divisor is the *virtual* screen. The
 * same note probe-input.c carries, because it is the same trap. */
static void move_to(int x, int y)
{
	int w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
	int h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
	if (w <= 0) w = GetSystemMetrics(SM_CXSCREEN);
	if (h <= 0) h = GetSystemMetrics(SM_CYSCREEN);

	INPUT in;
	memset(&in, 0, sizeof in);
	in.type = INPUT_MOUSE;
	in.mi.dx = (LONG)((double)x * 65535.0 / (double)(w - 1));
	in.mi.dy = (LONG)((double)y * 65535.0 / (double)(h - 1));
	in.mi.dwFlags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE;
	SendInput(1, &in, sizeof in);
	Sleep(40);
}

/* Describe one cursor handle, whatever it came from. */
static void describe(const char *tag, HCURSOR h)
{
	if (!h) {
		printf("probe-cursor: %s cursor=hidden\n", tag);
		fflush(stdout);
		return;
	}

	const char *name = stock_name(h);
	if (name) {
		printf("probe-cursor: %s cursor=stock %s handle=%p\n",
		       tag, name, (void *)h);
		fflush(stdout);
		return;
	}

	/* Not one of the system's: somebody built it. Say what it is made of,
	 * because "not a stock cursor" on its own would also be the answer for
	 * a corrupt handle. */
	ICONINFO ii;
	memset(&ii, 0, sizeof ii);
	if (!GetIconInfo(h, &ii)) {
		printf("probe-cursor: %s cursor=custom handle=%p "
		       "(GetIconInfo failed, %lu)\n",
		       tag, (void *)h, (unsigned long)GetLastError());
		fflush(stdout);
		return;
	}

	BITMAP bm;
	int w = 0, ht = 0, argb = 0;
	/* The colour bitmap is the one that carries the size and the alpha. A
	 * cursor with no colour bitmap is the old two-bitmap monochrome kind,
	 * whose AND/XOR masks are stacked in one bitmap of double height — not
	 * something this compositor can produce, so its appearance would mean
	 * the handle did not come from us. */
	if (ii.hbmColor && GetObjectW(ii.hbmColor, sizeof bm, &bm)) {
		w = (int)bm.bmWidth;
		ht = (int)bm.bmHeight;
		argb = (bm.bmBitsPixel == 32);
	} else if (ii.hbmMask && GetObjectW(ii.hbmMask, sizeof bm, &bm)) {
		w = (int)bm.bmWidth;
		ht = (int)bm.bmHeight / 2;
	}

	printf("probe-cursor: %s cursor=custom handle=%p fIcon=%d "
	       "size=%dx%d hotspot=%lu,%lu argb=%d\n",
	       tag, (void *)h, (int)ii.fIcon, w, ht,
	       (unsigned long)ii.xHotspot, (unsigned long)ii.yHotspot, argb);
	fflush(stdout);

	if (ii.hbmColor) DeleteObject(ii.hbmColor);
	if (ii.hbmMask)  DeleteObject(ii.hbmMask);
}

/* Ask the window itself what it would install, without moving anything.
 *
 * **This is the assertion that does not need a pointer.** `WM_SETCURSOR` is
 * an ordinary message: `wParam` is the window, `LOWORD(lParam)` is the
 * hit-test code and `HIWORD` is the mouse message that provoked it. Sending
 * one by hand runs the compositor's window procedure exactly as a real mouse
 * move would, and the *return value* is the claim being tested — `TRUE` means
 * "I set the cursor, stop here", which the compositor must say for `HTCLIENT`
 * and must not say for the frame.
 *
 * Sending it also has the side effect of installing the cursor on the
 * receiving thread's queue, which `GetCursor` then reads back after
 * `AttachThreadInput` has put this thread on the same queue. That path never
 * asks the window server where the pointer is, which matters: it is the one
 * reading here that does not depend on Wine's own cursor tracking.
 */
static void ask_window(const char *tag, HWND w, int hittest)
{
	LRESULT r = SendMessageW(w, WM_SETCURSOR, (WPARAM)w,
				 MAKELPARAM(hittest, WM_MOUSEMOVE));
	printf("probe-cursor: %s WM_SETCURSOR(hittest=%d) returned %ld\n",
	       tag, hittest, (long)r);
	fflush(stdout);
	describe(tag, GetCursor());
}

/* Report the cursor at the current pointer position, prefixed with `tag` so a
 * shell script can grep for one probe point among several. */
static void report(const char *tag)
{
	CURSORINFO ci;
	memset(&ci, 0, sizeof ci);
	ci.cbSize = sizeof ci;
	if (!GetCursorInfo(&ci)) {
		printf("probe-cursor: %s GetCursorInfo failed (%lu)\n",
		       tag, (unsigned long)GetLastError());
		return;
	}

	/* `CURSOR_SHOWING` clear means the cursor is hidden, which is a result
	 * and not a failure: it is what `wl_pointer.set_cursor(NULL)` asks for,
	 * and telling it apart from "no cursor was installed" is the whole
	 * reason this flag is read rather than assumed. */
	if (!(ci.flags & CURSOR_SHOWING)) {
		printf("probe-cursor: %s cursor=hidden\n", tag);
		fflush(stdout);
		return;
	}
	describe(tag, ci.hCursor);
}

static HWND find_target(const wchar_t *cls, int seconds)
{
	for (int i = 0; i < seconds * 10; i++) {
		HWND h = FindWindowW(cls, NULL);
		if (h && IsWindowVisible(h))
			return h;
		Sleep(100);
	}
	return NULL;
}

int main(int argc, char **argv)
{
	wchar_t cls[128] = L"WaycomposeToplevel";
	int wait = 20, settle = 1200, edge = 1, dwell = 0;

	for (int i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "--log") && i + 1 < argc) {
			/* Inside the Wine virtual desktop a shell redirect on
			 * the outside catches the loader and nothing else. Same
			 * problem, same fix, as probe-input.c and the
			 * compositor's own `--log`. */
			if (!freopen(argv[++i], "w", stdout))
				return 3;
			setvbuf(stdout, NULL, _IONBF, 0);
		} else if (!strcmp(argv[i], "--class") && i + 1 < argc) {
			MultiByteToWideChar(CP_ACP, 0, argv[++i], -1, cls, 128);
		} else if (!strcmp(argv[i], "--wait") && i + 1 < argc) {
			wait = atoi(argv[++i]);
		} else if (!strcmp(argv[i], "--settle") && i + 1 < argc) {
			settle = atoi(argv[++i]);
		} else if (!strcmp(argv[i], "--dwell") && i + 1 < argc) {
			dwell = atoi(argv[++i]);
		} else if (!strcmp(argv[i], "--no-edge")) {
			edge = 0;
		} else {
			printf("probe-cursor: unknown argument %s\n", argv[i]);
			return 2;
		}
	}

	HWND w = find_target(cls, wait);
	if (!w) {
		printf("probe-cursor: no visible window of class '%ls' after %d s\n",
		       cls, wait);
		return 1;
	}

	RECT rc, wr;
	GetClientRect(w, &rc);
	GetWindowRect(w, &wr);
	printf("probe-cursor: target HWND %p, client %ldx%ld, window %ld,%ld %ldx%ld\n",
	       (void *)w, (long)rc.right, (long)rc.bottom,
	       (long)wr.left, (long)wr.top,
	       (long)(wr.right - wr.left), (long)(wr.bottom - wr.top));
	fflush(stdout);

	/* `AttachThreadInput` puts this thread on the target's input queue, which
	 * is what makes `GetCursor` below read the compositor's cursor rather
	 * than this process's. It is also what `SetForegroundWindow` needs: that
	 * call is refused for a process that is not already in front, and
	 * without attaching, the focus calls address a queue the target window
	 * is not in. Both facts were measured by probe-input.c. */
	DWORD self = GetCurrentThreadId();
	DWORD owner = GetWindowThreadProcessId(w, NULL);
	BOOL attached = AttachThreadInput(self, owner, TRUE);
	BringWindowToTop(w);
	SetForegroundWindow(w);
	SetActiveWindow(w);
	Sleep(200);
	printf("probe-cursor: attached=%d\n", (int)attached);
	fflush(stdout);

	/* Before: whatever is up with the pointer outside the window. Named
	 * `outside` rather than `default` because it is not a claim about the
	 * compositor at all — it is the baseline the readings below differ
	 * from. */
	report("outside");

	POINT centre = { rc.right / 2, rc.bottom / 2 };
	ClientToScreen(w, &centre);
	move_to(centre.x, centre.y);
	/* One more move, a few pixels along. A client learns where the pointer
	 * is from `wl_pointer.enter`/`motion` and decides its cursor from that,
	 * so the second move is what a client that reacts to position has to
	 * see before it can react. */
	move_to(centre.x + 9, centre.y + 7);
	/* And then wait: the client's `set_cursor` has to cross the proxy, be
	 * dispatched by the compositor's Wayland thread, cross the command
	 * queue and be applied by the UI thread. Every one of those is a
	 * different thread or a different process. */
	Sleep(settle);

	/* The message-based readings first, and deliberately so. They go
	 * through `SendMessage` and `GetCursor` — the wineserver, not the X
	 * server — whereas `GetCursorInfo` below goes through Wine's X11 driver
	 * and `XQueryPointer`, which is measurably the fragile one here: a run
	 * of this probe under wine-11.14/Xvfb died with a fatal
	 * `BadWindow (X_QueryPointer)` at exactly that call. Putting the sturdy
	 * assertions first means a repeat of that failure loses the weakest
	 * evidence rather than all of it. */
	ask_window("client-area-msg", w, HTCLIENT);
	/* `HTRIGHT` is the window's right resize border. The compositor must
	 * *not* claim this one, or the client's cursor replaces the system
	 * resize arrows on the frame. */
	ask_window("frame-msg", w, HTRIGHT);
	/* Put the client's cursor back, so the reading below is not measuring
	 * the frame reply that was just provoked. */
	ask_window("client-area-msg2", w, HTCLIENT);

	report("client-area");

	if (dwell > 0) {
		/* Stay still and read again. This is the one thing a moving
		 * probe cannot check: a cursor that is only correct while
		 * `WM_SETCURSOR` keeps arriving is a cursor that reverts the
		 * moment the user stops moving, and `set_cursor`'s immediate
		 * `SetCursor` is what stops that. */
		Sleep(dwell);
		report("client-area-still");
	}

	if (edge) {
		/* The right border of the frame, two pixels in from the outside
		 * edge and vertically centred, which is `HTRIGHT` on a
		 * `WS_OVERLAPPEDWINDOW`. Windows draws `IDC_SIZEWE` there and
		 * the compositor must not have taken it over. */
		move_to(wr.right - 2, (wr.top + wr.bottom) / 2);
		Sleep(250);
		report("frame-right-edge");

		/* And the caption, which is `HTCAPTION`. `GetSystemMetrics`
		 * rather than a constant: the caption height is a theme
		 * setting. */
		int cap = GetSystemMetrics(SM_CYCAPTION) + GetSystemMetrics(SM_CYFRAME);
		move_to((wr.left + wr.right) / 2, wr.top + cap / 2);
		Sleep(250);
		report("frame-caption");
	}

	printf("probe-cursor: done\n");
	fflush(stdout);
	return 0;
}
