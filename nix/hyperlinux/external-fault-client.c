/* Actual EL0 load from the original file mapping after truncation. */
#define _GNU_SOURCE
#include <assert.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <ucontext.h>
#include <unistd.h>

static void *fault_address;
static volatile sig_atomic_t caught;
static volatile sig_atomic_t user_signal;
static void on_user_signal(int signum) {
    if (signum != SIGUSR1 || user_signal) _exit(92);
    user_signal = 1;
}
static void on_fault(int signum, siginfo_t *info, void *context) {
    if (signum != SIGBUS || info->si_code != BUS_ADRERR ||
        info->si_addr != fault_address || caught) _exit(90);
    caught = 1;
    /* The faulting load below is exactly one AArch64 instruction. Returning
     * through rt_sigreturn must restore all state and continue after it. */
    ucontext_t *uc = context;
    uc->uc_mcontext.pc += 4;
}
int main(int argc, char **argv) {
    assert(argc == 2);
    int queued = !strcmp(argv[1], "pending");
    int offset = !strcmp(argv[1], "offset") || !strcmp(argv[1], "write");
    int store = !strcmp(argv[1], "write");
    int handled = queued || offset || !strcmp(argv[1], "handled");
    int blocked = !strcmp(argv[1], "blocked");
    int ignored = !strcmp(argv[1], "ignored");
    assert(handled || blocked || ignored || !strcmp(argv[1], "default"));
    char path[] = "/tmp/hl-external-fault-XXXXXX";
    int fd = mkstemp(path);
    assert(fd >= 0 && !unlink(path) && !ftruncate(fd, 16384));
    void *mapping = mmap(NULL, 16384, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    assert(mapping != MAP_FAILED);
    fault_address = (char *)mapping + (offset ? 4160 : 0);
    *(volatile uint32_t *)fault_address = 0x12345678;
    assert(*(volatile uint32_t *)fault_address == 0x12345678);
    if (handled) {
        struct sigaction action = { .sa_sigaction = on_fault, .sa_flags = SA_SIGINFO };
        sigemptyset(&action.sa_mask);
        assert(!sigaction(SIGBUS, &action, NULL));
    }
    if (blocked) {
        sigset_t mask;
        sigemptyset(&mask);
        sigaddset(&mask, SIGBUS);
        assert(!sigprocmask(SIG_BLOCK, &mask, NULL));
    }
    if (ignored) assert(signal(SIGBUS, SIG_IGN) != SIG_ERR);
    sigset_t user_mask;
    if (queued) {
        struct sigaction action = { .sa_handler = on_user_signal };
        sigemptyset(&action.sa_mask);
        assert(!sigaction(SIGUSR1, &action, NULL));
        sigemptyset(&user_mask);
        sigaddset(&user_mask, SIGUSR1);
        assert(!sigprocmask(SIG_BLOCK, &user_mask, NULL));
        assert(!raise(SIGUSR1));
    }
    assert(!ftruncate(fd, 0));
    uint32_t value = 0x87654321;
    if (store)
        __asm__ volatile("str %w0, [%1]" :: "r"(value), "r"(fault_address) : "memory");
    else
        __asm__ volatile("ldr %w0, [%1]" : "=r"(value) : "r"(fault_address) : "memory");
    (void)value;
    if (!handled || !caught) return 91;
    if (queued) {
        assert(!user_signal);
        assert(!sigprocmask(SIG_UNBLOCK, &user_mask, NULL));
        assert(user_signal == 1);
        puts("external-fault-client: pending SIGUSR1 preserved across SIGBUS passed");
    }
    assert(!munmap(mapping, 16384));
    assert(!close(fd));
    puts("external-fault-client: SIGBUS/BUS_ADRERR address and rt_sigreturn passed");
    return 0;
}
