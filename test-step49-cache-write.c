// SPDX-License-Identifier: GPL-2.0
/* Step 49 writeback, fsync and daemon-failure test helper. */
#define _POSIX_C_SOURCE 200809L
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define DATA_LEN (128 * 1024 + 37)
#define FAIL_LEN 4096

static unsigned char expected_byte(size_t i)
{
	return (unsigned char)((i * 1315423911U + i / 127 + 0x5a) & 0xff);
}

static int verify_fd(int fd)
{
	unsigned char block[4096];
	size_t offset = 0;

	while (offset < DATA_LEN) {
		size_t length = DATA_LEN - offset < sizeof(block) ?
			DATA_LEN - offset : sizeof(block);
		ssize_t got = pread(fd, block, length, offset);

		if (got != (ssize_t)length)
			return 1;
		for (size_t i = 0; i < length; i++) {
			if (block[i] != expected_byte(offset + i)) {
				fprintf(stderr, "mismatch at %zu\n", offset + i);
				return 1;
			}
		}
		offset += length;
	}
	return 0;
}

static int write_and_fsync(const char *path)
{
	unsigned char block[4096];
	size_t offset = 0;
	int fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0644);

	if (fd < 0)
		return 1;
	while (offset < DATA_LEN) {
		size_t length = DATA_LEN - offset < sizeof(block) ?
			DATA_LEN - offset : sizeof(block);

		for (size_t i = 0; i < length; i++)
			block[i] = expected_byte(offset + i);
		if (write(fd, block, length) != (ssize_t)length) {
			perror("write");
			close(fd);
			return 1;
		}
		offset += length;
	}
	if (fsync(fd) || verify_fd(fd)) {
		perror("fsync/verify");
		close(fd);
		return 1;
	}
	return close(fd) != 0;
}

static int failure_writeback(int fd)
{
	unsigned char block[FAIL_LEN];
	ssize_t got;

	memset(block, 'Q', sizeof(block));
	errno = 0;
	got = pwrite(fd, block, sizeof(block), 0);
	if (got != (ssize_t)sizeof(block)) {
		fprintf(stderr, "delayed offline write returned %zd errno=%d\n",
			got, errno);
		return 1;
	}
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

static int verify_failure_file(const char *path)
{
	unsigned char block[FAIL_LEN];
	int fd = open(path, O_RDONLY);

	if (fd < 0)
		return 1;
	if (read(fd, block, sizeof(block)) != sizeof(block)) {
		close(fd);
		return 1;
	}
	for (size_t i = 0; i < sizeof(block); i++) {
		if (block[i] != 'Q') {
			close(fd);
			return 1;
		}
	}
	return close(fd) != 0;
}

int main(int argc, char **argv)
{
	int fd;

	if (argc != 3)
		return 2;
	if (!strcmp(argv[1], "write-fsync"))
		return write_and_fsync(argv[2]);
	if (!strcmp(argv[1], "verify")) {
		fd = open(argv[2], O_RDONLY);
		if (fd < 0)
			return 1;
		int ret = verify_fd(fd);
		close(fd);
		return ret;
	}
	if (!strcmp(argv[1], "verify-fd"))
		return verify_fd(atoi(argv[2]));
	if (!strcmp(argv[1], "fail-write-fd"))
		return failure_writeback(atoi(argv[2]));
	if (!strcmp(argv[1], "fsync-fd"))
		return fsync(atoi(argv[2])) != 0;
	if (!strcmp(argv[1], "verify-failure"))
		return verify_failure_file(argv[2]);
	return 2;
}
