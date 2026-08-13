/* wc-poolshim — does an unmodified client's `wl_shm` pool have to be the
 * client's own memory?
 *
 * EXPERIMENT ONLY. This is the standalone half of waycompose-design.md §30: it
 * answers "can host-side code choose where an unmodified Firefox's pool pages
 * come from" without touching the arena, the proxy, or the region lifecycle.
 * It backs pools with a plain file so that the question under test is only the
 * interposition, never the allocator.
 *
 * THE RULE. Everything here is host-side. The client, this shim and `wc-proxy`
 * are three host processes; the descriptor this shim substitutes is created on
 * the host, travels one `SCM_RIGHTS` hop over the local Wayland socket that
 * `wl_shm` already uses, and is consumed on the host. It never approaches the
 * VM boundary, so it is not the kind of descriptor design.md §1a forbids.
 *
 * WHY THIS SYMBOL. `wl_shm_create_pool` is a `static inline` in the generated
 * protocol header, so it cannot be interposed. It calls the exported varargs
 * `wl_proxy_marshal_flags`, which cannot be forwarded portably. But that
 * function reaches `wl_proxy_marshal_array_flags` *through the PLT* (verified
 * by objdump against wayland 1.24), and that one is not varargs: its `args`
 * array is a mutable `union wl_argument *`. So one interposed symbol sees every
 * request of every client, and can rewrite the fd in place.
 *
 * HOW TO RUN IT.
 *
 *   gcc -shared -fPIC -O2 -o wc-poolshim.so scripts/wc-poolshim.c -ldl
 *   weston --backend=headless --fake-seat --socket=wc-shim &
 *   WAYLAND_DISPLAY=wc-shim LD_PRELOAD=./wc-poolshim.so WC_POOLSHIM=1 firefox
 *
 * `--fake-seat` is not optional: the headless backend has no seat without it,
 * GTK's `gdk_seat_get_keyboard` asserts, and Firefox falls back to headless
 * mode where it creates no `wl_shm` pool at all — a rig that measures nothing
 * while looking like it works.
 *
 * `WC_POOLSHIM=0` observes without redirecting, which is the control.
 * `WC_POOLSHIM_VERBOSE=2` names every request the client marshals.
 * Building with `-DWC_MARSHAL_ONLY` drops the `mmap` family and keeps only the
 * protocol interposition; that is the bisect that separated "the shim crashes
 * Firefox" from "the redirection crashes Firefox", and it earned its keep.
 */

#define _GNU_SOURCE
#include <dirent.h>
#include <dlfcn.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <signal.h>
#include <sys/syscall.h>
#include <sys/stat.h>
#include <unistd.h>

/* Stable libwayland ABI, declared here so the shim needs no wayland headers. */
struct wl_message {
    const char *name;
    const char *signature;
    const struct wl_interface **types;
};
struct wl_interface {
    const char *name;
    int version;
    int method_count;
    const struct wl_message *methods;
    int event_count;
    const struct wl_message *events;
};
union wl_argument {
    int32_t i;
    uint32_t u;
    int32_t f;
    const char *s;
    void *o;
    uint32_t n;
    void *a;
    int32_t h;
};
struct wl_proxy;

static void *(*real_mmap)(void *, size_t, int, int, int, off_t);
static void *(*real_mmap64)(void *, size_t, int, int, int, off_t);
static int (*real_munmap)(void *, size_t);
static int (*real_ftruncate)(int, off_t);
static int (*real_close)(int);
static int (*real_posix_fallocate)(int, off_t, off_t);
static void *(*real_mremap)(void *, size_t, size_t, int, ...);
static struct wl_proxy *(*real_marshal_array)(struct wl_proxy *, uint32_t,
                                              const struct wl_interface *,
                                              uint32_t, uint32_t,
                                              union wl_argument *);
static const struct wl_interface *(*real_get_interface)(struct wl_proxy *);

static int shim_on;      /* WC_POOLSHIM=1 arms the redirection */
static int shim_verbose;
static int shim_diag;    /* WC_POOLSHIM_DIAG=1 answers §30.10, observing only */
static const char *backing_dir;

/* Counters the harness asserts on. Printed at exit. */
static unsigned long n_create_pool, n_redirected, n_resize, n_no_mapping, n_commit, n_retired;
static unsigned long long bytes_redirected;

#define MAXMAP 65536
struct maprec {
    int fd;
    void *addr;
    size_t len;
    int used;
};
static struct maprec maps[MAXMAP];

#define MAXPOOL 64
struct poolrec {
    int client_fd; /* fd the client passed to create_pool */
    int our_fd;    /* fd we substituted */
    void *addr;    /* client's VA, now backed by our_fd */
    size_t len;
    int used;
    /* An independent mapping of our_fd, held only by the shim. This is the
     * instrument: if the client's drawing shows up here, the client is writing
     * into the very object the compositor was handed, and there is nothing left
     * to copy. If a resize ever silently put the client back on its own memfd,
     * this mapping stops changing and says so. */
    unsigned char *witness;
    unsigned long long last_sum;
    unsigned long checks, changes;
    int retired; /* fd number given back, stats kept for the report */
    /* WHAT THE POOL ACTUALLY IS. §30.10 measured that Firefox maps its pool
     * through a *different descriptor* than the one it passes to
     * `create_pool` — the object is `mozilla-ipc` shared memory, whose fd is
     * duplicated and passed around, so the number the shim watched was never
     * the number the mapping was made from. Identity has to be the object, and
     * `(st_dev, st_ino)` is what names an object. */
    dev_t obj_dev;
    ino_t obj_ino;
};
static struct poolrec pools[MAXPOOL];

static struct poolrec *pool_by_client_fd(int fd) {
    for (int i = 0; i < MAXPOOL; i++)
        if (pools[i].used && !pools[i].retired && pools[i].client_fd == fd)
            return &pools[i];
    return NULL;
}

static struct poolrec *pool_by_addr(void *a) {
    for (int i = 0; i < MAXPOOL; i++)
        if (pools[i].used && pools[i].addr == a) return &pools[i];
    return NULL;
}

/* §30.10: WHICH OF THE TWO CAUSES IS IT.
 *
 * §30.6 measured that Firefox paints through `wl_shm` and not into the objects
 * this shim substitutes, and left two candidates it could not separate:
 *
 *   (a) the drawing does not happen in the process that owns the Wayland
 *       connection, so the memory was allocated and written somewhere this
 *       shim's `create_pool` hook can never reach; or
 *   (b) the client holds more than one mapping of the same object, and the
 *       shim replaced one while the writes go through another.
 *
 * Both are questions about who maps the pool object, and the kernel already
 * knows the answer. Take the object's inode from the descriptor, then walk
 * every `/proc/<pid>/maps` we can read and count the mappings of that inode.
 *
 *   two or more mappings in this process   -> (b)
 *   mappings only in some *other* process  -> (a)
 *   no mapping anywhere at create_pool     -> the pool is mapped later, by
 *                                             whoever paints, and the pid of
 *                                             that mapping decides it
 *
 * Observation only: it runs with the redirection disarmed, so what it measures
 * is what Firefox does rather than what the shim did to it. */
/* Inodes seen at `create_pool`, so the same question can be asked again once
 * frames are actually flowing. Asking only at `create_pool` is what left §30.6
 * ambiguous: a pool with no mapping anywhere at that instant has not told you
 * whether it will be mapped here later or somewhere else. */
#define MAXINO 32
static unsigned long long diag_inos[MAXINO];
static int diag_sizes[MAXINO];
static int n_diag_ino;

static int scan_maps_for_ino(unsigned long long want, const char *tag) {
    DIR *proc = opendir("/proc");
    if (!proc) return 0;
    struct dirent *ent;
    int total = 0;
    while ((ent = readdir(proc))) {
        if (ent->d_name[0] < '0' || ent->d_name[0] > '9') continue;
        char path[256];
        snprintf(path, sizeof path, "/proc/%s/maps", ent->d_name);
        FILE *f = fopen(path, "r");
        if (!f) continue;
        char line[512];
        while (fgets(line, sizeof line, f)) {
            unsigned long long a1, a2, off, ino;
            char perms[8], dev[16];
            int n = 0;
            if (sscanf(line, "%llx-%llx %7s %llx %15s %llu%n", &a1, &a2, perms,
                       &off, dev, &ino, &n) == 6 &&
                ino == want) {
                const char *name = line + n;
                while (*name == ' ') name++;
                char *nl = strchr((char *)name, '\n');
                if (nl) *nl = 0;
                /* Name the process rather than leaving the pid to be guessed:
                 * "the other mapper must be the compositor" is exactly the kind
                 * of inference this section exists to avoid. */
                char cpath[256], comm[128] = "?";
                snprintf(cpath, sizeof cpath, "/proc/%s/comm", ent->d_name);
                FILE *cf = fopen(cpath, "r");
                if (cf) {
                    if (fgets(comm, sizeof comm, cf)) {
                        char *c = strchr(comm, '\n');
                        if (c) *c = 0;
                    }
                    fclose(cf);
                }
                fprintf(stderr,
                        "wc-poolshim: DIAG %s ino=%llu mapped by pid=%s (%s)%s "
                        "len=%llu perms=%s %s\n",
                        tag, want, ent->d_name, comm,
                        atoi(ent->d_name) == (int)getpid() ? " [SELF]" : "",
                        a2 - a1, perms, name);
                total++;
            }
        }
        fclose(f);
    }
    closedir(proc);
    return total;
}

/* Called once frames are flowing. This is the measurement that separates (a)
 * from (b): if the mapping now exists in a *different* pid, the painting is not
 * done by the process that owns the Wayland connection. */
static void diagnose_again(const char *tag) {
    for (int i = 0; i < n_diag_ino; i++) {
        int t = scan_maps_for_ino(diag_inos[i], tag);
        fprintf(stderr,
                "wc-poolshim: DIAG %s ino=%llu size=%d total mappings: %d\n",
                tag, diag_inos[i], diag_sizes[i], t);
    }
}

static void diagnose_pool(int client_fd, int size) {
    struct stat st;
    if (fstat(client_fd, &st) != 0) return;
    fprintf(stderr,
            "wc-poolshim: DIAG create_pool pid=%d fd=%d size=%d ino=%llu\n",
            (int)getpid(), client_fd, size, (unsigned long long)st.st_ino);
    if (n_diag_ino < MAXINO) {
        diag_inos[n_diag_ino] = (unsigned long long)st.st_ino;
        diag_sizes[n_diag_ino] = size;
        n_diag_ino++;
    }

    DIR *proc = opendir("/proc");
    if (!proc) return;
    struct dirent *ent;
    int total = 0;
    while ((ent = readdir(proc))) {
        if (ent->d_name[0] < '0' || ent->d_name[0] > '9') continue;
        char path[256];
        snprintf(path, sizeof path, "/proc/%s/maps", ent->d_name);
        FILE *f = fopen(path, "r");
        if (!f) continue;
        char line[512];
        while (fgets(line, sizeof line, f)) {
            unsigned long long a1, a2, off, ino;
            char perms[8], dev[16];
            int n = 0;
            if (sscanf(line, "%llx-%llx %7s %llx %15s %llu%n", &a1, &a2, perms,
                       &off, dev, &ino, &n) == 6 &&
                ino == (unsigned long long)st.st_ino) {
                const char *name = line + n;
                while (*name == ' ') name++;
                char *nl = strchr((char *)name, '\n');
                if (nl) *nl = 0;
                fprintf(stderr,
                        "wc-poolshim: DIAG   mapped by pid=%s len=%llu perms=%s "
                        "off=%llu %s\n",
                        ent->d_name, a2 - a1, perms, off, name);
                total++;
            }
        }
        fclose(f);
    }
    closedir(proc);
    fprintf(stderr, "wc-poolshim: DIAG   total mappings of ino=%llu: %d\n",
            (unsigned long long)st.st_ino, total);
}

/* FNV-1a over a sample of the pool. Sampled rather than complete because this
 * runs per commit and is an instrument, not the product. */
static unsigned long long witness_sum(const unsigned char *p, size_t len) {
    unsigned long long h = 1469598103934665603ULL;
    size_t step = len / 4096 ? len / 4096 : 1;
    for (size_t i = 0; i < len; i += step) {
        h ^= p[i];
        h *= 1099511628211ULL;
    }
    return h;
}

static void log_line(const char *fmt, ...) {
    if (!shim_verbose) return;
    va_list ap;
    va_start(ap, fmt);
    fprintf(stderr, "wc-poolshim: ");
    vfprintf(stderr, fmt, ap);
    fprintf(stderr, "\n");
    va_end(ap);
}

static void init(void) {
    static int done;
    if (done) return;
    done = 1;
    real_mmap = dlsym(RTLD_NEXT, "mmap");
    real_mmap64 = dlsym(RTLD_NEXT, "mmap64");
    real_munmap = dlsym(RTLD_NEXT, "munmap");
    real_ftruncate = dlsym(RTLD_NEXT, "ftruncate");
    real_close = dlsym(RTLD_NEXT, "close");
    real_posix_fallocate = dlsym(RTLD_NEXT, "posix_fallocate");
    real_mremap = dlsym(RTLD_NEXT, "mremap");
    real_marshal_array = dlsym(RTLD_NEXT, "wl_proxy_marshal_array_flags");
    real_get_interface = dlsym(RTLD_NEXT, "wl_proxy_get_interface");
    const char *on = getenv("WC_POOLSHIM");
    const char *vb = getenv("WC_POOLSHIM_VERBOSE");
    shim_on = on ? atoi(on) : 0;
    shim_verbose = vb ? atoi(vb) : 0;
    const char *dg = getenv("WC_POOLSHIM_DIAG");
    shim_diag = dg ? atoi(dg) : 0;
    backing_dir = getenv("WC_POOLSHIM_DIR");
    if (!backing_dir) backing_dir = "/tmp";
}

static void remember_map(int fd, void *addr, size_t len) {
    for (int i = 0; i < MAXMAP; i++)
        if (!maps[i].used) {
            maps[i].used = 1;
            maps[i].fd = fd;
            maps[i].addr = addr;
            maps[i].len = len;
            return;
        }
}

static struct maprec *find_map(int fd) {
    for (int i = 0; i < MAXMAP; i++)
        if (maps[i].used && maps[i].fd == fd) return &maps[i];
    return NULL;
}

/* THE STEP THAT IS EASY TO MISS, AND SILENT WHEN MISSED. A client does not
 * grow a pool with `mremap`. libwayland's toytoolkit — and Firefox — grow it by
 * `munmap` followed by a fresh `mmap` of *the client's own fd*, the one this
 * shim replaced at `create_pool` and has no further say over. Redirecting once
 * is therefore not enough: after the first resize the client is quietly writing
 * into its own memfd again while the compositor still reads the substituted
 * object, and the picture goes stale without a single error anywhere. So the
 * substitution has to be reasserted on every `mmap` of a pool fd. */
/* Match a descriptor to a pool by the *object* it refers to, not by its number.
 * This is the §30.10 fix, and it happens to retire hazard §30.6d as well: a
 * recycled descriptor number now names a different object and simply does not
 * match, so there is nothing left for the `close` handler to protect against. */
static struct poolrec *pool_by_object(int fd) {
    struct stat st;
    if (fstat(fd, &st) != 0) return NULL;
    for (int i = 0; i < MAXPOOL; i++)
        if (pools[i].used && !pools[i].retired && pools[i].obj_ino == st.st_ino &&
            pools[i].obj_dev == st.st_dev)
            return &pools[i];
    return NULL;
}

/* NO DESCRIPTOR-NUMBER FALLBACK. Keeping `pool_by_client_fd` as a second chance
 * here re-arms §30.6d in its worst form: with retirement now gone, a recycled
 * number matches a live pool forever and re-points unrelated objects. Measured
 * — that combination killed Firefox in one run. The object is the identity or
 * there is no identity. */
static int substitute_fd(int fd) {
    if (!shim_on) return fd;
    struct poolrec *p = pool_by_object(fd);
    return p ? p->our_fd : fd;
}

/* WHY `close` HAS TO BE WATCHED, AND WHAT IT COSTS TO SKIP IT.
 *
 * A pool is remembered by its descriptor *number*, and descriptor numbers are
 * recycled the moment they are closed. So a pool that has been torn down leaves
 * a record matching fd 21, and the next unrelated object the client opens
 * becomes fd 21 and is silently re-pointed at the dead pool's pages. In Firefox
 * that lands on its IPC shared memory and the content process dies with
 * `[GFX1-]: Replay failure: Magic` — measured, not imagined, and it is exactly
 * the class of failure this project calls its most expensive: nothing reports
 * an error at the point where the wrong thing happened.
 *
 * Retiring the record on `close` is necessary. It is not obviously sufficient,
 * and §30 says so: the client may `dup` the fd, in which case the number the
 * shim watches is not the last reference.
 *
 * WHAT RETIREMENT MUST NOT TAKE WITH IT. Retirement is about the descriptor
 * *number* — it stops a recycled number matching a dead pool. It is not about
 * the object. Dropping the witness here as well is what blinded the first
 * measurement: Firefox closes the pool fd immediately after `create_pool`,
 * because the compositor now holds its own reference, so the instrument stopped
 * looking at the exact moment the frames started and every pool but one
 * reported `commits=0`. The witness mapping is an independent `mmap`, and an
 * `mmap` holds its own reference to the file, so it stays valid after the
 * descriptor is gone. Keep it, and keep checking it. */
int close(int fd) {
    if (!real_close) {
        init();
        if (!real_close) return -1;
    }
    /* Once pools are keyed on the object (§30.10), closing the client's
     * descriptor is not the end of the pool: Firefox closes the number it
     * passed to `create_pool` and then maps the same object through another
     * one. Retiring here would throw away the record just before it is needed,
     * which is the §30.6 failure with a different cause. The recycling hazard
     * that made retirement necessary is gone for the same reason — a reused
     * number names a different object and no longer matches. Keep our
     * descriptor open; the pool ends at `wl_shm_pool.destroy`. */
    /* Nothing to do for pools here any more, and that is the point: the pool is
     * named by its object, so a descriptor going away neither ends it nor
     * endangers anything. */
    for (int i = 0; i < MAXMAP; i++)
        if (maps[i].used && maps[i].fd == fd) maps[i].used = 0;
    return real_close(fd);
}

#ifndef WC_MARSHAL_ONLY
/* WHY `mmap` NEEDS A FLOOR UNDER IT. `dlsym` allocates, the allocator maps, and
 * the map lands back here before `dlsym` has returned the pointer this function
 * is about to call. Firefox reaches that window during startup and a shim
 * without this guard segfaults before it has intercepted anything; the weston
 * clients never do, so the bug looks like "Firefox is special" when it is only
 * "Firefox allocates sooner". The raw syscall is the floor. */
static void *mmap_floor(void *addr, size_t len, int prot, int flags, int fd,
                        off_t off) {
    return (void *)syscall(SYS_mmap, addr, len, prot, flags, fd, off);
}

void *mmap(void *addr, size_t len, int prot, int flags, int fd, off_t off) {
    if (!real_mmap) {
        static int resolving;
        if (resolving) return mmap_floor(addr, len, prot, flags, fd, off);
        resolving = 1;
        init();
        resolving = 0;
        if (!real_mmap) return mmap_floor(addr, len, prot, flags, fd, off);
    }
    init();
    int use = substitute_fd(fd);
    if (use != fd) {
        log_line("mmap of pool fd=%d re-pointed at our fd=%d (len=%zu)", fd, use,
                 len);
        struct poolrec *p = pool_by_object(fd);
        void *r2 = real_mmap(addr, len, prot, flags, use, off);
        if (r2 != MAP_FAILED && p) {
            if (p->witness) real_munmap(p->witness, p->len);
            p->addr = r2;
            p->len = len;
            void *w = real_mmap(NULL, len, PROT_READ, MAP_SHARED, use, 0);
            p->witness = (w == MAP_FAILED) ? NULL : (unsigned char *)w;
            n_resize++;
            remember_map(fd, r2, len);
        }
        return r2;
    }
    void *r = real_mmap(addr, len, prot, flags, fd, off);
    if (r != MAP_FAILED && fd >= 0 && (flags & MAP_SHARED) && off == 0)
        remember_map(fd, r, len);
    return r;
}

void *mmap64(void *addr, size_t len, int prot, int flags, int fd, off_t off) {
    init();
    if (!real_mmap64 && !real_mmap) { init(); }
    void *r = real_mmap64 ? real_mmap64(addr, len, prot, flags, fd, off)
                          : real_mmap(addr, len, prot, flags, fd, off);
    if (r != MAP_FAILED && fd >= 0 && (flags & MAP_SHARED) && off == 0)
        remember_map(fd, r, len);
    return r;
}

/* THE RESIZE HAZARD. libwayland's client-side pool grows by
 * `ftruncate(fd, bigger)` then `mremap`. Both name the client's own object. If
 * only the client's object grows, the compositor is left holding a short one
 * and the next attach reads past its end; if only the mapping moves, the client
 * quietly returns to writing into its own memfd and the pixels the compositor
 * shows go stale. Neither failure announces itself, which is why both calls are
 * interposed rather than one. */
int ftruncate(int fd, off_t len) {
    init();
    struct poolrec *p = shim_on ? pool_by_object(fd) : NULL;
    if (p && shim_on) {
        if (real_ftruncate(p->our_fd, len) != 0) return -1;
        log_line("ftruncate: grew our fd=%d to %lld alongside client fd=%d",
                 p->our_fd, (long long)len, fd);
    }
    return real_ftruncate(fd, len);
}

/* On x86-64 a client compiled with `_FILE_OFFSET_BITS=64` — which is every
 * client — calls `ftruncate64`, so interposing `ftruncate` alone catches
 * nothing. Measured the hard way: the first build of this shim saw no resize at
 * all. */
int ftruncate64(int fd, off_t len) { return ftruncate(fd, len); }

/* And libwayland's `os_resize_anonymous_file` prefers `posix_fallocate` over
 * `ftruncate`, because it wants the failure up front rather than a SIGBUS
 * later. Measured: 9 resizes went past an `ftruncate`-only shim untouched. */
int posix_fallocate(int fd, off_t off, off_t len) {
    init();
    struct poolrec *p = shim_on ? pool_by_object(fd) : NULL;
    if (p && shim_on) {
        int r = real_posix_fallocate ? real_posix_fallocate(p->our_fd, off, len)
                                     : real_ftruncate(p->our_fd, off + len);
        if (r != 0) return r;
        log_line("posix_fallocate: grew our fd=%d to %lld", p->our_fd,
                 (long long)(off + len));
    }
    return real_posix_fallocate ? real_posix_fallocate(fd, off, len)
                                : real_ftruncate(fd, off + len);
}

int posix_fallocate64(int fd, off_t off, off_t len) {
    return posix_fallocate(fd, off, len);
}

void *mremap(void *old_addr, size_t old_len, size_t new_len, int flags, ...) {
    init();
    struct poolrec *p = pool_by_addr(old_addr);
    if (p && shim_on) {
        /* Keep the client on our object across the growth. */
        void *n = real_mmap(NULL, new_len, PROT_READ | PROT_WRITE, MAP_SHARED,
                            p->our_fd, 0);
        if (n == MAP_FAILED) return MAP_FAILED;
        real_munmap(old_addr, old_len);
        if (p->witness) real_munmap(p->witness, p->len);
        p->addr = n;
        p->len = new_len;
        void *w = real_mmap(NULL, new_len, PROT_READ, MAP_SHARED, p->our_fd, 0);
        p->witness = (w == MAP_FAILED) ? NULL : (unsigned char *)w;
        for (int i = 0; i < MAXMAP; i++)
            if (maps[i].used && maps[i].addr == old_addr) {
                maps[i].addr = n;
                maps[i].len = new_len;
            }
        n_resize++;
        log_line("mremap: pool moved onto our fd=%d, %zu -> %zu", p->our_fd,
                 old_len, new_len);
        return n;
    }
    va_list ap;
    va_start(ap, flags);
    void *want = (flags & MREMAP_FIXED) ? va_arg(ap, void *) : NULL;
    va_end(ap);
    return want ? real_mremap(old_addr, old_len, new_len, flags, want)
                : real_mremap(old_addr, old_len, new_len, flags);
}

int munmap(void *addr, size_t len) {
    init();
    for (int i = 0; i < MAXMAP; i++)
        if (maps[i].used && maps[i].addr == addr) maps[i].used = 0;
    return real_munmap(addr, len);
}

#endif /* WC_MARSHAL_ONLY */

/* Make a backing object standing in for an arena slice, and move the client's
 * existing mapping onto it. Returns our fd, or -1 to leave the call alone. */
static int redirect_pool(int client_fd, int32_t size) {
    /* THE ORDER FIREFOX ACTUALLY USES, WHICH IS THE OPPOSITE OF THE ONE THAT IS
     * EASY TO HANDLE.
     *
     * The weston toytoolkit maps its pool and then announces it, so at
     * `create_pool` there is already a mapping to move onto our object.
     * Firefox's rendering pools arrive the other way round: `create_pool`
     * first, `mmap` afterwards. Measured — the two pools this shim originally
     * redirected in Firefox were transient ones it closed at once, while both
     * 4.6 MB pools it actually paints into were logged "no tracked mapping,
     * left alone", so the route looked viable while redirecting nothing that
     * mattered.
     *
     * Nothing has been drawn yet in that case, which makes it the *easier*
     * half: allocate the object, substitute the descriptor, and let the
     * `mmap` interposer hand the client our pages when it asks. No copy, and no
     * `MAP_FIXED` replacement of a live mapping either. */
    struct maprec *m = find_map(client_fd);

    char path[512];
    snprintf(path, sizeof path, "%s/wc-poolshim-%d-%d.bin", backing_dir,
             (int)getpid(), client_fd);
    int ourfd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    if (ourfd < 0) return -1;
    unlink(path); /* anonymous: only the fd names it from here on */
    if (ftruncate(ourfd, size) != 0) {
        close(ourfd);
        return -1;
    }

    if (m) {
        /* Preserve whatever the client already drew, once. */
        void *ours =
            real_mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, ourfd, 0);
        if (ours == MAP_FAILED) {
            close(ourfd);
            return -1;
        }
        size_t copy = m->len < (size_t)size ? m->len : (size_t)size;
        memcpy(ours, m->addr, copy);
        real_munmap(ours, size);

        /* THE STEP THE MODULE HEADER CALLS IMPOSSIBLE, NARROWED. You cannot
         * re-point an existing mapping at another object's pages. You can
         * replace the mapping at that address atomically, which keeps every
         * pointer the client already handed out valid. */
        void *rep = real_mmap(m->addr, size, PROT_READ | PROT_WRITE,
                              MAP_SHARED | MAP_FIXED, ourfd, 0);
        if (rep != m->addr) {
            close(ourfd);
            return -1;
        }
    } else {
        n_no_mapping++;
    }

    for (int i = 0; i < MAXPOOL; i++)
        if (!pools[i].used) {
            pools[i].used = 1;
            pools[i].client_fd = client_fd;
            pools[i].our_fd = ourfd;
            pools[i].addr = m ? m->addr : NULL;
            pools[i].len = size;
            pools[i].last_sum = 0;
            pools[i].checks = pools[i].changes = 0;
            struct stat cst;
            if (fstat(client_fd, &cst) == 0) {
                pools[i].obj_dev = cst.st_dev;
                pools[i].obj_ino = cst.st_ino;
            }
            void *w = real_mmap(NULL, size, PROT_READ, MAP_SHARED, ourfd, 0);
            pools[i].witness = (w == MAP_FAILED) ? NULL : (unsigned char *)w;
            break;
        }
    if (m) m->fd = ourfd; /* the mapping now belongs to our object */
    n_redirected++;
    bytes_redirected += (unsigned long long)size;
    log_line("create_pool fd=%d size=%d -> redirected onto our fd=%d at %p",
             client_fd, size, ourfd, m ? m->addr : NULL);
    return ourfd;
}

struct wl_proxy *wl_proxy_marshal_array_flags(struct wl_proxy *proxy,
                                              uint32_t opcode,
                                              const struct wl_interface *interface,
                                              uint32_t version, uint32_t flags,
                                              union wl_argument *args) {
    init();
    /* WHY THIS CANNOT BE RESOLVED ONCE AT STARTUP, WHICH COST AN AFTERNOON.
     * A preloaded library's constructor runs before the main executable's, but
     * Firefox does not link libxul — it `dlopen`s it, and libwayland-client
     * comes in with it. At constructor time the symbol therefore does not
     * exist, `dlsym` returns NULL, and a shim that caches that NULL calls it
     * later and segfaults before it has intercepted a single request. The
     * weston clients hide this completely: they link libwayland the ordinary
     * way, so the constructor resolves it and the shim looks correct. Resolve
     * on first use instead, and keep retrying while it is absent. */
    if (!real_marshal_array) {
        real_marshal_array = dlsym(RTLD_NEXT, "wl_proxy_marshal_array_flags");
        if (!real_get_interface)
            real_get_interface = dlsym(RTLD_NEXT, "wl_proxy_get_interface");
        if (!real_marshal_array) {
            fprintf(stderr, "wc-poolshim: cannot resolve libwayland, giving up\n");
            _exit(70);
        }
    }
    if (real_get_interface && args) {
        const struct wl_interface *pi = real_get_interface(proxy);
        if (pi && pi->name && opcode < (uint32_t)pi->method_count) {
            const char *method = pi->methods[opcode].name;
            if (shim_verbose >= 2)
                fprintf(stderr, "wc-poolshim: marshal %s.%s\n", pi->name, method);
            if (!strcmp(pi->name, "wl_shm") && !strcmp(method, "create_pool")) {
                /* signature "nhi": args[0]=new_id, args[1]=fd, args[2]=size */
                n_create_pool++;
                log_line("saw wl_shm.create_pool fd=%d size=%d", args[1].h,
                         args[2].i);
                if (shim_diag) diagnose_pool(args[1].h, args[2].i);
                if (shim_on) {
                    int ourfd = redirect_pool(args[1].h, args[2].i);
                    if (ourfd >= 0) args[1].h = ourfd;
                }
            } else if (!strcmp(pi->name, "wl_shm_pool") && !strcmp(method, "resize")) {
                log_line("saw wl_shm_pool.resize size=%d", args[0].i);
            } else if (!strcmp(pi->name, "wl_surface") && !strcmp(method, "commit")) {
                /* WHAT REPLACES `--verify-damage`. With no copy there are no two
                 * buffers to compare, so the question changes from "do the
                 * bytes match" to "is the client writing where the compositor
                 * reads". The witness mapping answers that: it is an
                 * independent view of the object the compositor was handed, so
                 * a commit whose pixels changed must change it too. */
                n_commit++;
                if (shim_diag && (n_commit == 200 || n_commit == 2000))
                    diagnose_again(n_commit == 200 ? "at-200-commits"
                                                   : "at-2000-commits");
                if (n_commit % 5 == 0)
                    fprintf(stderr, "wc-poolshim: commits=%lu pools=%lu\n",
                            n_commit, n_redirected);
                for (int i = 0; i < MAXPOOL; i++) {
                    /* Deliberately not skipping retired pools: a retired pool
                     * is one whose descriptor is gone, not one whose pixels
                     * are. Those are the pools Firefox actually paints into. */
                    if (!pools[i].used || !pools[i].witness) continue;
                    unsigned long long s =
                        witness_sum(pools[i].witness, pools[i].len);
                    pools[i].checks++;
                    if (s != pools[i].last_sum) {
                        pools[i].changes++;
                        pools[i].last_sum = s;
                    }
                    /* Long-lived clients are killed rather than exited, so the
                     * report cannot wait for a destructor. */
                    if (pools[i].checks % 5 == 0)
                        fprintf(stderr,
                                "wc-poolshim: witness pool=%d len=%zu "
                                "commits=%lu changed=%lu\n",
                                i, pools[i].len, pools[i].checks,
                                pools[i].changes);
                }
            }
        }
    }
    return real_marshal_array(proxy, opcode, interface, version, flags, args);
}

static void report(void);

/* Long-lived clients are killed, not exited, so the destructor alone never
 * runs and the experiment reports nothing. */
static void on_signal(int sig) {
    report();
    _exit(sig == 15 ? 0 : 128 + sig);
}

__attribute__((constructor)) static void arm_signals(void) {
    init();
    signal(SIGTERM, on_signal);
    signal(SIGINT, on_signal);
}

static void report(void) {
    if (!n_create_pool && !n_redirected) return;
    fprintf(stderr, "wc-poolshim: commits_seen=%lu\n", n_commit);
    fprintf(stderr,
            "wc-poolshim: pid=%d create_pool=%lu redirected=%lu resize=%lu "
            "no_mapping=%lu bytes=%llu\n",
            (int)getpid(), n_create_pool, n_redirected, n_resize, n_no_mapping,
            bytes_redirected);
    for (int i = 0; i < MAXPOOL; i++)
        if (pools[i].used)
            fprintf(stderr,
                    "wc-poolshim: witness pool=%d len=%zu commits=%lu "
                    "changed=%lu\n",
                    i, pools[i].len, pools[i].checks, pools[i].changes);
}
