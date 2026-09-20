// SPDX-License-Identifier: Apache-2.0
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#include "kestrelfs_ipc.h"

#define DATA_LEN 4096

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

static int write_generation(int fd, unsigned int generation)
{
	unsigned char buffer[DATA_LEN];

	fill(buffer, generation);
	if (pwrite(fd, buffer, sizeof(buffer), 0) != (ssize_t)sizeof(buffer)) {
		perror("pwrite");
		return 1;
	}
	return 0;
}

static int verify_generation(int fd, unsigned int generation)
{
	unsigned char buffer[DATA_LEN];
	size_t i;

	if (pread(fd, buffer, sizeof(buffer), 0) != (ssize_t)sizeof(buffer)) {
		perror("pread");
		return 1;
	}
	for (i = 0; i < sizeof(buffer); i++) {
		if (buffer[i] != pattern(generation, i)) {
			fprintf(stderr, "generation %u mismatch at %zu\n",
				generation, i);
			return 1;
		}
	}
	return 0;
}

static int prepare(const char *path)
{
	int fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0644);
	int ret;

	if (fd < 0) {
		perror("open prepare");
		return 1;
	}
	ret = write_generation(fd, 1);
	if (!ret && fsync(fd)) {
		perror("fsync prepare");
		ret = 1;
	}
	if (close(fd)) {
		perror("close prepare");
		ret = 1;
	}
	return ret;
}

static int async_write(int fd, unsigned int generation)
{
	struct timespec start;
	struct timespec end;
	long long elapsed_ms;

	if (clock_gettime(CLOCK_MONOTONIC, &start)) {
		perror("clock_gettime start");
		return 1;
	}
	if (write_generation(fd, generation))
		return 1;
	if (clock_gettime(CLOCK_MONOTONIC, &end)) {
		perror("clock_gettime end");
		return 1;
	}
	elapsed_ms = (end.tv_sec - start.tv_sec) * 1000LL +
		(end.tv_nsec - start.tv_nsec) / 1000000LL;
	printf("STEP51_ASYNC_WRITE_LATENCY_MS=%lld\n", elapsed_ms);
	/* The IPC timeout is two seconds. Staying below one second proves this
	 * cached full-page write did not wait for WRITE_DATA while the daemon was
	 * offline, without making timing a performance benchmark.
	 */
	if (elapsed_ms >= 1000) {
		fprintf(stderr, "delayed write unexpectedly blocked for %lld ms\n",
			elapsed_ms);
		return 1;
	}
	return 0;
}

static int expect_fsync_failure(int fd)
{
	errno = 0;
	if (!fsync(fd)) {
		fprintf(stderr, "offline fsync unexpectedly succeeded\n");
		return 1;
	}
	if (errno != ENOTCONN && errno != EIO && errno != ETIMEDOUT) {
		fprintf(stderr, "offline fsync errno=%d\n", errno);
		return 1;
	}
	return 0;
}

static int consume_prior_error(int fd, int mount_wide)
{
	int ret;

	errno = 0;
	ret = mount_wide ? syncfs(fd) : fsync(fd);
	if (!ret)
		return 0;
	if (errno == ENOTCONN || errno == EIO || errno == ETIMEDOUT)
		return 0;
	fprintf(stderr, "%s recovery errno=%d\n",
		mount_wide ? "syncfs" : "fsync", errno);
	return 1;
}

static int mmap_coherence(int fd, unsigned int generation)
{
	unsigned char expected[DATA_LEN];
	unsigned char *mapping;
	unsigned char resident;
	int ctl;
	int i;
	int ret = 1;

	fill(expected, generation);
	mapping = mmap(NULL, DATA_LEN, PROT_READ | PROT_WRITE, MAP_SHARED,
		       fd, 0);
	if (mapping == MAP_FAILED) {
		perror("mmap");
		return 1;
	}
	memcpy(mapping, expected, sizeof(expected));
	ctl = open("/dev/kestrel_ctl", O_RDWR);
	if (ctl < 0) {
		perror("open control");
		goto out_unmap;
	}
	if (ioctl(ctl, KESTRELFS_IOC_INVALIDATE_CACHE_ALL)) {
		perror("coherence ioctl");
		goto out_close;
	}
	for (i = 0; i < 200; i++) {
		if (mincore(mapping, DATA_LEN, &resident)) {
			perror("mincore");
			goto out_close;
		}
		if (!(resident & 1))
			break;
		usleep(10000);
	}
	if (i == 200) {
		fprintf(stderr, "coherence worker did not retire dirty mapping\n");
		goto out_close;
	}
	if (memcmp(mapping, expected, sizeof(expected))) {
		fprintf(stderr, "dirty mapping lost across coherence retirement\n");
		goto out_close;
	}
	if (msync(mapping, DATA_LEN, MS_SYNC)) {
		perror("msync coherence");
		goto out_close;
	}
	ret = 0;
out_close:
	if (close(ctl)) {
		perror("close control");
		ret = 1;
	}
out_unmap:
	if (munmap(mapping, DATA_LEN)) {
		perror("munmap coherence");
		ret = 1;
	}
	return ret;
}

int main(int argc, char **argv)
{
	int fd;
	unsigned int generation;

	if (argc < 3)
		return 64;
	if (!strcmp(argv[1], "prepare") && argc == 3)
		return prepare(argv[2]);

	fd = atoi(argv[2]);
	if (!strcmp(argv[1], "preload-fd") && argc == 4) {
		generation = (unsigned int)strtoul(argv[3], NULL, 10);
		return verify_generation(fd, generation);
	}
	if (!strcmp(argv[1], "async-write-fd") && argc == 4) {
		generation = (unsigned int)strtoul(argv[3], NULL, 10);
		return async_write(fd, generation);
	}
	if (!strcmp(argv[1], "verify-fd") && argc == 4) {
		generation = (unsigned int)strtoul(argv[3], NULL, 10);
		return verify_generation(fd, generation);
	}
	if (!strcmp(argv[1], "mmap-coherence-fd") && argc == 4) {
		generation = (unsigned int)strtoul(argv[3], NULL, 10);
		return mmap_coherence(fd, generation);
	}
	if (!strcmp(argv[1], "fsync-fail-fd") && argc == 3)
		return expect_fsync_failure(fd);
	if (!strcmp(argv[1], "fsync-recover-fd") && argc == 3)
		return consume_prior_error(fd, 0);
	if (!strcmp(argv[1], "syncfs-recover-fd") && argc == 3)
		return consume_prior_error(fd, 1);
	if (!strcmp(argv[1], "fsync-fd") && argc == 3) {
		if (fsync(fd)) {
			perror("fsync");
			return 1;
		}
		return 0;
	}
	if (!strcmp(argv[1], "syncfs-fd") && argc == 3) {
		if (syncfs(fd)) {
			perror("syncfs");
			return 1;
		}
		return 0;
	}
	return 64;
}
