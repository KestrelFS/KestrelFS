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

#include "kestrelfs_ipc.h"

#define KESTRELFS_NAME		"kestrelfs"
#define KESTRELFS_MAGIC		0x4B455354	/* "KEST" */

/* Static payload served by hello.txt (file.c). */
#define KESTRELFS_HELLO_CONTENT	"Hello from KestrelFS Kernel Module (Phase 1)!\n"

/* super.c */
extern struct file_system_type kestrelfs_fs_type;

/* inode.c: superblock lifecycle callbacks (statfs, evict_inode, ...). */
extern const struct super_operations kestrelfs_super_ops;

/* file.c: file_operations for the read-only hello.txt regular file. */
extern const struct file_operations kestrelfs_file_ops;

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

#endif /* _KESTRELFS_H */
