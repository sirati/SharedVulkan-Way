/* probe-unixproc.c -- two-process AF_UNIX test (the shape a compositor actually
 * has: server and client are separate processes, so SIO_AF_UNIX_GETPEERPID
 * would return a *different* pid than our own if it worked).
 *
 * usage: probe-unixproc.exe server <path>
 *        probe-unixproc.exe client <path>
 */
#include <winsock2.h>
#include <ws2tcpip.h>
#include <afunix.h>
#include <windows.h>
#include <stdio.h>
#include <string.h>

static void fill_un(struct sockaddr_un *a, const char *path)
{
    memset(a, 0, sizeof(*a));
    a->sun_family = AF_UNIX;
    strncpy(a->sun_path, path, UNIX_PATH_MAX - 1);
}

static void try_getpeerpid(const char *who, SOCKET s)
{
    DWORD pid = 0, ret = 0;
    int rc = WSAIoctl(s, SIO_AF_UNIX_GETPEERPID, NULL, 0, &pid, sizeof(pid),
                      &ret, NULL, NULL);
    if (rc == 0)
        printf("[%s] GETPEERPID OK: peer=%lu self=%lu bytes=%lu\n", who,
               (unsigned long)pid, (unsigned long)GetCurrentProcessId(),
               (unsigned long)ret);
    else
        printf("[%s] GETPEERPID FAILED rc=%d err=%d (self pid=%lu)\n", who, rc,
               WSAGetLastError(), (unsigned long)GetCurrentProcessId());
    fflush(stdout);
}

int main(int argc, char **argv)
{
    WSADATA wsa;
    struct sockaddr_un addr;
    const char *role, *path;
    char buf[64];
    int n;

    if (argc < 3) {
        printf("usage: probe-unixproc.exe server|client <path>\n");
        return 2;
    }
    role = argv[1];
    path = argv[2];

    if (WSAStartup(MAKEWORD(2, 2), &wsa) != 0) {
        printf("WSAStartup failed\n");
        return 1;
    }

    if (strcmp(role, "server") == 0) {
        SOCKET ls, cs;
        if (DeleteFileA(path))
            printf("[server] removed stale %s\n", path);
        ls = socket(AF_UNIX, SOCK_STREAM, 0);
        if (ls == INVALID_SOCKET) {
            printf("[server] socket FAILED err=%d\n", WSAGetLastError());
            return 1;
        }
        fill_un(&addr, path);
        if (bind(ls, (struct sockaddr *)&addr, sizeof(addr)) == SOCKET_ERROR) {
            printf("[server] bind FAILED err=%d\n", WSAGetLastError());
            return 1;
        }
        if (listen(ls, 4) == SOCKET_ERROR) {
            printf("[server] listen FAILED err=%d\n", WSAGetLastError());
            return 1;
        }
        printf("[server] listening on %s, pid=%lu\n", path,
               (unsigned long)GetCurrentProcessId());
        fflush(stdout);

        cs = accept(ls, NULL, NULL);
        if (cs == INVALID_SOCKET) {
            printf("[server] accept FAILED err=%d\n", WSAGetLastError());
            return 1;
        }
        printf("[server] accept OK\n");
        try_getpeerpid("server", cs);
        n = recv(cs, buf, sizeof(buf), 0);
        printf("[server] recv=%d \"%.*s\"\n", n, n > 0 ? n : 0, buf);
        if (n > 0)
            send(cs, buf, n, 0);
        closesocket(cs);
        closesocket(ls);
        printf("[server] done\n");
        return 0;
    } else {
        SOCKET s;
        static const char msg[] = "PING-crossproc";
        s = socket(AF_UNIX, SOCK_STREAM, 0);
        if (s == INVALID_SOCKET) {
            printf("[client] socket FAILED err=%d\n", WSAGetLastError());
            return 1;
        }
        fill_un(&addr, path);
        if (connect(s, (struct sockaddr *)&addr, sizeof(addr)) == SOCKET_ERROR) {
            printf("[client] connect FAILED err=%d\n", WSAGetLastError());
            return 1;
        }
        printf("[client] connect OK, pid=%lu\n",
               (unsigned long)GetCurrentProcessId());
        try_getpeerpid("client", s);
        send(s, msg, (int)strlen(msg), 0);
        n = recv(s, buf, sizeof(buf), 0);
        printf("[client] recv=%d \"%.*s\"\n", n, n > 0 ? n : 0, buf);
        closesocket(s);
        return (n == (int)strlen(msg) && memcmp(buf, msg, n) == 0) ? 0 : 1;
    }
}
