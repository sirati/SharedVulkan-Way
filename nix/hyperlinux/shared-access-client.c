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
int main(void) {
    char path[]="/tmp/hl-access-XXXXXX";
    int rw=mkstemp(path);assert(rw>=0);assert(!ftruncate(rw,16384));
    int ro=open(path,O_RDONLY);assert(ro>=0);assert(!unlink(path));
    void *map=mmap(NULL,16384,PROT_READ,MAP_SHARED,ro,0);assert(map!=MAP_FAILED);
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
    assert(!ftruncate(rw,0));output_denied(map);
    errno=0;assert(syscall(SYS_openat,AT_FDCWD,map,O_RDONLY,0)==-1 && errno==EFAULT);
    assert(!munmap(map,16384));close(rw);
    puts("shared-access-client: O_RDONLY, PROT_READ, PROT_NONE EFAULT and vectored/message direction passed");
}
