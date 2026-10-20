/* A directed asynchronous signal interrupts live EL0 code inside the handler
 * for an actual truncated-page SIGBUS, then both rt_sigreturn paths resume. */
#define _GNU_SOURCE
#include <assert.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <ucontext.h>
#include <unistd.h>

static void *fault_address;
static _Atomic int in_bus, asynchronous;
static pid_t main_pid, main_tid;
static void on_async(int signum) {
    if (signum != SIGUSR1 || !atomic_load_explicit(&in_bus, memory_order_acquire) ||
        atomic_load_explicit(&asynchronous, memory_order_relaxed)) _exit(92);
    atomic_store_explicit(&asynchronous, 1, memory_order_release);
}
static void on_bus(int signum, siginfo_t *info, void *context) {
    if (signum != SIGBUS || info->si_code != BUS_ADRERR ||
        info->si_addr != fault_address || atomic_load_explicit(&in_bus, memory_order_relaxed)) _exit(90);
    volatile uint64_t cookie = UINT64_C(0x123456789abcdef0);
    atomic_store_explicit(&in_bus, 1, memory_order_release);
    /* No syscall or shim boundary in this loop. The worker must interrupt
     * the live user PC, preserve this stack and return here through sigreturn. */
    while (!atomic_load_explicit(&asynchronous, memory_order_acquire))
        __asm__ volatile("" ::: "memory");
    if (cookie != UINT64_C(0x123456789abcdef0)) _exit(91);
    ucontext_t *uc = context;
    uc->uc_mcontext.pc += 4;
}
static void *worker(void *unused) {
    (void)unused;
    while (!atomic_load_explicit(&in_bus, memory_order_acquire))
        __asm__ volatile("" ::: "memory");
    if (syscall(SYS_tgkill, main_pid, main_tid, SIGUSR1)) _exit(93);
    return NULL;
}
int main(void) {
    main_pid = getpid(); main_tid = syscall(SYS_gettid);
    struct sigaction bus = { .sa_sigaction = on_bus, .sa_flags = SA_SIGINFO };
    struct sigaction async = { .sa_handler = on_async };
    sigemptyset(&bus.sa_mask); sigemptyset(&async.sa_mask);
    assert(!sigaction(SIGBUS, &bus, NULL) && !sigaction(SIGUSR1, &async, NULL));
    char path[] = "/tmp/hl-async-fault-XXXXXX";
    int fd = mkstemp(path);
    assert(fd >= 0 && !unlink(path) && !ftruncate(fd, 16384));
    fault_address = mmap(NULL, 16384, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    assert(fault_address != MAP_FAILED);
    *(volatile uint32_t *)fault_address = 0x76543210;
    assert(*(volatile uint32_t *)fault_address == 0x76543210);
    pthread_t thread;
    assert(!pthread_create(&thread, NULL, worker, NULL));
    assert(!ftruncate(fd, 0));
    uint32_t value;
    __asm__ volatile("ldr %w0, [%1]" : "=r"(value) : "r"(fault_address) : "memory");
    (void)value;
    assert(atomic_load(&in_bus) == 1 && atomic_load(&asynchronous) == 1);
    assert(!pthread_join(thread, NULL));
    assert(!munmap(fault_address, 16384) && !close(fd));
    puts("async-fault-client: directed EL0 signal inside SIGBUS handler and both rt_sigreturn paths passed");
    return 0;
}
