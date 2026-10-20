/* Guest-parent proof that forced BUS termination survives the private host
 * child-status transport as a signal, including a nonconsuming waitid read. */
#define _GNU_SOURCE
#include <assert.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static void check_child(const char *program,const char *mode,int fatal) {
    pid_t parent=getpid();
    pid_t child=fork();assert(child>=0);
    if(!child) {
        if(!strcmp(mode,"exit135"))_exit(135);
        execl(program,program,mode,(char *)NULL);
        _exit(97);
    }
    siginfo_t info;memset(&info,0,sizeof(info));
    assert(!waitid(P_PID,(id_t)child,&info,WEXITED|WNOWAIT));
    assert(info.si_pid==child && info.si_signo==SIGCHLD);
    if(fatal) {
        assert((info.si_code==CLD_KILLED || info.si_code==CLD_DUMPED) && info.si_status==SIGBUS);
    } else {
        assert(info.si_code==CLD_EXITED && info.si_status==(!strcmp(mode,"exit135") ? 135 : 0));
    }
    int status=-1;assert(waitpid(child,&status,0)==child);
    if(fatal)assert(WIFSIGNALED(status) && WTERMSIG(status)==SIGBUS && !WIFEXITED(status));
    else assert(WIFEXITED(status) && WEXITSTATUS(status)==(!strcmp(mode,"exit135") ? 135 : 0));
    assert(getpid()==parent);
    printf("fatal-bus-wait-client: %s waitid WNOWAIT and waitpid kind passed\n",mode);
}
int main(int argc,char **argv) {
    assert(argc==2);
    check_child(argv[1],"default",1);
    check_child(argv[1],"blocked",1);
    check_child(argv[1],"ignored",1);
    /* A handled BUS leaves ordinary pending USR1 intact and exits normally. */
    check_child(argv[1],"pending",0);
    /* A numerically equal shell-style exit must remain an ordinary exit. */
    check_child(argv[1],"exit135",0);
    puts("fatal-bus-wait-client: combined forced BUS fatal-kind and pending-signal preservation passed");
    return 0;
}
