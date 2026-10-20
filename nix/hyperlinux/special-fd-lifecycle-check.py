from pathlib import Path
import re,sys
src=Path(sys.argv[1])/'src';out=Path(sys.argv[2]);out.mkdir(parents=True,exist_ok=True)
(out/'lifecycle-test-main.h').write_text(Path(__file__).with_name('special-fd-lifecycle-check.h').read_text())
def fun(text,name):
 m=re.search(r'(?m)^[^\n;{}]*\b'+re.escape(name)+r'\([^;{}]*\)\s*\{',text);assert m,name
 stripped=re.sub(r'/\*.*?\*/|//[^\n]*|"(?:\\.|[^"\\])*"',lambda x:' '*len(x[0]),text,flags=re.S)
 a=text.index('{',m.start());level=1;i=a+1
 while level:
  level+=(stripped[i]=='{')-(stripped[i]=='}');i+=1
 return text[m.start():i]+'\n'
core=(src/'syscall.c').read_text();fs=(src/'syscall_fs.c').read_text();exe=(src/'syscall_exec.c').read_text()
helpers=core[core.index('pthread_mutex_t hl_fd_lifecycle_mutex'):core.index('/* ---------- Linux errno translation')]
execpart=exe[exe.index('    {\n    HL_FD_LIFECYCLE_SCOPE;',exe.index('POINT OF NO RETURN')):exe.index('    /* Step 5: Reset guest memory')]
unit='''#define _GNU_SOURCE
#define GUEST_H
#include <stdint.h>
#include <stddef.h>
typedef struct guest { int unused; } guest_t;
int guest_read(guest_t*,uint64_t,void*,size_t);
int guest_write(guest_t*,uint64_t,const void*,size_t);
int guest_read_str(guest_t*,uint64_t,char*,size_t);
#include "syscall.h"
#include "syscall_internal.h"
#include "fd_object.h"
#include "hl_fd_lifecycle.h"
#include "trace.h"
#include <unistd.h>
#include <fcntl.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <stdatomic.h>
#include <errno.h>
#include <dirent.h>
#include <time.h>
#ifdef __linux__
#include <sys/event.h>
static int test_kqueue(void) { return open("/dev/null",O_RDONLY|O_CLOEXEC); }
static int test_kevent(int q,const struct kevent*c,int n,struct kevent*o,int max,const struct timespec*t) {return 0;}
#define kqueue() test_kqueue()
#define kevent(...) test_kevent(__VA_ARGS__)
#define PTHREAD_RECURSIVE_MUTEX_INITIALIZER PTHREAD_RECURSIVE_MUTEX_INITIALIZER_NP
#endif
fd_entry_t fd_table[FD_TABLE_SIZE];
pthread_mutex_t fd_lock=PTHREAD_MUTEX_INITIALIZER;
int hl_verbose;
#define FD_BITMAP_WORDS (FD_TABLE_SIZE/64)
static uint64_t fd_free_bitmap[FD_BITMAP_WORDS];
static inline void fd_bitmap_set_free(int fd){fd_free_bitmap[fd/64]|=1ULL<<(fd%64);}
static inline void fd_bitmap_set_used(int fd){fd_free_bitmap[fd/64]&=~(1ULL<<(fd%64));}
uint32_t hl_trace_mask;
void hl_trace(uint32_t c,const char*f,...){(void)c;(void)f;}
void hl_wc_socket_drop(hl_open_file_t*of){(void)of;}
void hl_vdir_release(void*p){free(p);}
void hl_vdir_retain(void*p){(void)p;}
void epoll_instance_destroy(void*p){free(p);}
void hl_abstract_bind_release(int fd){(void)fd;}
void syscall_stats_clear_fd(int fd){(void)fd;}
int64_t linux_errno(void){return -errno;}
int guest_read(guest_t*g,uint64_t a,void*b,size_t n){memcpy(b,(void*)(uintptr_t)a,n);return 0;}
int guest_write(guest_t*g,uint64_t a,const void*b,size_t n){memcpy((void*)(uintptr_t)a,b,n);return 0;}
'''
unit+='#include "fd_object.c"\n#undef sa_handler\n#undef sa_sigaction\n#include "syscall_fd.c"\n#include "syscall_inotify.c"\n'+helpers
unit+=fun(fs,'fd_special_subsystem_close')+fun(fs,'sys_close')+'\n#define LINUX_CLOSE_RANGE_CLOEXEC 4\n'+fun(fs,'sys_close_range')
unit+='\nstatic void test_exec_cloexec(void) {\n'+execpart+'}\n'
unit+='\n#include "lifecycle-test-main.h"\n';(out/'lifecycle-unit.c').write_text(unit)
# Linux-only scaffolding for Darwin kqueue APIs. The exercised factories use
# actual pipe/fd operations; event-wait behavior is deliberately not asserted.
fake=out/'fake';(fake/'sys').mkdir(parents=True,exist_ok=True);(fake/'Hypervisor').mkdir(exist_ok=True)
(fake/'Hypervisor/Hypervisor.h').write_text('#pragma once\n#include <stdint.h>\ntypedef uint64_t hv_vcpu_t; typedef uint64_t hv_sys_reg_t; typedef struct {int dummy;} hv_vcpu_exit_t;\n')
tokens=set(re.findall(r'\b(?:NOTE_[A-Z_]+|EVFILT_[A-Z_]+|EV_[A-Z_]+)\b',(src/'syscall_fd.c').read_text()+(src/'syscall_inotify.c').read_text()))
defines='\n'.join('#define '+x+' '+str(i+1) for i,x in enumerate(sorted(tokens-{'EV_SET'})))
(fake/'sys/event.h').write_text('''#ifndef TEST_EVENT_H
#define TEST_EVENT_H
#include <stdint.h>
#include <time.h>
struct kevent {uintptr_t ident; short filter; unsigned short flags; unsigned fflags; intptr_t data; void*udata;};
'''+defines+'''
#define EV_SET(p,a,b,c,d,e,f) do{struct kevent*k=(p);k->ident=a;k->filter=b;k->flags=c;k->fflags=d;k->data=e;k->udata=f;}while(0)
#endif
''')
negative=out/'negative';negative.mkdir(exist_ok=True)
for name in ['fd_object.c','syscall_fd.c','syscall_inotify.c','lifecycle-unit.c']:
 text=((out if name=='lifecycle-unit.c' else src)/name).read_text()
 text=text.replace('    HL_FD_LIFECYCLE_SCOPE;','    /* Negative control: lifecycle exclusion removed. */')
 text=text.replace('hl_fd_lifecycle_end(&fd_lifecycle);','(void)0;')
 (negative/name).write_text(text)
print(out)
