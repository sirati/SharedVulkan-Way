/*
 * wc-activity.exe: when did a person last touch this Windows session?
 * (docs/waycompose-design.md §101)
 *
 * The rule for using <guest-user>'s desktop: it is ours when nobody has used the
 * keyboard or mouse for two minutes. This program answers "when did somebody
 * last". It runs in the interactive session without a window or focus and
 * installs WH_KEYBOARD_LL and WH_MOUSE_LL. Every event counts, injected or not:
 * a person working through TeamViewer arrives as SendInput, flagged injected
 * exactly like an agent's input. What does not count is OUR input: every agent
 * tool that calls SendInput puts WC_AGENT_TAG in dwExtraInfo, which a low-level
 * hook receives as it was sent, and those events are skipped (and counted, as
 * `ours`). Mouse moves of under 2 px from the last counted position are jitter.
 * The kind letter says which path an input came by: `k`/`m` hardware (QEMU's
 * virtual devices included, whose injections the host-side check discounts
 * against /tmp/wcrun/qmp-inject.log), `K`/`M` injected by something not ours.
 *
 * Every 2 s a second thread writes %LOCALAPPDATA%\waycompose\activity.txt
 * (replaced atomically): a heartbeat, and the times of the last real inputs,
 * newest first, coalesced to one per 250 ms. The hooks never wait on the disk,
 * because Windows silently removes a low-level hook that is slow to return.
 * They are reinstalled every minute in case it did anyway.
 *
 * Build: x86_64-w64-mingw32-gcc -O2 -mwindows -municode -o wc-activity.exe wc-activity.c
 * (scripts/wc-activity-install.sh builds, copies and starts it).
 */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <wchar.h>

#define RING 64
#define COALESCE_MS 250
/* "WCAGENT1": what our tools put in dwExtraInfo (docs/waycompose-design.md §101). */
#define WC_AGENT_TAG ((ULONG_PTR)0x57434147454E5431ULL)

static CRITICAL_SECTION cs;
static ULONGLONG ring_wall[RING]; /* Unix ms, newest at ring_head-1 */
static char ring_kind[RING];
static int ring_head, ring_count;
static ULONGLONG started_wall;
static POINT anchor;
static int have_anchor;
static HHOOK kb_hook, ms_hook;
static volatile LONG ours;

static ULONGLONG wall_ms(void) {
    FILETIME ft;
    GetSystemTimeAsFileTime(&ft);
    ULARGE_INTEGER u;
    u.LowPart = ft.dwLowDateTime;
    u.HighPart = ft.dwHighDateTime;
    return (u.QuadPart - 116444736000000000ULL) / 10000ULL;
}

static void mark(char kind) {
    ULONGLONG now = wall_ms();
    EnterCriticalSection(&cs);
    int last = (ring_head + RING - 1) % RING;
    if (ring_count > 0 && now - ring_wall[last] < COALESCE_MS) {
        ring_wall[last] = now;
        ring_kind[last] = kind;
    } else {
        ring_wall[ring_head] = now;
        ring_kind[ring_head] = kind;
        ring_head = (ring_head + 1) % RING;
        if (ring_count < RING) ring_count++;
    }
    LeaveCriticalSection(&cs);
}

static LRESULT CALLBACK on_key(int code, WPARAM w, LPARAM l) {
    if (code == HC_ACTION) {
        const KBDLLHOOKSTRUCT *k = (const KBDLLHOOKSTRUCT *)l;
        if (k->dwExtraInfo == WC_AGENT_TAG) InterlockedIncrement(&ours);
        else mark((k->flags & (LLKHF_INJECTED | LLKHF_LOWER_IL_INJECTED)) ? 'K' : 'k');
    }
    return CallNextHookEx(NULL, code, w, l);
}

static LRESULT CALLBACK on_mouse(int code, WPARAM w, LPARAM l) {
    if (code == HC_ACTION) {
        const MSLLHOOKSTRUCT *m = (const MSLLHOOKSTRUCT *)l;
        char kind = (m->flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED)) ? 'M' : 'm';
        if (m->dwExtraInfo == WC_AGENT_TAG) {
            InterlockedIncrement(&ours);
            /* Our move is where the pointer now is: not the person's jitter. */
            if (w == WM_MOUSEMOVE) { anchor = m->pt; have_anchor = 1; }
        } else if (w == WM_MOUSEMOVE) {
            if (!have_anchor || labs(m->pt.x - anchor.x) > 1 || labs(m->pt.y - anchor.y) > 1) {
                if (have_anchor) mark(kind);
                anchor = m->pt;
                have_anchor = 1;
            }
        } else {
            mark(kind);
        }
    }
    return CallNextHookEx(NULL, code, w, l);
}

static void install_hooks(void) {
    if (kb_hook) UnhookWindowsHookEx(kb_hook);
    if (ms_hook) UnhookWindowsHookEx(ms_hook);
    HINSTANCE self = GetModuleHandleW(NULL);
    kb_hook = SetWindowsHookExW(WH_KEYBOARD_LL, on_key, self, 0);
    ms_hook = SetWindowsHookExW(WH_MOUSE_LL, on_mouse, self, 0);
}

static wchar_t path[MAX_PATH], tmp[MAX_PATH];

static DWORD WINAPI writer(LPVOID unused) {
    (void)unused;
    DWORD session = 0;
    ProcessIdToSessionId(GetCurrentProcessId(), &session);
    for (;;) {
        char buf[4096];
        int n = 0;
        EnterCriticalSection(&cs);
        n += snprintf(buf + n, sizeof buf - n,
                      "wc-activity 2\r\npid %lu\r\nsession %lu\r\nhooks %d\r\nstarted_wall_ms %llu\r\n"
                      "heartbeat_wall_ms %llu\r\nours %ld\r\ninputs %d\r\n",
                      GetCurrentProcessId(), session, (kb_hook && ms_hook) ? 1 : 0, started_wall, wall_ms(),
                      ours, ring_count);
        for (int i = 0; i < ring_count && n < (int)sizeof buf - 64; i++) {
            int j = (ring_head + RING - 1 - i) % RING;
            n += snprintf(buf + n, sizeof buf - n, "input %llu %c\r\n", ring_wall[j], ring_kind[j]);
        }
        LeaveCriticalSection(&cs);
        HANDLE f = CreateFileW(tmp, GENERIC_WRITE, 0, NULL, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
        if (f != INVALID_HANDLE_VALUE) {
            DWORD wrote = 0;
            WriteFile(f, buf, (DWORD)n, &wrote, NULL);
            CloseHandle(f);
            MoveFileExW(tmp, path, MOVEFILE_REPLACE_EXISTING);
        }
        Sleep(2000);
    }
    return 0;
}

int WINAPI wWinMain(HINSTANCE inst, HINSTANCE prev, PWSTR cmd, int show) {
    (void)inst; (void)prev; (void)cmd; (void)show;
    /* One per session. */
    CreateMutexW(NULL, TRUE, L"Local\\wc-activity");
    if (GetLastError() == ERROR_ALREADY_EXISTS) return 0;
    wchar_t dir[MAX_PATH];
    DWORD len = GetEnvironmentVariableW(L"LOCALAPPDATA", dir, MAX_PATH);
    if (len == 0 || len >= MAX_PATH - 40) return 1;
    wcscat(dir, L"\\waycompose");
    CreateDirectoryW(dir, NULL);
    swprintf(path, MAX_PATH, L"%ls\\activity.txt", dir);
    swprintf(tmp, MAX_PATH, L"%ls\\activity.txt.tmp", dir);
    InitializeCriticalSection(&cs);
    started_wall = wall_ms();
    install_hooks();
    if (!CreateThread(NULL, 0, writer, NULL, 0, NULL)) return 1;
    SetTimer(NULL, 0, 60000, NULL);
    MSG msg;
    while (GetMessageW(&msg, NULL, 0, 0) > 0) {
        if (msg.message == WM_TIMER) install_hooks();
        DispatchMessageW(&msg);
    }
    return 0;
}
