/* probe-clock.c -- what a Windows monotonic clock is actually worth.
 *
 *   usage: probe-clock.exe
 *
 * docs/design.md §7.7 item 5: smithay's `Monotonic` clock currently has its
 * epoch at process start rather than at boot, which is wrong for
 * `wp_presentation`, and the fix named there is `QueryPerformanceCounter`.
 * §7.8 item 7 makes the resolution of that counter a thing to check on the
 * guest *if* the fix is taken. It costs one program to know the answer before
 * committing to it, so:
 *
 *   - the QPC frequency, which bounds the resolution;
 *   - the smallest non-zero delta two back-to-back QPC calls can observe, which
 *     is the resolution you actually get rather than the one advertised;
 *   - `GetTickCount64`, for scale;
 *   - QPC's own epoch, by comparing it against system uptime -- the property
 *     §7.7 item 5 is really about.
 *
 * Rust's `std::time::Instant` on Windows is QPC underneath, so these numbers
 * are also what `Instant` can resolve.
 */
#include <windows.h>
#include <stdio.h>

int main(void)
{
    LARGE_INTEGER freq, a, b;
    ULONGLONG tick;
    int i;
    LONGLONG smallest = 0;

    if (!QueryPerformanceFrequency(&freq)) {
        printf("QueryPerformanceFrequency FAILED err=%lu\n", GetLastError());
        return 1;
    }
    printf("QPC frequency: %lld Hz (%.1f ns per tick)\n", freq.QuadPart,
           1e9 / (double)freq.QuadPart);

    /* The smallest delta two adjacent calls can see. */
    for (i = 0; i < 200000; i++) {
        QueryPerformanceCounter(&a);
        QueryPerformanceCounter(&b);
        {
            LONGLONG d = b.QuadPart - a.QuadPart;
            if (d > 0 && (smallest == 0 || d < smallest))
                smallest = d;
        }
    }
    printf("smallest observed QPC delta: %lld ticks = %.1f ns\n", smallest,
           (double)smallest * 1e9 / (double)freq.QuadPart);

    QueryPerformanceCounter(&a);
    tick = GetTickCount64();
    printf("QPC now: %lld ticks = %.3f s since its epoch\n", a.QuadPart,
           (double)a.QuadPart / (double)freq.QuadPart);
    printf("GetTickCount64: %llu ms = %.3f s since boot\n",
           (unsigned long long)tick, (double)tick / 1000.0);
    {
        /* Both counters are documented to start at boot, so the question is
           whether they still agree. They are not required to agree exactly:
           GetTickCount64 is derived from the interrupt-time clock and does not
           advance in some low-power states, and this guest is hibernated and
           resumed routinely. So the test is relative -- a fraction of a percent
           of uptime is "the same epoch", a wall-clock-sized gap is not. */
        double qpc = (double)a.QuadPart / (double)freq.QuadPart;
        double up = (double)tick / 1000.0;
        double diff = qpc - up;
        double rel = up > 1.0 ? (diff < 0 ? -diff : diff) / up : 0.0;
        printf("epoch check: QPC %s boot-based (differs from GetTickCount64 by "
               "%.3f s = %.4f%% of uptime)\n",
               rel < 0.01 ? "is" : "is NOT", diff, rel * 100.0);
    }

    /* Sleep(1) is the classic scheduler-granularity question; a compositor that
       throttles a frame callback with it needs to know. */
    {
        LONGLONG worst = 0, total = 0;
        for (i = 0; i < 20; i++) {
            QueryPerformanceCounter(&a);
            Sleep(1);
            QueryPerformanceCounter(&b);
            {
                LONGLONG d = b.QuadPart - a.QuadPart;
                total += d;
                if (d > worst) worst = d;
            }
        }
        printf("Sleep(1): mean %.3f ms, worst %.3f ms over 20 tries\n",
               (double)total / 20.0 * 1000.0 / (double)freq.QuadPart,
               (double)worst * 1000.0 / (double)freq.QuadPart);
    }
    return 0;
}
