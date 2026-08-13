/* probe-filemap.c -- can a Windows process open a *path* (not a kernel object
 * name) and get a mapping that is coherent with a Linux process's mmap of the
 * same file?
 *
 *   probe-filemap.exe <path> <len>            create+fill, then wait for a reply
 *   probe-filemap.exe --open <path> <len>     open, verify, report
 *
 * WHY THIS EXISTS
 *
 * docs/design.md §2.2 chose *named kernel objects* because the peer can open
 * one by name and no handle has to be duplicated. That works between two
 * Windows processes. It does not work between a Linux process and a Windows
 * one: `Local\wc-...` is a name only the NT object manager understands, and a
 * Linux process can neither create it nor open it.
 *
 * Stage 4 puts a Linux proxy on one side and the Windows compositor on the
 * other, and the rule (docs/goal-firefox.md, and the stage brief) is absolute:
 * **no descriptor or handle crosses, ever — only a name the peer opens
 * itself.** So the question is what name both namespaces share. A filesystem
 * path is the candidate: under Wine that is the `Z:` drive, and on the real
 * guest it is the virtio-fs mount.
 *
 * That candidate has two separate failure modes and this probe separates them:
 *
 *  1. Can the Windows side get a section from a *file handle* at all
 *     (`CreateFileMappingA(hFile, ...)` rather than `OpenFileMappingW(name)`)?
 *  2. Is the resulting view **coherent** — does a store one side makes through
 *     its own mapping become visible to the other with no flush and no reopen?
 *
 * (2) is the one that cannot be assumed. A `MAP_SHARED` mmap on Linux and a
 * page-cache-backed mapping in a guest are coherent for a *local* filesystem
 * and are emphatically not guaranteed to be for a network or paravirtual one:
 * virtio-fs without DAX caches file pages in the guest, so the two sides would
 * each see their own copy and every frame would be stale or half-written. That
 * is a correctness failure that looks like a rendering bug, so it is measured
 * here rather than discovered later.
 *
 * The handshake is deliberately two-way. A one-way check passes if the reader
 * merely happens to fault the page in after the writer's data reached the
 * filesystem, which is a much weaker property than shared-page coherency.
 */
#include <windows.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

int main(int argc, char **argv)
{
    const char *path;
    unsigned long long len;
    int open_only = 0, i = 1;
    HANDLE hf, hm;
    unsigned char *p;
    unsigned long long k;

    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc > 1 && !strcmp(argv[1], "--open")) { open_only = 1; i = 2; }
    if (argc < i + 2) {
        printf("usage: probe-filemap.exe [--open] <path> <len>\n");
        return 2;
    }
    path = argv[i];
    len = strtoull(argv[i + 1], NULL, 10);

    hf = CreateFileA(path, GENERIC_READ | GENERIC_WRITE,
                     FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                     NULL, open_only ? OPEN_EXISTING : OPEN_ALWAYS,
                     FILE_ATTRIBUTE_NORMAL, NULL);
    if (hf == INVALID_HANDLE_VALUE) {
        printf("[fm] CreateFileA FAILED err=%lu\n", GetLastError());
        return 1;
    }
    printf("[fm] CreateFileA ok\n");

    hm = CreateFileMappingA(hf, NULL, PAGE_READWRITE,
                            (DWORD)(len >> 32), (DWORD)(len & 0xffffffffu), NULL);
    if (!hm) {
        printf("[fm] CreateFileMappingA FAILED err=%lu\n", GetLastError());
        return 1;
    }
    printf("[fm] CreateFileMappingA ok (unnamed section, from a file handle)\n");

    p = (unsigned char *)MapViewOfFile(hm, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0,
                                       (SIZE_T)len);
    if (!p) {
        printf("[fm] MapViewOfFile FAILED err=%lu\n", GetLastError());
        return 1;
    }
    printf("[fm] MapViewOfFile ok len=%llu\n", len);

    if (!open_only) {
        for (k = 0; k < len; k++) p[k] = (unsigned char)(k * 7 + 3);
        printf("[fm] wrote pattern; waiting for the peer to put 0xAB in byte 0\n");
        for (i = 0; i < 200; i++) {
            if (p[0] == 0xAB) {
                printf("[fm] SAW the peer's write after %d ms\n", i * 25);
                break;
            }
            Sleep(25);
        }
        if (p[0] != 0xAB)
            printf("[fm] NEVER saw the peer's write (byte0=%02x)\n", p[0]);
        p[1] = 0xCD;
        printf("[fm] wrote 0xCD to byte 1\n");
        Sleep(600);   /* stay mapped long enough for the peer to observe it */
    } else {
        int bad = 0;
        for (k = 0; k < len; k++)
            if (p[k] != (unsigned char)(k * 7 + 3)) bad++;
        printf("[fm] %s (%d bytes wrong of %llu)\n",
               bad ? "MISMATCH" : "MATCH", bad, len);
    }

    UnmapViewOfFile(p);
    CloseHandle(hm);
    CloseHandle(hf);
    return 0;
}
