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
#include <linux/slab.h>

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

/*
 * kestrelfs_evict_inode() - evict an inode from memory.
 * @inode: inode being evicted
 *
 * Called when VFS drops the last reference to an inode (umount, dentry
 * reclaim, memory pressure). We must:
 * 1. Invalidate all cached pages (truncate_inode_pages_final)
 * 2. Mark inode as clean (clear_inode)
 *
 * IMPORTANT: Do NOT send IPC here. The daemon is the authoritative metadata
 * store; kernel-side inode eviction is purely a cache management operation.
 * Attempting IPC during umount would deadlock (daemon may already be shutting
 * down, or umount is waiting for all inodes to be released).
 */
static void kestrelfs_evict_inode(struct inode *inode)
{
	if (S_ISREG(inode->i_mode) && inode->i_private)
		kestrelfs_pagecache_unregister_inode(inode);
	truncate_inode_pages_final(&inode->i_data);
	kfree(inode->i_private);
	inode->i_private = NULL;
	clear_inode(inode);
}

/*
 * kestrelfs_write_inode() - handle VFS writeback request.
 * @inode: inode to write
 * @wbc: writeback control (ignored)
 *
 * VFS may call this during umount or sync even if we don't call mark_inode_dirty().
 * notify_change() can mark inodes dirty internally.
 * 
 * The daemon is authoritative: all write/setattr IPC is committed before VFS
 * acknowledges it. Explicit durability uses file.c's ->fsync and ->sync_fs,
 * not this writeback callback (which may also run during inode teardown).
 *
 * MUST NOT send IPC here - daemon may be shutting down during umount.
 */
static int kestrelfs_write_inode(struct inode *inode, struct writeback_control *wbc)
{
	return 0;
}

/*
 * kestrelfs_sync_fs() - issue the mount-wide daemon durability barrier.
 * @sb: superblock
 * @wait: VFS zero means queue only; the blocking pass issues the barrier.
 */
static int kestrelfs_sync_fs(struct super_block *sb, int wait)
{
	if (!wait)
		return 0;
	return kestrelfs_sync_daemon(KESTRELFS_OP_SYNC_FS, 0);
}

const struct super_operations kestrelfs_super_ops = {
	.statfs		= kestrelfs_statfs,
	.drop_inode	= generic_drop_inode,
	.evict_inode	= kestrelfs_evict_inode,
	.write_inode	= kestrelfs_write_inode,
	.sync_fs	= kestrelfs_sync_fs,
};

/*
 * kestrelfs_get_inode() - fetch or create an inode with given attributes.
 * @sb:		superblock
 * @ino:	inode number
 * @mode:	file type and permissions (S_IFDIR | 0755, S_IFREG | 0644, etc.)
 * @size:	file size in bytes
 * @uid:	persistent numeric owner id
 * @gid:	persistent numeric group id
 * @nlink:	persistent link count reported by MetaStore
 *
 * Used by dir.c's lookup/create handlers to instantiate inodes dynamically.
 * 
 * iget_locked() makes the daemon's 64-bit inode id the superblock-local VFS
 * identity. This is required for hard links: independent lookups of two names
 * for the same daemon inode must share i_nlink and cache invalidation state.
 *
 * Return: pointer to inode on success, ERR_PTR(-errno) on failure.
 */
struct inode *kestrelfs_get_inode(struct super_block *sb, u64 ino,
				  u32 mode, u64 size, u32 uid, u32 gid,
				  u32 nlink)
{
	struct inode *inode;

	inode = iget_locked(sb, (unsigned long)ino);
	if (!inode)
		return ERR_PTR(-ENOMEM);
	if (!(inode->i_state & I_NEW)) {
		inode->i_mode = mode;
		i_uid_write(inode, uid);
		i_gid_write(inode, gid);
		i_size_write(inode, size);
		set_nlink(inode, nlink);
		return inode;
	}

	inode->i_ino = ino;
	inode->i_mode = mode;
	i_uid_write(inode, uid);
	i_gid_write(inode, gid);
	inode->i_size = size;
	inode_set_atime_to_ts(inode, current_time(inode));
	inode_set_mtime_to_ts(inode, current_time(inode));
	inode_set_ctime_to_ts(inode, current_time(inode));

	if (S_ISDIR(mode)) {
		/* Directory */
		inode->i_op = &kestrelfs_dir_inode_operations;
		inode->i_fop = &kestrelfs_dir_file_operations;
		set_nlink(inode, nlink);
	} else if (S_ISLNK(mode)) {
		/* Target bytes are fetched from MetaStore through ->get_link(). */
		inode->i_op = &kestrelfs_symlink_inode_operations;
		set_nlink(inode, nlink);
	} else if (S_ISREG(mode)) {
		struct kestrelfs_inode_state *state;

		state = kzalloc(sizeof(*state), GFP_KERNEL);
		if (!state) {
			iget_failed(inode);
			return ERR_PTR(-ENOMEM);
		}
		mutex_init(&state->lifecycle_lock);
		inode->i_private = state;
		kestrelfs_pagecache_register_inode(inode);
		/* Read-side page cache only; writes remain synchronous daemon IPC. */
		inode->i_mapping->a_ops = &kestrelfs_reg_aops;
		inode->i_op = &kestrelfs_reg_inode_ops;
		inode->i_fop = &kestrelfs_reg_file_ops;
		set_nlink(inode, nlink);
	} else {
		/* Unsupported file type */
		iget_failed(inode);
		return ERR_PTR(-EINVAL);
	}

	unlock_new_inode(inode);
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
 * mount_nodev() is appropriate here because KestrelFS has no backing block
 * device: it's a pseudo-filesystem where metadata lives in daemon memory
 * and the kernel only maintains inode caches.
 */
static struct dentry *kestrelfs_mount(struct file_system_type *fs_type,
				       int flags, const char *dev_name,
				       void *data)
{
	return mount_nodev(fs_type, flags, data, kestrelfs_fill_super);
}

/*
 * Use kill_anon_super() instead of kill_litter_super() because:
 * 1. We use mount_nodev() (no block device)
 * 2. We manually create root inode (no simple_fill_super tree_descr list)
 * 3. kill_anon_super() is the standard choice for pseudo-filesystems
 *    (procfs, sysfs, tmpfs, ramfs) - cleaner teardown path
 * 4. kill_litter_super() is for libfs-based filesystems using simple_fill_super,
 *    which we no longer use (Phase 3 removed it for dynamic LOOKUP)
 */
struct file_system_type kestrelfs_fs_type = {
	.owner		= THIS_MODULE,
	.name		= KESTRELFS_NAME,
	.mount		= kestrelfs_mount,
	.kill_sb	= kill_anon_super,
	.fs_flags	= FS_USERNS_MOUNT,
};
