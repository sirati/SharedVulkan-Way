/* Inject a **synthetic** multi-touch gesture into the compositor's window.
 *
 * STAGE 20. The companion to probe-input.c, and the label matters more than
 * the code: every contact this produces is manufactured by software. There is
 * no digitiser behind it, so a pass says the `WM_POINTER*` path in
 * `crates/wc-compositor/src/platform/win32.rs` is reachable, decodes what
 * Windows puts in front of it and produces the right `wl_touch` stream. It
 * does **not** say a finger works. See docs/design.md §24.6 for the division.
 *
 *   x86_64-w64-mingw32-gcc -O1 -o probe-touch.exe probe-touch.c -luser32
 *   wine probe-touch.exe --class WaycomposeToplevel
 *
 * WHY InjectTouchInput AND NOT PostMessage
 *
 * The same argument probe-input.c makes for `SendInput`, and here it is
 * stronger. A `WM_POINTERDOWN` cannot usefully be posted: its `wParam` is a
 * *pointer id* and the payload is fetched afterwards by `GetPointerTouchInfo`
 * from the input stack, which knows nothing about an id nobody registered. A
 * posted message would therefore make the compositor call `GetPointerType`,
 * get nothing, and correctly fall through to `DefWindowProc` — testing the
 * refusal path and nothing else.
 *
 * `InitializeTouchInjection` plus `InjectTouchInput` go in at the bottom, where
 * a digitiser's driver goes in. Windows builds the pointer ids, the frame ids
 * and the contact rectangles itself, routes the contacts by hit-testing, takes
 * the implicit capture, and delivers real `WM_POINTER*` messages. Everything
 * the compositor reads is then Windows' own, which is the property that makes
 * this worth running at all.
 *
 * WHY NOT CreateSyntheticPointerDevice / InjectSyntheticPointerInput
 *
 * They are the newer generalisation of the same thing (Windows 10 1809) and
 * would also work. `InjectTouchInput` is taken because it is Windows 8 and is
 * therefore available wherever the `WM_POINTER*` family this exercises is
 * available — a probe that needed a newer Windows than the code under test
 * would be able to fail for a reason that is not about the code.
 *
 * Both are resolved with `GetProcAddress` rather than linked, so that the probe
 * *runs* on a Windows or a Wine that lacks them and says so, instead of failing
 * to load with an error that names a symbol and not a cause.
 *
 * WHAT THE GESTURE IS, AND WHY IT IS A PINCH
 *
 * Two contacts moving apart. A tap would exercise down, motion, up and frame
 * and would say nothing about the part that is actually hard: that contacts are
 * tracked by id and the ids must stay put for the life of the contact. Two
 * fingers moving in *opposite* directions is the smallest gesture where a
 * swapped, renumbered or reused id is visible from outside — the fingers
 * converge instead of diverging — rather than only in a log.
 *
 * THE RULES OF AN INJECTION SESSION, WHICH ARE EASY TO GET WRONG
 *
 * `InjectTouchInput` takes the **whole frame** each time: every contact that is
 * still down must be present in the array on every call, with `POINTER_FLAG_
 * UPDATE` if it has not changed. A contact simply left out is not "unchanged",
 * it is an error, and the call fails with ERROR_INVALID_PARAMETER for the whole
 * frame. The first call for a contact carries DOWN|INRANGE|INCONTACT and the
 * last carries UP alone — UP with INCONTACT is also rejected.
 *
 * And the one that cost a round on the guest: **`pointerId` is an index, not a
 * name.** It must be in `0 .. maxCount-1` of the `InitializeTouchInjection`
 * call, so with two contacts the ids are 0 and 1 and *not* 1 and 2. Numbering
 * them from one fails the whole frame with 87, ERROR_INVALID_PARAMETER, which
 * is the same error every other mistake in this paragraph produces.
 *
 * Worth keeping for what it says about the compositor: these injection ids are
 * 0 and 1 and the `pointerId` Windows then puts in `POINTER_TOUCH_INFO` is
 * neither — measured on the guest, the same two contacts arrived as 4 and 5.
 * The mapping in `crate::input::touch::Slots` is not a formality.
 */
#include <windows.h>
#include <stdio.h>
#include <string.h>

#ifndef TOUCH_FEEDBACK_NONE
#define TOUCH_FEEDBACK_NONE 3
#endif

typedef BOOL(WINAPI *pfn_init_inject)(UINT32, DWORD);
typedef BOOL(WINAPI *pfn_inject)(UINT32, const POINTER_TOUCH_INFO *);

static pfn_init_inject p_InitializeTouchInjection;
static pfn_inject p_InjectTouchInput;

#define CONTACTS 2

static POINTER_TOUCH_INFO contact[CONTACTS];

/* A contact patch of `RADIUS` pixels either side of the point.
 *
 * `TOUCH_MASK_CONTACTAREA` is set, so this is the probe asserting that the
 * compositor's `shape` path runs on a rectangle Windows passed through — the
 * one half of `wl_touch`'s geometry that can be exercised without hardware.
 * Orientation is deliberately *not* claimed: no injection API offers one, and a
 * probe that set `TOUCH_MASK_ORIENTATION` on a made-up angle would be testing
 * that the compositor forwards the probe's fiction. */
#define RADIUS 6

/* `i` is both the array slot and the injection id, and they are the same number
 * on purpose: `pointerId` here is an index into the `maxCount` contacts
 * `InitializeTouchInjection` reserved, so with two contacts it must be 0 or 1.
 * Passing them as a separate argument is how this was got wrong once. */
static void set_contact(int i, int x, int y, UINT32 flags)
{
	memset(&contact[i], 0, sizeof contact[i]);
	contact[i].pointerInfo.pointerType = PT_TOUCH;
	contact[i].pointerInfo.pointerId = (UINT32)i;
	contact[i].pointerInfo.ptPixelLocation.x = x;
	contact[i].pointerInfo.ptPixelLocation.y = y;
	contact[i].pointerInfo.pointerFlags = flags;
	contact[i].touchFlags = TOUCH_FLAG_NONE;
	contact[i].touchMask = TOUCH_MASK_CONTACTAREA;
	contact[i].rcContact.left = x - RADIUS;
	contact[i].rcContact.right = x + RADIUS;
	contact[i].rcContact.top = y - RADIUS;
	contact[i].rcContact.bottom = y + RADIUS;
	contact[i].orientation = 0;
	contact[i].pressure = 32000;
}

static int inject(const char *what)
{
	if (!p_InjectTouchInput(CONTACTS, contact)) {
		printf("probe-touch: InjectTouchInput(%s) failed, error %lu\n",
		       what, GetLastError());
		fflush(stdout);
		return 0;
	}
	return 1;
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
	int wait = 20, steps = 4, spread = 15;

	for (int i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "--log") && i + 1 < argc) {
			/* See probe-input.c: inside a Wine virtual desktop the
			 * process is a child of explorer.exe and an outside
			 * redirect catches the loader and nothing else. */
			if (!freopen(argv[++i], "w", stdout))
				return 3;
			setvbuf(stdout, NULL, _IONBF, 0);
		} else if (!strcmp(argv[i], "--class") && i + 1 < argc) {
			MultiByteToWideChar(CP_ACP, 0, argv[++i], -1, cls, 128);
		} else if (!strcmp(argv[i], "--wait") && i + 1 < argc) {
			wait = atoi(argv[++i]);
		} else if (!strcmp(argv[i], "--steps") && i + 1 < argc) {
			steps = atoi(argv[++i]);
		} else {
			printf("probe-touch: unknown argument %s\n", argv[i]);
			return 2;
		}
	}

	HMODULE u32 = GetModuleHandleW(L"user32.dll");
	p_InitializeTouchInjection =
		(pfn_init_inject)(void *)GetProcAddress(u32, "InitializeTouchInjection");
	p_InjectTouchInput =
		(pfn_inject)(void *)GetProcAddress(u32, "InjectTouchInput");
	if (!p_InitializeTouchInjection || !p_InjectTouchInput) {
		/* The expected outcome under Wine, and the reason this is a
		 * distinct exit code: "the platform has no touch injection" is
		 * a fact about the platform and must not be read by a harness
		 * as "the compositor mishandled a touch". */
		printf("probe-touch: this user32 has no touch injection "
		       "(InitializeTouchInjection=%p InjectTouchInput=%p)\n",
		       (void *)p_InitializeTouchInjection, (void *)p_InjectTouchInput);
		return 4;
	}

	/* TOUCH_FEEDBACK_NONE: no visual contact indicator. The compositor
	 * paints the window and a system-drawn circle on top of it would land
	 * in the frame dumps the rest of the harness compares. */
	if (!p_InitializeTouchInjection(CONTACTS, TOUCH_FEEDBACK_NONE)) {
		printf("probe-touch: InitializeTouchInjection failed, error %lu\n",
		       GetLastError());
		return 4;
	}

	HWND target = find_target(cls, wait);
	if (!target) {
		printf("probe-touch: no visible window of class '%ls' after %d s\n",
		       cls, wait);
		return 1;
	}

	/* Touch is routed by hit-testing the desktop, not by focus — so unlike
	 * probe-input.c this does not need the window to be foreground for the
	 * contacts to land on it. It is raised anyway, because a window with
	 * something on top of it would receive nothing and the failure would
	 * look identical to a compositor that ignored the messages. */
	DWORD self = GetCurrentThreadId();
	DWORD owner = GetWindowThreadProcessId(target, NULL);
	BOOL attached = AttachThreadInput(self, owner, TRUE);
	BringWindowToTop(target);
	SetForegroundWindow(target);
	Sleep(300);

	RECT rc;
	GetClientRect(target, &rc);
	POINT centre = { rc.right / 2, rc.bottom / 2 };
	ClientToScreen(target, &centre);
	printf("probe-touch: target %p, client-area centre at screen %ld,%ld\n",
	       (void *)target, centre.x, centre.y);
	fflush(stdout);

	int rc_out = 0;
	int d = 20;

	set_contact(0, centre.x - d, centre.y,
		    POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT);
	set_contact(1, centre.x + d, centre.y,
		    POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT);
	if (!inject("down"))
		rc_out = 5;
	printf("probe-touch: two contacts down at %ld,%ld +/- %d (SYNTHETIC)\n",
	       centre.x, centre.y, d);
	fflush(stdout);
	Sleep(60);

	for (int s = 1; s <= steps && !rc_out; s++) {
		d = 20 + s * spread;
		set_contact(0, centre.x - d, centre.y,
			    POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT);
		set_contact(1, centre.x + d, centre.y,
			    POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT);
		if (!inject("update"))
			rc_out = 5;
		Sleep(40);
	}
	printf("probe-touch: pinched out to +/- %d over %d steps\n", d, steps);
	fflush(stdout);

	/* Both up in one frame. A contact left out of the array would fail the
	 * whole call, so a staggered release would need two injections with the
	 * survivor carrying UPDATE — worth knowing, and not worth doing: the
	 * ids are already proved by the pinch. */
	set_contact(0, centre.x - d, centre.y, POINTER_FLAG_UP);
	set_contact(1, centre.x + d, centre.y, POINTER_FLAG_UP);
	if (!inject("up"))
		rc_out = 5;
	printf("probe-touch: both contacts up\n");

	if (attached)
		AttachThreadInput(self, owner, FALSE);
	printf("probe-touch: done (every contact above was synthetic)\n");
	fflush(stdout);
	return rc_out;
}
