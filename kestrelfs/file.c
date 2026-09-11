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
 *   2. Encode (*ppos, clamped count) into a request payload and
 *      push it via kestrelfs_req_push(KESTRELFS_OP_READ_CHUNK, ...).
 *      A full ring is surfaced to the caller as -EAGAIN, matching
 *      the same errno a normal socket/pipe read would use for "try
 *      again later" backpressure.
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
	put_unaligned_le64((u64)*ppos, &payload[0]);
	put_unaligned_le32(clamped_count, &payload[8]);

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
