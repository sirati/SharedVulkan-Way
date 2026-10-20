#ifndef HL_PT_DOMAIN_H
#define HL_PT_DOMAIN_H
#include <pthread.h>
#include <stdlib.h>
/* Readers hold this only during root selection and the pure descriptor walk.
 * Writers never acquire native-I/O, fd-table or external-mapping locks while
 * holding it. In particular, external reset must precede the PT reset scope.
 * Existing nested split/update/allocation helpers share one writer scope. */
static pthread_rwlock_t hl_pt_domain_lock = PTHREAD_RWLOCK_INITIALIZER;
static _Thread_local unsigned hl_pt_writer_depth;
typedef struct { int writer, acquired; } hl_pt_domain_scope;
static inline hl_pt_domain_scope hl_pt_domain_begin(int writer) {
    hl_pt_domain_scope scope = { .writer = writer };
    if (writer) {
        if (hl_pt_writer_depth++ == 0) {
            if (pthread_rwlock_wrlock(&hl_pt_domain_lock)) abort();
            scope.acquired = 1;
        }
    } else if (!hl_pt_writer_depth) {
        if (pthread_rwlock_rdlock(&hl_pt_domain_lock)) abort();
        scope.acquired = 1;
    }
    return scope;
}
static inline void hl_pt_domain_end(hl_pt_domain_scope *scope) {
    if (scope->writer && !hl_pt_writer_depth--) abort();
    if (scope->acquired && pthread_rwlock_unlock(&hl_pt_domain_lock)) abort();
}
#define HL_PT_READ_SCOPE hl_pt_domain_scope pt_domain_scope \
    __attribute__((cleanup(hl_pt_domain_end))) = hl_pt_domain_begin(0)
#define HL_PT_WRITE_SCOPE hl_pt_domain_scope pt_domain_scope \
    __attribute__((cleanup(hl_pt_domain_end))) = hl_pt_domain_begin(1)
#endif
