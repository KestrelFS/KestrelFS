// SPDX-License-Identifier: Apache-2.0
/* Offline inspector and explicitly confirmed metadata wipe for cache format v4. */
#define _FILE_OFFSET_BITS 64

#include <endian.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <linux/fs.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <unistd.h>

#define CACHE_MAGIC UINT64_C(0x454843414353464b)
#define CACHE_FORMAT_VERSION 4U
#define JOURNAL_MAGIC UINT64_C(0x4e52554f4a53464b)
#define JOURNAL_VERSION 1U
#define JOURNAL_PREPARED 1U
#define CACHE_SECTOR_SIZE 512U
#define CACHE_BLOCK_SIZE 4096U
#define CACHE_METADATA_BYTES (UINT64_C(2) * 1024U * 1024U)
#define CACHE_JOURNAL_OFFSET CACHE_BLOCK_SIZE
#define CACHE_INDEX_OFFSET (2U * CACHE_BLOCK_SIZE)
#define CACHE_INDEX_CAPACITY ((CACHE_METADATA_BYTES - CACHE_INDEX_OFFSET) / 32U)
#define CACHE_DATA_START_LBA (CACHE_METADATA_BYTES / CACHE_SECTOR_SIZE)
#define EVICT_BATCH_MAGIC UINT32_C(0x48435442)
#define EVICT_BATCH_MAX 64U
#define WIPE_CONFIRM_ENV "KESTRELFS_CACHE_WIPE_CONFIRM"
#define WIPE_CONFIRM_FLAG "--yes-really-wipe"

enum journal_operation {
	JOURNAL_FILL = 1,
	JOURNAL_INVALIDATE = 2,
	JOURNAL_EVICT = 3,
	JOURNAL_RETIRE_CORRUPT = 4,
};

struct disk_index_entry {
	uint64_t inode_id;
	uint64_t file_offset;
	uint64_t generation;
	uint32_t data_checksum;
	uint32_t entry_checksum;
};

struct disk_superblock {
	uint64_t magic;
	uint32_t version;
	uint32_t header_size;
	uint32_t logical_block_size;
	uint32_t physical_block_size;
	uint32_t cache_block_size;
	uint32_t index_entry_size;
	uint64_t usable_sectors;
	uint64_t index_start_lba;
	uint64_t index_capacity;
	uint64_t data_start_lba;
	uint64_t format_generation;
	uint8_t namespace_id[32];
	uint64_t journal_start_lba;
	uint32_t journal_size;
	uint32_t feature_flags;
	uint8_t reserved[CACHE_BLOCK_SIZE - 124];
	uint32_t super_checksum;
};

struct disk_journal {
	uint64_t magic;
	uint32_t version;
	uint32_t state;
	uint64_t sequence;
	uint32_t operation;
	uint32_t slot;
	struct disk_index_entry old_entry;
	struct disk_index_entry new_entry;
	uint8_t reserved[CACHE_BLOCK_SIZE - 100];
	uint32_t checksum;
};

struct disk_evict_batch {
	uint32_t magic;
	uint32_t count;
	uint32_t slots[EVICT_BATCH_MAX];
};

struct cache_key {
	uint64_t inode_id;
	uint64_t file_offset;
};

struct index_summary {
	uint64_t entries;
	uint64_t empty;
	uint64_t crc_errors;
	uint64_t layout_errors;
	uint64_t duplicate_keys;
};

_Static_assert(sizeof(struct disk_index_entry) == 32, "index layout");
_Static_assert(offsetof(struct disk_index_entry, entry_checksum) == 28,
	       "index checksum offset");
_Static_assert(sizeof(struct disk_superblock) == CACHE_BLOCK_SIZE, "super layout");
_Static_assert(offsetof(struct disk_superblock, namespace_id) == 72,
	       "namespace offset");
_Static_assert(offsetof(struct disk_superblock, journal_start_lba) == 104,
	       "journal offset field");
_Static_assert(offsetof(struct disk_superblock, super_checksum) == 4092,
	       "super checksum offset");
_Static_assert(sizeof(struct disk_journal) == CACHE_BLOCK_SIZE, "journal layout");
_Static_assert(offsetof(struct disk_journal, old_entry) == 32,
	       "journal old entry offset");
_Static_assert(offsetof(struct disk_journal, new_entry) == 64,
	       "journal new entry offset");
_Static_assert(offsetof(struct disk_journal, checksum) == 4092,
	       "journal checksum offset");
_Static_assert(sizeof(struct disk_evict_batch) <=
	       sizeof(((struct disk_journal *)0)->reserved),
	       "eviction batch journal extension");

static uint32_t crc32_ieee(const void *buffer, size_t length)
{
	const uint8_t *bytes = buffer;
	uint32_t crc = UINT32_MAX;
	size_t i;

	for (i = 0; i < length; i++) {
		unsigned int bit;

		crc ^= bytes[i];
		for (bit = 0; bit < 8; bit++)
			crc = (crc >> 1) ^
			      (UINT32_C(0xedb88320) & (0U - (crc & 1U)));
	}
	return crc ^ UINT32_MAX;
}

static int all_zero(const void *buffer, size_t length)
{
	const uint8_t *bytes = buffer;
	size_t i;

	for (i = 0; i < length; i++) {
		if (bytes[i])
			return 0;
	}
	return 1;
}

static int is_power_of_two(uint32_t value)
{
	return value && !(value & (value - 1));
}

static int read_full_at(int fd, void *buffer, size_t length, off_t offset)
{
	uint8_t *cursor = buffer;
	size_t done = 0;

	while (done < length) {
		ssize_t ret = pread(fd, cursor + done, length - done,
				    offset + (off_t)done);

		if (ret < 0) {
			if (errno == EINTR)
				continue;
			perror("pread");
			return -1;
		}
		if (!ret) {
			fprintf(stderr, "unexpected EOF at offset %jd\n",
				(intmax_t)(offset + (off_t)done));
			return -1;
		}
		done += (size_t)ret;
	}
	return 0;
}

static int write_full_at(int fd, const void *buffer, size_t length,
			 off_t offset)
{
	const uint8_t *cursor = buffer;
	size_t done = 0;

	while (done < length) {
		ssize_t ret = pwrite(fd, cursor + done, length - done,
				     offset + (off_t)done);

		if (ret < 0) {
			if (errno == EINTR)
				continue;
			perror("pwrite");
			return -1;
		}
		if (!ret) {
			fprintf(stderr, "zero-length pwrite at offset %jd\n",
				(intmax_t)(offset + (off_t)done));
			return -1;
		}
		done += (size_t)ret;
	}
	return 0;
}

static int device_size(int fd, const struct stat *st, uint64_t *size)
{
	if (S_ISBLK(st->st_mode)) {
		unsigned long long bytes;

		if (ioctl(fd, BLKGETSIZE64, &bytes)) {
			perror("BLKGETSIZE64");
			return -1;
		}
		*size = bytes;
		return 0;
	}
	if (S_ISREG(st->st_mode)) {
		*size = (uint64_t)st->st_size;
		return 0;
	}
	fprintf(stderr, "target is neither a block device nor a regular image\n");
	return -1;
}

static void print_hex(const uint8_t *bytes, size_t length)
{
	size_t i;

	for (i = 0; i < length; i++)
		printf("%02x", bytes[i]);
}

static int index_entry_empty(const struct disk_index_entry *entry)
{
	return all_zero(entry, sizeof(*entry));
}

static int index_entry_crc_ok(const struct disk_index_entry *entry)
{
	return le32toh(entry->entry_checksum) ==
	       crc32_ieee(entry, offsetof(struct disk_index_entry, entry_checksum));
}

static int index_entry_layout_ok(const struct disk_index_entry *entry)
{
	return le64toh(entry->inode_id) &&
	       !(le64toh(entry->file_offset) % CACHE_BLOCK_SIZE) &&
	       le64toh(entry->generation);
}

static int cache_key_compare(const void *left, const void *right)
{
	const struct cache_key *a = left;
	const struct cache_key *b = right;

	if (a->inode_id != b->inode_id)
		return a->inode_id < b->inode_id ? -1 : 1;
	if (a->file_offset != b->file_offset)
		return a->file_offset < b->file_offset ? -1 : 1;
	return 0;
}

static uint64_t super_slot_count(const struct disk_superblock *super)
{
	uint64_t usable_bytes = le64toh(super->usable_sectors) * CACHE_SECTOR_SIZE;
	uint64_t data_blocks;
	uint64_t capacity = le64toh(super->index_capacity);

	if (usable_bytes < CACHE_METADATA_BYTES)
		return 0;
	data_blocks = (usable_bytes - CACHE_METADATA_BYTES) / CACHE_BLOCK_SIZE;
	return data_blocks < capacity ? data_blocks : capacity;
}

static int super_layout_ok(const struct disk_superblock *super,
			   uint64_t target_bytes)
{
	uint32_t logical = le32toh(super->logical_block_size);
	uint32_t physical = le32toh(super->physical_block_size);
	uint64_t usable_sectors = le64toh(super->usable_sectors);
	uint64_t usable_bytes;

	if (usable_sectors > UINT64_MAX / CACHE_SECTOR_SIZE)
		return 0;
	usable_bytes = usable_sectors * CACHE_SECTOR_SIZE;
	return le64toh(super->magic) == CACHE_MAGIC &&
	       le32toh(super->version) == CACHE_FORMAT_VERSION &&
	       le32toh(super->header_size) == CACHE_BLOCK_SIZE &&
	       is_power_of_two(logical) && logical >= CACHE_SECTOR_SIZE &&
	       !(CACHE_BLOCK_SIZE % logical) && is_power_of_two(physical) &&
	       physical >= logical && !(CACHE_BLOCK_SIZE % physical) &&
	       le32toh(super->cache_block_size) == CACHE_BLOCK_SIZE &&
	       le32toh(super->index_entry_size) ==
		       sizeof(struct disk_index_entry) &&
	       usable_bytes >= CACHE_METADATA_BYTES + CACHE_BLOCK_SIZE &&
	       usable_bytes <= target_bytes && !(usable_bytes % logical) &&
	       le64toh(super->index_start_lba) == CACHE_INDEX_OFFSET / CACHE_SECTOR_SIZE &&
	       le64toh(super->index_capacity) == CACHE_INDEX_CAPACITY &&
	       le64toh(super->data_start_lba) == CACHE_DATA_START_LBA &&
	       le64toh(super->format_generation) &&
	       le64toh(super->journal_start_lba) ==
		       CACHE_JOURNAL_OFFSET / CACHE_SECTOR_SIZE &&
	       le32toh(super->journal_size) == CACHE_BLOCK_SIZE &&
	       !le32toh(super->feature_flags) &&
	       all_zero(super->reserved, sizeof(super->reserved));
}

static const char *journal_operation_name(uint32_t operation)
{
	switch (operation) {
	case JOURNAL_FILL:
		return "fill";
	case JOURNAL_INVALIDATE:
		return "invalidate";
	case JOURNAL_EVICT:
		return "evict";
	case JOURNAL_RETIRE_CORRUPT:
		return "retire-corrupt";
	default:
		return "invalid";
	}
}

static int journal_layout_ok(const struct disk_journal *journal,
			     uint64_t slot_count)
{
	const struct disk_evict_batch *batch =
		(const void *)journal->reserved;
	uint32_t operation = le32toh(journal->operation);
	uint32_t batch_count = 1;
	uint32_t i;
	uint32_t j;
	int fill = operation == JOURNAL_FILL;
	int removal = operation == JOURNAL_INVALIDATE ||
		      operation == JOURNAL_EVICT ||
		      operation == JOURNAL_RETIRE_CORRUPT;
	int batch_ok = all_zero(journal->reserved, sizeof(journal->reserved));

	if (!batch_ok && operation == JOURNAL_EVICT &&
	    le32toh(batch->magic) == EVICT_BATCH_MAGIC) {
		batch_count = le32toh(batch->count);
		batch_ok = batch_count >= 2 && batch_count <= EVICT_BATCH_MAX &&
			   le32toh(batch->slots[0]) == le32toh(journal->slot);
		for (i = 0; batch_ok && i < batch_count; i++) {
			uint32_t slot = le32toh(batch->slots[i]);

			if (slot >= slot_count)
				batch_ok = 0;
			for (j = 0; batch_ok && j < i; j++) {
				if (slot == le32toh(batch->slots[j]))
					batch_ok = 0;
			}
		}
		for (; batch_ok && i < EVICT_BATCH_MAX; i++) {
			if (batch->slots[i])
				batch_ok = 0;
		}
		if (batch_ok)
			batch_ok = all_zero(journal->reserved + sizeof(*batch),
					    sizeof(journal->reserved) -
					    sizeof(*batch));
	}

	return le64toh(journal->magic) == JOURNAL_MAGIC &&
	       le32toh(journal->version) == JOURNAL_VERSION &&
	       le32toh(journal->state) == JOURNAL_PREPARED &&
	       le64toh(journal->sequence) &&
	       le32toh(journal->slot) < slot_count &&
	       (fill || removal) &&
	       batch_ok &&
	       ((fill && index_entry_empty(&journal->old_entry) &&
		 index_entry_layout_ok(&journal->new_entry) &&
		 index_entry_crc_ok(&journal->new_entry)) ||
		(removal && index_entry_layout_ok(&journal->old_entry) &&
		 index_entry_crc_ok(&journal->old_entry) &&
		 index_entry_empty(&journal->new_entry)));
}

static int metadata_after_super_is_zero(int fd)
{
	uint8_t buffer[CACHE_BLOCK_SIZE];
	off_t offset;

	for (offset = CACHE_BLOCK_SIZE; offset < (off_t)CACHE_METADATA_BYTES;
	     offset += CACHE_BLOCK_SIZE) {
		if (read_full_at(fd, buffer, sizeof(buffer), offset))
			return -1;
		if (!all_zero(buffer, sizeof(buffer)))
			return 0;
	}
	return 1;
}

static int inspect_index(int fd, uint64_t slot_count,
			 struct index_summary *summary)
{
	struct cache_key *keys;
	struct disk_index_entry block[CACHE_BLOCK_SIZE /
					      sizeof(struct disk_index_entry)];
	uint64_t key_count = 0;
	uint64_t slot;

	keys = calloc(CACHE_INDEX_CAPACITY, sizeof(*keys));
	if (!keys) {
		perror("calloc");
		return -1;
	}
	for (slot = 0; slot < CACHE_INDEX_CAPACITY; slot += CACHE_BLOCK_SIZE /
						      sizeof(struct disk_index_entry)) {
		uint64_t in_block;

		if (read_full_at(fd, block, sizeof(block),
				 CACHE_INDEX_OFFSET + (off_t)slot *
				 sizeof(struct disk_index_entry))) {
			free(keys);
			return -1;
		}
		for (in_block = 0;
		     in_block < CACHE_BLOCK_SIZE / sizeof(*block) &&
		     slot + in_block < CACHE_INDEX_CAPACITY; in_block++) {
			const struct disk_index_entry *entry = &block[in_block];
			uint64_t absolute_slot = slot + in_block;

			if (index_entry_empty(entry)) {
				summary->empty++;
				continue;
			}
			summary->entries++;
			if (!index_entry_crc_ok(entry)) {
				summary->crc_errors++;
				continue;
			}
			if (!index_entry_layout_ok(entry) ||
			    absolute_slot >= slot_count) {
				summary->layout_errors++;
				continue;
			}
			keys[key_count].inode_id = le64toh(entry->inode_id);
			keys[key_count].file_offset = le64toh(entry->file_offset);
			key_count++;
		}
	}
	qsort(keys, key_count, sizeof(*keys), cache_key_compare);
	for (slot = 1; slot < key_count; slot++) {
		if (!cache_key_compare(&keys[slot - 1], &keys[slot]))
			summary->duplicate_keys++;
	}
	free(keys);
	return 0;
}

static int inspect_device(const char *path)
{
	struct disk_superblock super;
	struct disk_journal journal;
	struct index_summary index = { 0 };
	struct stat st;
	uint64_t target_bytes;
	uint64_t slot_count;
	uint32_t stored_super_crc;
	uint32_t actual_super_crc;
	uint32_t stored_journal_crc;
	uint32_t actual_journal_crc;
	int super_ok;
	int journal_ok = 1;
	int journal_prepared = 0;
	int metadata_zero;
	int fd;

	/* O_EXCL makes block-device inspection fail while the module owns it. */
	fd = open(path, O_RDONLY | O_CLOEXEC | O_EXCL);
	if (fd < 0) {
		perror("open");
		return 1;
	}
	if (fstat(fd, &st)) {
		perror("fstat");
		close(fd);
		return 1;
	}
	if (device_size(fd, &st, &target_bytes)) {
		close(fd);
		return 1;
	}
	if (target_bytes < CACHE_METADATA_BYTES) {
		fprintf(stderr, "target is smaller than the v4 metadata area\n");
		close(fd);
		return 1;
	}
	if (read_full_at(fd, &super, sizeof(super), 0)) {
		close(fd);
		return 1;
	}

	printf("device=%s\n", path);
	printf("device_type=%s\n", S_ISBLK(st.st_mode) ? "block" : "image");
	printf("device_bytes=%" PRIu64 "\n", target_bytes);
	if (all_zero(&super, sizeof(super))) {
		metadata_zero = metadata_after_super_is_zero(fd);
		close(fd);
		if (metadata_zero < 0)
			return 1;
		printf("super_state=zero\n");
		printf("metadata_state=%s\n",
		       metadata_zero ? "zero" : "nonzero");
		printf("overall_status=%s\n",
		       metadata_zero ? "unformatted" : "invalid");
		return metadata_zero ? 0 : 2;
	}

	stored_super_crc = le32toh(super.super_checksum);
	actual_super_crc = crc32_ieee(&super,
				   offsetof(struct disk_superblock,
					    super_checksum));
	super_ok = stored_super_crc == actual_super_crc &&
		   super_layout_ok(&super, target_bytes);
	printf("super_magic=0x%016" PRIx64 "\n", le64toh(super.magic));
	printf("super_version=%" PRIu32 "\n", le32toh(super.version));
	printf("super_crc=%s\n",
	       stored_super_crc == actual_super_crc ? "ok" : "bad");
	printf("super_layout=%s\n",
	       super_layout_ok(&super, target_bytes) ? "ok" : "bad");
	printf("format_generation=%" PRIu64 "\n",
	       le64toh(super.format_generation));
	printf("namespace=");
	print_hex(super.namespace_id, sizeof(super.namespace_id));
	putchar('\n');
	printf("usable_sectors=%" PRIu64 "\n", le64toh(super.usable_sectors));
	printf("index_capacity=%" PRIu64 "\n", le64toh(super.index_capacity));
	printf("data_start_lba=%" PRIu64 "\n", le64toh(super.data_start_lba));

	slot_count = super_ok ? super_slot_count(&super) : 0;
	if (read_full_at(fd, &journal, sizeof(journal), CACHE_JOURNAL_OFFSET)) {
		close(fd);
		return 1;
	}
	if (all_zero(&journal, sizeof(journal))) {
		printf("journal_state=clean\n");
		printf("journal_crc=not-present\n");
	} else {
		journal_prepared = 1;
		stored_journal_crc = le32toh(journal.checksum);
		actual_journal_crc = crc32_ieee(&journal,
					 offsetof(struct disk_journal, checksum));
		journal_ok = stored_journal_crc == actual_journal_crc &&
			     super_ok && journal_layout_ok(&journal, slot_count);
		printf("journal_state=%s\n",
		       le32toh(journal.state) == JOURNAL_PREPARED ?
		       "prepared" : "invalid");
		printf("journal_crc=%s\n",
		       stored_journal_crc == actual_journal_crc ? "ok" : "bad");
		printf("journal_layout=%s\n",
		       super_ok && journal_layout_ok(&journal, slot_count) ?
		       "ok" : "bad");
		printf("journal_sequence=%" PRIu64 "\n",
		       le64toh(journal.sequence));
		printf("journal_operation=%s\n",
		       journal_operation_name(le32toh(journal.operation)));
		printf("journal_slot=%" PRIu32 "\n", le32toh(journal.slot));
		if (le32toh(((struct disk_evict_batch *)(void *)journal.reserved)->magic) ==
		    EVICT_BATCH_MAGIC)
			printf("journal_batch_count=%" PRIu32 "\n",
			       le32toh(((struct disk_evict_batch *)(void *)
				journal.reserved)->count));
	}

	if (inspect_index(fd, slot_count, &index)) {
		close(fd);
		return 1;
	}
	close(fd);
	printf("cache_slots=%" PRIu64 "\n", slot_count);
	printf("index_entries=%" PRIu64 "\n", index.entries);
	printf("index_empty=%" PRIu64 "\n", index.empty);
	printf("index_crc_errors=%" PRIu64 "\n", index.crc_errors);
	printf("index_layout_errors=%" PRIu64 "\n", index.layout_errors);
	printf("index_duplicate_keys=%" PRIu64 "\n", index.duplicate_keys);
	printf("index_status=%s\n",
	       !index.crc_errors && !index.layout_errors &&
	       !index.duplicate_keys ? "ok" : "invalid");
	printf("data_crc=not-scanned\n");
	if (!super_ok || !journal_ok || index.crc_errors ||
	    index.layout_errors || index.duplicate_keys) {
		printf("overall_status=invalid\n");
		return 2;
	}
	if (journal_prepared) {
		printf("overall_status=recovery-required\n");
		return 3;
	}
	printf("overall_status=ok\n");
	return 0;
}

static int wipe_device(const char *path, const char *flag)
{
	struct disk_superblock super;
	struct stat st;
	const char *confirmation = getenv(WIPE_CONFIRM_ENV);
	uint64_t target_bytes;
	uint8_t *zero;
	off_t offset;
	int fd;

	if (strcmp(flag, WIPE_CONFIRM_FLAG) || !confirmation ||
	    strcmp(confirmation, path)) {
		fprintf(stderr,
			"wipe refused: require %s and %s exactly equal to DEVICE\n",
			WIPE_CONFIRM_FLAG, WIPE_CONFIRM_ENV);
		return 2;
	}
	fd = open(path, O_RDWR | O_CLOEXEC | O_EXCL);
	if (fd < 0) {
		perror("open exclusive block device");
		return 1;
	}
	if (fstat(fd, &st)) {
		perror("fstat");
		close(fd);
		return 1;
	}
	if (!S_ISBLK(st.st_mode)) {
		fprintf(stderr, "wipe refused: DEVICE must be a block device\n");
		close(fd);
		return 2;
	}
	if (device_size(fd, &st, &target_bytes)) {
		close(fd);
		return 1;
	}
	if (target_bytes < CACHE_METADATA_BYTES) {
		fprintf(stderr, "wipe refused: block device is smaller than metadata area\n");
		close(fd);
		return 2;
	}
	if (read_full_at(fd, &super, sizeof(super), 0)) {
		close(fd);
		return 1;
	}
	zero = calloc(1, CACHE_BLOCK_SIZE);
	if (!zero) {
		perror("calloc");
		close(fd);
		return 1;
	}
	printf("wipe_target=%s\n", path);
	printf("wipe_scope=metadata-only-%" PRIu64 "-bytes\n", CACHE_METADATA_BYTES);
	printf("previous_namespace=");
	print_hex(super.namespace_id, sizeof(super.namespace_id));
	putchar('\n');
	for (offset = 0; offset < (off_t)CACHE_METADATA_BYTES;
	     offset += CACHE_BLOCK_SIZE) {
		if (write_full_at(fd, zero, CACHE_BLOCK_SIZE, offset)) {
			free(zero);
			close(fd);
			return 1;
		}
	}
	free(zero);
	if (fsync(fd)) {
		perror("fsync");
		close(fd);
		return 1;
	}
	close(fd);
	printf("wipe_status=ok\n");
	return 0;
}

static void usage(const char *program)
{
	fprintf(stderr,
		"usage: %s inspect DEVICE\n"
		"       %s wipe DEVICE %s\n"
		"wipe also requires %s to exactly equal DEVICE\n",
		program, program, WIPE_CONFIRM_FLAG, WIPE_CONFIRM_ENV);
}

int main(int argc, char **argv)
{
	if (argc == 3 && !strcmp(argv[1], "inspect"))
		return inspect_device(argv[2]);
	if (argc == 4 && !strcmp(argv[1], "wipe"))
		return wipe_device(argv[2], argv[3]);
	usage(argv[0]);
	return 2;
}
