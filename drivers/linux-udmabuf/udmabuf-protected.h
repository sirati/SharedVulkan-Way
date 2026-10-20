/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _UDMABUF_PROTECTED_H
#define _UDMABUF_PROTECTED_H
#include <linux/anon_inodes.h>
#include <linux/capability.h>
#include <linux/dma-fence.h>
#include <linux/file.h>
#include <linux/kdev_t.h>
#include <linux/kref.h>
#include <linux/poll.h>
#include <linux/random.h>
#include <linux/user_namespace.h>
#include <linux/workqueue.h>
#include "udmabuf-protected-uapi.h"
struct udmabuf;
struct udmabuf_retirement {
	struct kref refs;
	wait_queue_head_t wait;
	struct work_struct work;
	struct file *source;
	struct udmabuf *ubuf;
	struct dma_fence **fences;
	unsigned int fence_count;
	struct udmabuf_protected_info info;
	struct udmabuf_protected_range *ranges;
	atomic_t vmas, attachments, maps, vmaps;
};
static struct workqueue_struct *udmabuf_retire_wq;
static int udmabuf_protected_get_seals(struct file *file);
static bool udmabuf_is_readonly(struct udmabuf *ubuf);
static void udmabuf_retirement_put(struct udmabuf_retirement *r);
static void udmabuf_retire_buffer(struct dma_buf *buf);
static long udmabuf_ioctl_protected(struct file *filp, unsigned long arg);
static void udmabuf_vma_open(struct vm_area_struct *vma);
static void udmabuf_vma_close(struct vm_area_struct *vma);
static int udmabuf_protected_attach(struct dma_buf *buf, struct dma_buf_attachment *at);
static void udmabuf_protected_detach(struct dma_buf *buf, struct dma_buf_attachment *at);
#endif
