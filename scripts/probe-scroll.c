/* Scroll a compositor window, for a long time, the way a user does.
 *
 *   probe-scroll.exe --class WaycomposeToplevel --wait 40 --notches 60 --gap 250
 *
 * §27, with §57's refusal counting. `probe-input.exe` injects one wheel
 * notch, because the question it was written for is "does a notch arrive, and
 * with the right sign" -- a question one notch answers completely. This probe
 * asks a different one: what a *scrolling page* does to the damage path over
 * sixty frames' worth of changing rectangles, which is the shape of the bug in
 * §27 of docs/waycompose-design.md and which one notch cannot show. Separate
 * file rather than a flag on `probe-input.c` for the reason §16.8 gives about
 * shared guests: ten agents are editing this tree, and a probe with its own
 * name cannot collide with somebody else's edit to a probe with theirs.
 *
 * `SendInput` and not `PostMessage`, for exactly the reason `probe-input.c`
 * spells out at length: Win32's mouse tracking follows the real cursor, so a
 * posted `WM_MOUSEWHEEL` is delivered against coordinates the compositor is
 * right to disbelieve. This probe therefore refuses to have a `--post` mode.
 *
 * The gap between notches is not cosmetic. A burst of sixty notches with no
 * pause is coalesced by the client into a handful of repaints, which is the
 * opposite of the load this exists to produce: the bug needs *many commits with
 * different damage*, so the default gap is a quarter of a second, comfortably
 * longer than a frame and short enough that sixty of them fit in fifteen
 * seconds.
 */
#include <windows.h>
#include <stdio.h>
#include <string.h>

/* Why this enumerates instead of calling FindWindowW.
 *
 * `FindWindowW(cls, NULL)` returns the FIRST window of the class in Z-order,
 * and the compositor owns more than one: a toplevel it shows, and hidden
 * helper/owner windows of the same class. The probe took the first, found it
 * hidden, and waited out its whole timeout reporting "exists but never became
 * visible" while the real toplevel was up with 50 frames blitted into it. That
 * sent one run chasing a Windows session boundary that was never the problem.
 *
 * So: walk every top-level window, match the class, and take the first that is
 * actually VISIBLE and has a non-empty client rect. `found_any` is kept so the
 * failure can still distinguish "no window of this class at all" from "they are
 * all hidden", which is the distinction that made this diagnosable. */
struct find_ctx {
	const wchar_t *cls;
	HWND hit;
	int found_any;
};

static BOOL CALLBACK find_cb(HWND h, LPARAM lp)
{
	struct find_ctx *c = (struct find_ctx *)lp;
	wchar_t buf[128];

	if (!GetClassNameW(h, buf, 128) || wcscmp(buf, c->cls) != 0)
		return TRUE;
	c->found_any = 1;
	if (IsWindowVisible(h)) {
		RECT r;
		if (GetClientRect(h, &r) && r.right > 1 && r.bottom > 1) {
			c->hit = h;
			return FALSE;	/* stop: this is the one */
		}
	}
	return TRUE;
}

static int g_found_hidden;	/* the class exists here; every one was hidden */

static HWND find_target(const wchar_t *cls, int seconds)
{
	g_found_hidden = 0;
	for (int i = 0; i < seconds * 10; i++) {
		struct find_ctx c = { cls, NULL, 0 };
		EnumWindows(find_cb, (LPARAM)&c);
		if (c.hit)
			return c.hit;
		if (c.found_any)
			g_found_hidden = 1;
		Sleep(100);
	}
	return NULL;
}

static void move_to(int x, int y)
{
	INPUT in;
	memset(&in, 0, sizeof in);
	in.type = INPUT_MOUSE;
	/* Absolute coordinates are in a 0..65535 space over the virtual screen,
	 * which is what `MOUSEEVENTF_ABSOLUTE` means and is the one part of
	 * this API that silently does nothing if it is left in pixels. */
	in.mi.dx = (LONG)((x * 65535) / GetSystemMetrics(SM_CXSCREEN));
	in.mi.dy = (LONG)((y * 65535) / GetSystemMetrics(SM_CYSCREEN));
	in.mi.dwFlags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE;
	if (SendInput(1, &in, sizeof in) != 1)
		printf("probe-scroll: WARNING the pointer move was refused (%lu); "
		       "if the session is locked, SendInput cannot reach it\n",
		       (unsigned long)GetLastError());
}

/* How many notches `SendInput` refused. §57.
 *
 * **`SendInput` returns the number of events it inserted, and this probe used
 * to discard it.** On a Windows session that is *locked*, the input desktop is
 * `Winlogon` rather than `Default`, and a process on `Default` cannot inject
 * into it: `SendInput` returns 0 and sets `ERROR_ACCESS_DENIED`. It does not
 * block, does not fail loudly, and does not tell the caller anything unless the
 * caller looks.
 *
 * The consequence is the worst kind. This probe would print
 * `60 notches towards the user`, the harness would see its completion line and
 * pass that check, and the run would go on to measure a browser nobody
 * scrolled. Every downstream number would be real and about the wrong thing --
 * the §27.6 shape again, an experiment that did not contain what it was about.
 *
 * Counted rather than aborted on, because a partial refusal is worth seeing:
 * "0 of 60 accepted" is a locked session and "58 of 60" is contention. */
static int refused;

/* Negative notches roll *towards* the user, which is what scrolls a page down.
 * `probe-input.c` documents the sign in the other direction and this is the
 * same convention seen from the other end: Windows counts away-from-user as
 * +WHEEL_DELTA, Wayland's vertical axis is positive downwards, so a page moves
 * down when the notch count is negative. */
static void wheel(int notches)
{
	INPUT in;
	memset(&in, 0, sizeof in);
	in.type = INPUT_MOUSE;
	in.mi.dwFlags = MOUSEEVENTF_WHEEL;
	in.mi.mouseData = (DWORD)(notches * WHEEL_DELTA);
	if (SendInput(1, &in, sizeof in) != 1)
		refused++;
}

int main(int argc, char **argv)
{
	wchar_t cls[128] = L"WaycomposeToplevel";
	int wait = 40, notches = 60, gap = 250, settle = 3000;

	for (int i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "--log") && i + 1 < argc) {
			/* A program linked `-mwindows` has no console, so
			 * `probe-input.c`'s trick: reopen stdout on a file the
			 * harness names. Without it the run is silent and a
			 * failure is indistinguishable from a hang. */
			if (!freopen(argv[++i], "w", stdout))
				return 2;
			setvbuf(stdout, NULL, _IONBF, 0);
		} else if (!strcmp(argv[i], "--class") && i + 1 < argc) {
			MultiByteToWideChar(CP_ACP, 0, argv[++i], -1, cls, 128);
		} else if (!strcmp(argv[i], "--wait") && i + 1 < argc) {
			wait = atoi(argv[++i]);
		} else if (!strcmp(argv[i], "--notches") && i + 1 < argc) {
			notches = atoi(argv[++i]);
		} else if (!strcmp(argv[i], "--gap") && i + 1 < argc) {
			gap = atoi(argv[++i]);
		} else if (!strcmp(argv[i], "--settle") && i + 1 < argc) {
			settle = atoi(argv[++i]);
		} else {
			printf("probe-scroll: unknown argument %s\n", argv[i]);
			return 2;
		}
	}

	HWND target = find_target(cls, wait);
	if (!target) {
		if (g_found_hidden)
			printf("probe-scroll: a window of class '%ls' EXISTS in this desktop but never became visible after %d s -- the compositor made it and did not show it, or it is on a station with no visible desktop\n", cls, wait);
		else
			printf("probe-scroll: NO window of class '%ls' exists in this desktop at all after %d s -- wrong session/window station, or the compositor never created one\n", cls, wait);
		return 1;
	}
	RECT rc;
	GetClientRect(target, &rc);
	printf("probe-scroll: target HWND %p, client %ldx%ld\n",
	       (void *)target, (long)rc.right, (long)rc.bottom);

	DWORD self = GetCurrentThreadId();
	DWORD owner = GetWindowThreadProcessId(target, NULL);
	BOOL attached = AttachThreadInput(self, owner, TRUE);
	BringWindowToTop(target);
	SetForegroundWindow(target);
	SetActiveWindow(target);
	SetFocus(target);
	Sleep(300);

	HWND fg = GetForegroundWindow();
	printf("probe-scroll: attached=%d foreground=%p target=%p\n",
	       (int)attached, (void *)fg, (void *)target);
	if (fg != target)
		printf("probe-scroll: WARNING the target is not the foreground "
		       "window; the wheel will go elsewhere\n");

	/* The cursor has to be over the window: `WM_MOUSEWHEEL` goes to the
	 * focused window on Win32, but the *client* routes the axis event by
	 * the pointer's surface, so a wheel with the pointer outside scrolls
	 * nothing and looks exactly like a broken axis path. */
	POINT centre = { rc.right / 2, rc.bottom / 2 };
	ClientToScreen(target, &centre);
	move_to(centre.x, centre.y);
	Sleep(200);

	/* Let the page finish its first layout. Scrolling a document that is
	 * still being laid out produces repaints that are not scroll repaints,
	 * and the point is to measure the scrolling ones. */
	Sleep(settle);

	for (int i = 0; i < notches; i++) {
		wheel(-1);
		Sleep(gap);
	}
	/* The completion line still contains "notches towards the user", because
	 * two harnesses grep for exactly that to tell a finished scroll from a
	 * probe that died. What is new is the count in front of it: a harness
	 * that only looks for the phrase still works, and one that reads the
	 * numbers can tell a scroll from a refusal. */
	int accepted = notches - refused;
	printf("probe-scroll: %d of %d notches towards the user accepted, %d ms apart\n",
	       accepted, notches, gap);
	if (refused) {
		printf("probe-scroll: WARNING %d notch(es) were REFUSED by SendInput "
		       "(last error %lu)\n", refused, (unsigned long)GetLastError());
		printf("probe-scroll: a LOCKED Windows session puts the input desktop on "
		       "Winlogon, where a process on Default cannot inject. Unlock the "
		       "session and re-run; nothing else about this run is wrong.\n");
	}
	if (accepted == 0) {
		/* **A distinct exit code, because this is not "the probe failed".**
		 * The probe worked perfectly and Windows declined. A run that
		 * treated this as a crash would look for the wrong cause, and a
		 * run that treated it as success would measure an unscrolled
		 * browser -- which is the failure this whole return value exists
		 * to prevent. */
		printf("probe-scroll: nothing was injected; this run scrolled nothing\n");
		Sleep(500);
		return 3;
	}
	Sleep(1000);
	return 0;
}
