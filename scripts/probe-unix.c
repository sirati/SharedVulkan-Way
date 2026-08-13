/* probe-unix.c -- step 3 + step 5:
 *   full AF_UNIX loopback in one process, server thread + client on main thread,
 *   bound to a real filesystem path (Windows AF_UNIX semantics), plus
 *   WSAIoctl(SIO_AF_UNIX_GETPEERPID) on both ends of the connection.
 *
 * usage: probe-unix.exe [socket-path]     default C:\wc-test.sock
 */
#include <winsock2.h>
#include <ws2tcpip.h>
#include <afunix.h>
#include <windows.h>
#include <stdio.h>
#include <string.h>

static char g_path[UNIX_PATH_MAX];
static HANDLE g_listening;      /* server signals once listen() is up */
static int g_server_rc = -1;

static void fill_un(struct sockaddr_un *a)
{
    memset(a, 0, sizeof(*a));
    a->sun_family = AF_UNIX;
    strncpy(a->sun_path, g_path, UNIX_PATH_MAX - 1);
}

static void try_getpeerpid(const char *who, SOCKET s)
{
    DWORD pid = 0, ret = 0;
    int rc = WSAIoctl(s, SIO_AF_UNIX_GETPEERPID, NULL, 0, &pid, sizeof(pid),
                      &ret, NULL, NULL);
    if (rc == 0)
        printf("[%s] SIO_AF_UNIX_GETPEERPID OK: peer pid=%lu (bytes=%lu), our pid=%lu\n",
               who, (unsigned long)pid, (unsigned long)ret,
               (unsigned long)GetCurrentProcessId());
    else
        printf("[%s] SIO_AF_UNIX_GETPEERPID FAILED rc=%d WSAGetLastError=%d\n",
               who, rc, WSAGetLastError());
    fflush(stdout);
}

static DWORD WINAPI server_thread(LPVOID unused)
{
    SOCKET ls = INVALID_SOCKET, cs = INVALID_SOCKET;
    struct sockaddr_un addr;
    char buf[64];
    int n;

    (void)unused;

    ls = socket(AF_UNIX, SOCK_STREAM, 0);
    if (ls == INVALID_SOCKET) {
        printf("[server] socket FAILED err=%d\n", WSAGetLastError());
        goto out;
    }
    printf("[server] socket OK\n");

    fill_un(&addr);
    if (bind(ls, (struct sockaddr *)&addr, sizeof(addr)) == SOCKET_ERROR) {
        printf("[server] bind(\"%s\") FAILED err=%d\n", g_path, WSAGetLastError());
        goto out;
    }
    printf("[server] bind(\"%s\") OK\n", g_path);

    if (listen(ls, 4) == SOCKET_ERROR) {
        printf("[server] listen FAILED err=%d\n", WSAGetLastError());
        goto out;
    }
    printf("[server] listen OK\n");
    fflush(stdout);
    SetEvent(g_listening);

    {
        struct sockaddr_un peer;
        int plen = sizeof(peer);
        memset(&peer, 0, sizeof(peer));
        cs = accept(ls, (struct sockaddr *)&peer, &plen);
        if (cs == INVALID_SOCKET) {
            printf("[server] accept FAILED err=%d\n", WSAGetLastError());
            goto out;
        }
        printf("[server] accept OK (peer addrlen=%d family=%d path=\"%.*s\")\n",
               plen, (int)peer.sun_family, UNIX_PATH_MAX, peer.sun_path);
    }

    try_getpeerpid("server", cs);

    n = recv(cs, buf, sizeof(buf), 0);
    if (n <= 0) {
        printf("[server] recv returned %d err=%d\n", n, WSAGetLastError());
        goto out;
    }
    printf("[server] recv %d bytes: \"%.*s\"\n", n, n, buf);

    if (send(cs, buf, n, 0) != n) {
        printf("[server] send(echo) FAILED err=%d\n", WSAGetLastError());
        goto out;
    }
    printf("[server] echoed %d bytes back\n", n);
    g_server_rc = 0;

out:
    fflush(stdout);
    if (cs != INVALID_SOCKET) closesocket(cs);
    if (ls != INVALID_SOCKET) closesocket(ls);
    SetEvent(g_listening);      /* make sure client is never stuck */
    return 0;
}

int main(int argc, char **argv)
{
    WSADATA wsa;
    SOCKET s = INVALID_SOCKET;
    struct sockaddr_un addr;
    HANDLE th;
    char buf[64];
    static const char msg[] = "PING-wayland";
    int n, rc = 1;

    strncpy(g_path, (argc > 1) ? argv[1] : "C:\\wc-test.sock", sizeof(g_path) - 1);

    if (WSAStartup(MAKEWORD(2, 2), &wsa) != 0) {
        printf("WSAStartup failed\n");
        return 1;
    }

    /* Windows AF_UNIX bind() creates the file and fails if it already exists. */
    if (DeleteFileA(g_path))
        printf("[main] removed stale socket file %s\n", g_path);

    g_listening = CreateEventA(NULL, TRUE, FALSE, NULL);
    th = CreateThread(NULL, 0, server_thread, NULL, 0, NULL);
    if (!th) {
        printf("CreateThread failed %lu\n", (unsigned long)GetLastError());
        return 1;
    }
    WaitForSingleObject(g_listening, 10000);

    s = socket(AF_UNIX, SOCK_STREAM, 0);
    if (s == INVALID_SOCKET) {
        printf("[client] socket FAILED err=%d\n", WSAGetLastError());
        goto done;
    }
    printf("[client] socket OK\n");

    fill_un(&addr);
    if (connect(s, (struct sockaddr *)&addr, sizeof(addr)) == SOCKET_ERROR) {
        printf("[client] connect(\"%s\") FAILED err=%d\n", g_path, WSAGetLastError());
        goto done;
    }
    printf("[client] connect OK\n");

    try_getpeerpid("client", s);

    if (send(s, msg, (int)strlen(msg), 0) != (int)strlen(msg)) {
        printf("[client] send FAILED err=%d\n", WSAGetLastError());
        goto done;
    }
    printf("[client] sent \"%s\"\n", msg);

    n = recv(s, buf, sizeof(buf), 0);
    if (n <= 0) {
        printf("[client] recv returned %d err=%d\n", n, WSAGetLastError());
        goto done;
    }
    printf("[client] recv %d bytes: \"%.*s\"\n", n, n, buf);

    if (n == (int)strlen(msg) && memcmp(buf, msg, n) == 0) {
        printf("[main] ROUND TRIP OK\n");
        rc = 0;
    } else {
        printf("[main] ROUND TRIP MISMATCH\n");
    }

done:
    if (s != INVALID_SOCKET) closesocket(s);
    WaitForSingleObject(th, 10000);
    printf("[main] server_rc=%d overall_rc=%d\n", g_server_rc, rc);
    {
        DWORD attr = GetFileAttributesA(g_path);
        printf("[main] GetFileAttributes(\"%s\") = 0x%lx (err=%lu)\n", g_path,
               (unsigned long)attr, (unsigned long)GetLastError());
    }
    WSACleanup();
    return rc;
}
