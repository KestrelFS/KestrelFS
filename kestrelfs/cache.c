// SPDX-License-Identifier: GPL-2.0
/*
 * cache.c - Kernel-owned persistent block cache for KestrelFS.
 *
 * Step 21 binds Step 20's persistent 4 KiB cache to one metadata namespace.
 * Step 22 sends aligned, contiguous cache hits straight into pinned user pages;
 * unaligned head/tail blocks retain the buffered fallback.  Cache failures
 * never replace the authoritative daemon/ObjectStore path.
 */

#include <linux/bio.h>
#include <linux/bitmap.h>
#include <linux/blkdev.h>
#include <linux/build_bug.h>
#include <linux/err.h>
#include <linux/errno.h>
#include <linux/fs.h>
#include <linux/highmem.h>
#include <linux/hex.h>
#include <linux/list.h>
#include <linux/mm.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/random.h>
#include <linux/rhashtable.h>
#include <linux/string.h>
#include <linux/uaccess.h>

#include "kestrelfs.h"

#define KESTRELFS_CACHE_MAGIC		0x454843414353464bULL /* "KFSCACHE" LE */
#define KESTRELFS_CACHE_FORMAT_VERSION	2U
#define KESTRELFS_CACHE_SUPERBLOCK_SIZE	4096U
#define KESTRELFS_CACHE_BLOCK_SIZE	4096U
#define KESTRELFS_CACHE_NAMESPACE_SIZE	32U
#define KESTRELFS_CACHE_NAMESPACE_HEX_LEN \
	(2U * KESTRELFS_CACHE_NAMESPACE_SIZE)
#define KESTRELFS_CACHE_METADATA_BYTES	(2ULL * 1024 * 1024)
#define KESTRELFS_CACHE_MIN_BYTES	\
	(KESTRELFS_CACHE_METADATA_BYTES + KESTRELFS_CACHE_BLOCK_SIZE)
#define KESTRELFS_CACHE_SECTORS_PER_BLOCK \
	(KESTRELFS_CACHE_BLOCK_SIZE >> SECTOR_SHIFT)
/* Bound GUP and BIO resources while still batching normal readahead-sized IO. */
#define KESTRELFS_CACHE_DIRECT_MAX_BYTES	(128U * 1024U)

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
	u8 namespace_id[KESTRELFS_CACHE_NAMESPACE_SIZE];
	u8 reserved[KESTRELFS_CACHE_SUPERBLOCK_SIZE - 104];
};

static_assert(sizeof(struct kestrelfs_cache_disk_index_entry) == 32);
static_assert(sizeof(struct kestrelfs_cache_disk_superblock) ==
	      KESTRELFS_CACHE_SUPERBLOCK_SIZE);
static_assert(offsetof(struct kestrelfs_cache_disk_superblock, namespace_id) ==
	      72);
static_assert(KESTRELFS_CACHE_SUPERBLOCK_SIZE <= PAGE_SIZE);

struct kestrelfs_cache_index_key {
	u64 inode_id;
	u64 file_offset;
};

struct kestrelfs_cache_index_entry {
	struct rhash_head node;
	struct list_head list;
	struct kestrelfs_cache_index_key key;
	sector_t lba;
	u64 generation;
	u32 slot;
	bool hashed;
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

static char *cache_namespace;
module_param(cache_namespace, charp, 0444);
MODULE_PARM_DESC(cache_namespace,
		 "SHA-256 metadata namespace identity as exactly 64 hex digits");

static bool cache_direct_io = true;
module_param(cache_direct_io, bool, 0444);
MODULE_PARM_DESC(cache_direct_io,
		 "read aligned cache hits directly into pinned user pages (default Y)");

/* Read-only observability for vng correctness/performance tests. */
static unsigned long kestrelfs_cache_direct_hit_blocks;
module_param_named(cache_direct_hit_blocks,
		   kestrelfs_cache_direct_hit_blocks, ulong, 0444);
MODULE_PARM_DESC(cache_direct_hit_blocks,
		 "4 KiB cache-hit blocks read directly into pinned user pages");

static unsigned long kestrelfs_cache_copy_hit_blocks;
module_param_named(cache_copy_hit_blocks,
		   kestrelfs_cache_copy_hit_blocks, ulong, 0444);
MODULE_PARM_DESC(cache_copy_hit_blocks,
		 "4 KiB cache-hit blocks served through the buffered copy fallback");

static unsigned long kestrelfs_cache_direct_fallbacks;
module_param_named(cache_direct_fallbacks,
		   kestrelfs_cache_direct_fallbacks, ulong, 0444);
MODULE_PARM_DESC(cache_direct_fallbacks,
		 "direct user-page attempts that fell back to buffered cache IO");

static char kestrelfs_cache_holder;
static struct file *kestrelfs_cache_file;
static struct block_device *kestrelfs_cache_bdev;
static struct rhashtable kestrelfs_cache_index;
static LIST_HEAD(kestrelfs_cache_entries);
static DEFINE_MUTEX(kestrelfs_cache_lock);
static unsigned long *kestrelfs_cache_slots;
static bool kestrelfs_cache_index_ready;
static u64 kestrelfs_cache_usable_bytes;
static u64 kestrelfs_cache_mutation_epoch = 1;
static u64 kestrelfs_cache_entry_generation;
static u32 kestrelfs_cache_slot_count;
static u32 kestrelfs_cache_logical_size;
static u32 kestrelfs_cache_physical_size;
static u8 kestrelfs_cache_namespace_id[KESTRELFS_CACHE_NAMESPACE_SIZE];

static int kestrelfs_cache_parse_namespace(void)
{
	int ret;

	if (!cache_namespace ||
	    strlen(cache_namespace) != KESTRELFS_CACHE_NAMESPACE_HEX_LEN) {
		pr_err("kestrelfs: cache_namespace must be exactly 64 hex digits\n");
		return -EINVAL;
	}

	ret = hex2bin(kestrelfs_cache_namespace_id, cache_namespace,
		      KESTRELFS_CACHE_NAMESPACE_SIZE);
	if (ret) {
		pr_err("kestrelfs: cache_namespace contains non-hex characters\n");
		return -EINVAL;
	}

	return 0;
}

static u64 kestrelfs_cache_index_capacity(void)
{
	return (KESTRELFS_CACHE_METADATA_BYTES -
		KESTRELFS_CACHE_SUPERBLOCK_SIZE) /
	       sizeof(struct kestrelfs_cache_disk_index_entry);
}

static sector_t kestrelfs_cache_slot_lba(u32 slot)
{
	return (KESTRELFS_CACHE_METADATA_BYTES >> SECTOR_SHIFT) +
	       (sector_t)slot * KESTRELFS_CACHE_SECTORS_PER_BLOCK;
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
		memcpy(mapped, buffer, KESTRELFS_CACHE_BLOCK_SIZE);
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
	added = bio_add_page(bio, page, KESTRELFS_CACHE_BLOCK_SIZE, 0);
	if (added != KESTRELFS_CACHE_BLOCK_SIZE) {
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
		memcpy(buffer, mapped, KESTRELFS_CACHE_BLOCK_SIZE);
		kunmap_local(mapped);
	}

out_bio:
	bio_put(bio);
out_page:
	__free_page(page);
	return ret;
}

/*
 * Read one or more contiguous 4 KiB cache blocks directly into userspace.
 * The caller holds kestrelfs_cache_lock across pin, BIO completion and unpin,
 * so an invalidation cannot retire/reuse the indexed slots during DMA.
 *
 * Alignment or transient GUP/BIO construction failures are returned to the
 * caller as a request to use the buffered cache path.  Once submitted, all
 * pinned pages are dirtied even on IO failure because the device may have
 * modified a prefix before reporting the error.
 */
static int kestrelfs_cache_read_user_blocks(sector_t sector, char __user *buf,
					     size_t length)
{
	struct page **pages;
	struct bio *bio;
	unsigned long address = (unsigned long)buf;
	unsigned long page_start = address & PAGE_MASK;
	unsigned int page_offset = offset_in_page(address);
	unsigned int dma_alignment;
	unsigned int nr_pages;
	unsigned int i;
	long pinned;
	size_t remaining;
	bool submitted = false;
	int ret = 0;

	if (!length || length > KESTRELFS_CACHE_DIRECT_MAX_BYTES ||
	    length % KESTRELFS_CACHE_BLOCK_SIZE)
		return -EINVAL;

	dma_alignment = bdev_dma_alignment(kestrelfs_cache_bdev);
	if ((address & dma_alignment) ||
	    !IS_ALIGNED(address, kestrelfs_cache_logical_size))
		return -EINVAL;

	nr_pages = DIV_ROUND_UP(page_offset + length, PAGE_SIZE);
	pages = kcalloc(nr_pages, sizeof(*pages), GFP_KERNEL);
	if (!pages)
		return -ENOMEM;

	pinned = pin_user_pages_fast(page_start, nr_pages, FOLL_WRITE, pages);
	if (pinned != nr_pages) {
		if (pinned > 0)
			unpin_user_pages(pages, pinned);
		ret = pinned < 0 ? pinned : -EFAULT;
		goto out_pages;
	}

	bio = bio_alloc(kestrelfs_cache_bdev, nr_pages, REQ_OP_READ, GFP_KERNEL);
	if (!bio) {
		ret = -ENOMEM;
		goto out_unpin;
	}
	bio->bi_iter.bi_sector = sector;
	remaining = length;
	for (i = 0; i < nr_pages && remaining; i++) {
		unsigned int offset = i ? 0 : page_offset;
		unsigned int bytes = min_t(size_t, PAGE_SIZE - offset,
					       remaining);

		if (bio_add_page(bio, pages[i], bytes, offset) != bytes) {
			ret = -EIO;
			goto out_bio;
		}
		remaining -= bytes;
	}
	if (remaining) {
		ret = -EIO;
		goto out_bio;
	}

	submitted = true;
	ret = submit_bio_wait(bio);
out_bio:
	bio_put(bio);
out_unpin:
	if (submitted)
		unpin_user_pages_dirty_lock(pages, nr_pages, true);
	else
		unpin_user_pages(pages, nr_pages);
out_pages:
	kfree(pages);
	return ret;
}

static int kestrelfs_cache_metadata_is_clean(bool *clean)
{
	void *buffer;
	sector_t sector;
	sector_t end = KESTRELFS_CACHE_METADATA_BYTES >> SECTOR_SHIFT;
	int ret = 0;

	buffer = kmalloc(KESTRELFS_CACHE_BLOCK_SIZE, GFP_KERNEL);
	if (!buffer)
		return -ENOMEM;

	*clean = true;
	for (sector = KESTRELFS_CACHE_SUPERBLOCK_SIZE >> SECTOR_SHIFT;
	     sector < end; sector += KESTRELFS_CACHE_SECTORS_PER_BLOCK) {
		ret = kestrelfs_cache_rw_block(buffer, sector, false);
		if (ret)
			break;
		if (memchr_inv(buffer, 0, KESTRELFS_CACHE_BLOCK_SIZE)) {
			*clean = false;
			break;
		}
	}

	kfree(buffer);
	return ret;
}

static int kestrelfs_cache_validate_geometry(void)
{
	u64 data_blocks;
	u64 device_bytes;
	u64 requested_bytes;

	device_bytes = bdev_nr_bytes(kestrelfs_cache_bdev);
	kestrelfs_cache_logical_size =
		bdev_logical_block_size(kestrelfs_cache_bdev);
	kestrelfs_cache_physical_size =
		bdev_physical_block_size(kestrelfs_cache_bdev);

	if (!is_power_of_2(kestrelfs_cache_logical_size) ||
	    kestrelfs_cache_logical_size < SECTOR_SIZE ||
	    KESTRELFS_CACHE_BLOCK_SIZE % kestrelfs_cache_logical_size) {
		pr_err("kestrelfs: unsupported cache logical sector size %u\n",
		       kestrelfs_cache_logical_size);
		return -EINVAL;
	}

	if (!is_power_of_2(kestrelfs_cache_physical_size) ||
	    kestrelfs_cache_physical_size < kestrelfs_cache_logical_size ||
	    KESTRELFS_CACHE_BLOCK_SIZE % kestrelfs_cache_physical_size) {
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

	data_blocks = (kestrelfs_cache_usable_bytes -
		       KESTRELFS_CACHE_METADATA_BYTES) /
		      KESTRELFS_CACHE_BLOCK_SIZE;
	kestrelfs_cache_slot_count = min_t(u64, data_blocks,
					   kestrelfs_cache_index_capacity());
	if (!kestrelfs_cache_slot_count)
		return -ENOSPC;

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
		if (ret)
			goto out;
		if (!clean) {
			pr_err("kestrelfs: zero superblock but nonzero metadata area\n");
			ret = -EINVAL;
			goto out;
		}

		generation = get_random_u64() ?: 1;
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
		memcpy(super->namespace_id, kestrelfs_cache_namespace_id,
		       sizeof(super->namespace_id));

		ret = kestrelfs_cache_rw_block(super, 0, true);
		if (ret)
			goto out;
		pr_info("kestrelfs: formatted cache device=%s generation=%llu\n",
			cache_device, (unsigned long long)generation);
	} else if (!kestrelfs_cache_superblock_matches(super)) {
		pr_err("kestrelfs: cache superblock format or geometry mismatch\n");
		ret = -EINVAL;
		goto out;
	} else if (memcmp(super->namespace_id, kestrelfs_cache_namespace_id,
			  sizeof(super->namespace_id))) {
		pr_err("kestrelfs: cache namespace identity mismatch\n");
		ret = -EINVAL;
		goto out;
	} else {
		pr_info("kestrelfs: reusing cache device=%s generation=%llu\n",
			cache_device,
			(unsigned long long)le64_to_cpu(super->format_generation));
	}

	pr_info("kestrelfs: cache geometry bytes=%llu logical=%u physical=%u "
		"slots=%u data_lba=%llu\n",
		(unsigned long long)kestrelfs_cache_usable_bytes,
		kestrelfs_cache_logical_size, kestrelfs_cache_physical_size,
		kestrelfs_cache_slot_count,
		(unsigned long long)(KESTRELFS_CACHE_METADATA_BYTES >>
				     SECTOR_SHIFT));
out:
	kfree(super);
	return ret;
}

static int kestrelfs_cache_rw_index_entry(
	u32 slot, struct kestrelfs_cache_disk_index_entry *disk, bool write)
{
	u64 byte_offset = KESTRELFS_CACHE_SUPERBLOCK_SIZE +
			  (u64)slot * sizeof(*disk);
	sector_t sector = round_down(byte_offset,
				     (u64)KESTRELFS_CACHE_BLOCK_SIZE) >>
			  SECTOR_SHIFT;
	u32 in_block = byte_offset % KESTRELFS_CACHE_BLOCK_SIZE;
	void *block;
	int ret;

	if (slot >= kestrelfs_cache_index_capacity())
		return -EINVAL;
	block = kmalloc(KESTRELFS_CACHE_BLOCK_SIZE, GFP_KERNEL);
	if (!block)
		return -ENOMEM;

	ret = kestrelfs_cache_rw_block(block, sector, false);
	if (ret)
		goto out;
	if (write) {
		memcpy((u8 *)block + in_block, disk, sizeof(*disk));
		ret = kestrelfs_cache_rw_block(block, sector, true);
	} else {
		memcpy(disk, (u8 *)block + in_block, sizeof(*disk));
	}
out:
	kfree(block);
	return ret;
}

static void kestrelfs_cache_free_index(void)
{
	struct kestrelfs_cache_index_entry *entry, *tmp;

	if (kestrelfs_cache_index_ready) {
		list_for_each_entry_safe(entry, tmp, &kestrelfs_cache_entries,
					 list) {
			if (entry->hashed)
				rhashtable_remove_fast(&kestrelfs_cache_index,
						       &entry->node,
						       kestrelfs_cache_index_params);
			list_del(&entry->list);
			kfree(entry);
		}
		rhashtable_destroy(&kestrelfs_cache_index);
		kestrelfs_cache_index_ready = false;
	}
	bitmap_free(kestrelfs_cache_slots);
	kestrelfs_cache_slots = NULL;
	INIT_LIST_HEAD(&kestrelfs_cache_entries);
}

static int kestrelfs_cache_restore_index(void)
{
	struct kestrelfs_cache_disk_index_entry *disk;
	struct kestrelfs_cache_index_entry *entry;
	u64 inode_id, file_offset, lba, generation;
	u64 restored = 0;
	void *block = NULL;
	sector_t sector;
	u32 entries_per_block = KESTRELFS_CACHE_BLOCK_SIZE / sizeof(*disk);
	u32 in_block;
	u32 slot;
	int ret;

	ret = rhashtable_init(&kestrelfs_cache_index,
			      &kestrelfs_cache_index_params);
	if (ret)
		return ret;
	kestrelfs_cache_index_ready = true;
	kestrelfs_cache_slots = bitmap_zalloc(kestrelfs_cache_slot_count,
					      GFP_KERNEL);
	if (!kestrelfs_cache_slots) {
		ret = -ENOMEM;
		goto err;
	}
	block = kmalloc(KESTRELFS_CACHE_BLOCK_SIZE, GFP_KERNEL);
	if (!block) {
		ret = -ENOMEM;
		goto err;
	}

	for (slot = 0; slot < kestrelfs_cache_index_capacity();
	     slot += entries_per_block) {
		sector = (KESTRELFS_CACHE_SUPERBLOCK_SIZE >> SECTOR_SHIFT) +
			 (sector_t)(slot / entries_per_block) *
			 KESTRELFS_CACHE_SECTORS_PER_BLOCK;
		ret = kestrelfs_cache_rw_block(block, sector, false);
		if (ret)
			goto err;

		for (in_block = 0;
		     in_block < entries_per_block &&
		     slot + in_block < kestrelfs_cache_index_capacity();
		     in_block++) {
			disk = (struct kestrelfs_cache_disk_index_entry *)block +
			       in_block;
			inode_id = le64_to_cpu(disk->inode_id);
			file_offset = le64_to_cpu(disk->file_offset);
			lba = le64_to_cpu(disk->lba);
			generation = le64_to_cpu(disk->generation);

			if (!inode_id) {
				if (file_offset || lba || generation) {
					ret = -EINVAL;
					goto corrupt;
				}
				continue;
			}
			if (slot + in_block >= kestrelfs_cache_slot_count ||
			    file_offset % KESTRELFS_CACHE_BLOCK_SIZE ||
			    lba != kestrelfs_cache_slot_lba(slot + in_block) ||
			    !generation) {
				ret = -EINVAL;
				goto corrupt;
			}

			entry = kzalloc(sizeof(*entry), GFP_KERNEL);
			if (!entry) {
				ret = -ENOMEM;
				goto err;
			}
			entry->key.inode_id = inode_id;
			entry->key.file_offset = file_offset;
			entry->lba = lba;
			entry->generation = generation;
			entry->slot = slot + in_block;
			ret = rhashtable_insert_fast(&kestrelfs_cache_index,
						     &entry->node,
						     kestrelfs_cache_index_params);
			if (ret) {
				kfree(entry);
				goto corrupt;
			}
			entry->hashed = true;
			list_add_tail(&entry->list, &kestrelfs_cache_entries);
			__set_bit(entry->slot, kestrelfs_cache_slots);
			kestrelfs_cache_entry_generation =
				max(kestrelfs_cache_entry_generation, generation);
			restored++;
		}
	}

	kfree(block);
	pr_info("kestrelfs: restored %llu cache index entries\n",
		(unsigned long long)restored);
	return 0;

corrupt:
	pr_err("kestrelfs: invalid cache index entry at slot %u\n",
		slot + in_block);
err:
	kfree(block);
	kestrelfs_cache_free_index();
	return ret;
}

static struct kestrelfs_cache_index_entry *
kestrelfs_cache_find(u64 inode_id, u64 file_offset)
{
	struct kestrelfs_cache_index_key key = {
		.inode_id = inode_id,
		.file_offset = file_offset,
	};

	return rhashtable_lookup_fast(&kestrelfs_cache_index, &key,
				      kestrelfs_cache_index_params);
}

static int kestrelfs_cache_fill_block(u64 inode_id, u64 file_offset,
				      const u8 *data, size_t length)
{
	struct kestrelfs_cache_disk_index_entry disk = { 0 };
	struct kestrelfs_cache_index_entry *entry;
	unsigned long slot;
	void *block;
	int ret;

	if (kestrelfs_cache_find(inode_id, file_offset))
		return 0;
	slot = find_first_zero_bit(kestrelfs_cache_slots,
				   kestrelfs_cache_slot_count);
	if (slot >= kestrelfs_cache_slot_count)
		return -ENOSPC;

	entry = kzalloc(sizeof(*entry), GFP_KERNEL);
	block = kzalloc(KESTRELFS_CACHE_BLOCK_SIZE, GFP_KERNEL);
	if (!entry || !block) {
		ret = -ENOMEM;
		goto out;
	}
	memcpy(block, data, length);
	entry->key.inode_id = inode_id;
	entry->key.file_offset = file_offset;
	entry->lba = kestrelfs_cache_slot_lba(slot);
	if (!++kestrelfs_cache_entry_generation)
		kestrelfs_cache_entry_generation = 1;
	entry->generation = kestrelfs_cache_entry_generation;
	entry->slot = slot;

	ret = kestrelfs_cache_rw_block(block, entry->lba, true);
	if (ret)
		goto out;
	/*
	 * Reserve the in-memory key while the cache mutex still hides it from
	 * lookup.  This prevents publishing a persistent entry that cannot be
	 * represented in the hash table (and avoids duplicate keys on retry).
	 */
	ret = rhashtable_insert_fast(&kestrelfs_cache_index, &entry->node,
				     kestrelfs_cache_index_params);
	if (ret)
		goto out;
	entry->hashed = true;

	disk.inode_id = cpu_to_le64(inode_id);
	disk.file_offset = cpu_to_le64(file_offset);
	disk.lba = cpu_to_le64(entry->lba);
	disk.generation = cpu_to_le64(entry->generation);
	ret = kestrelfs_cache_rw_index_entry(slot, &disk, true);
	if (ret) {
		rhashtable_remove_fast(&kestrelfs_cache_index, &entry->node,
				       kestrelfs_cache_index_params);
		entry->hashed = false;
		goto out;
	}

	__set_bit(slot, kestrelfs_cache_slots);
	list_add_tail(&entry->list, &kestrelfs_cache_entries);
	entry = NULL;
out:
	kfree(block);
	kfree(entry);
	return ret;
}

ssize_t kestrelfs_cache_lookup(struct inode *inode, char __user *buf,
			       size_t count, loff_t *ppos, u64 *miss_epoch)
{
	struct kestrelfs_cache_index_entry *entry;
	u64 inode_id = inode->i_ino;
	u64 file_size;
	u64 offset;
	u64 end;
	u64 block_offset;
	size_t wanted;
	size_t copied = 0;
	void *block = NULL;
	ssize_t ret = -ENODATA;

	*miss_epoch = 0;
	if (!READ_ONCE(kestrelfs_cache_index_ready))
		return -ENODATA;
	if (mutex_lock_interruptible(&kestrelfs_cache_lock))
		return -EINTR;

	*miss_epoch = kestrelfs_cache_mutation_epoch;
	file_size = i_size_read(inode);
	offset = *ppos;
	if (offset >= file_size) {
		ret = 0;
		goto out;
	}
	wanted = min_t(u64, count, file_size - offset);
	end = offset + wanted;

	block_offset = round_down(offset, (u64)KESTRELFS_CACHE_BLOCK_SIZE);
	for (; block_offset < end; block_offset += KESTRELFS_CACHE_BLOCK_SIZE) {
		if (!kestrelfs_cache_find(inode_id, block_offset))
			goto out;
	}

	block_offset = round_down(offset, (u64)KESTRELFS_CACHE_BLOCK_SIZE);
	while (block_offset < end) {
		u32 within = offset > block_offset ? offset - block_offset : 0;
		size_t bytes = min_t(u64, KESTRELFS_CACHE_BLOCK_SIZE - within,
				     end - (block_offset + within));
		size_t direct_bytes = KESTRELFS_CACHE_BLOCK_SIZE;

		entry = kestrelfs_cache_find(inode_id, block_offset);
		if (!entry) {
			ret = -ENODATA;
			goto out;
		}

		/*
		 * Coalesce complete file blocks whose cache slots are contiguous.
		 * Head/tail partial blocks deliberately stay on the buffered path:
		 * a block-device BIO must never overwrite bytes outside the read(2)
		 * range in a userspace page.
		 */
		if (cache_direct_io && !within &&
		    bytes == KESTRELFS_CACHE_BLOCK_SIZE) {
			while (direct_bytes < KESTRELFS_CACHE_DIRECT_MAX_BYTES &&
			       block_offset + direct_bytes +
				       KESTRELFS_CACHE_BLOCK_SIZE <= end) {
				struct kestrelfs_cache_index_entry *next;

				next = kestrelfs_cache_find(inode_id,
						 block_offset + direct_bytes);
				if (!next || next->lba != entry->lba +
						(direct_bytes >> SECTOR_SHIFT))
					break;
				direct_bytes += KESTRELFS_CACHE_BLOCK_SIZE;
			}

			if (!kestrelfs_cache_read_user_blocks(entry->lba,
							   buf + copied,
							   direct_bytes)) {
				kestrelfs_cache_direct_hit_blocks +=
					direct_bytes / KESTRELFS_CACHE_BLOCK_SIZE;
				copied += direct_bytes;
				offset += direct_bytes;
				block_offset += direct_bytes;
				continue;
			}
			kestrelfs_cache_direct_fallbacks++;
		}

		if (!block)
			block = kmalloc(KESTRELFS_CACHE_BLOCK_SIZE, GFP_KERNEL);
		if (!block) {
			ret = -ENODATA;
			goto out;
		}
		if (kestrelfs_cache_rw_block(block, entry->lba, false)) {
			ret = -ENODATA;
			goto out;
		}
		if (copy_to_user(buf + copied, (u8 *)block + within, bytes)) {
			ret = -EFAULT;
			goto out;
		}
		copied += bytes;
		offset += bytes;
		block_offset += KESTRELFS_CACHE_BLOCK_SIZE;
		kestrelfs_cache_copy_hit_blocks++;
	}

	*ppos += copied;
	ret = copied;
out:
	kfree(block);
	mutex_unlock(&kestrelfs_cache_lock);
	return ret;
}

void kestrelfs_cache_fill(struct inode *inode, u64 offset, const u8 *data,
			  size_t length, u64 miss_epoch)
{
	u64 file_size;
	u64 end;
	u64 block_offset;
	int ret;

	if (!miss_epoch || !READ_ONCE(kestrelfs_cache_index_ready) || !length)
		return;
	mutex_lock(&kestrelfs_cache_lock);
	if (miss_epoch != kestrelfs_cache_mutation_epoch)
		goto out;

	file_size = i_size_read(inode);
	end = offset + length;
	block_offset = round_up(offset, (u64)KESTRELFS_CACHE_BLOCK_SIZE);
	if (block_offset == offset && block_offset < end) {
		/* Already aligned. */
	} else if (block_offset >= end) {
		goto out;
	}

	for (; block_offset < end; block_offset += KESTRELFS_CACHE_BLOCK_SIZE) {
		size_t available = min_t(u64, KESTRELFS_CACHE_BLOCK_SIZE,
					 end - block_offset);

		if (available < KESTRELFS_CACHE_BLOCK_SIZE &&
		    block_offset + available != file_size)
			break;
		ret = kestrelfs_cache_fill_block(inode->i_ino, block_offset,
					 data + (block_offset - offset), available);
		if (ret) {
			if (ret != -ENOSPC)
				pr_warn("kestrelfs: cache fill failed: %d\n", ret);
			break;
		}
	}
out:
	mutex_unlock(&kestrelfs_cache_lock);
}

int kestrelfs_cache_invalidate_inode(u64 inode_id)
{
	struct kestrelfs_cache_disk_index_entry disk = { 0 };
	struct kestrelfs_cache_index_entry *entry, *tmp;
	int ret = 0;

	if (!READ_ONCE(kestrelfs_cache_index_ready))
		return 0;
	if (mutex_lock_interruptible(&kestrelfs_cache_lock))
		return -EINTR;
	if (!++kestrelfs_cache_mutation_epoch)
		kestrelfs_cache_mutation_epoch = 1;

	list_for_each_entry_safe(entry, tmp, &kestrelfs_cache_entries, list) {
		if (entry->key.inode_id != inode_id)
			continue;
		ret = kestrelfs_cache_rw_index_entry(entry->slot, &disk, true);
		if (ret)
			break;
		if (entry->hashed)
			rhashtable_remove_fast(&kestrelfs_cache_index, &entry->node,
					       kestrelfs_cache_index_params);
		__clear_bit(entry->slot, kestrelfs_cache_slots);
		list_del(&entry->list);
		kfree(entry);
	}

	mutex_unlock(&kestrelfs_cache_lock);
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
	kestrelfs_cache_direct_hit_blocks = 0;
	kestrelfs_cache_copy_hit_blocks = 0;
	kestrelfs_cache_direct_fallbacks = 0;
	ret = kestrelfs_cache_parse_namespace();
	if (ret)
		return ret;

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
	ret = kestrelfs_cache_restore_index();
	if (ret)
		goto err_release;

	return 0;

err_release:
	kestrelfs_cache_free_index();
	bdev_fput(kestrelfs_cache_file);
	kestrelfs_cache_file = NULL;
	kestrelfs_cache_bdev = NULL;
	return ret;
}

void kestrelfs_cache_exit(void)
{
	mutex_lock(&kestrelfs_cache_lock);
	pr_info("kestrelfs: cache hit stats direct_blocks=%lu copied_blocks=%lu direct_fallbacks=%lu\n",
		kestrelfs_cache_direct_hit_blocks,
		kestrelfs_cache_copy_hit_blocks,
		kestrelfs_cache_direct_fallbacks);
	kestrelfs_cache_free_index();
	if (kestrelfs_cache_file) {
		bdev_fput(kestrelfs_cache_file);
		kestrelfs_cache_file = NULL;
		kestrelfs_cache_bdev = NULL;
	}
	mutex_unlock(&kestrelfs_cache_lock);
}
