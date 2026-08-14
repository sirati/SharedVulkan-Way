/* probe-life.c -- did that window stop existing? Asked from outside.
 *
 * WHY THIS EXISTS
 *
 * Every window gate in this tree asserts that something **appeared**.
 * `wc-window.sh` waits for a toplevel, `wc-taskbar.sh` counts buttons,
 * `wc-icon.sh` reads back an icon, `wc-titlebar.sh` measures a caption. The one
 * thing none of them asserts is that a window **stopped existing**, and the
 * three that mention destruction at all do it by grepping the compositor's own
 * log for `xdg_toplevel destroyed -> window N`.
 *
 * That is the code under test reporting its own success, which
 * docs/waycompose-design.md §35 is the section about. A compositor that removed
 * its map entry, printed that line, and never called `DestroyWindow` would keep
 * every existing gate green while leaking an `HWND`, its icons and its
 * drop-target registration once per client, for as long as the session lasts.
 * Nothing would notice until a user's machine ran out of window handles.
 *
 * So this is a **separate process** that never talks to the compositor and asks
 * the window manager instead:
 *
 *   probe-life.exe pidof   --title <substr>   the PID owning a visible toplevel
 *   probe-life.exe count   --pid <n>          that process's windows and handles
 *   probe-life.exe alive   --hwnd <h>         IsWindow, as an exit code
 *   probe-life.exe windows --pid <n>          one line per toplevel it still owns
 *
 * `count` prints `user=<n> gdi=<n> toplevels=<n> all=<n>`. The first two come
 * from `GetGuiResources`, which is the kernel's own per-process accounting of
 * USER and GDI objects and cannot be talked out of by the process being
 * measured. The last two are `EnumWindows`, which is the same question asked a
 * different way: a leaked `HWND` is still enumerable, and a leaked `HICON` is
 * not, so the two together cover what one alone would miss.
 *
 * Exit codes so a shell can assert without parsing: 0 success / alive,
 * 1 the thing asked about is gone, 2 usage or API failure.
 */

#include <windows.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <stdint.h>

#ifndef GR_GDIOBJECTS
#define GR_GDIOBJECTS 0
#define GR_USEROBJECTS 1
#endif

static DWORD g_pid;
static const char *g_title;
/* Where the answer goes.
 *
 * Not stdout, and that is not a preference. Every invocation has to run inside
 * the compositor.s virtual desktop -- `EnumWindows` enumerates the *calling
 * thread.s* desktop, so a probe on the default one sees none of the windows
 * being asked about -- and the only way in is `wine explorer /desktop=NAME`,
 * which launches the program and does **not** connect its stdout to the caller.
 * A version of this that printed found the right windows and reported nothing,
 * which reads exactly like the windows not being there. */
static FILE *g_out;
static int n_visible, n_all;
static DWORD found_pid;
static char lines[64][640];
static int n_lines;

static BOOL CALLBACK by_title(HWND w, LPARAM p)
{
	char title[512];
	DWORD pid = 0;
	(void)p;
	if (!IsWindowVisible(w))
		return TRUE;
	title[0] = 0;
	GetWindowTextA(w, title, sizeof title);
	if (g_title && *g_title && strstr(title, g_title)) {
		GetWindowThreadProcessId(w, &pid);
		found_pid = pid;
		return FALSE;                    /* stop at the first match */
	}
	return TRUE;
}

static BOOL CALLBACK by_pid(HWND w, LPARAM p)
{
	DWORD pid = 0;
	RECT r;
	char title[512];
	(void)p;
	GetWindowThreadProcessId(w, &pid);
	if (pid != g_pid)
		return TRUE;
	n_all++;
	if (!IsWindowVisible(w))
		return TRUE;
	n_visible++;
	title[0] = 0;
	GetWindowTextA(w, title, sizeof title);
	if (!GetWindowRect(w, &r))
		memset(&r, 0, sizeof r);
	if (n_lines < 64)
		snprintf(lines[n_lines++], sizeof lines[0], "%llu\t%ld,%ld,%ld,%ld\t%s",
			 (unsigned long long)(uintptr_t)w,
			 (long)r.left, (long)r.top, (long)r.right, (long)r.bottom, title);
	return TRUE;
}

int main(int argc, char **argv)
{
	const char *cmd = argc > 1 ? argv[1] : "";
	unsigned long long hwnd_arg = 0;
	int i;

	for (i = 2; i < argc; i++) {
		if (!strcmp(argv[i], "--pid") && i + 1 < argc)
			g_pid = (DWORD)strtoul(argv[++i], NULL, 0);
		else if (!strcmp(argv[i], "--title") && i + 1 < argc)
			g_title = argv[++i];
		else if (!strcmp(argv[i], "--hwnd") && i + 1 < argc)
			hwnd_arg = strtoull(argv[++i], NULL, 0);
		else if (!strcmp(argv[i], "--out") && i + 1 < argc)
			g_out = fopen(argv[++i], "w");
	}

	/* Does this platform's `GetGuiResources` answer at all?
	 *
	 * ADDED FOR docs/waycompose-design.md §69. §53.3 measured it returning 0
	 * under Wine on the workstation and skipped the USER/GDI assertion for
	 * that reason. That left a question the skip could not settle: **is the
	 * zero a fact about Wine, or a fact about the venue?** If Wine stubs it
	 * everywhere then no arrangement of virtual machines will make that
	 * assertion possible and the guest run should not be spent on it; if it
	 * answers somewhere, the skip has somewhere to go.
	 *
	 * Self-contained on purpose. The other modes need a second process with
	 * windows to point at, and shipping two programs and a compositor into a
	 * VM to ask one question is how a cheap measurement becomes a project.
	 * This makes its own windows, asks about its own process, and prints
	 * both numbers with the count of windows they are supposed to describe —
	 * so `user=0 windows=4` is legible as a stub rather than as an absence.
	 */
	if (!strcmp(cmd, "selftest")) {
		WNDCLASSEXW wc;
		HWND made[4];
		DWORD before_u, before_g, after_u, after_g;
		HANDLE self = GetCurrentProcess();
		int k;

		memset(&wc, 0, sizeof wc);
		wc.cbSize = sizeof wc;
		wc.lpfnWndProc = DefWindowProcW;
		wc.hInstance = GetModuleHandleW(NULL);
		wc.lpszClassName = L"probe_life_selftest";
		RegisterClassExW(&wc);

		before_u = GetGuiResources(self, GR_USEROBJECTS);
		before_g = GetGuiResources(self, GR_GDIOBJECTS);
		for (k = 0; k < 4; k++)
			made[k] = CreateWindowExW(0, L"probe_life_selftest", L"probe-life selftest",
						  WS_OVERLAPPEDWINDOW, 0, 0, 120, 90,
						  NULL, NULL, wc.hInstance, NULL);
		after_u = GetGuiResources(self, GR_USEROBJECTS);
		after_g = GetGuiResources(self, GR_GDIOBJECTS);

		n_all = 0;
		g_pid = GetCurrentProcessId();
		EnumWindows(by_pid, 0);

		fprintf(g_out ? g_out : stdout,
			"before user=%lu gdi=%lu\nafter  user=%lu gdi=%lu\nwindows=%d\ncounter=%s\n",
			(unsigned long)before_u, (unsigned long)before_g,
			(unsigned long)after_u, (unsigned long)after_g, n_all,
			/* The same contradiction the `count` mode uses: a process that
			 * owns windows owns USER objects. Four were just made. */
			(after_u == 0) ? "stub"
				       : (after_u > before_u ? "live" : "flat"));
		/* AND DOWN AGAIN. ADDED FOR docs/waycompose-design.md §73.
		 *
		 * A counter that only goes up cannot show a leak: the assertion
		 * §53 wants is "the number came back", and on a stubbed counter
		 * `0 == 0` satisfies it forever. So the windows are destroyed and
		 * the count is taken a third time, and the verdict is `live` only
		 * if it rose AND fell. On real Windows this is the difference
		 * between an instrument and a decoration. */
		for (k = 0; k < 4; k++)
			if (made[k]) DestroyWindow(made[k]);
		{
			DWORD gone_u = GetGuiResources(self, GR_USEROBJECTS);
			fprintf(g_out ? g_out : stdout, "destroyed user=%lu\nfell=%s\n",
				(unsigned long)gone_u,
				(gone_u < after_u) ? "yes" : "no");
		}
		if (g_out) fclose(g_out);
		return 0;
	}

	if (!strcmp(cmd, "pidof")) {
		if (!g_title) { fprintf(stderr, "probe-life: pidof needs --title\n"); return 2; }
		EnumWindows(by_title, 0);
		if (!found_pid) { fprintf(g_out ? g_out : stdout, "0\n"); if (g_out) fclose(g_out); return 1; }
		fprintf(g_out ? g_out : stdout, "%lu\n", (unsigned long)found_pid);
		if (g_out) fclose(g_out);
		return 0;
	}

	if (!strcmp(cmd, "alive")) {
		HWND h = (HWND)(uintptr_t)hwnd_arg;
		int alive = h && IsWindow(h);
		/* `IsWindow` and nothing else, deliberately. A window that has been
		 * destroyed but whose handle value has been *reused* would answer
		 * TRUE here, which is why the harness pairs this with a count
		 * rather than trusting it alone: a reuse cannot make a total go up. */
		fprintf(g_out ? g_out : stdout, "%s\n", alive ? "alive" : "gone");
		if (g_out) fclose(g_out);
		return alive ? 0 : 1;
	}

	if (!strcmp(cmd, "count") || !strcmp(cmd, "windows")) {
		HANDLE proc;
		DWORD user = 0, gdi = 0;
		int opened = 0;
		if (!g_pid) { fprintf(stderr, "probe-life: needs --pid\n"); return 2; }
		EnumWindows(by_pid, 0);
		/* PROCESS_QUERY_LIMITED_INFORMATION is what GetGuiResources wants and
		 * is grantable across sessions; falling back rather than failing,
		 * because a zero here must be distinguishable from a refusal. */
		proc = OpenProcess(0x1000 /* QUERY_LIMITED_INFORMATION */, FALSE, g_pid);
		if (!proc)
			proc = OpenProcess(PROCESS_QUERY_INFORMATION, FALSE, g_pid);
		if (proc) {
			SetLastError(0);
			user = GetGuiResources(proc, GR_USEROBJECTS);
			gdi = GetGuiResources(proc, GR_GDIOBJECTS);
			opened = 1;
			CloseHandle(proc);
		}
		if (!strcmp(cmd, "windows")) {
			for (i = 0; i < n_lines; i++)
				fprintf(g_out ? g_out : stdout, "%s\n", lines[i]);
			if (g_out) fclose(g_out);
			return 0;
		}
		/* WHETHER THE HANDLE COUNTER IS EVEN THERE, said out loud.
		 *
		 * `GetGuiResources` is the obvious instrument for "did this leak"
		 * and **Wine returns 0 for it** — measured, on wine-11.14 staging,
		 * against a `notepad` that plainly owned three windows. A harness
		 * that asserted `user == baseline` on this platform would compare
		 * zero to zero and pass forever: a gate that cannot fail, which is
		 * the thing docs/waycompose-design.md §35 is about and the reason
		 * this line exists.
		 *
		 * The detection is not a platform check but a contradiction: a
		 * process that owns windows owns USER objects, so `toplevels > 0`
		 * with `user == 0` means the counter is not implemented, whoever is
		 * underneath. The harness reads this word and skips rather than
		 * asserts. */
		fprintf(g_out ? g_out : stdout, "user=%lu gdi=%lu toplevels=%d all=%d counter=%s\n",
		       (unsigned long)user, (unsigned long)gdi, n_visible, n_all,
		       (n_all > 0 && user == 0) ? "stub" : (opened ? "live" : "denied"));
		if (g_out) fclose(g_out);
		return 0;
	}

	fprintf(stderr, "usage: probe-life.exe pidof|count|alive|windows [--pid N] "
			"[--title S] [--hwnd H]\n");
	return 2;
}
