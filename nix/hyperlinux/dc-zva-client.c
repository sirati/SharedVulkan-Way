#define _GNU_SOURCE
#include <assert.h>
#include <signal.h>
#include <stdint.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <ucontext.h>
#include <unistd.h>
static volatile sig_atomic_t faulted;
static uintptr_t expected;
static void on_fault(int sig, siginfo_t *info, void *context) {
    if (sig != SIGSEGV || (uintptr_t)info->si_addr != expected || faulted) _exit(90);
    faulted = 1;
    ((ucontext_t *)context)->uc_mcontext.pc += 4;
}
static void install(void) {
    struct sigaction action = { .sa_sigaction = on_fault, .sa_flags = SA_SIGINFO };
    sigemptyset(&action.sa_mask); assert(!sigaction(SIGSEGV, &action, NULL));
}
static void zva(void *pointer) {
    register void *operand __asm__("x0") = pointer;
    register uint64_t a __asm__("x9") = UINT64_C(0x123456789abcdef0);
    register uint64_t b __asm__("x10") = UINT64_C(0x223456789abcdef0);
    register uint64_t c __asm__("x11") = UINT64_C(0x323456789abcdef0);
    uint64_t flags;
    __asm__ volatile("msr nzcv, %5\n\t.inst 0xd50b7420\n\tmrs %4, nzcv"
        : "+r"(operand), "+r"(a), "+r"(b), "+r"(c), "=r"(flags)
        : "r"(UINT64_C(0x90000000)) : "cc", "memory");
    assert(operand == pointer && a == UINT64_C(0x123456789abcdef0));
    assert(b == UINT64_C(0x223456789abcdef0) && c == UINT64_C(0x323456789abcdef0));
    assert(flags == UINT64_C(0x90000000));
}
int main(int argc, char **argv) {
    assert(argc == 2); const char *mode = argv[1];
    int valid = !strcmp(mode, "valid"), tagged = !strcmp(mode, "validtag");
    int readonly = !strcmp(mode, "ro"), inaccessible = !strcmp(mode, "none");
    if (valid || tagged || readonly || inaccessible) {
        uint8_t *page = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        assert(page != MAP_FAILED); memset(page, 0xa5, 4096);
        uintptr_t address = (uintptr_t)(page + 17);
        if (readonly || inaccessible) {
            expected = address; install();
            assert(!mprotect(page, 4096, readonly ? PROT_READ : PROT_NONE));
        }
        zva((void *)(tagged ? address | UINT64_C(0xab00000000000000) : address));
        if (inaccessible) assert(!mprotect(page, 4096, PROT_READ | PROT_WRITE));
        assert(faulted == (readonly || inaccessible));
        for (int i = 0; i < 4096; i++)
            assert(page[i] == ((valid || tagged) && i < 64 ? 0 : 0xa5));
        assert(!munmap(page, 4096));
        printf("DCZVA_%s exact line/sentinel and GPR/NZCV preserved PASS\n", mode);
        return 0;
    }
    char *end; uintptr_t address = strtoull(mode, &end, 0); assert(!*end && address);
    expected = address; install();
    zva((void *)(address | UINT64_C(0xab00000000000000)));
    assert(faulted);
    puts("DCZVA_CONTROL tagged original address refused, signal returned, GPR/NZCV preserved PASS");
    return 0;
}
