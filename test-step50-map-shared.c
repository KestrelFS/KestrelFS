// SPDX-License-Identifier: Apache-2.0
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#define FILE_LEN (8192 + 73)

static unsigned char initial_byte(size_t offset)
{
	return (unsigned char)((offset * 29U + 7U) & 0xffU);
}

static void require(int condition, const char *message)
{
	if (!condition) {
		perror(message);
		exit(1);
	}
}

static void fill_initial(unsigned char *buffer)
{
	size_t i;

	for (i = 0; i < FILE_LEN; i++)
		buffer[i] = initial_byte(i);
}

static int write_shared(const char *path)
{
	unsigned char initial[FILE_LEN];
	unsigned char check[FILE_LEN];
	unsigned char *shared;
	unsigned char *private;
	int fd;

	fill_initial(initial);
	fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0644);
	require(fd >= 0, "open shared file");
	require(write(fd, initial, sizeof(initial)) == (ssize_t)sizeof(initial),
		"write initial");
	shared = mmap(NULL, FILE_LEN, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	require(shared != MAP_FAILED, "mmap shared writable");
	shared[1] = 0xa1;
	shared[4095] = 0xb2;
	shared[4096] = 0xc3;
	shared[FILE_LEN - 1] = 0xd4;
	require(msync(shared, FILE_LEN, MS_SYNC) == 0, "msync shared");
	require(fsync(fd) == 0, "fsync shared");
	require(pread(fd, check, sizeof(check), 0) == (ssize_t)sizeof(check),
		"pread shared");
	require(memcmp(shared, check, sizeof(check)) == 0,
		"shared mapping differs from file");
	private = mmap(NULL, FILE_LEN, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
	require(private != MAP_FAILED, "mmap private");
	private[1] ^= 0xff;
	require(private[1] != shared[1], "private COW did not diverge");
	require(pread(fd, check, sizeof(check), 0) == (ssize_t)sizeof(check),
		"pread private comparison");
	require(check[1] == shared[1], "private COW changed file");
	require(munmap(private, FILE_LEN) == 0, "munmap private");

	/* Exercise the VMA close path without a preceding msync. */
	shared[17] = 0xe5;
	require(mprotect(shared, FILE_LEN, PROT_READ) == 0,
		"mprotect shared read-only before close");
	require(munmap(shared, FILE_LEN) == 0, "munmap shared");
	require(close(fd) == 0, "close shared file");
	return 0;
}

static int verify_shared(const char *path)
{
	unsigned char check[FILE_LEN];
	int fd = open(path, O_RDONLY);

	require(fd >= 0, "open verify file");
	require(read(fd, check, sizeof(check)) == (ssize_t)sizeof(check),
		"read verify file");
	require(check[1] == 0xa1, "shared byte 1");
	require(check[17] == 0xe5, "munmap byte 17");
	require(check[4095] == 0xb2, "shared byte 4095");
	require(check[4096] == 0xc3, "shared byte 4096");
	require(check[FILE_LEN - 1] == 0xd4, "shared tail byte");
	return close(fd) != 0;
}

int main(int argc, char **argv)
{
	if (argc != 3)
		return 64;
	if (!strcmp(argv[1], "write"))
		return write_shared(argv[2]);
	if (!strcmp(argv[1], "verify"))
		return verify_shared(argv[2]);
	return 64;
}
