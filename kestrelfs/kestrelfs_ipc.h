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
 *   |   __u8 data_buffer[16384]                        |
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

/*
 * Single shared bounce buffer for bulk file I/O and names. Data/name IPC is
 * synchronous and serialized, so exactly one *_DATA request owns these bytes
 * at a time.
 */
#define KESTRELFS_DATA_BUFFER_SIZE	(16 * 1024)

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
#define KESTRELFS_OP_WRITE_CHUNK	4	/* req: write data to file */
#define KESTRELFS_OP_TRUNCATE		5	/* req: set file size (truncate/ftruncate) */
#define KESTRELFS_OP_CREATE		6	/* req: create new file/directory */
#define KESTRELFS_OP_READDIR		7	/* req: list directory entries */
#define KESTRELFS_OP_MKDIR		8	/* req: create new directory */
#define KESTRELFS_OP_UNLINK		9	/* req: remove file or directory */
#define KESTRELFS_OP_RENAME		10	/* req: rename/move file or directory */
#define KESTRELFS_OP_WRITE_DATA		11	/* req: write bytes from data_buffer */
#define KESTRELFS_OP_READ_DATA		12	/* req: read bytes into data_buffer */
#define KESTRELFS_OP_RENAME_DATA	13	/* req: rename using names in data_buffer */
#define KESTRELFS_OP_LOOKUP_DATA	14	/* req: lookup name in data_buffer */
#define KESTRELFS_OP_CREATE_DATA	15	/* req: create name in data_buffer */
#define KESTRELFS_OP_MKDIR_DATA		16	/* req: mkdir name in data_buffer */
#define KESTRELFS_OP_UNLINK_DATA	17	/* req: unlink name in data_buffer */
#define KESTRELFS_OP_READDIR_DATA	18	/* req: batched readdir via data_buffer */
#define KESTRELFS_OP_SYMLINK_DATA	19	/* req: create symlink via data_buffer */
#define KESTRELFS_OP_READLINK_DATA	20	/* req: read symlink target via data_buffer */
#define KESTRELFS_OP_LINK_DATA		21	/* req: hard link using name in data_buffer */
#define KESTRELFS_OP_RESULT_OK		64	/* resp: generic success */
#define KESTRELFS_OP_RESULT_ERROR	65	/* resp: generic failure, see error_code */

/*
 * Payload layout for KESTRELFS_OP_WRITE_CHUNK requests
 * -----------------------------------------------------
 *
 * struct kestrelfs_write_chunk_req (packed manually into
 * kestrelfs_event.payload, NOT a separate C struct - both sides must
 * encode/decode at exact byte offsets):
 *
 *   offset  0, 8 bytes, little-endian u64: inode_id - target file inode
 *   offset  8, 8 bytes, little-endian u64: offset - file byte offset
 *   offset 16, 4 bytes, little-endian u32: count - number of bytes to write
 *   offset 20, up to 12 bytes: data - actual bytes to write
 *
 * Maximum write size per call is 12 bytes (32-byte payload - 20 bytes header).
 * Introduced in KESTRELFS_ABI_VERSION 3.
 *
 * Response: RESULT_OK on success (daemon has durably stored the write),
 * RESULT_ERROR with negative errno on failure.
 *
 * The daemon updates file size via max(old_size, offset+count). For shrinking
 * the file or handling O_TRUNC, use KESTRELFS_OP_TRUNCATE (see below).
 */

/*
 * Payload layout for KESTRELFS_OP_TRUNCATE requests
 * --------------------------------------------------
 *
 * struct kestrelfs_truncate_req (packed manually into
 * kestrelfs_event.payload, NOT a separate C struct - both sides must
 * encode/decode at exact byte offsets):
 *
 *   offset  0, 8 bytes, little-endian u64: inode_id - target file inode
 *   offset  8, 8 bytes, little-endian u64: new_size - new file size in bytes
 *
 * Introduced in KESTRELFS_ABI_VERSION 4.
 *
 * This operation is triggered by VFS setattr(ATTR_SIZE) calls, which handle:
 *   - open(..., O_TRUNC) - truncate to 0 on open
 *   - ftruncate(fd, size) - explicit truncate via syscall
 *   - truncate(path, size) - explicit truncate via syscall
 *
 * The daemon MUST update the inode's size and mtime. Historical slices beyond
 * new_size MAY be retained for lazy GC, but read operations MUST respect the
 * new size limit (clamp reads to [0, new_size) and return EOF appropriately).
 *
 * Response: RESULT_OK on success, RESULT_ERROR with negative errno on failure.
 * On failure, the kernel will NOT update i_size.
 */

/*
 * Payload layout for KESTRELFS_OP_READ_CHUNK requests
 * -----------------------------------------------------
 *
 * struct kestrelfs_read_chunk_req (packed manually into
 * kestrelfs_event.payload, NOT a separate C struct on the wire - both
 * sides must encode/decode these three fields at these exact byte
 * offsets within the 32-byte payload array):
 *
 *   offset  0, 8 bytes, little-endian u64: inode_id - which file this
 *            read targets. Introduced in KESTRELFS_ABI_VERSION 2 (see
 *            that macro's doc comment): without it, the Rust daemon
 *            had no way to distinguish which of possibly many files a
 *            KESTRELFS_OP_READ_CHUNK request was actually for, and
 *            had to assume a single hardcoded target. The kernel
 *            producer supplies this from the calling ->read()'s own
 *            `file->f_inode->i_ino` - see kestrelfs_remote_read() in
 *            file.c.
 *   offset  8, 8 bytes, little-endian u64: requested file offset.
 *   offset 16, 4 bytes, little-endian u32: requested byte count,
 *            already clamped by the kernel producer to fit within a
 *            single response payload (see
 *            KESTRELFS_READ_CHUNK_MAX_LEN below) - the Rust daemon
 *            need not re-clamp, only honor whatever value is present.
 *   offset 20..32: reserved, must be zero.
 *
 * The matching KESTRELFS_OP_RESULT_OK response's payload carries the
 * actual data bytes back, starting at payload offset 0, with the
 * valid length given by... there is deliberately no explicit
 * "length" field in the response: the daemon fills exactly
 * KESTRELFS_READ_CHUNK_MAX_LEN bytes (zero-padding if the real
 * content is shorter), and the kernel consumer clamps its own
 * copy_to_user() to min(requested count, KESTRELFS_READ_CHUNK_MAX_LEN).
 * This avoids needing a separate "actual length" field for this
 * bootstrap protocol version; a real chunk-serving opcode in a later
 * phase should carry an explicit length instead once responses can
 * legitimately be shorter than the fixed clamp for reasons other than
 * "caller asked for less".
 *
 * NOTE ON THE v1 -> v2 BREAKING CHANGE: KESTRELFS_ABI_VERSION 1 placed
 * offset at byte 0 and count at byte 8, with no inode_id field at
 * all. That layout is retired as of ABI_VERSION 2 - there was no
 * in-tree producer/consumer of a mixed v1/v2 fleet to preserve
 * compatibility for (this whole IPC bridge is unreleased,
 * project-internal software), so the byte offsets were simply
 * shifted rather than versioned/unioned.
 */
#define KESTRELFS_READ_CHUNK_MAX_LEN	KESTRELFS_EVENT_PAYLOAD_SIZE

/*
 * Payload layout for KESTRELFS_OP_WRITE_DATA / KESTRELFS_OP_READ_DATA
 * -------------------------------------------------------------------
 *
 * REQUEST (kernel -> Rust), packed into kestrelfs_event.payload:
 *
 *   offset  0, 8 bytes, little-endian u64: inode_id
 *   offset  8, 8 bytes, little-endian u64: file_offset
 *   offset 16, 4 bytes, little-endian u32: length
 *   offset 20..32: reserved, must be zero
 *
 * length MUST be <= KESTRELFS_DATA_BUFFER_SIZE.  WRITE_DATA bytes are placed
 * in shared_region.data_buffer before the request is published.  READ_DATA
 * bytes are placed there by the daemon before it publishes RESULT_OK.
 *
 * A READ_DATA RESULT_OK response stores the actual byte count as a
 * little-endian u32 at payload offset 0.  WRITE_DATA uses an empty RESULT_OK.
 * Data-buffer ownership transfers at the existing request/response ring
 * release/acquire publication points; the kernel serializes all data IPC so
 * the buffer needs no independent lock or sequence field.
 *
 * Introduced in KESTRELFS_ABI_VERSION 8.  READ_CHUNK and WRITE_CHUNK remain in
 * the ABI for legacy unit/self-tests, but normal regular-file VFS I/O uses the
 * DATA opcodes.
 */

/*
 * Payload layout for KESTRELFS_OP_LOOKUP requests/responses
 * ------------------------------------------------------------
 *
 * REQUEST (kernel -> Rust), packed into kestrelfs_event.payload:
 *
 *   offset  0, 8 bytes, little-endian u64: parent inode id.
 *   offset  8, 1 byte,  u8: name_len, the number of valid bytes in
 *            the `name` field below. MUST be <=
 *            KESTRELFS_LOOKUP_NAME_MAX (23) - see that macro's doc
 *            comment for the exact truncation/rejection rule.
 *   offset  9, 23 bytes: name, the child's filename, NOT
 *            NUL-terminated (name_len is authoritative; any bytes at
 *            offset 9+name_len..32 are reserved/ignored padding, not
 *            part of the name). Not required to be valid UTF-8 at
 *            the wire level - the Rust daemon is responsible for
 *            deciding how to handle non-UTF-8 names (this bootstrap
 *            protocol's MemStore-backed implementation only ever
 *            deals in ASCII names, so this does not yet matter in
 *            practice).
 *
 * There is deliberately no separate "parent inode" vs "name" struct
 * on the wire - both fields simply occupy fixed byte ranges within
 * the flat 32-byte payload array, exactly like every other opcode's
 * payload in this file.
 *
 * RESPONSE (Rust -> kernel), KESTRELFS_OP_RESULT_OK payload:
 *
 *   offset  0, 8 bytes, little-endian u64: child_inode_id.
 *   offset  8, 8 bytes, little-endian u64: size (bytes, matches
 *            struct kestrelfs_ipc's Inode.size on the Rust side).
 *   offset 16, 4 bytes, little-endian u32: mode (POSIX S_IF type bits
 *            OR'd with permission bits, matches Inode.mode on the
 *            Rust side).
 *   offset 20, 4 bytes, little-endian u32: uid.
 *   offset 24, 4 bytes, little-endian u32: gid.
 *   offset 28, 4 bytes, little-endian u32: nlink.
 *   ------------------------------------------------------------
 *   total: exactly 32 bytes - every byte of the payload is used.
 *
 * On failure (KESTRELFS_OP_RESULT_ERROR), the payload is unused
 * (zeroed) and the failure reason is carried in the event header's
 * error_code field (a negative errno value), NOT in the payload -
 * see kestrelfs_event's error_code field doc comment. This is a
 * deliberate, fixed design decision for this whole protocol: no
 * opcode's error path ever needs to inspect payload bytes, keeping
 * every error-handling code path opcode-agnostic. Recommended
 * mappings for this opcode specifically: no such parent inode, or no
 * child with this name -> -ENOENT; parent inode exists but is not a
 * directory -> -ENOTDIR; name_len exceeds KESTRELFS_LOOKUP_NAME_MAX
 * -> -ENAMETOOLONG.
 */
#define KESTRELFS_LOOKUP_NAME_MAX	23

/*
 * Payload layout for KESTRELFS_OP_GETATTR requests/responses
 * -------------------------------------------------------------
 *
 * REQUEST (kernel -> Rust):
 *
 *   offset  0, 8 bytes, little-endian u64: inode_id to fetch
 *            attributes for.
 *   offset  8..32: reserved, must be zero.
 *
 * RESPONSE (Rust -> kernel), KESTRELFS_OP_RESULT_OK payload:
 *
 *   offset  0, 8 bytes, little-endian u64: size (bytes).
 *   offset  8, 4 bytes, little-endian u32: mode.
 *   offset 12, 4 bytes, little-endian u32: uid.
 *   offset 16, 4 bytes, little-endian u32: gid.
 *   offset 20, 4 bytes, little-endian u32: nlink.
 *   offset 24, 8 bytes, little-endian u64: mtime (Unix epoch seconds).
 *   ------------------------------------------------------------
 *   total: exactly 32 bytes.
 *
 * Note this response does NOT repeat the inode_id (unlike
 * KESTRELFS_OP_LOOKUP's response, which must return a *newly
 * discovered* child_inode_id the caller did not already know) -
 * the requester already supplied inode_id in the request and
 * matches the response back to it via the event header's req_id
 * field, exactly like every other opcode. This is what makes room
 * for the extra mtime field within the same 32-byte budget that
 * KESTRELFS_OP_LOOKUP spends on child_inode_id instead.
 *
 * On failure: no such inode -> -ENOENT, in the event header's
 * error_code field (see KESTRELFS_OP_LOOKUP's response section above
 * for why errors never use the payload).
 */

/*
 * Payload layout for KESTRELFS_OP_CREATE requests/responses
 * ----------------------------------------------------------
 *
 * REQUEST (kernel -> Rust):
 *
 *   offset  0, 8 bytes, little-endian u64: parent_inode_id (directory).
 *   offset  8, 4 bytes, little-endian u32: mode (file type + permissions,
 *            e.g., S_IFREG | 0644).
 *   offset 12, 20 bytes: NUL-terminated filename (max 19 chars + NUL).
 *   ------------------------------------------------------------
 *   total: exactly 32 bytes.
 *
 * RESPONSE (Rust -> kernel), KESTRELFS_OP_RESULT_OK payload:
 *
 *   Same layout as KESTRELFS_OP_LOOKUP response (child_inode_id + attributes).
 *   offset  0, 8 bytes, little-endian u64: new_inode_id.
 *   offset  8, 8 bytes, little-endian u64: size (initially 0 for new files).
 *   offset 16, 4 bytes, little-endian u32: mode.
 *   offset 20, 4 bytes, little-endian u32: uid.
 *   offset 24, 4 bytes, little-endian u32: gid.
 *   offset 28, 4 bytes, little-endian u32: nlink.
 *
 * On failure: parent not found -> -ENOENT, not a directory -> -ENOTDIR,
 * file exists -> -EEXIST.
 *
 * Introduced in KESTRELFS_ABI_VERSION 5.
 */

/*
 * Payload layout for KESTRELFS_OP_READDIR requests/responses
 * -----------------------------------------------------------
 *
 * REQUEST (kernel -> Rust):
 *
 *   offset  0, 8 bytes, little-endian u64: dir_inode_id.
 *   offset  8, 4 bytes, little-endian u32: offset (entry index to start from).
 *   offset 12..32: reserved, must be zero.
 *
 * RESPONSE (Rust -> kernel), KESTRELFS_OP_RESULT_OK payload:
 *
 *   offset  0, 1 byte: entry_count (number of entries in this response, 0-2).
 *   offset  1, 8 bytes, little-endian u64: entry[0].inode_id (or 0 if no entry).
 *   offset  9, 11 bytes: entry[0].name (NUL-terminated, max 10 chars + NUL).
 *   offset 20, 8 bytes, little-endian u64: entry[1].inode_id (or 0 if no entry).
 *   offset 28, 4 bytes: entry[1].name (truncated to 4 bytes for space).
 *   ------------------------------------------------------------
 *   total: exactly 32 bytes.
 *
 * Note: Due to payload size constraints, each READDIR response can return
 * at most 2 entries. The kernel may need to issue multiple READDIR requests
 * with increasing offsets to fetch all directory entries. entry_count=0
 * signals end-of-directory.
 *
 * On failure: not a directory -> -ENOTDIR, inode not found -> -ENOENT.
 *
 * Introduced in KESTRELFS_ABI_VERSION 5.
 */

/*
 * Payload layout for KESTRELFS_OP_MKDIR requests
 * -----------------------------------------------
 *
 * REQUEST (kernel -> Rust):
 *
 *   offset  0, 8 bytes, little-endian u64: parent_inode_id.
 *   offset  8, 4 bytes, little-endian u32: mode (permission bits, e.g. 0755).
 *   offset 12, up to 20 bytes: name (NUL-terminated string).
 *
 * RESPONSE (Rust -> kernel), KESTRELFS_OP_RESULT_OK payload:
 *
 *   offset  0, 8 bytes, little-endian u64: new_dir_inode_id.
 *   offset  8..32: reserved.
 *
 * On failure: parent not found -> -ENOENT, not a directory -> -ENOTDIR,
 * directory exists -> -EEXIST.
 *
 * Introduced in KESTRELFS_ABI_VERSION 6.
 */

/*
 * Payload layout for KESTRELFS_OP_UNLINK requests
 * ------------------------------------------------
 *
 * REQUEST (kernel -> Rust):
 *
 *   offset  0, 8 bytes, little-endian u64: parent_inode_id.
 *   offset  8, up to 24 bytes: name (NUL-terminated string).
 *
 * RESPONSE: KESTRELFS_OP_RESULT_OK (no payload needed).
 *
 * This operation removes a file or directory entry from the parent directory.
 * For directories, the daemon MUST return -ENOTEMPTY if the directory is not
 * empty. The kernel distinguishes unlink (removes files) and rmdir (removes
 * directories), but both use this opcode - the daemon checks the inode type.
 *
 * On failure: parent not found -> -ENOENT, entry not found -> -ENOENT,
 * directory not empty -> -ENOTEMPTY, parent not a directory -> -ENOTDIR.
 *
 * Introduced in KESTRELFS_ABI_VERSION 6.
 */

/*
 * Payload layout for KESTRELFS_OP_RENAME requests
 * ------------------------------------------------
 *
 * REQUEST (kernel -> Rust):
 *
 *   offset  0, 8 bytes, little-endian u64: old_parent_inode_id.
 *   offset  8, 8 bytes, little-endian u64: new_parent_inode_id.
 *   offset 16, 1 byte, u8: old_name_len (MUST be <= KESTRELFS_RENAME_NAME_MAX).
 *   offset 17, 1 byte, u8: new_name_len (MUST be <= KESTRELFS_RENAME_NAME_MAX).
 *   offset 18, 7 bytes: old_name (NOT NUL-terminated, length in old_name_len).
 *   offset 25, 7 bytes: new_name (NOT NUL-terminated, length in new_name_len).
 *   ------------------------------------------------------------
 *   total: exactly 32 bytes.
 *
 * RESPONSE: KESTRELFS_OP_RESULT_OK (no payload needed).
 *
 * This operation atomically renames/moves a file or directory from
 * (old_parent, old_name) to (new_parent, new_name).
 *
 * Supports:
 * - Same directory rename (old_parent == new_parent)
 * - Cross-directory move (old_parent != new_parent)
 * - Atomic replacement: if new_name exists as a regular file, it is atomically
 *   replaced (POSIX semantics)
 *
 * The daemon MUST:
 * - Return -ENOTEMPTY if target exists and is a non-empty directory
 * - Prevent renaming a directory into its own subtree (return -EINVAL)
 * - Update mtime of both parent directories
 *
 * On failure: source not found -> -ENOENT, parent not directory -> -ENOTDIR,
 * target is non-empty directory -> -ENOTEMPTY, directory loop -> -EINVAL,
 * invalid name -> -ENAMETOOLONG.
 *
 * Introduced in KESTRELFS_ABI_VERSION 7.
 */
#define KESTRELFS_RENAME_NAME_MAX	7

/*
 * Payload layout for KESTRELFS_OP_RENAME_DATA requests
 * -----------------------------------------------------
 *
 * REQUEST (kernel -> Rust), packed into kestrelfs_event.payload:
 *
 *   offset  0, 8 bytes, little-endian u64: old_parent_inode_id.
 *   offset  8, 8 bytes, little-endian u64: new_parent_inode_id.
 *   offset 16, 2 bytes, little-endian u16: old_name_len.
 *   offset 18, 2 bytes, little-endian u16: new_name_len.
 *   offset 20, 4 bytes, little-endian u32: rename flags.
 *   offset 24..32: reserved, must be zero.
 *
 * The first old_name_len bytes of shared_region.data_buffer hold old_name;
 * the following new_name_len bytes hold new_name. Neither is NUL-terminated.
 * Each name is limited to POSIX NAME_MAX (255 bytes), and their combined
 * length MUST fit in KESTRELFS_DATA_BUFFER_SIZE. The event-header flags remain
 * zero; the payload rename flags currently permit only
 * KESTRELFS_RENAME_NOREPLACE.
 *
 * RESPONSE and rename semantics are identical to legacy KESTRELFS_OP_RENAME.
 * The legacy opcode remains in ABI v9 for compatibility tests; normal VFS
 * rename uses RENAME_DATA.
 *
 * Introduced in KESTRELFS_ABI_VERSION 9.
 */
#define KESTRELFS_RENAME_DATA_NAME_MAX	255
#define KESTRELFS_RENAME_NOREPLACE	0x00000001U

/*
 * Common ABI v10 single-name request layout
 * ------------------------------------------
 * Used by LOOKUP_DATA, CREATE_DATA, MKDIR_DATA, and UNLINK_DATA:
 *
 *   offset  0, 8 bytes, little-endian u64: parent_inode_id.
 *   offset  8, 2 bytes, little-endian u16: name_len.
 *   offset 10, 2 bytes: reserved, must be zero.
 *   offset 12, 4 bytes, little-endian u32: mode for CREATE_DATA and
 *            MKDIR_DATA; zero for LOOKUP_DATA and UNLINK_DATA.
 *   offset 16..32: reserved, must be zero.
 *
 * Exactly name_len bytes at the start of data_buffer hold the name, without
 * a trailing NUL. name_len MUST be 1..KESTRELFS_NAME_DATA_MAX and request
 * flags MUST be zero. Success responses match the corresponding legacy
 * opcode. The legacy short-name opcodes remain available for compatibility
 * tests; normal VFS paths use these DATA opcodes.
 */
#define KESTRELFS_NAME_DATA_MAX		255

/*
 * ABI v10 READDIR_DATA layout
 * ---------------------------
 * REQUEST payload:
 *   offset 0, 8 bytes, little-endian u64: directory inode id.
 *   offset 8, 4 bytes, little-endian u32: first entry index.
 *   offset 12..32: reserved, must be zero.
 *
 * RESULT_OK payload:
 *   offset 0, 4 bytes, little-endian u32: entry_count.
 *   offset 4, 4 bytes, little-endian u32: encoded byte count in data_buffer.
 *
 * data_buffer contains entry_count adjacent variable-length records:
 *   little-endian u64 inode_id, little-endian u16 name_len, then name_len
 *   bytes of non-NUL-terminated name. Each name is at most 255 bytes. The
 * daemon packs as many complete records as fit; entry_count == 0 marks EOF.
 */
#define KESTRELFS_READDIR_DATA_ENTRY_HEADER_SIZE	10

/*
 * ABI v11 symbolic-link layouts
 * -----------------------------
 * SYMLINK_DATA request payload:
 *   offset 0,  8 bytes, little-endian u64: parent inode id.
 *   offset 8,  2 bytes, little-endian u16: link name length.
 *   offset 10, 2 bytes, little-endian u16: target length.
 *   offset 12..32: reserved, must be zero.
 * data_buffer contains name immediately followed by target, neither NUL-
 * terminated. The name is limited to POSIX NAME_MAX; the UTF-8 target is
 * limited to 4095 bytes. RESULT_OK uses the normal LOOKUP response layout.
 *
 * READLINK_DATA request payload:
 *   offset 0, 8 bytes, little-endian u64: symbolic-link inode id.
 *   offset 8..32: reserved, must be zero.
 * On RESULT_OK, payload offset 0 holds a little-endian u32 target length and
 * data_buffer holds exactly that many non-NUL-terminated target bytes.
 */
#define KESTRELFS_SYMLINK_TARGET_MAX	4095

/*
 * ABI v12 hard-link layout
 * ------------------------
 * LINK_DATA request payload:
 *   offset  0, 8 bytes, little-endian u64: destination parent inode id.
 *   offset  8, 8 bytes, little-endian u64: existing target inode id.
 *   offset 16, 2 bytes, little-endian u16: new name length.
 *   offset 18..32: reserved, must be zero.
 * data_buffer starts with the non-NUL-terminated new name, limited to
 * KESTRELFS_NAME_DATA_MAX bytes. RESULT_OK payload offset 0 contains the
 * atomically updated little-endian u32 nlink value.
 */

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
 * based - INCLUDING when an individual opcode's documented payload
 * byte layout changes incompatibly (even though struct kestrelfs_event
 * itself, as a fixed 64-byte container, does not change size/shape -
 * see the KESTRELFS_OP_READ_CHUNK v1->v2 change below for exactly
 * this kind of bump). The Rust daemon must refuse to attach if this
 * does not match what it was built against.
 *
 * Version history:
 *   1 - initial Phase 2 bridge (ring buffers, LOOKUP/GETATTR/
 *       READ_CHUNK opcodes defined but READ_CHUNK's request payload
 *       had no inode_id field - see below).
 *   2 - KESTRELFS_OP_READ_CHUNK's request payload gained an inode_id
 *       field (offset/count shifted from bytes 0/8 to bytes 8/16 to
 *       make room - see "Payload layout for KESTRELFS_OP_READ_CHUNK
 *       requests" above). Required once remote.txt's inode number was
 *       corrected from 1 (colliding with the root directory - see
 *       kestrelfs_files[] in inode.c) to 3, making it no longer safe
 *       for the Rust daemon to assume every READ_CHUNK request targets
 *       one single, implicit file.
 *
 *   3 - Phase 3 step 4: Added KESTRELFS_OP_WRITE_CHUNK (opcode 4) for
 *       write operations. Payload layout: inode_id (u64 @0), offset
 *       (u64 @8), count (u32 @16, max 12), data (@20). This enables
 *       the daemon to accept writes from the kernel and persist them
 *       via MetaStore + ObjectStore, completing the read/write path.
 *
 *   4 - Phase 3 step 5: Added KESTRELFS_OP_TRUNCATE (opcode 5).
 *
 *   5 - Phase 3 step 7: Added KESTRELFS_OP_CREATE and KESTRELFS_OP_READDIR.
 *
 *   6 - Phase 3 step 9: Added KESTRELFS_OP_MKDIR and KESTRELFS_OP_UNLINK.
 *
 *   7 - Phase 3 step 10: Added KESTRELFS_OP_RENAME (opcode 10). Supports
 *       atomic rename/move with POSIX semantics. Names limited to 7 bytes
 *       each to fit within 32-byte payload.
 *
 *   8 - Phase 3 step 11: Added a 16 KiB data bounce buffer after both rings,
 *       plus KESTRELFS_OP_WRITE_DATA (opcode 11) and
 *       KESTRELFS_OP_READ_DATA (opcode 12).
 *
 *   9 - Phase 3 step 12: Added KESTRELFS_OP_RENAME_DATA (opcode 13). Old and
 *       new names are concatenated in the bounce buffer and may each be up
 *       to 255 bytes. Legacy KESTRELFS_OP_RENAME remains available.
 *
 *  10 - Phase 3 step 13: Added LOOKUP_DATA, CREATE_DATA, MKDIR_DATA,
 *       UNLINK_DATA, and batched READDIR_DATA (opcodes 14..18). All active
 *       VFS name paths now support 255-byte names through the bounce buffer.
 *
 *  11 - Phase 3 step 14: Added SYMLINK_DATA and READLINK_DATA (opcodes
 *       19..20). Link names and targets use the existing serialized bounce
 *       buffer; symlink targets remain metadata rather than object data.
 *
 *  12 - Phase 4/control-plane step 32: Added LINK_DATA (opcode 21). The destination name
 *       uses the serialized bounce buffer and supports POSIX NAME_MAX; the
 *       response returns the persistent inode link count.
 *
 *  13 - Phase 4/control-plane step 33: RENAME_DATA payload offset 20 now
 *       carries rename flags. KESTRELFS_RENAME_NOREPLACE is supported;
 *       EXCHANGE and WHITEOUT remain rejected.
 */
#define KESTRELFS_ABI_VERSION		13

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
 * @data_buffer:  serialized 16 KiB bounce buffer for all *_DATA operations.
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
	__u8				data_buffer[KESTRELFS_DATA_BUFFER_SIZE];
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
		(2 * KESTRELFS_RING_SLOTS * sizeof(struct kestrelfs_event)) +
		KESTRELFS_DATA_BUFFER_SIZE,
		"kestrelfs_shared_region layout drifted, check padding");

_Static_assert(__builtin_offsetof(struct kestrelfs_shared_region, data_buffer) ==
		KESTRELFS_CACHELINE_SIZE +
		(2 * KESTRELFS_CACHELINE_SIZE) +
		(2 * KESTRELFS_RING_SLOTS * sizeof(struct kestrelfs_event)),
		"data_buffer must immediately follow both rings");

_Static_assert(KESTRELFS_DATA_BUFFER_SIZE <= (__u32)-1,
		"KESTRELFS_DATA_BUFFER_SIZE must fit in the DATA opcode u32 length");

_Static_assert(8 + 8 + 4 <= KESTRELFS_EVENT_PAYLOAD_SIZE,
		"READ_DATA/WRITE_DATA request fields overflow the event payload");

_Static_assert(8 + 8 + 2 + 2 + 4 <= KESTRELFS_EVENT_PAYLOAD_SIZE,
		"RENAME_DATA request fields overflow the event payload");

_Static_assert(8 + 8 + 2 <= KESTRELFS_EVENT_PAYLOAD_SIZE,
		"LINK_DATA request fields overflow the event payload");

_Static_assert(2 * KESTRELFS_RENAME_DATA_NAME_MAX <=
		KESTRELFS_DATA_BUFFER_SIZE,
		"two maximum-length rename names must fit in data_buffer");

_Static_assert(8 + 2 + 2 + 4 <= KESTRELFS_EVENT_PAYLOAD_SIZE,
		"single-name DATA request fields overflow the event payload");

_Static_assert(KESTRELFS_NAME_DATA_MAX <= (__u16)-1,
		"NAME_DATA maximum must fit in its u16 length field");

_Static_assert(KESTRELFS_NAME_DATA_MAX == KESTRELFS_RENAME_DATA_NAME_MAX,
		"all active name opcodes must share one NAME_MAX limit");

_Static_assert(KESTRELFS_READDIR_DATA_ENTRY_HEADER_SIZE +
		KESTRELFS_NAME_DATA_MAX <= KESTRELFS_DATA_BUFFER_SIZE,
		"one maximum-length READDIR_DATA entry must fit in data_buffer");

_Static_assert(8 + 2 + 2 <= KESTRELFS_EVENT_PAYLOAD_SIZE,
		"SYMLINK_DATA request fields overflow the event payload");

_Static_assert(KESTRELFS_NAME_DATA_MAX + KESTRELFS_SYMLINK_TARGET_MAX <=
		KESTRELFS_DATA_BUFFER_SIZE,
		"maximum symlink name and target must fit in data_buffer");

_Static_assert(KESTRELFS_SYMLINK_TARGET_MAX <= (__u16)-1,
		"symlink target maximum must fit in its u16 length field");

/*
 * KESTRELFS_OP_LOOKUP request payload: 8 bytes (parent_inode) + 1
 * byte (name_len) + KESTRELFS_LOOKUP_NAME_MAX bytes (name) must fit
 * within KESTRELFS_EVENT_PAYLOAD_SIZE. Guards against
 * KESTRELFS_LOOKUP_NAME_MAX ever being widened without re-checking
 * this arithmetic.
 */
_Static_assert(8 + 1 + KESTRELFS_LOOKUP_NAME_MAX <= KESTRELFS_EVENT_PAYLOAD_SIZE,
		"KESTRELFS_OP_LOOKUP request payload (parent_inode + name_len + name) overflows KESTRELFS_EVENT_PAYLOAD_SIZE");

/*
 * KESTRELFS_OP_LOOKUP KESTRELFS_OP_RESULT_OK response payload:
 * child_inode_id(8) + size(8) + mode(4) + uid(4) + gid(4) + nlink(4)
 * must total exactly KESTRELFS_EVENT_PAYLOAD_SIZE (32) - this
 * response is defined to use every payload byte, see the "Payload
 * layout for KESTRELFS_OP_LOOKUP" section above.
 */
_Static_assert(8 + 8 + 4 + 4 + 4 + 4 == KESTRELFS_EVENT_PAYLOAD_SIZE,
		"KESTRELFS_OP_LOOKUP response payload field layout no longer sums to KESTRELFS_EVENT_PAYLOAD_SIZE");

/*
 * KESTRELFS_OP_GETATTR KESTRELFS_OP_RESULT_OK response payload:
 * size(8) + mode(4) + uid(4) + gid(4) + nlink(4) + mtime(8) must
 * total exactly KESTRELFS_EVENT_PAYLOAD_SIZE (32).
 */
_Static_assert(8 + 4 + 4 + 4 + 4 + 8 == KESTRELFS_EVENT_PAYLOAD_SIZE,
		"KESTRELFS_OP_GETATTR response payload field layout no longer sums to KESTRELFS_EVENT_PAYLOAD_SIZE");

#endif /* _KESTRELFS_IPC_H */
