/* probe-pipe.c -- step 4 fallback transport: Windows named pipe round trip,
 * server thread + client on main thread, plus GetNamedPipeClientProcessId()
 * (the named-pipe equivalent of SIO_AF_UNIX_GETPEERPID) so we can tell whether
 * peer identification is available on the fallback path.
 */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static const char *g_pipe = "\\\\.\\pipe\\wc-test";
static HANDLE g_ready;
static int g_server_rc = -1;

static DWORD WINAPI server_thread(LPVOID unused)
{
    HANDLE h;
    char buf[64];
    DWORD n = 0, w = 0;

    (void)unused;

    h = CreateNamedPipeA(g_pipe, PIPE_ACCESS_DUPLEX,
                         PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                         PIPE_UNLIMITED_INSTANCES, 4096, 4096, 0, NULL);
    if (h == INVALID_HANDLE_VALUE) {
        printf("[server] CreateNamedPipeA FAILED GetLastError=%lu\n",
               (unsigned long)GetLastError());
        SetEvent(g_ready);
        return 0;
    }
    printf("[server] CreateNamedPipeA(\"%s\") OK\n", g_pipe);
    fflush(stdout);
    SetEvent(g_ready);

    if (!ConnectNamedPipe(h, NULL) && GetLastError() != ERROR_PIPE_CONNECTED) {
        printf("[server] ConnectNamedPipe FAILED GetLastError=%lu\n",
               (unsigned long)GetLastError());
        goto out;
    }
    printf("[server] ConnectNamedPipe OK\n");

    {
        ULONG pid = 0;
        if (GetNamedPipeClientProcessId(h, &pid))
            printf("[server] GetNamedPipeClientProcessId OK: client pid=%lu (self=%lu)\n",
                   (unsigned long)pid, (unsigned long)GetCurrentProcessId());
        else
            printf("[server] GetNamedPipeClientProcessId FAILED GetLastError=%lu\n",
                   (unsigned long)GetLastError());
    }

    if (!ReadFile(h, buf, sizeof(buf), &n, NULL) || n == 0) {
        printf("[server] ReadFile FAILED GetLastError=%lu n=%lu\n",
               (unsigned long)GetLastError(), (unsigned long)n);
        goto out;
    }
    printf("[server] read %lu bytes: \"%.*s\"\n", (unsigned long)n, (int)n, buf);

    if (!WriteFile(h, buf, n, &w, NULL) || w != n) {
        printf("[server] WriteFile FAILED GetLastError=%lu\n",
               (unsigned long)GetLastError());
        goto out;
    }
    FlushFileBuffers(h);
    printf("[server] echoed %lu bytes\n", (unsigned long)w);
    g_server_rc = 0;

out:
    fflush(stdout);
    DisconnectNamedPipe(h);
    CloseHandle(h);
    return 0;
}

int main(void)
{
    HANDLE th, h;
    char buf[64];
    static const char msg[] = "PING-pipe";
    DWORD n = 0, w = 0;
    int rc = 1;

    g_ready = CreateEventA(NULL, TRUE, FALSE, NULL);
    th = CreateThread(NULL, 0, server_thread, NULL, 0, NULL);
    WaitForSingleObject(g_ready, 10000);

    h = CreateFileA(g_pipe, GENERIC_READ | GENERIC_WRITE, 0, NULL,
                    OPEN_EXISTING, 0, NULL);
    if (h == INVALID_HANDLE_VALUE) {
        printf("[client] CreateFileA FAILED GetLastError=%lu\n",
               (unsigned long)GetLastError());
        goto done;
    }
    printf("[client] CreateFileA OK\n");

    {
        ULONG pid = 0;
        if (GetNamedPipeServerProcessId(h, &pid))
            printf("[client] GetNamedPipeServerProcessId OK: server pid=%lu (self=%lu)\n",
                   (unsigned long)pid, (unsigned long)GetCurrentProcessId());
        else
            printf("[client] GetNamedPipeServerProcessId FAILED GetLastError=%lu\n",
                   (unsigned long)GetLastError());
    }

    if (!WriteFile(h, msg, (DWORD)strlen(msg), &w, NULL)) {
        printf("[client] WriteFile FAILED GetLastError=%lu\n",
               (unsigned long)GetLastError());
        goto done;
    }
    printf("[client] wrote %lu bytes\n", (unsigned long)w);

    if (!ReadFile(h, buf, sizeof(buf), &n, NULL) || n == 0) {
        printf("[client] ReadFile FAILED GetLastError=%lu\n",
               (unsigned long)GetLastError());
        goto done;
    }
    printf("[client] read %lu bytes: \"%.*s\"\n", (unsigned long)n, (int)n, buf);

    if (n == strlen(msg) && memcmp(buf, msg, n) == 0) {
        printf("[main] PIPE ROUND TRIP OK\n");
        rc = 0;
    } else {
        printf("[main] PIPE ROUND TRIP MISMATCH\n");
    }

done:
    if (h != INVALID_HANDLE_VALUE) CloseHandle(h);
    WaitForSingleObject(th, 10000);
    printf("[main] server_rc=%d overall_rc=%d\n", g_server_rc, rc);
    return rc;
}
