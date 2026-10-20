#ifndef HL_WC_SEQPACKET_H
#define HL_WC_SEQPACKET_H
#include <sys/socket.h>
#include "hl_socket.h"
/* Supported pair ABI. All MSG_PEEK and named endpoints fail explicitly.
 * Every message is one native byte plus a private bounded record file and
 * original SCM_RIGHTS. Native stream buffering retains records and rights. */
#define HL_WC_SEQPACKET_MAX 16384u
#define HL_WC_SEQPACKET_BUFFER 256
int hl_wc_seqpacket_pair(int [2]);
ssize_t hl_wc_seqpacket_send(int, const struct msghdr *, int);
ssize_t hl_wc_seqpacket_recv(int, struct msghdr *, int, int);
ssize_t hl_wc_seqpacket_io(int, const struct iovec *, int, int);
#endif
