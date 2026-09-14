// SPDX-License-Identifier: GPL-2.0
/*
 * cache.c - Phase 4 kernel-owned NVMe cache skeleton.
 *
 * Step 18 only reserves configuration and the read-path boundary.  It does
 * not open the block device, allocate an index, or issue block I/O.  Keeping
 * the hook in the kernel establishes the key invariant for later steps:
 * cache hits can be served without entering the daemon IPC path.
 */

#include <linux/errno.h>
#include <linux/fs.h>
#include <linux/module.h>
#include <linux/namei.h>

#include "kestrelfs.h"

static char *cache_device;
module_param(cache_device, charp, 0444);
MODULE_PARM_DESC(cache_device,
		 "cache block-device path (loop, zvol, or raw block device)");

static unsigned long cache_size_mib;
module_param(cache_size_mib, ulong, 0444);
MODULE_PARM_DESC(cache_size_mib,
		 "maximum cache size in MiB (0 means device capacity; reserved)");

/*
 * Validate only the type of a configured cache path.  Opening and claiming
 * the block device belongs to a later Phase 4 step, but rejecting regular
 * files now prevents a ZFS dataset file from becoming an accidental cache
 * device contract.
 */
int kestrelfs_cache_init(void)
{
	struct path path;
	int ret;

	if (!cache_device || !cache_device[0]) {
		pr_info("kestrelfs: NVMe cache skeleton disabled (no cache_device)\n");
		return 0;
	}

	ret = kern_path(cache_device, LOOKUP_FOLLOW, &path);
	if (ret) {
		pr_err("kestrelfs: cache device %s cannot be resolved: %d\n",
		       cache_device, ret);
		return ret;
	}

	if (!S_ISBLK(d_inode(path.dentry)->i_mode)) {
		pr_err("kestrelfs: cache_device must name a block device: %s\n",
		       cache_device);
		ret = -ENOTBLK;
	}
	path_put(&path);
	if (ret)
		return ret;

	pr_info("kestrelfs: NVMe cache skeleton configured device=%s size_mib=%lu (I/O disabled)\n",
		cache_device, cache_size_mib);
	return 0;
}

void kestrelfs_cache_exit(void)
{
	/* Step 18 owns no device handle, index, or cache memory. */
}

ssize_t kestrelfs_cache_lookup(struct inode *inode, char __user *buf,
			       size_t count, loff_t *ppos)
{
	/*
	 * Future implementation: resolve (inode->i_ino, *ppos) to an LBA,
	 * validate the cached generation/range, and serve @buf directly.
	 */
	(void)inode;
	(void)buf;
	(void)count;
	(void)ppos;
	return -ENODATA;
}
