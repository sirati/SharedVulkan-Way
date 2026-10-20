/* SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note */
#ifndef _UDMABUF_IMPORT_TEST_UAPI_H
#define _UDMABUF_IMPORT_TEST_UAPI_H
#include <linux/ioctl.h>
#define WC_IMPORT_ACQUIRE _IOW('x', 0xf0, int)
#define WC_IMPORT_DROP_DELAYED _IO('x', 0xf1)
#define WC_IMPORT_SIGNAL _IO('x', 0xf2)
#endif
