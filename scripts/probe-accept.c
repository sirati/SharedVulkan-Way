/* probe-accept.c -- the two questions Wine could not answer, asked on the real
 * machine.
 *
 *   usage: probe-accept.exe server <path>
 *          probe-accept.exe client <path>
 *
 * Both roles are separate *processes* on purpose: `SIO_AF_UNIX_GETPEERPID` is
 * only interesting when the peer's pid is not our own, and the accept path is
 * only interesting when a real afunix.sys connection crosses a process
 * boundary.
 *
 * QUESTION 1 -- SIO_AF_UNIX_GETPEERPID (docs/waycompose-design.md §7.8 item 1)
 *
 * Stage 1 chose AF_UNIX over a named pipe partly because this ioctl exists.
 * Wine implements no `IOC_VENDOR` ioctl at all, so the call has compiled and
 * never once executed. The client sends its own pid as text over the socket;
 * the server asks the kernel for the peer pid and compares the two. Agreement
 * is the only result that proves anything: a plausible-looking non-zero number
 * that happens to be wrong would be worse than a failure.
 *
 * QUESTION 2 -- what `accept()` writes into the peer address
 *              (docs/waycompose-design.md §7.8 item 2)
 *
 * `uds_windows::UnixListener::accept` calls `accept()` with a real
 * `sockaddr_un` and then rejects the connection outright if `len != 0 &&
 * sun_family != AF_UNIX` (uds_windows 1.2.1, src/stdnet/mod.rs:148). A Wayland
 * client never `bind()`s, so it has no address to report. Under Wine 11.14
 * staging the measured answer was `len = 110, sun_family = 0`, which trips that
 * exact branch and made the server accept nobody. `wc_transport::accept` passes
 * a null address so the question never arises -- but whether the *stock* crate
 * would have worked here is worth knowing, because it decides whether stage 1's
 * workaround is a permanent requirement or a Wine detail.
 *
 * So the buffer is poisoned with 0xCC before the call and the raw result is
 * printed: whatever is reported is what the platform really wrote, not what a
 * zeroed buffer flattered us into reading.
 */
#include <winsock2.h>
#include <ws2tcpip.h>
#include <afunix.h>
#include <windows.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

static void fill_un(struct sockaddr_un *a, const char *path)
{
    memset(a, 0, sizeof(*a));
    a->sun_family = AF_UNIX;
    strncpy(a->sun_path, path, UNIX_PATH_MAX - 1);
}

/* Ask the kernel for the peer's pid. Returns the pid, or 0 on failure. */
static DWORD get_peer_pid(const char *who, SOCKET s)
{
    DWORD pid = 0, ret = 0;
    int rc = WSAIoctl(s, SIO_AF_UNIX_GETPEERPID, NULL, 0, &pid, sizeof(pid),
                      &ret, NULL, NULL);
    if (rc == 0) {
        printf("[%s] GETPEERPID ok: peer=%lu bytes=%lu (self=%lu)\n", who,
               (unsigned long)pid, (unsigned long)ret,
               (unsigned long)GetCurrentProcessId());
        return pid;
    }
    printf("[%s] GETPEERPID FAILED rc=%d WSAGetLastError=%d (self=%lu)\n", who,
           rc, WSAGetLastError(), (unsigned long)GetCurrentProcessId());
    return 0;
}

static void dump_addr(const char *label, const struct sockaddr_un *a, int len)
{
    const unsigned char *raw = (const unsigned char *)a;
    int i, n = len > 16 ? 16 : (len < 0 ? 0 : len);
    printf("  %s: len=%d sun_family=%d raw[0..%d]=", label, len,
           (int)a->sun_family, n);
    for (i = 0; i < n; i++)
        printf("%02x", raw[i]);
    printf(" path=\"%.*s\"\n", UNIX_PATH_MAX, a->sun_path);
}

int main(int argc, char **argv)
{
    WSADATA wsa;
    struct sockaddr_un addr;
    const char *role, *path;
    char buf[128];
    int n;

    setvbuf(stdout, NULL, _IONBF, 0);

    if (argc < 3) {
        printf("usage: probe-accept.exe server|client <path>\n");
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
        struct sockaddr_un peer;
        int peerlen = (int)sizeof(peer);
        DWORD peer_pid, claimed_pid = 0;
        int verdict_ok = 1;

        DeleteFileA(path);
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

        /* Poison, so that anything we read back was really written by the
           platform. */
        memset(&peer, 0xCC, sizeof(peer));
        cs = accept(ls, (struct sockaddr *)&peer, &peerlen);
        if (cs == INVALID_SOCKET) {
            printf("[server] accept FAILED err=%d\n", WSAGetLastError());
            return 1;
        }
        printf("[server] accept ok\n");
        dump_addr("accept peer address", &peer, peerlen);

        /* The exact predicate from uds_windows 1.2.1 src/stdnet/mod.rs:148. */
        if (peerlen == 0) {
            printf("  uds_windows verdict: WOULD ACCEPT (len == 0, treated as "
                   "unnamed)\n");
        } else if (peer.sun_family == AF_UNIX) {
            printf("  uds_windows verdict: WOULD ACCEPT (sun_family == "
                   "AF_UNIX)\n");
        } else {
            printf("  uds_windows verdict: WOULD REJECT -- \"file descriptor "
                   "did not correspond to a Unix socket\"\n");
        }

        {   /* getpeername on the accepted socket asks the same question a
               different way. */
            struct sockaddr_un pn;
            int pl = (int)sizeof(pn);
            memset(&pn, 0xCC, sizeof(pn));
            if (getpeername(cs, (struct sockaddr *)&pn, &pl) == 0)
                dump_addr("getpeername", &pn, pl);
            else
                printf("  getpeername: ERR err=%d\n", WSAGetLastError());
            memset(&pn, 0xCC, sizeof(pn));
            pl = (int)sizeof(pn);
            if (getsockname(cs, (struct sockaddr *)&pn, &pl) == 0)
                dump_addr("getsockname", &pn, pl);
            else
                printf("  getsockname: ERR err=%d\n", WSAGetLastError());
        }

        peer_pid = get_peer_pid("server", cs);

        n = recv(cs, buf, (int)sizeof(buf) - 1, 0);
        if (n > 0) {
            buf[n] = 0;
            if (strncmp(buf, "PID:", 4) == 0)
                claimed_pid = (DWORD)strtoul(buf + 4, NULL, 10);
            printf("[server] recv=%d \"%s\"\n", n, buf);
            send(cs, buf, n, 0);
        } else {
            printf("[server] recv=%d err=%d\n", n, WSAGetLastError());
            verdict_ok = 0;
        }

        if (peer_pid == 0) {
            printf("[server] RESULT: GETPEERPID did not work\n");
            verdict_ok = 0;
        } else if (peer_pid == claimed_pid) {
            printf("[server] RESULT: GETPEERPID returned the client's real pid "
                   "(%lu) -- correct\n", (unsigned long)peer_pid);
        } else {
            printf("[server] RESULT: GETPEERPID returned %lu but the client is "
                   "%lu -- WRONG\n", (unsigned long)peer_pid,
                   (unsigned long)claimed_pid);
            verdict_ok = 0;
        }

        closesocket(cs);
        closesocket(ls);
        DeleteFileA(path);
        printf("[server] done\n");
        return verdict_ok ? 0 : 1;
    } else {
        SOCKET s;
        char msg[64];
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
        printf("[client] connect ok, pid=%lu\n",
               (unsigned long)GetCurrentProcessId());
        /* The client's own view: on a connected socket the peer is the
           *server*, so this is a second, independent sample of the ioctl. */
        get_peer_pid("client", s);

        snprintf(msg, sizeof(msg), "PID:%lu", (unsigned long)GetCurrentProcessId());
        send(s, msg, (int)strlen(msg), 0);
        n = recv(s, buf, (int)sizeof(buf) - 1, 0);
        if (n > 0) buf[n] = 0;
        printf("[client] echo=%d \"%s\"\n", n, n > 0 ? buf : "");
        closesocket(s);
        return (n == (int)strlen(msg) && memcmp(buf, msg, n) == 0) ? 0 : 1;
    }
}
