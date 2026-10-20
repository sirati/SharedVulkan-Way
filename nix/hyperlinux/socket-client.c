#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>
static void option(int fd, int expected) {
    int value=-1; socklen_t size=sizeof(value);
    assert(getsockopt(fd,SOL_SOCKET,SO_PASSCRED,&value,&size)==0);
    assert(size==sizeof(value) && value==expected);
}
static void set(int fd,int value) { assert(setsockopt(fd,SOL_SOCKET,SO_PASSCRED,&value,sizeof(value))==0); }
int main(void) {
    int s[2], t[2]; assert(socketpair(AF_UNIX,SOCK_STREAM,0,s)==0);
    int alias=dup(s[0]); assert(alias>=0); set(s[0],1); option(alias,1);
    pid_t pid=fork(); assert(pid>=0);
    if (!pid) { option(alias,1); set(alias,0); _exit(0); }
    int status; assert(waitpid(pid,&status,0)==pid && WIFEXITED(status) && WEXITSTATUS(status)==0);
    option(s[0],0); set(alias,1);
    char byte='x'; assert(write(s[1],&byte,1)==1);
    struct iovec iov={&byte,1};
    union { struct cmsghdr align; char bytes[128]; } ctrl;
    struct msghdr msg={.msg_iov=&iov,.msg_iovlen=1,.msg_control=ctrl.bytes,.msg_controllen=sizeof(ctrl)};
    assert(recvmsg(s[0],&msg,0)==1 && !(msg.msg_flags & MSG_CTRUNC));
    struct cmsghdr *c=CMSG_FIRSTHDR(&msg); assert(c && c->cmsg_level==SOL_SOCKET && c->cmsg_type==SCM_CREDENTIALS);
    struct ucred peer; memcpy(&peer,CMSG_DATA(c),sizeof(peer));
    assert(peer.pid>0 && peer.uid==getuid() && peer.gid==getgid());
    /* Claimed identity cannot replace the native kernel peer. */
    memset(&ctrl,0,sizeof(ctrl)); msg.msg_controllen=CMSG_SPACE(sizeof(struct ucred));
    c=CMSG_FIRSTHDR(&msg); c->cmsg_level=SOL_SOCKET; c->cmsg_type=SCM_CREDENTIALS;
    c->cmsg_len=CMSG_LEN(sizeof(struct ucred));
    struct ucred forged={.pid=getpid(),.uid=getuid()+1,.gid=getgid()};
    memcpy(CMSG_DATA(c),&forged,sizeof(forged));
    assert(sendmsg(s[1],&msg,0)<0 && errno==EPERM);
    assert(socketpair(AF_UNIX,SOCK_STREAM,0,t)==0);
    memset(&ctrl,0,sizeof(ctrl)); msg.msg_controllen=CMSG_SPACE(sizeof(alias));
    c=CMSG_FIRSTHDR(&msg); c->cmsg_level=SOL_SOCKET; c->cmsg_type=SCM_RIGHTS; c->cmsg_len=CMSG_LEN(sizeof(alias));
    memcpy(CMSG_DATA(c),&alias,sizeof(alias)); assert(sendmsg(t[0],&msg,0)==1);
    memset(&ctrl,0,sizeof(ctrl)); msg.msg_controllen=CMSG_SPACE(sizeof(int));
    assert(recvmsg(t[1],&msg,MSG_CMSG_CLOEXEC)==1 && !(msg.msg_flags&MSG_CTRUNC));
    c=CMSG_FIRSTHDR(&msg); assert(c && c->cmsg_type==SCM_RIGHTS && c->cmsg_len==CMSG_LEN(sizeof(int)));
    int received; memcpy(&received,CMSG_DATA(c),sizeof(received)); option(received,1);
    assert(fcntl(received,F_GETFD)&FD_CLOEXEC);
    int status_flags=fcntl(received,F_GETFL); assert(status_flags>=0);
    assert(fcntl(received,F_SETFL,status_flags|O_NONBLOCK)==0);
    assert(fcntl(received,F_GETFD)&FD_CLOEXEC);
    assert(fcntl(received,F_SETFD,0)==0 && fcntl(received,F_GETFD)==0);
    assert(fcntl(received,F_SETFL,status_flags)==0 && fcntl(received,F_GETFD)==0);
    set(received,0); option(alias,0); close(received);
    set(s[0],1); assert(write(s[1],&byte,1)==1);
    msg.msg_control=NULL; msg.msg_controllen=0;
    assert(recvmsg(s[0],&msg,0)==1 && (msg.msg_flags&MSG_CTRUNC));
    int d=socket(AF_UNIX,SOCK_DGRAM,0); assert(d>=0);
    int one=1; assert(setsockopt(d,SOL_SOCKET,SO_PASSCRED,&one,sizeof(one))<0 && errno==EOPNOTSUPP);
    close(d); close(alias); close(s[0]); close(s[1]); close(t[0]); close(t[1]);
    puts("kernel stream credentials and shared option state across dup/fork/SCM_RIGHTS passed");
}
