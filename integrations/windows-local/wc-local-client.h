/* SPDX-License-Identifier: MIT
 * Process-local identities, never Win32 HANDLEs or Cygwin descriptors.
 * The Cygwin LP64 and MinGW LLP64 ABIs use these same fixed-width fields.
 */
#ifndef WC_LOCAL_CLIENT_H
#define WC_LOCAL_CLIENT_H
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
int32_t wc_local_error(void);
int32_t wc_local_counts(uint32_t *);
int32_t wc_local_nonblocking(uint64_t, int32_t);
int32_t wc_local_wait(uint64_t, int32_t, int32_t);
int32_t wc_local_connect(const uint8_t *, uint32_t, uint64_t *);
int32_t wc_local_read(uint64_t, uint8_t *, uint32_t);
int32_t wc_local_write(uint64_t, const uint8_t *, uint32_t);
int32_t wc_local_disconnect(uint64_t);
int32_t wc_local_section_create(uint64_t, uint64_t *);
int32_t wc_local_section_clone(uint64_t, uint64_t *);
int32_t wc_local_section_close(uint64_t);
/* Export returns required bytes; NULL queries size. Bound is 4096 bytes. */
int32_t wc_local_section_export(uint64_t, uint8_t *, uint32_t);
int32_t wc_local_section_import(const uint8_t *, uint32_t, uint64_t *);
/* Writable mapping is creator-only; imports are always read-only.
 * Address is valid until unmap, even after every section alias closes. */
int32_t wc_local_map(uint64_t, int32_t, uint64_t *, uint64_t *, uint64_t *);
int32_t wc_local_unmap(uint64_t);
#ifdef __cplusplus
}
#endif
#endif
