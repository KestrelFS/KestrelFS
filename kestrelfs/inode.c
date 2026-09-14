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
	truncate_inode_pages_final(&inode->i_data);
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
 * Since daemon is the authoritative metadata store and we already send IPC during
 * write/setattr, we don't need to do anything here. Just return success.
 *
 * MUST NOT send IPC here - daemon may be shutting down during umount.
 */
static int kestrelfs_write_inode(struct inode *inode, struct writeback_control *wbc)
{
	return 0;
}

/*
 * kestrelfs_sync_fs() - handle sync(2) / syncfs(2).
 * @sb: superblock
 * @wait: whether to wait for completion (ignored)
 *
 * Since all writes are synchronous IPC calls that complete before returning,
 * there's nothing to flush. Just return success.
 */
static int kestrelfs_sync_fs(struct super_block *sb, int wait)
{
	return 0;
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
 *
 * Used by dir.c's lookup/create handlers to instantiate inodes dynamically.
 * 
 * NOTE: We use new_inode() instead of iget5_locked() because:
 * 1. Daemon is the authoritative metadata store (no persistent inode cache needed)
 * 2. VFS dentry cache already prevents duplicate lookups for the same path
 * 3. iget5_locked() adds complexity and potential race conditions during evict
 * 4. Each lookup creates a fresh inode, but dentry cache ensures path uniqueness
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
	} else if (S_ISLNK(mode)) {
		/* Target bytes are fetched from MetaStore through ->get_link(). */
		inode->i_op = &kestrelfs_symlink_inode_operations;
		set_nlink(inode, 1);
	} else if (S_ISREG(mode)) {
		/* Regular file - no custom a_ops (avoid dirty_folio without writeback) */
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
