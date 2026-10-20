/* SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note */
#ifndef _UDMABUF_PROTECTED_UAPI_H
#define _UDMABUF_PROTECTED_UAPI_H
#include <linux/types.h>
#include <linux/ioctl.h>
#define UDMABUF_PROTECTED_VERSION 1
#define UDMABUF_PROTECTED_READONLY 1
struct udmabuf_protected_range { __u64 offset; __u64 size; };
/* All ranges refer to the single retained memfd captured by this ioctl. */
struct udmabuf_protected_create {
	__u32 version, flags, memfd, count;
	__aligned_u64 ranges;
	__s32 data_fd, monitor_fd;
	__u64 reserved;
};
#define UDMABUF_PROTECTED_LIVE 0
#define UDMABUF_PROTECTED_DRAINING 1
#define UDMABUF_PROTECTED_RETIRED 2
#define UDMABUF_PROTECTED_FAILED 3
struct udmabuf_protected_info {
	__u32 version, flags, state, count;
	__u8 instance[16];
	__u64 export_id, source_dev, source_ino, source_bytes, bytes;
};
struct udmabuf_protected_query_range {
	__u32 index, reserved;
	struct udmabuf_protected_range range;
};
/* Private experimental ABI, not an upstream ioctl-number allocation. */
#define UDMABUF_PROTECTED_CREATE _IOWR('u', 0xf0, struct udmabuf_protected_create)
#define UDMABUF_PROTECTED_INFO _IOR('u', 0xf1, struct udmabuf_protected_info)
#define UDMABUF_PROTECTED_RANGE _IOWR('u', 0xf2, struct udmabuf_protected_query_range)
#endif
