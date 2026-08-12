/* probe-pipeproc.c -- two-process named-pipe test, to check that Wine's
 * GetNamedPipeClientProcessId/GetNamedPipeServerProcessId report the *other*
 * process's pid and not just our own.
 *
 * usage: probe-pipeproc.exe server|client \\.\pipe\name
 */
#include <windows.h>
#include <stdio.h>
#include <string.h>

int main(int argc, char **argv)
{
    const char *role, *pipe;
    char buf[64];
    DWORD n = 0, w = 0;
    ULONG pid = 0;

    if (argc < 3) {
        printf("usage: probe-pipeproc.exe server|client <pipename>\n");
        return 2;
    }
    role = argv[1];
    pipe = argv[2];

    if (strcmp(role, "server") == 0) {
        HANDLE h = CreateNamedPipeA(pipe, PIPE_ACCESS_DUPLEX,
                                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                                    PIPE_UNLIMITED_INSTANCES, 4096, 4096, 0, NULL);
        if (h == INVALID_HANDLE_VALUE) {
            printf("[server] CreateNamedPipeA FAILED %lu\n",
                   (unsigned long)GetLastError());
            return 1;
        }
        printf("[server] listening on %s, pid=%lu\n", pipe,
               (unsigned long)GetCurrentProcessId());
        fflush(stdout);
        if (!ConnectNamedPipe(h, NULL) && GetLastError() != ERROR_PIPE_CONNECTED) {
            printf("[server] ConnectNamedPipe FAILED %lu\n",
                   (unsigned long)GetLastError());
            return 1;
        }
        if (GetNamedPipeClientProcessId(h, &pid))
            printf("[server] client pid=%lu (self=%lu)\n", (unsigned long)pid,
                   (unsigned long)GetCurrentProcessId());
        else
            printf("[server] GetNamedPipeClientProcessId FAILED %lu\n",
                   (unsigned long)GetLastError());
        if (!ReadFile(h, buf, sizeof(buf), &n, NULL))
            printf("[server] ReadFile FAILED %lu\n", (unsigned long)GetLastError());
        printf("[server] read %lu: \"%.*s\"\n", (unsigned long)n, (int)n, buf);
        WriteFile(h, buf, n, &w, NULL);
        FlushFileBuffers(h);
        DisconnectNamedPipe(h);
        CloseHandle(h);
        printf("[server] done\n");
        return 0;
    } else {
        static const char msg[] = "PING-pipeproc";
        HANDLE h = CreateFileA(pipe, GENERIC_READ | GENERIC_WRITE, 0, NULL,
                               OPEN_EXISTING, 0, NULL);
        if (h == INVALID_HANDLE_VALUE) {
            printf("[client] CreateFileA FAILED %lu\n",
                   (unsigned long)GetLastError());
            return 1;
        }
        printf("[client] connected, pid=%lu\n",
               (unsigned long)GetCurrentProcessId());
        if (GetNamedPipeServerProcessId(h, &pid))
            printf("[client] server pid=%lu (self=%lu)\n", (unsigned long)pid,
                   (unsigned long)GetCurrentProcessId());
        else
            printf("[client] GetNamedPipeServerProcessId FAILED %lu\n",
                   (unsigned long)GetLastError());
        WriteFile(h, msg, (DWORD)strlen(msg), &w, NULL);
        if (!ReadFile(h, buf, sizeof(buf), &n, NULL))
            printf("[client] ReadFile FAILED %lu\n", (unsigned long)GetLastError());
        printf("[client] read %lu: \"%.*s\"\n", (unsigned long)n, (int)n, buf);
        CloseHandle(h);
        return (n == strlen(msg) && memcmp(buf, msg, n) == 0) ? 0 : 1;
    }
}
