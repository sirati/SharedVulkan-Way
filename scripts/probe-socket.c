/* probe-socket.c -- step 2: does socket(AF_UNIX, SOCK_STREAM, 0) succeed?
 * Also reports what AF_INET does, as a control, so we can tell "winsock is
 * broken" apart from "AF_UNIX specifically is unsupported".
 */
#include <winsock2.h>
#include <ws2tcpip.h>
#include <afunix.h>
#include <windows.h>
#include <stdio.h>

int main(void)
{
    WSADATA wsa;
    SOCKET s;
    int rc;

    printf("AF_UNIX constant = %d, UNIX_PATH_MAX = %d, sizeof(sockaddr_un) = %d\n",
           (int)AF_UNIX, (int)UNIX_PATH_MAX, (int)sizeof(struct sockaddr_un));

    rc = WSAStartup(MAKEWORD(2, 2), &wsa);
    printf("WSAStartup rc=%d (0 = ok)\n", rc);
    if (rc != 0)
        return 1;
    printf("  wVersion=0x%04x wHighVersion=0x%04x desc=%s\n",
           wsa.wVersion, wsa.wHighVersion, wsa.szDescription);

    /* control: AF_INET should certainly work */
    s = socket(AF_INET, SOCK_STREAM, 0);
    if (s == INVALID_SOCKET)
        printf("socket(AF_INET,SOCK_STREAM,0) FAILED, WSAGetLastError=%d\n",
               WSAGetLastError());
    else {
        printf("socket(AF_INET,SOCK_STREAM,0) OK, handle=%p\n", (void *)s);
        closesocket(s);
    }

    /* the thing we actually care about */
    s = socket(AF_UNIX, SOCK_STREAM, 0);
    if (s == INVALID_SOCKET) {
        printf("socket(AF_UNIX,SOCK_STREAM,0) FAILED, WSAGetLastError=%d\n",
               WSAGetLastError());
    } else {
        printf("socket(AF_UNIX,SOCK_STREAM,0) OK, handle=%p\n", (void *)s);
        closesocket(s);
    }

    /* and the datagram variant, since Windows 10 gained SOCK_DGRAM AF_UNIX
     * later than SOCK_STREAM; useful to know separately */
    s = socket(AF_UNIX, SOCK_DGRAM, 0);
    if (s == INVALID_SOCKET)
        printf("socket(AF_UNIX,SOCK_DGRAM,0) FAILED, WSAGetLastError=%d\n",
               WSAGetLastError());
    else {
        printf("socket(AF_UNIX,SOCK_DGRAM,0) OK, handle=%p\n", (void *)s);
        closesocket(s);
    }

    /* Is the afunix provider even registered with winsock? */
    {
        DWORD len = 0;
        int err = 0;
        WSAEnumProtocolsA(NULL, NULL, &len);
        err = WSAGetLastError();
        if (err == WSAENOBUFS && len > 0) {
            WSAPROTOCOL_INFOA *pi = (WSAPROTOCOL_INFOA *)malloc(len);
            int n = WSAEnumProtocolsA(NULL, pi, &len);
            if (n == SOCKET_ERROR) {
                printf("WSAEnumProtocolsA failed, err=%d\n", WSAGetLastError());
            } else {
                int i;
                printf("WSAEnumProtocolsA: %d providers\n", n);
                for (i = 0; i < n; i++)
                    printf("  [%d] af=%d type=%d proto=%d name=%s\n", i,
                           pi[i].iAddressFamily, pi[i].iSocketType,
                           pi[i].iProtocol, pi[i].szProtocol);
            }
            free(pi);
        } else {
            printf("WSAEnumProtocolsA sizing call: err=%d len=%lu\n", err,
                   (unsigned long)len);
        }
    }

    WSACleanup();
    return 0;
}
