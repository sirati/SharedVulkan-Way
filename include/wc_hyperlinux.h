#ifndef WC_HYPERLINUX_H
#define WC_HYPERLINUX_H
#include <stdint.h>
/* Native ABI. Never install this library into the Linux ELF closure.
 * All errors are negative HOST errno, translated by the hl C adapter.
 * No call unwinds through C, launches processes, or copies guest payloads. */
typedef struct wc_hl_backing wc_hl_backing;
typedef struct {
    void *base;
    uint64_t mapped_len;
    uint64_t data_offset;
    uint64_t len;
    uint64_t file_offset;
} wc_hl_mapping;
int32_t wc_hl_backing_map(int32_t fd, uint64_t offset, uint64_t len,
    int32_t writable, wc_hl_backing **out, wc_hl_mapping *info);
/* Each mapping retains its own fd. Release only after removing HVF mappings.
 * Closing a guest descriptor does not release a live backing handle. */
void wc_hl_backing_release(wc_hl_backing *handle);
/* CLOEXEC duplicate for SCM_RIGHTS fork transfer, owned by caller. */
int32_t wc_hl_backing_export_fd(const wc_hl_backing *handle, int32_t *out);
typedef struct wc_hl_drm wc_hl_drm;
typedef struct {
    void *context;
    int32_t (*read)(void *, uint64_t, uint8_t *, uint64_t);
    int32_t (*write)(void *, uint64_t, const uint8_t *, uint64_t);
    int32_t (*token)(void *, int32_t, uint8_t *, uint64_t);
    int32_t (*relay)(void *, int32_t, const uint8_t *, uint64_t);
} wc_hl_drm_callbacks;
int32_t wc_hl_drm_open(uint32_t uid, uint32_t pid, wc_hl_drm **out);
int32_t wc_hl_drm_ioctl(wc_hl_drm *, uint32_t uid, uint32_t pid,
    uint32_t cmd, uint64_t arg, wc_hl_drm_callbacks);
void wc_hl_drm_release(wc_hl_drm *);
int32_t wc_hl_token_validate(const uint8_t *, uint64_t);
int32_t wc_hl_prime_register(int32_t, const uint8_t *, uint64_t, void **);
void *wc_hl_prime_clone(const void *);
void wc_hl_prime_release(void *);
#endif
