// SPDX-License-Identifier: Apache-2.0
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define MAX_BYTES (64U * 1024U * 1024U)

static unsigned char pattern(size_t offset)
{
	return (unsigned char)((offset * 37U + 0x5bU) & 0xffU);
}

static uint64_t elapsed_ns(const struct timespec *start,
			   const struct timespec *end)
{
	int64_t seconds = end->tv_sec - start->tv_sec;
	int64_t nanoseconds = end->tv_nsec - start->tv_nsec;

	return (uint64_t)(seconds * 1000000000LL + nanoseconds);
}

static size_t parse_count(const char *text)
{
	char *end;
	unsigned long value;

	errno = 0;
	value = strtoul(text, &end, 10);
	if (errno || *end || !value || value > MAX_BYTES)
		return 0;
	return (size_t)value;
}

static int timed_write(const char *path, size_t bytes)
{
	struct timespec start;
	struct timespec after_write;
	struct timespec after_fsync;
	unsigned char *buffer;
	uint64_t write_ns;
	uint64_t fsync_ns;
	double mib;
	size_t i;
	ssize_t written;
	int fd;
	int ret = 1;

	buffer = malloc(bytes);
	if (!buffer) {
		perror("malloc");
		return 1;
	}
	for (i = 0; i < bytes; i++)
		buffer[i] = pattern(i);
	fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0644);
	if (fd < 0) {
		perror("open write");
		goto out_free;
	}
	if (clock_gettime(CLOCK_MONOTONIC, &start)) {
		perror("clock_gettime write start");
		goto out_close;
	}
	written = pwrite(fd, buffer, bytes, 0);
	if (written != (ssize_t)bytes) {
		if (written < 0)
			perror("pwrite");
		else
			fprintf(stderr, "short pwrite: %zd\n", written);
		goto out_close;
	}
	if (clock_gettime(CLOCK_MONOTONIC, &after_write)) {
		perror("clock_gettime write end");
		goto out_close;
	}
	if (fsync(fd)) {
		perror("fsync");
		goto out_close;
	}
	if (clock_gettime(CLOCK_MONOTONIC, &after_fsync)) {
		perror("clock_gettime fsync end");
		goto out_close;
	}
	write_ns = elapsed_ns(&start, &after_write);
	fsync_ns = elapsed_ns(&after_write, &after_fsync);
	mib = (double)bytes / (1024.0 * 1024.0);
	printf("STEP52_PERF_WRITE_BEHIND bytes=%zu write_ns=%llu write_mib_s=%.2f fsync_ns=%llu\n",
	       bytes, (unsigned long long)write_ns,
	       write_ns ? mib * 1000000000.0 / (double)write_ns : 0.0,
	       (unsigned long long)fsync_ns);
	ret = 0;
out_close:
	if (close(fd)) {
		perror("close write");
		ret = 1;
	}
out_free:
	free(buffer);
	return ret;
}

static int read_full(int fd, unsigned char *buffer, size_t bytes)
{
	size_t done = 0;
	ssize_t got;

	while (done < bytes) {
		got = pread(fd, buffer + done, bytes - done, (off_t)done);
		if (got < 0 && errno == EINTR)
			continue;
		if (got <= 0) {
			if (got < 0)
				perror("pread");
			else
				fprintf(stderr, "unexpected EOF at %zu\n", done);
			return 1;
		}
		done += (size_t)got;
	}
	return 0;
}

static int verify(const unsigned char *buffer, size_t bytes)
{
	size_t i;

	for (i = 0; i < bytes; i++) {
		if (buffer[i] != pattern(i)) {
			fprintf(stderr, "data mismatch at %zu\n", i);
			return 1;
		}
	}
	return 0;
}

static int timed_reads_fd(int fd, size_t bytes, unsigned int loops,
			  const char *label)
{
	struct timespec start;
	struct timespec end;
	unsigned char *buffer;
	uint64_t duration_ns;
	double total_mib;
	unsigned int loop;
	int ret = 1;

	buffer = malloc(bytes);
	if (!buffer) {
		perror("malloc");
		return 1;
	}
	if (clock_gettime(CLOCK_MONOTONIC, &start)) {
		perror("clock_gettime read start");
		goto out_free;
	}
	for (loop = 0; loop < loops; loop++) {
		if (read_full(fd, buffer, bytes) || verify(buffer, bytes))
			goto out_free;
	}
	if (clock_gettime(CLOCK_MONOTONIC, &end)) {
		perror("clock_gettime read end");
		goto out_free;
	}
	duration_ns = elapsed_ns(&start, &end);
	total_mib = (double)bytes * loops / (1024.0 * 1024.0);
	printf("STEP52_PERF_%s bytes=%zu loops=%u elapsed_ns=%llu mib_s=%.2f\n",
	       label, bytes, loops, (unsigned long long)duration_ns,
	       duration_ns ? total_mib * 1000000000.0 / (double)duration_ns : 0.0);
	ret = 0;
out_free:
	free(buffer);
	return ret;
}

static int timed_reads_path(const char *path, size_t bytes, unsigned int loops,
			    const char *label)
{
	int fd;
	int ret;

	fd = open(path, O_RDONLY);
	if (fd < 0) {
		perror("open read");
		return 1;
	}
	ret = timed_reads_fd(fd, bytes, loops, label);
	if (close(fd)) {
		perror("close read");
		ret = 1;
	}
	return ret;
}

int main(int argc, char **argv)
{
	size_t bytes;
	unsigned long loops;

	if (argc == 4 && !strcmp(argv[1], "write")) {
		bytes = parse_count(argv[3]);
		return bytes ? timed_write(argv[2], bytes) : 64;
	}
	if (argc == 6 && !strcmp(argv[1], "read-loop")) {
		bytes = parse_count(argv[3]);
		loops = strtoul(argv[4], NULL, 10);
		if (!bytes || !loops || loops > 10000)
			return 64;
		return timed_reads_path(argv[2], bytes, (unsigned int)loops, argv[5]);
	}
	if (argc == 6 && !strcmp(argv[1], "read-loop-fd")) {
		bytes = parse_count(argv[3]);
		loops = strtoul(argv[4], NULL, 10);
		if (!bytes || !loops || loops > 10000)
			return 64;
		return timed_reads_fd(atoi(argv[2]), bytes, (unsigned int)loops,
				      argv[5]);
	}
	fprintf(stderr,
		"usage: %s write PATH BYTES | read-loop PATH BYTES LOOPS LABEL | read-loop-fd FD BYTES LOOPS LABEL\n",
		argv[0]);
	return 64;
}
