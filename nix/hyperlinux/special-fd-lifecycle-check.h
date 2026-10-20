static unsigned assertions;
#define CHECK(x) do { assertions++; if(!(x)){fprintf(stderr,"lifecycle line%d %s errno%d\n",__LINE__,#x,errno);exit(1);} }while(0)
enum { FACTORY, CLOSE_ONE, ALLOCATE, CLOSE_RANGE, EXEC_CLOSE };
typedef struct { int action,kind,fd,native; int64_t result; _Atomic int done; } job;
static int make_special(int kind) {
    uint64_t mask=0;
    switch(kind){
    case 0:return sys_eventfd2(0,0x80800);
    case 1:return sys_timerfd_create(1,0x80800);
    case 2:return sys_signalfd4(NULL,-1,(uint64_t)(uintptr_t)&mask,8,0x80800);
    default:return sys_inotify_init1(0x80800);
    }
}
static void*run_job(void*p){
    job*j=p;
    switch(j->action){
    case FACTORY:j->result=make_special(j->kind);break;
    case CLOSE_ONE:j->result=sys_close(j->fd);break;
    case ALLOCATE:j->result=fd_alloc(FD_REGULAR,j->native);break;
    case CLOSE_RANGE:j->result=sys_close_range(0,FD_TABLE_SIZE-1,0);break;
    default:test_exec_cloexec();j->result=0;break;
    }
    atomic_store(&j->done,1);return NULL;
}
static int table_closed(int fd){
    pthread_mutex_lock(&fd_lock);int closed=fd_table[fd].type==FD_CLOSED;pthread_mutex_unlock(&fd_lock);return closed;
}
static void wait_closed(int fd){
    for(int i=0;i<2000;i++){if(table_closed(fd))return;usleep(1000);}CHECK(0);
}
static void wait_lifecycle_held(void){
    for(int i=0;i<2000;i++){
        int rc=pthread_mutex_trylock(&hl_fd_lifecycle_mutex);
        if(rc==EBUSY)return;
        CHECK(rc==0);pthread_mutex_unlock(&hl_fd_lifecycle_mutex);usleep(1000);
    }CHECK(0);
}
static void check_state(int kind,int fd){
    CHECK(fd>=0&&fd<FD_TABLE_SIZE);
    CHECK(fcntl(fd_table[fd].host_fd,F_GETFD)>=0);
    if(kind<3)pthread_mutex_lock(&sfd_lock);else pthread_mutex_lock(&inotify_lock);
    int slot=kind==0?eventfd_find(fd):kind==1?timerfd_find(fd):kind==2?signalfd_find(fd):inotify_find(fd);
    CHECK(slot>=0);
    int native=kind==0?eventfd_state[slot].pipe_rd:kind==1?timerfd_state[slot].kq_fd:kind==2?signalfd_state[slot].pipe_rd:inotify_state[slot].pipe_rd;
    CHECK(native==fd_table[fd].host_fd);
    if(kind<3)pthread_mutex_unlock(&sfd_lock);else pthread_mutex_unlock(&inotify_lock);
    if(kind==0){uint64_t one=1,value=0;CHECK(eventfd_write(fd,NULL,(uint64_t)(uintptr_t)&one,8)==8);CHECK(eventfd_read(fd,NULL,(uint64_t)(uintptr_t)&value,8)==8&&value==1);}
}
static void publication(int kind){
    pthread_t factory,closer;job f={.action=FACTORY,.kind=kind},c={.action=CLOSE_ONE,.fd=0};
    pthread_mutex_lock(&fd_lock);
    CHECK(pthread_create(&factory,NULL,run_job,&f)==0);
    wait_lifecycle_held(); /* Factory is blocked at allocation, before publication. */
    CHECK(pthread_create(&closer,NULL,run_job,&c)==0);
    usleep(20000);CHECK(!atomic_load(&c.done));
    pthread_mutex_unlock(&fd_lock);
    CHECK(pthread_join(factory,NULL)==0&&f.result==0);
    CHECK(pthread_join(closer,NULL)==0&&c.result==0);
    CHECK(table_closed(0));
    pthread_mutex_t*lock=kind<3?&sfd_lock:&inotify_lock;pthread_mutex_lock(lock);
    CHECK((kind==0?eventfd_find(0):kind==1?timerfd_find(0):kind==2?signalfd_find(0):inotify_find(0))<0);
    pthread_mutex_unlock(lock);
}
static void retirement(int kind,int backing){
    int old=make_special(kind);CHECK(old==0);check_state(kind,old);
    pthread_mutex_t*lock=kind<3?&sfd_lock:&inotify_lock;pthread_mutex_lock(lock);
    pthread_t closer,allocator,factory;
    job c={.action=CLOSE_ONE,.fd=old},a={.action=ALLOCATE,.native=dup(backing)},f={.action=FACTORY,.kind=kind};
    CHECK(a.native>=0&&pthread_create(&closer,NULL,run_job,&c)==0);
    wait_closed(old); /* Cleanup is parked on the exact old subsystem lock. */
    CHECK(pthread_create(&allocator,NULL,run_job,&a)==0&&pthread_create(&factory,NULL,run_job,&f)==0);
    usleep(20000);CHECK(!atomic_load(&a.done)&&!atomic_load(&f.done));
    pthread_mutex_unlock(lock);
    CHECK(pthread_join(closer,NULL)==0&&c.result==0);
    CHECK(pthread_join(allocator,NULL)==0&&a.result>=0);
    CHECK(pthread_join(factory,NULL)==0&&f.result>=0&&f.result!=a.result);
    check_state(kind,(int)f.result);
    CHECK(sys_close((int)a.result)==0&&sys_close((int)f.result)==0);
    uint64_t value=0;CHECK(pread(backing,&value,8,0)==8&&value==UINT64_C(0x57434c4946453031));
}
static void sweep(int action){
    for(int i=0;i<4;i++)CHECK(make_special(i)==i);
    pthread_mutex_lock(&sfd_lock);
    pthread_t closer,factory;job c={.action=action},f={.action=FACTORY,.kind=0};
    CHECK(pthread_create(&closer,NULL,run_job,&c)==0);wait_closed(0);
    CHECK(pthread_create(&factory,NULL,run_job,&f)==0);usleep(20000);CHECK(!atomic_load(&f.done));
    pthread_mutex_unlock(&sfd_lock);
    CHECK(pthread_join(closer,NULL)==0&&c.result==0);
    CHECK(pthread_join(factory,NULL)==0&&f.result>=0);
    check_state(0,(int)f.result);CHECK(sys_close((int)f.result)==0);
}
int main(int argc,char **argv){
    alarm(20);memset(fd_free_bitmap,0xff,sizeof(fd_free_bitmap));memset(fd_table,0,sizeof(fd_table));
    timerfd_init();eventfd_init();signalfd_init();inotify_init();
    char path[]="/tmp/hl-lifecycle-XXXXXX";int backing=mkstemp(path);CHECK(backing>=0&&unlink(path)==0);
    uint64_t magic=UINT64_C(0x57434c4946453031);CHECK(pwrite(backing,&magic,8,0)==8);
    for(int kind=0;kind<4;kind++){if(argc==1)publication(kind);retirement(kind,backing);}
    sweep(CLOSE_RANGE);sweep(EXEC_CLOSE);
    for(int fd=0;fd<FD_TABLE_SIZE;fd++)CHECK(table_closed(fd));
    CHECK(close(backing)==0);printf("SPECIAL lifecycle factories close reuse range exec PASS assertions=%u\n",assertions);return 0;
}
