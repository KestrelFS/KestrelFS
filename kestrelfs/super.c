// SPDX-License-Identifier: GPL-2.0
/*
 * super.c - Module init/exit and filesystem_type registration for
 * KestrelFS Phase 1.
 *
 * This is the only file that touches module load/unload machinery.
 * Everything else (super_operations, file_operations, fill_super) is
 * defined in inode.c / file.c and simply wired together here via
 * register_filesystem()/unregister_filesystem().
 */

#include <linux/fs.h>
#include <linux/init.h>
#include <linux/module.h>

#include "kestrelfs.h"

/*
 * kestrelfs_init() - module load entry point.
 *
 * Registers the "kestrelfs" filesystem type with the VFS so that
 * `mount -t kestrelfs none /mnt` becomes available, and brings up
 * the /dev/kestrel_ctl char device (Phase 2 IPC bridge to the Rust
 * control-plane daemon). No NVMe device is touched in Phase 1/2.
 *
 * Return: 0 on success, negative errno propagated from
 * register_filesystem()/kestrelfs_chardev_init() on failure.
 */
static int __init kestrelfs_init(void)
{
	int ret;

	ret = kestrelfs_cache_init();
	if (ret)
		return ret;

	ret = register_filesystem(&kestrelfs_fs_type);
	if (ret) {
		pr_err("kestrelfs: register_filesystem failed: %d\n", ret);
		goto err_cache;
	}

	ret = kestrelfs_chardev_init();
	if (ret) {
		pr_err("kestrelfs: chardev init failed: %d\n", ret);
		unregister_filesystem(&kestrelfs_fs_type);
		goto err_cache;
	}

	kestrelfs_ipc_ring_init();

	pr_info("kestrelfs: module loaded, filesystem + chardev + cache skeleton registered\n");
	return 0;

err_cache:
	kestrelfs_cache_exit();
	return ret;
}

/*
 * kestrelfs_exit() - module unload entry point.
 *
 * Tears down the char device first (this blocks/fails via module
 * refcounting until every /dev/kestrel_ctl fd is closed - see the
 * comment on kestrelfs_chardev_exit() in chardev.c), then
 * unregisters the filesystem type. The kernel additionally
 * guarantees unregister_filesystem() is only reachable once every
 * active mount has already been released (module refcounting via
 * fs_type->owner prevents unload while any KestrelFS instance is
 * mounted).
 */
static void __exit kestrelfs_exit(void)
{
	kestrelfs_ipc_ring_exit();
	kestrelfs_chardev_exit();
	unregister_filesystem(&kestrelfs_fs_type);
	kestrelfs_cache_exit();
	pr_info("kestrelfs: module unloaded\n");
}

module_init(kestrelfs_init);
module_exit(kestrelfs_exit);

MODULE_LICENSE("GPL");
MODULE_AUTHOR("KestrelFS Project");
MODULE_DESCRIPTION("KestrelFS VFS, Rust IPC bridge, and NVMe cache skeleton");
MODULE_VERSION("0.4.0-step18");
