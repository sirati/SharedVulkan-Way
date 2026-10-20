// SPDX-License-Identifier: GPL-2.0
#define _GNU_SOURCE
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <linux/dma-buf.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include "../udmabuf-protected-uapi.h"
#include "udmabuf-import-test-uapi.h"
#define CHECK(x) do { if (!(x)) { fprintf(stderr,"FAIL line %d: %s errno=%d %s\n",__LINE__,#x,errno,strerror(errno)); exit(1); } } while (0)
static size_t page;
static int raw, source;
static void *source_mapping;
static struct udmabuf_protected_create make_export(unsigned flags, size_t bytes)
{
	struct udmabuf_protected_range range = { page, bytes };
	struct udmabuf_protected_create c = {
		.version=1,.flags=flags,.memfd=source,.count=1,
		.ranges=(uintptr_t)&range,.data_fd=-1,.monitor_fd=-1,
	};
	struct udmabuf_protected_info info;
	struct udmabuf_protected_query_range q = {0};
	struct stat st;
	CHECK(ioctl(raw, UDMABUF_PROTECTED_CREATE, &c)==0);
	CHECK(fstat(source,&st)==0);
	CHECK(ioctl(c.monitor_fd, UDMABUF_PROTECTED_INFO,&info)==0);
	CHECK(info.version==1 && info.state==UDMABUF_PROTECTED_LIVE);
	CHECK(info.flags==flags && info.count==1 && info.bytes==bytes);
	CHECK(info.source_dev==(uint64_t)st.st_dev && info.source_ino==(uint64_t)st.st_ino);
	CHECK(info.source_bytes==(uint64_t)st.st_size && info.export_id!=0);
	CHECK(ioctl(c.monitor_fd,UDMABUF_PROTECTED_RANGE,&q)==0);
	CHECK(q.range.offset==page && q.range.size==bytes);
	return c;
}
static unsigned state(int fd)
{
	struct udmabuf_protected_info i;
	CHECK(ioctl(fd,UDMABUF_PROTECTED_INFO,&i)==0);
	return i.state;
}
static void live(int fd)
{
	struct pollfd p={.fd=fd,.events=POLLIN};
	CHECK(state(fd)==UDMABUF_PROTECTED_LIVE);
	CHECK(poll(&p,1,0)==0);
}
static void retired(int fd)
{
	struct pollfd p={.fd=fd,.events=POLLIN};
	CHECK(poll(&p,1,5000)==1);
	CHECK(!(p.revents&POLLERR) && (p.revents&POLLIN));
	CHECK(state(fd)==UDMABUF_PROTECTED_RETIRED);
	CHECK(close(fd)==0);
}
static void byte_send(int fd) { char c='x'; CHECK(write(fd,&c,1)==1); }
static void byte_recv(int fd) { char c; CHECK(read(fd,&c,1)==1); }
static void child_ok(pid_t pid)
{
	int status;
	CHECK(waitpid(pid,&status,0)==pid && WIFEXITED(status) && WEXITSTATUS(status)==0);
}
struct sync_thread { int fd; unsigned long direction; pthread_barrier_t *barrier; int error; };
static void *sync_loop(void *arg)
{
	struct sync_thread *t=arg;
	pthread_barrier_wait(t->barrier);
	for(int i=0;i<1000;i++) {
		struct dma_buf_sync s={.flags=t->direction|DMA_BUF_SYNC_START};
		if(ioctl(t->fd,DMA_BUF_IOCTL_SYNC,&s)) { t->error=errno; break; }
		s.flags=t->direction|DMA_BUF_SYNC_END;
		if(ioctl(t->fd,DMA_BUF_IOCTL_SYNC,&s)) { t->error=errno; break; }
	}
	return NULL;
}
static void stress_sync(int fd, int ro)
{
	pthread_t threads[12]; struct sync_thread args[12]; pthread_barrier_t b;
	CHECK(pthread_barrier_init(&b,NULL,12)==0);
	for(int n=0;n<12;n++) {
		args[n]=(struct sync_thread){.fd=fd,.direction=ro?DMA_BUF_SYNC_READ:
			(n%3==0?DMA_BUF_SYNC_READ:n%3==1?DMA_BUF_SYNC_WRITE:DMA_BUF_SYNC_RW),.barrier=&b};
		CHECK(pthread_create(&threads[n],NULL,sync_loop,&args[n])==0);
	}
	for(int n=0;n<12;n++) { CHECK(pthread_join(threads[n],NULL)==0); CHECK(args[n].error==0); }
	CHECK(pthread_barrier_destroy(&b)==0);
}
static void fork_vma_test(void)
{
	struct udmabuf_protected_create c=make_export(UDMABUF_PROTECTED_READONLY,4*page);
	int d=dup(c.data_fd), go[2];
	CHECK(d>=0 && pipe2(go,O_CLOEXEC)==0);
	CHECK((fcntl(d,F_GETFL)&O_ACCMODE)==O_RDONLY);
	CHECK(mmap(NULL,5*page,PROT_READ,MAP_SHARED,d,0)==MAP_FAILED);
	CHECK(mmap(NULL,4*page,PROT_READ,MAP_SHARED,d,page)==MAP_FAILED);
	CHECK(mmap(NULL,page,PROT_READ|PROT_WRITE,MAP_SHARED,d,0)==MAP_FAILED);
	CHECK(mmap(NULL,page,PROT_READ|PROT_EXEC,MAP_SHARED,d,0)==MAP_FAILED);
	unsigned char *v=mmap(NULL,4*page,PROT_READ,MAP_SHARED,d,0);
	CHECK(v!=MAP_FAILED && v[0]==0x21 && v[3*page]==0x24);
	CHECK(mprotect(v,4*page,PROT_READ|PROT_WRITE)==-1);
	CHECK(mprotect(v,4*page,PROT_READ|PROT_EXEC)==-1);
	stress_sync(d,1);
	struct dma_buf_sync sync={.flags=DMA_BUF_SYNC_WRITE|DMA_BUF_SYNC_START};
	CHECK(ioctl(d,DMA_BUF_IOCTL_SYNC,&sync)==-1 && errno==EACCES);
	CHECK(close(c.data_fd)==0); live(c.monitor_fd);
	pid_t pid=fork(); CHECK(pid>=0);
	if(!pid) {
		close(go[1]); close(d); close(source); close(raw); close(c.monitor_fd);
		CHECK(munmap(source_mapping,6*page)==0);
		byte_recv(go[0]); CHECK(v[0]==0x21 && v[3*page]==0x24);
		CHECK(munmap(v,4*page)==0); _exit(0);
	}
	close(go[0]); CHECK(close(d)==0); live(c.monitor_fd);
	CHECK(munmap(v+page,page)==0); live(c.monitor_fd);
	CHECK(munmap(v,page)==0 && munmap(v+2*page,2*page)==0); live(c.monitor_fd);
	byte_send(go[1]); close(go[1]); child_ok(pid); retired(c.monitor_fd);
	puts("PASS immutable RO, bounds, concurrent CPU sync, dup, fork, split VMAs and surviving VMA after all user FDs close");
}
static void send_fd(int socket,int fd)
{
	char ch='f', control[CMSG_SPACE(sizeof(int))]={0};
	struct iovec io={&ch,1};
	struct msghdr msg={.msg_iov=&io,.msg_iovlen=1,.msg_control=control,.msg_controllen=sizeof(control)};
	struct cmsghdr *c=CMSG_FIRSTHDR(&msg);
	c->cmsg_level=SOL_SOCKET;c->cmsg_type=SCM_RIGHTS;c->cmsg_len=CMSG_LEN(sizeof(int));
	memcpy(CMSG_DATA(c),&fd,sizeof(fd)); CHECK(sendmsg(socket,&msg,0)==1);
}
static int receive_fd(int socket)
{
	char ch,control[CMSG_SPACE(sizeof(int))]={0}; int fd;
	struct iovec io={&ch,1};
	struct msghdr msg={.msg_iov=&io,.msg_iovlen=1,.msg_control=control,.msg_controllen=sizeof(control)};
	CHECK(recvmsg(socket,&msg,MSG_CMSG_CLOEXEC)==1 && !(msg.msg_flags&MSG_CTRUNC));
	struct cmsghdr *c=CMSG_FIRSTHDR(&msg);
	CHECK(c && c->cmsg_level==SOL_SOCKET && c->cmsg_type==SCM_RIGHTS);
	memcpy(&fd,CMSG_DATA(c),sizeof(fd)); return fd;
}
static void transfer_test(int ro)
{
	int s[2],go[2]; CHECK(socketpair(AF_UNIX,SOCK_SEQPACKET|SOCK_CLOEXEC,0,s)==0);
	CHECK(pipe2(go,O_CLOEXEC)==0);
	pid_t pid=fork(); CHECK(pid>=0);
	if(!pid) {
		close(s[0]);close(go[1]);close(source);close(raw);
		CHECK(munmap(source_mapping,6*page)==0);
		if(ro) { CHECK(setgid(65534)==0);CHECK(setuid(65534)==0); }
		byte_recv(go[0]); int fd=receive_fd(s[1]);
		int prot=PROT_READ|(ro?0:PROT_WRITE);
		unsigned char *v=mmap(NULL,page,prot,MAP_SHARED,fd,0);CHECK(v!=MAP_FAILED);
		if(ro) CHECK(mprotect(v,page,PROT_READ|PROT_WRITE)==-1);
		else { v[0]=0x7a; CHECK(v[0]==0x7a); }
		send_fd(s[1],fd);close(fd);byte_recv(go[0]);
		/* Actual process exit removes this VMA, but the parent's other VMA survives. */
		_exit(0);
	}
	close(s[1]);close(go[0]);
	struct udmabuf_protected_create c=make_export(ro?UDMABUF_PROTECTED_READONLY:0,page);
	if(!ro)stress_sync(c.data_fd,0);
	send_fd(s[0],c.data_fd);CHECK(close(c.data_fd)==0);
	live(c.monitor_fd); /* Only the queued SCM_RIGHTS message holds the export. */
	byte_send(go[1]);int returned=receive_fd(s[0]);live(c.monitor_fd);
	unsigned char *v=mmap(NULL,page,PROT_READ,MAP_SHARED,returned,0);CHECK(v!=MAP_FAILED);
	CHECK(close(returned)==0);byte_send(go[1]);close(go[1]);child_ok(pid);
	live(c.monitor_fd);CHECK(v[0]==0x7a);
	CHECK(munmap(v,page)==0);close(s[0]);retired(c.monitor_fd);
	puts(ro?"PASS unprivileged RO SCM_RIGHTS, queued alias, process exit with independent surviving VMA":
		"PASS RW SCM_RIGHTS, queued alias, concurrent mixed CPU sync and process exit with independent surviving VMA");
}
static void producer_exit_test(void)
{
	int s[2]; CHECK(socketpair(AF_UNIX,SOCK_SEQPACKET|SOCK_CLOEXEC,0,s)==0);
	struct udmabuf_protected_create c=make_export(0,page);
	pid_t pid=fork(); CHECK(pid>=0);
	if(!pid) {
		close(s[0]);close(source);close(raw);close(c.monitor_fd);
		CHECK(munmap(source_mapping,6*page)==0);
		unsigned char *v=mmap(NULL,page,PROT_READ|PROT_WRITE,MAP_SHARED,c.data_fd,0);
		CHECK(v!=MAP_FAILED);v[0]=0x7a;
		send_fd(s[1],c.data_fd);
		/* Neither explicit close nor munmap: actual producer exit removes
		 * both, while the queued transferred alias must keep pages alive. */
		_exit(0);
	}
	close(s[1]);CHECK(close(c.data_fd)==0);child_ok(pid);live(c.monitor_fd);
	int transferred=receive_fd(s[0]);
	unsigned char *v=mmap(NULL,page,PROT_READ,MAP_SHARED,transferred,0);
	CHECK(v!=MAP_FAILED && v[0]==0x7a);CHECK(close(transferred)==0);live(c.monitor_fd);
	CHECK(munmap(v,page)==0);close(s[0]);retired(c.monitor_fd);
	puts("PASS actual producer exit retains queued SCM_RIGHTS and subsequent recipient VMA aliases");
}
static void native_test(void)
{
	int importer=open("/dev/wc-udmabuf-import-test",O_RDWR|O_CLOEXEC);CHECK(importer>=0);
	struct udmabuf_protected_create ro=make_export(UDMABUF_PROTECTED_READONLY,page);
	CHECK(ioctl(importer,WC_IMPORT_ACQUIRE,&ro.data_fd)==-1 && errno==EACCES);
	CHECK(close(ro.data_fd)==0);retired(ro.monitor_fd);
	struct udmabuf_protected_create rw=make_export(0,page);
	CHECK(ioctl(importer,WC_IMPORT_ACQUIRE,&rw.data_fd)==0);
	CHECK(close(rw.data_fd)==0);live(rw.monitor_fd);
	CHECK(ioctl(importer,WC_IMPORT_DROP_DELAYED)==0);
	for(int n=0;n<500 && state(rw.monitor_fd)==UDMABUF_PROTECTED_LIVE;n++) usleep(1000);
	CHECK(state(rw.monitor_fd)==UDMABUF_PROTECTED_DRAINING);
	struct pollfd p={.fd=rw.monitor_fd,.events=POLLIN};CHECK(poll(&p,1,0)==0);
	CHECK(ioctl(importer,WC_IMPORT_SIGNAL)==0);retired(rw.monitor_fd);close(importer);
	puts("PASS native dma-buf refs, attachment/SG/vmap retention, RO attach refusal and controlled delayed BOOKKEEP fence");
}
int main(void)
{
	setbuf(stdout,NULL);page=sysconf(_SC_PAGESIZE);CHECK(page==4096);
	raw=open("/dev/udmabuf-protected-test",O_RDWR|O_CLOEXEC);CHECK(raw>=0);
	source=memfd_create("udmabuf-private-fixture",MFD_CLOEXEC|MFD_ALLOW_SEALING);CHECK(source>=0);
	CHECK(ftruncate(source,6*page)==0);
	unsigned char *v=mmap(NULL,6*page,PROT_READ|PROT_WRITE,MAP_SHARED,source,0);CHECK(v!=MAP_FAILED);
	source_mapping=v;
	for(int n=0;n<6;n++)memset(v+n*page,0x20+n,page);
	CHECK(fcntl(source,F_ADD_SEALS,F_SEAL_SHRINK)==0);
	fork_vma_test();transfer_test(0);transfer_test(1);producer_exit_test();native_test();
	CHECK(v[0]==0x20 && v[page]==0x7a && v[5*page]==0x25);
	CHECK(munmap(v,6*page)==0);close(source);close(raw);
	puts("PASS all private kernel alias/retirement assertions; no GPU hardware claim");return 0;
}
