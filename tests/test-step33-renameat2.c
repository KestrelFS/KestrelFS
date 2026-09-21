// SPDX-License-Identifier: Apache-2.0
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(int argc, char **argv)
{
	unsigned long flags;
	char *end;
	int saved_errno;

	if (argc != 4) {
		fprintf(stderr, "usage: %s OLD NEW FLAGS\n", argv[0]);
		return 64;
	}
	errno = 0;
	flags = strtoul(argv[3], &end, 0);
	if (errno || *end != '\0' || flags > 0xffffffffUL) {
		fprintf(stderr, "invalid flags: %s\n", argv[3]);
		return 64;
	}
	if (syscall(SYS_renameat2, AT_FDCWD, argv[1], AT_FDCWD, argv[2],
		    (unsigned int)flags) == 0)
		return 0;

	saved_errno = errno;
	perror("renameat2");
	return saved_errno > 0 && saved_errno < 64 ? saved_errno : 1;
}
