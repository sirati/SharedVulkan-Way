#define _GNU_SOURCE
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <linux/audit.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <ucontext.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifndef SYS_SECCOMP
#define SYS_SECCOMP 1
#endif
#define CHECK(c) do { if (!(c)) { fprintf(stderr,"seccomp probe failed line %d errno=%d\n",__LINE__,errno); return 1; } } while(0)
static int filter(unsigned nr, unsigned action, unsigned flags) {
    struct sock_filter p[]={BPF_STMT(BPF_LD|BPF_W|BPF_ABS,0),
        BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,nr,0,1),BPF_STMT(BPF_RET|BPF_K,action),
        BPF_STMT(BPF_RET|BPF_K,SECCOMP_RET_ALLOW)};
    struct sock_fprog f={4,p};
    return syscall(SYS_seccomp,SECCOMP_SET_MODE_FILTER,flags,&f);
}
static volatile sig_atomic_t trapped;
static void trap(int sig,siginfo_t *info,void *opaque) {
    ucontext_t *u=opaque;
    int ok=sig==SIGSYS && info->si_code==SYS_SECCOMP && info->si_errno==73 &&
        info->si_syscall==SYS_getppid && info->si_arch==AUDIT_ARCH_AARCH64 && info->si_call_addr;
#if defined(__aarch64__)
    ok=ok && u->uc_mcontext.regs[0]==0x12345678;
    u->uc_mcontext.regs[0]=0x76543210;
#else
    (void)u; ok=0;
#endif
    trapped=ok ? 1 : -1;
}
static atomic_int stage, divergent_tid;
static void *divergent_worker(void *unused) {
    (void)unused;
    if (prctl(PR_SET_NO_NEW_PRIVS,1,0,0,0) || filter(SYS_getppid,SECCOMP_RET_ERRNO|88,0)) return NULL;
    atomic_store(&divergent_tid,(int)syscall(SYS_gettid));
    atomic_store(&stage,1);
    while (atomic_load(&stage)==1) sched_yield();
    errno=0; long r=syscall(SYS_getppid);
    int ok=r==-1 && errno==88;
    ok=ok && syscall(SYS_getpid)>0;
    return (void *)(long)ok;
}
static void *killed_worker(void *unused) {
    (void)unused;
    if (prctl(PR_SET_NO_NEW_PRIVS,1,0,0,0) || filter(SYS_getppid,SECCOMP_RET_KILL_THREAD,0)) return NULL;
    atomic_store(&stage,1);
    syscall(SYS_getppid);
    atomic_store(&stage,2);
    return NULL;
}
static void *worker(void *unused) {
    (void)unused;
    int before=prctl(PR_GET_NO_NEW_PRIVS,0,0,0,0);
    atomic_store(&stage,1);
    while (atomic_load(&stage)==1) sched_yield();
    errno=0; long r=syscall(SYS_getppid);
    return (void *)(long)(before==0 && r==-1 && errno==77 && prctl(PR_GET_NO_NEW_PRIVS,0,0,0,0)==1);
}
static void *status_worker(void *unused) {
    (void)unused;
    atomic_store(&stage,1);
    for (;;) sched_yield();
    return NULL;
}
static int wait_status_tests(const char *self) {
    for (int mode=0;mode<10;++mode) {
        pid_t child=fork(); CHECK(child>=0);
        if (!child) {
            if (mode==0) _exit(159);
            if (mode==8) _exit(143);
            if (mode==9) {
                pthread_t t; CHECK(pthread_create(&t,NULL,killed_worker,NULL)==0);
                CHECK(pthread_join(t,NULL)==0 && atomic_load(&stage)==1);
                _exit(159);
            }
            if (mode==6) { raise(SIGTERM); _exit(92); }
            CHECK(prctl(PR_SET_NO_NEW_PRIVS,1,0,0,0)==0);
            if (mode==3) {
                struct sigaction a={0}; a.sa_handler=SIG_IGN;
                CHECK(sigaction(SIGSYS,&a,NULL)==0);
            }
            if (mode==4) {
                sigset_t set; sigemptyset(&set); sigaddset(&set,SIGSYS);
                CHECK(pthread_sigmask(SIG_BLOCK,&set,NULL)==0);
            }
            unsigned action=mode==1 || mode==7 ? SECCOMP_RET_KILL_PROCESS :
                            mode==5 ? SECCOMP_RET_KILL_THREAD : SECCOMP_RET_TRAP|73;
            CHECK(filter(SYS_getppid,action,0)==0);
            if (mode==1) {
                pthread_t t; CHECK(pthread_create(&t,NULL,status_worker,NULL)==0);
                while (!atomic_load(&stage)) sched_yield();
            }
            if (mode==7) {
                char *args[]={(char *)self,"--status-exec-child",NULL};
                execv(self,args); _exit(93);
            }
            syscall(SYS_getppid);
            _exit(94);
        }
        int status;
        if (mode==2) {
            siginfo_t info={0};
            CHECK(waitid(P_PID,(id_t)child,&info,WEXITED|WNOWAIT)==0);
            CHECK(info.si_pid==child && info.si_status==SIGSYS &&
                  (info.si_code==CLD_KILLED || info.si_code==CLD_DUMPED));
            memset(&info,0,sizeof(info));
            CHECK(waitid(P_PID,(id_t)child,&info,WEXITED|WNOWAIT)==0 &&
                  info.si_pid==child && info.si_status==SIGSYS);
        }
        CHECK(waitpid(mode==1 ? -1 : child,&status,0)==child);
        if (mode==0 || mode==8 || mode==9) {
            CHECK(WIFEXITED(status) && WEXITSTATUS(status)==(mode==8 ? 143 : 159));
        } else {
            CHECK(WIFSIGNALED(status) && WTERMSIG(status)==(mode==6 ? SIGTERM : SIGSYS));
        }
        errno=0; CHECK(waitpid(child,&status,WNOHANG)==-1 && errno==ECHILD);
    }
    puts("seccomp fatal waits: KILL_PROCESS group, TRAP default/ignored/blocked, KILL_THREAD, SIGTERM, exec, WNOWAIT, worker-only fatal and normal159/143 passed");
    return 0;
}
int main(int argc,char **argv) {
    if (argc==2 && !strcmp(argv[1],"--status-exec-child")) {
        syscall(SYS_getppid); return 94;
    }
    if (argc==2 && !strcmp(argv[1],"--wait-status")) return wait_status_tests(argv[0]);
    if (argc==2 && !strcmp(argv[1],"--exec-child")) {
        errno=0; CHECK(syscall(SYS_getppid)==-1 && errno==71);
        CHECK(prctl(PR_GET_NO_NEW_PRIVS,0,0,0,0)==1);
        return 0;
    }
    CHECK(argc==2);
    CHECK(prctl(PR_GET_NO_NEW_PRIVS,0,0,0,0)==0);
    if (!strcmp(argv[1],"--basic")) {
        errno=0; CHECK(prctl(PR_SET_SECCOMP,SECCOMP_MODE_FILTER,NULL)==-1 && errno==EFAULT);
        errno=0; CHECK(syscall(SYS_seccomp,SECCOMP_SET_MODE_FILTER,1,NULL)==-1 && errno==EFAULT);
        errno=0; CHECK(filter(SYS_getppid,SECCOMP_RET_ERRNO|71,5)==-1 && errno==EINVAL);
        errno=0; CHECK(filter(SYS_getppid,SECCOMP_RET_ERRNO|71,0)==-1 && errno==EACCES);
        errno=0; CHECK(prctl(PR_SET_NO_NEW_PRIVS,0,0,0,0)==-1 && errno==EINVAL);
        CHECK(prctl(PR_SET_NO_NEW_PRIVS,1,0,0,0)==0);
        CHECK(prctl(PR_GET_NO_NEW_PRIVS,0,0,0,0)==1);
        errno=0; CHECK(prctl(PR_SET_NO_NEW_PRIVS,0,0,0,0)==-1 && errno==EINVAL);
        errno=0; CHECK(filter(SYS_getppid,SECCOMP_RET_ERRNO|71,1U<<31)==-1 && errno==EINVAL);
        CHECK(filter(SYS_getppid,SECCOMP_RET_ERRNO|71,0)==0);
        errno=0; CHECK(syscall(SYS_getppid)==-1 && errno==71);
        CHECK(prctl(PR_GET_SECCOMP)==2);
        pid_t child=fork(); CHECK(child>=0);
        if (!child) {
            char *args[]={argv[0],"--exec-child",NULL};
            execv(argv[0],args); _exit(93);
        }
        int status; CHECK(waitpid(child,&status,0)==child && WIFEXITED(status) && WEXITSTATUS(status)==0);
        puts("seccomp basic: NNP, unsupported flags, ERRNO, fork+exec inheritance passed");
    } else if (!strcmp(argv[1],"--backend-denial")) {
        int fds[2]; CHECK(pipe2(fds,O_NONBLOCK)==0);
        CHECK(prctl(PR_SET_NO_NEW_PRIVS,1,0,0,0)==0);
        struct sock_filter p[]={BPF_STMT(BPF_LD|BPF_W|BPF_ABS,0),
            BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,SYS_write,0,3),
            BPF_STMT(BPF_LD|BPF_W|BPF_ABS,16),
            BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,(unsigned)fds[1],0,1),
            BPF_STMT(BPF_RET|BPF_K,SECCOMP_RET_ERRNO|79),BPF_STMT(BPF_RET|BPF_K,SECCOMP_RET_ALLOW)};
        struct sock_fprog f={6,p}; CHECK(syscall(SYS_seccomp,1,0,&f)==0);
        errno=0; CHECK(write(fds[1],"x",1)==-1 && errno==79);
        char ch; errno=0; CHECK(read(fds[0],&ch,1)==-1 && errno==EAGAIN);
        CHECK(close(fds[1])==0); CHECK(read(fds[0],&ch,1)==0); CHECK(close(fds[0])==0);
        puts("seccomp backend denial: filtered write delivered no pipe byte");
    } else if (!strcmp(argv[1],"--trap")) {
        struct sigaction a={0}; a.sa_sigaction=trap; a.sa_flags=SA_SIGINFO;
        CHECK(sigaction(SIGSYS,&a,NULL)==0);
        CHECK(prctl(PR_SET_NO_NEW_PRIVS,1,0,0,0)==0);
        CHECK(filter(SYS_getppid,SECCOMP_RET_TRAP|73,0)==0);
        CHECK(syscall(SYS_getppid,0x12345678)==0x76543210 && trapped==1);
        puts("seccomp trap: SIGSYS metadata and saved syscall argument passed");
    } else if (!strcmp(argv[1],"--stacked")) {
        CHECK(prctl(PR_SET_NO_NEW_PRIVS,1,0,0,0)==0);
        CHECK(filter(SYS_getppid,SECCOMP_RET_ERRNO|71,0)==0);
        CHECK(filter(SYS_getppid,0x12340000U,0)==0);
        errno=0; CHECK(syscall(SYS_getppid)==-1 && errno==71);
        CHECK(filter(SYS_getppid,SECCOMP_RET_ERRNO|72,0)==0);
        errno=0; CHECK(syscall(SYS_getppid)==-1 && errno==72);
        puts("seccomp stacked: raw action precedence and newest data passed");
    } else if (!strcmp(argv[1],"--tsync-divergent")) {
        pthread_t t; CHECK(pthread_create(&t,NULL,divergent_worker,NULL)==0);
        while (!atomic_load(&stage)) sched_yield();
        CHECK(prctl(PR_SET_NO_NEW_PRIVS,1,0,0,0)==0);
        CHECK(filter(SYS_getpid,SECCOMP_RET_ERRNO|89,0)==0);
        CHECK(filter(SYS_getppid,SECCOMP_RET_ERRNO|90,SECCOMP_FILTER_FLAG_TSYNC)==atomic_load(&divergent_tid));
        errno=0; CHECK(syscall(SYS_getpid)==-1 && errno==89);
        CHECK(syscall(SYS_getppid)>=0);
        atomic_store(&stage,2); void *result; CHECK(pthread_join(t,&result)==0 && result==(void *)1);
        puts("seccomp divergent TSYNC: offending TID and no partial update passed");
    } else if (!strcmp(argv[1],"--kill-thread")) {
        pthread_t t; CHECK(pthread_create(&t,NULL,killed_worker,NULL)==0);
        CHECK(pthread_join(t,NULL)==0);
        CHECK(atomic_load(&stage)==1);
        puts("seccomp KILL_THREAD: worker stopped and parent survived");
    } else if (!strcmp(argv[1],"--arguments")) {
        CHECK(prctl(PR_SET_NO_NEW_PRIVS,1,0,0,0)==0);
        struct sock_filter p[]={BPF_STMT(BPF_LD|BPF_W|BPF_ABS,0),
            BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,SYS_getppid,0,3),
            BPF_STMT(BPF_LD|BPF_W|BPF_ABS,20),
            BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,0x12345678U,0,1),
            BPF_STMT(BPF_RET|BPF_K,SECCOMP_RET_ERRNO|93),BPF_STMT(BPF_RET|BPF_K,SECCOMP_RET_ALLOW)};
        struct sock_fprog f={6,p}; CHECK(syscall(SYS_seccomp,1,0,&f)==0);
        errno=0; CHECK(syscall(SYS_getppid,0x1234567800000000ULL)==-1 && errno==93);
        CHECK(syscall(SYS_getppid,0x8765432100000000ULL)>=0);
        puts("seccomp arguments: actual 64-bit register high words passed");
    } else if (!strcmp(argv[1],"--tsync")) {
        pthread_t t; CHECK(pthread_create(&t,NULL,worker,NULL)==0);
        while (!atomic_load(&stage)) sched_yield();
        CHECK(prctl(PR_SET_NO_NEW_PRIVS,1,0,0,0)==0);
        CHECK(filter(SYS_getppid,SECCOMP_RET_ERRNO|77,SECCOMP_FILTER_FLAG_TSYNC)==0);
        atomic_store(&stage,2); void *result; CHECK(pthread_join(t,&result)==0 && result==(void *)1);
        puts("seccomp TSYNC: existing thread receives filter and NNP");
    } else return 2;
    return 0;
}
