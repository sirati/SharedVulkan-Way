/* Guest seccomp state. The existing thread-table mutex serializes installation,
 * TSYNC, clone inheritance, deactivation and filter evaluation. */
#include "hl_seccomp.h"
#include "thread.h"
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>

#define KILL_PROCESS 0x80000000U
#define ALLOW 0x7fff0000U
#define ACTION 0xffff0000U
#define MAX_PATH 32768U
struct hl_seccomp_filter {
    struct hl_seccomp_filter *prev;
    unsigned refs, len, path;
    hl_sock_filter insns[];
};
void hl_seccomp_retain(struct hl_seccomp_filter *p) { if (p) ++p->refs; }
void hl_seccomp_release(struct hl_seccomp_filter *p) {
    while (p && !--p->refs) {
        struct hl_seccomp_filter *prev = p->prev;
        free(p); p = prev;
    }
}
static int ancestor(struct hl_seccomp_filter *old, struct hl_seccomp_filter *p) {
    if (!old) return 1;
    for (; p; p=p->prev) if (old == p) return 1;
    return 0;
}
typedef struct { thread_entry_t *self; struct hl_seccomp_filter *p; int64_t bad; int apply; } sync_ctx;
static void sync_one(thread_entry_t *t, void *opaque) {
    sync_ctx *c = opaque;
    if (t->seccomp_tgid != c->self->seccomp_tgid || t->vm_exited ||
        atomic_load(&t->seccomp_killed)) return;
    if (!c->apply) {
        if (!c->bad && !ancestor(t->seccomp_filter, c->self->seccomp_filter))
            c->bad = t->guest_tid;
    } else {
        hl_seccomp_retain(c->p);
        hl_seccomp_release(t->seccomp_filter);
        t->seccomp_filter = c->p;
        if (c->self->no_new_privs) t->no_new_privs = 1;
    }
}
static int64_t install(guest_t *g, uint64_t flags, uint64_t addr) {
    /* No listener, SPEC_ALLOW, logging, or TSYNC_ESRCH flags are implemented. */
    if (flags & ~1ULL) return -22;
    struct { uint16_t len; uint8_t pad[6]; uint64_t filter; } f;
    if (guest_read(g, addr, &f, sizeof(f)) < 0) return -14;
    if (!f.len || f.len > HL_SECCOMP_MAX_INSNS) return -22;
    pthread_mutex_lock(thread_get_lock());
    int permitted=current_thread && current_thread->no_new_privs;
    pthread_mutex_unlock(thread_get_lock());
    if (!permitted) return -13;
    struct hl_seccomp_filter *p = calloc(1, sizeof(*p)+f.len*sizeof(hl_sock_filter));
    if (!p) return -12;
    p->len = f.len; p->refs = 1;
    if (guest_read(g, f.filter, p->insns, f.len*sizeof(hl_sock_filter)) < 0) {
        free(p); return -14;
    }
    int valid = hl_bpf_validate(p->insns, p->len);
    if (valid) { free(p); return valid == -2 ? -12 : -22; }
    pthread_mutex_lock(thread_get_lock());
    thread_entry_t *t = current_thread;
    int64_t result = 0;
    if (!t || !t->no_new_privs) result = -13;
    else if (p->len + (t->seccomp_filter ? t->seccomp_filter->path+4 : 0) > MAX_PATH)
        result = -12;
    else {
        sync_ctx c = { .self=t, .p=p };
        if (flags & 1) thread_for_each_locked(sync_one, &c);
        if (c.bad) result = c.bad;
        else {
            p->prev = t->seccomp_filter;
            hl_seccomp_retain(p->prev);
            p->path = p->len + (p->prev ? p->prev->path+4 : 0);
            if (flags & 1) { c.apply=1; thread_for_each_locked(sync_one, &c); }
            else {
                hl_seccomp_retain(p);
                hl_seccomp_release(t->seccomp_filter);
                t->seccomp_filter=p;
            }
        }
    }
    hl_seccomp_release(p);
    pthread_mutex_unlock(thread_get_lock());
    return result;
}
int64_t hl_seccomp_prctl(guest_t *g, uint64_t op, uint64_t a, uint64_t b, uint64_t c, uint64_t d) {
    op=(uint32_t)op; /* prctl option is a Linux int */
    if (op == 22) { /* PR_SET_SECCOMP */
        if (a != 2) return -22;
        return install(g, 0, b);
    }
    pthread_mutex_lock(thread_get_lock());
    thread_entry_t *t = current_thread;
    int64_t r = -22;
    if (op == 21) r = t && t->seccomp_filter ? 2 : 0;
    else if (op == 38 && a == 1 && !b && !c && !d && t) {
        t->no_new_privs=1; r=0;
    } else if (op == 39 && !a && !b && !c && !d) r=t ? t->no_new_privs : 0;
    pthread_mutex_unlock(thread_get_lock());
    return r;
}
int64_t hl_seccomp_syscall(guest_t *g, uint64_t op, uint64_t flags, uint64_t arg) {
    op=(uint32_t)op; flags=(uint32_t)flags; /* Linux unsigned int ABI */
    if (op == 1) return install(g, flags, arg);
    if (op == 2) { /* SECCOMP_GET_ACTION_AVAIL */
        uint32_t a;
        if (flags) return -22;
        if (guest_read(g, arg, &a, sizeof(a)) < 0) return -14;
        switch (a) {
        case 0: case KILL_PROCESS: case 0x30000: case 0x50000:
        case 0x7ff00000: case ALLOW: return 0;
        default: return -95;
        }
    }
    return -22;
}
static uint32_t normalized(uint32_t v) {
    switch (v & ACTION) {
    case 0: case KILL_PROCESS: case 0x30000: case 0x50000:
    case 0x7fc00000: case 0x7ff00000: case 0x7ffc0000: case ALLOW: return v;
    default: return KILL_PROCESS;
    }
}
uint32_t hl_seccomp_evaluate(const hl_seccomp_data *d) {
    uint32_t ret = ALLOW;
    pthread_mutex_lock(thread_get_lock());
    for (struct hl_seccomp_filter *p=current_thread ? current_thread->seccomp_filter : NULL;
         p; p=p->prev) {
        uint32_t v=hl_bpf_run(p->insns,p->len,d);
        /* Signed action ordering puts KILL_PROCESS first. Newest equal action
         * retains its data, as in seccomp_run_filters in Linux. */
        if ((int32_t)(v & ACTION) < (int32_t)(ret & ACTION)) ret=v;
    }
    pthread_mutex_unlock(thread_get_lock());
    return normalized(ret);
}
static int transfer(int fd, void *buf, size_t n, int sending) {
    char *p=buf;
    while (n) {
        ssize_t k=sending ? write(fd,p,n) : read(fd,p,n);
        if (k<0 && errno==EINTR) continue;
        if (k<=0) return -1;
        p+=k; n-=(size_t)k;
    }
    return 0;
}
/* Fork copies a bounded, pointer-free snapshot while holding the table lock,
 * then releases the lock before writing the IPC socket. */
int hl_seccomp_fork_send(int fd) {
    size_t cap=12+MAX_PATH*(sizeof(hl_sock_filter)+sizeof(uint32_t));
    unsigned char *buf=malloc(cap);
    if (!buf) return -1;
    uint32_t hdr[3]={0}; size_t pos=sizeof(hdr);
    pthread_mutex_lock(thread_get_lock());
    if (current_thread) {
        hdr[0]=current_thread->no_new_privs;
        for (struct hl_seccomp_filter *p=current_thread->seccomp_filter;p;p=p->prev) {
            uint32_t len=p->len;
            memcpy(buf+pos,&len,4); pos+=4;
            memcpy(buf+pos,p->insns,len*sizeof(hl_sock_filter)); pos+=len*sizeof(hl_sock_filter);
            ++hdr[1];
        }
    }
    pthread_mutex_unlock(thread_get_lock());
    hdr[2]=(uint32_t)(pos-sizeof(hdr)); memcpy(buf,hdr,sizeof(hdr));
    int r=transfer(fd,buf,pos,1); free(buf); return r;
}
int hl_seccomp_fork_receive(int fd) {
    uint32_t hdr[3];
    if (transfer(fd,hdr,sizeof(hdr),0)<0 || hdr[0]>1 || hdr[1]>MAX_PATH ||
        hdr[2]>MAX_PATH*(sizeof(hl_sock_filter)+sizeof(uint32_t))) return -1;
    unsigned char *buf=malloc(hdr[2] ? hdr[2] : 1);
    if (!buf) return -1;
    if (transfer(fd,buf,hdr[2],0)<0) { free(buf); return -1; }
    struct hl_seccomp_filter *head=NULL, **tail=&head;
    size_t pos=0; unsigned path=0;
    for (unsigned i=0;i<hdr[1];++i) {
        uint32_t len;
        if (hdr[2]-pos<4) goto bad;
        memcpy(&len,buf+pos,4); pos+=4;
        if (!len || len>HL_SECCOMP_MAX_INSNS || len>(hdr[2]-pos)/sizeof(hl_sock_filter)) goto bad;
        path+=len+(i ? 4 : 0);
        if (path>MAX_PATH) goto bad;
        struct hl_seccomp_filter *p=calloc(1,sizeof(*p)+len*sizeof(hl_sock_filter));
        if (!p) goto bad;
        p->refs=1; p->len=len;
        memcpy(p->insns,buf+pos,len*sizeof(hl_sock_filter)); pos+=len*sizeof(hl_sock_filter);
        if (hl_bpf_validate(p->insns,len)) { free(p); goto bad; }
        *tail=p; tail=&p->prev;
    }
    if (pos != hdr[2] || !current_thread) goto bad;
    /* Derive each suffix cost without trusting transmitted accounting. */
    for (struct hl_seccomp_filter *p=head;p;p=p->prev) {
        p->path=0;
        for (struct hl_seccomp_filter *q=p;q;q=q->prev) p->path+=q->len+(q!=p ? 4 : 0);
    }
    pthread_mutex_lock(thread_get_lock());
    hl_seccomp_release(current_thread->seccomp_filter);
    current_thread->seccomp_filter=head; current_thread->no_new_privs=hdr[0];
    pthread_mutex_unlock(thread_get_lock());
    free(buf); return 0;
bad:
    hl_seccomp_release(head); free(buf); return -1;
}
