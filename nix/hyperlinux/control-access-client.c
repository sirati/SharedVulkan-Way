/* Actual guest CPU and syscall authority checks. Each CPU mode runs in its
 * own guest process, so a native host crash cannot count as guest SIGSEGV. */
#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <unistd.h>

static void fault(int sig) {
    const char message[]="CONTROL_GUEST_SIGSEGV\n";
    _exit(sig==SIGSEGV && write(1,message,sizeof(message)-1)==sizeof(message)-1 ? 0 : 98);
}
static void denied(uintptr_t address) {
    void *p=(void *)address;
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
    char file[]="/tmp/hl-control-XXXXXX";int fd=mkstemp(file);assert(fd>=0);
    assert(!unlink(file));assert(!ftruncate(fd,16384));
    errno=0;assert(mmap(p,4096,PROT_READ|PROT_WRITE,
        MAP_FIXED|MAP_SHARED,fd,0)==MAP_FAILED && errno==EPERM);
    close(fd);
}
int main(int argc,char **argv) {
    assert(argc==3);
    char *end;uintptr_t address=strtoull(argv[2],&end,0);assert(!*end && address);
    if(!strcmp(argv[1],"--boundary")) {
        assert(!(address & 4095));
        void *p=mmap((void *)(address-4096),4096,PROT_READ|PROT_WRITE,
            MAP_FIXED|MAP_PRIVATE|MAP_ANONYMOUS,-1,0);assert(p!=MAP_FAILED);
        int pair[2];assert(!socketpair(AF_UNIX,SOCK_STREAM,0,pair));
        char bytes[8192];memset(bytes,0x5a,sizeof(bytes));
        assert(write(pair[0],bytes,sizeof(bytes))==sizeof(bytes));
        assert(read(pair[1],p,sizeof(bytes))==4096);
        assert(!memcmp(p,bytes,4096));
        errno=0;assert(read(pair[1],(void *)address,4096)==-1 && errno==EFAULT);
        /* Copying a metadata struct across the boundary must also stop. */
        errno=0;assert(syscall(SYS_gettimeofday,(void *)(address-8),NULL)==-1 && errno==EFAULT);
        close(pair[0]);close(pair[1]);
        puts("control boundary copy authority passed");return 0;
    }
    if(!strcmp(argv[1],"--syscalls")) {
        denied(address);puts("control syscall authority passed");return 0;
    }
    struct sigaction sa={.sa_handler=fault};sigemptyset(&sa.sa_mask);
    assert(!sigaction(SIGSEGV,&sa,NULL));
    if(!strcmp(argv[1],"--cpu-write")) *(volatile uint32_t *)address=0;
    else if(!strcmp(argv[1],"--cpu-read")) (void)*(volatile uint32_t *)address;
    else if(!strcmp(argv[1],"--cpu-exec")) ((void (*)(void))address)();
    else assert(0);
    return 99;
}
