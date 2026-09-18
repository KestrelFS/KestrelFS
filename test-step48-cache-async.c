// SPDX-License-Identifier: GPL-2.0
/* Concurrent same/adjacent-offset page-cache misses backed by cache-hit BIO. */
#define _POSIX_C_SOURCE 200809L
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define WORKERS 16
#define BLOCK 4096
#define STRIDE BLOCK

struct reader {
	const char *path;
	pthread_barrier_t *barrier;
	off_t offset;
	int failed;
};

static unsigned char expected_byte(uint64_t offset)
{
	return (unsigned char)((offset * UINT64_C(1315423911) +
		offset / 127 + UINT64_C(0x5a)) & UINT64_C(0xff));
}

static void *read_one(void *opaque)
{
	struct reader *reader = opaque;
	unsigned char *data = NULL;
	int fd, barrier_status;
	size_t i;

	fd = open(reader->path, O_RDONLY);
	if (fd < 0 || posix_memalign((void **)&data, BLOCK, BLOCK)) {
		perror("reader open/allocation");
		_exit(1);
	}
	posix_fadvise(fd, 0, 0, POSIX_FADV_RANDOM);
	barrier_status = pthread_barrier_wait(reader->barrier);
	if (barrier_status && barrier_status != PTHREAD_BARRIER_SERIAL_THREAD) {
		reader->failed = 1;
		goto out;
	}
	if (pread(fd, data, BLOCK, reader->offset) != BLOCK) {
		reader->failed = 1;
		goto out;
	}
	for (i = 0; i < BLOCK; i++) {
		if (data[i] != expected_byte(reader->offset + i)) {
			reader->failed = 1;
			break;
		}
	}
out:
	free(data);
	close(fd);
	return NULL;
}

static int run_readers(const char *path, int same_block)
{
	pthread_barrier_t barrier;
	pthread_t threads[WORKERS];
	struct reader readers[WORKERS];
	unsigned int i;
	int failed = 0;

	if (pthread_barrier_init(&barrier, NULL, WORKERS + 1))
		return 1;
	for (i = 0; i < WORKERS; i++) {
		readers[i] = (struct reader) {
			.path = path,
			.barrier = &barrier,
			.offset = same_block ? 0 : (off_t)i * STRIDE,
		};
		if (pthread_create(&threads[i], NULL, read_one, &readers[i])) {
			fprintf(stderr, "pthread_create failed\n");
			_exit(1);
		}
	}
	pthread_barrier_wait(&barrier);
	for (i = 0; i < WORKERS; i++) {
		pthread_join(threads[i], NULL);
		failed |= readers[i].failed;
	}
	pthread_barrier_destroy(&barrier);
	if (failed)
		fprintf(stderr, "concurrent read mismatch (same=%d)\n", same_block);
	return failed;
}

static int verify_new(const char *path, bool rewrite)
{
	unsigned char data[BLOCK];
	int fd;

	if (rewrite)
		memset(data, 'Z', sizeof(data));
	fd = open(path, O_RDWR);
	if (fd < 0)
		return 1;
	if ((rewrite && pwrite(fd, data, sizeof(data), 0) != sizeof(data)) ||
	    pread(fd, data, sizeof(data), 0) != sizeof(data)) {
		close(fd);
		return 1;
	}
	for (size_t i = 0; i < sizeof(data); i++) {
		if (data[i] != 'Z') {
			fprintf(stderr, "stale byte after rewrite at %zu\n", i);
			close(fd);
			return 1;
		}
	}
	close(fd);
	return 0;
}

int main(int argc, char **argv)
{
	if (argc != 3)
		return 2;
	if (!strcmp(argv[1], "same"))
		return run_readers(argv[2], 1);
	if (!strcmp(argv[1], "adjacent"))
		return run_readers(argv[2], 0);
	if (!strcmp(argv[1], "rewrite"))
		return verify_new(argv[2], true);
	if (!strcmp(argv[1], "verify-new"))
		return verify_new(argv[2], false);
	return 2;
}
