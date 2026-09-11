// SPDX-License-Identifier: GPL-2.0
/*
 * inode.c - Superblock and inode management for KestrelFS Phase 1.
 *
 * This builds a minimal, purely in-memory read-only tree:
 *
 *     /               (root directory, S_IFDIR)
 *     /hello.txt      (regular file, S_IFREG, served by file.c)
 *
 * No on-disk format exists yet: everything here is a VFS skeleton
 * exercised via simple_fill_super() + tree_descr, exactly like
 * ramfs/kernel's "simplefs" example filesystems. Real chunk/metadata
 * logic will be layered in during later phases via the Rust control
 * plane.
 */

#include <linux/fs.h>
#include <linux/module.h>
#include <linux/pagemap.h>
#include <linux/statfs.h>

#include "kestrelfs.h"

/*
 * kestrelfs_statfs() - report basic filesystem statistics.
 *
 * We currently have no notion of real capacity (no NVMe cache, no S3
 * accounting yet), so we simply delegate to simple_statfs() which
 * reports the values simple_fill_super() already populated into the
 * superblock (block size, name length, etc).
 */
static int kestrelfs_statfs(struct dentry *dentry, struct kstatfs *buf)
{
	return simple_statfs(dentry, buf);
}

const struct super_operations kestrelfs_super_ops = {
	.statfs		= kestrelfs_statfs,
	.drop_inode	= generic_delete_inode,
};

/*
 * kestrelfs_fill_super() - populate a freshly allocated superblock.
 * @sb:		superblock to fill in.
 * @data:	mount data (unused in Phase 1).
 * @silent:	unused.
 *
 * Builds the tiny static tree_descr table describing our single
 * regular file and hands it to simple_fill_super(), which takes care
 * of allocating the root inode/dentry and every entry in the table.
 *
 * Return: 0 on success, negative errno on failure.
 */
static int kestrelfs_fill_super(struct super_block *sb, void *data, int silent)
{
	static struct tree_descr kestrelfs_files[] = {
		{ "hello.txt", &kestrelfs_file_ops, S_IRUGO },
		{ "" },
	};
	int ret;

	ret = simple_fill_super(sb, KESTRELFS_MAGIC, kestrelfs_files);
	if (ret) {
		pr_err("kestrelfs: simple_fill_super failed: %d\n", ret);
		return ret;
	}

	/*
	 * simple_fill_super() already set sb->s_op to its own default
	 * simple_super_operations, so ours must be installed afterwards.
	 */
	sb->s_op = &kestrelfs_super_ops;

	pr_info("kestrelfs: superblock populated (root + hello.txt)\n");
	return 0;
}

/*
 * kestrelfs_mount() - fs_type->mount() callback.
 *
 * mount_nodev() is appropriate here because KestrelFS Phase 1 has no
 * backing block device: the entire tree lives in page cache-backed
 * inodes allocated by simple_fill_super().
 */
static struct dentry *kestrelfs_mount(struct file_system_type *fs_type,
				       int flags, const char *dev_name,
				       void *data)
{
	return mount_nodev(fs_type, flags, data, kestrelfs_fill_super);
}

struct file_system_type kestrelfs_fs_type = {
	.owner		= THIS_MODULE,
	.name		= KESTRELFS_NAME,
	.mount		= kestrelfs_mount,
	.kill_sb	= kill_litter_super,
	.fs_flags	= FS_USERNS_MOUNT,
};
