/* Native regression stubs for HVF and guest page tables. The actual resolver,
 * split, protection and retirement code is compiled from hl_shared.c. */
#ifndef HL_SHARED_TEST_PLATFORM_H
#define HL_SHARED_TEST_PLATFORM_H
#include <stdint.h>
#include <stdlib.h>
#include <pthread.h>
#include <errno.h>
#include <limits.h>
#include <unistd.h>
#include <sys/mman.h>
#include <assert.h>
#define LINUX_PROT_READ 1
#define LINUX_PROT_WRITE 2
#define LINUX_ENOMEM ENOMEM
#define LINUX_EINVAL EINVAL
#define LINUX_EACCES EACCES
#define MEM_PERM_R 1
#define MEM_PERM_W 2
#define HV_SUCCESS 0
#define HV_MEMORY_READ 1
#define HV_MEMORY_WRITE 2
#define HV_MEMORY_EXEC 4
#define GUEST_MAX_REGIONS 64
typedef struct { uint64_t start,end; } guest_region_t;
typedef struct { uint64_t mmap_limit,interp_base,ipa_base; void *host_base; int nregions,need_tlbi; guest_region_t regions[64]; } guest_t;
typedef struct { void *base; uint64_t mapped_len,data_offset,len,file_offset; } wc_hl_mapping;
typedef struct { void *base; size_t len; } wc_hl_backing;
typedef struct hl_wc_access_scope { struct hl_wc_access_scope *previous; void *pins; } hl_wc_access_scope;
static unsigned released, unmapped;
static int hv_vm_unmap(uint64_t p, uint64_t len) { (void)p; (void)len; unmapped++; return HV_SUCCESS; }
static int hv_vm_map(void *b,uint64_t p,uint64_t n,int f) { (void)b;(void)p;(void)n;(void)f;return HV_SUCCESS; }
static void hl_wc_seal_untrack(int fd,uint64_t id) { (void)fd;(void)id; }
static void wc_hl_prime_release(void *p) { (void)p; }
static void wc_hl_backing_release(wc_hl_backing *p) { assert(!munmap(p->base,p->len)); free(p);released++; }
static int hl_shm_ipa_busy(uint64_t p,uint64_t n) { (void)p;(void)n;return 0; }
static int guest_map_shared_pages(guest_t *g,uint64_t a,uint64_t n,uint64_t p,int f) { (void)g;(void)a;(void)n;(void)p;(void)f;return 0; }
static void guest_region_remove(guest_t *g,uint64_t a,uint64_t b) { (void)g;(void)a;(void)b; }
static void guest_region_set_prot(guest_t *g,uint64_t a,uint64_t b,int p) { (void)g;(void)a;(void)b;(void)p; }
#endif
