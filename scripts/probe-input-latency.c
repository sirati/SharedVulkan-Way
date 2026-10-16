/* probe-input-latency.c -- the guest's half of the input-latency measurement
 * (docs/waycompose-design.md §89).
 *
 *   usage: probe-input-latency.exe <secs> <motion|wheel|mixed> [period-ms]
 *
 * Finds the largest visible top-level window owned by an `sv-guest.exe` -- the
 * compositor's window for the application under test -- and, for <secs>
 * seconds, posts input into it the way Windows delivers it: `WM_MOUSEMOVE` to
 * a new point each time, and in `wheel`/`mixed` mode `WM_MOUSEWHEEL` notches
 * (down, then up again, so the page does not run out). Posting rather than
 * `SendInput` because the measurement runs in session 0 over ssh, where there
 * is no interactive input to synthesise; what is measured starts at the
 * compositor's window procedure either way, and the post time is logged so the
 * queue in between is visible too.
 *
 * The period is jittered (period/2 .. 3*period/2) so that nothing downstream
 * that ticks -- the compositor's 5 ms turn, a 60 Hz frame grid -- lines up with
 * it and hides its own share.
 *
 * One line per post, to stdout:
 *   inj <qpc-us> <kind> <tick-ms> <x> <y>
 * where qpc-us is QueryPerformanceCounter in microseconds -- the clock
 * WC_FRAME_TRACE_CLOCK=mono stamps with -- and tick-ms is GetTickCount, which
 * is the message time a posted message carries and the `time` the client is
 * sent. The analysis is scripts/wc-input-latency.py. */
#include <windows.h>
#include <tlhelp32.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static LARGE_INTEGER g_freq;

static unsigned long long now_us(void)
{
    LARGE_INTEGER t;
    QueryPerformanceCounter(&t);
    return (unsigned long long)((double)t.QuadPart * 1000000.0 / (double)g_freq.QuadPart);
}

static DWORD g_pids[64];
static int g_npids;
static HWND g_best;
static long g_best_area;

static BOOL CALLBACK each(HWND h, LPARAM unused)
{
    DWORD pid = 0;
    RECT r;
    int i;
    (void)unused;
    GetWindowThreadProcessId(h, &pid);
    for (i = 0; i < g_npids; i++) {
        if (g_pids[i] == pid) break;
    }
    if (i == g_npids) return TRUE;
    if (!GetClientRect(h, &r)) return TRUE;
    /* In session 0 a window may never be shown on a desktop anybody sees, and
     * IsWindowVisible is not what makes it receive messages: so every
     * top-level window of the compositor is a candidate, and the largest is
     * the application's. */
    printf("candidate %p %ldx%ld visible=%d\n", (void *)h, r.right, r.bottom, IsWindowVisible(h) ? 1 : 0);
    if ((long)r.right * r.bottom > g_best_area) {
        g_best_area = (long)r.right * r.bottom;
        g_best = h;
    }
    return TRUE;
}

static void find_compositor_pids(void)
{
    PROCESSENTRY32W pe;
    HANDLE snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    pe.dwSize = sizeof(pe);
    if (snap == INVALID_HANDLE_VALUE) return;
    if (Process32FirstW(snap, &pe)) {
        do {
            if (_wcsicmp(pe.szExeFile, L"sv-guest.exe") == 0 && g_npids < 64) {
                g_pids[g_npids++] = pe.th32ProcessID;
            }
        } while (Process32NextW(snap, &pe));
    }
    CloseHandle(snap);
}

int main(int argc, char **argv)
{
    int secs, period, n = 0, wheel_dir = -1, wheel_left = 0;
    const char *mode;
    unsigned long long end;
    RECT r;
    HANDLE timer;
    wchar_t title[256];

    setvbuf(stdout, NULL, _IOFBF, 1 << 16);
    if (argc < 3) {
        printf("usage: probe-input-latency.exe <secs> <motion|wheel|mixed> [period-ms]\n");
        return 2;
    }
    secs = atoi(argv[1]);
    mode = argv[2];
    period = argc > 3 ? atoi(argv[3]) : 50;
    if (period < 4) period = 4;
    QueryPerformanceFrequency(&g_freq);
    srand((unsigned)GetTickCount());

    find_compositor_pids();
    EnumWindows(each, 0);
    if (!g_best) {
        printf("error: no visible window of any sv-guest.exe (%d running)\n", g_npids);
        return 1;
    }
    GetClientRect(g_best, &r);
    GetWindowTextW(g_best, title, 256);
    printf("window %p client %ldx%ld title \"%ls\"\n", (void *)g_best, r.right, r.bottom, title);
    fflush(stdout);

    timer = CreateWaitableTimerExW(NULL, NULL, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS);
    end = now_us() + (unsigned long long)secs * 1000000ULL;
    while (now_us() < end) {
        LARGE_INTEGER due;
        int wait_ms = period / 2 + rand() % (period + 1);
        /* Points well inside the client area, never the same twice running,
         * clear of the tab strip at the top. */
        int x = r.right / 4 + (n * 37) % (r.right / 2);
        int y = r.bottom / 3 + (n * 53) % (r.bottom / 2);
        int is_wheel = 0;
        if (strcmp(mode, "wheel") == 0) is_wheel = 1;
        else if (strcmp(mode, "mixed") == 0) is_wheel = (n % 3) == 2;
        if (is_wheel) {
            POINT p;
            DWORD tick;
            unsigned long long t;
            if (wheel_left == 0) {
                wheel_dir = -wheel_dir;
                wheel_left = 20;
            }
            wheel_left--;
            p.x = x;
            p.y = y;
            ClientToScreen(g_best, &p);
            /* A move first, back to back with the notch. In session 0 the real
             * cursor is never over the window, so the compositor's
             * TrackMouseEvent answers every move with WM_MOUSELEAVE -- each
             * move is an *enter* -- and a notch that arrived after the leave
             * would have no surface to go to. Posted together, the leave lands
             * behind the notch. */
            t = now_us();
            tick = GetTickCount();
            PostMessageW(g_best, WM_MOUSEMOVE, 0, MAKELPARAM(x, y));
            printf("inj %llu motion %lu %d %d\n", t, tick, x, y);
            t = now_us();
            tick = GetTickCount();
            PostMessageW(g_best, WM_MOUSEWHEEL, MAKEWPARAM(0, (WORD)(short)(-120 * wheel_dir)),
                         MAKELPARAM(p.x, p.y));
            printf("inj %llu wheel %lu %d %d\n", t, tick, x, y);
        } else {
            unsigned long long t = now_us();
            DWORD tick = GetTickCount();
            PostMessageW(g_best, WM_MOUSEMOVE, 0, MAKELPARAM(x, y));
            printf("inj %llu motion %lu %d %d\n", t, tick, x, y);
        }
        n++;
        due.QuadPart = -(LONGLONG)wait_ms * 10000;
        SetWaitableTimer(timer, &due, 0, NULL, NULL, 0);
        WaitForSingleObject(timer, INFINITE);
    }
    printf("posted %d\n", n);
    return 0;
}
