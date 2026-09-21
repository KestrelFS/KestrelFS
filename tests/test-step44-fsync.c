// SPDX-License-Identifier: GPL-2.0
/* Guest helper exercising fsync, fdatasync, syncfs and daemon-offline errors. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static const char payload[] = "step44 fsync fdatasync syncfs persistent payload\n";

int main(int argc, char **argv)
{
	int fd;
	char received[sizeof(payload)] = { 0 };
	int result = -1;

	if (argc != 3) {
		fprintf(stderr, "usage: %s write|write-global|verify|offline PATH\n", argv[0]);
		return 2;
	}
	if (!strcmp(argv[1], "write")) {
		fd = open(argv[2], O_CREAT | O_EXCL | O_RDWR, 0644);
		if (fd >= 0 && write(fd, payload, sizeof(payload) - 1) == sizeof(payload) - 1 &&
		    fsync(fd) == 0 && fdatasync(fd) == 0 && syncfs(fd) == 0) {
			puts("STEP44_FSYNC_FDATASYNC_SYNCFS_PASS");
			result = 0;
		}
	} else if (!strcmp(argv[1], "write-global")) {
		fd = open(argv[2], O_CREAT | O_EXCL | O_RDWR, 0644);
		if (fd >= 0 && write(fd, payload, sizeof(payload) - 1) == sizeof(payload) - 1 &&
		    syncfs(fd) == 0) {
			puts("STEP44_SYNCFS_ONLY_PASS");
			result = 0;
		}
	} else if (!strcmp(argv[1], "verify")) {
		fd = open(argv[2], O_RDONLY);
		if (fd >= 0 && read(fd, received, sizeof(payload)) == sizeof(payload) - 1 &&
		    !memcmp(received, payload, sizeof(payload))) {
			puts("STEP44_RESTART_READBACK_PASS");
			result = 0;
		}
	} else if (!strcmp(argv[1], "offline")) {
		fd = open(argv[2], O_RDONLY);
		if (fd >= 0) {
			errno = 0;
			if (fsync(fd) == -1 && (errno == ENOTCONN || errno == ETIMEDOUT)) {
				puts("STEP44_OFFLINE_FAIL_CLOSED_PASS");
				result = 0;
			}
		}
	} else {
		return 2;
	}
	if (fd < 0 || result)
		perror(argv[1]);
	if (fd >= 0 && close(fd)) {
		perror("close");
		return 1;
	}
	return result ? 1 : 0;
}
