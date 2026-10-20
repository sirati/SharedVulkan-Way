/* Linux reference checks for the portable runtime verifier/interpreter. */
#define _GNU_SOURCE
#include "hl_seccomp_bpf.h"
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#define I(c,k) {c,0,0,k}
static int check(const hl_sock_filter *p, unsigned n, int expected_valid) {
    int valid=hl_bpf_validate(p,n)==0;
    if (valid != expected_valid) { fprintf(stderr,"verifier mismatch\n"); return 1; }
    pid_t pid=fork();
    if (!pid) {
        struct sock_fprog f={.len=n,.filter=(struct sock_filter *)p};
        if (prctl(PR_SET_NO_NEW_PRIVS,1,0,0,0)) _exit(90);
        errno=0;
        int r=prctl(PR_SET_SECCOMP,SECCOMP_MODE_FILTER,&f);
        if ((r==0)!=valid) _exit(91);
        _exit(0);
    }
    int status; if (waitpid(pid,&status,0)!=pid || status) {
        fprintf(stderr,"kernel verifier disagreed, status=%x\n",status); return 1;
    }
    return 0;
}
static int arithmetic(uint16_t op, uint32_t x, uint32_t a) {
    hl_sock_filter p[]={I(0x20,0),{0x15,0,6,SYS_getpid},I(0x00,a),I(0x01,x),
        I(op,3),I(0x54,4095),I(0x44,SECCOMP_RET_ERRNO),I(0x16,0),I(0x06,SECCOMP_RET_ALLOW)};
    hl_seccomp_data d={.nr=SYS_getpid};
    if (hl_bpf_validate(p,sizeof(p)/sizeof(*p))) return 1;
    uint32_t expected=hl_bpf_run(p,sizeof(p)/sizeof(*p),&d)&0xffff;
    pid_t pid=fork();
    if (!pid) {
        struct sock_fprog f={sizeof(p)/sizeof(*p),(struct sock_filter *)p};
        if (prctl(PR_SET_NO_NEW_PRIVS,1,0,0,0) || prctl(PR_SET_SECCOMP,2,&f)) _exit(90);
        errno=0; long r=syscall(SYS_getpid);
        _exit(expected ? !(r==-1 && errno==(int)expected) : r!=0);
    }
    int status; if (waitpid(pid,&status,0)!=pid || status) {
        fprintf(stderr,"ALU mismatch op=%x x=%u a=%u expected=%u status=%x\n",op,x,a,expected,status); return 1;
    }
    return 0;
}
int main(void) {
    int failed=0;
    hl_sock_filter good[]={I(0x20,60),I(0x06,SECCOMP_RET_ALLOW)};
    failed+=check(good,2,1);
    hl_sock_filter unaligned[]={I(0x20,1),I(0x06,SECCOMP_RET_ALLOW)};
    failed+=check(unaligned,2,0);
    hl_sock_filter beyond[]={I(0x20,64),I(0x06,SECCOMP_RET_ALLOW)};
    failed+=check(beyond,2,0);
    hl_sock_filter scratch[]={I(0x60,0),I(0x06,SECCOMP_RET_ALLOW)};
    failed+=check(scratch,2,0);
    hl_sock_filter branched[]={I(0x20,0),{0x15,0,1,0},I(0x02,0),I(0x60,0),I(0x06,SECCOMP_RET_ALLOW)};
    failed+=check(branched,5,0);
    hl_sock_filter initialized[]={I(0x02,0),I(0x60,0),I(0x06,SECCOMP_RET_ALLOW)};
    failed+=check(initialized,3,1);
    hl_sock_filter backwards[]={I(0x05,0xffffffffU),I(0x06,SECCOMP_RET_ALLOW)};
    failed+=check(backwards,2,0);
    hl_sock_filter dead_scratch[]={I(0x06,SECCOMP_RET_ALLOW),I(0x60,0),I(0x06,SECCOMP_RET_ALLOW)};
    failed+=check(dead_scratch,3,0);
    hl_sock_filter divzero[]={I(0x34,0),I(0x06,SECCOMP_RET_ALLOW)};
    failed+=check(divzero,2,0);
    hl_sock_filter shift[]={I(0x64,32),I(0x06,SECCOMP_RET_ALLOW)};
    failed+=check(shift,2,0);
    uint16_t ops[]={0x04,0x0c,0x14,0x1c,0x24,0x2c,0x34,0x3c,0x44,0x4c,
        0x54,0x5c,0x64,0x6c,0x74,0x7c,0x84,0xa4,0xac};
    uint32_t values[]={1,31,32,33,0x80000001U,0xffffffffU};
    for (unsigned i=0;i<sizeof(ops)/sizeof(*ops);++i)
        for(unsigned j=0;j<sizeof(values)/sizeof(*values);++j)
            failed+=arithmetic(ops[i],values[j],0xf1234567U+j);
    puts(failed ? "seccomp BPF reference FAILED" : "seccomp BPF reference passed: 10 verifier and 114 arithmetic cases");
    return failed ? 1 : 0;
}
