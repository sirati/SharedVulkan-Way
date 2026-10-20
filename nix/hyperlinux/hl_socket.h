#ifndef HL_WC_SOCKET_H
#define HL_WC_SOCKET_H
#include "fd_object.h"
#include <stdint.h>
typedef struct { int32_t pid; uint32_t uid, gid; } hl_wc_ucred;
int hl_wc_socket_alloc(int);
void hl_wc_socket_drop(hl_open_file_t *);
int hl_wc_socket_set(int, int);
int hl_wc_socket_get(int, int *);
int hl_wc_socket_enabled(hl_open_file_t *);
int hl_wc_socket_peer(int, hl_wc_ucred *);
int hl_wc_socket_same_runtime(int);
/* Export returns a retained native state fd, -1/no state, -2/error. */
int hl_wc_socket_export(int);
int hl_wc_socket_import(int, int);
int hl_wc_socket_is_state(int);
typedef struct { int fds[64]; unsigned count; } hl_wc_socket_transfers;
void hl_wc_socket_received_end(hl_wc_socket_transfers *);
void hl_wc_socket_transfer_end(hl_wc_socket_transfers *);
int hl_wc_socket_transfer(int, const int *, unsigned, int *, unsigned *, hl_wc_socket_transfers *);
int hl_wc_socket_receive(int, int *, unsigned, int *, unsigned *);
#endif
