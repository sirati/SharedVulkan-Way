#ifndef HL_PT_ACCESS_H
#define HL_PT_ACCESS_H
#include <stdint.h>
/* The callback must reject non-table storage and out-of-range table pages. */
typedef const uint64_t *(*hl_pt_fetch_fn)(const void *, uint64_t);
#define HL_PT_ADDRESS UINT64_C(0x0000fffffffff000)
/* Translate one 4KiB-bounded range. trusted_private is exclusively for kernel
 * initialization/clearing of owned private backing, including retained invalid
 * L3 output addresses. Syscall buffer access must always pass zero. */
static inline int hl_pt_access_translate(const void *context, hl_pt_fetch_fn fetch,
    uint64_t root, uint64_t va, int write, int trusted_private,
    uint64_t *ipa, uint64_t *available) {
    uint64_t table = root & HL_PT_ADDRESS;
    for (unsigned level = 0; level < 4; level++) {
        const uint64_t *entries = fetch(context, table);
        if (!entries) return -1;
        unsigned shift = 39 - level * 9;
        uint64_t descriptor = entries[(va >> shift) & 511];
        unsigned type = descriptor & 3;
        if (level < 3 && type == 3) {
            /* APTable bit 0 forbids EL0, bit 1 removes write access. */
            if (!trusted_private && ((descriptor & (UINT64_C(1) << 61)) ||
                (write && (descriptor & (UINT64_C(1) << 62))))) return -1;
            table = descriptor & HL_PT_ADDRESS;
            continue;
        }
        if (level == 0) return -1; /* No L0 block descriptors. */
        if (level < 3) {
            if (type != 1) return -1;
        } else if (type != 3 && !(trusted_private && type == 0 &&
                                  (descriptor & HL_PT_ADDRESS))) {
            return -1;
        }
        if (!trusted_private && (!(descriptor & (UINT64_C(1) << 10)) ||
            !(descriptor & (UINT64_C(1) << 6)) ||
            (write && (descriptor & (UINT64_C(1) << 7))))) return -1;
        uint64_t mask = (UINT64_C(1) << shift) - 1;
        if (descriptor & HL_PT_ADDRESS & mask) return -1;
        *ipa = (descriptor & HL_PT_ADDRESS) | (va & mask);
        *available = 4096 - (va & 4095);
        return 0;
    }
    return -1;
}
#endif
