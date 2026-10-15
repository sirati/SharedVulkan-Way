/* probe-tunnel-latency.c -- how long does a small message from the guest take
 * to cross the launcher's ssh tunnel when nothing is coming the other way?
 * (docs/waycompose-design.md §88)
 *
 *   usage: probe-tunnel-latency.exe <tcp-port> <secs>
 *
 * Listens on 127.0.0.1:<port> for one connection -- the guest end of an
 * `ssh.exe -N -R <host-socket>:127.0.0.1:<port>` forward, which is the shape of
 * the launcher's Wayland tunnel -- and then, for <secs> seconds:
 *
 *  - every 47 ms sends 16 bytes unprompted: 'U', a sequence number and the
 *    send time in microseconds of QueryPerformanceCounter. 47 ms is not a
 *    multiple of the 15.6 ms scheduler tick, so a delay that is quantised to
 *    the tick shows up as spread rather than as a constant;
 *  - echoes every 8-byte message it receives at once, as 'E', the host's
 *    8 bytes and the time, so the host can measure the prompted round trip.
 *
 * The socket has TCP_NODELAY and an ACK per segment, as the relay has, so
 * what is measured is ssh.exe and the link, not this program.
 *
 * The host's half is scripts/wc-tunnel-latency.py. */
#include <winsock2.h>
#include <ws2tcpip.h>
#include <mstcpip.h>
#include <windows.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

static SOCKET g_c;
static LARGE_INTEGER g_freq;
static volatile LONG g_stop;
static CRITICAL_SECTION g_send;

static unsigned long long now_us(void)
{
    LARGE_INTEGER t;
    QueryPerformanceCounter(&t);
    return (unsigned long long)(t.QuadPart * 1000000.0 / g_freq.QuadPart);
}

static void put(char tag, unsigned long long a, unsigned long long b)
{
    char m[24];
    m[0] = tag;
    memset(m + 1, 0, 7);
    memcpy(m + 8, &a, 8);
    memcpy(m + 16, &b, 8);
    EnterCriticalSection(&g_send);
    send(g_c, m, sizeof(m), 0);
    LeaveCriticalSection(&g_send);
}

static DWORD WINAPI echo(LPVOID unused)
{
    unsigned long long v;
    char buf[8];
    int have = 0;
    (void)unused;
    while (!g_stop) {
        int n = recv(g_c, buf + have, 8 - have, 0);
        if (n <= 0) break;
        have += n;
        if (have == 8) {
            memcpy(&v, buf, 8);
            put('E', v, now_us());
            have = 0;
        }
    }
    g_stop = 1;
    return 0;
}

int main(int argc, char **argv)
{
    WSADATA wsa;
    SOCKET ls;
    struct sockaddr_in sin;
    BOOL one = TRUE;
    DWORD freq = 1, got = 0;
    HANDLE timer;
    unsigned long long seq = 0, end;
    int secs;

    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc < 3) {
        printf("usage: probe-tunnel-latency.exe <tcp-port> <secs>\n");
        return 2;
    }
    secs = atoi(argv[2]);
    QueryPerformanceFrequency(&g_freq);
    InitializeCriticalSection(&g_send);
    WSAStartup(MAKEWORD(2, 2), &wsa);
    ls = socket(AF_INET, SOCK_STREAM, 0);
    memset(&sin, 0, sizeof(sin));
    sin.sin_family = AF_INET;
    sin.sin_port = htons((u_short)atoi(argv[1]));
    sin.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (bind(ls, (struct sockaddr *)&sin, sizeof(sin)) || listen(ls, 1)) {
        printf("bind/listen failed %d\n", WSAGetLastError());
        return 1;
    }
    printf("listening\n");
    g_c = accept(ls, NULL, NULL);
    setsockopt(g_c, IPPROTO_TCP, TCP_NODELAY, (const char *)&one, sizeof(one));
    WSAIoctl(g_c, SIO_TCP_SET_ACK_FREQUENCY, &freq, sizeof(freq), NULL, 0, &got, NULL, NULL);
    printf("connected\n");
    CloseHandle(CreateThread(NULL, 0, echo, NULL, 0, NULL));
    timer = CreateWaitableTimerExW(NULL, NULL, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS);
    end = now_us() + (unsigned long long)secs * 1000000ULL;
    while (!g_stop && now_us() < end) {
        LARGE_INTEGER due;
        due.QuadPart = -470000; /* 47 ms */
        SetWaitableTimer(timer, &due, 0, NULL, NULL, 0);
        WaitForSingleObject(timer, INFINITE);
        put('U', seq++, now_us());
    }
    printf("sent %llu\n", seq);
    shutdown(g_c, SD_BOTH);
    closesocket(g_c);
    return 0;
}
