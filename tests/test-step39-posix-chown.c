#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <unistd.h>

static void fail(const char *operation)
{
	perror(operation);
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

	fd = open(argv[1], O_CREAT | O_RDWR | O_TRUNC, 0640);
	if (fd < 0)
		fail("open");
	if (write(fd, "orphan-owner\n", 13) != 13)
		fail("write");
	if (unlink(argv[1]) < 0)
		fail("unlink");
	if (fchown(fd, 3456, 4567) < 0)
		fail("fchown");
	if (fstat(fd, &st) < 0)
		fail("fstat");
	if (st.st_uid != 3456 || st.st_gid != 4567) {
		fprintf(stderr, "unexpected orphan owner %u:%u\n",
			(unsigned int)st.st_uid, (unsigned int)st.st_gid);
		return EXIT_FAILURE;
	}
	if (close(fd) < 0)
		fail("close");
	return EXIT_SUCCESS;
}
