#!/usr/bin/env python3
"""Compile/run actual GDB packet and restore functions with fake HV setters.
Run inside the build cgroup: SCRIPT patched/gdb_stub.c header-directory CC.
This checks handler refusal/atomicity, not native debugger transport.
"""
import pathlib
import re
import subprocess
import sys
import tempfile
source = pathlib.Path(sys.argv[1]).read_text()

def function(name):
    match = re.search(r"^static [^\n]*\b" + name + r"\(", source, re.M)
    if not match:
        raise SystemExit("Missing actual function: " + name)
    begin = source.index("{", match.start())
    depth = 1
    end = begin + 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[match.start():end] + "\n"

macros = "\n".join(re.findall(r"^#define REG_[^\n]+", source, re.M))
stubs = r'''
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include "hl_user_regs.h"
#define HV_CHECK(x) ((void)(x))
#define HV_SYS_REG_SP_EL0 1
#define HV_SYS_REG_ELR_EL1 2
#define HV_SYS_REG_SPSR_EL1 3
#define HV_REG_FPSR 4
#define HV_REG_FPCR 5
#define HV_SIMD_FP_REG_Q0 6
typedef int hv_vcpu_t;
typedef int hv_simd_fp_reg_t;
typedef struct { unsigned char bytes[16]; } hv_simd_fp_uchar16_t;
typedef struct { int vcpu; unsigned char gdb_reg_snapshot[788]; int gdb_regs_dirty; } thread_entry_t;
static thread_entry_t target;
static struct { int current_g_tid; } gdb = {1};
static int reply, mutations;
static const char hex_chars[]="0123456789abcdef";
static thread_entry_t *find_thread_for_tid(int tid) { return tid==1 ? &target : NULL; }
static void rsp_reply_error(int error) { reply=error; }
static void rsp_reply_ok(void) { reply=0; }
static void vcpu_set_gpr(int v,unsigned i,uint64_t n) { (void)v;(void)i;(void)n;mutations++; }
static void vcpu_set_sysreg(int v,int i,uint64_t n) { (void)v;(void)i;(void)n;mutations++; }
static int hv_vcpu_set_simd_fp_reg(int v,int i,hv_simd_fp_uchar16_t n) { (void)v;(void)i;(void)n;mutations++;return 0; }
static int hv_vcpu_set_reg(int v,int i,uint64_t n) { (void)v;(void)i;(void)n;mutations++;return 0; }
'''
tests = r'''
int main(void) {
    uint64_t pc=0x400000;uint32_t flags=0x60000000;
    memcpy(target.gdb_reg_snapshot+REG_OFF_PC,&pc,8);
    memcpy(target.gdb_reg_snapshot+REG_OFF_CPSR,&flags,4);
    unsigned char saved[788],proposed[788];memcpy(saved,target.gdb_reg_snapshot,788);
    char packet[1577];memcpy(proposed,saved,788);
    proposed[0]=0xa5;flags=5;memcpy(proposed+REG_OFF_CPSR,&flags,4);
    hex_encode(packet,proposed,788);handle_write_regs(packet);
    assert(reply==22 && !target.gdb_regs_dirty && !memcmp(saved,target.gdb_reg_snapshot,788));
    /* A late decoder error cannot commit the already decoded first GPR. */
    packet[1500]='z';handle_write_regs(packet);
    assert(reply==1 && !memcmp(saved,target.gdb_reg_snapshot,788));
    handle_write_reg("21=05000000");assert(reply==22 && !memcmp(saved,target.gdb_reg_snapshot,788));
    handle_write_reg("20=0100400000000000");assert(reply==22 && !memcmp(saved,target.gdb_reg_snapshot,788));
    /* Defense at the actual HV boundary runs before all GPR writes. */
    memcpy(target.gdb_reg_snapshot,proposed,788);target.gdb_regs_dirty=1;
    restore_vcpu_regs(&target);assert(!mutations && !target.gdb_regs_dirty);
    memcpy(target.gdb_reg_snapshot,saved,788);
    flags=0x90000000;memcpy(proposed,saved,788);memcpy(proposed+REG_OFF_CPSR,&flags,4);
    hex_encode(packet,proposed,788);handle_write_regs(packet);
    assert(!reply && target.gdb_regs_dirty && !memcmp(proposed,target.gdb_reg_snapshot,788));
    restore_vcpu_regs(&target);assert(mutations>31 && !target.gdb_regs_dirty);
    puts("actual GDB G/P refusal, decode atomicity and pre-HV restore guard passed");
}
'''
functions = "\n".join(function(name) for name in (
    "hex_val", "hex_encode", "hex_decode", "parse_hex", "user_snapshot_valid",
    "restore_vcpu_regs", "reg_offset_and_size", "handle_write_regs", "handle_write_reg"))
with tempfile.TemporaryDirectory(prefix="hl-gdb-state-") as directory:
    path = pathlib.Path(directory)
    (path / "test.c").write_text(stubs + macros + "\n" + functions + tests)
    subprocess.run([sys.argv[3], "-std=c11", "-O2", "-Wall", "-Wextra", "-Werror",
                    "-I", sys.argv[2], str(path / "test.c"), "-o", str(path / "test")], check=True)
    subprocess.run([str(path / "test")], check=True)
