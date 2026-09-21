// SPDX-License-Identifier: GPL-2.0
/*
 * chardev.c - /dev/kestrel_ctl misc device: the Phase 2 IPC bridge
 * between the KestrelFS kernel module (data plane) and the userspace
 * Rust daemon (control plane).
 *
 * This file only implements the *infrastructure*: allocation of the
 * shared mmap'able region, exposing it to userspace via .mmap, and
 * the two wait queues used to put both sides to sleep instead of
 * busy-spinning on the ring buffers. No VFS-facing push/pop business
 * logic lives here yet - that arrives once inode.c/file.c actually
 * need to issue requests through this bridge (later Phase 2 steps /
 * Phase 3).
 *
 * Exported API (see kestrelfs.h):
 *   - kestrelfs_chardev_init() / kestrelfs_chardev_exit()
 *       Module-level lifecycle, called from super.c's module_init/exit.
 *   - kestrelfs_shm_region()
 *       Returns the kernel VA of the shared region for any future
 *       in-tree producer/consumer code (e.g. inode.c pushing a
 *       KESTRELFS_OP_LOOKUP request).
 *   - kestrelfs_wake_req_waiters() / kestrelfs_wait_for_resp()
 *       Thin wrappers around the two wait queues, exported so later
 *       phases can signal "a request was published" or block until
 *       "a response arrived" without duplicating waitqueue plumbing.
 */

#include <linux/module.h>
#include <linux/fs.h>
#include <linux/miscdevice.h>
#include <linux/mm.h>
#include <linux/vmalloc.h>
#include <linux/poll.h>
#include <linux/wait.h>
#include <linux/uaccess.h>
#include <linux/slab.h>

#include "kestrelfs.h"
#include "kestrelfs_ipc.h"

/*
 * kestrelfs_shm - singleton allocation backing /dev/kestrel_ctl.
 *
 * @region:    kernel virtual address of the vmalloc_user()-allocated
 *             struct kestrelfs_shared_region. Allocated eagerly in
 *             kestrelfs_chardev_init() (module load time), so it is
 *             non-NULL for the entire lifetime of the module once
 *             loaded successfully; freed on module exit.
 * @refcount:  number of currently open file descriptors on
 *             /dev/kestrel_ctl. Guarded by @lock. Purely diagnostic
 *             bookkeeping in the current design - since @region's
 *             lifetime is tied to the module rather than to any
 *             open fd, nothing currently branches on this value, but
 *             it is kept for future use (e.g. refusing a second
 *             daemon instance, or diagnostics).
 * @lock:      protects @region and @refcount against concurrent
 *             open()/release().
 * @req_wq:    kernel producer -> Rust consumer signalling. Rust
 *             blocks here (via poll()/epoll on the fd) until the
 *             kernel calls kestrelfs_wake_req_waiters().
 * @resp_wq:   Rust producer -> kernel consumer signalling. Kernel
 *             threads block here (via kestrelfs_wait_for_resp())
 *             until Rust issues KESTRELFS_IOC_NOTIFY_RESP.
 * @resp_generation: bumped every time KESTRELFS_IOC_NOTIFY_RESP
 *             fires. Lets kestrelfs_wait_for_resp() use
 *             wait_event_interruptible() with a real progress
 *             condition instead of waking spuriously on every
 *             notification regardless of relevance.
 */
struct kestrelfs_shm {
	struct kestrelfs_shared_region	*region;
	int				refcount;
	struct mutex			lock;

	wait_queue_head_t		req_wq;
	wait_queue_head_t		resp_wq;
	atomic_t			resp_generation;
	atomic_t			daemon_alive;  /* 1 if daemon attached, 0 if detached */
};

static struct kestrelfs_shm kestrelfs_shm = {
	.region	= NULL,
	.refcount = 0,
};

/*
 * kestrelfs_shm_alloc() - allocate and initialize the shared region.
 *
 * Uses vmalloc_user() rather than plain vmalloc(): the _user variant
 * both zero-fills the memory and tags the resulting vm_struct with
 * VM_USERMAP, which remap_vmalloc_range() requires in order to allow
 * mapping this allocation into a userspace VMA later.
 *
 * Return: 0 on success, negative errno on failure. Idempotent: if
 * kestrelfs_shm.region is already set, returns 0 immediately.
 */
static int kestrelfs_shm_alloc(void)
{
	struct kestrelfs_shared_region *region;

	if (kestrelfs_shm.region)
		return 0;

	region = vmalloc_user(KESTRELFS_SHM_REGION_SIZE);
	if (!region) {
		pr_err("kestrelfs: failed to allocate %zu bytes for shared region\n",
		       (size_t)KESTRELFS_SHM_REGION_SIZE);
		return -ENOMEM;
	}

	region->magic = KESTRELFS_SHM_MAGIC;
	region->abi_version = KESTRELFS_ABI_VERSION;

	region->req_ctrl.head = 0;
	region->req_ctrl.tail = 0;
	region->req_ctrl.capacity = KESTRELFS_RING_SLOTS;

	region->resp_ctrl.head = 0;
	region->resp_ctrl.tail = 0;
	region->resp_ctrl.capacity = KESTRELFS_RING_SLOTS;

	kestrelfs_shm.region = region;

	pr_info("kestrelfs: chardev shared region allocated (%zu bytes, %d slots/ring)\n",
		(size_t)KESTRELFS_SHM_REGION_SIZE, KESTRELFS_RING_SLOTS);
	return 0;
}

/*
 * kestrelfs_shm_free() - release the shared region, if allocated.
 *
 * Only called from module exit. Safe to call even if allocation
 * never happened (region == NULL).
 */
static void kestrelfs_shm_free(void)
{
	if (!kestrelfs_shm.region)
		return;

	vfree(kestrelfs_shm.region);
	kestrelfs_shm.region = NULL;

	pr_info("kestrelfs: chardev shared region freed\n");
}

/*
 * kestrelfs_shm_region() - accessor for the shared region's kernel VA.
 *
 * Exported so future in-tree code (VFS glue in inode.c/file.c) can
 * push requests into req_slots[] / read resp_slots[] without this
 * file needing to know anything about ring push/pop semantics.
 *
 * Return: kernel VA of the region, or NULL if never allocated (i.e.
 * /dev/kestrel_ctl has never been opened).
 */
struct kestrelfs_shared_region *kestrelfs_shm_region(void)
{
	return kestrelfs_shm.region;
}
EXPORT_SYMBOL_GPL(kestrelfs_shm_region);

/*
 * kestrelfs_wake_req_waiters() - signal that new REQ events exist.
 *
 * Call this after publishing one or more entries into
 * region->req_slots[] and advancing region->req_ctrl.head. Wakes any
 * Rust daemon thread blocked in poll()/epoll_wait() on the char
 * device fd (POLLIN becomes ready via kestrelfs_poll() below).
 *
 * Safe to call even if nobody is currently waiting (wake_up on an
 * empty wait queue is a cheap no-op).
 */
void kestrelfs_wake_req_waiters(void)
{
	wake_up_interruptible(&kestrelfs_shm.req_wq);
}
EXPORT_SYMBOL_GPL(kestrelfs_wake_req_waiters);

/*
 * kestrelfs_wait_for_resp() - block until Rust notifies of new
 * RESP events, or @timeout_jiffies elapses, or a signal arrives.
 * @timeout_jiffies: maximum time to sleep, in jiffies. Pass
 *                   MAX_SCHEDULE_TIMEOUT to wait indefinitely.
 *
 * Intended for future kernel-side callers (e.g. a VFS ->read() that
 * issued a KESTRELFS_OP_READ_CHUNK request and must block until the
 * matching response lands in resp_slots[]). This helper only
 * provides the generic "something new arrived" wakeup; matching the
 * response back to a specific req_id is the caller's responsibility
 * once that logic is introduced.
 *
 * Return: 0 if woken by a notification, -ETIME if the timeout
 * elapsed, -ERESTARTSYS if interrupted by a signal.
 */
long kestrelfs_wait_for_resp(long timeout_jiffies)
{
	int generation = atomic_read(&kestrelfs_shm.resp_generation);
	long ret;

	ret = wait_event_interruptible_timeout(
		kestrelfs_shm.resp_wq,
		atomic_read(&kestrelfs_shm.resp_generation) != generation,
		timeout_jiffies);

	if (ret < 0)
		return ret;		/* -ERESTARTSYS */
	if (ret == 0)
		return -ETIME;
	return 0;
}
EXPORT_SYMBOL_GPL(kestrelfs_wait_for_resp);

/*
 * kestrelfs_is_daemon_alive() - check if daemon is currently attached.
 *
 * Returns 1 if daemon has /dev/kestrel_ctl open, 0 otherwise.
 * Used by IPC sync helpers to fail fast when daemon has disconnected.
 */
int kestrelfs_is_daemon_alive(void)
{
	return atomic_read(&kestrelfs_shm.daemon_alive);
}
EXPORT_SYMBOL_GPL(kestrelfs_is_daemon_alive);

/*
 * kestrelfs_open() - fops->open for /dev/kestrel_ctl.
 *
 * The shared region is now allocated eagerly at module load time
 * (see kestrelfs_chardev_init()), not on first open - this lets
 * in-tree kernel producers (kestrelfs_req_push() in ipc_ring.c, and
 * therefore the debugfs self-test) use the ring buffers immediately
 * after insmod, without requiring userspace to have opened
 * /dev/kestrel_ctl first. open() here therefore only bumps the
 * refcount for bookkeeping/diagnostics purposes.
 */
static int kestrelfs_open(struct inode *inode, struct file *file)
{
	mutex_lock(&kestrelfs_shm.lock);
	kestrelfs_shm.refcount++;
	atomic_set(&kestrelfs_shm.daemon_alive, 1);  /* Daemon attached */
	mutex_unlock(&kestrelfs_shm.lock);

	pr_info("kestrelfs: daemon attached (refcount=%d)\n", kestrelfs_shm.refcount);
	return 0;
}

/*
 * kestrelfs_release() - fops->release for /dev/kestrel_ctl.
 *
 * Decrements the refcount. Deliberately does NOT free the region
 * even when refcount reaches zero: the allocation is reused across
 * daemon restarts and is only ever released at module unload, which
 * keeps this path free of use-after-free risk against any stray
 * mapping.
 *
 * IMPORTANT: When the daemon disconnects, we mark daemon_alive=0 and
 * wake all waiting VFS threads to prevent hanging umount.
 */
static int kestrelfs_release(struct inode *inode, struct file *file)
{
	mutex_lock(&kestrelfs_shm.lock);
	if (kestrelfs_shm.refcount > 0)
		kestrelfs_shm.refcount--;
	
	/* Mark daemon as dead if last fd closed */
	if (kestrelfs_shm.refcount == 0) {
		atomic_set(&kestrelfs_shm.daemon_alive, 0);
		pr_warn("kestrelfs: daemon detached, waking all waiters\n");
		
		/* Wake all threads waiting for responses (prevent hanging umount) */
		atomic_inc(&kestrelfs_shm.resp_generation);
		wake_up_interruptible_all(&kestrelfs_shm.resp_wq);
	}
	mutex_unlock(&kestrelfs_shm.lock);

	return 0;
}

/*
 * kestrelfs_mmap() - fops->mmap for /dev/kestrel_ctl.
 *
 * Maps the entire shared region into the calling process' address
 * space via remap_vmalloc_range(). The requested mapping length must
 * exactly match KESTRELFS_SHM_REGION_SIZE - partial or oversized
 * mappings are rejected outright since the ring buffer layout is not
 * meaningful at any other size.
 *
 * VM_DONTEXPAND / VM_DONTDUMP are applied: this region must never be
 * grown via mremap() (the ring layout is fixed-size) and has no
 * business appearing in core dumps.
 */
static int kestrelfs_mmap(struct file *file, struct vm_area_struct *vma)
{
	unsigned long requested_size = vma->vm_end - vma->vm_start;
	int ret;

	if (!kestrelfs_shm.region) {
		pr_err("kestrelfs: mmap attempted before shared region allocation\n");
		return -EINVAL;
	}

	if (vma->vm_pgoff != 0) {
		pr_err("kestrelfs: mmap rejected, nonzero pgoff unsupported\n");
		return -EINVAL;
	}

	if (requested_size != PAGE_ALIGN(KESTRELFS_SHM_REGION_SIZE)) {
		pr_err("kestrelfs: mmap size mismatch: requested %lu, expected %lu\n",
		       requested_size,
		       PAGE_ALIGN((unsigned long)KESTRELFS_SHM_REGION_SIZE));
		return -EINVAL;
	}

	vm_flags_set(vma, VM_DONTEXPAND | VM_DONTDUMP);

	ret = remap_vmalloc_range(vma, kestrelfs_shm.region, 0);
	if (ret) {
		pr_err("kestrelfs: remap_vmalloc_range failed: %d\n", ret);
		return ret;
	}

	return 0;
}

/*
 * kestrelfs_poll() - fops->poll for /dev/kestrel_ctl.
 *
 * The Rust daemon calls epoll_wait()/poll() on this fd to sleep
 * until new REQ events exist instead of busy-spinning on
 * region->req_ctrl.head/tail. poll_wait() registers the calling
 * task on req_wq; kestrelfs_wake_req_waiters() (called by future
 * in-tree producers) wakes it back up.
 *
 * We report POLLIN/POLLRDNORM readiness whenever req_ctrl.head !=
 * req_ctrl.tail, i.e. there is at least one unconsumed REQ slot.
 * This is a plain READ_ONCE-style peek for readiness reporting only;
 * the actual consume protocol (with proper acquire/release ordering)
 * lives on the Rust side once it starts draining the ring.
 */
static __poll_t kestrelfs_poll(struct file *file, poll_table *wait)
{
	__poll_t mask = 0;

	poll_wait(file, &kestrelfs_shm.req_wq, wait);

	if (kestrelfs_shm.region) {
		u64 head = READ_ONCE(kestrelfs_shm.region->req_ctrl.head);
		u64 tail = READ_ONCE(kestrelfs_shm.region->req_ctrl.tail);

		if (head != tail)
			mask |= EPOLLIN | EPOLLRDNORM;
	}

	return mask;
}

/*
 * kestrelfs_ioctl() - fops->unlocked_ioctl for /dev/kestrel_ctl.
 *
 * KESTRELFS_IOC_NOTIFY_RESP:
 *     Rust daemon -> kernel notification that new entries were
 *     published into resp_slots[] and resp_ctrl.head advanced.
 *     Bumps resp_generation and wakes every kernel thread parked in
 *     kestrelfs_wait_for_resp().
 *
 * KESTRELFS_IOC_GET_ABI_VERSION:
 *     Read back KESTRELFS_ABI_VERSION so userspace can refuse to
 *     proceed on a protocol mismatch before touching the mapped
 *     region at all.
 *
 * KESTRELFS_IOC_GET_REGION_SIZE:
 *     Read back KESTRELFS_SHM_REGION_SIZE so userspace can size its
 *     mmap() call without hardcoding the constant redundantly.
 *
 * KESTRELFS_IOC_INVALIDATE_CACHE_ALL:
 *     Persistently retire all local cache entries after the daemon observes
 *     a shared metadata revision change.
 *
 * KESTRELFS_IOC_INVALIDATE_CACHE_INODES:
 *     Persistently retire a bounded list of dirty inode cache entries after
 *     the daemon enumerates a shared metadata revision range.
 */
static long kestrelfs_ioctl(struct file *file, unsigned int cmd,
			     unsigned long arg)
{
	int ret;

	switch (cmd) {
	case KESTRELFS_IOC_NOTIFY_RESP: {
		atomic_inc(&kestrelfs_shm.resp_generation);
		wake_up_all(&kestrelfs_shm.resp_wq);
		return 0;
	}

	case KESTRELFS_IOC_GET_ABI_VERSION: {
		__u32 version = KESTRELFS_ABI_VERSION;

		if (copy_to_user((void __user *)arg, &version, sizeof(version)))
			return -EFAULT;
		return 0;
	}

	case KESTRELFS_IOC_GET_REGION_SIZE: {
		__u64 size = KESTRELFS_SHM_REGION_SIZE;

		if (copy_to_user((void __user *)arg, &size, sizeof(size)))
			return -EFAULT;
		return 0;
	}

	case KESTRELFS_IOC_INVALIDATE_CACHE_ALL:
		ret = kestrelfs_cache_invalidate_all();
		kestrelfs_pagecache_coherence_advance();
		return ret;

	case KESTRELFS_IOC_INVALIDATE_CACHE_INODES: {
		struct kestrelfs_cache_invalidate_inodes request;

		if (copy_from_user(&request, (void __user *)arg,
				   sizeof(request)))
			return -EFAULT;
		if (!request.count ||
		    request.count > KESTRELFS_CACHE_INVALIDATE_INODES_MAX ||
		    request.reserved)
			return -EINVAL;
		ret = kestrelfs_cache_invalidate_inodes(request.inode_ids,
						request.count);
		kestrelfs_pagecache_coherence_advance();
		return ret;
	}

	case KESTRELFS_IOC_PEEK_ORPHAN_RETRY: {
		__u64 inode_id;

		ret = kestrelfs_orphan_retry_peek(&inode_id);
		if (ret)
			return ret;
		if (copy_to_user((void __user *)arg, &inode_id,
				 sizeof(inode_id)))
			return -EFAULT;
		return 0;
	}

	case KESTRELFS_IOC_ACK_ORPHAN_RETRY: {
		__u64 inode_id;

		if (copy_from_user(&inode_id, (void __user *)arg,
				   sizeof(inode_id)))
			return -EFAULT;
		return kestrelfs_orphan_retry_ack(inode_id);
	}

	default:
		return -ENOTTY;
	}
}

static const struct file_operations kestrelfs_chardev_fops = {
	.owner		= THIS_MODULE,
	.open		= kestrelfs_open,
	.release	= kestrelfs_release,
	.mmap		= kestrelfs_mmap,
	.poll		= kestrelfs_poll,
	.unlocked_ioctl	= kestrelfs_ioctl,
	.llseek		= noop_llseek,
};

static struct miscdevice kestrelfs_miscdev = {
	.minor	= MISC_DYNAMIC_MINOR,
	.name	= "kestrel_ctl",
	.fops	= &kestrelfs_chardev_fops,
	.mode	= 0666,
};

/*
 * kestrelfs_chardev_init() - register /dev/kestrel_ctl.
 *
 * Called from super.c's module_init. Eagerly allocates the shared
 * region (kestrelfs_shm_alloc()) BEFORE registering the misc device,
 * so that:
 *
 *   1. In-tree kernel producers (kestrelfs_req_push() in
 *      ipc_ring.c) can safely call kestrelfs_shm_region() and get a
 *      non-NULL pointer immediately after module load, without
 *      requiring userspace to open() the device first. This is what
 *      makes the debugfs self-test (and, later, real VFS call paths)
 *      usable standalone.
 *   2. By the time misc_register() makes /dev/kestrel_ctl visible to
 *      userspace, the region is already guaranteed to exist - no
 *      window where a fast userspace mmap() could race an
 *      allocation that hasn't happened yet.
 *
 * Return: 0 on success, negative errno from kestrelfs_shm_alloc() or
 * misc_register() on failure. On allocation failure the misc device
 * is never registered, so /dev/kestrel_ctl simply does not appear.
 */
int kestrelfs_chardev_init(void)
{
	int ret;

	mutex_init(&kestrelfs_shm.lock);
	init_waitqueue_head(&kestrelfs_shm.req_wq);
	init_waitqueue_head(&kestrelfs_shm.resp_wq);
	atomic_set(&kestrelfs_shm.resp_generation, 0);
	atomic_set(&kestrelfs_shm.daemon_alive, 0);  /* No daemon initially */

	ret = kestrelfs_shm_alloc();
	if (ret)
		return ret;

	ret = misc_register(&kestrelfs_miscdev);
	if (ret) {
		pr_err("kestrelfs: misc_register(kestrel_ctl) failed: %d\n", ret);
		kestrelfs_shm_free();
		return ret;
	}

	pr_info("kestrelfs: /dev/kestrel_ctl registered\n");
	return 0;
}

/*
 * kestrelfs_chardev_exit() - unregister /dev/kestrel_ctl and free
 * the shared region.
 *
 * Called from super.c's module_exit. Because kestrelfs_chardev_fops
 * sets .owner = THIS_MODULE, the VFS automatically holds a module
 * reference for as long as any fd on /dev/kestrel_ctl remains open;
 * rmmod fails with "module in use" until every fd is closed. By the
 * time this function runs we are therefore guaranteed no open fd
 * (and hence no live mmap mapping) references kestrelfs_shm.region,
 * making the vfree() in kestrelfs_shm_free() safe.
 */
void kestrelfs_chardev_exit(void)
{
	misc_deregister(&kestrelfs_miscdev);
	kestrelfs_shm_free();
	pr_info("kestrelfs: /dev/kestrel_ctl unregistered\n");
}
