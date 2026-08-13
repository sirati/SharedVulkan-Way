/* Synthesise real Win32 input into the compositor's window.
 *
 * The stage-6 harness needs keystrokes and mouse events that arrive the way a
 * user's do — through the desktop's input queue, into the thread's message
 * queue, out of `GetMessage` as `WM_KEYDOWN` with a scan code in `lParam`.
 * Nothing in the compositor can produce those for itself, and a test that
 * called the compositor's own translation function directly would be testing
 * the function rather than the path.
 *
 * Follows the convention of the other probes here (probe-window.c,
 * probe-xkb.c): one translation unit, linked by hand, no cargo.
 *
 *   x86_64-w64-mingw32-gcc -O1 -o probe-input.exe probe-input.c -luser32
 *   wine probe-input.exe --class WaycomposeToplevel --type hello
 *
 * WHY SendInput AND NOT PostMessage
 *
 * `PostMessage(hwnd, WM_KEYDOWN, ...)` puts a message in the queue directly and
 * would exercise the window procedure — but it would bypass everything between
 * the desktop and the window, which is exactly the part this stage claims to
 * have got right: which window has the focus, whether `WM_SETFOCUS` was
 * delivered, whether `SetCapture` keeps a drag alive, whether `TrackMouseEvent`
 * produces `WM_MOUSELEAVE`. `SendInput` goes in at the bottom, so a run that
 * passes has exercised the focus and hit-testing machinery too.
 *
 * `--post` is kept as a fallback because Wine's `SendInput` has been unreliable
 * before, and a harness that could not tell "our code is wrong" from "the
 * injection did not arrive" would be no use. If the two modes disagree, the
 * difference is Wine's, not the compositor's.
 *
 * WHY SCAN CODES AND NOT VIRTUAL KEYS
 *
 * `KEYEVENTF_SCANCODE` injects the code the hardware would have produced, which
 * is what the compositor translates (see `crates/wc-compositor/src/input.rs`).
 * Injecting virtual keys would make the test depend on the injecting process's
 * keyboard layout, so a machine set to a German layout would silently be
 * testing something else.
 */
#include <windows.h>
#include <stdio.h>
#include <string.h>

/* Set-1 scan codes for the letters this probe can type. Positional, so this
 * table says where a key is and not what is printed on it — the same claim the
 * compositor's table makes, written independently here so that a shared
 * mistake needs two people to make it. */
struct key {
	char ch;
	WORD scan;
};

static const struct key KEYS[] = {
	{'a', 0x1E}, {'b', 0x30}, {'c', 0x2E}, {'d', 0x20}, {'e', 0x12},
	{'f', 0x21}, {'g', 0x22}, {'h', 0x23}, {'i', 0x17}, {'j', 0x24},
	{'k', 0x25}, {'l', 0x26}, {'m', 0x32}, {'n', 0x31}, {'o', 0x18},
	{'p', 0x19}, {'q', 0x10}, {'r', 0x13}, {'s', 0x1F}, {'t', 0x14},
	{'u', 0x16}, {'v', 0x2F}, {'w', 0x11}, {'x', 0x2D}, {'y', 0x15},
	{'z', 0x2C},
	{'1', 0x02}, {'2', 0x03}, {'3', 0x04}, {'4', 0x05}, {'5', 0x06},
	{'6', 0x07}, {'7', 0x08}, {'8', 0x09}, {'9', 0x0A}, {'0', 0x0B},
	{' ', 0x39}, {'\n', 0x1C}, {'.', 0x34}, {'-', 0x0C},
	{0, 0},
};

static WORD scan_for(char c)
{
	for (int i = 0; KEYS[i].ch; i++)
		if (KEYS[i].ch == c)
			return KEYS[i].scan;
	return 0;
}

static int use_post;
static HWND target;

static void key_event(WORD scan, int down)
{
	if (use_post) {
		/* Rebuild the `lParam` Windows would have built: repeat count
		 * 1, the scan code at bits 16-23, and for a release the
		 * transition and previous-state bits. Getting this wrong is
		 * how a PostMessage-based test passes while the real path
		 * fails, so it is spelled out rather than left at zero. */
		LPARAM lp = 1 | ((LPARAM)scan << 16);
		if (!down)
			lp |= (1u << 30) | (1u << 31);
		PostMessageW(target, down ? WM_KEYDOWN : WM_KEYUP,
			     MapVirtualKeyW(scan, MAPVK_VSC_TO_VK), lp);
		return;
	}
	INPUT in;
	memset(&in, 0, sizeof in);
	in.type = INPUT_KEYBOARD;
	in.ki.wScan = scan;
	in.ki.dwFlags = KEYEVENTF_SCANCODE | (down ? 0 : KEYEVENTF_KEYUP);
	SendInput(1, &in, sizeof in);
}

/* Left shift. Its own constant because it is used as a *modifier* here and not
 * as a character: an upper-case letter in the string is typed as shift-down,
 * key, key-up, shift-up — four events, which is what a user's keyboard sends
 * and what the compositor has to turn into `wl_keyboard.modifiers` around a
 * `key`. Injecting the upper-case character directly would be injecting a
 * decision the keymap is supposed to make. */
#define SCAN_LSHIFT 0x2A

static void type_string(const char *s)
{
	for (const char *p = s; *p; p++) {
		int upper = (*p >= 'A' && *p <= 'Z');
		WORD scan = scan_for(upper ? (char)(*p - 'A' + 'a') : *p);
		if (!scan) {
			printf("probe-input: no scan code for '%c', skipped\n", *p);
			continue;
		}
		if (upper) {
			key_event(SCAN_LSHIFT, 1);
			Sleep(30);
		}
		key_event(scan, 1);
		Sleep(30);
		key_event(scan, 0);
		Sleep(30);
		if (upper) {
			key_event(SCAN_LSHIFT, 0);
			Sleep(30);
		}
	}
	printf("probe-input: typed \"%s\"\n", s);
}

/* Absolute mouse coordinates are normalised over the whole virtual screen in
 * units of 1/65535, which is the part everyone gets wrong: they are not
 * pixels, and the divisor is the *virtual* screen rather than the primary
 * monitor. */
static void move_to(int x, int y)
{
	int w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
	int h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
	if (w <= 0) w = GetSystemMetrics(SM_CXSCREEN);
	if (h <= 0) h = GetSystemMetrics(SM_CYSCREEN);

	if (use_post) {
		POINT p = { x, y };
		ScreenToClient(target, &p);
		PostMessageW(target, WM_MOUSEMOVE, 0,
			     (LPARAM)((p.y << 16) | (p.x & 0xffff)));
		return;
	}
	INPUT in;
	memset(&in, 0, sizeof in);
	in.type = INPUT_MOUSE;
	in.mi.dx = (LONG)((double)x * 65535.0 / (double)(w - 1));
	in.mi.dy = (LONG)((double)y * 65535.0 / (double)(h - 1));
	in.mi.dwFlags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE;
	SendInput(1, &in, sizeof in);
	/* Wine's cursor position lags the injected event; a real driver's does
	 * too, and the compositor is downstream of both. */
	Sleep(40);
}

static void click(void)
{
	if (use_post) {
		POINT p;
		GetCursorPos(&p);
		ScreenToClient(target, &p);
		LPARAM lp = (LPARAM)((p.y << 16) | (p.x & 0xffff));
		PostMessageW(target, WM_LBUTTONDOWN, MK_LBUTTON, lp);
		Sleep(60);
		PostMessageW(target, WM_LBUTTONUP, 0, lp);
		return;
	}
	INPUT in;
	memset(&in, 0, sizeof in);
	in.type = INPUT_MOUSE;
	in.mi.dwFlags = MOUSEEVENTF_LEFTDOWN;
	SendInput(1, &in, sizeof in);
	Sleep(60);
	in.mi.dwFlags = MOUSEEVENTF_LEFTUP;
	SendInput(1, &in, sizeof in);
}

/* One notch away from the user, which Windows calls +WHEEL_DELTA and Wayland
 * must be told is *negative* vertical scroll. The sign is the thing being
 * tested, so the probe scrolls in one direction only and the harness asserts on
 * which one came out. */
static void wheel(int notches)
{
	if (use_post) {
		POINT p;
		GetCursorPos(&p);
		PostMessageW(target, WM_MOUSEWHEEL,
			     (WPARAM)((notches * WHEEL_DELTA) << 16),
			     (LPARAM)((p.y << 16) | (p.x & 0xffff)));
		return;
	}
	INPUT in;
	memset(&in, 0, sizeof in);
	in.type = INPUT_MOUSE;
	in.mi.dwFlags = MOUSEEVENTF_WHEEL;
	in.mi.mouseData = (DWORD)(notches * WHEEL_DELTA);
	SendInput(1, &in, sizeof in);
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
	const char *text = "hello";
	wchar_t cls[128] = L"WaycomposeToplevel";
	int wait = 20, do_mouse = 1, do_keys = 1, leave = 1;

	for (int i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "--log") && i + 1 < argc) {
			/* The probe has to run *inside* the Wine virtual desktop
			 * to see the compositor's windows at all — `FindWindow`
			 * enumerates one desktop and `wine explorer /desktop=`
			 * makes a second one. A program launched that way is a
			 * child of `explorer.exe` and its console is inside the
			 * desktop, so a shell redirect on the outside catches
			 * the loader and nothing else. Same problem, same fix,
			 * as `wc-compositor`'s own `--log`. */
			if (!freopen(argv[++i], "w", stdout))
				return 3;
			setvbuf(stdout, NULL, _IONBF, 0);
		} else if (!strcmp(argv[i], "--post")) {
			use_post = 1;
		} else if (!strcmp(argv[i], "--no-mouse")) {
			do_mouse = 0;
		} else if (!strcmp(argv[i], "--no-keys")) {
			do_keys = 0;
		} else if (!strcmp(argv[i], "--no-leave")) {
			leave = 0;
		} else if (!strcmp(argv[i], "--type") && i + 1 < argc) {
			text = argv[++i];
		} else if (!strcmp(argv[i], "--class") && i + 1 < argc) {
			MultiByteToWideChar(CP_ACP, 0, argv[++i], -1, cls, 128);
		} else if (!strcmp(argv[i], "--wait") && i + 1 < argc) {
			wait = atoi(argv[++i]);
		} else {
			printf("probe-input: unknown argument %s\n", argv[i]);
			return 2;
		}
	}

	target = find_target(cls, wait);
	if (!target) {
		printf("probe-input: no visible window of class '%ls' after %d s\n",
		       cls, wait);
		return 1;
	}
	RECT rc, wr;
	GetClientRect(target, &rc);
	GetWindowRect(target, &wr);
	printf("probe-input: target HWND %p, client %ldx%ld, at %ld,%ld\n",
	       (void *)target, (long)rc.right, (long)rc.bottom,
	       (long)wr.left, (long)wr.top);
	fflush(stdout);

	/* The window must have the focus before a keystroke means anything, and
	 * taking it is the point rather than a preliminary: `WM_SETFOCUS` is
	 * what the compositor turns into `wl_keyboard.enter`, so a run where
	 * this failed would have no keyboard focus and the harness would see
	 * that rather than a silent absence of keys.
	 *
	 * `AttachThreadInput` first, and it is required rather than defensive.
	 * `SetForegroundWindow` is refused for a process that is not already
	 * the foreground one — that restriction exists so applications cannot
	 * steal focus — and `SetFocus`/`GetFocus` operate on the *calling
	 * thread's* input queue, so without attaching they address a queue the
	 * target window is not in. Measured: without this, the compositor's
	 * window was found, `SetForegroundWindow` returned, the foreground
	 * window was still somebody else's, and every injected key went to
	 * Wine's desktop window. The compositor logged nothing, which reads
	 * exactly like a compositor that ignores input. */
	DWORD self = GetCurrentThreadId();
	DWORD owner = GetWindowThreadProcessId(target, NULL);
	BOOL attached = AttachThreadInput(self, owner, TRUE);
	BringWindowToTop(target);
	SetForegroundWindow(target);
	SetActiveWindow(target);
	SetFocus(target);
	Sleep(300);

	HWND fg = GetForegroundWindow();
	wchar_t fgcls[128] = L"";
	if (fg)
		GetClassNameW(fg, fgcls, 128);
	printf("probe-input: attached=%d foreground=%p (class '%ls') focus=%p target=%p\n",
	       (int)attached, (void *)fg, fgcls, (void *)GetFocus(), (void *)target);
	if (fg != target)
		printf("probe-input: WARNING the target is not the foreground window; "
		       "injected input will go elsewhere\n");
	fflush(stdout);

	/* Client-area centre, in screen coordinates. */
	POINT centre = { rc.right / 2, rc.bottom / 2 };
	ClientToScreen(target, &centre);

	if (do_mouse) {
		move_to(centre.x, centre.y);
		move_to(centre.x + 12, centre.y + 8);
		printf("probe-input: moved to the client-area centre\n");
		click();
		printf("probe-input: clicked\n");
		wheel(1);
		printf("probe-input: wheel +1 notch (away from the user)\n");
		fflush(stdout);
		Sleep(200);
	}

	if (do_keys) {
		type_string(text);
		fflush(stdout);
		Sleep(200);
	}

	if (do_mouse && leave) {
		/* Off the window entirely, which is the only way to produce
		 * `WM_MOUSELEAVE` — and therefore the only way to find out
		 * whether `TrackMouseEvent` was ever armed. */
		move_to(wr.left > 40 ? wr.left - 30 : wr.right + 30,
			wr.top > 40 ? wr.top - 30 : wr.bottom + 30);
		printf("probe-input: moved off the window\n");
		fflush(stdout);
		Sleep(300);
	}

	if (attached)
		AttachThreadInput(self, owner, FALSE);
	printf("probe-input: done\n");
	return 0;
}
