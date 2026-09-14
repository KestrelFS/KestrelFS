// SPDX-License-Identifier: GPL-2.0
/*
 * cache.c - Phase 4 kernel-owned NVMe cache device and format skeleton.
 *
 * Step 19 exclusively claims a dedicated block device, validates or creates
 * a minimal on-disk superblock, and initializes an empty in-memory index.
 * It intentionally does not populate that index or serve cache hits yet.
 */

#include <linux/bio.h>
#include <linux/blkdev.h>
#include <linux/build_bug.h>
#include <linux/err.h>
#include <linux/errno.h>
#include <linux/fs.h>
#include <linux/highmem.h>
#include <linux/module.h>
#include <linux/random.h>
#include <linux/rhashtable.h>
#include <linux/string.h>

#include "kestrelfs.h"

#define KESTRELFS_CACHE_MAGIC		0x454843414353464bULL /* "KFSCACHE" LE */
#define KESTRELFS_CACHE_FORMAT_VERSION	1U
#define KESTRELFS_CACHE_SUPERBLOCK_SIZE	4096U
#define KESTRELFS_CACHE_BLOCK_SIZE	4096U
#define KESTRELFS_CACHE_METADATA_BYTES	(2ULL * 1024 * 1024)
#define KESTRELFS_CACHE_MIN_BYTES	\
	(KESTRELFS_CACHE_METADATA_BYTES + KESTRELFS_CACHE_BLOCK_SIZE)

/*
 * One fixed-size entry format is reserved in the on-disk metadata area.
 * Step 19 does not persist entries yet; defining it now makes the geometry
 * versioned and leaves room for a later crash-recoverable index.
 */
struct kestrelfs_cache_disk_index_entry {
	__le64 inode_id;
	__le64 file_offset;
	__le64 lba;
	__le64 generation;
};

struct kestrelfs_cache_disk_superblock {
	__le64 magic;
	__le32 version;
	__le32 header_size;
	__le32 logical_block_size;
	__le32 physical_block_size;
	__le32 cache_block_size;
	__le32 index_entry_size;
	__le64 usable_sectors;
	__le64 index_start_lba;
	__le64 index_capacity;
	__le64 data_start_lba;
	__le64 format_generation;
	u8 reserved[KESTRELFS_CACHE_SUPERBLOCK_SIZE - 72];
};

static_assert(sizeof(struct kestrelfs_cache_disk_index_entry) == 32);
static_assert(sizeof(struct kestrelfs_cache_disk_superblock) ==
	      KESTRELFS_CACHE_SUPERBLOCK_SIZE);
static_assert(KESTRELFS_CACHE_SUPERBLOCK_SIZE <= PAGE_SIZE);

struct kestrelfs_cache_index_key {
	u64 inode_id;
	u64 file_offset;
};

struct kestrelfs_cache_index_entry {
	struct rhash_head node;
	struct kestrelfs_cache_index_key key;
	sector_t lba;
	u64 generation;
	u32 length;
};

static const struct rhashtable_params kestrelfs_cache_index_params = {
	.head_offset = offsetof(struct kestrelfs_cache_index_entry, node),
	.key_offset = offsetof(struct kestrelfs_cache_index_entry, key),
	.key_len = sizeof(struct kestrelfs_cache_index_key),
	.automatic_shrinking = true,
};

static char *cache_device;
module_param(cache_device, charp, 0444);
MODULE_PARM_DESC(cache_device,
		 "dedicated cache block-device path (loop, zvol, or raw device)");

static unsigned long cache_size_mib;
module_param(cache_size_mib, ulong, 0444);
MODULE_PARM_DESC(cache_size_mib,
		 "cache capacity in MiB (0 uses the whole block device)");

static char kestrelfs_cache_holder;
static struct file *kestrelfs_cache_file;
static struct block_device *kestrelfs_cache_bdev;
static struct rhashtable kestrelfs_cache_index;
static bool kestrelfs_cache_index_ready;
static u64 kestrelfs_cache_usable_bytes;
static u32 kestrelfs_cache_logical_size;
static u32 kestrelfs_cache_physical_size;

static u64 kestrelfs_cache_index_capacity(void)
{
	return (KESTRELFS_CACHE_METADATA_BYTES -
		KESTRELFS_CACHE_SUPERBLOCK_SIZE) /
	       sizeof(struct kestrelfs_cache_disk_index_entry);
}

static int kestrelfs_cache_rw_block(void *buffer, sector_t sector, bool write)
{
	struct page *page;
	struct bio *bio;
	void *mapped;
	int added;
	int ret;

	page = alloc_page(GFP_KERNEL | __GFP_ZERO);
	if (!page)
		return -ENOMEM;

	if (write) {
		mapped = kmap_local_page(page);
		memcpy(mapped, buffer, KESTRELFS_CACHE_SUPERBLOCK_SIZE);
		kunmap_local(mapped);
	}

	bio = bio_alloc(kestrelfs_cache_bdev, 1,
			write ? REQ_OP_WRITE | REQ_SYNC : REQ_OP_READ,
			GFP_KERNEL);
	if (!bio) {
		ret = -ENOMEM;
		goto out_page;
	}

	bio->bi_iter.bi_sector = sector;
	added = bio_add_page(bio, page, KESTRELFS_CACHE_SUPERBLOCK_SIZE, 0);
	if (added != KESTRELFS_CACHE_SUPERBLOCK_SIZE) {
		ret = -EIO;
		goto out_bio;
	}

	ret = submit_bio_wait(bio);
	if (ret)
		goto out_bio;

	if (write) {
		ret = blkdev_issue_flush(kestrelfs_cache_bdev);
	} else {
		mapped = kmap_local_page(page);
		memcpy(buffer, mapped, KESTRELFS_CACHE_SUPERBLOCK_SIZE);
		kunmap_local(mapped);
	}

out_bio:
	bio_put(bio);
out_page:
	__free_page(page);
	return ret;
}

static int kestrelfs_cache_metadata_is_clean(bool *clean)
{
	void *buffer;
	sector_t sector;
	sector_t end = KESTRELFS_CACHE_METADATA_BYTES >> SECTOR_SHIFT;
	int ret = 0;

	buffer = kmalloc(KESTRELFS_CACHE_SUPERBLOCK_SIZE, GFP_KERNEL);
	if (!buffer)
		return -ENOMEM;

	*clean = true;
	for (sector = KESTRELFS_CACHE_SUPERBLOCK_SIZE >> SECTOR_SHIFT;
	     sector < end;
	     sector += KESTRELFS_CACHE_SUPERBLOCK_SIZE >> SECTOR_SHIFT) {
		ret = kestrelfs_cache_rw_block(buffer, sector, false);
		if (ret)
			break;
		if (memchr_inv(buffer, 0, KESTRELFS_CACHE_SUPERBLOCK_SIZE)) {
			*clean = false;
			break;
		}
	}

	kfree(buffer);
	return ret;
}

static int kestrelfs_cache_validate_geometry(void)
{
	u64 device_bytes;
	u64 requested_bytes;

	device_bytes = bdev_nr_bytes(kestrelfs_cache_bdev);
	kestrelfs_cache_logical_size =
		bdev_logical_block_size(kestrelfs_cache_bdev);
	kestrelfs_cache_physical_size =
		bdev_physical_block_size(kestrelfs_cache_bdev);

	if (!is_power_of_2(kestrelfs_cache_logical_size) ||
	    kestrelfs_cache_logical_size < SECTOR_SIZE ||
	    KESTRELFS_CACHE_SUPERBLOCK_SIZE % kestrelfs_cache_logical_size) {
		pr_err("kestrelfs: unsupported cache logical sector size %u\n",
		       kestrelfs_cache_logical_size);
		return -EINVAL;
	}

	if (!is_power_of_2(kestrelfs_cache_physical_size) ||
	    kestrelfs_cache_physical_size < kestrelfs_cache_logical_size ||
	    KESTRELFS_CACHE_SUPERBLOCK_SIZE % kestrelfs_cache_physical_size) {
		pr_err("kestrelfs: unsupported cache physical sector size %u\n",
		       kestrelfs_cache_physical_size);
		return -EINVAL;
	}

	if (cache_size_mib > (U64_MAX >> 20))
		return -EOVERFLOW;
	requested_bytes = (u64)cache_size_mib << 20;
	if (!requested_bytes)
		requested_bytes = device_bytes;
	if (requested_bytes > device_bytes) {
		pr_err("kestrelfs: requested cache size %llu exceeds device size %llu\n",
		       (unsigned long long)requested_bytes,
		       (unsigned long long)device_bytes);
		return -ENOSPC;
	}

	kestrelfs_cache_usable_bytes =
		round_down(requested_bytes, (u64)kestrelfs_cache_logical_size);
	if (kestrelfs_cache_usable_bytes < KESTRELFS_CACHE_MIN_BYTES) {
		pr_err("kestrelfs: cache size %llu is below minimum %llu\n",
		       (unsigned long long)kestrelfs_cache_usable_bytes,
		       (unsigned long long)KESTRELFS_CACHE_MIN_BYTES);
		return -ENOSPC;
	}

	return 0;
}

static bool kestrelfs_cache_superblock_matches(
	const struct kestrelfs_cache_disk_superblock *super)
{
	return le64_to_cpu(super->magic) == KESTRELFS_CACHE_MAGIC &&
	       le32_to_cpu(super->version) == KESTRELFS_CACHE_FORMAT_VERSION &&
	       le32_to_cpu(super->header_size) ==
			KESTRELFS_CACHE_SUPERBLOCK_SIZE &&
	       le32_to_cpu(super->logical_block_size) ==
			kestrelfs_cache_logical_size &&
	       le32_to_cpu(super->physical_block_size) ==
			kestrelfs_cache_physical_size &&
	       le32_to_cpu(super->cache_block_size) ==
			KESTRELFS_CACHE_BLOCK_SIZE &&
	       le32_to_cpu(super->index_entry_size) ==
			sizeof(struct kestrelfs_cache_disk_index_entry) &&
	       le64_to_cpu(super->usable_sectors) ==
			(kestrelfs_cache_usable_bytes >> SECTOR_SHIFT) &&
	       le64_to_cpu(super->index_start_lba) ==
			(KESTRELFS_CACHE_SUPERBLOCK_SIZE >> SECTOR_SHIFT) &&
	       le64_to_cpu(super->index_capacity) ==
			kestrelfs_cache_index_capacity() &&
	       le64_to_cpu(super->data_start_lba) ==
			(KESTRELFS_CACHE_METADATA_BYTES >> SECTOR_SHIFT) &&
	       le64_to_cpu(super->format_generation) != 0;
}

static int kestrelfs_cache_load_or_format(void)
{
	struct kestrelfs_cache_disk_superblock *super;
	u64 generation;
	bool clean;
	int ret;

	super = kzalloc(sizeof(*super), GFP_KERNEL);
	if (!super)
		return -ENOMEM;

	ret = kestrelfs_cache_rw_block(super, 0, false);
	if (ret) {
		pr_err("kestrelfs: failed to read cache superblock: %d\n", ret);
		goto out;
	}

	if (!memchr_inv(super, 0, sizeof(*super))) {
		ret = kestrelfs_cache_metadata_is_clean(&clean);
		if (ret) {
			pr_err("kestrelfs: failed to inspect cache metadata area: %d\n",
			       ret);
			goto out;
		}
		if (!clean) {
			pr_err("kestrelfs: zero superblock but nonzero cache metadata area\n");
			ret = -EINVAL;
			goto out;
		}

		generation = get_random_u64();
		if (!generation)
			generation = 1;
		super->magic = cpu_to_le64(KESTRELFS_CACHE_MAGIC);
		super->version = cpu_to_le32(KESTRELFS_CACHE_FORMAT_VERSION);
		super->header_size =
			cpu_to_le32(KESTRELFS_CACHE_SUPERBLOCK_SIZE);
		super->logical_block_size =
			cpu_to_le32(kestrelfs_cache_logical_size);
		super->physical_block_size =
			cpu_to_le32(kestrelfs_cache_physical_size);
		super->cache_block_size = cpu_to_le32(KESTRELFS_CACHE_BLOCK_SIZE);
		super->index_entry_size = cpu_to_le32(
			sizeof(struct kestrelfs_cache_disk_index_entry));
		super->usable_sectors = cpu_to_le64(
			kestrelfs_cache_usable_bytes >> SECTOR_SHIFT);
		super->index_start_lba = cpu_to_le64(
			KESTRELFS_CACHE_SUPERBLOCK_SIZE >> SECTOR_SHIFT);
		super->index_capacity =
			cpu_to_le64(kestrelfs_cache_index_capacity());
		super->data_start_lba = cpu_to_le64(
			KESTRELFS_CACHE_METADATA_BYTES >> SECTOR_SHIFT);
		super->format_generation = cpu_to_le64(generation);

		ret = kestrelfs_cache_rw_block(super, 0, true);
		if (ret) {
			pr_err("kestrelfs: failed to write cache superblock: %d\n",
			       ret);
			goto out;
		}
		pr_info("kestrelfs: formatted cache device=%s generation=%llu\n",
			cache_device, (unsigned long long)generation);
	} else if (!kestrelfs_cache_superblock_matches(super)) {
		pr_err("kestrelfs: cache superblock format or geometry mismatch\n");
		ret = -EINVAL;
		goto out;
	} else {
		pr_info("kestrelfs: reusing cache device=%s generation=%llu\n",
			cache_device,
			(unsigned long long)le64_to_cpu(super->format_generation));
	}

	pr_info("kestrelfs: cache geometry bytes=%llu logical=%u physical=%u "
		"index_entries=%llu data_lba=%llu\n",
		(unsigned long long)kestrelfs_cache_usable_bytes,
		kestrelfs_cache_logical_size, kestrelfs_cache_physical_size,
		(unsigned long long)kestrelfs_cache_index_capacity(),
		(unsigned long long)(KESTRELFS_CACHE_METADATA_BYTES >>
				     SECTOR_SHIFT));
out:
	kfree(super);
	return ret;
}

int kestrelfs_cache_init(void)
{
	blk_mode_t mode = BLK_OPEN_READ | BLK_OPEN_WRITE | BLK_OPEN_EXCL |
			  BLK_OPEN_RESTRICT_WRITES;
	int ret;

	if (!cache_device || !cache_device[0]) {
		pr_info("kestrelfs: NVMe cache disabled (no cache_device)\n");
		return 0;
	}

	kestrelfs_cache_file = bdev_file_open_by_path(cache_device, mode,
						      &kestrelfs_cache_holder,
						      NULL);
	if (IS_ERR(kestrelfs_cache_file)) {
		ret = PTR_ERR(kestrelfs_cache_file);
		kestrelfs_cache_file = NULL;
		pr_err("kestrelfs: cannot claim cache device %s: %d\n",
		       cache_device, ret);
		return ret;
	}
	kestrelfs_cache_bdev = file_bdev(kestrelfs_cache_file);

	ret = kestrelfs_cache_validate_geometry();
	if (ret)
		goto err_release;
	ret = kestrelfs_cache_load_or_format();
	if (ret)
		goto err_release;
	ret = rhashtable_init(&kestrelfs_cache_index,
			      &kestrelfs_cache_index_params);
	if (ret)
		goto err_release;
	kestrelfs_cache_index_ready = true;

	return 0;

err_release:
	bdev_fput(kestrelfs_cache_file);
	kestrelfs_cache_file = NULL;
	kestrelfs_cache_bdev = NULL;
	return ret;
}

void kestrelfs_cache_exit(void)
{
	if (kestrelfs_cache_index_ready) {
		rhashtable_destroy(&kestrelfs_cache_index);
		kestrelfs_cache_index_ready = false;
	}
	if (kestrelfs_cache_file) {
		bdev_fput(kestrelfs_cache_file);
		kestrelfs_cache_file = NULL;
		kestrelfs_cache_bdev = NULL;
	}
}

ssize_t kestrelfs_cache_lookup(struct inode *inode, char __user *buf,
			       size_t count, loff_t *ppos)
{
	/* Step 19 has a real device and index container, but no hit path yet. */
	(void)inode;
	(void)buf;
	(void)count;
	(void)ppos;
	return -ENODATA;
}
