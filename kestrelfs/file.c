// SPDX-License-Identifier: GPL-2.0
/*
 * file.c - Regular file operations for KestrelFS.
 *
 * Two files are currently served:
 *
 *   hello.txt   Static, read-only, purely in-memory. Its content is
 *               never written to a backing store or NVMe cache: the
 *               bytes come straight from a kernel .rodata string.
 *               simple_read_from_buffer() is the same helper used by
 *               debugfs and countless other pseudo filesystems for
 *               exactly this purpose. This file has zero dependency
 *               on the Phase 2 IPC bridge and remains readable even
 *               if no Rust daemon is attached - useful as a baseline
 *               mount health check.
 *
 *   remote.txt  The Phase 2 VFS integration step's proof of concept:
 *               every read is served by round-tripping through the
 *               kernel<->Rust IPC bridge (kestrelfs_req_push() /
 *               kestrelfs_wait_for_resp() / kestrelfs_check_resp(),
 *               see ipc_ring.c and chardev.c). This is the first real
 *               VFS call path in the project that depends on the
 *               Rust control-plane daemon being attached; with no
 *               daemon running, reads against remote.txt time out
 *               with -ETIMEDOUT instead of hanging forever or
 *               crashing - see kestrelfs_remote_read() below.
 */

#include <linux/fs.h>
#include <linux/uaccess.h>
#include <linux/unaligned.h>
#include <linux/minmax.h>
#include <linux/errno.h>
#include <linux/jiffies.h>
#include <linux/mm.h>

#include "kestrelfs.h"

/*
 * kestrelfs_file_read() - serve reads against hello.txt.
 * @file:	open file instance (unused beyond sanity, content is static).
 * @buf:	userspace destination buffer.
 * @count:	number of bytes requested.
 * @ppos:	file position, updated by simple_read_from_buffer().
 *
 * Return: number of bytes copied, or negative errno.
 */
static ssize_t kestrelfs_file_read(struct file *file, char __user *buf,
				    size_t count, loff_t *ppos)
{
	const char *content = KESTRELFS_HELLO_CONTENT;
	size_t len = strlen(content);

	return simple_read_from_buffer(buf, count, ppos, content, len);
}

const struct file_operations kestrelfs_file_ops = {
	.owner	= THIS_MODULE,
	.read	= kestrelfs_file_read,
	.llseek	= generic_file_llseek,
};

/*
 * KESTRELFS_REMOTE_WAIT_MS - how long a single remote.txt read()
 * blocks waiting for the Rust daemon to answer before giving up.
 *
 * This is deliberately a fixed, generous-but-bounded value for this
 * bootstrap VFS integration step. A production cache-miss path
 * (Phase 3+) would likely derive this from mount options or per-call
 * context (e.g. O_NONBLOCK handling, or a shorter budget for
 * read-ahead than for a synchronous foreground read) rather than a
 * single global constant.
 */
#define KESTRELFS_REMOTE_WAIT_MS	2000

/*
 * KESTRELFS_REMOTE_FILE_SIZE - the logical size reported for
 * remote.txt reads.
 *
 * remote.txt has no real backing store yet (Phase 3 will wire this
 * up to actual chunk/object storage) - it exists purely to exercise
 * the IPC round trip. Without SOME notion of EOF, remote.txt would
 * behave as an infinite stream: every read() (even one requesting 0
 * new bytes of *content*) returns a nonzero count as long as the
 * daemon keeps answering, which is exactly what a *pipe* or *device*
 * should do but is fatal for a regular file - tools like `cat`,
 * which loop read() until they observe a short/zero read, would spin
 * forever pushing one IPC round trip per iteration.
 *
 * KESTRELFS_REMOTE_FILE_SIZE fixes this by giving remote.txt a
 * well-defined, finite logical length: once *ppos reaches this
 * value, kestrelfs_remote_read() returns 0 (EOF) without pushing any
 * further IPC request at all, exactly like a normal bounded regular
 * file. Chosen as one KESTRELFS_READ_CHUNK_MAX_LEN-sized chunk's
 * worth times a small round number of "pages" worth of demonstration
 * content - large enough to show offset advancing across multiple
 * reads, small enough that `cat`/`hexdump`-style tools finish near
 * instantly even with the Rust daemon attached.
 *
 * Defined in kestrelfs.h (not here) since inode.c also needs it, to
 * fix up remote.txt's inode->i_size right after simple_fill_super()
 * creates it with the default of 0 - see
 * kestrelfs_fixup_remote_size() in inode.c.
 */

/*
 * kestrelfs_remote_read() - serve reads against remote.txt by
 * round-tripping through the kernel<->Rust IPC bridge.
 * @file:	open file instance (unused: remote.txt is stateless,
 *		every read is answered independently by the daemon).
 * @buf:	userspace destination buffer.
 * @count:	number of bytes requested.
 * @ppos:	file position; advanced by the number of bytes actually
 *		returned, exactly like a normal seekable file.
 *
 * Protocol (see kestrelfs_ipc.h's "Payload layout for
 * KESTRELFS_OP_READ_CHUNK requests" section for the exact byte
 * layout this function produces/consumes):
 *
 *   0. If *ppos has already reached KESTRELFS_REMOTE_FILE_SIZE (this
 *      file's fixed logical size, see that macro's doc comment for
 *      why remote.txt has one at all), return 0 (EOF) immediately -
 *      no IPC request is pushed for a read that cannot possibly
 *      return any data.
 *   1. Clamp the caller's requested @count down to both
 *      KESTRELFS_READ_CHUNK_MAX_LEN (32 bytes - this bootstrap
 *      protocol has no chunking/continuation support yet, so a
 *      single request can only ever return one payload's worth of
 *      data) and to whatever remains before
 *      KESTRELFS_REMOTE_FILE_SIZE (so the very last read of the file
 *      returns a correctly short final chunk instead of overrunning
 *      the logical EOF boundary). A @count of 0 is short-circuited to
 *      return 0 immediately without pushing any request at all.
 *   2. Encode (inode_id, *ppos, clamped count) into a request payload
 *      and push it via kestrelfs_req_push(KESTRELFS_OP_READ_CHUNK, ...).
 *      inode_id is taken from file->f_inode->i_ino, the same inode
 *      number simple_fill_super() assigned to this file when parsing
 *      the tree_descr array (see kestrelfs_fill_super() in inode.c).
 *      This field was added in KESTRELFS_ABI_VERSION 2 (see
 *      kestrelfs_ipc.h) - before that, the Rust daemon had no way to
 *      know which of possibly many files a READ_CHUNK request actually
 *      targeted. A full ring is surfaced to the caller as -EAGAIN,
 *      matching the same errno a normal socket/pipe read would use for
 *      "try again later" backpressure.
 *   3. Block on kestrelfs_wait_for_resp() in a retry loop, re-checking
 *      kestrelfs_check_resp(req_id, ...) after every wakeup, until a
 *      matching response arrives or KESTRELFS_REMOTE_WAIT_MS elapses
 *      - mirroring the exact pattern already proven correct by
 *      ipc_ring.c's debugfs self-test, just driven by a real VFS
 *      caller instead of a manual trigger.
 *   4. On success, copy_to_user() the response payload's leading
 *      @count bytes back and advance *ppos.
 *
 * Return: number of bytes copied on success (0 once *ppos reaches
 * KESTRELFS_REMOTE_FILE_SIZE, signaling EOF exactly like a normal
 * bounded regular file - see step 0 above). Negative errno on
 * failure: -EAGAIN if the REQ ring was full, -ETIMEDOUT if no
 * response arrived within KESTRELFS_REMOTE_WAIT_MS, -EINTR if
 * interrupted by a signal while waiting, -EFAULT if the userspace
 * buffer could not be written to.
 */
static ssize_t kestrelfs_remote_read(struct file *file, char __user *buf,
				      size_t count, loff_t *ppos)
{
	struct kestrelfs_event resp;
	u8 payload[KESTRELFS_EVENT_PAYLOAD_SIZE];
	unsigned long deadline;
	u64 req_id;
	u32 clamped_count;
	int ret;

	if (*ppos >= KESTRELFS_REMOTE_FILE_SIZE)
		return 0;

	if (count == 0)
		return 0;

	clamped_count = (u32)min3((u64)count, (u64)KESTRELFS_READ_CHUNK_MAX_LEN,
				   (u64)(KESTRELFS_REMOTE_FILE_SIZE - *ppos));

	memset(payload, 0, sizeof(payload));
	put_unaligned_le64((u64)file->f_inode->i_ino, &payload[0]);
	put_unaligned_le64((u64)*ppos, &payload[8]);
	put_unaligned_le32(clamped_count, &payload[16]);

	ret = kestrelfs_req_push(KESTRELFS_OP_READ_CHUNK, 0, payload, &req_id);
	if (ret) {
		/* Ring full: propagate as a transient failure, exactly
		 * like kestrelfs_req_push()'s own documented -EAGAIN
		 * contract. */
		return ret;
	}

	deadline = jiffies + msecs_to_jiffies(KESTRELFS_REMOTE_WAIT_MS);

	for (;;) {
		ret = kestrelfs_check_resp(req_id, &resp);
		if (ret == 0)
			break;

		if (time_after_eq(jiffies, deadline))
			return -ETIMEDOUT;

		ret = kestrelfs_wait_for_resp(msecs_to_jiffies(KESTRELFS_REMOTE_WAIT_MS));
		if (ret == -ERESTARTSYS)
			return -EINTR;
		/* -ETIME just means "no notification yet"; loop back
		 * around to the deadline check above. */
	}

	if (resp.opcode == KESTRELFS_OP_RESULT_ERROR) {
		/* The daemon explicitly reported a failure for this
		 * request; propagate its errno-style error_code rather
		 * than inventing a generic one. Defensively fall back
		 * to -EIO if a buggy daemon sent 0/positive here. */
		return resp.error_code < 0 ? resp.error_code : -EIO;
	}

	if (copy_to_user(buf, resp.payload, clamped_count))
		return -EFAULT;

	*ppos += clamped_count;
	return clamped_count;
}

/*
 * kestrelfs_remote_llseek() - fops->llseek for remote.txt.
 *
 * Thin wrapper around fixed_size_llseek(): remote.txt has a
 * well-defined logical size (KESTRELFS_REMOTE_FILE_SIZE, see that
 * macro's doc comment), so SEEK_END/SEEK_SET/SEEK_CUR should behave
 * exactly like they would on any other bounded regular file -
 * fixed_size_llseek() already implements that correctly, we only
 * need to supply the size.
 */
static loff_t kestrelfs_remote_llseek(struct file *file, loff_t offset,
				       int whence)
{
	return fixed_size_llseek(file, offset, whence,
				  KESTRELFS_REMOTE_FILE_SIZE);
}

const struct file_operations kestrelfs_remote_file_ops = {
	.owner	= THIS_MODULE,
	.read	= kestrelfs_remote_read,
	.llseek	= kestrelfs_remote_llseek,
};

/*
 * kestrelfs_writable_read() - fops->read for writable.dat (inode 4).
 *
 * Similar to kestrelfs_remote_read, but uses i_size_read() instead of
 * KESTRELFS_REMOTE_FILE_SIZE. This allows writable.dat to have a dynamic
 * size that can be changed via write/truncate operations.
 *
 * Steps:
 *   0. Read current file size from inode->i_size (respects truncate)
 *   1. If *ppos >= i_size, return 0 (EOF)
 *   2. Clamp count to min(requested, 32, remaining_bytes_to_eof)
 *   3. Send READ_CHUNK IPC request to daemon
 *   4. Wait for response and copy data to userspace
 *   5. Advance *ppos and return bytes read
 *
 * Return: bytes read on success, 0 at EOF, negative errno on error.
 */
static ssize_t kestrelfs_writable_read(struct file *file, char __user *buf,
					size_t count, loff_t *ppos)
{
	struct inode *inode = file->f_inode;
	struct kestrelfs_event resp;
	u8 payload[KESTRELFS_EVENT_PAYLOAD_SIZE];
	u64 req_id, file_size;
	u32 clamped_count;
	int ret;

	/* Get current file size (respects truncate) */
	file_size = i_size_read(inode);

	/* EOF check */
	if (*ppos >= file_size)
		return 0;

	if (count == 0)
		return 0;

	/* Clamp to (requested, max_chunk_size, bytes_remaining_to_eof) */
	clamped_count = (u32)min3((u64)count, (u64)KESTRELFS_READ_CHUNK_MAX_LEN,
				   file_size - *ppos);

	/* Build READ_CHUNK request */
	memset(payload, 0, sizeof(payload));
	put_unaligned_le64((u64)inode->i_ino, &payload[0]);
	put_unaligned_le64((u64)*ppos, &payload[8]);
	put_unaligned_le32(clamped_count, &payload[16]);

	ret = kestrelfs_req_push(KESTRELFS_OP_READ_CHUNK, 0, payload, &req_id);
	if (ret)
		return ret;

	/* Wait for response */
	for (;;) {
		ret = kestrelfs_check_resp(req_id, &resp);
		if (ret == 0)
			break;

		ret = kestrelfs_wait_for_resp(msecs_to_jiffies(KESTRELFS_REMOTE_WAIT_MS));
		if (ret == -ERESTARTSYS)
			return -EINTR;
	}

	if (resp.opcode == KESTRELFS_OP_RESULT_ERROR)
		return resp.error_code < 0 ? resp.error_code : -EIO;

	/* Copy data to userspace */
	if (copy_to_user(buf, resp.payload, clamped_count))
		return -EFAULT;

	*ppos += clamped_count;
	return clamped_count;
}

/*
 * kestrelfs_writable_llseek() - fops->llseek for writable.dat.
 *
 * Uses generic_file_llseek which dynamically reads i_size, allowing
 * SEEK_END to reflect the current file size after write/truncate operations.
 */
static loff_t kestrelfs_writable_llseek(struct file *file, loff_t offset,
					 int whence)
{
	return generic_file_llseek(file, offset, whence);
}

/*
 * kestrelfs_writable_write() - handle write(2) on writable.dat.
 *
 * Sends a WRITE_CHUNK IPC request to the daemon with:
 * - inode_id (u64) = file's inode number
 * - offset (u64) = *ppos (adjusted for O_APPEND if needed)
 * - count (u32) = min(len, 12) (limited by IPC payload size)
 * - data (up to 12 bytes)
 *
 * O_APPEND handling: Unlike write_iter-based paths, when a filesystem
 * implements f_op->write directly, the VFS vfs_write() does NOT call
 * generic_write_checks() to automatically translate O_APPEND into
 * "seek to i_size before writing". We must manually check f_flags and
 * update *ppos to i_size when O_APPEND is set, otherwise "echo foo >> file"
 * would incorrectly write at offset 0 instead of appending.
 *
 * Returns number of bytes written on success, negative errno on error.
 */
static ssize_t kestrelfs_writable_write(struct file *filp, const char __user *buf,
					 size_t len, loff_t *ppos)
{
	struct kestrelfs_event resp;
	u8 payload[KESTRELFS_EVENT_PAYLOAD_SIZE];
	unsigned long deadline;
	u64 req_id;
	size_t write_len;
	int ret;

	if (len == 0)
		return 0;

	/* Handle O_APPEND: VFS does not automatically seek to EOF for us
	 * when using f_op->write (only for write_iter). */
	if (filp->f_flags & O_APPEND)
		*ppos = i_size_read(filp->f_inode);

	/* Clamp to max 12 bytes per IPC call */
	write_len = min_t(size_t, len, 12);

	/* Build payload: inode_id@0, offset@8, count@16, data@20 */
	memset(payload, 0, sizeof(payload));
	put_unaligned_le64((u64)filp->f_inode->i_ino, &payload[0]);
	put_unaligned_le64((u64)*ppos, &payload[8]);
	put_unaligned_le32(write_len, &payload[16]);

	/* Copy user data to payload */
	if (copy_from_user(&payload[20], buf, write_len))
		return -EFAULT;

	ret = kestrelfs_req_push(KESTRELFS_OP_WRITE_CHUNK, 0, payload, &req_id);
	if (ret)
		return ret;

	deadline = jiffies + msecs_to_jiffies(KESTRELFS_REMOTE_WAIT_MS);

	for (;;) {
		ret = kestrelfs_check_resp(req_id, &resp);
		if (ret == 0)
			break;

		if (time_after_eq(jiffies, deadline))
			return -ETIMEDOUT;

		ret = kestrelfs_wait_for_resp(msecs_to_jiffies(KESTRELFS_REMOTE_WAIT_MS));
		if (ret == -ERESTARTSYS)
			return -EINTR;
	}

	if (resp.opcode == KESTRELFS_OP_RESULT_ERROR)
		return resp.error_code < 0 ? resp.error_code : -EIO;

	/* Daemon succeeded: update file position */
	*ppos += write_len;

	/* Update i_size if write extended the file.
	 * This ensures stat() reflects the new size after write.
	 * 
	 * DO NOT mark_inode_dirty(): daemon is the authoritative metadata store.
	 * Marking dirty would cause umount to call .write_inode (which we don't
	 * implement), leading to "busy inodes after umount" warnings or hangs.
	 */
	if (*ppos > i_size_read(filp->f_inode)) {
		i_size_write(filp->f_inode, *ppos);
	}

	return write_len;
}

const struct file_operations kestrelfs_writable_file_ops = {
	.owner	= THIS_MODULE,
	.read	= kestrelfs_writable_read,
	.write	= kestrelfs_writable_write,
	.llseek	= kestrelfs_writable_llseek,
};

/**
 * kestrelfs_writable_setattr() - handle setattr for writable.dat (truncate support).
 * @idmap: idmap for user namespace (unused, VFS plumbing)
 * @dentry: dentry of writable.dat
 * @attr: attributes to set
 *
 * This function is called by the VFS when userspace invokes:
 *   - open(..., O_TRUNC) - VFS calls setattr(ATTR_SIZE, 0) after open
 *   - ftruncate(fd, size) - explicit size change
 *   - truncate(path, size) - explicit size change
 *
 * We only handle ATTR_SIZE changes. For other attributes, we use the
 * simple_setattr() helper.
 *
 * Steps:
 *   1. If ATTR_SIZE is set, send KESTRELFS_OP_TRUNCATE IPC to daemon
 *   2. If daemon succeeds, call truncate_setsize() to update kernel i_size
 *   3. Call setattr_copy() to apply other attribute changes
 *
 * Return: 0 on success, negative errno on failure.
 */
static int kestrelfs_writable_setattr(struct mnt_idmap *idmap,
				       struct dentry *dentry,
				       struct iattr *attr)
{
	struct inode *inode = d_inode(dentry);
	int ret;

	/* Handle ATTR_SIZE (truncate/ftruncate) via IPC */
	if (attr->ia_valid & ATTR_SIZE) {
		u64 inode_id = inode->i_ino;
		u64 new_size = attr->ia_size;
		u64 req_id;
		u8 payload[KESTRELFS_EVENT_PAYLOAD_SIZE] = {0};
		struct kestrelfs_event resp;

		/* Build KESTRELFS_OP_TRUNCATE request payload:
		 * inode_id@0 (u64), new_size@8 (u64) */
		put_unaligned_le64(inode_id, &payload[0]);
		put_unaligned_le64(new_size, &payload[8]);

		ret = kestrelfs_req_push(KESTRELFS_OP_TRUNCATE, 0, payload, &req_id);
		if (ret) {
			pr_err("kestrelfs: failed to push TRUNCATE request: %d\n", ret);
			return ret;
		}

		/* Wait for response (2 second timeout) */
		for (;;) {
			ret = kestrelfs_check_resp(req_id, &resp);
			if (ret == 0)
				break;

			ret = kestrelfs_wait_for_resp(msecs_to_jiffies(2000));
			if (ret == -ERESTARTSYS)
				return -EINTR;
			if (ret == -ETIME) {
				pr_err("kestrelfs: TRUNCATE inode=%llu new_size=%llu timeout\n",
				       inode_id, new_size);
				return -ETIMEDOUT;
			}
		}

		if (resp.opcode == KESTRELFS_OP_RESULT_ERROR) {
			pr_err("kestrelfs: TRUNCATE inode=%llu new_size=%llu failed: %d\n",
			       inode_id, new_size, resp.error_code);
			return resp.error_code;
		}

		/* Daemon succeeded, update kernel i_size.
		 * truncate_setsize() handles page cache invalidation. */
		truncate_setsize(inode, new_size);
	}

	/* Apply other attribute changes (mtime, mode, etc.)
	 * 
	 * DO NOT mark_inode_dirty(): daemon is the authoritative metadata store.
	 * Marking dirty would cause umount to call .write_inode (which we don't
	 * implement), leading to "busy inodes after umount" warnings or hangs.
	 */
	setattr_copy(idmap, inode, attr);

	return 0;
}

const struct inode_operations kestrelfs_writable_inode_ops = {
	.setattr = kestrelfs_writable_setattr,
};

/*
 * Unified file_operations and inode_operations for all regular files
 * (Phase 3 Step 7b).
 *
 * These replace the per-file static ops (hello/remote/writable) with
 * generic read/write/llseek/setattr that work on any inode, based on
 * i_ino and dynamic i_size from MemStore.
 */
const struct file_operations kestrelfs_reg_file_ops = {
	.owner	= THIS_MODULE,
	.read	= kestrelfs_writable_read,
	.write	= kestrelfs_writable_write,
	.llseek	= kestrelfs_writable_llseek,
};

const struct inode_operations kestrelfs_reg_inode_ops = {
	.setattr = kestrelfs_writable_setattr,
};
