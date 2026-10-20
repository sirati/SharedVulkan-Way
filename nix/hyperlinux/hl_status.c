/* Private parent/child terminal-status records over the existing fork socket.
 * This socket never appears in the guest fd table and is excluded from further
 * posix_spawn children by CLOEXEC_DEFAULT. No PID or process identity is changed. */
#include "hl_status.h"
#include "thread.h"
#include <errno.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

typedef struct { uint32_t magic, version, kind, value; } terminal_record;
#define STATUS_MAGIC 0x48535453U
#define STATUS_VERSION 1U
static void mark_one(thread_entry_t *t, void *opaque) {
    int sig=*(int *)opaque;
    if (current_thread && t->seccomp_tgid==current_thread->seccomp_tgid)
        atomic_store(&t->termination_signal,sig);
}
void hl_status_mark_signal(int linux_signal) {
    pthread_mutex_lock(thread_get_lock());
    thread_for_each_locked(mark_one,&linux_signal);
    pthread_mutex_unlock(thread_get_lock());
}
int hl_status_current(int exit_code) {
    int sig=current_thread ? atomic_load(&current_thread->termination_signal) : 0;
    return sig ? sig & 0x7f : (exit_code & 255)<<8;
}
void hl_status_publish(int fd,int status) {
    terminal_record rec={STATUS_MAGIC,STATUS_VERSION,0,0};
    if ((status & 0x7f)!=0) { rec.kind=1; rec.value=(uint32_t)(status & 0x7f); }
    else rec.value=(uint32_t)((status>>8)&255);
#ifdef SO_NOSIGPIPE
    int on=1;
    (void)setsockopt(fd,SOL_SOCKET,SO_NOSIGPIPE,&on,sizeof(on));
#endif
    const char *p=(const char *)&rec;
    size_t left=sizeof(rec);
    while (left) {
        int flags=0;
#ifdef MSG_NOSIGNAL
        flags=MSG_NOSIGNAL;
#endif
        ssize_t n=send(fd,p,left,flags);
        if (n<0 && errno==EINTR) continue;
        if (n<=0) break;
        p+=n; left-=(size_t)n;
    }
    close(fd);
}
static int linux_signal(int sig) {
    switch (sig) {
    case SIGHUP:return 1; case SIGINT:return 2; case SIGQUIT:return 3;
    case SIGILL:return 4; case SIGTRAP:return 5; case SIGABRT:return 6;
    case SIGBUS:return 7; case SIGFPE:return 8; case SIGKILL:return 9;
    case SIGUSR1:return 10; case SIGSEGV:return 11; case SIGUSR2:return 12;
    case SIGPIPE:return 13; case SIGALRM:return 14; case SIGTERM:return 15;
    case SIGCHLD:return 17; case SIGCONT:return 18; case SIGSTOP:return 19;
    case SIGTSTP:return 20; case SIGTTIN:return 21; case SIGTTOU:return 22;
    case SIGURG:return 23; case SIGXCPU:return 24; case SIGXFSZ:return 25;
    case SIGVTALRM:return 26; case SIGPROF:return 27; case SIGWINCH:return 28;
    case SIGIO:return 29; case SIGSYS:return 31;
    default:return sig;
    }
}
int hl_status_collect(int fd,int host_status) {
    terminal_record rec={0};
    size_t have=0;
    if (fd>=0) {
        while (have<sizeof(rec)) {
            ssize_t n=recv(fd,(char *)&rec+have,sizeof(rec)-have,MSG_DONTWAIT);
            if (n<0 && errno==EINTR) continue;
            if (n<=0) break;
            have+=(size_t)n;
        }
        close(fd);
    }
    if (WIFEXITED(host_status) && have==sizeof(rec) &&
        rec.magic==STATUS_MAGIC && rec.version==STATUS_VERSION) {
        if (rec.kind==0 && rec.value<=255) return (int)rec.value<<8;
        if (rec.kind==1 && rec.value>0 && rec.value<=64) return (int)rec.value;
    }
    if (WIFEXITED(host_status)) return WEXITSTATUS(host_status)<<8;
    if (WIFSIGNALED(host_status))
        return linux_signal(WTERMSIG(host_status)) | (WCOREDUMP(host_status) ? 0x80 : 0);
#ifdef WIFCONTINUED
    if (WIFCONTINUED(host_status)) return 0xffff;
#endif
    if (WIFSTOPPED(host_status)) return (linux_signal(WSTOPSIG(host_status))<<8)|0x7f;
    return host_status;
}
