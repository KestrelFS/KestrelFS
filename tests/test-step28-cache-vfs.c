// SPDX-License-Identifier: GPL-2.0
/* readv/preadv verifier for the Phase 4 Step 28 CACHE-VFS path. */
#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <unistd.h>

#define PAGE_BYTES 4096U
#define GUARD_BYTES 64U
#define SENTINEL 0xa5U

struct test_segment {
	unsigned char *allocation;
	unsigned char *base;
	size_t length;
};

static unsigned char expected_byte(uint64_t offset)
{
	return (unsigned char)((offset * UINT64_C(1315423911) +
				 offset / 127 + UINT64_C(0x5a)) & UINT64_C(0xff));
}

static uint64_t parse_u64(const char *text, const char *name)
{
	char *end = NULL;
	unsigned long long value;

	errno = 0;
	value = strtoull(text, &end, 10);
	if (errno || !end || *end) {
		fprintf(stderr, "invalid %s: %s\n", name, text);
		exit(2);
	}
	return value;
}

static int prepare_file(const char *path, size_t length)
{
	unsigned char *buffer;
	size_t done = 0;
	int fd;

	if (posix_memalign((void **)&buffer, PAGE_BYTES, length ? length : 1)) {
		fprintf(stderr, "posix_memalign failed\n");
		return 1;
	}
	for (done = 0; done < length; done++)
		buffer[done] = expected_byte(done);
	fd = open(path, O_CREAT | O_TRUNC | O_WRONLY, 0644);
	if (fd < 0) {
		perror("open prepare");
		free(buffer);
		return 1;
	}
	done = 0;
	while (done < length) {
		ssize_t written = write(fd, buffer + done, length - done);

		if (written <= 0) {
			perror("write prepare");
			close(fd);
			free(buffer);
			return 1;
		}
		done += (size_t)written;
	}
	if (close(fd)) {
		perror("close prepare");
		free(buffer);
		return 1;
	}
	free(buffer);
	return 0;
}

static void free_segments(struct test_segment *segments, size_t nr_segments)
{
	size_t i;

	for (i = 0; i < nr_segments; i++)
		free(segments[i].allocation);
}

static int verify_preadv_case(int fd, uint64_t file_size, uint64_t file_offset,
			      const size_t *lengths, const size_t *shifts,
			      size_t nr_segments)
{
	struct test_segment segments[8] = { 0 };
	struct iovec iov[8];
	uint64_t expected_length;
	size_t requested = 0;
	size_t logical = 0;
	ssize_t got;
	size_t i;

	if (!nr_segments || nr_segments > 8)
		return 2;
	for (i = 0; i < nr_segments; i++) {
		size_t allocation_size;

		if (lengths[i] > SIZE_MAX - 2 * PAGE_BYTES ||
		    shifts[i] >= PAGE_BYTES)
			goto invalid;
		allocation_size = lengths[i] + 2 * PAGE_BYTES;
		if (posix_memalign((void **)&segments[i].allocation, PAGE_BYTES,
				   allocation_size)) {
			fprintf(stderr, "posix_memalign failed\n");
			goto fail;
		}
		memset(segments[i].allocation, SENTINEL, allocation_size);
		segments[i].base = segments[i].allocation + PAGE_BYTES + shifts[i];
		segments[i].length = lengths[i];
		iov[i].iov_base = segments[i].base;
		iov[i].iov_len = lengths[i];
		if (requested > SIZE_MAX - lengths[i])
			goto invalid;
		requested += lengths[i];
	}

	expected_length = file_offset < file_size ? file_size - file_offset : 0;
	if (expected_length > requested)
		expected_length = requested;
	got = preadv(fd, iov, (int)nr_segments, (off_t)file_offset);
	if (got < 0) {
		perror("preadv");
		goto fail;
	}
	if ((uint64_t)got != expected_length) {
		fprintf(stderr, "short preadv: got=%zd expected=%" PRIu64 "\n",
			got, expected_length);
		goto fail;
	}

	for (i = 0; i < nr_segments; i++) {
		size_t j;

		for (j = 0; j < GUARD_BYTES; j++) {
			if (segments[i].base[-(ssize_t)GUARD_BYTES + (ssize_t)j] !=
			    SENTINEL ||
			    segments[i].base[segments[i].length + j] != SENTINEL) {
				fprintf(stderr, "iov guard overwritten at segment=%zu\n", i);
				goto fail;
			}
		}
		for (j = 0; j < segments[i].length; j++, logical++) {
			unsigned char expected = logical < (size_t)got ?
				expected_byte(file_offset + logical) : SENTINEL;

			if (segments[i].base[j] != expected) {
				fprintf(stderr,
					"iov data mismatch segment=%zu byte=%zu logical=%zu\n",
					i, j, logical);
				goto fail;
			}
		}
	}
	free_segments(segments, nr_segments);
	return 0;

invalid:
	fprintf(stderr, "invalid test vector\n");
fail:
	free_segments(segments, nr_segments);
	return 1;
}

static int warm_file(const char *path)
{
	const size_t lengths[] = { PAGE_BYTES, 2 * PAGE_BYTES, PAGE_BYTES + 137 };
	const size_t shifts[] = { 0, 0, 0 };
	struct stat st;
	int fd;
	int ret;

	fd = open(path, O_RDONLY);
	if (fd < 0) {
		perror("open warm");
		return 1;
	}
	if (fstat(fd, &st)) {
		perror("fstat warm");
		close(fd);
		return 1;
	}
	ret = verify_preadv_case(fd, st.st_size, 0, lengths, shifts, 3);
	close(fd);
	if (!ret)
		puts("STEP28_WARM_PASS");
	return ret;
}

static int verify_suite(const char *path)
{
	const size_t aligned_lengths[] = { PAGE_BYTES, 2 * PAGE_BYTES };
	const size_t aligned_shifts[] = { 0, 0 };
	const size_t unaligned_lengths[] = { 2047, 5000, 3000 };
	const size_t unaligned_shifts[] = { 3, 5, 7 };
	const size_t eof_lengths[] = { 511, PAGE_BYTES, PAGE_BYTES };
	const size_t eof_shifts[] = { 11, 13, 17 };
	struct stat st;
	int fd;
	int ret;

	fd = open(path, O_RDONLY);
	if (fd < 0) {
		perror("open verify");
		return 1;
	}
	if (fstat(fd, &st)) {
		perror("fstat verify");
		close(fd);
		return 1;
	}
	ret = verify_preadv_case(fd, st.st_size, 0, aligned_lengths,
				aligned_shifts, 2);
	if (ret)
		goto out;
	puts("STEP28_IOVEC_PASS");
	ret = verify_preadv_case(fd, st.st_size, 1, unaligned_lengths,
				unaligned_shifts, 3);
	if (ret)
		goto out;
	puts("STEP28_UNALIGNED_PASS");
	ret = verify_preadv_case(fd, st.st_size, st.st_size - 2000,
				eof_lengths, eof_shifts, 3);
	if (ret)
		goto out;
	puts("STEP28_EOF_PASS");
out:
	close(fd);
	return ret;
}

int main(int argc, char **argv)
{
	if (argc == 4 && !strcmp(argv[1], "prepare")) {
		uint64_t length = parse_u64(argv[3], "length");

		if (length > SIZE_MAX)
			return 2;
		return prepare_file(argv[2], (size_t)length);
	}
	if (argc == 3 && !strcmp(argv[1], "warm"))
		return warm_file(argv[2]);
	if (argc == 3 && !strcmp(argv[1], "verify"))
		return verify_suite(argv[2]);
	fprintf(stderr,
		"usage: %s prepare PATH LENGTH\n"
		"       %s warm PATH\n"
		"       %s verify PATH\n",
		argv[0], argv[0], argv[0]);
	return 2;
}
