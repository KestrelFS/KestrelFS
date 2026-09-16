// SPDX-License-Identifier: GPL-2.0
/*
 * dir.c - KestrelFS directory inode/file operations (Phase 3 Step 7b).
 *
 * Implements dynamic LOOKUP/CREATE/READDIR via IPC to the Rust daemon,
 * replacing the static simple_fill_super() tree.
 */

#include <linux/fs.h>
#include <linux/slab.h>
#include <linux/dcache.h>
#include <linux/namei.h>
#include <linux/time.h>
#include <linux/delay.h>
#include <linux/unaligned.h>

#include "kestrelfs.h"
#include "kestrelfs_ipc.h"

/*
 * Helper: synchronously send an IPC request and wait for response.
 * Returns 0 on success with response event copied to *resp_out.
 * Returns negative errno on timeout/error.
 *
 * Timeout: 2 seconds (reduced from 10s to avoid hanging umount).
 * Interruptible: returns -EINTR if interrupted by signal (e.g., Ctrl+C).
 * Fast-fail: returns -EIO immediately if daemon is not attached.
 */
static int kestrelfs_ipc_sync_call(struct kestrelfs_event *req,
				   struct kestrelfs_event *resp_out)
{
	u64 req_id;
	int ret;
	int i;

	/* Fast-fail if daemon is not attached */
	if (!kestrelfs_is_daemon_alive()) {
		pr_err("kestrelfs: IPC failed - daemon not attached\n");
		return -EIO;
	}

	ret = kestrelfs_req_push(req->opcode, req->flags, req->payload, &req_id);
	if (ret) {
		pr_err("kestrelfs: failed to push IPC request: %d\n", ret);
		return ret;
	}

	/* Poll for response with timeout (2 seconds = 2000 iterations) */
	for (i = 0; i < 2000; i++) {
		/* Check if daemon disconnected during wait */
		if (!kestrelfs_is_daemon_alive()) {
			pr_err("kestrelfs: daemon detached while waiting for req_id %llu\n", req_id);
			return -EIO;
		}

		ret = kestrelfs_check_resp(req_id, resp_out);
		if (ret == 0) {
			/* Got our response */
			if (resp_out->opcode == KESTRELFS_OP_RESULT_ERROR) {
				/* Daemon returned error, convert to errno */
				return resp_out->error_code;
			}
			if (resp_out->opcode == KESTRELFS_OP_RESULT_OK) {
				return 0;
			}
			pr_err("kestrelfs: unexpected response opcode %u\n",
			       resp_out->opcode);
			return -EIO;
		}

		/* No response yet, wait for daemon notification */
		ret = kestrelfs_wait_for_resp(msecs_to_jiffies(1));
		if (ret == -ERESTARTSYS) {
			/* Interrupted by signal (Ctrl+C, umount, etc.) */
			pr_info("kestrelfs: IPC call interrupted for req_id %llu\n", req_id);
			return -EINTR;
		}
		if (ret == -ETIME) {
			/* 1ms timeout expired, continue polling */
			continue;
		}
	}

	pr_err("kestrelfs: IPC timeout waiting for req_id %llu (daemon dead?)\n", req_id);
	return -ETIMEDOUT;
}

/*
 * Send one ABI v10 request whose single name lives at data_buffer[0].
 * The shared mutex covers both publication and response consumption, so the
 * daemon has exclusive access to the name until it finishes the request.
 */
static int kestrelfs_name_data_call(u32 opcode, u64 parent_ino, u32 mode,
				    const char *name, size_t name_len,
				    struct kestrelfs_event *resp)
{
	struct kestrelfs_shared_region *region;
	struct kestrelfs_event req = { 0 };
	int ret;

	if (name_len == 0 || name_len > KESTRELFS_NAME_DATA_MAX)
		return -ENAMETOOLONG;

	ret = mutex_lock_interruptible(&kestrelfs_data_ipc_lock);
	if (ret)
		return ret;

	region = kestrelfs_shm_region();
	if (!region) {
		ret = -ENOTCONN;
		goto out_unlock;
	}

	memcpy(region->data_buffer, name, name_len);
	req.opcode = opcode;
	put_unaligned_le64(parent_ino, &req.payload[0]);
	put_unaligned_le16((u16)name_len, &req.payload[8]);
	put_unaligned_le32(mode, &req.payload[12]);

	ret = kestrelfs_ipc_sync_call(&req, resp);

out_unlock:
	mutex_unlock(&kestrelfs_data_ipc_lock);
	return ret;
}

/* Refresh directory attributes from the authoritative MetaStore. This also
 * keeps the special root inode's persisted nlink visible after remount.
 */
static int kestrelfs_inode_getattr(struct mnt_idmap *idmap,
				   const struct path *path, struct kstat *stat,
				   u32 request_mask, unsigned int query_flags)
{
	struct inode *inode = d_inode(path->dentry);
	struct kestrelfs_event req = { 0 };
	struct kestrelfs_event resp = { 0 };
	struct timespec64 mtime;
	u64 size;
	u64 mtime_sec;
	u32 mode;
	u32 uid;
	u32 gid;
	u32 nlink;
	int ret;

	req.opcode = KESTRELFS_OP_GETATTR;
	put_unaligned_le64(inode->i_ino, &req.payload[0]);
	ret = kestrelfs_ipc_sync_call(&req, &resp);
	if (ret)
		return ret;

	size = get_unaligned_le64(&resp.payload[0]);
	mode = get_unaligned_le32(&resp.payload[8]);
	uid = get_unaligned_le32(&resp.payload[12]);
	gid = get_unaligned_le32(&resp.payload[16]);
	nlink = get_unaligned_le32(&resp.payload[20]);
	mtime_sec = get_unaligned_le64(&resp.payload[24]);
	if (!S_ISDIR(mode) || nlink < 2)
		return -EIO;

	inode->i_mode = mode;
	i_uid_write(inode, uid);
	i_gid_write(inode, gid);
	i_size_write(inode, size);
	set_nlink(inode, nlink);
	mtime.tv_sec = mtime_sec;
	mtime.tv_nsec = 0;
	inode_set_mtime_to_ts(inode, mtime);
	ret = kestrelfs_refresh_inode_times(inode);
	if (ret)
		return ret;
	generic_fillattr(idmap, request_mask, inode, stat);
	return 0;
}

/*
 * kestrelfs_inode_lookup - VFS ->lookup() for directories.
 * 
 * Sends KESTRELFS_OP_LOOKUP_DATA to daemon, creates and returns a new inode
 * on success, or ERR_PTR(-errno) on failure.
 */
static struct dentry *kestrelfs_inode_lookup(struct inode *dir,
					     struct dentry *dentry,
					     unsigned int flags)
{
	struct super_block *sb = dir->i_sb;
	struct kestrelfs_event resp = { 0 };
	struct inode *inode;
	const char *name = dentry->d_name.name;
	size_t name_len = dentry->d_name.len;
	u64 child_ino;
	u64 size;
	u32 mode;
	u32 uid;
	u32 gid;
	u32 nlink;
	int ret;

	pr_info("kestrelfs: lookup parent=%lu name=\"%s\"\n",
		dir->i_ino, name);

	if (name_len > KESTRELFS_NAME_DATA_MAX) {
		pr_err("kestrelfs: lookup name too long: %zu\n", name_len);
		return ERR_PTR(-ENAMETOOLONG);
	}

	ret = kestrelfs_name_data_call(KESTRELFS_OP_LOOKUP_DATA,
				      dir->i_ino, 0, name, name_len, &resp);
	if (ret) {
		pr_info("kestrelfs: lookup failed: %d\n", ret);
		if (ret == -ENOENT) {
			/* Not found - return NULL for negative dentry */
			return d_splice_alias(NULL, dentry);
		}
		return ERR_PTR(ret);
	}

	/* Parse response: child_ino(u64@0) + size(u64@8) + mode(u32@16) + ... */
	memcpy(&child_ino, &resp.payload[0], sizeof(u64));
	memcpy(&size, &resp.payload[8], sizeof(u64));
	memcpy(&mode, &resp.payload[16], sizeof(u32));
	memcpy(&uid, &resp.payload[20], sizeof(u32));
	memcpy(&gid, &resp.payload[24], sizeof(u32));
	memcpy(&nlink, &resp.payload[28], sizeof(u32));

	pr_info("kestrelfs: lookup found child_ino=%llu size=%llu mode=0%o\n",
		child_ino, size, mode);

	/* Create/fetch inode */
	inode = kestrelfs_get_inode(sb, child_ino, mode, size, uid, gid, nlink);
	if (IS_ERR(inode)) {
		pr_err("kestrelfs: failed to create inode: %ld\n",
		       PTR_ERR(inode));
		return ERR_CAST(inode);
	}
	ret = kestrelfs_refresh_inode_times(inode);
	if (ret) {
		iput(inode);
		return ERR_PTR(ret);
	}

	/* Attach to dentry */
	return d_splice_alias(inode, dentry);
}

/*
 * kestrelfs_inode_create - VFS ->create() for directories.
 *
 * Sends KESTRELFS_OP_CREATE_DATA to daemon, creates inode and instantiates
 * dentry.
 */
static int kestrelfs_inode_create(struct mnt_idmap *idmap,
				  struct inode *dir,
				  struct dentry *dentry,
				  umode_t mode,
				  bool excl)
{
	struct super_block *sb = dir->i_sb;
	struct kestrelfs_event resp = { 0 };
	struct inode *inode;
	const char *name = dentry->d_name.name;
	size_t name_len = dentry->d_name.len;
	u64 new_ino;
	u64 size;
	u32 resp_mode;
	u32 uid;
	u32 gid;
	u32 create_mode;
	u32 nlink;
	int ret;

	pr_info("kestrelfs: create parent=%lu name=\"%s\" mode=0%o\n",
		dir->i_ino, name, mode);

	if (name_len > KESTRELFS_NAME_DATA_MAX) {
		pr_err("kestrelfs: create name too long: %zu\n", name_len);
		return -ENAMETOOLONG;
	}

	create_mode = S_IFREG | (mode & 07777);
	ret = kestrelfs_name_data_call(KESTRELFS_OP_CREATE_DATA,
				      dir->i_ino, create_mode,
				      name, name_len, &resp);
	if (ret) {
		pr_err("kestrelfs: create failed: %d\n", ret);
		return ret;
	}

	/* Parse response: same as LOOKUP (new_ino + size + mode + ...) */
	memcpy(&new_ino, &resp.payload[0], sizeof(u64));
	memcpy(&size, &resp.payload[8], sizeof(u64));
	memcpy(&resp_mode, &resp.payload[16], sizeof(u32));
	memcpy(&uid, &resp.payload[20], sizeof(u32));
	memcpy(&gid, &resp.payload[24], sizeof(u32));
	memcpy(&nlink, &resp.payload[28], sizeof(u32));

	pr_info("kestrelfs: create -> new_ino=%llu size=%llu mode=0%o\n",
		new_ino, size, resp_mode);

	/* Create inode */
	inode = kestrelfs_get_inode(sb, new_ino, resp_mode, size, uid, gid,
				    nlink);
	if (IS_ERR(inode)) {
		pr_err("kestrelfs: failed to create inode: %ld\n",
		       PTR_ERR(inode));
		return PTR_ERR(inode);
	}

	/* Instantiate dentry */
	d_instantiate(dentry, inode);
	inode_set_mtime_to_ts(dir, inode_set_ctime_current(dir));

	return 0;
}

/* Create a metadata-only symbolic link via the serialized bounce buffer. */
static int kestrelfs_inode_symlink(struct mnt_idmap *idmap,
				   struct inode *dir, struct dentry *dentry,
				   const char *symname)
{
	struct kestrelfs_shared_region *region;
	struct kestrelfs_event req = { 0 };
	struct kestrelfs_event resp = { 0 };
	struct inode *inode;
	size_t name_len = dentry->d_name.len;
	size_t target_len = strnlen(symname, KESTRELFS_SYMLINK_TARGET_MAX + 1);
	u64 new_ino, size;
	u32 mode, uid, gid, nlink;
	int ret;

	if (name_len == 0 || name_len > KESTRELFS_NAME_DATA_MAX)
		return -ENAMETOOLONG;
	if (target_len == 0)
		return -ENOENT;
	if (target_len > KESTRELFS_SYMLINK_TARGET_MAX ||
	    name_len + target_len > KESTRELFS_DATA_BUFFER_SIZE)
		return -ENAMETOOLONG;

	ret = mutex_lock_interruptible(&kestrelfs_data_ipc_lock);
	if (ret)
		return ret;
	region = kestrelfs_shm_region();
	if (!region) {
		ret = -ENOTCONN;
		goto out_unlock_symlink;
	}

	memcpy(region->data_buffer, dentry->d_name.name, name_len);
	memcpy(&region->data_buffer[name_len], symname, target_len);
	req.opcode = KESTRELFS_OP_SYMLINK_DATA;
	put_unaligned_le64(dir->i_ino, &req.payload[0]);
	put_unaligned_le16((u16)name_len, &req.payload[8]);
	put_unaligned_le16((u16)target_len, &req.payload[10]);
	ret = kestrelfs_ipc_sync_call(&req, &resp);

out_unlock_symlink:
	mutex_unlock(&kestrelfs_data_ipc_lock);
	if (ret)
		return ret;

	new_ino = get_unaligned_le64(&resp.payload[0]);
	size = get_unaligned_le64(&resp.payload[8]);
	mode = get_unaligned_le32(&resp.payload[16]);
	uid = get_unaligned_le32(&resp.payload[20]);
	gid = get_unaligned_le32(&resp.payload[24]);
	nlink = get_unaligned_le32(&resp.payload[28]);
	inode = kestrelfs_get_inode(dir->i_sb, new_ino, mode, size, uid, gid,
				    nlink);
	if (IS_ERR(inode))
		return PTR_ERR(inode);
	d_instantiate(dentry, inode);
	inode_set_mtime_to_ts(dir, inode_set_ctime_current(dir));
	return 0;
}

/*
 * Resolve the raw target for VFS pathname walking/readlink(2). For RCU walk,
 * return -ECHILD so VFS retries in reference-walk mode where sleeping IPC and
 * delayed allocation are permitted.
 */
static const char *kestrelfs_get_link(struct dentry *dentry,
				      struct inode *inode,
				      struct delayed_call *done)
{
	struct kestrelfs_shared_region *region;
	struct kestrelfs_event req = { 0 };
	struct kestrelfs_event resp = { 0 };
	char *target = NULL;
	u32 target_len;
	int ret;

	if (!dentry)
		return ERR_PTR(-ECHILD);

	ret = mutex_lock_interruptible(&kestrelfs_data_ipc_lock);
	if (ret)
		return ERR_PTR(ret);
	region = kestrelfs_shm_region();
	if (!region) {
		ret = -ENOTCONN;
		goto out_unlock_readlink;
	}

	req.opcode = KESTRELFS_OP_READLINK_DATA;
	put_unaligned_le64(inode->i_ino, &req.payload[0]);
	ret = kestrelfs_ipc_sync_call(&req, &resp);
	if (ret)
		goto out_unlock_readlink;

	target_len = get_unaligned_le32(&resp.payload[0]);
	if (target_len == 0 || target_len > KESTRELFS_SYMLINK_TARGET_MAX ||
	    target_len > KESTRELFS_DATA_BUFFER_SIZE) {
		ret = -EPROTO;
		goto out_unlock_readlink;
	}
	target = kmalloc(target_len + 1, GFP_KERNEL);
	if (!target) {
		ret = -ENOMEM;
		goto out_unlock_readlink;
	}
	memcpy(target, region->data_buffer, target_len);
	target[target_len] = '\0';

out_unlock_readlink:
	mutex_unlock(&kestrelfs_data_ipc_lock);
	if (ret) {
		kfree(target);
		return ERR_PTR(ret);
	}
	set_delayed_call(done, kfree_link, target);
	return target;
}

const struct inode_operations kestrelfs_symlink_inode_operations = {
	.get_link	= kestrelfs_get_link,
};

/*
 * kestrelfs_inode_mkdir - VFS ->mkdir() for directories.
 *
 * Sends KESTRELFS_OP_MKDIR_DATA to daemon, waits for response, creates VFS
 * inode.
 */
static int kestrelfs_inode_mkdir(struct mnt_idmap *idmap, struct inode *dir,
				 struct dentry *dentry, umode_t mode)
{
	struct super_block *sb = dir->i_sb;
	struct kestrelfs_event resp = { 0 };
	u64 new_ino;
	struct inode *inode;
	const char *name = dentry->d_name.name;
	size_t name_len = dentry->d_name.len;
	int ret;

	pr_info("kestrelfs: mkdir parent=%lu name=\"%s\" mode=0%o\n",
		dir->i_ino, name, mode);

	if (name_len > KESTRELFS_NAME_DATA_MAX) {
		pr_warn("kestrelfs: mkdir name too long: %zu bytes\n", name_len);
		return -ENAMETOOLONG;
	}

	ret = kestrelfs_name_data_call(KESTRELFS_OP_MKDIR_DATA,
				      dir->i_ino, (u32)mode,
				      name, name_len, &resp);
	if (ret) {
		pr_info("kestrelfs: mkdir failed: %d\n", ret);
		return ret;
	}

	/* Parse response: new_ino(u64@0) */
	memcpy(&new_ino, &resp.payload[0], sizeof(u64));

	pr_info("kestrelfs: mkdir created dir ino=%llu\n", new_ino);

	/* Create VFS inode for the new directory */
	inode = kestrelfs_get_inode(sb, new_ino, S_IFDIR | mode, 0, 0, 0, 2);
	if (IS_ERR(inode)) {
		pr_err("kestrelfs: failed to create inode: %ld\n",
		       PTR_ERR(inode));
		return PTR_ERR(inode);
	}

	/* Instantiate dentry */
	d_instantiate(dentry, inode);
	inc_nlink(dir);
	inode_set_mtime_to_ts(dir, inode_set_ctime_current(dir));

	return 0;
}

/* Create another directory entry for an existing non-directory inode. */
static int kestrelfs_inode_link(struct dentry *old_dentry, struct inode *dir,
				struct dentry *new_dentry)
{
	struct kestrelfs_shared_region *region;
	struct kestrelfs_event req = { 0 };
	struct kestrelfs_event resp = { 0 };
	struct inode *inode = d_inode(old_dentry);
	size_t name_len = new_dentry->d_name.len;
	u32 nlink;
	int ret;

	if (S_ISDIR(inode->i_mode))
		return -EPERM;
	if (name_len == 0 || name_len > KESTRELFS_NAME_DATA_MAX)
		return -ENAMETOOLONG;

	ret = mutex_lock_interruptible(&kestrelfs_data_ipc_lock);
	if (ret)
		return ret;
	region = kestrelfs_shm_region();
	if (!region) {
		ret = -ENOTCONN;
		goto out_unlock_link;
	}

	memcpy(region->data_buffer, new_dentry->d_name.name, name_len);
	req.opcode = KESTRELFS_OP_LINK_DATA;
	put_unaligned_le64(dir->i_ino, &req.payload[0]);
	put_unaligned_le64(inode->i_ino, &req.payload[8]);
	put_unaligned_le16((u16)name_len, &req.payload[16]);
	ret = kestrelfs_ipc_sync_call(&req, &resp);

out_unlock_link:
	mutex_unlock(&kestrelfs_data_ipc_lock);
	if (ret)
		return ret;

	nlink = get_unaligned_le32(&resp.payload[0]);
	if (nlink < 2)
		return -EPROTO;
	set_nlink(inode, nlink);
	inode_set_ctime_current(inode);
	inode_set_mtime_to_ts(dir, inode_set_ctime_current(dir));
	ihold(inode);
	d_instantiate(new_dentry, inode);
	return 0;
}

/*
 * kestrelfs_inode_unlink - VFS ->unlink() for files.
 *
 * Sends KESTRELFS_OP_UNLINK_DATA to daemon, waits for response.
 */
static int kestrelfs_inode_unlink(struct inode *dir, struct dentry *dentry)
{
	struct kestrelfs_event resp = { 0 };
	struct inode *inode = d_really_is_positive(dentry) ? d_inode(dentry) : NULL;
	struct kestrelfs_inode_state *state = NULL;
	u64 parent_ino = dir->i_ino;
	const char *name = dentry->d_name.name;
	size_t name_len = dentry->d_name.len;
	bool defer_reclaim = false;
	int ret;

	pr_info("kestrelfs: unlink parent=%lu name=\"%s\"\n",
		dir->i_ino, name);

	if (name_len > KESTRELFS_NAME_DATA_MAX) {
		pr_warn("kestrelfs: unlink name too long: %zu bytes\n", name_len);
		return -ENAMETOOLONG;
	}
	if (inode && S_ISREG(inode->i_mode)) {
		state = inode->i_private;
		if (!state)
			return -EIO;
		ret = mutex_lock_interruptible(&state->lifecycle_lock);
		if (ret)
			return ret;
		defer_reclaim = inode->i_nlink == 1 && state->open_handles > 0;
	}
	if (inode && inode->i_nlink == 1 && !defer_reclaim) {
		ret = kestrelfs_cache_invalidate_inode(inode->i_ino);
		if (ret)
			goto out_unlock_lifecycle;
	}

	ret = kestrelfs_name_data_call(KESTRELFS_OP_UNLINK_DATA,
				      parent_ino,
				      defer_reclaim ?
				      KESTRELFS_LIFECYCLE_DEFER_RECLAIM : 0,
				      name, name_len, &resp);
	if (ret) {
		pr_info("kestrelfs: unlink failed: %d\n", ret);
		goto out_unlock_lifecycle;
	}

	pr_info("kestrelfs: unlink removed \"%s\" from parent=%llu\n",
		name, parent_ino);

	/* Update inode metadata (VFS will handle dentry invalidation). */
	if (inode) {
		if (S_ISDIR(inode->i_mode)) {
			clear_nlink(inode);
			drop_nlink(dir);
		} else {
			drop_nlink(inode);
		}
		inode_set_ctime_current(inode);
		inode_set_mtime_to_ts(dir, inode_set_ctime_current(dir));
	}

out_unlock_lifecycle:
	if (state)
		mutex_unlock(&state->lifecycle_lock);
	return ret;
}

/*
 * kestrelfs_inode_rmdir - VFS ->rmdir() for directories.
 *
 * Uses the same KESTRELFS_OP_UNLINK_DATA opcode; daemon checks whether the
 * directory is empty.
 */
static int kestrelfs_inode_rmdir(struct inode *dir, struct dentry *dentry)
{
	pr_info("kestrelfs: rmdir parent=%lu name=\"%s\"\n",
		dir->i_ino, dentry->d_name.name);

	/* Reuse unlink logic (daemon will check if directory is empty) */
	return kestrelfs_inode_unlink(dir, dentry);
}

/*
 * kestrelfs_inode_rename - VFS ->rename() for files and directories.
 *
 * Sends KESTRELFS_OP_RENAME_DATA to daemon. Supports same-directory rename
 * and cross-directory moves with POSIX semantics (atomic replacement and
 * RENAME_NOREPLACE and RENAME_EXCHANGE).
 * Names are concatenated in the shared data bounce buffer, so each may be up
 * to KESTRELFS_RENAME_DATA_NAME_MAX bytes.
 */
static int kestrelfs_inode_rename(struct mnt_idmap *idmap,
				  struct inode *old_dir, struct dentry *old_dentry,
				  struct inode *new_dir, struct dentry *new_dentry,
				  unsigned int flags)
{
	struct kestrelfs_event req = { 0 };
	struct kestrelfs_event resp = { 0 };
	u64 old_parent_ino = old_dir->i_ino;
	u64 new_parent_ino = new_dir->i_ino;
	const char *old_name = old_dentry->d_name.name;
	const char *new_name = new_dentry->d_name.name;
	size_t old_name_len = old_dentry->d_name.len;
	size_t new_name_len = new_dentry->d_name.len;
	struct kestrelfs_shared_region *region;
	struct inode *replaced = d_really_is_positive(new_dentry) ?
		d_inode(new_dentry) : NULL;
	struct kestrelfs_inode_state *replaced_state = NULL;
	bool defer_reclaim = false;
	bool source_is_dir = d_really_is_positive(old_dentry) &&
		S_ISDIR(d_inode(old_dentry)->i_mode);
	bool target_is_dir = d_really_is_positive(new_dentry) &&
		S_ISDIR(d_inode(new_dentry)->i_mode);
	bool exchange = flags & RENAME_EXCHANGE;
	int ret;

	pr_info("kestrelfs: rename old_parent=%lu old_name=\"%s\" new_parent=%lu new_name=\"%s\" flags=0x%x\n",
		old_dir->i_ino, old_name, new_dir->i_ino, new_name, flags);

	/* WHITEOUT/unknown bits and NOREPLACE|EXCHANGE remain unsupported. */
	if ((flags & ~(RENAME_NOREPLACE | RENAME_EXCHANGE)) ||
	    (flags & RENAME_NOREPLACE && flags & RENAME_EXCHANGE)) {
		pr_warn("kestrelfs: rename flags 0x%x not supported\n", flags);
		return -EINVAL;
	}
	static_assert(RENAME_NOREPLACE == KESTRELFS_RENAME_NOREPLACE);
	static_assert(RENAME_EXCHANGE == KESTRELFS_RENAME_EXCHANGE);
	if (exchange && !replaced)
		return -ENOENT;

	/* Validate the per-name ABI limit and combined bounce-buffer budget. */
	if (old_name_len > KESTRELFS_RENAME_DATA_NAME_MAX) {
		pr_warn("kestrelfs: rename old_name too long: %zu bytes\n", old_name_len);
		return -ENAMETOOLONG;
	}
	if (new_name_len > KESTRELFS_RENAME_DATA_NAME_MAX) {
		pr_warn("kestrelfs: rename new_name too long: %zu bytes\n", new_name_len);
		return -ENAMETOOLONG;
	}
	if (old_name_len + new_name_len > KESTRELFS_DATA_BUFFER_SIZE)
		return -ENAMETOOLONG;
	/* A NOREPLACE request must leave cache state untouched on EEXIST. */
	if (replaced && replaced != d_inode(old_dentry) && flags == 0 &&
	    S_ISREG(replaced->i_mode)) {
		replaced_state = replaced->i_private;
		if (!replaced_state)
			return -EIO;
		ret = mutex_lock_interruptible(&replaced_state->lifecycle_lock);
		if (ret)
			return ret;
		defer_reclaim = replaced->i_nlink == 1 &&
			replaced_state->open_handles > 0;
	}
	if (replaced && replaced != d_inode(old_dentry) && flags == 0 &&
	    !defer_reclaim) {
		ret = kestrelfs_cache_invalidate_inode(replaced->i_ino);
		if (ret)
			goto out_unlock_lifecycle;
	}

	ret = mutex_lock_interruptible(&kestrelfs_data_ipc_lock);
	if (ret)
		goto out_unlock_lifecycle;

	region = kestrelfs_shm_region();
	if (!region) {
		ret = -ENOTCONN;
		goto out_unlock;
	}

	/* The lock remains held until the response transfers buffer ownership back. */
	memcpy(&region->data_buffer[0], old_name, old_name_len);
	memcpy(&region->data_buffer[old_name_len], new_name, new_name_len);

	/* Build the fixed-size header; names live contiguously in data_buffer. */
	req.opcode = KESTRELFS_OP_RENAME_DATA;
	req.req_id = 0;
	put_unaligned_le64(old_parent_ino, &req.payload[0]);
	put_unaligned_le64(new_parent_ino, &req.payload[8]);
	put_unaligned_le16((u16)old_name_len, &req.payload[16]);
	put_unaligned_le16((u16)new_name_len, &req.payload[18]);
	put_unaligned_le32(flags, &req.payload[20]);
	put_unaligned_le32(defer_reclaim ?
			   KESTRELFS_LIFECYCLE_DEFER_RECLAIM : 0,
			   &req.payload[24]);

	/* Send IPC request */
	ret = kestrelfs_ipc_sync_call(&req, &resp);

out_unlock:
	mutex_unlock(&kestrelfs_data_ipc_lock);
	if (ret) {
		pr_info("kestrelfs: rename failed: %d\n", ret);
		goto out_unlock_lifecycle;
	}

	pr_info("kestrelfs: rename success\n");

	/* Update VFS metadata. VFS performs the dentry move after this callback. */
	if (exchange) {
		/* Both inodes survive. Only cross-parent mixed-type exchanges change
		 * the parents' immediate-subdirectory counts. Linux permits exchanging
		 * a directory and a non-directory when both paths exist.
		 */
		if (old_dir != new_dir && source_is_dir != target_is_dir) {
			if (source_is_dir) {
				drop_nlink(old_dir);
				inc_nlink(new_dir);
			} else {
				inc_nlink(old_dir);
				drop_nlink(new_dir);
			}
		}
		if (replaced)
			inode_set_ctime_current(replaced);
	} else {
		if (d_really_is_positive(new_dentry) &&
		    d_inode(new_dentry) != d_inode(old_dentry)) {
			if (target_is_dir)
				clear_nlink(replaced);
			else
				drop_nlink(replaced);
			inode_set_ctime_current(replaced);
		}
		if (old_dir == new_dir) {
			if (target_is_dir)
				drop_nlink(old_dir);
		} else {
			if (source_is_dir)
				drop_nlink(old_dir);
			if (source_is_dir && !target_is_dir)
				inc_nlink(new_dir);
			else if (!source_is_dir && target_is_dir)
				drop_nlink(new_dir);
		}
	}
	if (d_really_is_positive(old_dentry)) {
		struct inode *inode = d_inode(old_dentry);
		inode_set_ctime_current(inode);
	}
	inode_set_mtime_to_ts(old_dir, inode_set_ctime_current(old_dir));
	if (old_dir != new_dir) {
		inode_set_mtime_to_ts(new_dir, inode_set_ctime_current(new_dir));
	}

out_unlock_lifecycle:
	if (replaced_state)
		mutex_unlock(&replaced_state->lifecycle_lock);
	return ret;
}

const struct inode_operations kestrelfs_dir_inode_operations = {
	.getattr	= kestrelfs_inode_getattr,
	.setattr	= kestrelfs_inode_setattr,
	.lookup		= kestrelfs_inode_lookup,
	.create		= kestrelfs_inode_create,
	.link		= kestrelfs_inode_link,
	.symlink	= kestrelfs_inode_symlink,
	.mkdir		= kestrelfs_inode_mkdir,
	.unlink		= kestrelfs_inode_unlink,
	.rmdir		= kestrelfs_inode_rmdir,
	.rename		= kestrelfs_inode_rename,
};

/*
 * kestrelfs_readdir - VFS ->iterate_shared() for directories.
 *
 * READDIR_DATA returns a batch of variable-length records in the bounce
 * buffer. The daemon sorts by inode for stable pagination. If dir_emit()
 * fills the caller's buffer mid-batch, ctx->pos remains at the first un-emitted
 * entry so the next iterate call requests it again.
 */
static int kestrelfs_readdir(struct file *file, struct dir_context *ctx)
{
	struct inode *inode = file_inode(file);
	struct kestrelfs_shared_region *region;
	u32 offset;
	int ret;

	pr_info("kestrelfs: readdir ino=%lu pos=%lld\n", inode->i_ino, ctx->pos);

	/* Emit . and .. (dir_emit_dots handles ctx->pos advancement) */
	if (!dir_emit_dots(file, ctx))
		return 0;

	if (ctx->pos - 2 > U32_MAX)
		return -EOVERFLOW;

	offset = (ctx->pos >= 2) ? (u32)(ctx->pos - 2) : 0;

	for (;;) {
		struct kestrelfs_event req = { 0 };
		struct kestrelfs_event resp = { 0 };
		u32 entry_count, data_len, cursor = 0, i;

		ret = mutex_lock_interruptible(&kestrelfs_data_ipc_lock);
		if (ret)
			return ret;

		region = kestrelfs_shm_region();
		if (!region) {
			ret = -ENOTCONN;
			goto out_unlock_batch;
		}

		req.opcode = KESTRELFS_OP_READDIR_DATA;
		put_unaligned_le64(inode->i_ino, &req.payload[0]);
		put_unaligned_le32(offset, &req.payload[8]);

		ret = kestrelfs_ipc_sync_call(&req, &resp);
		if (ret) {
			pr_err("kestrelfs: readdir failed: %d\n", ret);
			goto out_unlock_batch;
		}

		entry_count = get_unaligned_le32(&resp.payload[0]);
		data_len = get_unaligned_le32(&resp.payload[4]);
		if (data_len > KESTRELFS_DATA_BUFFER_SIZE) {
			ret = -EPROTO;
			goto out_unlock_batch;
		}

		pr_info("kestrelfs: readdir offset=%u -> entries=%u bytes=%u\n",
			offset, entry_count, data_len);

		if (entry_count == 0) {
			ret = data_len == 0 ? 0 : -EPROTO;
			mutex_unlock(&kestrelfs_data_ipc_lock);
			return ret;
		}

		for (i = 0; i < entry_count; i++) {
			u64 ino;
			u16 name_len;

			if (cursor + KESTRELFS_READDIR_DATA_ENTRY_HEADER_SIZE > data_len) {
				ret = -EPROTO;
				goto out_unlock_batch;
			}

			ino = get_unaligned_le64(&region->data_buffer[cursor]);
			name_len = get_unaligned_le16(
				&region->data_buffer[cursor + sizeof(u64)]);
			cursor += KESTRELFS_READDIR_DATA_ENTRY_HEADER_SIZE;
			if (name_len == 0 || name_len > KESTRELFS_NAME_DATA_MAX ||
			    cursor + name_len > data_len) {
				ret = -EPROTO;
				goto out_unlock_batch;
			}

			if (!dir_emit(ctx, &region->data_buffer[cursor], name_len,
				      ino, DT_UNKNOWN)) {
				mutex_unlock(&kestrelfs_data_ipc_lock);
				return 0;
			}
			cursor += name_len;
			ctx->pos++;
			offset++;
		}

		if (cursor != data_len) {
			ret = -EPROTO;
			goto out_unlock_batch;
		}
		mutex_unlock(&kestrelfs_data_ipc_lock);
		continue;

out_unlock_batch:
		mutex_unlock(&kestrelfs_data_ipc_lock);
		return ret;
	}
}

const struct file_operations kestrelfs_dir_file_operations = {
	.read		= generic_read_dir,
	.iterate_shared	= kestrelfs_readdir,
	.llseek		= generic_file_llseek,
};
