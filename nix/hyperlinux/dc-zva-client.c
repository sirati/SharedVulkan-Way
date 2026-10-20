#define _GNU_SOURCE
#include <assert.h>
#include <signal.h>
#include <stdint.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <ucontext.h>
#include <unistd.h>
static volatile sig_atomic_t faulted; static uintptr_t expected;
static void on_fault(int sig,siginfo_t *si,void *ctx){
    if(sig!=SIGSEGV || !si->si_addr || (uintptr_t)si->si_addr != expected || faulted)_exit(90);
    faulted=1; ((ucontext_t *)ctx)->uc_mcontext.pc += 4;
}
static void zva(void *p){register void *x0 __asm__("x0")=p;__asm__ volatile(".inst 0xd50b7420":"+r"(x0)::"cc","memory");}
static void install(void){struct sigaction sa={.sa_sigaction=on_fault,.sa_flags=SA_SIGINFO};sigemptyset(&sa.sa_mask);assert(!sigaction(SIGSEGV,&sa,NULL));}
int main(int argc,char **argv){
    assert(argc==2); const char *mode=argv[1];
    if(!strcmp(mode,"valid")||!strcmp(mode,"validtag")){uint8_t *p=mmap(NULL,4096,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);assert(p!=MAP_FAILED);for(int i=0;i<128;i++)p[i]=0xa5;zva(!strcmp(mode,"validtag")?(void *)((uintptr_t)(p+17)|UINT64_C(0xab00000000000000)):p+17);for(int i=0;i<64;i++)assert(p[i]==0);for(int i=64;i<128;i++)assert(p[i]==0xa5);puts("DCZVA_VALID_ZEROED");return 0;}
    char *e; uintptr_t a=strtoull(mode,&e,0); assert(!*e&&a); uint8_t *page=mmap(NULL,4096,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);assert(page!=MAP_FAILED);memset(page,0xa5,4096);
    if(!strcmp(mode,"ro")){expected=(uintptr_t)page;assert(!mprotect(page,4096,PROT_READ));install();zva(page+17);assert(faulted);for(int i=0;i<128;i++)assert(page[i]==0xa5);puts("DCZVA_RO_REFUSED");return 0;}
    if(!strcmp(mode,"none")){expected=(uintptr_t)page;assert(!mprotect(page,4096,PROT_NONE));install();zva(page+17);assert(faulted);puts("DCZVA_NONE_REFUSED");return 0;}
    expected=a;install();zva((void *)(a|UINT64_C(0xab00000000000000)));assert(faulted);puts("DCZVA_CONTROL_REFUSED");return 0;
}
