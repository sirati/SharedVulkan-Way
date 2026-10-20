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
/* Requires mmap_lock across query and permission change. */
int hl_wc_contains(uint64_t);
int hl_wc_stage2_fault(uint64_t va, uint64_t ipa);
int hl_wc_ipa_busy(uint64_t, uint64_t);
int hl_shm_ipa_busy(uint64_t, uint64_t);
/* Raw external pointers are valid only until their enclosing scope ends.
 * Scopes are thread-local and nest; blocking native I/O retains its scope. */
typedef struct hl_wc_access_scope {
    struct hl_wc_access_scope *previous;
    void *pins;
} hl_wc_access_scope;
void hl_wc_access_begin(hl_wc_access_scope *);
void hl_wc_access_end(hl_wc_access_scope *);
#define HL_WC_ACCESS_SCOPE hl_wc_access_scope wc_access_scope \
    __attribute__((cleanup(hl_wc_access_end))); hl_wc_access_begin(&wc_access_scope)
void *hl_wc_resolve(uint64_t, uint64_t *, int write, int *found);
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
