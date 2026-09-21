// SPDX-License-Identifier: GPL-2.0
/* Step 47: independent-process BSD, POSIX and OFD lock contention/cleanup. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/mman.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

enum lock_kind { BSD_FLOCK, POSIX_LOCK, OFD_LOCK };

static void require(int condition, const char *why)
{
	if (!condition) {
		perror(why);
		exit(1);
	}
}

static void send_byte(int fd, char byte)
{
	require(write(fd, &byte, 1) == 1, "pipe write");
}

static void expect_byte(int fd, char expected)
{
	char actual;

	require(read(fd, &actual, 1) == 1 && actual == expected,
		"pipe read");
}

static int change_lock(int fd, enum lock_kind kind, int operation,
		       int blocking)
{
	struct flock range = {
		.l_type = operation ? F_WRLCK : F_UNLCK,
		.l_whence = SEEK_SET,
		.l_start = kind == OFD_LOCK ? 4096 : 0,
		.l_len = 128,
	};

	if (kind == BSD_FLOCK)
		return flock(fd, operation ? LOCK_EX | (blocking ? 0 : LOCK_NB)
					   : LOCK_UN);
	if (kind == OFD_LOCK)
		return fcntl(fd, blocking ? F_OFD_SETLKW : F_OFD_SETLK, &range);
	return fcntl(fd, blocking ? F_SETLKW : F_SETLK, &range);
}

static void expect_conflict(int fd, enum lock_kind kind)
{
	errno = 0;
	require(change_lock(fd, kind, 1, 0) == -1 &&
		(errno == EWOULDBLOCK || errno == EAGAIN || errno == EACCES),
		"nonblocking lock must conflict");
}

static void test_contention(const char *path, int parent_fd,
			    enum lock_kind kind)
{
	int ready[2], granted[2], status;
	struct pollfd waiter;
	pid_t child;

	require(change_lock(parent_fd, kind, 1, 0) == 0, "parent lock");
	require(pipe(ready) == 0 && pipe(granted) == 0, "pipes");
	child = fork();
	require(child >= 0, "fork contender");
	if (child == 0) {
		struct flock query = {
			.l_type = F_WRLCK,
			.l_whence = SEEK_SET,
			.l_start = 0,
			.l_len = 128,
		};
		int fd;

		close(parent_fd);
		close(ready[0]);
		close(granted[0]);
		fd = open(path, O_RDWR);
		require(fd >= 0, "child open");
		if (kind == POSIX_LOCK) {
			require(fcntl(fd, F_GETLK, &query) == 0 &&
				query.l_type == F_WRLCK &&
				query.l_pid == getppid(), "F_GETLK owner");
		}
		expect_conflict(fd, kind);
		send_byte(ready[1], 'R');
		require(change_lock(fd, kind, 1, 1) == 0, "blocking lock");
		send_byte(granted[1], 'G');
		require(change_lock(fd, kind, 0, 0) == 0, "child unlock");
		close(fd);
		_exit(0);
	}
	close(ready[1]);
	close(granted[1]);
	expect_byte(ready[0], 'R');
	waiter.fd = granted[0];
	waiter.events = POLLIN;
	require(poll(&waiter, 1, 100) == 0, "waiter acquired too early");
	require(change_lock(parent_fd, kind, 0, 0) == 0, "parent unlock");
	expect_byte(granted[0], 'G');
	require(waitpid(child, &status, 0) == child && WIFEXITED(status) &&
		WEXITSTATUS(status) == 0, "contender exit");
	close(ready[0]);
	close(granted[0]);
}

static void test_exit_cleanup(const char *path, int parent_fd,
			      enum lock_kind kind)
{
	int ready[2], status;
	pid_t holder;

	require(pipe(ready) == 0, "exit pipe");
	holder = fork();
	require(holder >= 0, "fork holder");
	if (holder == 0) {
		int fd;

		close(parent_fd);
		close(ready[0]);
		fd = open(path, O_RDWR);
		require(fd >= 0, "holder open");
		require(change_lock(fd, kind, 1, 0) == 0, "holder lock");
		send_byte(ready[1], 'R');
		for (;;) pause();
	}
	close(ready[1]);
	expect_byte(ready[0], 'R');
	expect_conflict(parent_fd, kind);
	require(kill(holder, SIGKILL) == 0, "kill holder");
	require(waitpid(holder, &status, 0) == holder && WIFSIGNALED(status) &&
		WTERMSIG(status) == SIGKILL, "holder killed");
	require(change_lock(parent_fd, kind, 1, 0) == 0,
		"lock after killed holder");
	require(change_lock(parent_fd, kind, 0, 0) == 0, "cleanup unlock");
	close(ready[0]);
}

static void test_independent_classes(const char *path, int parent_fd)
{
	int status;
	pid_t child;

	require(change_lock(parent_fd, BSD_FLOCK, 1, 0) == 0,
		"parent flock for class test");
	child = fork();
	require(child >= 0, "fork class test");
	if (child == 0) {
		int fd;

		close(parent_fd);
		fd = open(path, O_RDWR);
		require(fd >= 0, "class child open");
		require(change_lock(fd, POSIX_LOCK, 1, 0) == 0,
			"POSIX must not conflict with flock");
		close(fd);
		_exit(0);
	}
	require(waitpid(child, &status, 0) == child && WIFEXITED(status) &&
		WEXITSTATUS(status) == 0, "class child exit");
	require(change_lock(parent_fd, BSD_FLOCK, 0, 0) == 0,
		"parent flock unlock");
}

static void test_locked_open_unlink_mmap_write(const char *path, int fd)
{
	char *map;
	char bytes[9];

	require(change_lock(fd, BSD_FLOCK, 1, 0) == 0,
		"flock before unlink");
	map = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0);
	require(map != MAP_FAILED && memcmp(map, "lock-test", 9) == 0,
		"mmap while locked");
	require(unlink(path) == 0, "unlink while locked");
	require(pwrite(fd, "stillopen", 9, 0) == 9,
		"write unlinked locked file");
	require(pread(fd, bytes, sizeof(bytes), 0) == sizeof(bytes) &&
		memcmp(bytes, "stillopen", sizeof(bytes)) == 0,
		"read unlinked locked file");
	require(munmap(map, 4096) == 0, "munmap locked orphan");
	require(change_lock(fd, BSD_FLOCK, 0, 0) == 0,
		"unlock unlinked file");
	puts("STEP47_LIFECYCLE_MMAP_WRITE_PASS");
}

int main(int argc, char **argv)
{
	int fd;

	if (argc != 2)
		return 2;
	alarm(20);
	fd = open(argv[1], O_CREAT | O_RDWR | O_TRUNC, 0644);
	require(fd >= 0, "open lock file");
	require(write(fd, "lock-test", 9) == 9, "write lock file");

	test_contention(argv[1], fd, BSD_FLOCK);
	test_exit_cleanup(argv[1], fd, BSD_FLOCK);
	puts("STEP47_FLOCK_PASS");

	test_contention(argv[1], fd, POSIX_LOCK);
	test_exit_cleanup(argv[1], fd, POSIX_LOCK);
	puts("STEP47_POSIX_FCNTL_PASS");

	test_contention(argv[1], fd, OFD_LOCK);
	test_exit_cleanup(argv[1], fd, OFD_LOCK);
	puts("STEP47_OFD_FCNTL_PASS");

	test_independent_classes(argv[1], fd);
	puts("STEP47_LOCK_CLASS_PASS");
	test_locked_open_unlink_mmap_write(argv[1], fd);
	require(close(fd) == 0, "close lock file");
	return 0;
}
