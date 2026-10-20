// SPDX-License-Identifier: GPL-2.0
/* Private-guest fixture only. No hardware DMA or production authority. */
#include <linux/dma-buf.h>
#include <linux/dma-mapping.h>
#include <linux/dma-fence.h>
#include <linux/dma-resv.h>
#include <linux/device.h>
#include <linux/iosys-map.h>
#include <linux/miscdevice.h>
#include <linux/module.h>
#include <linux/slab.h>
#include <linux/uaccess.h>
#include "udmabuf-import-test-uapi.h"
struct test_fence { struct dma_fence base; spinlock_t lock; };
struct test_import {
	struct mutex lock;
	struct dma_buf *buf;
	struct dma_buf_attachment *at;
	struct sg_table *sg;
	struct iosys_map map;
	struct dma_fence *fence;
};
static struct device *test_device;
static const char *test_fence_name(struct dma_fence *f) { return "udmabuf-private-test"; }
static void test_fence_free(struct dma_fence *f)
{
	kfree(container_of(f, struct test_fence, base));
}
static const struct dma_fence_ops test_fence_ops = {
	.get_driver_name = test_fence_name,
	.get_timeline_name = test_fence_name,
	.release = test_fence_free,
};
static void drop_import(struct test_import *t)
{
	if (!t->buf)
		return;
	if (!iosys_map_is_null(&t->map)) {
		dma_buf_vunmap_unlocked(t->buf, &t->map);
		iosys_map_clear(&t->map);
	}
	if (t->sg) {
		dma_buf_unmap_attachment_unlocked(t->at, t->sg, DMA_BIDIRECTIONAL);
		t->sg = NULL;
	}
	if (t->at) {
		dma_buf_detach(t->buf, t->at);
		t->at = NULL;
	}
	dma_buf_put(t->buf);
	t->buf = NULL;
}
static int test_open(struct inode *inode, struct file *file)
{
	struct test_import *t = kzalloc(sizeof(*t), GFP_KERNEL);
	if (!t)
		return -ENOMEM;
	mutex_init(&t->lock);
	file->private_data = t;
	return 0;
}
static int test_release(struct inode *inode, struct file *file)
{
	struct test_import *t = file->private_data;
	if (t->fence) {
		dma_fence_signal(t->fence);
		dma_fence_put(t->fence);
	}
	drop_import(t);
	kfree(t);
	return 0;
}
static long test_ioctl(struct file *file, unsigned int cmd, unsigned long arg)
{
	struct test_import *t = file->private_data;
	long ret = 0;
	int fd;
	mutex_lock(&t->lock);
	if (cmd == WC_IMPORT_ACQUIRE) {
		if (t->buf || t->fence) { ret = -EBUSY; goto out; }
		if (copy_from_user(&fd, (void __user *)arg, sizeof(fd))) { ret = -EFAULT; goto out; }
		t->buf = dma_buf_get(fd);
		if (IS_ERR(t->buf)) { ret = PTR_ERR(t->buf); t->buf = NULL; goto out; }
		t->at = dma_buf_attach(t->buf, test_device);
		if (IS_ERR(t->at)) { ret = PTR_ERR(t->at); t->at = NULL; goto fail; }
		t->sg = dma_buf_map_attachment_unlocked(t->at, DMA_BIDIRECTIONAL);
		if (IS_ERR(t->sg)) { ret = PTR_ERR(t->sg); t->sg = NULL; goto fail; }
		ret = dma_buf_vmap_unlocked(t->buf, &t->map);
		if (ret) goto fail;
	} else if (cmd == WC_IMPORT_DROP_DELAYED) {
		struct test_fence *f;
		if (!t->buf || t->fence) { ret = -EINVAL; goto out; }
		f = kzalloc(sizeof(*f), GFP_KERNEL);
		if (!f) { ret = -ENOMEM; goto out; }
		spin_lock_init(&f->lock);
		dma_fence_init(&f->base, &test_fence_ops, &f->lock,
			       dma_fence_context_alloc(1), 1);
		dma_resv_lock(t->buf->resv, NULL);
		ret = dma_resv_reserve_fences(t->buf->resv, 1);
		if (!ret)
			dma_resv_add_fence(t->buf->resv, &f->base, DMA_RESV_USAGE_BOOKKEEP);
		dma_resv_unlock(t->buf->resv);
		if (ret) { dma_fence_put(&f->base); goto out; }
		t->fence = &f->base;
		/* Intentionally drop every dma-buf/native reference while the
		 * independently retained synthetic fence remains unsignaled. */
		drop_import(t);
	} else if (cmd == WC_IMPORT_SIGNAL) {
		if (!t->fence || t->buf) { ret = -EINVAL; goto out; }
		dma_fence_signal(t->fence);
		dma_fence_put(t->fence);
		t->fence = NULL;
	} else {
		ret = -ENOTTY;
	}
	goto out;
fail:
	drop_import(t);
out:
	mutex_unlock(&t->lock);
	return ret;
}
static const struct file_operations test_fops = {
	.owner = THIS_MODULE, .open = test_open, .release = test_release,
	.unlocked_ioctl = test_ioctl,
};
static struct miscdevice test_misc = {
	.minor = MISC_DYNAMIC_MINOR, .name = "wc-udmabuf-import-test",
	.fops = &test_fops, .mode = 0600,
};
static int __init test_init(void)
{
	int ret;
	test_device = root_device_register("wc-udmabuf-import-test");
	if (IS_ERR(test_device))
		return PTR_ERR(test_device);
	ret = dma_coerce_mask_and_coherent(test_device, DMA_BIT_MASK(64));
	if (!ret)
		ret = misc_register(&test_misc);
	if (ret)
		root_device_unregister(test_device);
	return ret;
}
static void __exit test_exit(void)
{
	misc_deregister(&test_misc);
	root_device_unregister(test_device);
}
module_init(test_init);
module_exit(test_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Private udmabuf attachment and delayed fence fixture");
MODULE_IMPORT_NS("DMA_BUF");
