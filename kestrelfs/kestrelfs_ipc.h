/* SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note */
/*
 * kestrelfs_ipc.h - Phase 2 shared IPC contract between the KestrelFS
 * kernel module (data plane) and the KestrelFS Rust daemon (control
 * plane).
 *
 * DESIGN OVERVIEW
 * ================
 *
 * A single mmap()-able region (backed by kernel-allocated pages,
 * exposed through /dev/kestrel_ctl) hosts TWO independent lock-free
 * SPSC ring buffers:
 *
 *   - REQ ring : kernel (producer)  -> Rust daemon (consumer)
 *                Carries requests such as "resolve this metadata
 *                path", "fetch this chunk from S3", etc.
 *
 *   - RESP ring: Rust daemon (producer) -> kernel (consumer)
 *                Carries the corresponding results/completions.
 *
 * Each ring is a classic head/tail lock-free circular buffer of
 * fixed-size slots (struct kestrelfs_event). Slot count MUST be a
 * power of two so that index masking (idx & (KESTRELFS_RING_SLOTS-1))
 * replaces an expensive modulo.
 *
 * Memory layout of the mmap'd region (single contiguous block):
 *
 *   +------------------------------------------------+
 *   | struct kestrelfs_shared_region                 |
 *   |   struct kestrelfs_ring_ctrl req_ctrl           |  (cacheline aligned)
 *   |   struct kestrelfs_ring_ctrl resp_ctrl          |  (cacheline aligned)
 *   |   struct kestrelfs_event req_slots[N]           |
 *   |   struct kestrelfs_event resp_slots[N]          |
 *   +------------------------------------------------+
 *
 * SYNCHRONIZATION / MEMORY ORDERING
 * ==================================
 *
 * This header only defines the *data layout*. The lock-free
 * producer/consumer protocol (memory barriers, acquire/release
 * semantics on head/tail) is implemented separately in the kernel
 * ring-buffer code (C, using smp_store_release()/smp_load_acquire())
 * and in the Rust daemon (using core::sync::atomic with Acquire/
 * Release ordering). Both sides MUST treat head/tail as atomics with
 * at least Acquire/Release ordering - never plain loads/stores.
 *
 * WAKEUP MODEL (avoids busy-spin burning 100% CPU)
 * =================================================
 *
 *  - Kernel -> Rust : after the kernel pushes into the REQ ring, it
 *    calls wake_up_interruptible() on its internal wait_queue_head_t,
 *    which wakes any Rust thread blocked in poll()/epoll() on the
 *    char device (POLLIN becomes ready).
 *
 *  - Rust -> Kernel : after Rust pushes a completion into the RESP
 *    ring, it issues KESTRELFS_IOC_NOTIFY_RESP via ioctl() on the
 *    char device fd. The kernel driver's unlocked_ioctl() handler
 *    then calls wake_up_interruptible() on the wait queue that any
 *    kernel-side thread (e.g. a VFS call blocked waiting for this
 *    specific request's completion) is sleeping on.
 *
 * CROSS-LANGUAGE ALIGNMENT CONTRACT
 * ==================================
 *
 * - Every field uses fixed-width types (__u8/__u16/__u32/__u64) from
 *   <linux/types.h>. Never use plain int/long/size_t here.
 * - All multi-field structs are explicitly padded to be a multiple of
 *   8 bytes and use natural alignment (no bitfields, no packed structs)
 *   so that the Rust #[repr(C)] mirror produces an IDENTICAL layout on
 *   x86_64 without needing #[repr(packed)] (which would kill atomic
 *   access performance on some fields).
 * - Ring control blocks are cacheline-aligned (64 bytes) and placed on
 *   separate cachelines from the slot arrays to avoid false sharing
 *   between the producer and consumer indices.
 * - This file is intentionally written in plain C89-compatible style
 *   with <linux/types.h> so it can be consumed BOTH by the kernel
 *   module (kestrelfs_ipc.h included directly) AND by a `bindgen`
 *   pass on the Rust side (bindgen prefers real headers over manual
 *   transcription to guarantee ABI accuracy). It is also valid
 *   standalone userspace C (see the _Static_assert layout checks
 *   at the bottom, compiled by both gcc-kernel and gcc-userspace).
 */

#ifndef _KESTRELFS_IPC_H
#define _KESTRELFS_IPC_H

#include <linux/types.h>
#include <linux/ioctl.h>

/* ------------------------------------------------------------------
 * Ring buffer sizing
 * ------------------------------------------------------------------ */

/*
 * Number of slots per ring. MUST be a power of two - index masking
 * (idx & (KESTRELFS_RING_SLOTS - 1)) relies on this instead of a
 * modulo operation on the hot path.
 */
#define KESTRELFS_RING_SLOTS		1024

#define KESTRELFS_RING_MASK		(KESTRELFS_RING_SLOTS - 1)

/* Cacheline size assumed for producer/consumer index isolation. */
#define KESTRELFS_CACHELINE_SIZE	64

/* ------------------------------------------------------------------
 * Event opcodes
 * ------------------------------------------------------------------ */

/*
 * kestrelfs_opcode - identifies the meaning of a kestrelfs_event's
 * payload union. Kernel->Rust opcodes and Rust->Kernel opcodes share
 * the same numbering space so a single enum-like set of #defines is
 * enough; the ring a given opcode travels on already disambiguates
 * "is this a request or a response".
 */
#define KESTRELFS_OP_NOP		0	/* no-op / padding event */
#define KESTRELFS_OP_LOOKUP		1	/* req: resolve path -> inode metadata */
#define KESTRELFS_OP_READ_CHUNK		2	/* req: fetch chunk data (cache miss) */
#define KESTRELFS_OP_GETATTR		3	/* req: fetch inode attributes */
#define KESTRELFS_OP_RESULT_OK		64	/* resp: generic success */
#define KESTRELFS_OP_RESULT_ERROR	65	/* resp: generic failure, see error_code */

/* ------------------------------------------------------------------
 * Event payload
 * ------------------------------------------------------------------ */

/*
 * KESTRELFS_EVENT_PAYLOAD_SIZE - size in bytes of the union's largest
 * member. Fixed so that struct kestrelfs_event has a stable, ABI-safe
 * size (KESTRELFS_EVENT_SIZE) regardless of which payload variant is
 * added in the future. Growing this requires a protocol version bump
 * (see kestrelfs_ring_ctrl.abi_version).
 *
 * Chosen so that sizeof(struct kestrelfs_event) == 64 bytes exactly
 * (one full cacheline): the 32 bytes of fixed header fields (seq,
 * opcode, flags, req_id, error_code, _reserved0) plus this payload
 * must sum to KESTRELFS_CACHELINE_SIZE.
 */
#define KESTRELFS_EVENT_PAYLOAD_SIZE	32

/*
 * struct kestrelfs_event - fixed-size slot carried on either ring.
 *
 * @seq:        monotonically increasing sequence number, assigned by
 *              the producer when the slot is published. Used by the
 *              consumer as a sanity check against the head index and
 *              by the requester to match a RESP event back to its
 *              original REQ (req_id echoed back verbatim).
 * @opcode:     one of KESTRELFS_OP_*, see above.
 * @flags:      reserved for future use (e.g. "no reply expected"),
 *              must be zero-initialized for now.
 * @req_id:     unique identifier chosen by the request producer
 *              (kernel side). The Rust daemon MUST echo this value
 *              back unchanged in the corresponding RESP event so the
 *              kernel can wake up the correct waiting context.
 * @error_code: 0 on success; negative errno-style value on failure.
 *              Only meaningful on RESP events.
 * @payload:    opcode-specific data. Interpreted according to
 *              @opcode; raw bytes here so the union itself never
 *              needs to be part of the cross-language ABI - only
 *              the outer fixed-size byte array is.
 *
 * Total size is fixed at KESTRELFS_EVENT_SIZE (see static_assert
 * below) so that ring slot arrays can be indexed with simple pointer
 * arithmetic on both the C and Rust sides.
 */
struct kestrelfs_event {
	__u64	seq;
	__u32	opcode;
	__u32	flags;
	__u64	req_id;
	__s32	error_code;
	__u32	_reserved0;
	__u8	payload[KESTRELFS_EVENT_PAYLOAD_SIZE];
};

#define KESTRELFS_EVENT_SIZE	sizeof(struct kestrelfs_event)

/* ------------------------------------------------------------------
 * Ring control block (head/tail indices)
 * ------------------------------------------------------------------ */

/*
 * struct kestrelfs_ring_ctrl - producer/consumer indices for one
 * ring buffer.
 *
 * @head:  next free slot index to be written by the PRODUCER.
 *         Monotonically increasing, wraps via masking
 *         (head & KESTRELFS_RING_MASK) to obtain the real slot.
 * @tail:  next slot index to be read by the CONSUMER. Same wrapping
 *         rule as @head.
 * @capacity: number of slots in this ring (mirrors
 *         KESTRELFS_RING_SLOTS, stored here too so a future ABI
 *         version could support variable-sized rings without
 *         breaking older readers).
 * @_pad:  explicit padding out to a full cacheline so that @head
 *         (written only by the producer) and @tail (written only by
 *         the consumer) each effectively occupy their own cacheline
 *         footprint at the ring_ctrl granularity, minimizing false
 *         sharing between the two ring_ctrl instances (req_ctrl,
 *         resp_ctrl) that live back-to-back in kestrelfs_shared_region.
 *
 * NOTE: head and tail are logically atomics. The kernel C side must
 * access them with smp_store_release()/smp_load_acquire() (or
 * WRITE_ONCE/READ_ONCE plus explicit barriers); the Rust side must
 * use AtomicU64 with Ordering::Release/Acquire. This header only
 * fixes the byte layout, not the access discipline.
 */
struct kestrelfs_ring_ctrl {
	__u64	head;
	__u64	tail;
	__u32	capacity;
	__u32	_reserved0;
	__u8	_pad[KESTRELFS_CACHELINE_SIZE - (2 * sizeof(__u64) + 2 * sizeof(__u32))];
} __attribute__((aligned(KESTRELFS_CACHELINE_SIZE)));

/* ------------------------------------------------------------------
 * Full shared memory region layout
 * ------------------------------------------------------------------ */

/*
 * KESTRELFS_ABI_VERSION - bump whenever the layout of
 * kestrelfs_shared_region, kestrelfs_ring_ctrl or kestrelfs_event
 * changes in a way that is not purely additive-and-reserved-field
 * based. The Rust daemon must refuse to attach if this does not
 * match what it was built against.
 */
#define KESTRELFS_ABI_VERSION		1

/*
 * struct kestrelfs_shared_region - the entire mmap'd layout.
 *
 * @magic:        must equal KESTRELFS_SHM_MAGIC; lets the Rust daemon
 *                sanity-check it actually mapped a KestrelFS char
 *                device region and not garbage.
 * @abi_version:  must equal KESTRELFS_ABI_VERSION.
 * @req_ctrl:     head/tail for the kernel->Rust request ring.
 * @resp_ctrl:    head/tail for the Rust->kernel response ring.
 * @req_slots:    fixed-size array of request event slots.
 * @resp_slots:   fixed-size array of response event slots.
 *
 * This whole struct is what gets mmap()-ed by the Rust daemon over
 * the /dev/kestrel_ctl char device. Its total size
 * (KESTRELFS_SHM_REGION_SIZE) determines how many pages the kernel
 * driver must allocate and how large an mmap() length userspace must
 * request.
 */
struct kestrelfs_shared_region {
	__u32	magic;
	__u32	abi_version;
	__u8	_pad0[KESTRELFS_CACHELINE_SIZE - 2 * sizeof(__u32)];

	struct kestrelfs_ring_ctrl	req_ctrl;
	struct kestrelfs_ring_ctrl	resp_ctrl;

	struct kestrelfs_event		req_slots[KESTRELFS_RING_SLOTS];
	struct kestrelfs_event		resp_slots[KESTRELFS_RING_SLOTS];
};

#define KESTRELFS_SHM_MAGIC		0x4B535253u	/* "KSRS" */

#define KESTRELFS_SHM_REGION_SIZE	sizeof(struct kestrelfs_shared_region)

/* ------------------------------------------------------------------
 * ioctl commands (char device: /dev/kestrel_ctl)
 * ------------------------------------------------------------------
 *
 * Magic number 0xE0 is currently unused according to
 * Documentation/userspace-api/ioctl/ioctl-number.rst as of Linux 6.12.
 */

#define KESTRELFS_IOC_MAGIC		0xE0

/*
 * KESTRELFS_IOC_NOTIFY_RESP - Rust daemon informs the kernel driver
 * that it has just published one or more new events into the RESP
 * ring. No argument: the kernel re-reads resp_ctrl.head itself and
 * wakes every kernel thread waiting on a matching req_id.
 */
#define KESTRELFS_IOC_NOTIFY_RESP	_IO(KESTRELFS_IOC_MAGIC, 1)

/*
 * KESTRELFS_IOC_GET_ABI_VERSION - read back KESTRELFS_ABI_VERSION
 * from the running kernel module, so the Rust daemon can refuse to
 * proceed on a mismatch before doing anything else with the mapped
 * region.
 */
#define KESTRELFS_IOC_GET_ABI_VERSION	_IOR(KESTRELFS_IOC_MAGIC, 2, __u32)

/*
 * KESTRELFS_IOC_GET_REGION_SIZE - read back
 * KESTRELFS_SHM_REGION_SIZE, so userspace can size its mmap() call
 * without hardcoding the constant on both sides.
 */
#define KESTRELFS_IOC_GET_REGION_SIZE	_IOR(KESTRELFS_IOC_MAGIC, 3, __u64)

/* ------------------------------------------------------------------
 * Compile-time layout guarantees (checked under BOTH kernel-C and
 * plain userspace gcc, see Phase 2 verification notes)
 * ------------------------------------------------------------------ */

_Static_assert(sizeof(struct kestrelfs_event) == 64,
		"kestrelfs_event must be exactly 64 bytes (one cacheline)");

_Static_assert((KESTRELFS_RING_SLOTS & (KESTRELFS_RING_SLOTS - 1)) == 0,
		"KESTRELFS_RING_SLOTS must be a power of two");

_Static_assert(sizeof(struct kestrelfs_ring_ctrl) == KESTRELFS_CACHELINE_SIZE,
		"kestrelfs_ring_ctrl must be exactly one cacheline");

_Static_assert(sizeof(struct kestrelfs_shared_region) ==
		KESTRELFS_CACHELINE_SIZE +
		(2 * KESTRELFS_CACHELINE_SIZE) +
		(2 * KESTRELFS_RING_SLOTS * sizeof(struct kestrelfs_event)),
		"kestrelfs_shared_region layout drifted, check padding");

#endif /* _KESTRELFS_IPC_H */
