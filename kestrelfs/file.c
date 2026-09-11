// SPDX-License-Identifier: GPL-2.0
/*
 * file.c - Regular file operations for KestrelFS Phase 1.
 *
 * hello.txt is a static, read-only, in-memory file. Its content is
 * never written to a backing store or NVMe cache in this phase: the
 * bytes come straight from a kernel .rodata string. simple_read_from_buffer()
 * is the same helper used by debugfs and countless other pseudo
 * filesystems for exactly this purpose, so we reuse it instead of
 * reinventing ->read().
 */

#include <linux/fs.h>
#include <linux/uaccess.h>

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
