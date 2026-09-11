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
 * kestrelfs_inode_lookup - VFS ->lookup() for directories.
 * 
 * Sends KESTRELFS_OP_LOOKUP to daemon, creates and returns a new inode
 * on success, or ERR_PTR(-errno) on failure.
 */
static struct dentry *kestrelfs_inode_lookup(struct inode *dir,
					     struct dentry *dentry,
					     unsigned int flags)
{
	struct super_block *sb = dir->i_sb;
	struct kestrelfs_event req = { 0 };
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

	/* Check name length (LOOKUP max is 23 bytes) */
	if (name_len > 23) {
		pr_err("kestrelfs: lookup name too long: %zu\n", name_len);
		return ERR_PTR(-ENAMETOOLONG);
	}

	/* Build LOOKUP request: parent(u64@0) + name_len(u8@8) + name@9 */
	req.opcode = KESTRELFS_OP_LOOKUP;
	req.flags = 0;
	memcpy(&req.payload[0], &dir->i_ino, sizeof(u64));
	req.payload[8] = (u8)name_len;
	memcpy(&req.payload[9], name, name_len);

	/* Send IPC request */
	ret = kestrelfs_ipc_sync_call(&req, &resp);
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

	/* Attach to dentry */
	return d_splice_alias(inode, dentry);
}

/*
 * kestrelfs_inode_create - VFS ->create() for directories.
 *
 * Sends KESTRELFS_OP_CREATE to daemon, creates inode and instantiates dentry.
 */
static int kestrelfs_inode_create(struct mnt_idmap *idmap,
				  struct inode *dir,
				  struct dentry *dentry,
				  umode_t mode,
				  bool excl)
{
	struct super_block *sb = dir->i_sb;
	struct kestrelfs_event req = { 0 };
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

	/* Check name length (CREATE max is 19 + NUL = 20 bytes) */
	if (name_len > 19) {
		pr_err("kestrelfs: create name too long: %zu\n", name_len);
		return -ENAMETOOLONG;
	}

	/* Build CREATE request: parent(u64@0) + mode(u32@8) + name@12 (NUL-terminated) */
	req.opcode = KESTRELFS_OP_CREATE;
	req.flags = 0;
	create_mode = S_IFREG | (mode & 0777);
	memcpy(&req.payload[0], &dir->i_ino, sizeof(u64));
	memcpy(&req.payload[8], &create_mode, sizeof(u32));
	memcpy(&req.payload[12], name, name_len);
	req.payload[12 + name_len] = 0; /* NUL terminator */

	/* Send IPC request */
	ret = kestrelfs_ipc_sync_call(&req, &resp);
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

	/* Instantiate dentry */
	d_instantiate(dentry, inode);

	return 0;
}

const struct inode_operations kestrelfs_dir_inode_operations = {
	.lookup		= kestrelfs_inode_lookup,
	.create		= kestrelfs_inode_create,
};

/*
 * kestrelfs_readdir - VFS ->iterate_shared() for directories.
 *
 * Sends KESTRELFS_OP_READDIR to daemon repeatedly with increasing offset
 * until entry_count == 0. Emits entries via dir_emit().
 *
 * LIMITATION: READDIR response can only fit 2 entries per call due to
 * payload size constraints (32 bytes). Entry names are limited:
 * - entry[0]: max 10 chars + NUL (11 bytes)
 * - entry[1]: max 4 chars (truncated, no NUL guarantee)
 * Long names will be truncated and may cause issues with userspace tools.
 */
static int kestrelfs_readdir(struct file *file, struct dir_context *ctx)
{
	struct inode *inode = file_inode(file);
	
	struct kestrelfs_event req = { 0 };
	struct kestrelfs_event resp = { 0 };
	u32 offset = (u32)ctx->pos;
	int ret;

	pr_info("kestrelfs: readdir ino=%lu pos=%lld\n", inode->i_ino, ctx->pos);

	/* Emit . and .. */
	if (ctx->pos == 0) {
		if (!dir_emit_dot(file, ctx))
			return 0;
	}
	if (ctx->pos == 1) {
		if (!dir_emit_dotdot(file, ctx))
			return 0;
	}

	/* Adjust offset for daemon (skip . and ..) */
	if (ctx->pos >= 2)
		offset = (u32)(ctx->pos - 2);
	else
		offset = 0;

	/* Loop: send READDIR requests until entry_count == 0 */
	while (1) {
		u8 entry_count;
		u64 ino1, ino2;
		char name1[12], name2[5];
		int i;

		/* Build READDIR request: dir_ino(u64@0) + offset(u32@8) */
		req.opcode = KESTRELFS_OP_READDIR;
		req.flags = 0;
		memcpy(&req.payload[0], &inode->i_ino, sizeof(u64));
		memcpy(&req.payload[8], &offset, sizeof(u32));

		ret = kestrelfs_ipc_sync_call(&req, &resp);
		if (ret) {
			pr_err("kestrelfs: readdir failed: %d\n", ret);
			return ret;
		}

		/* Parse response: entry_count(u8@0) + entries */
		entry_count = resp.payload[0];

		pr_info("kestrelfs: readdir offset=%u -> entry_count=%u\n",
			offset, entry_count);

		if (entry_count == 0) {
			/* End of directory */
			break;
		}

		/* Entry 0: ino(u64@1) + name(11 bytes@9, NUL-terminated) */
		if (entry_count >= 1) {
			memcpy(&ino1, &resp.payload[1], sizeof(u64));
			memcpy(name1, &resp.payload[9], 11);
			name1[11] = 0; /* ensure NUL termination */

			/* Find actual NUL in name1 */
			for (i = 0; i < 11; i++) {
				if (name1[i] == 0)
					break;
			}

			if (i > 0) {
				if (!dir_emit(ctx, name1, i, ino1, DT_UNKNOWN)) {
					return 0; /* Buffer full */
				}
				ctx->pos++;
			}
		}

		/* Entry 1: ino(u64@20) + name(4 bytes@28, possibly truncated) */
		if (entry_count >= 2) {
			memcpy(&ino2, &resp.payload[20], sizeof(u64));
			memcpy(name2, &resp.payload[28], 4);
			name2[4] = 0; /* ensure NUL termination */

			/* Find actual NUL or use all 4 bytes */
			for (i = 0; i < 4; i++) {
				if (name2[i] == 0)
					break;
			}

			if (i > 0) {
				if (!dir_emit(ctx, name2, i, ino2, DT_UNKNOWN)) {
					return 0; /* Buffer full */
				}
				ctx->pos++;
			}
		}

		/* Advance offset for next batch */
		offset += entry_count;

		/* Safety: if we got fewer than 2 entries, we're at the end */
		if (entry_count < 2)
			break;
	}

	return 0;
}

const struct file_operations kestrelfs_dir_file_operations = {
	.read		= generic_read_dir,
	.iterate_shared	= kestrelfs_readdir,
	.llseek		= generic_file_llseek,
};
