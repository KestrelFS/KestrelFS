// SPDX-License-Identifier: Apache-2.0
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

struct linux_dirent64 {
	uint64_t d_ino;
	int64_t d_off;
	unsigned short d_reclen;
	unsigned char d_type;
	char d_name[];
};

static void fail(const char *what)
{
	perror(what);
	exit(EXIT_FAILURE);
}

static void path_join(char *out, size_t size, const char *dir,
		      const char *name)
{
	if (snprintf(out, size, "%s/%s", dir, name) >= (int)size) {
		errno = ENAMETOOLONG;
		fail("path");
	}
}

static void expect_mknod_error(const char *dir, const char *name,
			       mode_t mode, dev_t dev, int expected)
{
	char path[4096];

	path_join(path, sizeof(path), dir, name);
	errno = 0;
	if (mknod(path, mode, dev) == 0) {
		fprintf(stderr, "mknod unexpectedly accepted %s\n", name);
		exit(EXIT_FAILURE);
	}
	if (errno != expected) {
		fprintf(stderr, "%s errno=%d expected=%d\n", name, errno,
			expected);
		exit(EXIT_FAILURE);
	}
}

static void create_nodes(const char *dir)
{
	char path[4096];
	struct stat st;

	path_join(path, sizeof(path), dir, "dtype-whiteout");
	if (mknod(path, S_IFCHR | 0600, makedev(0, 0)) < 0)
		fail("mknod whiteout");
	if (lstat(path, &st) < 0)
		fail("lstat whiteout");
	if (!S_ISCHR(st.st_mode) || major(st.st_rdev) != 0 ||
	    minor(st.st_rdev) != 0) {
		fprintf(stderr, "whiteout stat mismatch mode=%#o dev=%u:%u\n",
			st.st_mode, major(st.st_rdev), minor(st.st_rdev));
		exit(EXIT_FAILURE);
	}

	expect_mknod_error(dir, "bad-block", S_IFBLK | 0600,
			    makedev(0, 0), EOPNOTSUPP);
	expect_mknod_error(dir, "bad-char-device", S_IFCHR | 0600,
			    makedev(1, 3), EOPNOTSUPP);
	expect_mknod_error(dir, "bad-fifo", S_IFIFO | 0600, 0, EOPNOTSUPP);
}

static unsigned char expected_type(const char *name)
{
	if (strcmp(name, "dtype-file") == 0)
		return DT_REG;
	if (strcmp(name, "dtype-dir") == 0)
		return DT_DIR;
	if (strcmp(name, "dtype-link") == 0)
		return DT_LNK;
	if (strcmp(name, "dtype-whiteout") == 0)
		return DT_CHR;
	return DT_UNKNOWN;
}

static void check_types(const char *dir)
{
	unsigned int seen = 0;
	char buffer[4096];
	int fd;

	fd = open(dir, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
	if (fd < 0)
		fail("open directory");
	for (;;) {
		long bytes = syscall(SYS_getdents64, fd, buffer, sizeof(buffer));
		long cursor = 0;

		if (bytes < 0)
			fail("getdents64");
		if (bytes == 0)
			break;
		while (cursor < bytes) {
			struct linux_dirent64 *entry =
				(void *)(buffer + cursor);
			unsigned char expected = expected_type(entry->d_name);

			if (entry->d_reclen == 0 || cursor + entry->d_reclen > bytes) {
				fprintf(stderr, "invalid getdents record\n");
				exit(EXIT_FAILURE);
			}
			if (expected != DT_UNKNOWN) {
				if (entry->d_type != expected) {
					fprintf(stderr,
						"%s d_type=%u expected=%u\n",
						entry->d_name, entry->d_type,
						expected);
					exit(EXIT_FAILURE);
				}
				if (strcmp(entry->d_name, "dtype-file") == 0)
					seen |= 1U;
				else if (strcmp(entry->d_name, "dtype-dir") == 0)
					seen |= 2U;
				else if (strcmp(entry->d_name, "dtype-link") == 0)
					seen |= 4U;
				else
					seen |= 8U;
			}
			cursor += entry->d_reclen;
		}
	}
	close(fd);
	if (seen != 15U) {
		fprintf(stderr, "missing typed entries mask=%#x\n", seen);
		exit(EXIT_FAILURE);
	}
}

static void check_unprivileged(const char *dir)
{
	char path[4096];

	if (setgroups(0, NULL) < 0 || setgid(65534) < 0 || setuid(65534) < 0)
		fail("drop privileges");
	path_join(path, sizeof(path), dir, "unpriv-whiteout");
	errno = 0;
	if (mknod(path, S_IFCHR | 0600, makedev(0, 0)) == 0 || errno != EPERM) {
		fprintf(stderr, "unprivileged mknod errno=%d expected=%d\n", errno,
			EPERM);
		exit(EXIT_FAILURE);
	}
}

int main(int argc, char **argv)
{
	if (argc != 3) {
		fprintf(stderr, "usage: %s create|check|unpriv DIR\n", argv[0]);
		return EXIT_FAILURE;
	}
	if (strcmp(argv[1], "create") == 0)
		create_nodes(argv[2]);
	else if (strcmp(argv[1], "check") == 0)
		check_types(argv[2]);
	else if (strcmp(argv[1], "unpriv") == 0)
		check_unprivileged(argv[2]);
	else
		return EXIT_FAILURE;
	return EXIT_SUCCESS;
}
