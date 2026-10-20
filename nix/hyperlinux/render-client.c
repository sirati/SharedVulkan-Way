/* Headless Linux ABI acceptance for the embedded node and seal adapter. */
#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stdatomic.h>
#include <sys/socket.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/wait.h>
#include <unistd.h>
struct version { int major, minor, patch; size_t name_len; char *name; size_t date_len; char *date; size_t desc_len; char *desc; };
struct cap { uint64_t name, value; };
struct prime { uint32_t handle, flags; int32_t fd; };
static atomic_int worker_ready;
static void *blocked_worker(void *arg) {
    int fd = *(int *)arg;
    atomic_store_explicit(&worker_ready, 1, memory_order_release);
    char byte;
    (void)recv(fd, &byte, 1, 0);
    return NULL;
}
int main(void) {
    int node = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC); assert(node >= 0);
    int alias = dup(node); assert(alias >= 0); close(node);
    struct stat st; assert(fstat(alias, &st) == 0 && S_ISCHR(st.st_mode));
    assert(major(st.st_rdev) == 226 && minor(st.st_rdev) == 128);
    struct statx sx;
    assert(statx(alias, "", AT_EMPTY_PATH, STATX_TYPE, &sx) == 0);
    assert(S_ISCHR(sx.stx_mode) && sx.stx_rdev_major == 226 && sx.stx_rdev_minor == 128);
    assert(statx(AT_FDCWD, "/dev/dri/renderD128", 0, STATX_TYPE, &sx) == 0);
    assert(S_ISCHR(sx.stx_mode) && sx.stx_rdev_major == 226 && sx.stx_rdev_minor == 128);
    char name[64]={0}, date[64]={0}, desc[128]={0};
    struct version version = { .name_len=63, .name=name, .date_len=63, .date=date, .desc_len=127, .desc=desc };
    assert(ioctl(alias, 0xc0406400u, &version) == 0);
    assert(version.major == 1 && version.name_len == 5 && strcmp(name,"svdrm") == 0);
    assert(ioctl(alias, 0xc0406400u, NULL) < 0 && errno == EFAULT);
    struct cap cap = { 5, 0 }; assert(ioctl(alias,0xc010640cu,&cap) == 0 && cap.value == 3);
    struct pollfd pollnode = { alias, POLLIN | POLLOUT, 0 };
    assert(poll(&pollnode, 1, 20) == 0);
    errno=0; assert(fork() < 0 && errno == EAGAIN);
    int fd=memfd_create("hl-render-seal-check", MFD_CLOEXEC | MFD_ALLOW_SEALING); assert(fd>=0);
    assert(ftruncate(fd,8192)==0);
    struct prime prime = { .fd=fd }; assert(ioctl(alias,0xc00c642eu,&prime)<0 && errno==EINVAL);
    close(alias);
    volatile uint32_t *sync=mmap(NULL,4096,PROT_READ|PROT_WRITE,MAP_SHARED,fd,4096);assert(sync!=MAP_FAILED);
    int seals=F_SEAL_SHRINK|F_SEAL_GROW|F_SEAL_FUTURE_WRITE;
    assert(fcntl(fd,F_ADD_SEALS,seals)==0 && fcntl(fd,F_GET_SEALS)==seals);
    int other=dup(fd);assert(other>=0);close(fd);
    assert(pwrite(other,"X",1,0)<0 && errno==EPERM);
    assert(ftruncate(other,0)<0 && errno==EPERM);
    assert(ftruncate(other,16384)<0 && errno==EPERM);
    assert(mmap(NULL,4096,PROT_READ|PROT_WRITE,MAP_SHARED,other,0)==MAP_FAILED && errno==EPERM);
    void *read=mmap(NULL,4096,PROT_READ,MAP_SHARED,other,0); assert(read!=MAP_FAILED);
    assert(mprotect(read,4096,PROT_READ|PROT_WRITE)<0 && errno==EACCES);
    *sync=42;
    pid_t pid=fork();assert(pid>=0);
    if (!pid) {
        assert(fcntl(other,F_GET_SEALS)==seals);
        assert(pwrite(other,"X",1,0)<0 && errno==EPERM);
        assert(*sync==42); *sync=43; _exit(0);
    }
    int status;assert(waitpid(pid,&status,0)==pid && WIFEXITED(status) && WEXITSTATUS(status)==0);
    assert(*sync==43);
    assert(munmap(read,4096)==0 && munmap((void *)sync,4096)==0);
    assert(fcntl(other,F_ADD_SEALS,F_SEAL_WRITE|F_SEAL_SEAL)==0);
    assert(fcntl(other,F_ADD_SEALS,0)<0 && errno==EPERM);
    close(other);
    /* exit_group must interrupt native recv without shutting down aliases. */
    int sockets[2]; assert(socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) == 0);
    pthread_t worker; assert(pthread_create(&worker, NULL, blocked_worker, &sockets[0]) == 0);
    while (!atomic_load_explicit(&worker_ready, memory_order_acquire)) usleep(100);
    usleep(10000);
    puts("embedded DRM identity, dup, ioctl bounds/caps, quiet poll, fork refusal and sibling seals passed");
}
