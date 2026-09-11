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
 * kestrelfs_get_inode() - fetch or create an inode with given attributes.
 * @sb:		superblock
 * @ino:	inode number
 * @mode:	file type and permissions (S_IFDIR | 0755, S_IFREG | 0644, etc.)
 * @size:	file size in bytes
 *
 * Used by dir.c's lookup/create handlers to instantiate inodes dynamically.
 * For simplicity, we always create a new inode (no caching yet - can be
 * optimized with iget5_locked/ilookup if needed).
 *
 * Return: pointer to inode on success, ERR_PTR(-errno) on failure.
 */
struct inode *kestrelfs_get_inode(struct super_block *sb, u64 ino,
				  u32 mode, u64 size)
{
	struct inode *inode;

	inode = new_inode(sb);
	if (!inode)
		return ERR_PTR(-ENOMEM);

	inode->i_ino = ino;
	inode->i_mode = mode;
	inode->i_uid = GLOBAL_ROOT_UID;
	inode->i_gid = GLOBAL_ROOT_GID;
	inode->i_size = size;
	inode_set_atime_to_ts(inode, current_time(inode));
	inode_set_mtime_to_ts(inode, current_time(inode));
	inode_set_ctime_to_ts(inode, current_time(inode));

	if (S_ISDIR(mode)) {
		/* Directory */
		inode->i_op = &kestrelfs_dir_inode_operations;
		inode->i_fop = &kestrelfs_dir_file_operations;
		set_nlink(inode, 2);
	} else if (S_ISREG(mode)) {
		/* Regular file */
		inode->i_op = &kestrelfs_reg_inode_ops;
		inode->i_fop = &kestrelfs_reg_file_ops;
		set_nlink(inode, 1);
	} else {
		/* Unsupported file type */
		iput(inode);
		return ERR_PTR(-EINVAL);
	}

	return inode;
}

/*
 * kestrelfs_fill_super() - populate a freshly allocated superblock.
 * @sb:		superblock to fill in.
 * @data:	mount data (unused).
 * @silent:	unused.
 *
 * Phase 3 Step 7b: manually creates the root inode (i_ino=1) with
 * dynamic directory operations, replacing simple_fill_super().
 * All files (hello.txt, remote.txt, writable.dat) now appear via
 * dynamic LOOKUP from the daemon's MemStore, not static tree_descr.
 *
 * Return: 0 on success, negative errno on failure.
 */
static int kestrelfs_fill_super(struct super_block *sb, void *data, int silent)
{
	struct inode *root_inode;
	struct dentry *root_dentry;

	/* Set up superblock parameters */
	sb->s_magic = KESTRELFS_MAGIC;
	sb->s_op = &kestrelfs_super_ops;
	sb->s_maxbytes = MAX_LFS_FILESIZE;
	sb->s_blocksize = PAGE_SIZE;
	sb->s_blocksize_bits = PAGE_SHIFT;
	sb->s_time_gran = 1;

	/* Create root inode (i_ino=1, S_IFDIR | 0755) */
	root_inode = new_inode(sb);
	if (!root_inode) {
		pr_err("kestrelfs: failed to allocate root inode\n");
		return -ENOMEM;
	}

	root_inode->i_ino = 1; /* ROOT_INODE */
	root_inode->i_mode = S_IFDIR | 0755;
	root_inode->i_uid = GLOBAL_ROOT_UID;
	root_inode->i_gid = GLOBAL_ROOT_GID;
	inode_set_atime_to_ts(root_inode, current_time(root_inode));
	inode_set_mtime_to_ts(root_inode, current_time(root_inode));
	inode_set_ctime_to_ts(root_inode, current_time(root_inode));
	root_inode->i_op = &kestrelfs_dir_inode_operations;
	root_inode->i_fop = &kestrelfs_dir_file_operations;
	set_nlink(root_inode, 2); /* . and .. */

	/* Create root dentry */
	root_dentry = d_make_root(root_inode);
	if (!root_dentry) {
		pr_err("kestrelfs: d_make_root failed\n");
		return -ENOMEM;
	}

	sb->s_root = root_dentry;

	pr_info("kestrelfs: dynamic superblock populated (root inode only, files via LOOKUP)\n");
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
