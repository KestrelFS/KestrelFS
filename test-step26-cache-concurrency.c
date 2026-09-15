// SPDX-License-Identifier: GPL-2.0
/* Concurrent cache-reader versus rewrite verifier for Step 26 vng tests. */
#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

struct reader_args {
	int fd;
	size_t length;
	pthread_barrier_t *barrier;
	int failed;
};

static unsigned char pattern_byte(uint64_t offset, int rewritten)
{
	unsigned char original =
		(unsigned char)((offset * UINT64_C(1315423911) + offset / 127 +
				 UINT64_C(0x5a)) & UINT64_C(0xff));

	return rewritten ? (unsigned char)(original ^ 0xffU) : original;
}

static uint64_t parse_u64(const char *text, const char *name)
{
	char *end = NULL;
	unsigned long long value;

	errno = 0;
	value = strtoull(text, &end, 10);
	if (errno || !end || *end) {
		fprintf(stderr, "invalid %s: %s\n", name, text);
		exit(2);
	}
	return (uint64_t)value;
}

static int pread_full(int fd, unsigned char *buffer, size_t length)
{
	size_t done = 0;

	while (done < length) {
		ssize_t got = pread(fd, buffer + done, length - done, (off_t)done);

		if (got <= 0) {
			if (got < 0)
				perror("pread");
			else
				fprintf(stderr, "unexpected EOF at %zu\n", done);
			return 1;
		}
		done += (size_t)got;
	}
	return 0;
}

static int verify_pattern(const unsigned char *buffer, size_t length,
			  int rewritten)
{
	size_t i;

	for (i = 0; i < length; i++) {
		if (buffer[i] != pattern_byte(i, rewritten)) {
			fprintf(stderr, "pattern mismatch at %zu rewritten=%d\n",
				i, rewritten);
			return 1;
		}
	}
	return 0;
}

static void *reader_main(void *opaque)
{
	struct reader_args *args = opaque;
	unsigned char *buffer = NULL;
	int barrier_ret;

	if (posix_memalign((void **)&buffer, 4096, args->length)) {
		fprintf(stderr, "reader allocation failed\n");
		_exit(1);
	}
	/* Fault and initialize the destination before entering the kernel read. */
	memset(buffer, 0, args->length);
	barrier_ret = pthread_barrier_wait(args->barrier);
	if (barrier_ret && barrier_ret != PTHREAD_BARRIER_SERIAL_THREAD) {
		fprintf(stderr, "reader barrier failed\n");
		_exit(1);
	}
	args->failed = pread_full(args->fd, buffer, args->length) ||
		verify_pattern(buffer, args->length, 0);
	free(buffer);
	return NULL;
}

static unsigned long read_counter(const char *path)
{
	char buffer[64];
	char *end = NULL;
	ssize_t got;
	int fd;

	fd = open(path, O_RDONLY | O_CLOEXEC);
	if (fd < 0) {
		perror("open counter");
		exit(1);
	}
	got = read(fd, buffer, sizeof(buffer) - 1);
	close(fd);
	if (got <= 0) {
		perror("read counter");
		exit(1);
	}
	buffer[got] = '\0';
	errno = 0;
	unsigned long value = strtoul(buffer, &end, 10);
	if (errno || end == buffer) {
		fprintf(stderr, "invalid counter: %s\n", buffer);
		exit(1);
	}
	return value;
}

static int wait_for_readers(const char *counter_path, unsigned int readers)
{
	struct timespec start;
	struct timespec now;

	if (clock_gettime(CLOCK_MONOTONIC, &start)) {
		perror("clock_gettime");
		return 1;
	}
	do {
		if (read_counter(counter_path) >= readers)
			return 0;
		sched_yield();
		if (clock_gettime(CLOCK_MONOTONIC, &now)) {
			perror("clock_gettime");
			return 1;
		}
	} while ((now.tv_sec - start.tv_sec) < 10);
	fprintf(stderr, "timed out waiting for %u active cache readers\n", readers);
	return 1;
}

static int write_rewritten_pattern(const char *path, size_t length)
{
	unsigned char *buffer = NULL;
	size_t done = 0;
	int fd;
	int ret = 1;

	if (posix_memalign((void **)&buffer, 4096, length))
		return 1;
	for (size_t i = 0; i < length; i++)
		buffer[i] = pattern_byte(i, 1);
	fd = open(path, O_WRONLY | O_CLOEXEC);
	if (fd < 0) {
		perror("open rewrite");
		goto out;
	}
	while (done < length) {
		ssize_t written = pwrite(fd, buffer + done, length - done,
					 (off_t)done);

		if (written <= 0) {
			perror("pwrite");
			goto out_close;
		}
		done += (size_t)written;
	}
	ret = 0;
out_close:
	close(fd);
out:
	free(buffer);
	return ret;
}

static int verify_file(const char *path, size_t length, int rewritten)
{
	unsigned char *buffer = NULL;
	int fd;
	int ret = 1;

	if (posix_memalign((void **)&buffer, 4096, length))
		return 1;
	fd = open(path, O_RDONLY | O_CLOEXEC);
	if (fd < 0) {
		perror("open verify");
		goto out;
	}
	ret = pread_full(fd, buffer, length) ||
		verify_pattern(buffer, length, rewritten);
	close(fd);
out:
	free(buffer);
	return ret;
}

static int rewrite_race(const char *path, size_t length,
			unsigned int reader_count, const char *counter_path)
{
	struct reader_args *args;
	pthread_t *threads;
	pthread_barrier_t barrier;
	unsigned int started = 0;
	unsigned int i;
	int read_fd;
	int ret = 1;

	args = calloc(reader_count, sizeof(*args));
	threads = calloc(reader_count, sizeof(*threads));
	if (!args || !threads)
		goto out;
	read_fd = open(path, O_RDONLY | O_CLOEXEC);
	if (read_fd < 0) {
		perror("open readers");
		goto out;
	}
	if (pthread_barrier_init(&barrier, NULL, reader_count + 1))
		goto out_close;
	for (i = 0; i < reader_count; i++) {
		args[i].fd = read_fd;
		args[i].length = length;
		args[i].barrier = &barrier;
		if (pthread_create(&threads[i], NULL, reader_main, &args[i])) {
			fprintf(stderr, "pthread_create failed\n");
			exit(1);
		}
		started++;
	}
	{
		int barrier_ret = pthread_barrier_wait(&barrier);

		if (barrier_ret && barrier_ret != PTHREAD_BARRIER_SERIAL_THREAD) {
			fprintf(stderr, "main barrier failed\n");
			goto out_join;
		}
	}
	if (wait_for_readers(counter_path, reader_count))
		goto out_join;
	/* The write-side invalidation must wait for every shared-lock reader. */
	if (write_rewritten_pattern(path, length))
		goto out_join;
	ret = 0;
out_join:
	for (i = 0; i < started; i++) {
		pthread_join(threads[i], NULL);
		if (args[i].failed)
			ret = 1;
	}
	pthread_barrier_destroy(&barrier);
out_close:
	close(read_fd);
out:
	free(threads);
	free(args);
	if (!ret && verify_file(path, length, 1))
		ret = 1;
	if (!ret)
		printf("rewrite_race_pass readers=%u bytes=%zu\n",
		       reader_count, length);
	return ret;
}

int main(int argc, char **argv)
{
	uint64_t length;
	uint64_t readers;

	if (argc == 4 && !strcmp(argv[1], "verify-new")) {
		length = parse_u64(argv[3], "length");
		if (length > SIZE_MAX)
			return 2;
		return verify_file(argv[2], (size_t)length, 1);
	}
	if (argc == 6 && !strcmp(argv[1], "rewrite-race")) {
		length = parse_u64(argv[3], "length");
		readers = parse_u64(argv[4], "reader count");
		if (!length || length > SIZE_MAX || readers < 2 ||
		    readers > 64)
			return 2;
		return rewrite_race(argv[2], (size_t)length,
				    (unsigned int)readers, argv[5]);
	}
	fprintf(stderr,
		"usage: %s rewrite-race PATH LENGTH READERS ACTIVE_COUNTER\n"
		"       %s verify-new PATH LENGTH\n",
		argv[0], argv[0]);
	return 2;
}
