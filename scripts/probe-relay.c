/* probe-relay.c -- a byte pipe from a TCP port to a Windows AF_UNIX socket, so
 * that a real third-party Wayland client can drive a compositor running on the
 * real machine.
 *
 *   usage: probe-relay.exe <tcp-port> <af_unix-path> [--timeout SEC]
 *                          [--chunk N] [--delay MS]
 *
 * WHY THIS EXISTS
 *
 * Under Wine, a Windows AF_UNIX socket is backed by a real host Unix socket
 * node, so `wayland-info` and `weston-multi-resource` -- native Linux C
 * programs built on libwayland, which have never heard of this project -- can
 * connect straight to a mingw-built .exe. That is what made stages 1 and 2
 * evidence rather than self-agreement.
 *
 * On the real guest that trick is gone. `afunix.sys` creates a reparse point on
 * NTFS, not a host socket node, and no Linux process can connect to it by any
 * path. The obvious fallback -- run our own `wc-probe.exe` as the client -- is
 * a matched pair of our own binaries, and a symmetric bug in our own wire
 * encoding would pass.
 *
 * There is no third-party Wayland client for Windows, and porting libwayland is
 * a project rather than a test. So move the bytes instead of the client: this
 * relay accepts a TCP connection on the guest's loopback and forwards it, byte
 * for byte, into the compositor's AF_UNIX socket. An `ssh -L <socket>:...`
 * forward puts the Linux end of that pipe in an `XDG_RUNTIME_DIR`, where a
 * stock libwayland client opens it without knowing it is not a compositor.
 *
 * WHAT THE RELAY DOES AND DOES NOT WEAKEN
 *
 * The Wayland wire protocol over the socket is a byte stream, and this forwards
 * it unaltered, so everything the compositor parses, dispatches and answers is
 * exactly what it would parse from a local client. Two things do change, and
 * both are worth stating:
 *
 *  - Ancillary data cannot cross a TCP hop. That would be fatal on Linux, where
 *    an `h` argument is an `SCM_RIGHTS` fd -- but on Windows an `h` is encoded
 *    *in band* as a length-prefixed descriptor (docs/design.md §2.3), so for
 *    this port there is nothing out of band to lose. It does mean the relay can
 *    only carry clients that reach the milestone under test before their first
 *    `h`, which is exactly why `weston-multi-resource` was chosen over
 *    `weston-flower` in the first place (scripts/wc-xdg.sh).
 *  - `SIO_AF_UNIX_GETPEERPID` on the compositor's side reports the relay's pid,
 *    not the client's. Truthfully so -- the relay really is the local peer.
 *    scripts/probe-accept.c measures that ioctl directly and does not go
 *    through here.
 *
 * The AF_UNIX accept path, afunix.sys, the framing, wepoll and the whole server
 * are on the real side of the pipe. That is what is being tested.
 *
 * `--chunk` and `--delay` deliberately fragment the client-to-server direction.
 * A Wayland message is 8 bytes of header plus arguments, so forwarding 3 bytes
 * at a time guarantees the server reads a partial message and has to wait for
 * the rest -- which is docs/design.md §7.8 item 3, wepoll under a partial read,
 * level-triggered. Nothing had produced one before.
 */
#include <winsock2.h>
#include <ws2tcpip.h>
#include <afunix.h>
#include <windows.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

static int g_chunk = 0;     /* 0 = whole buffer */
static int g_delay = 0;     /* ms between chunks, client->server only */
static volatile LONG g_conns = 0;

struct pump {
    SOCKET from, to;
    int fragment;
    const char *label;
};

static DWORD WINAPI pump_thread(LPVOID arg)
{
    struct pump *p = (struct pump *)arg;
    char buf[8192];
    long long total = 0;
    for (;;) {
        int n = recv(p->from, buf, (int)sizeof(buf), 0);
        if (n <= 0) break;
        {
            int off = 0;
            while (off < n) {
                int want = n - off;
                if (p->fragment && g_chunk > 0 && want > g_chunk)
                    want = g_chunk;
                {
                    int m = send(p->to, buf + off, want, 0);
                    if (m <= 0) goto done;
                    off += m;
                    total += m;
                }
                if (p->fragment && g_delay > 0)
                    Sleep(g_delay);
            }
        }
    }
done:
    printf("[relay] %s closed after %lld bytes\n", p->label, total);
    fflush(stdout);
    /* Half-close so the other side sees EOF and the peer thread unwinds. */
    shutdown(p->to, SD_SEND);
    return 0;
}

static DWORD WINAPI conn_thread(LPVOID arg)
{
    SOCKET tcp = (SOCKET)(ULONG_PTR)arg;
    SOCKET ux;
    struct sockaddr_un addr;
    struct pump up, down;
    HANDLE th;
    extern const char *g_path;

    ux = socket(AF_UNIX, SOCK_STREAM, 0);
    if (ux == INVALID_SOCKET) {
        printf("[relay] AF_UNIX socket FAILED err=%d\n", WSAGetLastError());
        closesocket(tcp);
        return 1;
    }
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    strncpy(addr.sun_path, g_path, UNIX_PATH_MAX - 1);
    if (connect(ux, (struct sockaddr *)&addr, sizeof(addr)) == SOCKET_ERROR) {
        printf("[relay] connect to \"%s\" FAILED err=%d\n", g_path,
               WSAGetLastError());
        fflush(stdout);
        closesocket(ux);
        closesocket(tcp);
        return 1;
    }
    printf("[relay] connection %ld: tcp <-> %s\n",
           (long)InterlockedIncrement(&g_conns), g_path);
    fflush(stdout);

    up.from = tcp; up.to = ux; up.fragment = 1; up.label = "client->server";
    down.from = ux; down.to = tcp; down.fragment = 0; down.label = "server->client";

    th = CreateThread(NULL, 0, pump_thread, &down, 0, NULL);
    pump_thread(&up);
    WaitForSingleObject(th, 5000);
    CloseHandle(th);
    closesocket(ux);
    closesocket(tcp);
    return 0;
}

const char *g_path;

int main(int argc, char **argv)
{
    WSADATA wsa;
    SOCKET ls;
    struct sockaddr_in sin;
    int port, i, timeout = 60;
    DWORD start;

    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc < 3) {
        printf("usage: probe-relay.exe <tcp-port> <af_unix-path> "
               "[--timeout SEC] [--chunk N] [--delay MS]\n");
        return 2;
    }
    port = atoi(argv[1]);
    g_path = argv[2];
    for (i = 3; i < argc; i++) {
        if (!strcmp(argv[i], "--timeout") && i + 1 < argc) timeout = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--chunk") && i + 1 < argc) g_chunk = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--delay") && i + 1 < argc) g_delay = atoi(argv[++i]);
    }

    if (WSAStartup(MAKEWORD(2, 2), &wsa) != 0) return 1;

    ls = socket(AF_INET, SOCK_STREAM, 0);
    if (ls == INVALID_SOCKET) {
        printf("[relay] socket FAILED err=%d\n", WSAGetLastError());
        return 1;
    }
    memset(&sin, 0, sizeof(sin));
    sin.sin_family = AF_INET;
    sin.sin_port = htons((u_short)port);
    /* Loopback only. The relay is a hole in the compositor's isolation for the
       duration of a test, and nothing outside the guest should be able to reach
       it; the ssh forward terminates on the guest's own loopback. */
    sin.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (bind(ls, (struct sockaddr *)&sin, sizeof(sin)) == SOCKET_ERROR) {
        printf("[relay] bind FAILED err=%d\n", WSAGetLastError());
        return 1;
    }
    if (listen(ls, 8) == SOCKET_ERROR) {
        printf("[relay] listen FAILED err=%d\n", WSAGetLastError());
        return 1;
    }
    printf("[relay] listening on 127.0.0.1:%d -> %s (chunk=%d delay=%dms "
           "timeout=%ds)\n", port, g_path, g_chunk, g_delay, timeout);

    start = GetTickCount();
    for (;;) {
        fd_set rd;
        struct timeval tv;
        FD_ZERO(&rd);
        FD_SET(ls, &rd);
        tv.tv_sec = 1; tv.tv_usec = 0;
        if (select(0, &rd, NULL, NULL, &tv) > 0) {
            SOCKET c = accept(ls, NULL, NULL);
            if (c != INVALID_SOCKET) {
                HANDLE t = CreateThread(NULL, 0, conn_thread,
                                        (LPVOID)(ULONG_PTR)c, 0, NULL);
                if (t) CloseHandle(t);
            }
        }
        /* STAGE 14. `--timeout 0` is "for as long as somebody wants it", which
         * is what a warm session needs: every other caller of this program is a
         * test with a deadline, and one of them being a product does not make
         * the deadline wrong for the rest. Written as a guard rather than as a
         * large default, because a large default is a relay that outlives the
         * compositor it was pointed at and then accepts into nothing. */
        if (timeout > 0 && (GetTickCount() - start) / 1000 >= (DWORD)timeout) {
            printf("[relay] timeout reached; exiting\n");
            break;
        }
    }
    closesocket(ls);
    WSACleanup();
    return 0;
}
