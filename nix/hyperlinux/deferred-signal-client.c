/* Test-only runtime injection queues SIGUSR1 after the marked gettid's safe
 * HVC check and forces a real canceled EL1 exit. No syscall follows the marked
 * call before delivery. The IRQ-delivered handler nests a real SIGBUS. */
#define _GNU_SOURCE
#include <assert.h>
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
static pid_t main_tid;
static volatile sig_atomic_t bus_seen, user_seen, second_seen;
static void on_second(int signum) {
    if (signum != SIGUSR2 || syscall(SYS_gettid) != main_tid || second_seen) _exit(93);
    second_seen = 1;
}
static void on_bus(int signum, siginfo_t *info, void *context) {
    if (signum != SIGBUS || info->si_code != BUS_ADRERR ||
        info->si_addr != fault_address || bus_seen || !user_seen) _exit(90);
    bus_seen = 1;
    ((ucontext_t *)context)->uc_mcontext.pc += 4;
}
static void on_user(int signum) {
    if (signum != SIGUSR1 || user_seen) _exit(91);
    user_seen = 1;
    /* A second signal must arrive without relying on a syscall in the first
     * handler. This also verifies IRQ rearming for distinct queued signals. */
    while (!second_seen) __asm__ volatile("" ::: "memory");
    if (syscall(SYS_gettid) != main_tid) _exit(94);
    volatile uint64_t cookie = UINT64_C(0x123456789abcdef0);
    uint32_t value;
    __asm__ volatile("ldr %w0, [%1]" : "=r"(value) : "r"(fault_address) : "memory");
    (void)value;
    if (!bus_seen || cookie != UINT64_C(0x123456789abcdef0)) _exit(92);
    user_seen = 2;
}
int main(void) {
    main_tid = syscall(SYS_gettid);
    struct sigaction bus = { .sa_sigaction = on_bus, .sa_flags = SA_SIGINFO };
    struct sigaction user = { .sa_handler = on_user };
    struct sigaction second = { .sa_handler = on_second };
    sigemptyset(&bus.sa_mask); sigemptyset(&user.sa_mask); sigemptyset(&second.sa_mask);
    assert(!sigaction(SIGBUS, &bus, NULL) && !sigaction(SIGUSR1, &user, NULL));
    assert(!sigaction(SIGUSR2, &second, NULL));
    char path[] = "/tmp/hl-deferred-signal-XXXXXX";
    int fd = mkstemp(path);
    assert(fd >= 0 && !unlink(path) && !ftruncate(fd, 16384));
    fault_address = mmap(NULL, 16384, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    assert(fault_address != MAP_FAILED);
    *(volatile uint32_t *)fault_address = 0x12345678;
    assert(*(volatile uint32_t *)fault_address == 0x12345678);
    assert(!ftruncate(fd, 0));
    assert(syscall(SYS_gettid, UINT64_C(0x484c534947)) == main_tid);
    while (!user_seen) __asm__ volatile("" ::: "memory");
    assert(user_seen == 2 && bus_seen == 1 && second_seen == 1);
    assert(!munmap(fault_address, 16384) && !close(fd));
    puts("deferred-signal-client: canceled EL1 to syscall-free EL0 IRQ delivery, exact TID and nested SIGBUS returns passed");
    return 0;
}
