// SPDX-License-Identifier: GPL-2.0
/*
 * inode.c - Superblock and inode management for KestrelFS Phase 1.
 *
 * This builds a minimal, mostly in-memory tree:
 *
 *     /               (root directory, S_IFDIR)
 *     /hello.txt      (regular file, S_IFREG, served by file.c - pure
 *                      in-memory content, no IPC dependency)
 *     /remote.txt      (regular file, S_IFREG, served by file.c - every
 *                      read round-trips through the Phase 2
 *                      kernel<->Rust IPC bridge)
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
#include <linux/namei.h>
#include <linux/dcache.h>

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
 * kestrelfs_fixup_remote_size() - set remote.txt's reported i_size.
 * @sb:		superblock, already populated by simple_fill_super().
 *
 * simple_fill_super() has no concept of per-file size - every inode
 * it creates is left at i_size == 0 (see fs/libfs.c). That is
 * harmless for hello.txt (its length is implicitly bounded by
 * simple_read_from_buffer()'s own @len argument, never by i_size),
 * but for remote.txt it means stat()/ls -l would misleadingly report
 * "0 bytes" for a file that kestrelfs_remote_read() (file.c) actually
 * serves KESTRELFS_REMOTE_FILE_SIZE bytes from. This looks up the
 * freshly created remote.txt dentry directly under the root (no
 * subdirectories exist in this filesystem, so a single
 * lookup_one_len_unlocked() suffices - no need for a generic
 * recursive tree walk) and corrects its inode's i_size to match.
 *
 * Called once, right after simple_fill_super() succeeds, while no
 * other thread can yet be operating on this superblock (the VFS does
 * not publish a superblock/mount to any other context until
 * ->mount() returns) - so no additional inode locking around the
 * lookup itself is required here despite lookup_one_len() normally
 * asserting the parent is locked.
 *
 * Failure to find remote.txt (e.g. if the tree_descr table above is
 * ever changed to rename/remove it) is logged but not fatal - a
 * missing size fixup only affects stat() cosmetics, never read()
 * correctness (see kestrelfs_remote_read()'s own independent EOF
 * check against KESTRELFS_REMOTE_FILE_SIZE), so it must not fail the
 * entire mount.
 */
static void kestrelfs_fixup_remote_size(struct super_block *sb)
{
	struct dentry *dentry;

	dentry = lookup_one_len_unlocked("remote.txt", sb->s_root,
					  strlen("remote.txt"));
	if (IS_ERR(dentry)) {
		pr_warn("kestrelfs: could not look up remote.txt to fix up i_size: %ld\n",
			PTR_ERR(dentry));
		return;
	}

	if (!dentry->d_inode) {
		pr_warn("kestrelfs: remote.txt dentry has no inode, skipping i_size fixup\n");
		dput(dentry);
		return;
	}

	i_size_write(dentry->d_inode, KESTRELFS_REMOTE_FILE_SIZE);
	dput(dentry);
}

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
		{ "remote.txt", &kestrelfs_remote_file_ops, S_IRUGO },
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

	kestrelfs_fixup_remote_size(sb);

	pr_info("kestrelfs: superblock populated (root + hello.txt + remote.txt)\n");
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
