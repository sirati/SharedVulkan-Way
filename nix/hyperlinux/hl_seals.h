#ifndef HL_WC_SEALS_H
#define HL_WC_SEALS_H
#include <stdint.h>
#include <sys/types.h>
#include <sys/uio.h>
typedef struct hl_open_file hl_open_file_t;
#define HL_WC_XATTR_PREFIX "com.waycompose.hl."
typedef struct { int lockfd, fd, dirty, ownfd; void *metadata; } hl_wc_seal_guard;
int hl_wc_seal_init(int fd, int allow);
/* Native errno API. Guard is released on every exit with the cleanup macro. */
int hl_wc_seal_guest(int guestfd, int operation, uint64_t value, hl_wc_seal_guard *);
int hl_wc_seal_begin(int fd, int operation, uint64_t value, hl_wc_seal_guard *);
void hl_wc_seal_end(hl_wc_seal_guard *);
#define HL_WC_SEAL_GUARD(name, fd, operation, value) \
    hl_wc_seal_guard name __attribute__((cleanup(hl_wc_seal_end))) = { .lockfd = -1 }; \
    if (hl_wc_seal_begin(fd, operation, value, &name) < 0) return linux_errno()
#define HL_WC_SEAL_PIN -1
#define HL_WC_SEAL_WRITE 1
#define HL_WC_SEAL_RESIZE 2
#define HL_WC_SEAL_MAP 3
int hl_wc_seal_writable(const hl_wc_seal_guard *);
int hl_wc_seal_track(hl_wc_seal_guard *, uint64_t, uint64_t, int, uint64_t *);
void hl_wc_seal_untrack(int, uint64_t);
int64_t hl_wc_seal_fcntl(int, int, uint64_t);
int hl_wc_token_snapshot(int, uint8_t *, uint64_t);
int hl_wc_is_memfd(const hl_open_file_t *);
int hl_wc_memfd_adopt(hl_open_file_t *, int);
int hl_wc_memfd_restore(int);
ssize_t hl_wc_write(int, const void *, size_t);
ssize_t hl_wc_pwrite(int, const void *, size_t, off_t);
ssize_t hl_wc_writev(int, const struct iovec *, int);
ssize_t hl_wc_pwritev(int, const struct iovec *, int, off_t);
int hl_wc_ftruncate(int, off_t);
#endif
