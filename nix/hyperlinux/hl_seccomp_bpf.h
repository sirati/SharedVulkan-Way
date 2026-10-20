#ifndef HL_SECCOMP_BPF_H
#define HL_SECCOMP_BPF_H
#include <stdint.h>
#include <stddef.h>
#define HL_SECCOMP_MAX_INSNS 4096
#define HL_AUDIT_ARCH_AARCH64 0xc00000b7U
/* Linux UAPI layouts, independent of the host ABI. */
typedef struct { uint16_t code; uint8_t jt, jf; uint32_t k; } hl_sock_filter;
typedef struct {
    int32_t nr; uint32_t arch; uint64_t instruction_pointer; uint64_t args[6];
} hl_seccomp_data;
int hl_bpf_validate(const hl_sock_filter *p, size_t n);
/* Requires a successfully validated immutable program and a nonnull data pointer. */
uint32_t hl_bpf_run(const hl_sock_filter *p, size_t n, const hl_seccomp_data *d);
#endif
