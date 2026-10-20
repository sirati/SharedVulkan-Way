#ifndef HL_FD_LIFECYCLE_H
#define HL_FD_LIFECYCLE_H
#include <pthread.h>
/* Lock order: lifecycle, then fd_lock, then subsystem locks. Never park a
 * guest I/O wait under this lock. Special factories and retirement hold it
 * through publication/cleanup; every allocator participates, so a freed guest
 * number cannot be reused before its keyed subsystem state is gone. */
extern pthread_mutex_t hl_fd_lifecycle_mutex;
typedef struct { int locked; } hl_fd_lifecycle_guard;
static inline hl_fd_lifecycle_guard hl_fd_lifecycle_enter(void) {
    pthread_mutex_lock(&hl_fd_lifecycle_mutex);
    return (hl_fd_lifecycle_guard){1};
}
static inline void hl_fd_lifecycle_end(hl_fd_lifecycle_guard *guard) {
    if (guard->locked) {
        guard->locked = 0;
        pthread_mutex_unlock(&hl_fd_lifecycle_mutex);
    }
}
#define HL_FD_LIFECYCLE_SCOPE \
    hl_fd_lifecycle_guard fd_lifecycle __attribute__((cleanup(hl_fd_lifecycle_end))) = hl_fd_lifecycle_enter()
#endif
