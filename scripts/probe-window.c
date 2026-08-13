/* Which CreateWindowExW succeeds under Wine, and which does not.
 *
 * `wc-compositor.exe` got `ERROR_INVALID_WINDOW_HANDLE` (1400) from
 * `CreateWindowExW` for an ordinary `WS_OVERLAPPEDWINDOW` toplevel, while the
 * message-only controller window created moments earlier succeeded. Rather
 * than change one thing at a time in a 56 MB Rust binary and rebuild, this
 * isolates the call: same class, several parameter combinations, one line of
 * output each.
 *
 * Follows the convention of the other probes in this directory (probe-socket.c,
 * probe-xkb.c): a single translation unit, linked by hand, so that what is
 * being tested is the Win32 call and not cargo's idea of it.
 *
 *   x86_64-w64-mingw32-gcc -O0 -g -o probe-window.exe probe-window.c \
 *       -luser32 -lgdi32
 *   wine probe-window.exe
 */
#include <windows.h>
#include <stdio.h>

static LRESULT CALLBACK proc(HWND h, UINT m, WPARAM w, LPARAM l)
{
	return DefWindowProcW(h, m, w, l);
}

static void try_create(const char *label, DWORD ex, const wchar_t *cls,
		       DWORD style, int x, int y, int cw, int ch,
		       HWND parent, HINSTANCE inst)
{
	SetLastError(0);
	HWND h = CreateWindowExW(ex, cls, L"probe", style, x, y, cw, ch,
				 parent, NULL, inst, NULL);
	DWORD err = GetLastError();
	printf("%-46s -> %s (err %lu)\n", label,
	       h ? "OK" : "NULL", (unsigned long)err);
	fflush(stdout);
	if (h)
		DestroyWindow(h);
}

int main(void)
{
	HINSTANCE inst = GetModuleHandleW(NULL);
	printf("probe-window: hInstance=%p\n", (void *)inst);

	/* Two classes: one registered with the module handle, one with NULL,
	 * so the hInstance question is answered rather than assumed. */
	WNDCLASSEXW c;
	memset(&c, 0, sizeof c);
	c.cbSize = sizeof c;
	c.lpfnWndProc = proc;
	c.hInstance = inst;
	c.lpszClassName = L"ProbeWithInst";
	ATOM a1 = RegisterClassExW(&c);
	printf("probe-window: RegisterClassExW(hInstance=module) -> %u (err %lu)\n",
	       (unsigned)a1, (unsigned long)GetLastError());

	memset(&c, 0, sizeof c);
	c.cbSize = sizeof c;
	c.lpfnWndProc = proc;
	c.hInstance = NULL;
	c.lpszClassName = L"ProbeNullInst";
	ATOM a2 = RegisterClassExW(&c);
	printf("probe-window: RegisterClassExW(hInstance=NULL)   -> %u (err %lu)\n",
	       (unsigned)a2, (unsigned long)GetLastError());

	const DWORD overlapped = WS_OVERLAPPEDWINDOW;
	const DWORD clipped = WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_CLIPSIBLINGS;

	puts("");
	puts("-- class registered with the module handle --");
	try_create("overlapped, CW_USEDEFAULT, inst=module", 0, L"ProbeWithInst",
		   overlapped, CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT,
		   CW_USEDEFAULT, NULL, inst);
	try_create("overlapped, CW_USEDEFAULT, inst=NULL", 0, L"ProbeWithInst",
		   overlapped, CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT,
		   CW_USEDEFAULT, NULL, NULL);
	try_create("overlapped, explicit size, inst=module", 0, L"ProbeWithInst",
		   overlapped, 100, 100, 400, 300, NULL, inst);
	try_create("overlapped, explicit size, inst=NULL", 0, L"ProbeWithInst",
		   overlapped, 100, 100, 400, 300, NULL, NULL);
	try_create("clipchildren+siblings, explicit, inst=module", 0,
		   L"ProbeWithInst", clipped, 100, 100, 400, 300, NULL, inst);
	try_create("WS_POPUP, explicit, inst=module", 0, L"ProbeWithInst",
		   WS_POPUP, 100, 100, 400, 300, NULL, inst);
	try_create("message-only, inst=module", 0, L"ProbeWithInst", 0, 0, 0, 0,
		   0, HWND_MESSAGE, inst);

	puts("");
	puts("-- class registered with a NULL instance --");
	try_create("overlapped, CW_USEDEFAULT, inst=NULL", 0, L"ProbeNullInst",
		   overlapped, CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT,
		   CW_USEDEFAULT, NULL, NULL);
	try_create("overlapped, explicit size, inst=module", 0, L"ProbeNullInst",
		   overlapped, 100, 100, 400, 300, NULL, inst);

	puts("");
	printf("probe-window: GetDesktopWindow()=%p\n", (void *)GetDesktopWindow());
	printf("probe-window: GetSystemMetrics(SM_CXSCREEN)=%d SM_CYSCREEN=%d\n",
	       GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
	return 0;
}
