// SPDX-License-Identifier: GPL-2.0
/* Small aligned-IO verifier/benchmark used only by test-step22-cache-vng.sh. */
#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

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
	return (uint64_t)value;
}

static void fill_pattern(unsigned char *buffer, uint64_t offset, size_t length)
{
	size_t i;

	for (i = 0; i < length; i++)
		buffer[i] = expected_byte(offset + i);
}

static int prepare_file(const char *path, size_t length)
{
	unsigned char *buffer;
	ssize_t written;
	size_t done = 0;
	int fd;

	if (posix_memalign((void **)&buffer, 4096, length ? length : 1)) {
		fprintf(stderr, "posix_memalign failed\n");
		return 1;
	}
	fill_pattern(buffer, 0, length);
	fd = open(path, O_CREAT | O_TRUNC | O_WRONLY, 0644);
	if (fd < 0) {
		perror("open prepare");
		free(buffer);
		return 1;
	}
	while (done < length) {
		written = write(fd, buffer + done, length - done);
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

static uint64_t elapsed_ns(const struct timespec *start,
			   const struct timespec *end)
{
	return (uint64_t)(end->tv_sec - start->tv_sec) * UINT64_C(1000000000) +
	       (uint64_t)(end->tv_nsec - start->tv_nsec);
}

static int verify_reads(const char *path, uint64_t file_offset,
			uint64_t requested, size_t user_shift,
			uint64_t iterations)
{
	struct timespec start, end;
	struct stat st;
	unsigned char *allocation;
	unsigned char *buffer;
	uint64_t expected_length;
	uint64_t duration;
	uint64_t iteration;
	size_t i;
	ssize_t got;
	int fd;

	if (requested > SIZE_MAX - 4096 || user_shift >= 4096 || !iterations) {
		fprintf(stderr, "read parameters out of range\n");
		return 2;
	}
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
	expected_length = file_offset < (uint64_t)st.st_size ?
		(uint64_t)st.st_size - file_offset : 0;
	if (expected_length > requested)
		expected_length = requested;
	if (posix_memalign((void **)&allocation, 4096,
			   (size_t)requested + 4096)) {
		fprintf(stderr, "posix_memalign failed\n");
		close(fd);
		return 1;
	}
	buffer = allocation + user_shift;

	if (clock_gettime(CLOCK_MONOTONIC, &start)) {
		perror("clock_gettime start");
		goto fail;
	}
	for (iteration = 0; iteration < iterations; iteration++) {
		memset(buffer, 0xcc, (size_t)requested);
		got = pread(fd, buffer, (size_t)requested, (off_t)file_offset);
		if (got < 0) {
			perror("pread verify");
			goto fail;
		}
		if ((uint64_t)got != expected_length) {
			fprintf(stderr, "short read: got=%zd expected=%" PRIu64 "\n",
				got, expected_length);
			goto fail;
		}
		for (i = 0; i < (size_t)got; i++) {
			if (buffer[i] != expected_byte(file_offset + i)) {
				fprintf(stderr,
					"data mismatch at offset=%" PRIu64 "\n",
					file_offset + i);
				goto fail;
			}
		}
	}
	if (clock_gettime(CLOCK_MONOTONIC, &end)) {
		perror("clock_gettime end");
		goto fail;
	}
	duration = elapsed_ns(&start, &end);
	printf("elapsed_ns=%" PRIu64 " bytes=%" PRIu64
	       " iterations=%" PRIu64 " mib_per_sec=%.2f\n",
	       duration, expected_length, iterations,
	       duration ? ((double)expected_length * (double)iterations * 1000.0 /
			   (double)duration * 1000000.0 / (1024.0 * 1024.0)) : 0.0);
	free(allocation);
	close(fd);
	return 0;

fail:
	free(allocation);
	close(fd);
	return 1;
}

int main(int argc, char **argv)
{
	uint64_t length;

	if (argc == 4 && !strcmp(argv[1], "prepare")) {
		length = parse_u64(argv[3], "length");
		if (length > SIZE_MAX)
			return 2;
		return prepare_file(argv[2], (size_t)length);
	}
	if (argc == 7 && !strcmp(argv[1], "verify")) {
		uint64_t shift = parse_u64(argv[5], "user shift");

		if (shift > SIZE_MAX)
			return 2;
		return verify_reads(argv[2],
			parse_u64(argv[3], "file offset"),
			parse_u64(argv[4], "requested length"),
			(size_t)shift,
			parse_u64(argv[6], "iterations"));
	}
	fprintf(stderr,
		"usage: %s prepare PATH LENGTH\n"
		"       %s verify PATH FILE_OFFSET LENGTH USER_SHIFT ITERATIONS\n",
		argv[0], argv[0]);
	return 2;
}
