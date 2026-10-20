/* HyperLinux supports same-VM thread tracing for its app debugger. Exercise
 * SETREGSET rejection, then actual ERET and a second live stopped snapshot. */
#define _GNU_SOURCE
#include <assert.h>
#include <elf.h>
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>
#include <asm/ptrace.h>
static _Atomic int tid, go;
static volatile uint64_t live_nzcv;
static const uint64_t bad[]={5,4,0x10,0x80,0x3c0,1ULL<<20,1ULL<<21,
    1ULL<<22,1ULL<<23,1ULL<<13,1ULL<<63};
static void *tracee(void *unused) {
    (void)unused;atomic_store(&tid,(int)syscall(SYS_gettid));
    while(!atomic_load(&go))__asm__ volatile("" ::: "memory");
    for(size_t i=0;i<sizeof(bad)/sizeof(*bad)+3;i++) {
        __asm__ volatile("brk #0\n mrs x9, nzcv\n str x9, [%0]\n brk #0"
            :: "r"(&live_nzcv) : "x9","memory");
    }
    return NULL;
}
static void stopped(int target) {
    int status;assert(waitpid(target,&status,__WALL)==target);
    assert(WIFSTOPPED(status) && WSTOPSIG(status)==SIGTRAP);
}
static struct user_pt_regs get(int target) {
    struct user_pt_regs regs;struct iovec iov={&regs,sizeof(regs)};
    assert(!ptrace(PTRACE_GETREGSET,target,(void *)NT_PRSTATUS,&iov));
    assert(iov.iov_len==sizeof(regs));return regs;
}
static long set(int target,struct user_pt_regs *regs,size_t len) {
    struct iovec iov={regs,len};
    return ptrace(PTRACE_SETREGSET,target,(void *)NT_PRSTATUS,&iov);
}
int main(void) {
    int unsafe=0;
    pthread_t worker;assert(!pthread_create(&worker,NULL,tracee,NULL));
    int target;while(!(target=atomic_load(&tid)))sched_yield();
    assert(!ptrace(PTRACE_SEIZE,target,NULL,NULL));atomic_store(&go,1);
    for(size_t i=0;i<sizeof(bad)/sizeof(*bad)+3;i++) {
        stopped(target);struct user_pt_regs before=get(target), proposed=before;
        size_t len=sizeof(proposed);
        if(i<sizeof(bad)/sizeof(*bad))proposed.pstate|=bad[i];
        else if(i==sizeof(bad)/sizeof(*bad))proposed.pc|=1;
        else if(i==sizeof(bad)/sizeof(*bad)+1)proposed.pc=UINT64_C(0xffff000000000000);
        else {proposed.pstate|=5;len=offsetof(struct user_pt_regs,pstate)+1;}
        errno=0;long result=set(target,&proposed,len);
        if(!result) {
            /* Old-runtime negative proof never resumes privileged state. */
            unsafe++;
            fprintf(stderr,"ptrace-pstate-client: forbidden candidate %zu accepted\n",i);
        } else {
            assert(result==-1 && errno==EINVAL);
            struct user_pt_regs unchanged=get(target);
            assert(!memcmp(&before,&unchanged,sizeof(before)));
        }
        proposed=before;proposed.pc+=4;
        uint64_t flags=(i&1) ? UINT64_C(0x90000000) : UINT64_C(0x60000000);
        proposed.pstate=(before.pstate&UINT64_C(0x03001c00))|flags;
        assert(!set(target,&proposed,sizeof(proposed)));
        assert(!ptrace(PTRACE_CONT,target,NULL,NULL));stopped(target);
        struct user_pt_regs after=get(target);
        assert(!(after.pstate&~UINT64_C(0xf3001c00)));
        assert((after.pstate&UINT64_C(0xf0000000))==flags && live_nzcv==flags);
        /* Resume the second BRK only after inspecting actual resumed state. */
        after.pc+=4;assert(!set(target,&after,sizeof(after)));
        assert(!ptrace(PTRACE_CONT,target,NULL,NULL));
    }
    assert(!pthread_join(worker,NULL));
    if(unsafe){fprintf(stderr,"ptrace-pstate-client: %d unsafe candidates accepted\n",unsafe);return 95;}
    puts("ptrace-pstate-client: invalid PSTATE/PC refused without mutation; live resumed NZCV and EL0 state preserved");
    return 0;
}
