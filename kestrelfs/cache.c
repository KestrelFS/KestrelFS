// SPDX-License-Identifier: GPL-2.0
/*
 * cache.c - Kernel-owned persistent block cache for KestrelFS.
 *
 * Step 21 binds Step 20's persistent 4 KiB cache to one metadata namespace.
 * Step 22 sends aligned, contiguous cache hits straight into pinned user pages;
 * unaligned head/tail blocks retain the buffered fallback.  Step 23 adds an
 * in-memory block LRU so a full cache can recycle slots.  Step 24 verifies a
 * persisted checksum before returning any hit.  Step 25 adds one persistent
 * intent-journal page: an interrupted index transaction is recovered to a
 * safe miss before the index is restored.  Cache failures never replace the
 * authoritative daemon/ObjectStore path.
 */

#include <linux/bio.h>
#include <linux/bitmap.h>
#include <linux/blkdev.h>
#include <linux/build_bug.h>
#include <linux/crc32.h>
#include <linux/err.h>
#include <linux/errno.h>
#include <linux/fs.h>
#include <linux/highmem.h>
#include <linux/hex.h>
#include <linux/list.h>
#include <linux/list_sort.h>
#include <linux/mm.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/random.h>
#include <linux/rhashtable.h>
#include <linux/string.h>
#include <linux/uaccess.h>

#include "kestrelfs.h"

#define KESTRELFS_CACHE_MAGIC		0x454843414353464bULL /* "KFSCACHE" LE */
#define KESTRELFS_CACHE_FORMAT_VERSION	4U
#define KESTRELFS_CACHE_SUPERBLOCK_SIZE	4096U
#define KESTRELFS_CACHE_BLOCK_SIZE	4096U
#define KESTRELFS_CACHE_JOURNAL_MAGIC	0x4e52554f4a53464bULL /* "KFSJOURN" LE */
#define KESTRELFS_CACHE_JOURNAL_VERSION	1U
#define KESTRELFS_CACHE_JOURNAL_PREPARED	1U
#define KESTRELFS_CACHE_JOURNAL_START_LBA \
	(KESTRELFS_CACHE_SUPERBLOCK_SIZE >> SECTOR_SHIFT)
#define KESTRELFS_CACHE_INDEX_START_BYTES \
	(KESTRELFS_CACHE_SUPERBLOCK_SIZE + KESTRELFS_CACHE_BLOCK_SIZE)
#define KESTRELFS_CACHE_INDEX_START_LBA \
	(KESTRELFS_CACHE_INDEX_START_BYTES >> SECTOR_SHIFT)
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
	__le64 generation;
	__le32 data_checksum;
	__le32 entry_checksum;
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
	__le64 journal_start_lba;
	__le32 journal_size;
	__le32 feature_flags;
	u8 reserved[KESTRELFS_CACHE_SUPERBLOCK_SIZE - 124];
	__le32 super_checksum;
};

enum kestrelfs_cache_journal_operation {
	KESTRELFS_CACHE_JOURNAL_FILL = 1,
	KESTRELFS_CACHE_JOURNAL_INVALIDATE = 2,
	KESTRELFS_CACHE_JOURNAL_EVICT = 3,
	KESTRELFS_CACHE_JOURNAL_RETIRE_CORRUPT = 4,
};

struct kestrelfs_cache_disk_journal {
	__le64 magic;
	__le32 version;
	__le32 state;
	__le64 sequence;
	__le32 operation;
	__le32 slot;
	struct kestrelfs_cache_disk_index_entry old_entry;
	struct kestrelfs_cache_disk_index_entry new_entry;
	u8 reserved[KESTRELFS_CACHE_BLOCK_SIZE - 100];
	__le32 checksum;
};

static_assert(sizeof(struct kestrelfs_cache_disk_index_entry) == 32);
static_assert(offsetof(struct kestrelfs_cache_disk_index_entry,
		       entry_checksum) == 28);
static_assert(sizeof(struct kestrelfs_cache_disk_superblock) ==
	      KESTRELFS_CACHE_SUPERBLOCK_SIZE);
static_assert(offsetof(struct kestrelfs_cache_disk_superblock, namespace_id) ==
	      72);
static_assert(offsetof(struct kestrelfs_cache_disk_superblock,
		       journal_start_lba) == 104);
static_assert(offsetof(struct kestrelfs_cache_disk_superblock,
		       super_checksum) == 4092);
static_assert(sizeof(struct kestrelfs_cache_disk_journal) ==
	      KESTRELFS_CACHE_BLOCK_SIZE);
static_assert(offsetof(struct kestrelfs_cache_disk_journal, old_entry) == 32);
static_assert(offsetof(struct kestrelfs_cache_disk_journal, new_entry) == 64);
static_assert(offsetof(struct kestrelfs_cache_disk_journal, checksum) == 4092);
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
	u32 data_checksum;
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

static unsigned long kestrelfs_cache_evictions;
module_param_named(cache_evictions, kestrelfs_cache_evictions, ulong, 0444);
MODULE_PARM_DESC(cache_evictions,
		 "cache blocks retired for slot reuse since module load");

static unsigned long kestrelfs_cache_checksum_failures;
module_param_named(cache_checksum_failures,
		   kestrelfs_cache_checksum_failures, ulong, 0444);
MODULE_PARM_DESC(cache_checksum_failures,
		 "data blocks rejected because their checksum did not match");

static unsigned long kestrelfs_cache_journal_recoveries;
module_param_named(cache_journal_recoveries,
		   kestrelfs_cache_journal_recoveries, ulong, 0444);
MODULE_PARM_DESC(cache_journal_recoveries,
		 "incomplete index transactions recovered to cache misses");

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
static u64 kestrelfs_cache_journal_sequence;
static u32 kestrelfs_cache_slot_count;
static u32 kestrelfs_cache_logical_size;
static u32 kestrelfs_cache_physical_size;
static u8 kestrelfs_cache_namespace_id[KESTRELFS_CACHE_NAMESPACE_SIZE];
static bool kestrelfs_cache_journal_active;

static u32 kestrelfs_cache_checksum(const void *data, size_t length)
{
	return crc32_le(~0U, data, length) ^ ~0U;
}

static u32 kestrelfs_cache_disk_entry_checksum(
	const struct kestrelfs_cache_disk_index_entry *disk)
{
	return kestrelfs_cache_checksum(disk,
		offsetof(struct kestrelfs_cache_disk_index_entry,
			 entry_checksum));
}

static u32 kestrelfs_cache_super_checksum(
	const struct kestrelfs_cache_disk_superblock *super)
{
	return kestrelfs_cache_checksum(super,
		offsetof(struct kestrelfs_cache_disk_superblock,
			 super_checksum));
}

static u32 kestrelfs_cache_journal_checksum(
	const struct kestrelfs_cache_disk_journal *journal)
{
	return kestrelfs_cache_checksum(journal,
		offsetof(struct kestrelfs_cache_disk_journal, checksum));
}

static u32 kestrelfs_cache_pinned_checksum(struct page **pages,
					    unsigned int first_offset,
					    size_t start, size_t length)
{
	size_t absolute = first_offset + start;
	size_t remaining = length;
	u32 checksum = ~0U;

	while (remaining) {
		unsigned int page_index = absolute / PAGE_SIZE;
		unsigned int offset = absolute % PAGE_SIZE;
		unsigned int bytes = min_t(size_t, PAGE_SIZE - offset,
					       remaining);
		void *mapped = kmap_local_page(pages[page_index]);

		checksum = crc32_le(checksum, (u8 *)mapped + offset, bytes);
		kunmap_local(mapped);
		absolute += bytes;
		remaining -= bytes;
	}
	return checksum ^ ~0U;
}

static int kestrelfs_cache_lru_compare(void *priv,
				       const struct list_head *left,
				       const struct list_head *right)
{
	const struct kestrelfs_cache_index_entry *left_entry;
	const struct kestrelfs_cache_index_entry *right_entry;

	left_entry = list_entry(left, struct kestrelfs_cache_index_entry, list);
	right_entry = list_entry(right, struct kestrelfs_cache_index_entry, list);
	if (left_entry->generation < right_entry->generation)
		return -1;
	if (left_entry->generation > right_entry->generation)
		return 1;
	if (left_entry->slot < right_entry->slot)
		return -1;
	if (left_entry->slot > right_entry->slot)
		return 1;
	return 0;
}

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
		KESTRELFS_CACHE_INDEX_START_BYTES) /
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
					     size_t length,
					     const u32 *checksums,
					     unsigned int *bad_block)
{
	struct page **pages;
	struct bio *bio;
	unsigned long address = (unsigned long)buf;
	unsigned long page_start = address & PAGE_MASK;
	unsigned int page_offset = offset_in_page(address);
	unsigned int dma_alignment;
	unsigned int nr_pages;
	unsigned int nr_blocks;
	unsigned int i;
	long pinned;
	size_t remaining;
	bool submitted = false;
	int ret = 0;

	if (!length || length > KESTRELFS_CACHE_DIRECT_MAX_BYTES ||
	    length % KESTRELFS_CACHE_BLOCK_SIZE)
		return -EINVAL;
	nr_blocks = length / KESTRELFS_CACHE_BLOCK_SIZE;

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
	if (!ret) {
		for (i = 0; i < nr_blocks; i++) {
			u32 actual = kestrelfs_cache_pinned_checksum(
				pages, page_offset,
				i * KESTRELFS_CACHE_BLOCK_SIZE,
				KESTRELFS_CACHE_BLOCK_SIZE);

			if (actual != checksums[i]) {
				*bad_block = i;
				ret = -EILSEQ;
				break;
			}
		}
	}
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
	       le32_to_cpu(super->super_checksum) ==
			kestrelfs_cache_super_checksum(super) &&
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
			KESTRELFS_CACHE_INDEX_START_LBA &&
	       le64_to_cpu(super->index_capacity) ==
			kestrelfs_cache_index_capacity() &&
	       le64_to_cpu(super->data_start_lba) ==
			(KESTRELFS_CACHE_METADATA_BYTES >> SECTOR_SHIFT) &&
	       le64_to_cpu(super->format_generation) != 0 &&
	       le64_to_cpu(super->journal_start_lba) ==
			KESTRELFS_CACHE_JOURNAL_START_LBA &&
	       le32_to_cpu(super->journal_size) ==
			KESTRELFS_CACHE_BLOCK_SIZE &&
	       !le32_to_cpu(super->feature_flags) &&
	       !memchr_inv(super->reserved, 0, sizeof(super->reserved));
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
			KESTRELFS_CACHE_INDEX_START_LBA);
		super->index_capacity =
			cpu_to_le64(kestrelfs_cache_index_capacity());
		super->data_start_lba = cpu_to_le64(
			KESTRELFS_CACHE_METADATA_BYTES >> SECTOR_SHIFT);
		super->format_generation = cpu_to_le64(generation);
		memcpy(super->namespace_id, kestrelfs_cache_namespace_id,
		       sizeof(super->namespace_id));
		super->journal_start_lba = cpu_to_le64(
			KESTRELFS_CACHE_JOURNAL_START_LBA);
		super->journal_size = cpu_to_le32(KESTRELFS_CACHE_BLOCK_SIZE);
		super->super_checksum = cpu_to_le32(
			kestrelfs_cache_super_checksum(super));

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
	u64 byte_offset = KESTRELFS_CACHE_INDEX_START_BYTES +
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

static bool kestrelfs_cache_disk_entry_empty(
	const struct kestrelfs_cache_disk_index_entry *disk)
{
	return !memchr_inv(disk, 0, sizeof(*disk));
}

static bool kestrelfs_cache_disk_entry_valid(
	const struct kestrelfs_cache_disk_index_entry *disk)
{
	return le64_to_cpu(disk->inode_id) &&
	       !(le64_to_cpu(disk->file_offset) % KESTRELFS_CACHE_BLOCK_SIZE) &&
	       le64_to_cpu(disk->generation) &&
	       le32_to_cpu(disk->entry_checksum) ==
			kestrelfs_cache_disk_entry_checksum(disk);
}

static void kestrelfs_cache_entry_to_disk(
	const struct kestrelfs_cache_index_entry *entry,
	struct kestrelfs_cache_disk_index_entry *disk)
{
	memset(disk, 0, sizeof(*disk));
	disk->inode_id = cpu_to_le64(entry->key.inode_id);
	disk->file_offset = cpu_to_le64(entry->key.file_offset);
	disk->generation = cpu_to_le64(entry->generation);
	disk->data_checksum = cpu_to_le32(entry->data_checksum);
	disk->entry_checksum = cpu_to_le32(
		kestrelfs_cache_disk_entry_checksum(disk));
}

static const char *kestrelfs_cache_journal_operation_name(u32 operation)
{
	switch (operation) {
	case KESTRELFS_CACHE_JOURNAL_FILL:
		return "fill";
	case KESTRELFS_CACHE_JOURNAL_INVALIDATE:
		return "invalidate";
	case KESTRELFS_CACHE_JOURNAL_EVICT:
		return "evict";
	case KESTRELFS_CACHE_JOURNAL_RETIRE_CORRUPT:
		return "retire-corrupt";
	default:
		return "invalid";
	}
}

static bool kestrelfs_cache_journal_valid(
	const struct kestrelfs_cache_disk_journal *journal)
{
	u32 operation = le32_to_cpu(journal->operation);
	bool fill = operation == KESTRELFS_CACHE_JOURNAL_FILL;
	bool removal = operation == KESTRELFS_CACHE_JOURNAL_INVALIDATE ||
		       operation == KESTRELFS_CACHE_JOURNAL_EVICT ||
		       operation == KESTRELFS_CACHE_JOURNAL_RETIRE_CORRUPT;

	return le64_to_cpu(journal->magic) == KESTRELFS_CACHE_JOURNAL_MAGIC &&
	       le32_to_cpu(journal->version) ==
			KESTRELFS_CACHE_JOURNAL_VERSION &&
	       le32_to_cpu(journal->state) ==
			KESTRELFS_CACHE_JOURNAL_PREPARED &&
	       le64_to_cpu(journal->sequence) &&
	       le32_to_cpu(journal->slot) < kestrelfs_cache_slot_count &&
	       (fill || removal) &&
	       le32_to_cpu(journal->checksum) ==
			kestrelfs_cache_journal_checksum(journal) &&
	       ((fill &&
		 kestrelfs_cache_disk_entry_empty(&journal->old_entry) &&
		 kestrelfs_cache_disk_entry_valid(&journal->new_entry)) ||
		(removal &&
		 kestrelfs_cache_disk_entry_valid(&journal->old_entry) &&
		 kestrelfs_cache_disk_entry_empty(&journal->new_entry)));
}

static int kestrelfs_cache_journal_begin(
	enum kestrelfs_cache_journal_operation operation, u32 slot,
	const struct kestrelfs_cache_disk_index_entry *old_entry,
	const struct kestrelfs_cache_disk_index_entry *new_entry)
{
	struct kestrelfs_cache_disk_journal *journal;
	int ret;

	if (kestrelfs_cache_journal_active)
		return -EIO;
	journal = kzalloc(sizeof(*journal), GFP_KERNEL);
	if (!journal)
		return -ENOMEM;
	if (!++kestrelfs_cache_journal_sequence)
		kestrelfs_cache_journal_sequence = 1;
	journal->magic = cpu_to_le64(KESTRELFS_CACHE_JOURNAL_MAGIC);
	journal->version = cpu_to_le32(KESTRELFS_CACHE_JOURNAL_VERSION);
	journal->state = cpu_to_le32(KESTRELFS_CACHE_JOURNAL_PREPARED);
	journal->sequence = cpu_to_le64(kestrelfs_cache_journal_sequence);
	journal->operation = cpu_to_le32(operation);
	journal->slot = cpu_to_le32(slot);
	journal->old_entry = *old_entry;
	journal->new_entry = *new_entry;
	journal->checksum = cpu_to_le32(
		kestrelfs_cache_journal_checksum(journal));

	/* A failed write may still be partial; block later transactions. */
	kestrelfs_cache_journal_active = true;
	ret = kestrelfs_cache_rw_block(journal,
				       KESTRELFS_CACHE_JOURNAL_START_LBA, true);
	kfree(journal);
	return ret;
}

static int kestrelfs_cache_journal_clear(void)
{
	void *empty;
	int ret;

	empty = kzalloc(KESTRELFS_CACHE_BLOCK_SIZE, GFP_KERNEL);
	if (!empty)
		return -ENOMEM;
	ret = kestrelfs_cache_rw_block(empty,
				       KESTRELFS_CACHE_JOURNAL_START_LBA, true);
	kfree(empty);
	if (!ret)
		kestrelfs_cache_journal_active = false;
	return ret;
}

static int kestrelfs_cache_journal_replace(
	enum kestrelfs_cache_journal_operation operation, u32 slot,
	const struct kestrelfs_cache_disk_index_entry *old_entry,
	struct kestrelfs_cache_disk_index_entry *new_entry)
{
	int ret;

	ret = kestrelfs_cache_journal_begin(operation, slot, old_entry,
					    new_entry);
	if (ret)
		return ret;
	ret = kestrelfs_cache_rw_index_entry(slot, new_entry, true);
	if (ret)
		return ret;
	return kestrelfs_cache_journal_clear();
}

static int kestrelfs_cache_recover_journal(void)
{
	struct kestrelfs_cache_disk_journal *journal;
	struct kestrelfs_cache_disk_index_entry empty = { 0 };
	u64 sequence;
	u32 operation;
	u32 slot;
	int ret;

	journal = kzalloc(sizeof(*journal), GFP_KERNEL);
	if (!journal)
		return -ENOMEM;
	ret = kestrelfs_cache_rw_block(journal,
				       KESTRELFS_CACHE_JOURNAL_START_LBA, false);
	if (ret)
		goto out;
	if (!memchr_inv(journal, 0, sizeof(*journal))) {
		kestrelfs_cache_journal_active = false;
		ret = 0;
		goto out;
	}
	if (!kestrelfs_cache_journal_valid(journal)) {
		pr_err("kestrelfs: invalid or torn cache journal\n");
		ret = -EILSEQ;
		goto out;
	}

	sequence = le64_to_cpu(journal->sequence);
	operation = le32_to_cpu(journal->operation);
	slot = le32_to_cpu(journal->slot);
	kestrelfs_cache_journal_sequence =
		max(kestrelfs_cache_journal_sequence, sequence);
	kestrelfs_cache_journal_active = true;

	/* PREPARED is intentionally recovered to an empty slot, never replayed. */
	ret = kestrelfs_cache_rw_index_entry(slot, &empty, true);
	if (ret)
		goto out;
	ret = kestrelfs_cache_journal_clear();
	if (ret)
		goto out;
	kestrelfs_cache_journal_recoveries++;
	pr_info("kestrelfs: recovered incomplete cache %s transaction sequence=%llu slot=%u to miss\n",
		kestrelfs_cache_journal_operation_name(operation),
		(unsigned long long)sequence, slot);
out:
	kfree(journal);
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
	u64 inode_id, file_offset, generation;
	u32 data_checksum;
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
		sector = KESTRELFS_CACHE_INDEX_START_LBA +
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
			generation = le64_to_cpu(disk->generation);

			if (!inode_id) {
				if (memchr_inv(disk, 0, sizeof(*disk))) {
					ret = -EINVAL;
					goto corrupt;
				}
				continue;
			}
			if (le32_to_cpu(disk->entry_checksum) !=
			    kestrelfs_cache_disk_entry_checksum(disk)) {
				ret = -EILSEQ;
				goto corrupt;
			}
			data_checksum = le32_to_cpu(disk->data_checksum);
			if (slot + in_block >= kestrelfs_cache_slot_count ||
			    file_offset % KESTRELFS_CACHE_BLOCK_SIZE ||
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
			entry->lba = kestrelfs_cache_slot_lba(slot + in_block);
			entry->generation = generation;
			entry->data_checksum = data_checksum;
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

	/* Persisted generations recover insertion order, not runtime hit recency. */
	list_sort(NULL, &kestrelfs_cache_entries,
		  kestrelfs_cache_lru_compare);
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

/*
 * A corrupt data block must stop being a hit even if persisting the cleared
 * entry fails.  Keep that slot reserved for this module lifetime when the
 * clear fails: reusing it could leave an old key on disk pointing at another
 * key's identical data after a crash.  Reload may restore the old entry, but
 * its data checksum will reject it again before any bytes are returned.
 */
static void kestrelfs_cache_discard_corrupt(
	struct kestrelfs_cache_index_entry *entry)
{
	struct kestrelfs_cache_disk_index_entry old_entry;
	struct kestrelfs_cache_disk_index_entry empty = { 0 };
	int ret;

	pr_warn_ratelimited("kestrelfs: cache data checksum mismatch inode=%llu offset=%llu slot=%u\n",
		(unsigned long long)entry->key.inode_id,
		(unsigned long long)entry->key.file_offset, entry->slot);
	kestrelfs_cache_entry_to_disk(entry, &old_entry);
	ret = kestrelfs_cache_journal_replace(
		KESTRELFS_CACHE_JOURNAL_RETIRE_CORRUPT, entry->slot,
		&old_entry, &empty);
	if (ret) {
		pr_warn("kestrelfs: failed to persist corrupt cache entry removal: %d\n",
			ret);
	} else {
		__clear_bit(entry->slot, kestrelfs_cache_slots);
	}
	if (entry->hashed)
		rhashtable_remove_fast(&kestrelfs_cache_index, &entry->node,
				       kestrelfs_cache_index_params);
	list_del(&entry->list);
	kfree(entry);
	kestrelfs_cache_checksum_failures++;
}

/*
 * Retire the least-recently-used entry before its data slot is overwritten.
 * Clearing and flushing the persistent index first is the key invariant: a
 * crash can lose the replacement, but can never restore the evicted key after
 * its data block has been reused.
 */
static int kestrelfs_cache_evict_lru(unsigned long *reclaimed_slot)
{
	struct kestrelfs_cache_disk_index_entry old_entry;
	struct kestrelfs_cache_disk_index_entry empty = { 0 };
	struct kestrelfs_cache_index_entry *victim;
	int ret;

	if (list_empty(&kestrelfs_cache_entries))
		return -ENOSPC;
	victim = list_first_entry(&kestrelfs_cache_entries,
				  struct kestrelfs_cache_index_entry, list);

	kestrelfs_cache_entry_to_disk(victim, &old_entry);
	ret = kestrelfs_cache_journal_replace(
		KESTRELFS_CACHE_JOURNAL_EVICT, victim->slot, &old_entry, &empty);
	if (ret)
		return ret;
	ret = rhashtable_remove_fast(&kestrelfs_cache_index, &victim->node,
				     kestrelfs_cache_index_params);
	if (ret) {
		pr_err("kestrelfs: failed to remove evicted cache key: %d\n",
		       ret);
		return ret;
	}
	victim->hashed = false;
	*reclaimed_slot = victim->slot;
	__clear_bit(victim->slot, kestrelfs_cache_slots);
	list_del(&victim->list);
	kfree(victim);
	kestrelfs_cache_evictions++;
	return 0;
}

static int kestrelfs_cache_fill_block(u64 inode_id, u64 file_offset,
				      const u8 *data, size_t length)
{
	struct kestrelfs_cache_disk_index_entry empty = { 0 };
	struct kestrelfs_cache_disk_index_entry disk = { 0 };
	struct kestrelfs_cache_index_entry *entry;
	unsigned long slot;
	void *block;
	int ret;

	entry = kestrelfs_cache_find(inode_id, file_offset);
	if (entry) {
		list_move_tail(&entry->list, &kestrelfs_cache_entries);
		return 0;
	}

	entry = kzalloc(sizeof(*entry), GFP_KERNEL);
	block = kzalloc(KESTRELFS_CACHE_BLOCK_SIZE, GFP_KERNEL);
	if (!entry || !block) {
		ret = -ENOMEM;
		goto out;
	}
	slot = find_first_zero_bit(kestrelfs_cache_slots,
				   kestrelfs_cache_slot_count);
	if (slot >= kestrelfs_cache_slot_count) {
		ret = kestrelfs_cache_evict_lru(&slot);
		if (ret)
			goto out;
	}
	memcpy(block, data, length);
	entry->key.inode_id = inode_id;
	entry->key.file_offset = file_offset;
	entry->lba = kestrelfs_cache_slot_lba(slot);
	entry->data_checksum = kestrelfs_cache_checksum(
		block, KESTRELFS_CACHE_BLOCK_SIZE);
	if (!++kestrelfs_cache_entry_generation)
		kestrelfs_cache_entry_generation = 1;
	entry->generation = kestrelfs_cache_entry_generation;
	entry->slot = slot;

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

	kestrelfs_cache_entry_to_disk(entry, &disk);
	ret = kestrelfs_cache_journal_begin(KESTRELFS_CACHE_JOURNAL_FILL,
					    slot, &empty, &disk);
	if (ret)
		goto remove_hash;
	ret = kestrelfs_cache_rw_block(block, entry->lba, true);
	if (ret)
		goto remove_hash;
	ret = kestrelfs_cache_rw_index_entry(slot, &disk, true);
	if (ret)
		goto remove_hash;
	ret = kestrelfs_cache_journal_clear();
	if (ret)
		goto remove_hash;

	__set_bit(slot, kestrelfs_cache_slots);
	list_add_tail(&entry->list, &kestrelfs_cache_entries);
	entry = NULL;
	goto out;
remove_hash:
	rhashtable_remove_fast(&kestrelfs_cache_index, &entry->node,
			       kestrelfs_cache_index_params);
	entry->hashed = false;
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
	u32 direct_checksums[KESTRELFS_CACHE_DIRECT_MAX_BYTES /
			     KESTRELFS_CACHE_BLOCK_SIZE];
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
			unsigned int bad_block = 0;
			int io_ret;

			direct_checksums[0] = entry->data_checksum;
			while (direct_bytes < KESTRELFS_CACHE_DIRECT_MAX_BYTES &&
			       block_offset + direct_bytes +
				       KESTRELFS_CACHE_BLOCK_SIZE <= end) {
				struct kestrelfs_cache_index_entry *next;

				next = kestrelfs_cache_find(inode_id,
						 block_offset + direct_bytes);
				if (!next || next->lba != entry->lba +
						(direct_bytes >> SECTOR_SHIFT))
					break;
				direct_checksums[direct_bytes /
						 KESTRELFS_CACHE_BLOCK_SIZE] =
					next->data_checksum;
				direct_bytes += KESTRELFS_CACHE_BLOCK_SIZE;
			}

			io_ret = kestrelfs_cache_read_user_blocks(
				entry->lba, buf + copied, direct_bytes,
				direct_checksums, &bad_block);
			if (!io_ret) {
				u64 touch_offset;

				for (touch_offset = block_offset;
				     touch_offset < block_offset + direct_bytes;
				     touch_offset += KESTRELFS_CACHE_BLOCK_SIZE) {
					entry = kestrelfs_cache_find(inode_id,
							 touch_offset);
					if (entry)
						list_move_tail(&entry->list,
							       &kestrelfs_cache_entries);
				}
				kestrelfs_cache_direct_hit_blocks +=
					direct_bytes / KESTRELFS_CACHE_BLOCK_SIZE;
				copied += direct_bytes;
				offset += direct_bytes;
				block_offset += direct_bytes;
				continue;
			}
			if (io_ret == -EILSEQ) {
				entry = kestrelfs_cache_find(
					inode_id, block_offset +
						  bad_block *
						  KESTRELFS_CACHE_BLOCK_SIZE);
				if (entry)
					kestrelfs_cache_discard_corrupt(entry);
				ret = -ENODATA;
				goto out;
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
		if (kestrelfs_cache_checksum(block,
					     KESTRELFS_CACHE_BLOCK_SIZE) !=
		    entry->data_checksum) {
			kestrelfs_cache_discard_corrupt(entry);
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
		list_move_tail(&entry->list, &kestrelfs_cache_entries);
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
	struct kestrelfs_cache_disk_index_entry old_entry;
	struct kestrelfs_cache_disk_index_entry empty = { 0 };
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
		kestrelfs_cache_entry_to_disk(entry, &old_entry);
		ret = kestrelfs_cache_journal_replace(
			KESTRELFS_CACHE_JOURNAL_INVALIDATE, entry->slot,
			&old_entry, &empty);
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
	kestrelfs_cache_evictions = 0;
	kestrelfs_cache_checksum_failures = 0;
	kestrelfs_cache_journal_recoveries = 0;
	kestrelfs_cache_journal_sequence = 0;
	kestrelfs_cache_journal_active = false;
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
	ret = kestrelfs_cache_recover_journal();
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
	pr_info("kestrelfs: cache stats direct_blocks=%lu copied_blocks=%lu direct_fallbacks=%lu evictions=%lu checksum_failures=%lu journal_recoveries=%lu\n",
		kestrelfs_cache_direct_hit_blocks,
		kestrelfs_cache_copy_hit_blocks,
		kestrelfs_cache_direct_fallbacks,
		kestrelfs_cache_evictions,
		kestrelfs_cache_checksum_failures,
		kestrelfs_cache_journal_recoveries);
	kestrelfs_cache_free_index();
	if (kestrelfs_cache_file) {
		bdev_fput(kestrelfs_cache_file);
		kestrelfs_cache_file = NULL;
		kestrelfs_cache_bdev = NULL;
	}
	mutex_unlock(&kestrelfs_cache_lock);
}
