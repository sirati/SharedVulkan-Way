/* Restricted classic BPF as accepted for Linux seccomp filters. */
#include "hl_seccomp_bpf.h"
#include <stdlib.h>
#include <string.h>
_Static_assert(sizeof(hl_seccomp_data) == 64, "seccomp_data ABI");
_Static_assert(sizeof(hl_sock_filter) == 8, "sock_filter ABI");

static void merge(uint32_t *m, size_t i, uint32_t state) {
    /* Bit 16 distinguishes an unseen instruction from a reachable path. */
    if (m[i] & 0x10000U) m[i] &= state | 0x10000U;
    else m[i] = state | 0x10000U;
}
int hl_bpf_validate(const hl_sock_filter *p, size_t n) {
    if (!p || !n || n > HL_SECCOMP_MAX_INSNS) return -1;
    if (p[n-1].code != 0x06 && p[n-1].code != 0x16) return -1;
    uint32_t *mask = calloc(n, sizeof(*mask));
    if (!mask) return -2;
    mask[0] = 0x10000U;
    int valid = 1;
    for (size_t i = 0; i < n && valid; i++) {
        const hl_sock_filter *s = &p[i];
        uint32_t m = mask[i] & 0xffffU;
        int reachable = !!(mask[i] & 0x10000U);
        size_t a = i + 1, b = a;
        switch (s->code) {
        case 0x20: /* LD W ABS: aligned 32-bit seccomp_data load only */
            if ((s->k & 3) || s->k > sizeof(hl_seccomp_data)-4) valid = 0;
            break;
        case 0x00: case 0x01: /* LD/LDX IMM */
        case 0x80: case 0x81: /* LD/LDX W LEN */
        case 0x04: case 0x0c: case 0x14: case 0x1c: /* ADD/SUB */
        case 0x24: case 0x2c: /* MUL */
        case 0x44: case 0x4c: case 0x54: case 0x5c: /* OR/AND */
        case 0xa4: case 0xac: /* XOR */
        case 0x84: /* NEG */
        case 0x3c: /* DIV/MOD X, zero checked at runtime */
        case 0x6c: case 0x7c: /* LSH/RSH X */
        case 0x07: case 0x87: /* TAX/TXA */
            break;
        case 0x34: if (!s->k) valid = 0; break;
        case 0x64: case 0x74: if (s->k >= 32) valid = 0; break;
        case 0x60: case 0x61: /* LD/LDX MEM */
            if (s->k >= 16 || (reachable && !(m & (1U << s->k)))) valid = 0;
            break;
        case 0x02: case 0x03: /* ST/STX */
            if (s->k >= 16) valid = 0;
            else m |= 1U << s->k;
            break;
        case 0x05: /* JA */
            if (s->k >= n - i - 1) valid = 0;
            else a = b = i + 1 + s->k;
            break;
        case 0x15: case 0x1d: case 0x25: case 0x2d:
        case 0x35: case 0x3d: case 0x45: case 0x4d:
            a += s->jt; b += s->jf;
            if (a >= n || b >= n) valid = 0;
            break;
        case 0x06: case 0x16: continue; /* RET K/A */
        default: valid = 0; break;
        }
        if (!valid || a >= n || b >= n) { valid = 0; break; }
        if (reachable) { merge(mask, a, m); merge(mask, b, m); }
    }
    /* Linux checks scratch initialization in linear instruction order, even
     * following RET. Match that additional acceptance constraint for dead code. */
    if (valid) {
        for (size_t i=0;i<n;++i) mask[i]=0xffffU;
        uint32_t m=0;
        for (size_t i=0;i<n;++i) {
            const hl_sock_filter *s=&p[i];
            m &= mask[i];
            if (s->code==0x02 || s->code==0x03) m |= 1U<<s->k;
            else if (s->code==0x60 || s->code==0x61) {
                if (!(m & (1U<<s->k))) { valid=0; break; }
            } else if (s->code==0x05) {
                mask[i+1+s->k] &= m; m=0xffffU;
            } else if ((s->code & 7)==5) {
                mask[i+1+s->jt] &= m; mask[i+1+s->jf] &= m; m=0xffffU;
            }
        }
    }
    free(mask);
    return valid ? 0 : -1;
}
uint32_t hl_bpf_run(const hl_sock_filter *p, size_t n, const hl_seccomp_data *d) {
    uint32_t a = 0, x = 0, mem[16] = {0};
    for (size_t pc = 0; pc < n; pc++) {
        const hl_sock_filter *s = &p[pc];
        uint32_t v = (s->code & 8) ? x : s->k;
        switch (s->code) {
        case 0x20: memcpy(&a, (const unsigned char *)d+s->k, 4); break;
        case 0x00: a = s->k; break;
        case 0x01: x = s->k; break;
        case 0x80: a = sizeof(*d); break;
        case 0x81: x = sizeof(*d); break;
        case 0x60: a = mem[s->k]; break;
        case 0x61: x = mem[s->k]; break;
        case 0x02: mem[s->k] = a; break;
        case 0x03: mem[s->k] = x; break;
        case 0x04: case 0x0c: a += v; break;
        case 0x14: case 0x1c: a -= v; break;
        case 0x24: case 0x2c: a *= v; break;
        case 0x34: case 0x3c: if (!v) return 0; a /= v; break;
        case 0x94: case 0x9c: if (!v) return 0; a %= v; break;
        case 0x44: case 0x4c: a |= v; break;
        case 0x54: case 0x5c: a &= v; break;
        case 0xa4: case 0xac: a ^= v; break;
        case 0x64: case 0x6c: a = a << (v & 31); break;
        case 0x74: case 0x7c: a = a >> (v & 31); break;
        case 0x84: a = 0U-a; break;
        case 0x07: x = a; break;
        case 0x87: a = x; break;
        case 0x05: pc += s->k; break;
        case 0x15: case 0x1d: pc += a == v ? s->jt : s->jf; break;
        case 0x25: case 0x2d: pc += a > v ? s->jt : s->jf; break;
        case 0x35: case 0x3d: pc += a >= v ? s->jt : s->jf; break;
        case 0x45: case 0x4d: pc += (a & v) ? s->jt : s->jf; break;
        case 0x06: return s->k;
        case 0x16: return a;
        default: return 0; /* Installed programs are validated before this call. */
        }
    }
    return 0;
}
