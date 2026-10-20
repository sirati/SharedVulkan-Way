/* Native Darwin regression against the production copy implementation. */
#include "hl_copy.h"
#include <assert.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
static pthread_mutex_t lock=PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t gate=PTHREAD_COND_INITIALIZER;
static int ready, truncated, checked;
static void *mapping;
static _Atomic int stop;
static void *copy_reader(void *unused) {
    (void)unused;char bytes[64];
    pthread_mutex_lock(&lock);ready=1;pthread_cond_broadcast(&gate);
    while (!truncated) pthread_cond_wait(&gate,&lock);
    pthread_mutex_unlock(&lock);
    assert(hl_wc_copy(bytes,mapping,sizeof(bytes))==-1);
    assert(hl_wc_copy(mapping,bytes,sizeof(bytes))==-1);
    pthread_mutex_lock(&lock);checked=1;pthread_cond_broadcast(&gate);pthread_mutex_unlock(&lock);
    while (!atomic_load(&stop)) (void)hl_wc_copy(bytes,mapping,sizeof(bytes));
    return NULL;
}
int main(void) {
    char path[]="/tmp/hl-copy-fault-XXXXXX";
    int fd=mkstemp(path);assert(fd>=0);assert(!unlink(path));assert(!ftruncate(fd,16384));
    mapping=mmap(NULL,16384,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0);assert(mapping!=MAP_FAILED);
    char in[64]="metadata",out[64];
    void *second=mmap(NULL,16384,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0);assert(second!=MAP_FAILED);
    assert(!hl_wc_copy(mapping,in,sizeof(in)));assert(!hl_wc_copy(out,mapping,sizeof(out)));
    char disk[64];assert(pread(fd,disk,sizeof(disk),0)==sizeof(disk));
    assert(!memcmp(in,disk,sizeof(in)) && !memcmp(in,second,sizeof(in)));
    /* Exercise a full native page too. The copy must mutate the original
     * file-backed shared allocation, never replace/COW the destination VMA. */
    void *page=mmap(NULL,16384,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);assert(page!=MAP_FAILED);
    memset(page,0xa5,16384);assert(!hl_wc_copy(mapping,page,16384));
    assert(!memcmp(page,second,16384));
    for(off_t off=0;off<16384;off+=sizeof(disk)) {
        assert(pread(fd,disk,sizeof(disk),off)==sizeof(disk));
        assert(!memcmp(disk,(char *)page+off,sizeof(disk)));
    }
    assert(!munmap(page,16384));assert(!munmap(second,16384));
    pthread_t thread;assert(!pthread_create(&thread,NULL,copy_reader,NULL));
    pthread_mutex_lock(&lock);while(!ready)pthread_cond_wait(&gate,&lock);
    assert(!ftruncate(fd,0));truncated=1;pthread_cond_broadcast(&gate);pthread_mutex_unlock(&lock);
    /* Wait until the deterministic fault checks finish before growing. */
    pthread_mutex_lock(&lock);while(!checked)pthread_cond_wait(&gate,&lock);pthread_mutex_unlock(&lock);
    for(int i=0;i<1000;i++){assert(!ftruncate(fd,16384));assert(!ftruncate(fd,0));}
    atomic_store(&stop,1);assert(!pthread_join(thread,NULL));
    assert(!munmap(mapping,16384));close(fd);
    puts("copy-fault-check: native truncate reads/writes return failure without host crash");
}
