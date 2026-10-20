/* SPDX-License-Identifier: MIT */
#ifndef WC_NATIVE_SHM_H
#define WC_NATIVE_SHM_H
#include <stdint.h>
struct wc_native_shm_mapping;
/* No fallback: NULL means the allocation was refused. A broker region is
 * opened with the caller's authenticated identity. Fixture kind 2 requires
 * WC_NATIVE_SHM_FIXTURE=1. The mapping owns its fd and its broker lease. */
struct wc_native_shm_mapping *wc_native_shm_open(uint32_t kind, uint64_t region,
    const char *fixture_path, uint64_t offset, uint64_t size);
void *wc_native_shm_data(const struct wc_native_shm_mapping *mapping);
/* Borrowed fd. The Wayland marshaller copies it before this mapping closes. */
int wc_native_shm_fd(const struct wc_native_shm_mapping *mapping);
uint64_t wc_native_shm_offset(const struct wc_native_shm_mapping *mapping);
/* Exactly once, after every Cairo use ends. */
void wc_native_shm_close(struct wc_native_shm_mapping *mapping);
#endif
