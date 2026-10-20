#ifndef WC_MINGW_MMAN_H
#define WC_MINGW_MMAN_H
#include <stddef.h>
#include <sys/types.h>
#define PROT_READ 1
#define PROT_WRITE 2
#define MAP_SHARED 1
#define MAP_PRIVATE 2
#define MAP_FAILED ((void *)-1)
void *wc_native_mmap(void *, size_t, int, int, int, off_t);
int wc_native_munmap(void *, size_t);
#define mmap wc_native_mmap
#define munmap wc_native_munmap
#endif
