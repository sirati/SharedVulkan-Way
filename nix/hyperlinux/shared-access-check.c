/* Compile with -D_GNU_SOURCE -pthread. No HVF, guest or real Mac is used. */
#define HL_WC_SHARED_TEST
#include "hl_shared.c"
#include <stdio.h>
#include <string.h>

static char primary[0x400000];
static guest_t guest={.host_base=primary};
static owner *add_view(int writable, int prot, uint64_t va) {
    owner *o = calloc(1,sizeof(*o));
    view *v = calloc(1,sizeof(*v));
    void *base = mmap(NULL,4096,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    assert(base != MAP_FAILED);
    memcpy(base,"shared",7);
    if (!writable) assert(!mprotect(base,4096,PROT_READ));
    o->backing = malloc(sizeof(*o->backing));
    *o->backing = (wc_hl_backing){base,4096};
    o->map = (wc_hl_mapping){ .base=base,.mapped_len=4096,.len=4096 };
    o->refs=1; o->writable=writable; o->seal_fd=-1; o->ipa=va+0x200000; o->span=4096;
    *v=(view){.va=va,.end=va+4096,.prot=prot,.o=o,.next=views}; views=v;
    return o;
}
static pthread_mutex_t gate_lock=PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t gate=PTHREAD_COND_INITIALIZER;
static int acquired, removed;
static void *reader(void *unused) {
    (void)unused;
    hl_wc_access_scope scope;hl_wc_access_begin(&scope);
    int found;uint64_t avail;
    char *p=hl_wc_resolve(0x1000,&avail,0,&found);
    assert(found && p && avail==4096);
    pthread_mutex_lock(&gate_lock);acquired=1;pthread_cond_broadcast(&gate);
    while (!removed) pthread_cond_wait(&gate,&gate_lock);
    assert(released==0 && unmapped==1);
    assert(!memcmp(p,"shared",7)); /* Complete native use AFTER concurrent unmap. */
    pthread_mutex_unlock(&gate_lock);
    hl_wc_access_end(&scope);
    return NULL;
}
int main(void) {
    add_view(1,LINUX_PROT_READ|LINUX_PROT_WRITE,0x1000);
    assert(hl_wc_stage2_fault(0x1000,0x201000));
    assert(hl_wc_stage2_fault(0x1040,0x201000)); /* HVF may report page IPA. */
    assert(hl_wc_stage2_fault(0x1040,0x201040));
    assert(!hl_wc_stage2_fault(0x1040,0x202000));
    assert(!hl_wc_stage2_fault(0x2000,0x201000));
    pthread_t thread;assert(!pthread_create(&thread,NULL,reader,NULL));
    pthread_mutex_lock(&gate_lock);
    while (!acquired) pthread_cond_wait(&gate,&gate_lock);
    assert(!hl_wc_unmap(&guest,0x1000,4096));
    assert(!views && retained && released==0 && unmapped==1);
    assert(!hl_wc_stage2_fault(0x1000,0x201000));
    assert(hl_wc_ipa_busy(0x201000,4096));
    removed=1;pthread_cond_broadcast(&gate);pthread_mutex_unlock(&gate_lock);
    assert(!pthread_join(thread,NULL));
    assert(released==1 && !retained && !hl_wc_ipa_busy(0x201000,4096));

    add_view(0,LINUX_PROT_READ,0x3000); /* Genuine O_RDONLY-style native view. */
    hl_wc_access_scope scope;hl_wc_access_begin(&scope);
    int found;uint64_t avail;
    assert(hl_wc_resolve(0x3000,&avail,0,&found) && found);
    assert(!hl_wc_resolve(0x3000,&avail,1,&found) && found); /* EFAULT, no fallback. */
    assert(!hl_wc_protect(&guest,0x3000,4096,0));
    assert(!hl_wc_resolve(0x3000,&avail,0,&found) && found);
    assert(!hl_wc_resolve(0x3000,&avail,1,&found) && found);
    assert(hl_wc_protect(&guest,0x3000,4096,LINUX_PROT_WRITE)==-EACCES);
    hl_wc_access_end(&scope);
    assert(!hl_wc_unmap(&guest,0x3000,4096));assert(released==2);

    add_view(1,LINUX_PROT_READ,0x5000);
    assert(!hl_wc_resolve(0x5000,&avail,0,&found) && found); /* No raw pointer without scope. */
    hl_wc_access_begin(&scope);
    assert(!hl_wc_resolve(0x5000,&avail,1,&found) && found);
    assert(!hl_wc_protect(&guest,0x5000,4096,LINUX_PROT_READ|LINUX_PROT_WRITE));
    assert(hl_wc_resolve(0x5000,&avail,1,&found) && found);
    assert(!hl_wc_unmap(&guest,0x5000,4096));assert(released==2);
    hl_wc_access_end(&scope);assert(released==3 && !retained);
    puts("shared-access-check: concurrent unmap/pinned completion and read-only/PROT_NONE denial passed");
}
