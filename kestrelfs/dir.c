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

	pr_info("kestrelfs: lookup found child_ino=%llu size=%llu mode=0%o\n",
		child_ino, size, mode);

	/* Create/fetch inode */
	inode = kestrelfs_get_inode(sb, child_ino, mode, size);
	if (IS_ERR(inode)) {
		pr_err("kestrelfs: failed to create inode: %ld\n",
		       PTR_ERR(inode));
		return ERR_CAST(inode);
	}

	/* Insert into inode hash (like shmem, avoid unhashed state) */
	insert_inode_hash(inode);

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
	u32 create_mode;
	int ret;

	pr_info("kestrelfs: create parent=%lu name=\"%s\" mode=0%o\n",
		dir->i_ino, name, mode);

	if (name_len > KESTRELFS_NAME_DATA_MAX) {
		pr_err("kestrelfs: create name too long: %zu\n", name_len);
		return -ENAMETOOLONG;
	}

	create_mode = S_IFREG | (mode & 0777);
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

	pr_info("kestrelfs: create -> new_ino=%llu size=%llu mode=0%o\n",
		new_ino, size, resp_mode);

	/* Create inode */
	inode = kestrelfs_get_inode(sb, new_ino, resp_mode, size);
	if (IS_ERR(inode)) {
		pr_err("kestrelfs: failed to create inode: %ld\n",
		       PTR_ERR(inode));
		return PTR_ERR(inode);
	}

	/* Insert into inode hash (like shmem, avoid unhashed state) */
	insert_inode_hash(inode);

	/* Instantiate dentry */
	d_instantiate(dentry, inode);

	return 0;
}

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
	inode = kestrelfs_get_inode(sb, new_ino, S_IFDIR | mode, 0);
	if (IS_ERR(inode)) {
		pr_err("kestrelfs: failed to create inode: %ld\n",
		       PTR_ERR(inode));
		return PTR_ERR(inode);
	}

	/* Instantiate dentry */
	d_instantiate(dentry, inode);

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
	u64 parent_ino = dir->i_ino;
	const char *name = dentry->d_name.name;
	size_t name_len = dentry->d_name.len;
	int ret;

	pr_info("kestrelfs: unlink parent=%lu name=\"%s\"\n",
		dir->i_ino, name);

	if (name_len > KESTRELFS_NAME_DATA_MAX) {
		pr_warn("kestrelfs: unlink name too long: %zu bytes\n", name_len);
		return -ENAMETOOLONG;
	}

	ret = kestrelfs_name_data_call(KESTRELFS_OP_UNLINK_DATA,
				      parent_ino, 0, name, name_len, &resp);
	if (ret) {
		pr_info("kestrelfs: unlink failed: %d\n", ret);
		return ret;
	}

	pr_info("kestrelfs: unlink removed \"%s\" from parent=%llu\n",
		name, parent_ino);

	/* Update inode metadata (VFS will handle dentry invalidation) */
	if (d_really_is_positive(dentry)) {
		struct inode *inode = d_inode(dentry);
		drop_nlink(inode);
		inode_set_ctime_current(inode);
		inode_set_mtime_to_ts(dir, inode_set_ctime_current(dir));
	}

	return 0;
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
 * and cross-directory moves with POSIX semantics (atomic replacement).
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
	int ret;

	pr_info("kestrelfs: rename old_parent=%lu old_name=\"%s\" new_parent=%lu new_name=\"%s\" flags=0x%x\n",
		old_dir->i_ino, old_name, new_dir->i_ino, new_name, flags);

	/* VFS may pass flags like RENAME_NOREPLACE, RENAME_EXCHANGE, etc.
	 * For now, we only support basic rename (flags=0).
	 */
	if (flags != 0) {
		pr_warn("kestrelfs: rename flags 0x%x not supported\n", flags);
		return -EINVAL;
	}

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

	ret = mutex_lock_interruptible(&kestrelfs_data_ipc_lock);
	if (ret)
		return ret;

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

	/* Send IPC request */
	ret = kestrelfs_ipc_sync_call(&req, &resp);

out_unlock:
	mutex_unlock(&kestrelfs_data_ipc_lock);
	if (ret) {
		pr_info("kestrelfs: rename failed: %d\n", ret);
		return ret;
	}

	pr_info("kestrelfs: rename success\n");

	/* Update VFS metadata. VFS performs the dentry move after this callback. */
	if (d_really_is_positive(old_dentry)) {
		struct inode *inode = d_inode(old_dentry);
		inode_set_ctime_current(inode);
	}
	inode_set_mtime_to_ts(old_dir, inode_set_ctime_current(old_dir));
	if (old_dir != new_dir) {
		inode_set_mtime_to_ts(new_dir, inode_set_ctime_current(new_dir));
	}

	return 0;
}

const struct inode_operations kestrelfs_dir_inode_operations = {
	.lookup		= kestrelfs_inode_lookup,
	.create		= kestrelfs_inode_create,
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
