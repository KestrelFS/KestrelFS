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
#include <linux/mutex.h>
#include <linux/uio.h>
#include <linux/limits.h>

#include "kestrelfs.h"

/* One shared bounce buffer means at most one bulk data IPC may be in flight. */
DEFINE_MUTEX(kestrelfs_data_ipc_lock);

/* Must mirror daemon/src/fs_model.rs::CHUNK_SIZE for write slicing. */
#define KESTRELFS_MODEL_CHUNK_SIZE	(64ULL * 1024 * 1024)

/*
 * kestrelfs_ipc_sync_call() - unified IPC helper with total deadline.
 * @req: request event to send
 * @resp: response event to receive
 *
 * Sends IPC request and waits up to 2 seconds total, checking daemon liveness.
 * Returns 0 on success, negative errno on failure.
 */
static int kestrelfs_ipc_sync_call(struct kestrelfs_event *req,
				   struct kestrelfs_event *resp)
{
	u64 req_id;
	unsigned long deadline = jiffies + msecs_to_jiffies(2000);
	int ret;

	/* Check daemon is alive before sending */
	if (!kestrelfs_is_daemon_alive())
		return -ENOTCONN;

	ret = kestrelfs_req_push(req->opcode, req->flags, req->payload, &req_id);
	if (ret)
		return ret;

	/* Wait with total deadline (not per-iteration timeout) */
	while (time_before(jiffies, deadline)) {
		ret = kestrelfs_check_resp(req_id, resp);
		if (ret == 0)
			return 0; /* Success */

		ret = kestrelfs_wait_for_resp(msecs_to_jiffies(100));
		if (ret == -ERESTARTSYS)
			return -EINTR;

		/* Check daemon liveness on each iteration */
		if (!kestrelfs_is_daemon_alive())
			return -ENOTCONN;
	}

	return -ETIMEDOUT;
}

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
 * kestrelfs_writable_read_iter() - fops->read_iter for regular files.
 *
 * Uses an iov_iter for cache hits and ABI v8 READ_DATA requests for misses.
 * The miss path loops in 16 KiB bounce-buffer chunks, allowing one read(2) or
 * readv(2) call to return the caller's full requested range.
 *
 * Return: bytes read on success, 0 at EOF, negative errno on error.
 */
static ssize_t kestrelfs_writable_read_iter(struct kiocb *iocb,
					   struct iov_iter *to)
{
	struct file *file = iocb->ki_filp;
	struct inode *inode = file->f_inode;
	struct kestrelfs_shared_region *region;
	size_t count = iov_iter_count(to);
	loff_t file_size;
	size_t done = 0;
	ssize_t cache_ret;
	u64 miss_epoch;
	int ret;

	if (count == 0)
		return 0;
	if (iocb->ki_pos < 0)
		return -EINVAL;

	/*
	 * Phase 4 fast-path hook.  It deliberately runs before the bounce-buffer
	 * mutex so a cache hit does not serialize with daemon IPC.  The cache
	 * serves a request only when its complete range is indexed;
	 * otherwise READ_DATA remains the authoritative miss path.
	 */
	cache_ret = kestrelfs_cache_read_iter(inode, to, &iocb->ki_pos,
					      &miss_epoch);
	if (cache_ret != -ENODATA)
		return cache_ret;

	ret = mutex_lock_interruptible(&kestrelfs_data_ipc_lock);
	if (ret)
		return ret;

	region = kestrelfs_shm_region();
	if (!region) {
		ret = -ENOTCONN;
		goto out_unlock;
	}

	file_size = i_size_read(inode);
	while (done < count && iocb->ki_pos < file_size) {
		struct kestrelfs_event req = { 0 };
		struct kestrelfs_event resp = { 0 };
		size_t chunk = min_t(size_t, count - done,
					 KESTRELFS_DATA_BUFFER_SIZE);
		u32 actual, requested;

		u64 chunk_offset = iocb->ki_pos;
		size_t copied;

		chunk = min_t(u64, chunk, (u64)(file_size - iocb->ki_pos));
		requested = (u32)chunk;
		req.opcode = KESTRELFS_OP_READ_DATA;
		put_unaligned_le64((u64)inode->i_ino, &req.payload[0]);
		put_unaligned_le64(chunk_offset, &req.payload[8]);
		put_unaligned_le32(requested, &req.payload[16]);

		ret = kestrelfs_ipc_sync_call(&req, &resp);
		if (ret)
			goto out_unlock;
		if (resp.opcode == KESTRELFS_OP_RESULT_ERROR) {
			ret = resp.error_code < 0 ? resp.error_code : -EIO;
			goto out_unlock;
		}
		if (resp.opcode != KESTRELFS_OP_RESULT_OK) {
			ret = -EPROTO;
			goto out_unlock;
		}

		actual = get_unaligned_le32(&resp.payload[0]);
		if (actual > requested || actual > KESTRELFS_DATA_BUFFER_SIZE) {
			ret = -EPROTO;
			goto out_unlock;
		}
		if (actual == 0)
			break;
		copied = copy_to_iter(region->data_buffer, actual, to);
		if (copied != actual) {
			done += copied;
			iocb->ki_pos += copied;
			ret = -EFAULT;
			goto out_unlock;
		}
		kestrelfs_cache_fill(inode, chunk_offset, region->data_buffer,
				       actual, miss_epoch);

		done += actual;
		iocb->ki_pos += actual;
		if (actual < chunk)
			break;
	}

	ret = 0;

out_unlock:
	mutex_unlock(&kestrelfs_data_ipc_lock);
	return done ? (ssize_t)done : ret;
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

static int kestrelfs_regular_open(struct inode *inode, struct file *file)
{
	struct kestrelfs_inode_state *state = inode->i_private;
	int ret = 0;

	if (!state)
		return -EIO;
	mutex_lock(&state->lifecycle_lock);
	if (state->open_handles == UINT_MAX)
		ret = -EMFILE;
	else
		state->open_handles++;
	mutex_unlock(&state->lifecycle_lock);
	return ret;
}

/*
 * Final close is the only reclaim trigger for a retained nlink=0 inode.
 * evict_inode remains IPC-free, preserving bounded unmount behavior.
 */
static int kestrelfs_regular_release(struct inode *inode, struct file *file)
{
	struct kestrelfs_inode_state *state = inode->i_private;
	struct kestrelfs_event req = { 0 };
	struct kestrelfs_event resp = { 0 };
	int ret = 0;

	if (!state)
		return -EIO;

	mutex_lock(&state->lifecycle_lock);
	if (WARN_ON_ONCE(state->open_handles == 0)) {
		ret = -EIO;
		goto out_unlock;
	}
	state->open_handles--;
	if (state->open_handles != 0 || inode->i_nlink != 0)
		goto out_unlock;

	ret = kestrelfs_cache_invalidate_inode(inode->i_ino);
	if (ret)
		goto out_unlock;

	req.opcode = KESTRELFS_OP_FINALIZE_ORPHAN;
	put_unaligned_le64((u64)inode->i_ino, &req.payload[0]);
	ret = kestrelfs_ipc_sync_call(&req, &resp);
	if (!ret && resp.opcode == KESTRELFS_OP_RESULT_ERROR)
		ret = resp.error_code < 0 ? resp.error_code : -EIO;
	else if (!ret && resp.opcode != KESTRELFS_OP_RESULT_OK)
		ret = -EPROTO;

out_unlock:
	mutex_unlock(&state->lifecycle_lock);
	return ret;
}

/*
 * kestrelfs_writable_write() - handle write(2) on writable.dat.
 *
 * Sends ABI v8 WRITE_DATA requests in 16 KiB bounce-buffer chunks. One
 * write(2) therefore completes a large buffer without relying on repeated
 * VFS short writes.
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
	struct kestrelfs_shared_region *region;
	size_t done = 0;
	int ret;

	if (len == 0)
		return 0;
	if (*ppos < 0)
		return -EINVAL;
	/* Persist invalidation before the authoritative write is committed. */
	ret = kestrelfs_cache_invalidate_inode(filp->f_inode->i_ino);
	if (ret)
		return ret;

	ret = mutex_lock_interruptible(&kestrelfs_data_ipc_lock);
	if (ret)
		return ret;

	region = kestrelfs_shm_region();
	if (!region) {
		ret = -ENOTCONN;
		goto out_unlock;
	}

	/* Handle O_APPEND: VFS does not automatically seek to EOF for us
	 * when using f_op->write (only for write_iter). */
	if (filp->f_flags & O_APPEND)
		*ppos = i_size_read(filp->f_inode);

	while (done < len) {
		struct kestrelfs_event req = { 0 };
		struct kestrelfs_event resp = { 0 };
		size_t chunk = min_t(size_t, len - done,
					 KESTRELFS_DATA_BUFFER_SIZE);
		u64 chunk_boundary = KESTRELFS_MODEL_CHUNK_SIZE -
			((u64)*ppos % KESTRELFS_MODEL_CHUNK_SIZE);

		/* A daemon Slice belongs to exactly one 64 MiB logical chunk. */
		chunk = min_t(u64, chunk, chunk_boundary);

		if (copy_from_user(region->data_buffer, buf + done, chunk)) {
			ret = -EFAULT;
			goto out_unlock;
		}

		req.opcode = KESTRELFS_OP_WRITE_DATA;
		put_unaligned_le64((u64)filp->f_inode->i_ino, &req.payload[0]);
		put_unaligned_le64((u64)*ppos, &req.payload[8]);
		put_unaligned_le32((u32)chunk, &req.payload[16]);

		ret = kestrelfs_ipc_sync_call(&req, &resp);
		if (ret)
			goto out_unlock;
		if (resp.opcode == KESTRELFS_OP_RESULT_ERROR) {
			ret = resp.error_code < 0 ? resp.error_code : -EIO;
			goto out_unlock;
		}
		if (resp.opcode != KESTRELFS_OP_RESULT_OK) {
			ret = -EPROTO;
			goto out_unlock;
		}

		done += chunk;
		*ppos += chunk;
		if (*ppos > i_size_read(filp->f_inode))
			i_size_write(filp->f_inode, *ppos);
	}

	ret = 0;

out_unlock:
	mutex_unlock(&kestrelfs_data_ipc_lock);
	return done ? (ssize_t)done : ret;
}

const struct file_operations kestrelfs_writable_file_ops = {
	.owner	= THIS_MODULE,
	.open	= kestrelfs_regular_open,
	.release = kestrelfs_regular_release,
	.read_iter	= kestrelfs_writable_read_iter,
	.write	= kestrelfs_writable_write,
	.llseek	= kestrelfs_writable_llseek,
};

/**
 * kestrelfs_inode_setattr() - persist supported inode attribute changes.
 * @idmap: idmap used by VFS ownership/permission checks
 * @dentry: dentry whose inode is being changed
 * @attr: attributes to set
 *
 * This function is called by the VFS when userspace invokes:
 *   - open(..., O_TRUNC) - VFS calls setattr(ATTR_SIZE, 0) after open
 *   - ftruncate(fd, size) - explicit size change
 *   - truncate(path, size) - explicit size change
 *   - chmod/fchmod - permission and special-bit change
 *   - chown/fchown - numeric owner/group change
 *   - utimensat/futimens/touch - explicit atime/mtime change
 *
 * ATTR_SIZE keeps using OP_TRUNCATE. ABI v19 OP_SETATTR atomically applies a
 * non-empty basic (mode/uid/gid) or time (atime/mtime) group and returns
 * authoritative values. Size and the overlapping basic/time wire layouts are
 * mutually exclusive. Timestamp persistence deliberately has second precision.
 *
 * Return: 0 on success, negative errno on failure.
 */
int kestrelfs_refresh_inode_times(struct inode *inode)
{
	struct kestrelfs_event req = { 0 };
	struct kestrelfs_event resp = { 0 };
	struct timespec64 atime = { .tv_nsec = 0 };
	struct timespec64 mtime = { .tv_nsec = 0 };
	u64 atime_sec;
	u64 mtime_sec;
	int ret;

	req.opcode = KESTRELFS_OP_GETATTR_TIMES;
	put_unaligned_le64(inode->i_ino, &req.payload[0]);
	ret = kestrelfs_ipc_sync_call(&req, &resp);
	if (ret)
		return ret;
	if (resp.opcode == KESTRELFS_OP_RESULT_ERROR)
		return resp.error_code;
	if (resp.opcode != KESTRELFS_OP_RESULT_OK)
		return -EIO;

	atime_sec = get_unaligned_le64(&resp.payload[0]);
	mtime_sec = get_unaligned_le64(&resp.payload[8]);
	if (atime_sec > S64_MAX || mtime_sec > S64_MAX)
		return -EPROTO;
	atime.tv_sec = (time64_t)atime_sec;
	mtime.tv_sec = (time64_t)mtime_sec;
	inode_set_atime_to_ts(inode, atime);
	inode_set_mtime_to_ts(inode, mtime);
	return 0;
}

int kestrelfs_inode_setattr(struct mnt_idmap *idmap, struct dentry *dentry,
			    struct iattr *attr)
{
	struct inode *inode = d_inode(dentry);
	unsigned int unsupported;
	int ret;

	unsupported = attr->ia_valid & ATTR_DELEG;
	if (unsupported)
		return -EOPNOTSUPP;
	if (!(attr->ia_valid & (ATTR_SIZE | ATTR_MODE | ATTR_UID | ATTR_GID |
			      ATTR_ATIME | ATTR_MTIME)))
		return -EOPNOTSUPP;
	/* TRUNCATE remains a separate mutation and the ABI v19 basic/time layouts
	 * overlap, so neither combination can be one atomic request. */
	if ((attr->ia_valid & ATTR_SIZE) &&
	    (attr->ia_valid & (ATTR_MODE | ATTR_UID | ATTR_GID |
			      ATTR_ATIME | ATTR_MTIME)))
		return -EOPNOTSUPP;
	if ((attr->ia_valid & (ATTR_MODE | ATTR_UID | ATTR_GID)) &&
	    (attr->ia_valid & (ATTR_ATIME | ATTR_MTIME)))
		return -EOPNOTSUPP;
	if ((attr->ia_valid & ATTR_SIZE) && !S_ISREG(inode->i_mode))
		return -EISDIR;

	ret = setattr_prepare(idmap, dentry, attr);
	if (ret)
		return ret;

	/* Handle ATTR_SIZE (truncate/ftruncate) via IPC */
	if (attr->ia_valid & ATTR_SIZE) {
		u64 inode_id = inode->i_ino;
		u64 new_size = attr->ia_size;
		struct kestrelfs_event req = { 0 };
		struct kestrelfs_event resp = { 0 };

		/* Fail the mutation if its persistent invalidation cannot commit. */
		ret = kestrelfs_cache_invalidate_inode(inode_id);
		if (ret)
			return ret;

		/* Build KESTRELFS_OP_TRUNCATE request payload:
		 * inode_id@0 (u64), new_size@8 (u64) */
		req.opcode = KESTRELFS_OP_TRUNCATE;
		req.flags = 0;
		memcpy(&req.payload[0], &inode_id, sizeof(u64));
		memcpy(&req.payload[8], &new_size, sizeof(u64));

		/* Use unified sync call with total deadline (2 seconds) */
		ret = kestrelfs_ipc_sync_call(&req, &resp);
		if (ret) {
			pr_err("kestrelfs: TRUNCATE inode=%llu new_size=%llu failed: %d\n",
			       inode_id, new_size, ret);
			return ret;
		}
		if (resp.opcode == KESTRELFS_OP_RESULT_ERROR)
			return resp.error_code;
		if (resp.opcode != KESTRELFS_OP_RESULT_OK)
			return -EIO;

		/* Daemon succeeded, update kernel i_size.
		 * truncate_setsize() handles page cache invalidation. */
		truncate_setsize(inode, new_size);

		/* Ensure no residual dirty pages remain */
		truncate_inode_pages(&inode->i_data, new_size);
	}

	if (attr->ia_valid & (ATTR_MODE | ATTR_UID | ATTR_GID)) {
		u64 inode_id = inode->i_ino;
		u32 valid = 0;
		u32 requested_mode = inode->i_mode;
		u32 requested_uid = i_uid_read(inode);
		u32 requested_gid = i_gid_read(inode);
		u32 persisted_mode;
		u32 persisted_uid;
		u32 persisted_gid;
		struct kestrelfs_event req = { 0 };
		struct kestrelfs_event resp = { 0 };

		if (attr->ia_valid & ATTR_MODE) {
			valid |= KESTRELFS_SETATTR_MODE;
			requested_mode = attr->ia_mode;
		}
		if (attr->ia_valid & ATTR_UID) {
			uid_t uid = from_kuid(i_user_ns(inode), attr->ia_uid);

			if (uid == (uid_t)-1)
				return -EOVERFLOW;
			valid |= KESTRELFS_SETATTR_UID;
			requested_uid = uid;
		}
		if (attr->ia_valid & ATTR_GID) {
			gid_t gid = from_kgid(i_user_ns(inode), attr->ia_gid);

			if (gid == (gid_t)-1)
				return -EOVERFLOW;
			valid |= KESTRELFS_SETATTR_GID;
			requested_gid = gid;
		}

		req.opcode = KESTRELFS_OP_SETATTR;
		put_unaligned_le64(inode_id, &req.payload[0]);
		put_unaligned_le32(valid, &req.payload[8]);
		put_unaligned_le32(requested_mode, &req.payload[12]);
		put_unaligned_le32(requested_uid, &req.payload[16]);
		put_unaligned_le32(requested_gid, &req.payload[20]);

		ret = kestrelfs_ipc_sync_call(&req, &resp);
		if (ret)
			return ret;
		if (resp.opcode == KESTRELFS_OP_RESULT_ERROR)
			return resp.error_code;
		if (resp.opcode != KESTRELFS_OP_RESULT_OK)
			return -EIO;

		persisted_mode = get_unaligned_le32(&resp.payload[0]);
		persisted_uid = get_unaligned_le32(&resp.payload[4]);
		persisted_gid = get_unaligned_le32(&resp.payload[8]);
		if ((persisted_mode & S_IFMT) != (inode->i_mode & S_IFMT))
			return -EPROTO;
		if (valid & KESTRELFS_SETATTR_MODE)
			attr->ia_mode = persisted_mode;
		i_uid_write(inode, persisted_uid);
		i_gid_write(inode, persisted_gid);
	}

	if (attr->ia_valid & (ATTR_ATIME | ATTR_MTIME)) {
		u64 inode_id = inode->i_ino;
		u64 requested_atime;
		u64 requested_mtime;
		u64 persisted_atime;
		u64 persisted_mtime;
		u32 valid = 0;
		struct kestrelfs_event req = { 0 };
		struct kestrelfs_event resp = { 0 };

		if (inode_get_atime_sec(inode) < 0 ||
		    inode_get_mtime_sec(inode) < 0)
			return -EOVERFLOW;
		requested_atime = inode_get_atime_sec(inode);
		requested_mtime = inode_get_mtime_sec(inode);
		if (attr->ia_valid & ATTR_ATIME) {
			if (attr->ia_atime.tv_sec < 0)
				return -EOVERFLOW;
			valid |= KESTRELFS_SETATTR_ATIME;
			requested_atime = attr->ia_atime.tv_sec;
		}
		if (attr->ia_valid & ATTR_MTIME) {
			if (attr->ia_mtime.tv_sec < 0)
				return -EOVERFLOW;
			valid |= KESTRELFS_SETATTR_MTIME;
			requested_mtime = attr->ia_mtime.tv_sec;
		}

		req.opcode = KESTRELFS_OP_SETATTR;
		put_unaligned_le64(inode_id, &req.payload[0]);
		put_unaligned_le32(valid, &req.payload[8]);
		put_unaligned_le64(requested_atime, &req.payload[12]);
		put_unaligned_le64(requested_mtime, &req.payload[20]);

		ret = kestrelfs_ipc_sync_call(&req, &resp);
		if (ret)
			return ret;
		if (resp.opcode == KESTRELFS_OP_RESULT_ERROR)
			return resp.error_code;
		if (resp.opcode != KESTRELFS_OP_RESULT_OK)
			return -EIO;

		persisted_atime = get_unaligned_le64(&resp.payload[0]);
		persisted_mtime = get_unaligned_le64(&resp.payload[8]);
		if (persisted_atime > S64_MAX || persisted_mtime > S64_MAX)
			return -EPROTO;
		if (valid & KESTRELFS_SETATTR_ATIME) {
			attr->ia_atime.tv_sec = (time64_t)persisted_atime;
			attr->ia_atime.tv_nsec = 0;
		}
		if (valid & KESTRELFS_SETATTR_MTIME) {
			attr->ia_mtime.tv_sec = (time64_t)persisted_mtime;
			attr->ia_mtime.tv_nsec = 0;
		}
	}

	/* Apply other attribute changes (mtime, mode, etc.)
	 * 
	 * DO NOT mark_inode_dirty(): daemon is the authoritative metadata store.
	 * VFS may still mark dirty internally via notify_change(), but we handle
	 * that with write_inode() returning 0.
	 */
	setattr_copy(idmap, inode, attr);

	return 0;
}

const struct inode_operations kestrelfs_writable_inode_ops = {
	.setattr = kestrelfs_inode_setattr,
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
	.open	= kestrelfs_regular_open,
	.release = kestrelfs_regular_release,
	.read_iter	= kestrelfs_writable_read_iter,
	.write	= kestrelfs_writable_write,
	.llseek	= kestrelfs_writable_llseek,
};

const struct inode_operations kestrelfs_reg_inode_ops = {
	.setattr = kestrelfs_inode_setattr,
};
