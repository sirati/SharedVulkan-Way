#ifndef HL_SECCOMP_H
#define HL_SECCOMP_H
#include "hl_seccomp_bpf.h"
#include "guest.h"
struct hl_seccomp_filter;
/* All chain references and per-thread security state use thread_get_lock().
 * No guest memory or blocking host I/O is accessed with that lock held. */
void hl_seccomp_retain(struct hl_seccomp_filter *p);
void hl_seccomp_release(struct hl_seccomp_filter *p);
int64_t hl_seccomp_prctl(guest_t *, uint64_t, uint64_t, uint64_t, uint64_t, uint64_t);
int64_t hl_seccomp_syscall(guest_t *, uint64_t, uint64_t, uint64_t);
uint32_t hl_seccomp_evaluate(const hl_seccomp_data *);
int hl_seccomp_fork_send(int fd);
int hl_seccomp_fork_receive(int fd);
#endif
