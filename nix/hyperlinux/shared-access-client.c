/* Stock Linux syscall acceptance for the integrated Hyper Linux runtime.
 * No compositor, native service, frame copy or real GPU is required. */
#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <linux/futex.h>
#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>

static void output_denied(void *map) {
    struct timeval *tv = map;
    errno=0; assert(syscall(SYS_gettimeofday,tv,NULL)==-1 && errno==EFAULT);
    int pair[2]; assert(!socketpair(AF_UNIX,SOCK_STREAM,0,pair));
    assert(write(pair[0],"x",1)==1);
    struct iovec iov={map,1};
    errno=0; assert(readv(pair[1],&iov,1)==-1 && errno==EFAULT);
    struct msghdr msg={.msg_iov=&iov,.msg_iovlen=1};
    errno=0; assert(recvmsg(pair[1],&msg,MSG_DONTWAIT)==-1 && errno==EFAULT);
    close(pair[0]);close(pair[1]);
}
static void guest_fault(int signal) {
    const char *message=signal==SIGBUS ? "GUEST_SIGBUS\n" : "GUEST_SIGSEGV\n";
    size_t length=signal==SIGBUS ? 13 : 14;
    ssize_t written=write(1,message,length);
    _exit(written==(ssize_t)length ? 0 : 98);
}
int main(int argc,char **argv) {
    if(argc==2 && !strcmp(argv[1],"--guest-map-fault")) {
        char name[]="/tmp/hl-guest-fault-XXXXXX";int fd=mkstemp(name);assert(fd>=0);
        assert(!unlink(name));assert(!ftruncate(fd,16384));
        void *p=mmap(NULL,16384,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0);assert(p!=MAP_FAILED);
        struct sigaction action={.sa_handler=guest_fault};sigemptyset(&action.sa_mask);
        assert(!sigaction(SIGBUS,&action,NULL));assert(!sigaction(SIGSEGV,&action,NULL));
        assert(!ftruncate(fd,0));
        (void)*(volatile uint32_t *)p;
        _exit(99);
    }
    assert(argc==1);
    char path[]="/tmp/hl-access-XXXXXX";
    int rw=mkstemp(path);assert(rw>=0);assert(!ftruncate(rw,16384));
    int ro=open(path,O_RDONLY);assert(ro>=0);assert(!unlink(path));
    void *map=mmap(NULL,16384,PROT_READ,MAP_SHARED,ro,0);
    if(map==MAP_FAILED) {
        int error=errno;
        fprintf(stderr,"O_RDONLY MAP_SHARED PROT_READ failed: errno=%d (%s), fd=%d flags=%d\n",error,strerror(error),ro,fcntl(ro,F_GETFL));
        if(!ftruncate(rw,65536)) {
            errno=0;void *probe=mmap(NULL,65536,PROT_READ,MAP_SHARED,ro,0);int e=errno;
            fprintf(stderr,"diagnostic RO64KiB result=%s errno=%d (%s)\n",probe==MAP_FAILED?"failed":"mapped",e,strerror(e));
            if(probe!=MAP_FAILED)assert(!munmap(probe,65536));
            errno=0;probe=mmap(NULL,16384,PROT_READ|PROT_WRITE,MAP_SHARED,rw,0);e=errno;
            fprintf(stderr,"diagnostic RW16KiB result=%s errno=%d (%s)\n",probe==MAP_FAILED?"failed":"mapped",e,strerror(e));
            if(probe!=MAP_FAILED)assert(!munmap(probe,16384));
        }
        errno=error;perror("shared access read-only mmap");
        return 1;
    }
    output_denied(map);assert(!munmap(map,16384));close(ro);
    map=mmap(NULL,16384,PROT_READ|PROT_WRITE,MAP_SHARED,rw,0);assert(map!=MAP_FAILED);
    assert(!mprotect(map,16384,PROT_READ));output_denied(map);
    assert(!mprotect(map,16384,PROT_NONE));output_denied(map);
    int pair[2];assert(!socketpair(AF_UNIX,SOCK_STREAM,0,pair));
    struct iovec iov={map,1};struct msghdr msg={.msg_iov=&iov,.msg_iovlen=1};
    errno=0;assert(writev(pair[0],&iov,1)==-1 && errno==EFAULT);
    errno=0;assert(sendmsg(pair[0],&msg,MSG_DONTWAIT)==-1 && errno==EFAULT);
    assert(!mprotect(map,16384,PROT_READ|PROT_WRITE));
    assert(!syscall(SYS_gettimeofday,map,NULL));
    assert(write(pair[0],"y",1)==1);assert(readv(pair[1],&iov,1)==1);
    assert(*(char *)map=='y');close(pair[0]);close(pair[1]);
    assert(!munmap(map,16384));
    map=mmap(NULL,16384,PROT_READ|PROT_WRITE,MAP_SHARED,rw,0);assert(map!=MAP_FAILED);
    *(uint32_t *)map=0;
    struct timespec timeout={0,0};errno=0;
    assert(syscall(SYS_futex,map,FUTEX_WAIT,0,&timeout,NULL,0)==-1 && errno==ETIMEDOUT);
    assert(!ftruncate(rw,0));output_denied(map);
    errno=0;assert(syscall(SYS_futex,map,FUTEX_WAIT,0,&timeout,NULL,0)==-1 && errno==EFAULT);
    errno=0;assert(syscall(SYS_futex,map,FUTEX_WAKE_OP,1,1,map,FUTEX_OP(FUTEX_OP_ADD,1,FUTEX_OP_CMP_EQ,0))==-1 && errno==EFAULT);
    errno=0;assert(syscall(SYS_futex,map,FUTEX_TRYLOCK_PI,0,NULL,NULL,0)==-1 && errno==EFAULT);
    errno=0;assert(syscall(SYS_futex,map,FUTEX_UNLOCK_PI,0,NULL,NULL,0)==-1 && errno==EFAULT);
    errno=0;assert(syscall(SYS_openat,AT_FDCWD,map,O_RDONLY,0)==-1 && errno==EFAULT);
    assert(!munmap(map,16384));close(rw);
    puts("shared-access-client: O_RDONLY, PROT_READ, PROT_NONE EFAULT and vectored/message/futex truncate direction passed");
}
