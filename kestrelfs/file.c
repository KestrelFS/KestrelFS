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
#include <linux/pagemap.h>
#include <linux/vmalloc.h>
#include <linux/filelock.h>
#include <linux/writeback.h>
#include <linux/highmem.h>

#include "kestrelfs.h"

/* One shared bounce buffer means at most one bulk data IPC may be in flight. */
DEFINE_MUTEX(kestrelfs_data_ipc_lock);
static atomic64_t kestrelfs_pagecache_coherence_epoch = ATOMIC64_INIT(0);
static DEFINE_MUTEX(kestrelfs_coherence_inodes_lock);
static LIST_HEAD(kestrelfs_coherence_inodes);

/* The daemon's ioctl must not wait for a folio whose READ_DATA request is
 * waiting on that same daemon. Retire mapped folios on a worker instead.
 * The per-inode scheduled flag and epoch comparison close the queue_work()
 * race when a second revision arrives while a worker is still running.
 */
static void kestrelfs_pagecache_coherence_work(struct work_struct *work)
{
	struct kestrelfs_inode_state *state = container_of(work,
			struct kestrelfs_inode_state, coherence_work);
	u64 epoch;

	for (;;) {
		epoch = atomic64_read(&kestrelfs_pagecache_coherence_epoch);
		inode_lock(state->inode);
		truncate_inode_pages(state->inode->i_mapping, 0);
		WRITE_ONCE(state->pagecache_coherence_epoch, epoch);
		inode_unlock(state->inode);
		wake_up_all(&state->coherence_wait);

		mutex_lock(&kestrelfs_coherence_inodes_lock);
		if (epoch == atomic64_read(&kestrelfs_pagecache_coherence_epoch)) {
			state->coherence_work_scheduled = false;
			mutex_unlock(&kestrelfs_coherence_inodes_lock);
			return;
		}
		mutex_unlock(&kestrelfs_coherence_inodes_lock);
	}
}

void kestrelfs_pagecache_register_inode(struct inode *inode)
{
	struct kestrelfs_inode_state *state = inode->i_private;

	state->inode = inode;
	INIT_LIST_HEAD(&state->coherence_link);
	INIT_WORK(&state->coherence_work, kestrelfs_pagecache_coherence_work);
	init_waitqueue_head(&state->coherence_wait);
	state->pagecache_coherence_epoch =
		atomic64_read(&kestrelfs_pagecache_coherence_epoch);
	mutex_lock(&kestrelfs_coherence_inodes_lock);
	list_add(&state->coherence_link, &kestrelfs_coherence_inodes);
	mutex_unlock(&kestrelfs_coherence_inodes_lock);
}

void kestrelfs_pagecache_unregister_inode(struct inode *inode)
{
	struct kestrelfs_inode_state *state = inode->i_private;

	mutex_lock(&kestrelfs_coherence_inodes_lock);
	list_del(&state->coherence_link);
	mutex_unlock(&kestrelfs_coherence_inodes_lock);
	cancel_work_sync(&state->coherence_work);
}

static void kestrelfs_pagecache_schedule_coherence(
	struct kestrelfs_inode_state *state)
{
	mutex_lock(&kestrelfs_coherence_inodes_lock);
	if (!state->coherence_work_scheduled) {
		state->coherence_work_scheduled = true;
		queue_work(system_unbound_wq, &state->coherence_work);
	}
	mutex_unlock(&kestrelfs_coherence_inodes_lock);
}

/* Remote Redis revision retirement must also retire read-side filemap pages.
 * The daemon's ioctl cannot wait for a folio locked by a READ_DATA request
 * serviced by that same daemon. Plain reads retire lazily; mapped inodes use
 * a worker to zap existing PTEs without blocking the daemon's ioctl.
 */
void kestrelfs_pagecache_coherence_advance(void)
{
	struct kestrelfs_inode_state *state;

	atomic64_inc(&kestrelfs_pagecache_coherence_epoch);
	mutex_lock(&kestrelfs_coherence_inodes_lock);
	list_for_each_entry(state, &kestrelfs_coherence_inodes,
			    coherence_link) {
		if (!mapping_mapped(state->inode->i_mapping) ||
		    state->coherence_work_scheduled)
			continue;
		state->coherence_work_scheduled = true;
		queue_work(system_unbound_wq, &state->coherence_work);
	}
	mutex_unlock(&kestrelfs_coherence_inodes_lock);
}

static ssize_t kestrelfs_regular_read_iter(struct kiocb *iocb,
					    struct iov_iter *to)
{
	struct inode *inode = file_inode(iocb->ki_filp);
	struct kestrelfs_inode_state *state = inode->i_private;
	u64 epoch;

	if (!state)
		return -EIO;
	/* The warm path is lockless with respect to the daemon/bounce buffer.
	 * i_rwsem serializes one lazy retirement with synchronous writes.
	 */
	epoch = atomic64_read(&kestrelfs_pagecache_coherence_epoch);
	if (READ_ONCE(state->pagecache_coherence_epoch) != epoch) {
		inode_lock(inode);
		epoch = atomic64_read(&kestrelfs_pagecache_coherence_epoch);
		if (state->pagecache_coherence_epoch != epoch) {
			truncate_inode_pages(inode->i_mapping, 0);
			WRITE_ONCE(state->pagecache_coherence_epoch, epoch);
			wake_up_all(&state->coherence_wait);
		}
		inode_unlock(inode);
	}
	return generic_file_read_iter(iocb, to);
}

static vm_fault_t kestrelfs_regular_mmap_fault(struct vm_fault *vmf)
{
	struct kestrelfs_inode_state *state =
		file_inode(vmf->vma->vm_file)->i_private;

	if (!state)
		return VM_FAULT_SIGBUS;
	if (READ_ONCE(state->pagecache_coherence_epoch) !=
	    atomic64_read(&kestrelfs_pagecache_coherence_epoch)) {
		kestrelfs_pagecache_schedule_coherence(state);
		/* A fault cannot wait with mmap_lock/VMA lock held: the worker
		 * needs that lock to zap an old PTE. Retry after it retires pages.
		 */
		if (!fault_flag_allow_retry_first(vmf->flags))
			return VM_FAULT_SIGBUS;
		if (vmf->flags & FAULT_FLAG_RETRY_NOWAIT)
			return VM_FAULT_RETRY;
		release_fault_lock(vmf);
		wait_event(state->coherence_wait,
			READ_ONCE(state->pagecache_coherence_epoch) ==
			atomic64_read(&kestrelfs_pagecache_coherence_epoch));
		return VM_FAULT_RETRY;
	}
	return filemap_fault(vmf);
}

static const struct vm_operations_struct kestrelfs_regular_vm_ops = {
	.fault = kestrelfs_regular_mmap_fault,
};

static int kestrelfs_regular_mmap(struct file *file,
				  struct vm_area_struct *vma)
{
	struct kestrelfs_inode_state *state = file_inode(file)->i_private;
	int ret;

	if (!state)
		return -EIO;
	if ((vma->vm_flags & VM_SHARED) && (vma->vm_flags & VM_WRITE))
		return -EOPNOTSUPP;
	/* A read-only shared VMA must not become writable via mprotect(). */
	if (vma->vm_flags & VM_SHARED)
		vm_flags_clear(vma, VM_MAYWRITE);
	ret = generic_file_mmap(file, vma);
	if (ret)
		return ret;
	vma->vm_ops = &kestrelfs_regular_vm_ops;
	if (READ_ONCE(state->pagecache_coherence_epoch) !=
	    atomic64_read(&kestrelfs_pagecache_coherence_epoch))
		kestrelfs_pagecache_schedule_coherence(state);
	return 0;
}

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

int kestrelfs_sync_daemon(u32 opcode, u64 inode_id)
{
	struct kestrelfs_event req = { 0 };
	struct kestrelfs_event resp = { 0 };
	int ret;

	if (opcode != KESTRELFS_OP_FSYNC && opcode != KESTRELFS_OP_SYNC_FS)
		return -EINVAL;
	ret = mutex_lock_interruptible(&kestrelfs_data_ipc_lock);
	if (ret)
		return ret;
	req.opcode = opcode;
	if (opcode == KESTRELFS_OP_FSYNC)
		put_unaligned_le64(inode_id, &req.payload[0]);
	ret = kestrelfs_ipc_sync_call(&req, &resp);
	mutex_unlock(&kestrelfs_data_ipc_lock);
	if (ret)
		return ret;
	if (resp.opcode == KESTRELFS_OP_RESULT_ERROR)
		return resp.error_code < 0 ? resp.error_code : -EIO;
	return resp.opcode == KESTRELFS_OP_RESULT_OK ? 0 : -EPROTO;
}

static int kestrelfs_regular_fsync(struct file *file, loff_t start,
				   loff_t end, int datasync)
{
	int ret;

	ret = file_write_and_wait_range(file, start, end);
	if (ret)
		return ret;
	/* The daemon barrier follows successful folio writeback. */
	return kestrelfs_sync_daemon(KESTRELFS_OP_FSYNC, file_inode(file)->i_ino);
}

static int kestrelfs_regular_flush(struct file *file, fl_owner_t id)
{
	/* Preserve close/reopen behavior without claiming fsync durability. */
	return file_write_and_wait(file);
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

/* Fill one locked folio from the kernel block cache or authoritative daemon.
 * Cache hits use the cache rwsem and can overlap on different folios. Only a
 * miss needs the bounce mutex; recheck the cache once acquired because another
 * reader may have filled it while this folio waited.
 */
static int kestrelfs_fill_folio_locked(struct file *file,
				      struct folio *folio)
{
	struct inode *inode = file_inode(file);
	struct kestrelfs_shared_region *region;
	loff_t offset = folio_pos(folio);
	loff_t size;
	size_t length = folio_size(folio);
	size_t wanted, done = 0;
	u8 *buffer;
	int ret = 0;

	buffer = kvzalloc(length, GFP_KERNEL);
	if (!buffer) {
		ret = -ENOMEM;
		goto out;
	}
	size = i_size_read(inode);
	wanted = offset < size ? min_t(u64, length, size - offset) : 0;
	while (done < wanted) {
		struct kvec vec;
		struct iov_iter iter;
		struct kestrelfs_event req = { 0 };
		struct kestrelfs_event resp = { 0 };
		loff_t pos = offset + done;
		size_t chunk = min_t(size_t, wanted - done,
					  KESTRELFS_DATA_BUFFER_SIZE);
		u64 miss_epoch;
		ssize_t hit;
		u32 actual;

		vec.iov_base = buffer + done;
		vec.iov_len = chunk;
		iov_iter_kvec(&iter, ITER_DEST, &vec, 1, chunk);
		hit = kestrelfs_cache_read_iter(inode, &iter, &pos,
						&miss_epoch);
		if (hit >= 0) {
			if (hit != chunk) {
				ret = -EIO;
				goto out_copy;
			}
			done += hit;
			continue;
		}
		if (hit != -ENODATA) {
			ret = hit;
			goto out_copy;
		}

		mutex_lock(&kestrelfs_data_ipc_lock);
		size = i_size_read(inode);
		if (pos >= size) {
			mutex_unlock(&kestrelfs_data_ipc_lock);
			break;
		}
		chunk = min_t(u64, chunk, size - pos);
		vec.iov_len = chunk;
		iov_iter_kvec(&iter, ITER_DEST, &vec, 1, chunk);
		hit = kestrelfs_cache_read_iter(inode, &iter, &pos,
						&miss_epoch);
		if (hit >= 0) {
			mutex_unlock(&kestrelfs_data_ipc_lock);
			if (hit != chunk) {
				ret = -EIO;
				goto out_copy;
			}
			done += hit;
			continue;
		}
		if (hit != -ENODATA) {
			ret = hit;
			goto out_unlock_mutex;
		}
		region = kestrelfs_shm_region();
		if (!region) {
			ret = -ENOTCONN;
			goto out_unlock_mutex;
		}
		req.opcode = KESTRELFS_OP_READ_DATA;
		put_unaligned_le64(inode->i_ino, &req.payload[0]);
		put_unaligned_le64(pos, &req.payload[8]);
		put_unaligned_le32(chunk, &req.payload[16]);
		ret = kestrelfs_ipc_sync_call(&req, &resp);
		if (ret)
			goto out_unlock_mutex;
		if (resp.opcode == KESTRELFS_OP_RESULT_ERROR) {
			ret = resp.error_code < 0 ? resp.error_code : -EIO;
			goto out_unlock_mutex;
		}
		if (resp.opcode != KESTRELFS_OP_RESULT_OK) {
			ret = -EPROTO;
			goto out_unlock_mutex;
		}
		actual = get_unaligned_le32(&resp.payload[0]);
		if (actual != chunk) {
			ret = -EIO;
			goto out_unlock_mutex;
		}
		memcpy(buffer + done, region->data_buffer, actual);
		kestrelfs_cache_fill(inode, pos, region->data_buffer, actual,
					miss_epoch);
		done += actual;
		mutex_unlock(&kestrelfs_data_ipc_lock);
	}
	goto out_copy;
out_unlock_mutex:
	mutex_unlock(&kestrelfs_data_ipc_lock);
out_copy:
	if (!ret) {
		memcpy_to_folio(folio, 0, buffer, length);
		folio_mark_uptodate(folio);
	}
	kvfree(buffer);
out:
	return ret;
}

static int kestrelfs_read_folio(struct file *file, struct folio *folio)
{
	int ret = kestrelfs_fill_folio_locked(file, folio);

	folio_unlock(folio);
	return ret;
}

static void kestrelfs_readahead(struct readahead_control *ractl)
{
	struct folio *folio;

	while ((folio = readahead_folio(ractl)))
		kestrelfs_read_folio(ractl->file, folio);
}

static int kestrelfs_write_begin(struct file *file,
				 struct address_space *mapping, loff_t pos,
				 unsigned int len, struct folio **foliop,
				 void **fsdata)
{
	struct folio *folio;
	int ret;

	folio = filemap_grab_folio(mapping, pos >> PAGE_SHIFT);
	if (IS_ERR(folio))
		return PTR_ERR(folio);
	if (!folio_test_uptodate(folio)) {
		ret = kestrelfs_fill_folio_locked(file, folio);
		if (ret) {
			folio_unlock(folio);
			folio_put(folio);
			return ret;
		}
	}
	*foliop = folio;
	return 0;
}

static int kestrelfs_write_end(struct file *file,
			       struct address_space *mapping, loff_t pos,
			       unsigned int len, unsigned int copied,
			       struct folio *folio, void *fsdata)
{
	struct inode *inode = mapping->host;

	if (copied) {
		loff_t end = pos + copied;

		if (end > i_size_read(inode))
			i_size_write(inode, end);
		folio_mark_uptodate(folio);
		folio_mark_dirty(folio);
	}
	folio_unlock(folio);
	folio_put(folio);
	return copied;
}

static int kestrelfs_write_folio(struct folio *folio,
				 struct writeback_control *wbc, void *data)
{
	struct inode *inode = folio->mapping->host;
	struct kestrelfs_shared_region *region;
	loff_t offset = folio_pos(folio);
	loff_t size = i_size_read(inode);
	size_t length = offset < size ? min_t(u64, folio_size(folio),
							 size - offset) : 0;
	size_t done = 0;
	int ret = 0;

	if (!length)
		goto out_unlock;
	folio_start_writeback(folio);
	mutex_lock(&kestrelfs_data_ipc_lock);
	region = kestrelfs_shm_region();
	if (!region) {
		ret = -ENOTCONN;
		goto out_mutex;
	}
	/* A dirty folio must not leave a stale persistent read-cache entry. */
	ret = kestrelfs_cache_invalidate_inode(inode->i_ino);
	if (ret)
		goto out_mutex;
	while (done < length) {
		struct kestrelfs_event req = { 0 };
		struct kestrelfs_event resp = { 0 };
		size_t chunk = min_t(size_t, length - done,
					  KESTRELFS_DATA_BUFFER_SIZE);
		u64 chunk_boundary = KESTRELFS_MODEL_CHUNK_SIZE -
			((u64)(offset + done) % KESTRELFS_MODEL_CHUNK_SIZE);

		chunk = min_t(u64, chunk, chunk_boundary);
		memcpy_from_folio(region->data_buffer, folio, done, chunk);
		req.opcode = KESTRELFS_OP_WRITE_DATA;
		put_unaligned_le64(inode->i_ino, &req.payload[0]);
		put_unaligned_le64(offset + done, &req.payload[8]);
		put_unaligned_le32(chunk, &req.payload[16]);
		ret = kestrelfs_ipc_sync_call(&req, &resp);
		if (ret)
			break;
		if (resp.opcode == KESTRELFS_OP_RESULT_ERROR) {
			ret = resp.error_code < 0 ? resp.error_code : -EIO;
			break;
		}
		if (resp.opcode != KESTRELFS_OP_RESULT_OK) {
			ret = -EPROTO;
			break;
		}
		done += chunk;
	}
out_mutex:
	mutex_unlock(&kestrelfs_data_ipc_lock);
	if (ret) {
		folio_redirty_for_writepage(wbc, folio);
		mapping_set_error(folio->mapping, ret);
	}
	folio_end_writeback(folio);
out_unlock:
	folio_unlock(folio);
	return ret;
}

static int kestrelfs_writepages(struct address_space *mapping,
				struct writeback_control *wbc)
{
	return write_cache_pages(mapping, wbc, kestrelfs_write_folio, NULL);
}

const struct address_space_operations kestrelfs_reg_aops = {
	.read_folio = kestrelfs_read_folio,
	.readahead = kestrelfs_readahead,
	.write_begin = kestrelfs_write_begin,
	.write_end = kestrelfs_write_end,
	.writepages = kestrelfs_writepages,
	.dirty_folio = filemap_dirty_folio,
};

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

/* Local VFS locks intentionally never enter the daemon/data IPC path.  The
 * kernel owns waiter queues and close/exit cleanup for this mount's inode.
 * POSIX and OFD byte-range locks share the POSIX lock context; BSD flock is
 * a separate lock class, as on a local Linux filesystem.
 */
static int kestrelfs_regular_lock(struct file *file, int cmd,
				   struct file_lock *fl)
{
	if (cmd == F_GETLK) {
		posix_test_lock(file, fl);
		return 0;
	}
	if (cmd != F_SETLK && cmd != F_SETLKW)
		return -EINVAL;
	return locks_lock_file_wait(file, fl);
}

static int kestrelfs_regular_flock(struct file *file, int cmd,
				    struct file_lock *fl)
{
	if (cmd != F_SETLK && cmd != F_SETLKW)
		return -EINVAL;
	return locks_lock_file_wait(file, fl);
}

/* Buffered writes dirty filemap folios. A writeback callback persists each
 * folio through WRITE_DATA; this minimal write-through policy waits before
 * returning from write_iter and keeps clean pages in filemap thereafter.
 * fsync and close also wait, so future dirty-folio sources stay covered.
 * Invalidate the NVMe read cache before and after the dirtying operation, so
 * concurrent old READ_DATA fills cannot remain indexed after this write.
 */
static ssize_t kestrelfs_writable_write_iter(struct kiocb *iocb,
					      struct iov_iter *from)
{
	struct inode *inode = file_inode(iocb->ki_filp);
	ssize_t checked;
	int ret;

	inode_lock(inode);
	checked = generic_write_checks(iocb, from);
	if (checked <= 0)
		goto out_checked;
	/* write_begin may fetch an old partial folio from the daemon. */
	if (iocb->ki_flags & IOCB_NOWAIT) {
		ret = -EOPNOTSUPP;
		goto out;
	}
	ret = mutex_lock_interruptible(&kestrelfs_data_ipc_lock);
	if (ret)
		goto out;
	ret = kestrelfs_cache_invalidate_inode(inode->i_ino);
	mutex_unlock(&kestrelfs_data_ipc_lock);
	if (ret)
		goto out;
	ret = file_update_time(iocb->ki_filp);
	if (ret)
		goto out;
	checked = generic_perform_write(iocb, from);
	mutex_lock(&kestrelfs_data_ipc_lock);
	ret = kestrelfs_cache_invalidate_inode(inode->i_ino);
	mutex_unlock(&kestrelfs_data_ipc_lock);
	if (ret) {
		if (checked > 0)
			mapping_set_error(inode->i_mapping, ret);
		goto out;
	}
	if (checked > 0) {
		ret = filemap_write_and_wait(inode->i_mapping);
		if (ret)
			goto out;
	}
out_checked:
	inode_unlock(inode);
	return checked > 0 ? generic_write_sync(iocb, checked) : checked;
out:
	inode_unlock(inode);
	return ret;
}

const struct file_operations kestrelfs_writable_file_ops = {
	.owner	= THIS_MODULE,
	.open	= kestrelfs_regular_open,
	.release = kestrelfs_regular_release,
	.flush = kestrelfs_regular_flush,
	.lock	= kestrelfs_regular_lock,
	.flock	= kestrelfs_regular_flock,
	.read_iter	= kestrelfs_regular_read_iter,
	.mmap	= kestrelfs_regular_mmap,
	.write_iter	= kestrelfs_writable_write_iter,
	.fsync	= kestrelfs_regular_fsync,
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
	/* A normal truncate carries an implicit ATTR_MTIME/ATTR_CTIME from VFS.
	 * OP_TRUNCATE already updates mtime in MetaStore. Explicit time changes
	 * (ATTR_MTIME_SET) and other attributes still cannot be one mutation.
	 */
	if ((attr->ia_valid & ATTR_SIZE) &&
	    (attr->ia_valid & (ATTR_MODE | ATTR_UID | ATTR_GID |
			      ATTR_ATIME | ATTR_MTIME_SET)))
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

		/* Commit dirty folios before changing the authoritative size. */
		ret = filemap_write_and_wait(inode->i_mapping);
		if (ret)
			return ret;
		/* Order a cold folio fill against truncate just like a write. */
		ret = mutex_lock_interruptible(&kestrelfs_data_ipc_lock);
		if (ret)
			return ret;
		/* Fail the mutation if its persistent invalidation cannot commit. */
		ret = kestrelfs_cache_invalidate_inode(inode_id);
		if (ret) {
			mutex_unlock(&kestrelfs_data_ipc_lock);
			return ret;
		}

		/* Build KESTRELFS_OP_TRUNCATE request payload:
		 * inode_id@0 (u64), new_size@8 (u64) */
		req.opcode = KESTRELFS_OP_TRUNCATE;
		req.flags = 0;
		memcpy(&req.payload[0], &inode_id, sizeof(u64));
		memcpy(&req.payload[8], &new_size, sizeof(u64));

		/* Use unified sync call with total deadline (2 seconds) */
		ret = kestrelfs_ipc_sync_call(&req, &resp);
		mutex_unlock(&kestrelfs_data_ipc_lock);
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

	if (!(attr->ia_valid & ATTR_SIZE) &&
	    (attr->ia_valid & (ATTR_ATIME | ATTR_MTIME))) {
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
	.flush = kestrelfs_regular_flush,
	.lock	= kestrelfs_regular_lock,
	.flock	= kestrelfs_regular_flock,
	.read_iter	= kestrelfs_regular_read_iter,
	.mmap	= kestrelfs_regular_mmap,
	.write_iter	= kestrelfs_writable_write_iter,
	.fsync	= kestrelfs_regular_fsync,
	.llseek	= kestrelfs_writable_llseek,
};

const struct inode_operations kestrelfs_reg_inode_ops = {
	.setattr = kestrelfs_inode_setattr,
};
