/* SPDX-License-Identifier: GPL-2.0 */
/*
 * kestrelfs.h - Shared declarations for the KestrelFS Phase 1 VFS skeleton.
 *
 * KestrelFS is an out-of-tree Linux kernel module implementing the
 * data-plane (kernel side) half of a hybrid C+Rust distributed
 * filesystem. Phase 1 only mounts a minimal, read-only, in-memory
 * filesystem exposing a single "hello.txt" file, in order to validate
 * the VFS registration / mount skeleton before any real IO logic is
 * added.
 */

#ifndef _KESTRELFS_H
#define _KESTRELFS_H

#include <linux/fs.h>
#include <linux/magic.h>
#include <linux/mutex.h>

#include "kestrelfs_ipc.h"

#define KESTRELFS_NAME		"kestrelfs"
#define KESTRELFS_MAGIC		0x4B455354	/* "KEST" */

/* Static payload served by hello.txt (file.c). */
#define KESTRELFS_HELLO_CONTENT	"Hello from KestrelFS Kernel Module (Phase 1)!\n"

/*
 * KESTRELFS_REMOTE_FILE_SIZE - the logical size (in bytes) of
 * remote.txt. Shared between file.c (which enforces it as the EOF
 * boundary for kestrelfs_remote_read(), see that function's doc
 * comment for the full rationale) and inode.c (which writes it into
 * remote.txt's inode->i_size right after simple_fill_super() creates
 * that inode with the default of 0, so stat()/ls -l report the
 * correct size).
 */
#define KESTRELFS_REMOTE_FILE_SIZE	(16 * KESTRELFS_READ_CHUNK_MAX_LEN)

/* cache.c: Phase 4 kernel-owned persistent block-device cache. */
int kestrelfs_cache_init(void);
void kestrelfs_cache_exit(void);

/*
 * Try to satisfy a read from the kernel-owned cache.  A non-negative return
 * value is the number of bytes served; -ENODATA is a cache miss and tells the
 * caller to use the existing daemon READ_DATA path.
 */
ssize_t kestrelfs_cache_lookup(struct inode *inode, char __user *buf,
			       size_t count, loff_t *ppos, u64 *miss_epoch);
void kestrelfs_cache_fill(struct inode *inode, u64 offset, const u8 *data,
			  size_t length, u64 miss_epoch);
int kestrelfs_cache_invalidate_inode(u64 inode_id);

/* super.c */
extern struct file_system_type kestrelfs_fs_type;

/* inode.c: superblock lifecycle callbacks (statfs, evict_inode, ...). */
extern const struct super_operations kestrelfs_super_ops;

/* file.c: file_operations for the read-only hello.txt regular file. */
extern const struct file_operations kestrelfs_file_ops;

/*
 * file.c: file_operations for remote.txt, whose reads round-trip
 * through the Phase 2 kernel<->Rust IPC bridge (kestrelfs_req_push()/
 * kestrelfs_wait_for_resp()/kestrelfs_check_resp()). This is the
 * project's first VFS call path that depends on a Rust daemon being
 * attached to /dev/kestrel_ctl.
 */
extern const struct file_operations kestrelfs_remote_file_ops;

/*
 * kestrelfs_writable_file_ops - file operations for writable.dat (inode 4).
 *
 * Supports both read and write operations via IPC to the daemon.
 */
extern const struct file_operations kestrelfs_writable_file_ops;

/*
 * kestrelfs_writable_inode_ops - inode operations for writable.dat (inode 4).
 *
 * Implements setattr to handle truncate/ftruncate via OP_TRUNCATE IPC.
 */
extern const struct inode_operations kestrelfs_writable_inode_ops;

/*
 * chardev.c: Phase 2 IPC bridge infrastructure (/dev/kestrel_ctl).
 *
 * kestrelfs_chardev_init()/_exit() are called once from super.c's
 * module_init/module_exit. The remaining three are exported (see
 * EXPORT_SYMBOL_GPL in chardev.c) for future in-tree VFS glue code
 * that needs to push requests / await responses through the shared
 * ring buffers defined in kestrelfs_ipc.h.
 */
int kestrelfs_chardev_init(void);
void kestrelfs_chardev_exit(void);

struct kestrelfs_shared_region *kestrelfs_shm_region(void);
void kestrelfs_wake_req_waiters(void);
long kestrelfs_wait_for_resp(long timeout_jiffies);
int kestrelfs_is_daemon_alive(void);

/* Serializes every request that owns the single shared data bounce buffer. */
extern struct mutex kestrelfs_data_ipc_lock;

/*
 * ipc_ring.c: lock-free-consumer ring buffer push/pop primitives.
 *
 * kestrelfs_ipc_ring_init()/_exit() are called once from super.c's
 * module_init/module_exit (sets up/tears down the debugfs self-test
 * hook only). kestrelfs_req_push()/kestrelfs_check_resp() are
 * exported (EXPORT_SYMBOL_GPL) for future VFS glue code in
 * inode.c/file.c to issue requests and retrieve responses through
 * the shared ring buffers defined in kestrelfs_ipc.h.
 */
void kestrelfs_ipc_ring_init(void);
void kestrelfs_ipc_ring_exit(void);

int kestrelfs_req_push(u32 opcode, u32 flags, const u8 *payload,
			u64 *out_req_id);
int kestrelfs_check_resp(u64 req_id, struct kestrelfs_event *out_event);

/*
 * dir.c: directory inode/file operations (Phase 3 Step 7b).
 *
 * Dynamic LOOKUP/CREATE/READDIR via IPC, replacing simple_fill_super().
 */
extern const struct inode_operations kestrelfs_dir_inode_operations;
extern const struct file_operations kestrelfs_dir_file_operations;
extern const struct inode_operations kestrelfs_symlink_inode_operations;

/*
 * inode.c: inode cache management.
 *
 * kestrelfs_get_inode() - fetch or create an inode with given ino/mode/size.
 * Used by dir.c's lookup/create handlers.
 */
struct inode *kestrelfs_get_inode(struct super_block *sb, u64 ino,
				  u32 mode, u64 size);

/*
 * file.c: unified file operations for regular files.
 *
 * kestrelfs_reg_file_ops - generic read/write/llseek for all regular files.
 * kestrelfs_reg_inode_ops - generic setattr (truncate) for all regular files.
 */
extern const struct file_operations kestrelfs_reg_file_ops;
extern const struct inode_operations kestrelfs_reg_inode_ops;

#endif /* _KESTRELFS_H */
