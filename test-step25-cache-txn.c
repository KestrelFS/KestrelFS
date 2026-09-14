// SPDX-License-Identifier: Apache-2.0
/* Build synthetic v4 journal states for the Step 25 vng recovery test. */
#define _FILE_OFFSET_BITS 64

#include <endian.h>
#include <errno.h>
#include <fcntl.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define BLOCK_SIZE 4096U
#define JOURNAL_OFFSET 4096U
#define INDEX_OFFSET 8192U
#define JOURNAL_MAGIC UINT64_C(0x4e52554f4a53464b)
#define JOURNAL_VERSION 1U
#define JOURNAL_PREPARED 1U

enum journal_operation {
	JOURNAL_FILL = 1,
	JOURNAL_INVALIDATE = 2,
	JOURNAL_EVICT = 3,
};

struct disk_index_entry {
	uint64_t inode_id;
	uint64_t file_offset;
	uint64_t generation;
	uint32_t data_checksum;
	uint32_t entry_checksum;
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
	uint8_t reserved[BLOCK_SIZE - 100];
	uint32_t checksum;
};

_Static_assert(sizeof(struct disk_index_entry) == 32, "index ABI");
_Static_assert(sizeof(struct disk_journal) == BLOCK_SIZE, "journal ABI");
_Static_assert(offsetof(struct disk_journal, checksum) == BLOCK_SIZE - 4,
	       "journal checksum offset");

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

static void io_full(int fd, void *buffer, size_t length, off_t offset,
		    int write_data)
{
	uint8_t *cursor = buffer;
	size_t done = 0;

	while (done < length) {
		ssize_t ret;

		if (write_data)
			ret = pwrite(fd, cursor + done, length - done, offset + done);
		else
			ret = pread(fd, cursor + done, length - done, offset + done);
		if (ret < 0) {
			perror(write_data ? "pwrite" : "pread");
			exit(EXIT_FAILURE);
		}
		if (!ret) {
			fprintf(stderr, "unexpected EOF\n");
			exit(EXIT_FAILURE);
		}
		done += (size_t)ret;
	}
}

static enum journal_operation parse_operation(const char *name)
{
	if (!strcmp(name, "fill"))
		return JOURNAL_FILL;
	if (!strcmp(name, "invalidate"))
		return JOURNAL_INVALIDATE;
	if (!strcmp(name, "evict"))
		return JOURNAL_EVICT;
	fprintf(stderr, "unknown operation: %s\n", name);
	exit(EXIT_FAILURE);
}

static void make_fill_entry(struct disk_index_entry *entry, uint32_t slot)
{
	uint8_t zero_block[BLOCK_SIZE] = { 0 };

	entry->inode_id = htole64(UINT64_C(0xfeed000000000001) + slot);
	entry->file_offset = htole64(0);
	entry->generation = htole64(UINT64_C(0x100000000) + slot);
	entry->data_checksum = htole32(crc32_ieee(zero_block,
						  sizeof(zero_block)));
	entry->entry_checksum = htole32(crc32_ieee(entry,
						   offsetof(struct disk_index_entry,
							    entry_checksum)));
}

int main(int argc, char **argv)
{
	struct disk_journal journal = { 0 };
	struct disk_index_entry empty = { 0 };
	enum journal_operation operation;
	unsigned long slot;
	off_t index_offset;
	char *end;
	int fd;

	if (argc == 4 && !strcmp(argv[1], "check-zero")) {
		slot = strtoul(argv[3], &end, 10);
		if (*end)
			return EXIT_FAILURE;
		fd = open(argv[2], O_RDONLY | O_CLOEXEC);
		if (fd < 0) {
			perror("open");
			return EXIT_FAILURE;
		}
		io_full(fd, &journal.old_entry, sizeof(journal.old_entry),
			INDEX_OFFSET + (off_t)slot * sizeof(journal.old_entry), 0);
		close(fd);
		return memcmp(&journal.old_entry, &empty, sizeof(empty)) ?
			EXIT_FAILURE : EXIT_SUCCESS;
	}

	if (argc != 6 || strcmp(argv[1], "prepare")) {
		fprintf(stderr,
			"usage: %s prepare DEVICE OP SLOT before|after\n"
			"       %s check-zero DEVICE SLOT\n",
			argv[0], argv[0]);
		return EXIT_FAILURE;
	}
	operation = parse_operation(argv[3]);
	slot = strtoul(argv[4], &end, 10);
	if (*end || slot > UINT32_MAX ||
	    (strcmp(argv[5], "before") && strcmp(argv[5], "after"))) {
		fprintf(stderr, "invalid slot or stage\n");
		return EXIT_FAILURE;
	}
	index_offset = INDEX_OFFSET + (off_t)slot * sizeof(journal.old_entry);
	fd = open(argv[2], O_RDWR | O_CLOEXEC);
	if (fd < 0) {
		perror("open");
		return EXIT_FAILURE;
	}

	journal.magic = htole64(JOURNAL_MAGIC);
	journal.version = htole32(JOURNAL_VERSION);
	journal.state = htole32(JOURNAL_PREPARED);
	journal.sequence = htole64(UINT64_C(0x250000000) + slot);
	journal.operation = htole32(operation);
	journal.slot = htole32((uint32_t)slot);
	if (operation == JOURNAL_FILL) {
		make_fill_entry(&journal.new_entry, (uint32_t)slot);
	} else {
		io_full(fd, &journal.old_entry, sizeof(journal.old_entry),
			index_offset, 0);
	}
	journal.checksum = htole32(crc32_ieee(&journal,
					       offsetof(struct disk_journal,
							checksum)));

	io_full(fd, &journal, sizeof(journal), JOURNAL_OFFSET, 1);
	if (fsync(fd)) {
		perror("fsync journal");
		close(fd);
		return EXIT_FAILURE;
	}
	if (!strcmp(argv[5], "after")) {
		struct disk_index_entry *replacement =
			operation == JOURNAL_FILL ? &journal.new_entry : &empty;

		io_full(fd, replacement, sizeof(*replacement), index_offset, 1);
		if (fsync(fd)) {
			perror("fsync index");
			close(fd);
			return EXIT_FAILURE;
		}
	}
	close(fd);
	return EXIT_SUCCESS;
}
