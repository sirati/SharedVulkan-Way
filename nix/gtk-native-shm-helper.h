/* SPDX-License-Identifier: MIT */
#ifndef WC_GTK_NATIVE_SHM_HELPER_H
#define WC_GTK_NATIVE_SHM_HELPER_H
#include "gdkwayland.h"
#include "gdkdisplay-wayland.h"
/* 0: no extension on this display, 1: original allocation, -1: refused.
 * Refusal is never an invitation to substitute copied storage. */
int wc_gtk_native_shm_create(GdkWaylandDisplay *display, int size,
    struct wl_shm_pool **pool, void **pixels, void **owner, int *buffer_offset);
void wc_gtk_native_shm_close(void *owner);
void wc_gtk_native_shm_display_finish(GdkWaylandDisplay *display);
#endif
