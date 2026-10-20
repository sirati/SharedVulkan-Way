/* Guest-origin lifecycle isolation regressions. The controller must require
 * exit zero and the mode-specific marker. A host crash cannot pass. */
#define _GNU_SOURCE
#include <assert.h>
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>

static uintptr_t target;
static const char *operation;
static pid_t main_tid;
static volatile sig_atomic_t worker_fault;
static void on_fault(int sig) {
    static const char worker[]="CONTROL_WORKER_SIGSEGV\n";
    static const char child[]="CONTROL_FORK_SIGSEGV\n";
    const char *message=worker_fault ? worker : child;
    size_t len=worker_fault ? sizeof(worker)-1 : sizeof(child)-1;
    int correct_thread=!worker_fault || syscall(SYS_gettid)!=main_tid;
    _exit(sig==SIGSEGV && correct_thread && write(1,message,len)==(ssize_t)len ? 0 : 98);
}
static void handler(void) {
    struct sigaction sa={.sa_handler=on_fault};
    sigemptyset(&sa.sa_mask);assert(!sigaction(SIGSEGV,&sa,NULL));
}
static void denied(void) {
    void *p=(void *)target;
    errno=0;assert(syscall(SYS_gettimeofday,p,NULL)==-1 && errno==EFAULT);
    int pair[2];assert(!socketpair(AF_UNIX,SOCK_STREAM,0,pair));
    assert(write(pair[0],"x",1)==1);
    errno=0;assert(read(pair[1],p,1)==-1 && errno==EFAULT);
    errno=0;assert(recv(pair[1],p,1,MSG_DONTWAIT)==-1 && errno==EFAULT);
    close(pair[0]);close(pair[1]);
    errno=0;assert(mprotect(p,4096,PROT_READ|PROT_WRITE)==-1 && errno==EPERM);
    errno=0;assert(munmap(p,4096)==-1 && errno==EPERM);
    errno=0;assert(mmap(p,4096,PROT_READ|PROT_WRITE,
        MAP_FIXED|MAP_PRIVATE|MAP_ANONYMOUS,-1,0)==MAP_FAILED && errno==EPERM);
    char file[]="/tmp/hl-lifecycle-map-XXXXXX";int fd=mkstemp(file);assert(fd>=0);
    assert(!unlink(file));assert(!ftruncate(fd,16384));
    errno=0;assert(mmap(p,4096,PROT_READ|PROT_WRITE,
        MAP_FIXED|MAP_SHARED,fd,0)==MAP_FAILED && errno==EPERM);close(fd);
}
static void cpu_access(void) {
    handler();
    if(!strcmp(operation,"read")) (void)*(volatile uint32_t *)target;
    else if(!strcmp(operation,"write")) *(volatile uint32_t *)target=0;
    else if(!strcmp(operation,"exec")) ((void (*)(void))target)();
    else assert(0);
    _exit(99);
}
static void *worker(void *unused) {
    (void)unused;assert(syscall(SYS_gettid)!=main_tid);
    if(!strcmp(operation,"syscalls")) {
        denied();return NULL;
    }
    worker_fault=1;cpu_access();return NULL;
}
static void exec_rejected(void) {
    /* This minimal ELF is parseable and fits the primary allocation, but its
     * PT_LOAD would overwrite control storage. No interpreter obscures which
     * preflight rejected it. An ET_EXEC address must stay unrelocated. */
    char path[]="/tmp/hl-control-exec-XXXXXX";int fd=mkstemp(path);assert(fd>=0);
    Elf64_Ehdr h={0};memcpy(h.e_ident,ELFMAG,SELFMAG);
    h.e_ident[EI_CLASS]=ELFCLASS64;h.e_ident[EI_DATA]=ELFDATA2LSB;
    h.e_ident[EI_VERSION]=EV_CURRENT;h.e_type=ET_EXEC;h.e_machine=EM_AARCH64;
    h.e_version=EV_CURRENT;h.e_ehsize=sizeof(h);h.e_phoff=sizeof(h);
    h.e_phentsize=sizeof(Elf64_Phdr);h.e_phnum=1;h.e_entry=target;
    Elf64_Phdr p={.p_type=PT_LOAD,.p_flags=PF_R|PF_W|PF_X,
        .p_offset=4096,.p_vaddr=target,.p_filesz=4,.p_memsz=4096,.p_align=4096};
    uint32_t ret_instruction=0xd65f03c0;
    assert(write(fd,&h,sizeof(h))==sizeof(h));assert(write(fd,&p,sizeof(p))==sizeof(p));
    assert(pwrite(fd,&ret_instruction,4,4096)==4);assert(!fchmod(fd,0700));close(fd);
    unsigned char *sentinel=mmap(NULL,4096,PROT_READ|PROT_WRITE,
        MAP_PRIVATE|MAP_ANONYMOUS,-1,0);assert(sentinel!=MAP_FAILED);
    memset(sentinel,0x6d,4096);int pipefd[2];assert(!pipe2(pipefd,O_CLOEXEC));
    pid_t pid=getpid();handler();char *args[]={path,NULL};char *env[]={NULL};
    errno=0;assert(execve(path,args,env)==-1 && errno==ENOEXEC);
    assert(getpid()==pid);for(size_t i=0;i<4096;i++)assert(sentinel[i]==0x6d);
    int fdflags=fcntl(pipefd[0],F_GETFD);assert(fdflags>=0 && (fdflags&FD_CLOEXEC));
    fdflags=fcntl(pipefd[1],F_GETFD);assert(fdflags>=0 && (fdflags&FD_CLOEXEC));
    assert(write(pipefd[1],"z",1)==1);char byte=0;assert(read(pipefd[0],&byte,1)==1 && byte=='z');
    struct sigaction sa;assert(!sigaction(SIGSEGV,NULL,&sa));assert(sa.sa_handler==on_fault);
    close(pipefd[0]);close(pipefd[1]);assert(!munmap(sentinel,4096));assert(!unlink(path));
    puts("CONTROL_EXEC_PREPNR_PRESERVED");
}
int main(int argc,char **argv) {
    assert(argc==3);char *end;target=strtoull(argv[2],&end,0);assert(!*end && target);
    main_tid=(pid_t)syscall(SYS_gettid);
    if(!strcmp(argv[1],"--exec-reject")){exec_rejected();return 0;}
    if(!strncmp(argv[1],"--worker-",9)) {
        operation=argv[1]+9;pthread_t thread;assert(!pthread_create(&thread,NULL,worker,NULL));
        assert(!pthread_join(thread,NULL));assert(!strcmp(operation,"syscalls"));
        puts("CONTROL_WORKER_SYSCALLS_DENIED");return 0;
    }
    assert(!strncmp(argv[1],"--fork-",7));operation=argv[1]+7;
    unsigned char *inherited=mmap(NULL,4096,PROT_READ|PROT_WRITE,
        MAP_PRIVATE|MAP_ANONYMOUS,-1,0);assert(inherited!=MAP_FAILED);
    memset(inherited,0x73,4096);
    pid_t child=fork();assert(child>=0);
    if(!child){
        assert(getpid()!=getppid());
        for(size_t i=0;i<4096;i++)assert(inherited[i]==0x73);
        inherited[0]=0x42;
        if(!strcmp(operation,"syscalls")){denied();puts("CONTROL_FORK_SYSCALLS_DENIED");fflush(stdout);_exit(0);}
        cpu_access();
    }
    int status;assert(waitpid(child,&status,0)==child);
    assert(WIFEXITED(status) && WEXITSTATUS(status)==0);
    for(size_t i=0;i<4096;i++)assert(inherited[i]==0x73);
    assert(!munmap(inherited,4096));
    puts("CONTROL_FORK_PARENT_REAPED");return 0;
}
