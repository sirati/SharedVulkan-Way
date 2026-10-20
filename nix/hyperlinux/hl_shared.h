#ifndef HL_WC_SHARED_H
#define HL_WC_SHARED_H
#include "guest.h"
#include <stdint.h>
/* All mutating calls require mmap_lock, never fd_lock. */
int64_t hl_wc_mmap(guest_t *, uint64_t, uint64_t, int, int, int, int64_t);
int hl_wc_unmap(guest_t *, uint64_t, uint64_t);
int hl_wc_protect(guest_t *, uint64_t, uint64_t, int);
void hl_wc_reset(guest_t *);
void hl_wc_vm_destroyed(void);
uint64_t hl_wc_find_va(guest_t *, uint64_t);
int hl_wc_ipa_busy(uint64_t, uint64_t);
int hl_shm_ipa_busy(uint64_t, uint64_t);
void *hl_wc_resolve(uint64_t, uint64_t *);
/* Numeric records only. Each record's backing fd travels via SCM_RIGHTS. */
typedef struct { uint64_t va, len, offset; int32_t prot, writable; } hl_wc_fork_record;
int hl_wc_fork_count(void);
int hl_wc_fork_export(int, hl_wc_fork_record *, int *);
int hl_wc_fork_import(guest_t *, const hl_wc_fork_record *, int);
int64_t hl_wc_shm_open(const char *, int, int);
int64_t hl_wc_shm_unlink(const char *, int);
int64_t hl_wc_msync(uint64_t, uint64_t, int);
void hl_wc_prime_attach(int, const void *);
#endif
