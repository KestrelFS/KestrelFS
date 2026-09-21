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

#define DATA_LEN 4096
#define WATCH_TIMEOUT_MS 20000

static unsigned char pattern(unsigned int generation, size_t offset)
{
	return (unsigned char)((offset * 37U + generation * 83U + 11U) & 0xffU);
}

static void fill(unsigned char *buffer, unsigned int generation)
{
	size_t i;

	for (i = 0; i < DATA_LEN; i++)
		buffer[i] = pattern(generation, i);
}

static int classify(const unsigned char *buffer)
{
	unsigned int generation;
	size_t i;

	for (generation = 1; generation <= 2; generation++) {
		for (i = 0; i < DATA_LEN; i++) {
			if (buffer[i] != pattern(generation, i))
				break;
		}
		if (i == DATA_LEN)
			return (int)generation;
	}
	return 0;
}

static int read_generation(int fd)
{
	unsigned char buffer[DATA_LEN];
	ssize_t got;

	do {
		got = pread(fd, buffer, sizeof(buffer), 0);
	} while (got < 0 && errno == EINTR);
	if (got != (ssize_t)sizeof(buffer)) {
		if (got < 0)
			perror("pread");
		else
			fprintf(stderr, "short read: %zd\n", got);
		return -1;
	}
	return classify(buffer);
}

static int write_generation(const char *path, unsigned int generation)
{
	unsigned char buffer[DATA_LEN];
	int fd;
	int ret = 1;

	fill(buffer, generation);
	fd = open(path, O_CREAT | O_RDWR, 0644);
	if (fd < 0) {
		perror("open write");
		return 1;
	}
	if (pwrite(fd, buffer, sizeof(buffer), 0) != (ssize_t)sizeof(buffer)) {
		perror("pwrite");
		goto out;
	}
	if (ftruncate(fd, sizeof(buffer))) {
		perror("ftruncate");
		goto out;
	}
	if (fsync(fd)) {
		perror("fsync");
		goto out;
	}
	ret = 0;
out:
	if (close(fd)) {
		perror("close write");
		ret = 1;
	}
	return ret;
}

static int touch_marker(const char *path)
{
	int fd = open(path, O_CREAT | O_EXCL | O_WRONLY, 0644);

	if (fd < 0) {
		perror("create marker");
		return 1;
	}
	if (close(fd)) {
		perror("close marker");
		return 1;
	}
	return 0;
}

static int wait_marker(const char *path)
{
	int attempt;

	for (attempt = 0; attempt < 600; attempt++) {
		if (!access(path, F_OK))
			return 0;
		if (errno != ENOENT) {
			perror("access marker");
			return 1;
		}
		usleep(50000);
	}
	fprintf(stderr, "timed out waiting for marker %s\n", path);
	return 1;
}

static long long elapsed_ms(const struct timespec *start,
			    const struct timespec *end)
{
	return (end->tv_sec - start->tv_sec) * 1000LL +
	       (end->tv_nsec - start->tv_nsec) / 1000000LL;
}

static int watch_generation(const char *path, const char *ready,
			    const char *committed)
{
	struct timespec start;
	struct timespec now;
	int fd;
	int generation;
	int ret = 1;

	fd = open(path, O_RDONLY);
	if (fd < 0) {
		perror("open watch");
		return 1;
	}
	generation = read_generation(fd);
	if (generation != 1) {
		fprintf(stderr, "initial generation is %d, expected 1\n", generation);
		goto out;
	}
	if (touch_marker(ready) || wait_marker(committed))
		goto out;
	if (clock_gettime(CLOCK_MONOTONIC, &start)) {
		perror("clock_gettime");
		goto out;
	}
	for (;;) {
		generation = read_generation(fd);
		if (generation == 2)
			break;
		if (generation != 1) {
			fprintf(stderr, "observed corrupt/mixed generation %d\n",
				generation);
			goto out;
		}
		if (clock_gettime(CLOCK_MONOTONIC, &now)) {
			perror("clock_gettime");
			goto out;
		}
		if (elapsed_ms(&start, &now) >= WATCH_TIMEOUT_MS) {
			fprintf(stderr, "generation 2 not visible within %d ms\n",
				WATCH_TIMEOUT_MS);
			goto out;
		}
		usleep(10000);
	}
	if (clock_gettime(CLOCK_MONOTONIC, &now)) {
		perror("clock_gettime");
		goto out;
	}
	printf("STEP52_DIST_VISIBILITY_LATENCY_MS=%lld\n",
	       elapsed_ms(&start, &now));
	ret = 0;
out:
	if (close(fd)) {
		perror("close watch");
		ret = 1;
	}
	return ret;
}

static int verify_fd(int fd, unsigned int expected)
{
	int actual;

	actual = read_generation(fd);
	if (actual != (int)expected) {
		fprintf(stderr, "generation is %d, expected %u\n", actual, expected);
		return 1;
	}
	return 0;
}

static int verify_generation(const char *path, unsigned int expected)
{
	int fd;
	int ret;

	fd = open(path, O_RDONLY);
	if (fd < 0) {
		perror("open verify");
		return 1;
	}
	ret = verify_fd(fd, expected);
	if (close(fd)) {
		perror("close verify");
		ret = 1;
	}
	return ret;
}

int main(int argc, char **argv)
{
	unsigned long generation;

	if (argc == 4 && !strcmp(argv[1], "write")) {
		generation = strtoul(argv[3], NULL, 10);
		if (generation < 1 || generation > 2)
			return 64;
		return write_generation(argv[2], (unsigned int)generation);
	}
	if (argc == 5 && !strcmp(argv[1], "watch"))
		return watch_generation(argv[2], argv[3], argv[4]);
	if (argc == 4 && !strcmp(argv[1], "verify")) {
		generation = strtoul(argv[3], NULL, 10);
		return verify_generation(argv[2], (unsigned int)generation);
	}
	if (argc == 4 && !strcmp(argv[1], "verify-fd")) {
		generation = strtoul(argv[3], NULL, 10);
		return verify_fd(atoi(argv[2]), (unsigned int)generation);
	}
	fprintf(stderr,
		"usage: %s write PATH GEN | watch PATH READY COMMITTED | verify PATH GEN | verify-fd FD GEN\n",
		argv[0]);
	return 64;
}
