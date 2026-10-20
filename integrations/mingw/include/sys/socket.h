#ifndef WC_MINGW_SOCKET_H
#define WC_MINGW_SOCKET_H
#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>
typedef unsigned int uid_t;
typedef unsigned int gid_t;
typedef unsigned int socklen_t;
struct sockaddr {
  unsigned short sa_family;
  char sa_data[14];
};
struct iovec {
  void *iov_base;
  size_t iov_len;
};
struct msghdr {
  void *msg_name;
  socklen_t msg_namelen;
  struct iovec *msg_iov;
  size_t msg_iovlen;
  void *msg_control;
  size_t msg_controllen;
  int msg_flags;
};
struct cmsghdr {
  size_t cmsg_len;
  int cmsg_level, cmsg_type;
};
#define AF_UNIX 1
#define AF_LOCAL AF_UNIX
#define PF_LOCAL AF_UNIX
#define SOCK_STREAM 1
#define SOCK_CLOEXEC 0
#define SOL_SOCKET 1
#define SCM_RIGHTS 1
#define MSG_DONTWAIT 1
#define MSG_NOSIGNAL 2
#define MSG_CMSG_CLOEXEC 0
#define CMSG_ALIGN(n) (((n) + sizeof(size_t) - 1) & ~(sizeof(size_t) - 1))
#define CMSG_LEN(n) (CMSG_ALIGN(sizeof(struct cmsghdr)) + (n))
#define CMSG_SPACE(n) CMSG_ALIGN(CMSG_LEN(n))
#define CMSG_DATA(c) ((unsigned char *)(c) + CMSG_ALIGN(sizeof(struct cmsghdr)))
#define CMSG_FIRSTHDR(m)                                                       \
  ((m)->msg_controllen >= sizeof(struct cmsghdr)                               \
       ? (struct cmsghdr *)(m)->msg_control                                    \
       : NULL)
#define CMSG_NXTHDR(m, c) ((void)(m), (void)(c), (struct cmsghdr *)NULL)
ssize_t wc_native_sendmsg(int, const struct msghdr *, int);
ssize_t wc_native_recvmsg(int, struct msghdr *, int);
int wc_native_connect(int, const struct sockaddr *, socklen_t);
#define connect wc_native_connect
#define sendmsg wc_native_sendmsg
#endif
