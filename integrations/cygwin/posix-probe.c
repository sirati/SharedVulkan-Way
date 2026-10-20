/* SPDX-License-Identifier: MIT
 * Verify unrelated Cygwin mappings still use the real runtime implementation.
 */
#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>
int wc_cygwin_fd_words(int, uint32_t *);
int main(void)
{
    unsigned char *memory = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    assert(memory != MAP_FAILED);
    memory[0] = 0x35; memory[4095] = 0x8c;
    assert(memory[0] == 0x35 && memory[4095] == 0x8c);
    assert(!munmap(memory, 4096));
    uint32_t descriptor[7];
    assert(wc_cygwin_fd_words(-1, descriptor) == -1 && errno == ENOTSUP);
    puts("Cygwin POSIX fallback mapping passed");
    return 0;
}
