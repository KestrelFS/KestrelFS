// SPDX-License-Identifier: GPL-2.0
/* Step 46: file-backed mmap reads, COW, rewrite/truncate, and coherence. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#include "kestrelfs_ipc.h"

#define DATA_LEN (8192 + 73)
#define MAP_LEN 12288

static void require(int condition, const char *message)
{
	if (!condition) {
		perror(message);
		exit(1);
	}
}

static void write_exact(int fd, const void *data, size_t length)
{
	require(pwrite(fd, data, length, 0) == (ssize_t)length, "pwrite");
}

int main(int argc, char **argv)
{
	unsigned char old[DATA_LEN];
	unsigned char new_data[DATA_LEN];
	unsigned char check[DATA_LEN];
	unsigned char *private_read, *private_cow, *shared_read, *shared_write;
	unsigned char resident;
	pid_t child;
	int fd, ctl, status;
	int i;

	if (argc != 2)
		return 2;
	for (i = 0; i < DATA_LEN; i++) {
		old[i] = (unsigned char)(i * 17 + 3);
		new_data[i] = (unsigned char)(i * 23 + 11);
	}
	fd = open(argv[1], O_CREAT | O_RDWR | O_TRUNC, 0644);
	require(fd >= 0, "open file");
	write_exact(fd, old, sizeof(old));
	private_read = mmap(NULL, MAP_LEN, PROT_READ, MAP_PRIVATE, fd, 0);
	require(private_read != MAP_FAILED, "mmap private read");
	shared_read = mmap(NULL, MAP_LEN, PROT_READ, MAP_SHARED, fd, 0);
	require(shared_read != MAP_FAILED, "mmap shared read");
	private_cow = mmap(NULL, MAP_LEN, PROT_READ | PROT_WRITE,
			   MAP_PRIVATE, fd, 0);
	require(private_cow != MAP_FAILED, "mmap private COW");
	require(pread(fd, check, sizeof(check), 0) == sizeof(check), "pread");
	require(memcmp(private_read, check, sizeof(check)) == 0,
		"private mmap/read mismatch");
	require(memcmp(shared_read, check, sizeof(check)) == 0,
		"shared mmap/read mismatch");
	require(private_read[DATA_LEN - 1] == old[DATA_LEN - 1],
		"partial EOF byte");
	require(private_read[DATA_LEN] == 0, "partial EOF zero tail");
	puts("STEP46_MMAP_READ_PASS");

	private_cow[0] ^= 0xff;
	require(private_cow[0] != old[0], "private COW byte");
	require(pread(fd, check, 1, 0) == 1 && check[0] == old[0],
		"private COW changed file");
	puts("STEP46_PRIVATE_COW_PASS");

	shared_write = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
			    MAP_SHARED, fd, 0);
	require(shared_write != MAP_FAILED, "writable shared mmap");
	require(mprotect(shared_read, 4096, PROT_READ | PROT_WRITE) == 0,
		"shared mapping mprotect upgrade");
	puts("STEP46_SHARED_WRITE_AVAILABLE_PASS");

	write_exact(fd, new_data, sizeof(new_data));
	require(memcmp(private_read, new_data, sizeof(new_data)) == 0,
		"private mmap stale after write");
	require(memcmp(shared_read, new_data, sizeof(new_data)) == 0,
		"shared mmap stale after write");
	require(private_cow[0] != new_data[0], "private COW unexpectedly replaced");
	puts("STEP46_REWRITE_INVALIDATE_PASS");

	require(ftruncate(fd, 4096 + 23) == 0, "ftruncate");
	require(memcmp(shared_read, new_data, 4096 + 23) == 0,
		"mapped prefix stale after truncate");
	require(shared_read[4096 + 23] == 0,
		"mapped partial page not zero after truncate");
	child = fork();
	require(child >= 0, "fork");
	if (child == 0) {
		volatile unsigned char byte = shared_read[8192];

		(void)byte;
		_exit(0);
	}
	require(waitpid(child, &status, 0) == child, "waitpid");
	require(WIFSIGNALED(status) && WTERMSIG(status) == SIGBUS,
		"mapping past EOF did not SIGBUS");
	puts("STEP46_TRUNCATE_INVALIDATE_PASS");

	ctl = open("/dev/kestrel_ctl", O_RDWR);
	require(ctl >= 0, "open control device");
	require(mincore(shared_read, 4096, &resident) == 0 &&
		(resident & 1), "mapped page not resident before invalidation");
	require(ioctl(ctl, KESTRELFS_IOC_INVALIDATE_CACHE_ALL) == 0,
		"coherence invalidate ioctl");
	for (i = 0; i < 200; i++) {
		require(mincore(shared_read, 4096, &resident) == 0,
			"mincore after invalidation");
		if (!(resident & 1))
			break;
		usleep(10000);
	}
	require(i < 200, "coherence worker did not retire mapped page");
	require(memcmp(shared_read, new_data, 4096 + 23) == 0,
		"mapped refault after coherence");
	puts("STEP46_COHERENCE_REFAULT_PASS");

	require(close(ctl) == 0, "close control device");
	require(munmap(private_read, MAP_LEN) == 0, "munmap private read");
	require(munmap(private_cow, MAP_LEN) == 0, "munmap private COW");
	require(munmap(shared_read, MAP_LEN) == 0, "munmap shared read");
	require(munmap(shared_write, 4096) == 0, "munmap shared write");
	require(close(fd) == 0, "close file");
	return 0;
}
