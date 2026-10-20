/* Native Darwin acceptance against production guarded ARM64 atomics. */
#include "hl_atomic.h"
#include <mach/arm/thread_status.h>
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <spawn.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>
extern char **environ;
static pthread_mutex_t lock=PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t gate=PTHREAD_COND_INITIALIZER;
static int ready,truncated,checked;
static _Atomic int stop;
static uint32_t *word;
static void *atomic_reader(void *unused) {
    (void)unused;
    hl_wc_atomic_scope scope;assert(!hl_wc_atomic_begin(&scope));
    pthread_mutex_lock(&lock);ready=1;pthread_cond_broadcast(&gate);
    while(!truncated)pthread_cond_wait(&gate,&lock);pthread_mutex_unlock(&lock);
    uint32_t value=0;
    assert(hl_wc_atomic_load(word,&value)==-EFAULT);
    assert(hl_wc_atomic_store(word,1)==-EFAULT);
    assert(hl_wc_atomic_cas(word,&value,1)==-EFAULT);
    pthread_mutex_lock(&lock);checked=1;pthread_cond_broadcast(&gate);pthread_mutex_unlock(&lock);
    while(!atomic_load(&stop)) {
        int rc=hl_wc_atomic_load(word,&value);assert(rc==0 || rc==-EFAULT);
        rc=hl_wc_atomic_cas(word,&value,1);assert(rc==0 || rc==1 || rc==-EFAULT);
        rc=hl_wc_atomic_store(word,0);assert(rc==0 || rc==-EFAULT);
    }
    hl_wc_atomic_end(&scope);return NULL;
}
static mach_port_t thread_handler(thread_t thread) {
    exception_mask_t mask[EXC_TYPES_COUNT];mach_port_t ports[EXC_TYPES_COUNT];
    exception_behavior_t behavior[EXC_TYPES_COUNT];thread_state_flavor_t flavor[EXC_TYPES_COUNT];
    mach_msg_type_number_t n=EXC_TYPES_COUNT;
    assert(thread_get_exception_ports(thread,EXC_MASK_BAD_ACCESS,mask,&n,ports,behavior,flavor)==KERN_SUCCESS);
    assert(n<=1);return n ? ports[0] : MACH_PORT_NULL; /* caller releases send right */
}
typedef struct {
    exception_mask_t mask[EXC_TYPES_COUNT];mach_port_t port[EXC_TYPES_COUNT];
    exception_behavior_t behavior[EXC_TYPES_COUNT];thread_state_flavor_t flavor[EXC_TYPES_COUNT];
    mach_msg_type_number_t count;
} task_chain;
static void task_snapshot(task_chain *chain) {
    memset(chain,0,sizeof(*chain));chain->count=EXC_TYPES_COUNT;
    assert(task_get_exception_ports(mach_task_self(),EXC_MASK_BAD_ACCESS,
        chain->mask,&chain->count,chain->port,chain->behavior,chain->flavor)==KERN_SUCCESS);
}
static void thread_snapshot(thread_t thread,task_chain *chain) {
    memset(chain,0,sizeof(*chain));chain->count=EXC_TYPES_COUNT;
    assert(thread_get_exception_ports(thread,EXC_MASK_BAD_ACCESS,
        chain->mask,&chain->count,chain->port,chain->behavior,chain->flavor)==KERN_SUCCESS);
}
static void task_snapshot_release(task_chain *chain) {
    for(mach_msg_type_number_t i=0;i<chain->count;i++)
        if(chain->port[i])mach_port_deallocate(mach_task_self(),chain->port[i]);
}
int main(int argc,char **argv) {
    if(argc==2 && !strcmp(argv[1],"--spawn-truncate")) {
        char path[]="/tmp/hl-spawn-fault-XXXXXX";int fd=mkstemp(path);assert(fd>=0);
        assert(!unlink(path));assert(!ftruncate(fd,16384));
        uint32_t *p=mmap(NULL,16384,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0);assert(p!=MAP_FAILED);
        hl_wc_atomic_scope scope;assert(!hl_wc_atomic_begin(&scope));assert(!ftruncate(fd,0));
        uint32_t v;assert(hl_wc_atomic_load(p,&v)==-EFAULT);hl_wc_atomic_end(&scope);
        assert(!munmap(p,16384));close(fd);puts("SPAWNED_TRUNCATE_EFAULT");return 0;
    }
    if(argc==2 && !strcmp(argv[1],"--unrelated")) {
        struct rlimit limit={0,0};setrlimit(RLIMIT_CORE,&limit);alarm(5);
        hl_wc_atomic_scope scope;assert(!hl_wc_atomic_begin(&scope));
        *(volatile uint32_t *)(uintptr_t)1=1; /* Deliberately outside guarded leaf PCs. */
        _exit(99);
    }
    task_chain original_task;task_snapshot(&original_task);
    hl_wc_atomic_scope scope;assert(!hl_wc_atomic_begin(&scope));
    assert(hl_wc_atomic_begin(&scope)==-1 && scope.active);
    hl_wc_atomic_end(&scope);
    thread_t thread=mach_thread_self();mach_port_urefs_t before,after;
    assert(mach_port_get_refs(mach_task_self(),thread,MACH_PORT_RIGHT_SEND,&before)==KERN_SUCCESS);
    for(int i=0;i<1000;i++){assert(!hl_wc_atomic_begin(&scope));hl_wc_atomic_end(&scope);}
    assert(mach_port_get_refs(mach_task_self(),thread,MACH_PORT_RIGHT_SEND,&after)==KERN_SUCCESS && before==after);
    assert(thread_handler(thread)==MACH_PORT_NULL);
    assert(thread_set_exception_ports(thread,EXC_MASK_BAD_ACCESS,MACH_PORT_NULL,EXCEPTION_STATE,ARM_THREAD_STATE64)==KERN_SUCCESS);
    task_chain null_before,null_after;thread_snapshot(thread,&null_before);
    assert(!hl_wc_atomic_begin(&scope));hl_wc_atomic_end(&scope);thread_snapshot(thread,&null_after);
    assert(!memcmp(&null_before,&null_after,sizeof(null_before)));
    task_snapshot_release(&null_before);task_snapshot_release(&null_after);
    assert(thread_set_exception_ports(thread,EXC_MASK_BAD_ACCESS,MACH_PORT_NULL,EXCEPTION_DEFAULT,THREAD_STATE_NONE)==KERN_SUCCESS);
    mach_port_t custom;assert(mach_port_allocate(mach_task_self(),MACH_PORT_RIGHT_RECEIVE,&custom)==KERN_SUCCESS);
    assert(mach_port_insert_right(mach_task_self(),custom,custom,MACH_MSG_TYPE_MAKE_SEND)==KERN_SUCCESS);
    assert(thread_set_exception_ports(thread,EXC_MASK_BAD_ACCESS,custom,EXCEPTION_DEFAULT,THREAD_STATE_NONE)==KERN_SUCCESS);
    assert(mach_port_get_refs(mach_task_self(),custom,MACH_PORT_RIGHT_SEND,&before)==KERN_SUCCESS);
    assert(hl_wc_atomic_begin(&scope)==-1);
    mach_port_t actual=thread_handler(thread);assert(actual==custom);mach_port_deallocate(mach_task_self(),actual);
    assert(mach_port_get_refs(mach_task_self(),custom,MACH_PORT_RIGHT_SEND,&after)==KERN_SUCCESS && before==after);
    assert(thread_set_exception_ports(thread,EXC_MASK_BAD_ACCESS,MACH_PORT_NULL,EXCEPTION_DEFAULT,THREAD_STATE_NONE)==KERN_SUCCESS);
    mach_port_deallocate(mach_task_self(),custom);mach_port_mod_refs(mach_task_self(),custom,MACH_PORT_RIGHT_RECEIVE,-1);
    mach_port_deallocate(mach_task_self(),thread);

    char path[]="/tmp/hl-atomic-fault-XXXXXX";int fd=mkstemp(path);assert(fd>=0);assert(!unlink(path));assert(!ftruncate(fd,16384));
    word=mmap(NULL,16384,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0);assert(word!=MAP_FAILED);
    assert(!hl_wc_atomic_begin(&scope));uint32_t v;
    assert(!hl_wc_atomic_store(word,7));assert(!hl_wc_atomic_load(word,&v) && v==7);
    assert(hl_wc_atomic_cas(word,&v,8)==1);v=7;assert(hl_wc_atomic_cas(word,&v,9)==0 && v==8);
    hl_wc_atomic_end(&scope);
    pthread_t worker;assert(!pthread_create(&worker,NULL,atomic_reader,NULL));
    pthread_mutex_lock(&lock);while(!ready)pthread_cond_wait(&gate,&lock);
    assert(!ftruncate(fd,0));truncated=1;pthread_cond_broadcast(&gate);pthread_mutex_unlock(&lock);
    pthread_mutex_lock(&lock);while(!checked)pthread_cond_wait(&gate,&lock);pthread_mutex_unlock(&lock);
    for(int i=0;i<1000;i++){assert(!ftruncate(fd,16384));assert(!ftruncate(fd,0));}
    atomic_store(&stop,1);assert(!pthread_join(worker,NULL));assert(!munmap(word,16384));close(fd);
    pid_t child;char *args[]={argv[0],"--unrelated",NULL};assert(!posix_spawn(&child,argv[0],NULL,NULL,args,environ));
    int status;assert(waitpid(child,&status,0)==child);
    assert(WIFSIGNALED(status) && (WTERMSIG(status)==SIGBUS || WTERMSIG(status)==SIGSEGV));
    char *spawn_args[]={argv[0],"--spawn-truncate",NULL};
    assert(!posix_spawn(&child,argv[0],NULL,NULL,spawn_args,environ));
    assert(waitpid(child,&status,0)==child && WIFEXITED(status) && WEXITSTATUS(status)==0);
    task_chain final_task;task_snapshot(&final_task);
    assert(!memcmp(&original_task,&final_task,sizeof(original_task)));
    task_snapshot_release(&original_task);task_snapshot_release(&final_task);
    puts("atomic-fault-check: truncated atomic EFAULT/race, rights restoration, custom-handler refusal and unrelated-PC rejection passed");
}
