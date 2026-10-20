// SPDX-License-Identifier: GPL-2.0
/* Included by the pinned udmabuf.c. Policy and allocation remain in wc-broker. */
static_assert(sizeof(struct udmabuf_protected_create) == 40);
static_assert(sizeof(struct udmabuf_protected_info) == 72);
static_assert(sizeof(struct udmabuf_protected_query_range) == 24);
static atomic64_t udmabuf_export_id = ATOMIC64_INIT(0);
static u8 udmabuf_instance[16];

static bool udmabuf_is_readonly(struct udmabuf *ubuf)
{
	return ubuf->retirement &&
		(ubuf->retirement->info.flags & UDMABUF_PROTECTED_READONLY);
}

static void udmabuf_retirement_free(struct kref *ref)
{
	struct udmabuf_retirement *r = container_of(ref, struct udmabuf_retirement, refs);
	fput(r->source);
	kfree(r->ranges);
	kfree(r);
}
static void udmabuf_retirement_put(struct udmabuf_retirement *r)
{
	if (r)
		kref_put(&r->refs, udmabuf_retirement_free);
}
static void udmabuf_set_retirement(struct udmabuf_retirement *r, u32 state)
{
	smp_store_release(&r->info.state, state);
	wake_up_interruptible_poll(&r->wait,
		state == UDMABUF_PROTECTED_RETIRED ? EPOLLIN | EPOLLRDNORM : EPOLLERR);
}

static void udmabuf_vma_open(struct vm_area_struct *vma)
{
	struct udmabuf *ubuf = vma->vm_private_data;
	if (ubuf->retirement)
		atomic_inc(&ubuf->retirement->vmas);
}
static void udmabuf_vma_close(struct vm_area_struct *vma)
{
	struct udmabuf *ubuf = vma->vm_private_data;
	if (ubuf->retirement)
		atomic_dec(&ubuf->retirement->vmas);
}
static int udmabuf_protected_attach(struct dma_buf *buf, struct dma_buf_attachment *at)
{
	struct udmabuf *ubuf = buf->priv;
	/* Generic dma-buf import has no immutable GPU read-only contract.
	 * DMA_TO_DEVICE alone is not proof of device write protection. */
	if (udmabuf_is_readonly(ubuf))
		return -EACCES;
	if (ubuf->retirement)
		atomic_inc(&ubuf->retirement->attachments);
	return 0;
}
static void udmabuf_protected_detach(struct dma_buf *buf, struct dma_buf_attachment *at)
{
	struct udmabuf *ubuf = buf->priv;
	if (ubuf->retirement)
		atomic_dec(&ubuf->retirement->attachments);
}

/* All file/VMA/importer refs have ended before this work is queued. Retain
 * independently referenced reservation fences until they all signal. A fence
 * error is still completion; an interrupted/failed wait is not completion. */
static void udmabuf_finish_retirement(struct work_struct *work)
{
	struct udmabuf_retirement *r = container_of(work, struct udmabuf_retirement, work);
	struct udmabuf *ubuf = r->ubuf;
	unsigned int i;
	bool failed = false;
	for (i = 0; i < r->fence_count; i++) {
		if (dma_fence_wait(r->fences[i], false) < 0)
			failed = true;
		dma_fence_put(r->fences[i]);
	}
	kfree(r->fences);
	r->fences = NULL;
	if (failed) {
		/* Keep the pins and the buffer's monitor reference permanently. */
		udmabuf_set_retirement(r, UDMABUF_PROTECTED_FAILED);
		return;
	}
	udmabuf_cleanup_cpu(ubuf);
	deinit_udmabuf(ubuf);
	kfree(ubuf);
	r->ubuf = NULL;
	/* Publication follows actual cleanup and unpin, not a client close. */
	udmabuf_set_retirement(r, UDMABUF_PROTECTED_RETIRED);
	udmabuf_retirement_put(r);
}

static void udmabuf_retire_buffer(struct dma_buf *buf)
{
	struct udmabuf *ubuf = buf->priv;
	struct udmabuf_retirement *r = ubuf->retirement;
	int ret;
	r->ubuf = ubuf;
	/* These counters are additional fail-closed invariants, not a replacement
	 * for the core's file refs or the importer's required dma_buf_get ref. */
	if (atomic_read(&r->vmas) || atomic_read(&r->attachments) ||
	    atomic_read(&r->maps) || atomic_read(&r->vmaps) ||
	    !list_empty(&buf->attachments) || buf->vmapping_counter) {
		udmabuf_set_retirement(r, UDMABUF_PROTECTED_FAILED);
		return;
	}
	ret = dma_resv_get_fences(buf->resv, DMA_RESV_USAGE_BOOKKEEP,
				 &r->fence_count, &r->fences);
	if (ret) {
		udmabuf_set_retirement(r, UDMABUF_PROTECTED_FAILED);
		return;
	}
	smp_store_release(&r->info.state, UDMABUF_PROTECTED_DRAINING);
	queue_work(udmabuf_retire_wq, &r->work);
}

static __poll_t udmabuf_monitor_poll(struct file *file, poll_table *wait)
{
	struct udmabuf_retirement *r = file->private_data;
	u32 state;
	poll_wait(file, &r->wait, wait);
	state = smp_load_acquire(&r->info.state);
	if (state == UDMABUF_PROTECTED_RETIRED)
		return EPOLLIN | EPOLLRDNORM;
	if (state == UDMABUF_PROTECTED_FAILED)
		return EPOLLERR;
	return 0;
}
static long udmabuf_monitor_ioctl(struct file *file, unsigned int cmd, unsigned long arg)
{
	struct udmabuf_retirement *r = file->private_data;
	void __user *user = (void __user *)arg;
	if (cmd == UDMABUF_PROTECTED_INFO) {
		struct udmabuf_protected_info info = {
			.version = r->info.version, .flags = r->info.flags,
			.state = smp_load_acquire(&r->info.state), .count = r->info.count,
			.export_id = r->info.export_id, .source_dev = r->info.source_dev,
			.source_ino = r->info.source_ino, .source_bytes = r->info.source_bytes,
			.bytes = r->info.bytes,
		};
		memcpy(info.instance, r->info.instance, sizeof(info.instance));
		return copy_to_user(user, &info, sizeof(info)) ? -EFAULT : 0;
	}
	if (cmd == UDMABUF_PROTECTED_RANGE) {
		struct udmabuf_protected_query_range query;
		if (copy_from_user(&query, user, sizeof(query)))
			return -EFAULT;
		if (query.reserved || query.index >= r->info.count)
			return -EINVAL;
		query.range = r->ranges[query.index];
		return copy_to_user(user, &query, sizeof(query)) ? -EFAULT : 0;
	}
	return -ENOTTY;
}
static int udmabuf_monitor_release(struct inode *inode, struct file *file)
{
	udmabuf_retirement_put(file->private_data);
	return 0;
}
static const struct file_operations udmabuf_monitor_fops = {
	.owner = THIS_MODULE,
	.poll = udmabuf_monitor_poll,
	.unlocked_ioctl = udmabuf_monitor_ioctl,
#ifdef CONFIG_COMPAT
	.compat_ioctl = udmabuf_monitor_ioctl,
#endif
	.release = udmabuf_monitor_release,
};

static long udmabuf_ioctl_protected(struct file *filp, unsigned long arg)
{
	struct udmabuf_protected_create create;
	struct udmabuf_create_list head;
	struct udmabuf_create_item *items = NULL;
	struct udmabuf_retirement *r;
	struct dma_buf *dmabuf = NULL;
	struct file *monitor = NULL, *source;
	long ret = -EINVAL;
	s64 old, id;
	int data_fd = -1, monitor_fd = -1;
	u32 i;
	/* Raw resource operations remain privileged in the initial user namespace. */
	if (!ns_capable(&init_user_ns, CAP_SYS_ADMIN))
		return -EPERM;
	if (copy_from_user(&create, (void __user *)arg, sizeof(create)))
		return -EFAULT;
	if (create.version != UDMABUF_PROTECTED_VERSION || create.reserved ||
	    (create.flags & ~UDMABUF_PROTECTED_READONLY) || !create.count ||
	    create.count > list_limit || create.data_fd != -1 || create.monitor_fd != -1)
		return -EINVAL;
	source = fget(create.memfd);
	if (!source)
		return -EBADF;
	if (!(source->f_mode & FMODE_READ) ||
	    (!(create.flags & UDMABUF_PROTECTED_READONLY) && !(source->f_mode & FMODE_WRITE))) {
		fput(source);
		return -EACCES;
	}
	r = kzalloc(sizeof(*r), GFP_KERNEL);
	if (!r) { fput(source); return -ENOMEM; }
	kref_init(&r->refs);
	r->source = source;
	init_waitqueue_head(&r->wait);
	INIT_WORK(&r->work, udmabuf_finish_retirement);
	r->ranges = memdup_array_user(u64_to_user_ptr(create.ranges),
				     create.count, sizeof(*r->ranges));
	if (IS_ERR(r->ranges)) {
		ret = PTR_ERR(r->ranges);
		r->ranges = NULL;
		goto out;
	}
	items = kcalloc(create.count, sizeof(*items), GFP_KERNEL);
	if (!items) { ret = -ENOMEM; goto out; }
	r->info.source_bytes = i_size_read(file_inode(source));
	for (i = 0; i < create.count; i++) {
		items[i].memfd = create.memfd;
		items[i].offset = r->ranges[i].offset;
		items[i].size = r->ranges[i].size;
		/* Check signed loff_t endpoints before the upstream pinning helper. */
		if (!items[i].size || items[i].offset > r->info.source_bytes ||
		    items[i].size > r->info.source_bytes - items[i].offset ||
		    items[i].offset > S64_MAX ||
		    items[i].size > S64_MAX - items[i].offset ||
		    check_add_overflow(r->info.bytes, items[i].size, &r->info.bytes))
			goto out;
	}
	old = atomic64_read(&udmabuf_export_id);
	do {
		if (old == S64_MAX) { ret = -EOVERFLOW; goto out; }
		id = old + 1;
	} while (!atomic64_try_cmpxchg(&udmabuf_export_id, &old, id));
	r->info.version = UDMABUF_PROTECTED_VERSION;
	r->info.flags = create.flags;
	r->info.count = create.count;
	r->info.export_id = id;
	r->info.source_dev = huge_encode_dev(file_inode(source)->i_sb->s_dev);
	r->info.source_ino = file_inode(source)->i_ino;
	memcpy(r->info.instance, udmabuf_instance, sizeof(r->info.instance));
	head.flags = UDMABUF_FLAGS_CLOEXEC;
	head.count = create.count;
	dmabuf = udmabuf_create_buffer(filp->private_data, &head, items, r, source);
	if (IS_ERR(dmabuf)) { ret = PTR_ERR(dmabuf); dmabuf = NULL; goto out; }
	monitor = anon_inode_getfile("udmabuf-retirement", &udmabuf_monitor_fops, r,
				     O_RDONLY | O_CLOEXEC);
	if (IS_ERR(monitor)) { ret = PTR_ERR(monitor); monitor = NULL; goto out; }
	/* The monitor file now owns the initial state reference. */
	r = NULL;
	data_fd = get_unused_fd_flags(O_CLOEXEC);
	if (data_fd < 0) { ret = data_fd; goto out; }
	monitor_fd = get_unused_fd_flags(O_CLOEXEC);
	if (monitor_fd < 0) { ret = monitor_fd; goto out; }
	create.data_fd = data_fd;
	create.monitor_fd = monitor_fd;
	if (copy_to_user((void __user *)arg, &create, sizeof(create))) {
		ret = -EFAULT;
		goto out;
	}
	/* Both file descriptors become visible only after successful reply copy. */
	fd_install(data_fd, dmabuf->file);
	fd_install(monitor_fd, monitor);
	kfree(items);
	return 0;
out:
	if (monitor_fd >= 0)
		put_unused_fd(monitor_fd);
	if (data_fd >= 0)
		put_unused_fd(data_fd);
	if (monitor)
		fput(monitor);
	if (dmabuf)
		dma_buf_put(dmabuf);
	udmabuf_retirement_put(r);
	kfree(items);
	return ret;
}

/* Same seal lookup as mm/memfd.c's memfd_file_seals_ptr/memfd_get_seals,
 * pinned to this kernel. memfd_fcntl is built-in-only, not an exported module
 * symbol. The existing caller holds inode_lock_shared across this check and
 * pinning, so seal-change exclusion is preserved. */
static int udmabuf_protected_get_seals(struct file *file)
{
	if (shmem_file(file))
		return SHMEM_I(file_inode(file))->seals;
#ifdef CONFIG_HUGETLBFS
	if (is_file_hugepages(file))
		return HUGETLBFS_I(file_inode(file))->seals;
#endif
	return -EINVAL;
}
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Experimental protected range and retirement udmabuf overlay");
MODULE_IMPORT_NS("DMA_BUF");
