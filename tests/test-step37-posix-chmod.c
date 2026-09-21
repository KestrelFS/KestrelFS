// SPDX-License-Identifier: Apache-2.0
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <unistd.h>

static void die(const char *what)
{
	perror(what);
	exit(EXIT_FAILURE);
}

int main(int argc, char **argv)
{
	struct stat st;
	int fd;

	if (argc != 2) {
		fprintf(stderr, "usage: %s PATH\n", argv[0]);
		return EXIT_FAILURE;
	}

	fd = open(argv[1], O_CREAT | O_EXCL | O_RDWR, 0640);
	if (fd < 0)
		die("open orphan candidate");
	if (unlink(argv[1]) != 0)
		die("unlink orphan candidate");
	if (fchmod(fd, 0601) != 0)
		die("fchmod orphan");
	if (fstat(fd, &st) != 0)
		die("fstat orphan");
	if (!S_ISREG(st.st_mode) || (st.st_mode & 07777) != 0601 || st.st_nlink != 0) {
		fprintf(stderr, "unexpected orphan stat: mode=%#o nlink=%lu\n",
			(unsigned int)st.st_mode, (unsigned long)st.st_nlink);
		return EXIT_FAILURE;
	}
	if (close(fd) != 0)
		die("close orphan");

	puts("STEP37_ORPHAN_CHMOD_PASS");
	return EXIT_SUCCESS;
}
