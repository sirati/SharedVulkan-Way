#define _GNU_SOURCE
#include <assert.h>
#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>
static volatile sig_atomic_t faulted;
static void on_fault(int sig,siginfo_t *si,void *ctx){(void)ctx;if(sig!=SIGSEGV||!si->si_addr)_exit(90);faulted=1;_exit(0);}
static void zva(void *p){register void *x0 __asm__("x0")=p;__asm__ volatile(".inst 0xd50b7420" : "+r"(x0) : : "memory");}
int main(int argc,char **argv){assert(argc==2);char *e;uintptr_t a=strtoull(argv[1],&e,0);assert(!*e&&a);if(!strcmp(argv[1],"valid")){uint8_t *p=mmap(NULL,4096,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);assert(p!=MAP_FAILED);for(int i=0;i<128;i++)p[i]=0xa5;zva(p+17);for(int i=0;i<64;i++)assert(p[i]==0);for(int i=64;i<128;i++)assert(p[i]==0xa5);puts("DCZVA_VALID_ZEROED");return 0;}struct sigaction sa={.sa_sigaction=on_fault,.sa_flags=SA_SIGINFO};sigemptyset(&sa.sa_mask);assert(!sigaction(SIGSEGV,&sa,NULL));zva((void*)a);return 91;}
