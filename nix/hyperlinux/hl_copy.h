#ifndef HL_WC_COPY_H
#define HL_WC_COPY_H
#include <stddef.h>
/* Same source/destination copy as guest metadata memcpy, but the kernel
 * resolves page faults and reports failure instead of killing the host. */
int hl_wc_copy(void *destination, const void *source, size_t length);
#endif
