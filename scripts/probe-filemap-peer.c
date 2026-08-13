/* The Linux half of scripts/probe-filemap.c: mmap the same path MAP_SHARED,
 * verify the pattern the Windows side wrote through *its* mapping, store
 * 0xAB into byte 0, and wait for 0xCD to appear in byte 1.
 *
 * Two directions on purpose. A one-way check can pass by accident — the reader
 * may simply be faulting in a page the writer already flushed to the
 * filesystem, which is not shared-page coherency and is not enough to carry a
 * frame that is being written while the compositor reads it.
 */
#include <stdio.h>
#include <stdlib.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/mman.h>

int main(int argc, char **argv)
{
    size_t len, k;
    int fd, t, bad = 0;
    unsigned char *p;

    if (argc < 3) { printf("usage: probe-filemap-peer <path> <len>\n"); return 2; }
    len = strtoull(argv[2], NULL, 10);

    fd = open(argv[1], O_RDWR);
    if (fd < 0) { perror("[peer] open"); return 1; }
    p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (p == MAP_FAILED) { perror("[peer] mmap"); return 1; }

    for (k = 0; k < len; k++)
        if (p[k] != (unsigned char)(k * 7 + 3)) bad++;
    printf("[peer] pattern from the Windows side: %s (%d wrong of %zu)\n",
           bad ? "MISMATCH" : "MATCH", bad, len);

    p[0] = 0xAB;
    printf("[peer] stored 0xAB in byte 0; waiting for 0xCD in byte 1\n");
    for (t = 0; t < 200; t++) {
        if (p[1] == 0xCD) { printf("[peer] SAW the reply after %d ms\n", t * 25); break; }
        usleep(25000);
    }
    if (p[1] != 0xCD) printf("[peer] NEVER saw the reply (byte1=%02x)\n", p[1]);

    munmap(p, len);
    close(fd);
    return 0;
}
