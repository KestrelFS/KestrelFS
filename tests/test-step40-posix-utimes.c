#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static void fail(const char *operation)
{
	perror(operation);
	exit(EXIT_FAILURE);
}

int main(int argc, char **argv)
{
	const struct timespec requested[2] = {
		{ .tv_sec = 1577836802, .tv_nsec = 123456789 },
		{ .tv_sec = 1577836803, .tv_nsec = 987654321 },
	};
	struct stat st;
	int fd;

	if (argc != 2) {
		fprintf(stderr, "usage: %s PATH\n", argv[0]);
		return EXIT_FAILURE;
	}

	fd = open(argv[1], O_CREAT | O_RDWR | O_TRUNC, 0640);
	if (fd < 0)
		fail("open");
	if (write(fd, "orphan-times\n", 13) != 13)
		fail("write");
	if (unlink(argv[1]) < 0)
		fail("unlink");
	if (futimens(fd, requested) < 0)
		fail("futimens");
	if (fstat(fd, &st) < 0)
		fail("fstat");
	if (st.st_atim.tv_sec != requested[0].tv_sec ||
	    st.st_mtim.tv_sec != requested[1].tv_sec ||
	    st.st_atim.tv_nsec != 0 || st.st_mtim.tv_nsec != 0) {
		fprintf(stderr,
			"unexpected orphan times %lld.%09ld:%lld.%09ld\n",
			(long long)st.st_atim.tv_sec, st.st_atim.tv_nsec,
			(long long)st.st_mtim.tv_sec, st.st_mtim.tv_nsec);
		return EXIT_FAILURE;
	}
	if (close(fd) < 0)
		fail("close");
	return EXIT_SUCCESS;
}
