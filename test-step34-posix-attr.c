// SPDX-License-Identifier: Apache-2.0
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

int main(int argc, char **argv)
{
	char *end;
	unsigned long mode;
	int fd;

	if (argc != 4) {
		fprintf(stderr, "usage: %s file|dir PATH OCTAL_MODE\n", argv[0]);
		return 64;
	}
	errno = 0;
	mode = strtoul(argv[3], &end, 8);
	if (errno || *end != '\0' || mode > 07777UL) {
		fprintf(stderr, "invalid mode: %s\n", argv[3]);
		return 64;
	}
	umask(0);

	if (strcmp(argv[1], "file") == 0) {
		fd = open(argv[2], O_CREAT | O_EXCL | O_WRONLY, (mode_t)mode);
		if (fd >= 0 && close(fd) != 0)
			fd = -1;
		if (fd >= 0)
			return 0;
	} else if (strcmp(argv[1], "dir") == 0) {
		if (mkdir(argv[2], (mode_t)mode) == 0)
			return 0;
	} else {
		fprintf(stderr, "unknown kind: %s\n", argv[1]);
		return 64;
	}

	perror(argv[2]);
	return 1;
}
