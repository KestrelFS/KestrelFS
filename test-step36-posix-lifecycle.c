// SPDX-License-Identifier: Apache-2.0
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static const char original[] = "STEP36_ORIGINAL_0123456789abcdef\n";
static const char updated[] = "STEP36_UPDATED__0123456789abcdef\n";

static void die(const char *what)
{
	perror(what);
	exit(EXIT_FAILURE);
}

static void wait_for_path(const char *path)
{
	for (int attempt = 0; attempt < 200; attempt++) {
		if (access(path, F_OK) == 0)
			return;
		usleep(25000);
	}
	fprintf(stderr, "timed out waiting for %s\n", path);
	exit(EXIT_FAILURE);
}

static void assert_contents(int fd, const char *expected)
{
	char buffer[sizeof(original)] = { 0 };
	ssize_t length = (ssize_t)strlen(expected);

	if (pread(fd, buffer, (size_t)length, 0) != length)
		die("pread");
	if (memcmp(buffer, expected, (size_t)length) != 0) {
		fprintf(stderr, "open fd returned unexpected bytes\n");
		exit(EXIT_FAILURE);
	}
}

int main(int argc, char **argv)
{
	int fd;
	int marker;

	if (argc != 8) {
		fprintf(stderr,
			"usage: %s PATH READY FINAL_UNLINKED UPDATE_DONE READ_AGAIN READ_DONE CLOSE\n",
			argv[0]);
		return EXIT_FAILURE;
	}
	if (strlen(original) != strlen(updated)) {
		fprintf(stderr, "test strings must have equal length\n");
		return EXIT_FAILURE;
	}

	fd = open(argv[1], O_RDWR);
	if (fd < 0)
		die("open");
	if (unlink(argv[1]) != 0)
		die("unlink alias");
	assert_contents(fd, original);

	marker = open(argv[2], O_WRONLY | O_CREAT | O_EXCL, 0600);
	if (marker < 0)
		die("ready marker");
	close(marker);
	wait_for_path(argv[3]);
	if (pwrite(fd, updated, strlen(updated), 0) != (ssize_t)strlen(updated))
		die("pwrite after final unlink");
	assert_contents(fd, updated);
	marker = open(argv[4], O_WRONLY | O_CREAT | O_EXCL, 0600);
	if (marker < 0)
		die("update-done marker");
	close(marker);
	wait_for_path(argv[5]);
	assert_contents(fd, updated);
	marker = open(argv[6], O_WRONLY | O_CREAT | O_EXCL, 0600);
	if (marker < 0)
		die("read-done marker");
	close(marker);
	wait_for_path(argv[7]);
	if (close(fd) != 0)
		die("close");
	return EXIT_SUCCESS;
}
