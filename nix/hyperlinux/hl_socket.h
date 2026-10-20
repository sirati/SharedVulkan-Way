#ifndef HL_WC_SOCKET_H
#define HL_WC_SOCKET_H
#include "fd_object.h"
#include <stdint.h>
#include <sys/socket.h>
typedef struct { int32_t pid; uint32_t uid, gid; } hl_wc_ucred;
int hl_wc_socket_alloc(int);
int hl_wc_socket_alloc_retained(int, int, int, hl_descriptor_t **);
int hl_wc_socket_pair_metadata(int [2]);
int hl_wc_socket_native_close(int);
int hl_wc_socket_state_watch(hl_open_file_t *);
ssize_t hl_wc_socket_record_attempt(hl_open_file_t *, int, struct msghdr *, int, int);
int hl_wc_socket_read_closed(hl_open_file_t *, int);
int hl_wc_socket_shutdown_pair(hl_open_file_t *, int, int);
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
typedef struct { int fds[64]; hl_descriptor_t *descriptors[64]; unsigned count; } hl_wc_socket_transfers;
void hl_wc_socket_received_end(hl_wc_socket_transfers *);
void hl_wc_socket_transfer_end(hl_wc_socket_transfers *);
int hl_wc_socket_transfer(int, const int *, unsigned, int *, unsigned *, hl_wc_socket_transfers *, int);
int hl_wc_socket_receive(int, int *, unsigned, int *, unsigned *, int, int, hl_wc_socket_transfers *);
void hl_wc_socket_received_close(hl_wc_socket_transfers *, int);
void hl_wc_socket_received_commit(hl_wc_socket_transfers *);
typedef struct { struct msghdr *message; } hl_wc_socket_native_guard;
void hl_wc_socket_native_end(hl_wc_socket_native_guard *);
int hl_wc_socket_seqpacket(int);
int hl_wc_socket_seqpacket_of(hl_open_file_t *);
int hl_wc_socket_private_file(void);
#endif
