#include "hl_pt_access.h"
#include <assert.h>
#include <stdio.h>
#include <string.h>
static uint64_t tables[4][512];
static const uint64_t *fetch(const void *unused, uint64_t ipa) {
    (void)unused;
    if (ipa < 0x1000 || ipa >= 0x5000 || (ipa & 4095)) return NULL;
    return tables[ipa / 4096 - 1];
}
static int translate(uint64_t va, int write, int trusted, uint64_t *ipa, uint64_t *avail) {
    return hl_pt_access_translate(NULL, fetch, 0x1000, va, write, trusted, ipa, avail);
}
int main(void) {
    uint64_t ipa, avail;
    tables[0][0] = 0x2003; tables[1][0] = 0x3003;
    tables[2][2] = UINT64_C(0x800000) | 1 | (1 << 10) | (1 << 6);
    assert(!translate(0x400fff, 1, 0, &ipa, &avail) && ipa == 0x800fff && avail == 1);
    tables[2][2] |= 1 << 7;
    assert(!translate(0x401000, 0, 0, &ipa, &avail) && ipa == 0x801000 && avail == 4096);
    assert(translate(0x401000, 1, 0, &ipa, &avail) < 0);
    assert(!translate(0x401000, 1, 1, &ipa, &avail)); /* Kernel init only. */
    tables[2][2] &= ~(UINT64_C(1) << 6);
    assert(translate(0x401000, 0, 0, &ipa, &avail) < 0); /* EL1-only. */
    tables[2][2] = 0x4003;
    tables[3][1] = 0x900000 | 3 | (1 << 10) | (1 << 6);
    tables[3][2] = 0xa00000 | 3 | (1 << 10) | (3 << 6);
    assert(!translate(0x401fff, 1, 0, &ipa, &avail) && ipa == 0x900fff && avail == 1);
    assert(translate(0x402000, 1, 0, &ipa, &avail) < 0);
    assert(!translate(0x402000, 0, 0, &ipa, &avail) && ipa == 0xa00000);
    tables[3][1] &= HL_PT_ADDRESS;
    assert(translate(0x401000, 0, 0, &ipa, &avail) < 0);
    assert(!translate(0x401000, 1, 1, &ipa, &avail) && ipa == 0x900000);
    tables[1][0] |= UINT64_C(1) << 61;
    assert(translate(0x402000, 0, 0, &ipa, &avail) < 0);
    tables[1][0] &= ~(UINT64_C(1) << 61);
    tables[1][0] |= UINT64_C(1) << 62;
    tables[3][1] = 0x900000 | 3 | (1 << 10) | (1 << 6);
    assert(translate(0x401000, 1, 0, &ipa, &avail) < 0);
    assert(!translate(0x401000, 0, 0, &ipa, &avail));
    tables[1][0] = 0x3003;
    tables[3][1] &= ~(UINT64_C(1) << 10);
    assert(translate(0x401000, 0, 0, &ipa, &avail) < 0); /* No access flag. */
    tables[2][2] = 0xf00003;
    assert(translate(0x401000, 0, 0, &ipa, &avail) < 0); /* Bad table pointer. */
    /* TTBR1's canonical indices, with the same leaf permission checks. */
    memset(tables, 0, sizeof(tables));
    tables[0][511] = 0x2003; tables[1][511] = 0x3003;
    tables[2][384] = 0x4003; tables[3][0] = 0xb00000 | 3 | (1 << 10) | (3 << 6);
    assert(!translate(UINT64_C(0xfffffffff0000000), 0, 0, &ipa, &avail) && ipa == 0xb00000);
    assert(translate(UINT64_C(0xfffffffff0000000), 1, 0, &ipa, &avail) < 0);
    /* Nonidentity VA translation into an absolute overflow IPA, with a
     * nonzero primary IPA base. Subtracting that base would resolve wrongly. */
    memset(tables, 0, sizeof(tables));
    tables[0][256] = 0x2003; tables[1][0] = 0x3003;
    tables[2][2] = 0x4003;
    tables[3][1] = UINT64_C(0x100a00000) | 3 | (1 << 10) | (1 << 6);
    assert(!translate(UINT64_C(0x800000401123), 1, 0, &ipa, &avail));
    uint64_t off;
    assert(ipa == UINT64_C(0x100a00123));
    assert(hl_pt_backing_offset(ipa, UINT64_C(0x100000000), 0x800000, &off, &avail) < 0);
    assert(!hl_pt_backing_offset(ipa, UINT64_C(0x100a00000), 0x4000, &off, &avail) && off == 0x123);
    assert(hl_pt_backing_offset(ipa - UINT64_C(0x100000000), UINT64_C(0x100a00000), 0x4000, &off, &avail) < 0);
    avail = 4096;
    assert(!hl_pt_backing_offset(UINT64_C(0x100a03fff), UINT64_C(0x100a00000), 0x4000, &off, &avail) && avail == 1);
    /* A TTBR1 leaf's kbuf GPA is retained inside the primary buffer. */
    avail = 4096;
    assert(!hl_pt_backing_offset(0xb00123, 0, 0x1000000, &off, &avail) && off == 0xb00123);
    puts("pt-access-check: leaf/table permissions, invalid/AF denial, page bounds, aliases and TTBR1 passed");
}
