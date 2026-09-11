// SPDX-License-Identifier: GPL-2.0
/*
 * ipc_ring.c - Lock-free-consumer ring buffer push/pop primitives for
 * the KestrelFS Phase 2 IPC bridge.
 *
 * This file implements the actual data-plane read/write access to
 * the two ring buffers described in kestrelfs_ipc.h
 * (kestrelfs_shared_region.req_slots[] / resp_slots[]). chardev.c
 * only provides the *transport* (mmap, poll, ioctl wakeups); this
 * file provides the *protocol*: how a producer publishes a slot and
 * how a consumer observes it, with the memory-ordering guarantees
 * required for the data to be safely visible across CPUs without a
 * shared lock on the fast (Rust-side) path.
 *
 * PRODUCER/CONSUMER MODEL PER RING
 * =================================
 *
 * Each ring has exactly one intended producer role and one intended
 * consumer role at the protocol level:
 *
 *   REQ ring : kernel is the producer, Rust is the consumer.
 *   RESP ring: Rust is the producer, kernel is the consumer.
 *
 * However, on the KERNEL side, the REQ producer is reached from
 * multiple concurrent VFS call paths (any thread doing a lookup/
 * getattr/read can want to push a request "at the same time"), so
 * kestrelfs_req_push() is NOT a pure single-writer function - it is
 * serialized with a spinlock (req_push_lock) turning the kernel's
 * many producer threads into one logical producer before the
 * lock-free single-writer protocol below ever sees the ring.
 *
 * The RESP consumer (kestrelfs_check_resp(), called from the kernel)
 * is likewise reached from multiple concurrent VFS threads each
 * polling for their own req_id; see the comment on
 * kestrelfs_check_resp() for how "one physical consumer index,
 * multiple logical waiters" is reconciled without a second lock on
 * the fast path attempt.
 *
 * MEMORY ORDERING - THE CORE CONTRACT
 * =====================================
 *
 * head/tail in struct kestrelfs_ring_ctrl are logically atomics
 * shared with a *different address space* (Rust daemon via mmap),
 * so we cannot use kernel atomic_t/atomic64_t (those give no ABI
 * guarantee about representation to a foreign mmap'ing process,
 * only about instruction selection). Instead we treat the plain
 * __u64 fields as raw memory locations and enforce ordering
 * explicitly with smp_store_release()/smp_load_acquire(), exactly
 * as a lock-free SPSC ring between any two independent address
 * spaces must:
 *
 *   PUBLISH SIDE (this file, req producer / resp consumer's tail):
 *     1. Write the full slot contents (kestrelfs_event) into
 *        req_slots[idx] using ordinary stores.
 *     2. THEN publish the new head with smp_store_release(&head, v).
 *        smp_store_release() is a store with release semantics: it
 *        guarantees that step 1's stores are visible to any other
 *        CPU/process that subsequently observes step 2's new head
 *        value via a load with acquire semantics (step 2 acts as a
 *        one-way gate - nothing after it can be reordered before it,
 *        and nothing before it can be reordered after it, from the
 *        perspective of an smp_load_acquire() reader).
 *
 *   OBSERVE SIDE (this file, resp consumer / conceptually the Rust
 *   req consumer, mirrored on their side):
 *     1. Read head with smp_load_acquire(&head).
 *     2. THEN read the slot contents at the now-known-valid indices.
 *        smp_load_acquire() guarantees that step 2's reads cannot be
 *        speculatively hoisted above step 1 by either the compiler
 *        or (on weakly-ordered architectures) the CPU, so we can
 *        never observe a "new" head value while still seeing "old"
 *        (stale/torn) slot contents that were written before the
 *        producer's release store.
 *
 * On x86_64 (our primary target, per Module.symvers/vermagic) both
 * smp_store_release() and smp_load_acquire() compile down to a
 * compiler barrier plus an ordinary MOV (the architecture is already
 * strongly-ordered for these patterns), but the source-level
 * annotations are what make this code portable and, more
 * importantly, correct against COMPILER reordering regardless of
 * architecture - relying on x86's TSO implicitly would be a latent
 * bug on any future ARM64 port.
 *
 * WHY NOT A SECOND SPINLOCK ON THE CONSUME SIDE:
 * A spinlock would work but forces every consumer (including the
 * Rust daemon, which cannot take a kernel spinlock at all) onto a
 * blocking mutual-exclusion primitive for what is fundamentally an
 * SPSC data structure per ring. The acquire/release protocol lets
 * the true single producer and true single consumer of the REQ ring
 * (kernel and Rust respectively) proceed without ever blocking each
 * other; the spinlock in this file exists ONLY to linearize the
 * kernel's multiple VFS-side callers into that single logical
 * producer role, not to protect the ring protocol itself.
 */

#include <linux/module.h>
#include <linux/spinlock.h>
#include <linux/atomic.h>
#include <linux/string.h>
#include <linux/errno.h>
#include <linux/debugfs.h>
#include <linux/seq_file.h>

#include "kestrelfs.h"
#include "kestrelfs_ipc.h"

/*
 * req_push_lock - serializes the kernel's multiple concurrent VFS
 * threads into a single logical REQ-ring producer.
 *
 * Only ever held for the duration of "compute next head, bounds
 * check, memcpy one slot, publish head" - a handful of instructions,
 * making a spinlock (rather than a mutex) the right tool: we never
 * sleep while holding it.
 */
static DEFINE_SPINLOCK(req_push_lock);

/*
 * req_id_counter - global source of unique request identifiers.
 *
 * A plain kernel atomic64_t (NOT part of the cross-language shared
 * memory region - this lives in normal kernel .data, never mapped
 * into the Rust process). atomic64_inc_return() gives us a fully
 * ordered fetch-and-increment, so concurrent VFS threads calling
 * kestrelfs_req_push() concurrently can never observe the same
 * req_id twice.
 */
static atomic64_t req_id_counter = ATOMIC64_INIT(0);

/*
 * kestrelfs_req_push() - publish one request event into the REQ
 * ring (kernel -> Rust).
 * @opcode:      one of KESTRELFS_OP_* (see kestrelfs_ipc.h).
 * @flags:       opaque flags word, copied verbatim into the slot.
 * @payload:     pointer to exactly KESTRELFS_EVENT_PAYLOAD_SIZE bytes
 *               to copy into the slot's payload field, or NULL to
 *               zero-fill the payload instead.
 * @out_req_id:  on success, set to the unique request ID assigned to
 *               this event. The caller must remember this value and
 *               pass it to kestrelfs_check_resp() to retrieve the
 *               matching response later.
 *
 * Locking: serialized by req_push_lock so that multiple concurrent
 * VFS call paths can each safely call this function without
 * corrupting the ring - only one caller at a time computes the next
 * head slot and writes into it.
 *
 * Ordering: the event's full contents (seq/opcode/flags/req_id/
 * payload) are written into req_slots[idx] with ordinary stores
 * BEFORE head is published via smp_store_release(). This guarantees
 * any reader (the Rust daemon, or in principle another kernel
 * consumer) that observes the new head value via smp_load_acquire()
 * is guaranteed to also observe a fully-formed, non-torn slot - the
 * release store acts as the publication point for everything written
 * before it.
 *
 * Return: 0 on success. -EAGAIN if the ring is full (head would
 * catch up to tail - KESTRELFS_RING_SLOTS, i.e. no free slot
 * available - the caller, e.g. a VFS operation, should propagate
 * this as a transient failure and may retry). -EINVAL if the shared
 * region has not been allocated yet (chardev never opened).
 */
int kestrelfs_req_push(u32 opcode, u32 flags, const u8 *payload,
			u64 *out_req_id)
{
	struct kestrelfs_shared_region *region = kestrelfs_shm_region();
	struct kestrelfs_event *slot;
	unsigned long irqflags;
	u64 head, tail, idx, req_id;
	int ret = 0;

	if (!region || !out_req_id)
		return -EINVAL;

	spin_lock_irqsave(&req_push_lock, irqflags);

	/*
	 * Plain reads of head/tail are sufficient here: head is only
	 * ever written by this same lock-protected producer path, and
	 * we only need a fresh-enough view of tail (the consumer's
	 * progress) to decide "is there room" - a stale/lagging tail
	 * only makes us conservatively report -EAGAIN sooner, never
	 * corrupts anything, since the true full/empty decision is
	 * re-validated by the consumer via its own acquire load of
	 * head. We still use READ_ONCE to prevent the compiler from
	 * caching a stale value across the loop/retry boundary below.
	 */
	head = READ_ONCE(region->req_ctrl.head);
	tail = READ_ONCE(region->req_ctrl.tail);

	if (head - tail >= KESTRELFS_RING_SLOTS) {
		ret = -EAGAIN;
		goto out_unlock;
	}

	req_id = (u64)atomic64_inc_return(&req_id_counter);

	idx = head & KESTRELFS_RING_MASK;
	slot = &region->req_slots[idx];

	slot->seq = head;
	slot->opcode = opcode;
	slot->flags = flags;
	slot->req_id = req_id;
	slot->error_code = 0;
	slot->_reserved0 = 0;

	if (payload)
		memcpy(slot->payload, payload, KESTRELFS_EVENT_PAYLOAD_SIZE);
	else
		memset(slot->payload, 0, KESTRELFS_EVENT_PAYLOAD_SIZE);

	/*
	 * Publish: everything written above must be visible to any
	 * reader that observes this new head value. smp_store_release()
	 * is both a compiler barrier and (on non-x86 archs) an actual
	 * memory fence, preventing the slot writes from being reordered
	 * after this store by either the compiler or the CPU.
	 */
	smp_store_release(&region->req_ctrl.head, head + 1);

	*out_req_id = req_id;

out_unlock:
	spin_unlock_irqrestore(&req_push_lock, irqflags);

	if (!ret)
		kestrelfs_wake_req_waiters();

	return ret;
}
EXPORT_SYMBOL_GPL(kestrelfs_req_push);

/*
 * kestrelfs_check_resp() - non-blocking scan for a specific
 * response event on the RESP ring (Rust -> kernel).
 * @req_id:     the request ID previously returned by
 *              kestrelfs_req_push(), identifying which response we
 *              are looking for.
 * @out_event:  on success (return value 0), the full matching event
 *              is copied here.
 *
 * THIS IS A SCANNING, NOT A STRICT FIFO-DEQUEUE, CONSUMER:
 * Because multiple concurrent VFS threads may each be waiting on a
 * *different* req_id at the same time, but the RESP ring has only
 * one tail cursor, we cannot simply "pop the front slot" per caller
 * - the front slot might belong to a different waiter. Instead:
 *
 *   1. Acquire-load resp_ctrl.head to discover how many slots the
 *      Rust producer has published so far (smp_load_acquire()
 *      guarantees every slot in [tail, head) is fully-formed and
 *      safe to read, per the publish-side contract in
 *      kestrelfs_req_push()'s comment, mirrored on the Rust
 *      producer side for the RESP ring).
 *   2. Linearly scan every unconsumed slot in [tail, head) looking
 *      for slot->req_id == @req_id.
 *   3. If found: copy it out. If the found slot happens to be
 *      exactly at index `tail`, we can safely advance tail past it
 *      (smp_store_release) since we know it is now fully consumed.
 *      If the found slot is NOT at the current tail (i.e. some
 *      other, still-unclaimed response(s) sit in front of it), we
 *      deliberately do NOT advance tail - some other concurrent
 *      caller is still expected to claim those earlier slots. The
 *      ring's tail therefore only ever advances past *contiguously
 *      claimed* slots; a caller whose response is buried behind an
 *      as-yet-unclaimed one will simply keep polling and eventually
 *      succeed once the front slot(s) are claimed by their rightful
 *      owners and tail catches up.
 *
 * This function takes req_push_lock as well (despite operating on
 * the *response* ring, not the request ring) to keep the "advance
 * tail" bookkeeping race-free across concurrent kernel-side callers;
 * reusing the same lock is safe and simple at this scale (Phase 2
 * bootstrap) since neither critical section ever sleeps or nests.
 * A dedicated resp_tail_lock can be split out later if contention
 * between REQ producers and RESP consumers becomes measurable.
 *
 * Return: 0 if @req_id's response was found and copied into
 * @out_event. -ENOENT if the ring currently holds no matching
 * response (caller should retry, typically after blocking on
 * kestrelfs_wait_for_resp()). -EINVAL if the shared region has not
 * been allocated yet or @out_event is NULL.
 */
int kestrelfs_check_resp(u64 req_id, struct kestrelfs_event *out_event)
{
	struct kestrelfs_shared_region *region = kestrelfs_shm_region();
	unsigned long irqflags;
	u64 head, tail, idx, scan;
	int ret = -ENOENT;

	if (!region || !out_event)
		return -EINVAL;

	spin_lock_irqsave(&req_push_lock, irqflags);

	tail = READ_ONCE(region->resp_ctrl.tail);

	/*
	 * Acquire load: pairs with the Rust producer's release store
	 * of resp_ctrl.head after it finishes writing a slot's
	 * contents. Guarantees every slot index in [tail, head) below
	 * is safe to read - the Rust side must not publish head until
	 * the slot write is complete, exactly mirroring this file's
	 * own req-ring publish discipline.
	 */
	head = smp_load_acquire(&region->resp_ctrl.head);

	for (scan = tail; scan != head; scan++) {
		idx = scan & KESTRELFS_RING_MASK;

		if (region->resp_slots[idx].req_id != req_id)
			continue;

		memcpy(out_event, &region->resp_slots[idx],
		       sizeof(*out_event));
		ret = 0;

		if (scan == tail) {
			/*
			 * Our match was the oldest unconsumed slot:
			 * safe to advance tail past it immediately.
			 * smp_store_release() ensures our completed
			 * read above is ordered before this
			 * publication, so no other observer can ever
			 * see an advanced tail before the data it
			 * guarded was actually consumed.
			 */
			smp_store_release(&region->resp_ctrl.tail, tail + 1);
		}
		/*
		 * Match was found further back in the ring (some
		 * earlier slot(s) belong to other, still-pending
		 * waiters) - deliberately leave tail untouched; see
		 * function comment above.
		 */
		break;
	}

	spin_unlock_irqrestore(&req_push_lock, irqflags);

	return ret;
}
EXPORT_SYMBOL_GPL(kestrelfs_check_resp);

/* ------------------------------------------------------------------
 * Self-test: debugfs trigger, proves push/scan don't crash and that
 * a round trip (push a REQ, then look for a RESP that will never
 * arrive since no Rust daemon exists yet) behaves exactly as
 * documented (-ENOENT), without touching any VFS code path.
 * ------------------------------------------------------------------ */

static struct dentry *kestrelfs_debugfs_dir;

/*
 * kestrelfs_selftest_push() - debugfs write handler for
 * debugfs/kestrelfs/selftest_push.
 *
 * Writing anything to this file triggers one kestrelfs_req_push()
 * with a KESTRELFS_OP_NOP payload, immediately followed by one
 * kestrelfs_check_resp() call for the freshly-minted req_id. Since
 * nothing (yet) ever produces into the RESP ring, the expected and
 * logged outcome is always "push succeeded, resp not found (-ENOENT)"
 * - this proves the ring plumbing itself (locking, index math,
 * memory ordering annotations, EXPORT_SYMBOL linkage) executes
 * without corrupting state or crashing, ahead of any real consumer
 * existing on either side.
 */
static ssize_t kestrelfs_selftest_push_write(struct file *file,
					      const char __user *buf,
					      size_t count, loff_t *ppos)
{
	struct kestrelfs_event resp;
	u64 req_id = 0;
	int push_ret, resp_ret;

	push_ret = kestrelfs_req_push(KESTRELFS_OP_NOP, 0, NULL, &req_id);
	if (push_ret) {
		pr_info("kestrelfs: selftest_push: kestrelfs_req_push failed: %d\n",
			push_ret);
		return count;
	}

	pr_info("kestrelfs: selftest_push: pushed req_id=%llu opcode=NOP\n",
		(unsigned long long)req_id);

	resp_ret = kestrelfs_check_resp(req_id, &resp);
	if (resp_ret == 0) {
		pr_info("kestrelfs: selftest_push: unexpected resp found for req_id=%llu (opcode=%u error=%d)\n",
			(unsigned long long)req_id, resp.opcode,
			resp.error_code);
	} else {
		pr_info("kestrelfs: selftest_push: kestrelfs_check_resp(req_id=%llu) -> %d (expected -ENOENT, no Rust consumer exists yet)\n",
			(unsigned long long)req_id, resp_ret);
	}

	return count;
}

static const struct file_operations kestrelfs_selftest_push_fops = {
	.owner	= THIS_MODULE,
	.write	= kestrelfs_selftest_push_write,
	.open	= simple_open,
	.llseek	= noop_llseek,
};

/*
 * kestrelfs_ipc_ring_debugfs_init() - create
 * /sys/kernel/debug/kestrelfs/selftest_push.
 *
 * Purely diagnostic; failure to create the debugfs entries (e.g.
 * debugfs not mounted) is logged but never treated as a fatal
 * module load error - production kernels frequently disable
 * debugfs, and this self-test is not required for the IPC bridge
 * itself to function.
 */
static void kestrelfs_ipc_ring_debugfs_init(void)
{
	kestrelfs_debugfs_dir = debugfs_create_dir("kestrelfs", NULL);
	if (IS_ERR(kestrelfs_debugfs_dir)) {
		pr_warn("kestrelfs: debugfs_create_dir failed, self-test trigger unavailable\n");
		kestrelfs_debugfs_dir = NULL;
		return;
	}

	debugfs_create_file("selftest_push", 0200, kestrelfs_debugfs_dir,
			     NULL, &kestrelfs_selftest_push_fops);

	pr_info("kestrelfs: debugfs self-test ready: echo 1 > /sys/kernel/debug/kestrelfs/selftest_push\n");
}

static void kestrelfs_ipc_ring_debugfs_exit(void)
{
	debugfs_remove_recursive(kestrelfs_debugfs_dir);
	kestrelfs_debugfs_dir = NULL;
}

/*
 * kestrelfs_ipc_ring_init() - module-level init for this file,
 * called from super.c's module_init alongside kestrelfs_chardev_init().
 *
 * Only sets up the debugfs self-test hook; the push/pop functions
 * themselves need no initialization beyond the static spinlock and
 * atomic64_t already initialized at compile time above.
 */
void kestrelfs_ipc_ring_init(void)
{
	kestrelfs_ipc_ring_debugfs_init();
}

void kestrelfs_ipc_ring_exit(void)
{
	kestrelfs_ipc_ring_debugfs_exit();
}
