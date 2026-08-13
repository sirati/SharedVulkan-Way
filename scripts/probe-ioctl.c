/* probe-ioctl.c -- step 5 in detail.
 *
 * Establishes a real AF_UNIX connection (server thread + client), then probes
 * ioctl/getsockopt/getsockname behaviour on it, with controls so we can tell
 * "SIO_AF_UNIX_GETPEERPID specifically is unimplemented" apart from "WSAIoctl
 * does not work at all on this socket".
 */
#include <winsock2.h>
#include <ws2tcpip.h>
#include <afunix.h>
#include <windows.h>
#include <stdio.h>
#include <string.h>

#ifndef _WSAIOR
#define _WSAIOR(x,y) (IOC_OUT|(x)|(y))
#endif

/* a vendor ioctl that certainly does not exist anywhere */
#define SIO_BOGUS_VENDOR_IOCTL _WSAIOR(IOC_VENDOR, 4095)

static const char *g_path = "C:\\wc-ioctl.sock";
static HANDLE g_ready;
static SOCKET g_accepted = INVALID_SOCKET;

static void fill_un(struct sockaddr_un *a, const char *path)
{
    memset(a, 0, sizeof(*a));
    a->sun_family = AF_UNIX;
    strncpy(a->sun_path, path, UNIX_PATH_MAX - 1);
}

static DWORD WINAPI server_thread(LPVOID unused)
{
    SOCKET ls;
    struct sockaddr_un addr;
    (void)unused;

    DeleteFileA(g_path);
    ls = socket(AF_UNIX, SOCK_STREAM, 0);
    fill_un(&addr, g_path);
    if (ls == INVALID_SOCKET ||
        bind(ls, (struct sockaddr *)&addr, sizeof(addr)) == SOCKET_ERROR ||
        listen(ls, 4) == SOCKET_ERROR) {
        printf("[server] setup FAILED err=%d\n", WSAGetLastError());
        SetEvent(g_ready);
        return 0;
    }
    SetEvent(g_ready);
    g_accepted = accept(ls, NULL, NULL);
    if (g_accepted == INVALID_SOCKET)
        printf("[server] accept FAILED err=%d\n", WSAGetLastError());
    closesocket(ls);
    return 0;
}

static void probe_ioctl(const char *label, SOCKET s, DWORD code)
{
    DWORD out = 0, ret = 0;
    int rc = WSAIoctl(s, code, NULL, 0, &out, sizeof(out), &ret, NULL, NULL);
    if (rc == 0)
        printf("  %-34s OK  out=%lu bytes=%lu\n", label, (unsigned long)out,
               (unsigned long)ret);
    else
        printf("  %-34s ERR rc=%d WSAGetLastError=%d\n", label, rc,
               WSAGetLastError());
}

static void report(const char *who, SOCKET s)
{
    printf("[%s] socket=%p, own pid=%lu\n", who, (void *)s,
           (unsigned long)GetCurrentProcessId());

    printf("  code SIO_AF_UNIX_GETPEERPID = 0x%08lx\n",
           (unsigned long)SIO_AF_UNIX_GETPEERPID);
    probe_ioctl("WSAIoctl SIO_AF_UNIX_GETPEERPID", s, SIO_AF_UNIX_GETPEERPID);
    probe_ioctl("WSAIoctl bogus vendor ioctl", s, SIO_BOGUS_VENDOR_IOCTL);

    {   /* control 1: a plain, universally implemented ioctlsocket */
        u_long avail = 0;
        int rc = ioctlsocket(s, FIONREAD, &avail);
        if (rc == 0)
            printf("  %-34s OK  avail=%lu\n", "ioctlsocket FIONREAD", (unsigned long)avail);
        else
            printf("  %-34s ERR rc=%d err=%d\n", "ioctlsocket FIONREAD", rc,
                   WSAGetLastError());
    }
    {   /* control 2: getsockopt SO_TYPE */
        int type = 0, len = sizeof(type);
        int rc = getsockopt(s, SOL_SOCKET, SO_TYPE, (char *)&type, &len);
        if (rc == 0)
            printf("  %-34s OK  type=%d (SOCK_STREAM=%d)\n", "getsockopt SO_TYPE",
                   type, SOCK_STREAM);
        else
            printf("  %-34s ERR rc=%d err=%d\n", "getsockopt SO_TYPE", rc,
                   WSAGetLastError());
    }
    {   /* does the address round trip? matters for a compositor that wants to
           know which socket path a client came in on */
        struct sockaddr_un a;
        int len = sizeof(a);
        memset(&a, 0, sizeof(a));
        if (getsockname(s, (struct sockaddr *)&a, &len) == 0)
            printf("  %-34s OK  len=%d family=%d path=\"%.*s\"\n", "getsockname",
                   len, (int)a.sun_family, UNIX_PATH_MAX, a.sun_path);
        else
            printf("  %-34s ERR err=%d\n", "getsockname", WSAGetLastError());
        memset(&a, 0, sizeof(a));
        len = sizeof(a);
        if (getpeername(s, (struct sockaddr *)&a, &len) == 0)
            printf("  %-34s OK  len=%d family=%d path=\"%.*s\"\n", "getpeername",
                   len, (int)a.sun_family, UNIX_PATH_MAX, a.sun_path);
        else
            printf("  %-34s ERR err=%d\n", "getpeername", WSAGetLastError());
    }
    fflush(stdout);
}

int main(void)
{
    WSADATA wsa;
    SOCKET c;
    struct sockaddr_un addr;
    HANDLE th;

    if (WSAStartup(MAKEWORD(2, 2), &wsa) != 0)
        return 1;

    g_ready = CreateEventA(NULL, TRUE, FALSE, NULL);
    th = CreateThread(NULL, 0, server_thread, NULL, 0, NULL);
    WaitForSingleObject(g_ready, 10000);

    c = socket(AF_UNIX, SOCK_STREAM, 0);
    if (c == INVALID_SOCKET) {
        printf("[client] socket FAILED err=%d -- AF_UNIX unavailable\n",
               WSAGetLastError());
        return 1;
    }
    fill_un(&addr, g_path);
    if (connect(c, (struct sockaddr *)&addr, sizeof(addr)) == SOCKET_ERROR) {
        printf("[client] connect FAILED err=%d\n", WSAGetLastError());
        return 1;
    }
    WaitForSingleObject(th, 10000);
    printf("connection established\n");

    report("client", c);
    if (g_accepted != INVALID_SOCKET)
        report("server-accepted", g_accepted);

    /* also: a fresh, unconnected AF_UNIX socket -- does the ioctl fail
       differently before there is a peer? */
    {
        SOCKET u = socket(AF_UNIX, SOCK_STREAM, 0);
        printf("[unconnected AF_UNIX socket]\n");
        probe_ioctl("WSAIoctl SIO_AF_UNIX_GETPEERPID", u, SIO_AF_UNIX_GETPEERPID);
        closesocket(u);
    }
    /* and on a TCP socket, where real Windows also returns an error, to see
       whether Wine's error is address-family-specific */
    {
        SOCKET t = socket(AF_INET, SOCK_STREAM, 0);
        printf("[unconnected AF_INET socket]\n");
        probe_ioctl("WSAIoctl SIO_AF_UNIX_GETPEERPID", t, SIO_AF_UNIX_GETPEERPID);
        closesocket(t);
    }

    if (g_accepted != INVALID_SOCKET) closesocket(g_accepted);
    closesocket(c);
    WSACleanup();
    return 0;
}
