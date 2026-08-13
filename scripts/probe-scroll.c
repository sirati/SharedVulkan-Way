/* Scroll a compositor window, for a long time, the way a user does.
 *
 *   probe-scroll.exe --class WaycomposeToplevel --wait 40 --notches 60 --gap 250
 *
 * §27. `probe-input.exe` injects one wheel notch, because the question it
 * was written for is "does a notch arrive, and with the right sign" — a
 * question one notch answers completely. This probe asks a different one: what
 * a *scrolling page* does to the damage path over sixty frames' worth of
 * changing rectangles, which is the shape of the bug in §27 of
 * docs/waycompose-design.md and which one notch cannot show. Separate file
 * rather than a flag on
 * `probe-input.c` for the reason §16.8 gives about shared guests: ten agents
 * are editing this tree, and a probe with its own name cannot collide with
 * somebody else's edit to a probe with theirs.
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
	SendInput(1, &in, sizeof in);
}

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
	SendInput(1, &in, sizeof in);
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
		printf("probe-scroll: no visible window of class '%ls' after %d s\n",
		       cls, wait);
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
	printf("probe-scroll: %d notches towards the user, %d ms apart\n", notches, gap);
	Sleep(1000);
	return 0;
}
