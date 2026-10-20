/* Guest AArch64 user-state policy. Do not admit privileged modes or masks via
 * ptrace, signal frames, or another externally supplied register snapshot. */
#ifndef HL_USER_REGS_H
#define HL_USER_REGS_H
#include <stdint.h>
/* NZCV, TCO, DIT, SSBS and BTYPE are user-state bits. Mode/AArch32, DAIF,
 * IL, SS, PAN, UAO, ALLINT and reserved bits are never restored from guests. */
#define HL_USER_PSTATE_MASK UINT64_C(0xf3001c00)
static inline uint64_t hl_user_pstate(uint64_t pstate) {
    return pstate & HL_USER_PSTATE_MASK;
}
static inline int hl_user_regs_valid(uint64_t pc, uint64_t pstate) {
    return !(pc & 3) && pc <= UINT64_C(0x0000ffffffffffff) &&
           pstate == hl_user_pstate(pstate);
}
#endif
