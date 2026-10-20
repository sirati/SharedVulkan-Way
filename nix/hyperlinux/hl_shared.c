/* Embedded Rust backing + HVF maps. Host sockets carry bootstrap and bells only.
 * mmap_lock serializes changes with fork, exec, munmap and mprotect. The private
 * lock protects resolver readers; it is never held while acquiring shm_lock. */
#include "hl_shared.h"
#include "hl_seals.h"
#include <sys/stat.h>
#include "wc_hyperlinux.h"
#include "syscall.h"
#include "syscall_internal.h"
#include "fd_object.h"
#include <Hypervisor/Hypervisor.h>
#include <pthread.h>
#include <errno.h>
#include <stdlib.h>
#include <unistd.h>
#include <fcntl.h>
#include <limits.h>

#define BLOCK (2ULL*1024*1024)
typedef struct owner {
    wc_hl_backing *backing;
    wc_hl_mapping map;
    uint64_t ipa, span;
    int refs, writable;
    int seal_fd;
    uint64_t seal_id;
    dev_t dev; ino_t ino;
    void *prime;
    struct owner *retained_next;
} owner;
typedef struct view {
    uint64_t va, end, offset;
    int prot;
    owner *o;
    struct view *next;
} view;
static view *views;
static owner *retained;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;

static int overlap(uint64_t a, uint64_t b, uint64_t c, uint64_t d) {
    return a < d && c < b;
}
void *hl_wc_resolve(uint64_t va, uint64_t *avail) {
    void *p = NULL;
    pthread_mutex_lock(&lock);
    for (view *v = views; v; v = v->next) {
        if (va >= v->va && va < v->end) {
            *avail = v->end - va;
            p = (char *)v->o->map.base + v->o->map.data_offset + v->offset + va - v->va;
            break;
        }
    }
    pthread_mutex_unlock(&lock);
    return p;
}
int hl_wc_ipa_busy(uint64_t lo, uint64_t span) {
    int busy = 0;
    pthread_mutex_lock(&lock);
    for (view *v = views; v; v = v->next)
        if (overlap(lo, lo + span, v->o->ipa, v->o->ipa + v->o->span)) { busy = 1; break; }
    for (owner *o = retained; o && !busy; o = o->retained_next)
        if (overlap(lo, lo + span, o->ipa, o->ipa + o->span)) busy = 1;
    pthread_mutex_unlock(&lock);
    return busy;
}
static uint64_t find_ipa(guest_t *g, uint64_t span) {
    uint64_t lo = (g->mmap_limit + 0x80000000ULL + BLOCK - 1) & ~(BLOCK - 1);
    uint64_t hi = g->interp_base;
    for (uint64_t p = lo; p <= hi && span <= hi - p; p += BLOCK)
        if (!hl_wc_ipa_busy(p, span) && !hl_shm_ipa_busy(p, span)) return p;
    return 0;
}
static int tracker_room(guest_t *g, uint64_t lo, uint64_t hi, int protect) {
    int needed = 0;
    for (int i = 0; i < g->nregions; i++) {
        guest_region_t *r = &g->regions[i];
        if (!overlap(lo, hi, r->start, r->end)) continue;
        if (protect) {
            if (r->start < lo) needed++;
            if (r->end > hi) needed++;
        } else if (r->start < lo && r->end > hi) needed++;
    }
    return g->nregions <= GUEST_MAX_REGIONS - needed;
}
static void release_owner(guest_t *g, owner *o) {
    if (--o->refs) return;
    /* Remove Stage-2 before dropping the Rust-owned host view. Restore only
     * the private primary reservation, never memset an external allocation. */
    if (hv_vm_unmap(o->ipa, o->map.mapped_len) != HV_SUCCESS) {
        /* A failed Stage-2 removal is not a release acknowledgement. Keep
         * both the allocation and its IPA reservation until VM destruction. */
        o->refs = 1;
        o->retained_next = retained;
        retained = o;
        return;
    }
    hv_vm_map((char *)g->host_base + o->ipa - g->ipa_base,
        o->ipa, o->span, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC);
    hl_wc_seal_untrack(o->seal_fd, o->seal_id);
    if (o->seal_fd >= 0) close(o->seal_fd);
    wc_hl_prime_release(o->prime);
    wc_hl_backing_release(o->backing);
    free(o);
}
/* Split before mutation. Allocation failure leaves the original view intact. */
static int split_at(uint64_t at) {
    for (view *v = views; v; v = v->next) {
        if (at <= v->va || at >= v->end) continue;
        view *right = malloc(sizeof(*right));
        if (!right) return -LINUX_ENOMEM;
        *right = *v;
        right->va = at;
        right->offset += at - v->va;
        right->o->refs++;
        v->end = at;
        right->next = v->next;
        v->next = right;
        break;
    }
    return 0;
}
int hl_wc_unmap(guest_t *g, uint64_t lo, uint64_t len) {
    if (len > UINT64_MAX - lo) return -LINUX_EINVAL;
    uint64_t hi = lo + len;
    pthread_mutex_lock(&lock);
    for (view *v = views; v; v = v->next)
        if (overlap(lo, hi, v->va, v->end) && !tracker_room(g, lo, hi, 0)) {
            pthread_mutex_unlock(&lock); return -LINUX_ENOMEM;
        }
    int rc = split_at(lo);
    if (!rc) rc = split_at(hi);
    if (rc) { pthread_mutex_unlock(&lock); return rc; }
    view **at = &views;
    while (*at) {
        view *v = *at;
        if (!overlap(v->va, v->end, lo, hi)) { at = &v->next; continue; }
        /* Invalidation retains IPA bits for mprotect, but unmapped VMAs are
         * removed from our resolver before any backing is released. */
        if (guest_map_shared_pages(g, v->va, v->end - v->va, g->ipa_base + v->va, 0) < 0) {
            pthread_mutex_unlock(&lock); return -LINUX_ENOMEM;
        }
        guest_region_remove(g, v->va, v->end);
        *at = v->next;
        release_owner(g, v->o);
        free(v);
    }
    pthread_mutex_unlock(&lock);
    g->need_tlbi = 1;
    return 0;
}
int hl_wc_protect(guest_t *g, uint64_t lo, uint64_t len, int prot) {
    uint64_t hi = lo + len;
    if (hi < lo) return -LINUX_EINVAL;
    pthread_mutex_lock(&lock);
    for (view *v = views; v; v = v->next)
        if (overlap(lo, hi, v->va, v->end) &&
            ((prot & ~(LINUX_PROT_READ | LINUX_PROT_WRITE)) ||
            ((prot & LINUX_PROT_WRITE) && !v->o->writable))) {
            pthread_mutex_unlock(&lock); return -LINUX_EACCES;
        }
    for (view *v = views; v; v = v->next)
        if (overlap(lo, hi, v->va, v->end) && !tracker_room(g, lo, hi, 1)) {
            pthread_mutex_unlock(&lock); return -LINUX_ENOMEM;
        }
    int rc = split_at(lo);
    if (!rc) rc = split_at(hi);
    if (!rc) for (view *v = views; v; v = v->next) {
        if (!overlap(lo, hi, v->va, v->end)) continue;
        int perms = MEM_PERM_R | ((prot & LINUX_PROT_WRITE) ? MEM_PERM_W : 0);
        if (guest_map_shared_pages(g, v->va, v->end - v->va,
            v->o->ipa + v->o->map.data_offset + v->offset, prot ? perms : 0) < 0) {
            rc = -LINUX_ENOMEM; break;
        }
        v->prot = prot;
        guest_region_set_prot(g, v->va, v->end, prot);
    }
    pthread_mutex_unlock(&lock);
    return rc;
}

static int64_t map_host(guest_t *g, uint64_t addr, uint64_t len, int prot,
    int flags, int hfd, uint64_t offset, int inherited, void *prime) {
    if (g->is_rosetta) { wc_hl_prime_release(prime); return -LINUX_EOPNOTSUPP; }
    if (prot & ~(LINUX_PROT_READ | LINUX_PROT_WRITE)) { wc_hl_prime_release(prime); return -LINUX_EACCES; }
    owner *o = calloc(1, sizeof(*o));
    view *v = calloc(1, sizeof(*v));
    if (!o || !v) { wc_hl_prime_release(prime); free(o); free(v); return -LINUX_ENOMEM; }
    o->seal_fd = -1;
    o->prime = prime;
    struct stat identity;
    if (fstat(hfd, &identity) != 0) { wc_hl_prime_release(prime); free(o); free(v); return linux_errno(); }
    o->dev = identity.st_dev; o->ino = identity.st_ino;
    int access = fcntl(hfd, F_GETFL);
    if (access < 0) { wc_hl_prime_release(prime); free(o); free(v); return linux_errno(); }
    o->writable = (access & O_ACCMODE) == O_RDWR && inherited != 1;
    if ((prot & LINUX_PROT_WRITE) && !o->writable) {
        wc_hl_prime_release(prime); free(o); free(v); return -LINUX_EACCES;
    }
    hl_wc_seal_guard guard = { .lockfd = -1 };
    if (hl_wc_seal_begin(hfd, HL_WC_SEAL_MAP, inherited ? 0 : !!(prot & LINUX_PROT_WRITE), &guard) < 0) {
        int e = errno; hl_wc_seal_end(&guard); wc_hl_prime_release(o->prime); free(o); free(v); errno = e; return linux_errno();
    }
    if (!inherited && !hl_wc_seal_writable(&guard)) o->writable = 0;
    if (hl_wc_seal_track(&guard, offset, len, o->writable, &o->seal_id) < 0) {
        int e = errno; hl_wc_seal_end(&guard); wc_hl_prime_release(o->prime); free(o); free(v); errno = e; return linux_errno();
    }
    if (o->seal_id) o->seal_fd = fcntl(hfd, F_DUPFD_CLOEXEC, 0);
    hl_wc_seal_end(&guard);
    if (o->seal_id && o->seal_fd < 0) { hl_wc_seal_untrack(hfd, o->seal_id); wc_hl_prime_release(o->prime); free(o); free(v); return linux_errno(); }
    int rc = wc_hl_backing_map(hfd, offset, len, o->writable, &o->backing, &o->map);
    if (rc) { hl_wc_seal_untrack(o->seal_fd, o->seal_id); if (o->seal_fd >= 0) close(o->seal_fd);
        wc_hl_prime_release(o->prime); free(o); free(v); errno = -rc; return linux_errno(); }
    if (o->map.mapped_len > UINT64_MAX - BLOCK + 1) goto no_memory;
    o->span = (o->map.mapped_len + BLOCK - 1) & ~(BLOCK - 1);
    o->ipa = find_ipa(g, o->span);
    if (!o->ipa) goto no_memory;
    int fixed = flags & (LINUX_MAP_FIXED | LINUX_MAP_FIXED_NOREPLACE);
    if (!fixed) addr = hl_wc_find_va(g, len);
    if (addr == UINT64_MAX || !addr || addr > GUEST_USER_VA_MAX || len > GUEST_USER_VA_MAX - addr) goto no_memory;
    if ((flags & LINUX_MAP_FIXED_NOREPLACE) && guest_region_overlaps(g, addr, addr + len)) {
        hl_wc_seal_untrack(o->seal_fd, o->seal_id); if (o->seal_fd >= 0) close(o->seal_fd);
        wc_hl_prime_release(o->prime); wc_hl_backing_release(o->backing); free(o); free(v); return -LINUX_EEXIST;
    }
    /* Reserve Stage-2 first. If this fails the old guest mapping is intact. */
    if (hv_vm_unmap(o->ipa, o->span) != HV_SUCCESS) goto no_memory;
    if (hv_vm_map(o->map.base, o->ipa, o->map.mapped_len,
        HV_MEMORY_READ | (o->writable ? HV_MEMORY_WRITE : 0)) != HV_SUCCESS) {
        hv_vm_map((char *)g->host_base + o->ipa - g->ipa_base,
            o->ipa, o->span, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC);
        goto no_memory;
    }
    o->refs = 1;
    if (fixed && hl_wc_unmap(g, addr, len) < 0) { release_owner(g, o); free(v); return -LINUX_ENOMEM; }
    int perms = MEM_PERM_R | ((prot & LINUX_PROT_WRITE) ? MEM_PERM_W : 0);
    if (guest_map_shared_pages(g, addr, len, o->ipa + o->map.data_offset,
        prot ? perms : 0) < 0) {
        guest_invalidate_ptes(g, addr, addr + len);
        release_owner(g, o); free(v); return -LINUX_ENOMEM;
    }
    guest_region_remove(g, addr, addr + len);
    if (guest_region_add(g, addr, addr + len, prot, flags, offset, "[waycompose-shared]") < 0) {
        guest_invalidate_ptes(g, addr, addr + len);
        release_owner(g, o); free(v); return -LINUX_ENOMEM;
    }
    *v = (view){ .va = addr, .end = addr + len, .prot = prot, .o = o };
    pthread_mutex_lock(&lock);
    v->next = views; views = v;
    pthread_mutex_unlock(&lock);
    g->need_tlbi = 1;
    return (int64_t)addr;
no_memory:
    hl_wc_seal_untrack(o->seal_fd, o->seal_id); if (o->seal_fd >= 0) close(o->seal_fd);
    wc_hl_prime_release(o->prime); wc_hl_backing_release(o->backing); free(o); free(v); return -LINUX_ENOMEM;
}
int64_t hl_wc_mmap(guest_t *g, uint64_t addr, uint64_t len, int prot,
    int flags, int fd, int64_t offset) {
    if (!len || offset < 0 || (offset & 4095) || len > UINT64_MAX - 4095) return -LINUX_EINVAL;
    len = (len + 4095) & ~4095ULL;
    if ((flags & (LINUX_MAP_FIXED | LINUX_MAP_FIXED_NOREPLACE)) && (addr & 4095)) return -LINUX_EINVAL;
    /* Legacy host-backed entries may not yet have hl_descriptor_t. Duplicate
     * under the existing table lock so close/dup2 cannot replace the host fd
     * between lookup and retention. No Rust/HVF operation holds fd_lock. */
    pthread_mutex_lock(&fd_lock);
    int hfd = -1;
    void *prime = NULL;
    if (fd >= 0 && fd < FD_TABLE_SIZE && fd_table[fd].type != FD_CLOSED)
        { hfd = fcntl(fd_table[fd].host_fd, F_DUPFD_CLOEXEC, 0);
          if (fd_table[fd].of && fd_table[fd].of->state && hl_wc_is_memfd(fd_table[fd].of))
              prime = wc_hl_prime_clone(fd_table[fd].of->state); }
    else errno = EBADF;
    pthread_mutex_unlock(&fd_lock);
    if (hfd < 0) { wc_hl_prime_release(prime); return linux_errno(); }
    int64_t result = map_host(g, addr, len, prot, flags, hfd, (uint64_t)offset, 0, prime);
    close(hfd);
    return result;
}
void hl_wc_reset(guest_t *g) {
    while (views) {
        uint64_t va = views->va, len = views->end - va;
        if (hl_wc_unmap(g, va, len) < 0) abort();
    }
}
int hl_wc_fork_count(void) {
    int n = 0;
    for (view *v = views; v; v = v->next) { if (n == INT_MAX) return -1; n++; }
    return n;
}
int hl_wc_fork_export(int index, hl_wc_fork_record *out, int *fd) {
    view *v = views;
    while (v && index-- > 0) v = v->next;
    if (!v) return -1;
    *out = (hl_wc_fork_record){ .va = v->va, .len = v->end - v->va,
        .offset = v->o->map.file_offset + v->offset, .prot = v->prot, .writable = v->o->writable };
    return wc_hl_backing_export_fd(v->o->backing, fd);
}
int hl_wc_fork_import(guest_t *g, const hl_wc_fork_record *in, int fd) {
    void *prime = NULL;
    uint8_t token[448];
    int n = hl_wc_token_snapshot(fd, token, sizeof(token));
    if (n > 0 && wc_hl_prime_register(fd, token, (uint64_t)n, &prime) < 0) return -1;
    int64_t rc = map_host(g, in->va, in->len, in->prot, LINUX_MAP_FIXED | LINUX_MAP_SHARED, fd, in->offset,
        in->writable ? 2 : 1, prime);
    return rc == (int64_t)in->va ? 0 : -1;
}

/* PulseAudio's upstream POSIX SHM names are not filesystem files on Darwin.
 * A strict name adapter makes Linux shm_open/shm_unlink use the same kernel
 * objects as native libpulse. Native permissions and ownership remain decisive. */
#include <sys/mman.h>
#include <sys/stat.h>
#include <string.h>
static const char *pulse_name(const char *path) {
    const char *prefix = "/dev/shm/pulse-shm-";
    size_t n = strlen(prefix);
    if (strncmp(path, prefix, n) != 0 || !path[n]) return NULL;
    for (const char *p = path + n; *p; p++) if (*p < '0' || *p > '9') return NULL;
    return path + 8; /* /pulse-shm-N */
}
int64_t hl_wc_shm_open(const char *path, int flags, int mode) {
    const char *name = pulse_name(path);
    if (!name) return INT64_MIN;
    int hfd = shm_open(name, translate_open_flags(flags), (mode_t)mode);
    if (hfd < 0) return linux_errno();
    struct stat st;
    if (fstat(hfd, &st) != 0 || st.st_uid != geteuid()) {
        close(hfd); return -LINUX_EACCES;
    }
    hl_open_file_t *of = hl_open_file_create(FD_REGULAR, &hl_fd_ops_host_file, (uint32_t)flags, NULL);
    if (!of) { close(hfd); return -LINUX_ENOMEM; }
    int fd = hl_fd_install(of, hfd, flags & LINUX_O_CLOEXEC);
    return fd < 0 ? -LINUX_EMFILE : fd;
}
int64_t hl_wc_shm_unlink(const char *path, int flags) {
    const char *name = pulse_name(path);
    if (!name) return INT64_MIN;
    if (flags) return -LINUX_EINVAL;
    int fd = shm_open(name, O_RDONLY, 0);
    if (fd < 0) return linux_errno();
    struct stat st;
    int rc = fstat(fd, &st);
    close(fd);
    if (rc != 0) return linux_errno();
    if (st.st_uid != geteuid()) return -LINUX_EACCES;
    return shm_unlink(name) == 0 ? 0 : linux_errno();
}

int64_t hl_wc_msync(uint64_t addr, uint64_t len, int flags) {
    if ((addr & 4095) || len > UINT64_MAX - addr || (flags & ~7) || (flags & 5) == 5)
        return -LINUX_EINVAL;
    int native = (flags & 4) ? MS_SYNC : MS_ASYNC;
    if (flags & 2) native |= MS_INVALIDATE;
    /* Caller holds mmap_lock. The list cannot change while msync operates;
     * the resolver's separate mutex is not held during blocking filesystem IO. */
    for (view *v = views; v; v = v->next) {
        if (!overlap(addr, addr + len, v->va, v->end)) continue;
        if (msync(v->o->map.base, v->o->map.mapped_len, native) < 0) return linux_errno();
    }
    return 0;
}

void hl_wc_vm_destroyed(void) {
    /* Only called after successful hv_vm_destroy, with no active workers. */
    while (retained) {
        owner *o = retained;
        retained = o->retained_next;
        hl_wc_seal_untrack(o->seal_fd, o->seal_id);
        if (o->seal_fd >= 0) close(o->seal_fd);
        wc_hl_prime_release(o->prime);
        wc_hl_backing_release(o->backing);
        free(o);
    }
}

void hl_wc_prime_attach(int fd, const void *prime) {
    struct stat st;
    if (fstat(fd, &st) != 0) return;
    pthread_mutex_lock(&mmap_lock);
    pthread_mutex_lock(&lock);
    for (view *v = views; v; v = v->next) {
        owner *o = v->o;
        if (!o->prime && o->dev == st.st_dev && o->ino == st.st_ino)
            o->prime = wc_hl_prime_clone(prime);
    }
    pthread_mutex_unlock(&lock);
    pthread_mutex_unlock(&mmap_lock);
}
