// SPDX-License-Identifier: Apache-2.0
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define REGION_SIZE (2U * 1024U * 1024U)

struct worker {
	const char *path;
	off_t offset;
	unsigned int generation;
	pthread_barrier_t *barrier;
	int error;
};

static unsigned char pattern(unsigned int generation, size_t offset)
{
	return (unsigned char)((offset * 37U + generation * 83U + 19U) & 0xffU);
}

static void fill(unsigned char *buffer, unsigned int generation)
{
	size_t i;

	for (i = 0; i < REGION_SIZE; i++)
		buffer[i] = pattern(generation, i);
}

static void *write_and_fsync(void *arg)
{
	struct worker *worker = arg;
	unsigned char *buffer;
	ssize_t written;
	int fd;
	int barrier_ret;

	buffer = malloc(REGION_SIZE);
	if (!buffer) {
		worker->error = ENOMEM;
		return NULL;
	}
	fill(buffer, worker->generation);
	fd = open(worker->path, O_CREAT | O_RDWR, 0644);
	if (fd < 0) {
		worker->error = errno;
		free(buffer);
		return NULL;
	}
	written = pwrite(fd, buffer, REGION_SIZE, worker->offset);
	if (written != REGION_SIZE) {
		worker->error = written < 0 ? errno : EIO;
		close(fd);
		free(buffer);
		return NULL;
	}
	barrier_ret = pthread_barrier_wait(worker->barrier);
	if (barrier_ret != 0 && barrier_ret != PTHREAD_BARRIER_SERIAL_THREAD) {
		worker->error = barrier_ret;
	} else if (fsync(fd)) {
		worker->error = errno;
	}
	if (close(fd) && !worker->error)
		worker->error = errno;
	free(buffer);
	return NULL;
}

static int verify_region(const char *path, off_t offset,
			 unsigned int generation)
{
	unsigned char *buffer;
	ssize_t got;
	size_t i;
	int fd;
	int ret = 1;

	buffer = malloc(REGION_SIZE);
	if (!buffer)
		return 1;
	fd = open(path, O_RDONLY);
	if (fd < 0) {
		perror("open verify");
		goto out_free;
	}
	got = pread(fd, buffer, REGION_SIZE, offset);
	if (got != REGION_SIZE) {
		fprintf(stderr, "pread %s offset=%lld got=%zd errno=%d\n",
			path, (long long)offset, got, errno);
		goto out_close;
	}
	for (i = 0; i < REGION_SIZE; i++) {
		if (buffer[i] != pattern(generation, i)) {
			fprintf(stderr,
				"data mismatch %s generation=%u offset=%zu\n",
				path, generation, i);
			goto out_close;
		}
	}
	ret = 0;
out_close:
	close(fd);
out_free:
	free(buffer);
	return ret;
}

static int run_pair(struct worker workers[2])
{
	pthread_barrier_t barrier;
	pthread_t threads[2];
	int i;
	int ret = 0;

	if (pthread_barrier_init(&barrier, NULL, 2))
		return 1;
	for (i = 0; i < 2; i++) {
		workers[i].barrier = &barrier;
		workers[i].error = 0;
		if (pthread_create(&threads[i], NULL, write_and_fsync, &workers[i]))
			return 1;
	}
	for (i = 0; i < 2; i++) {
		pthread_join(threads[i], NULL);
		if (workers[i].error) {
			fprintf(stderr, "worker %d failed: %s\n", i,
				strerror(workers[i].error));
			ret = 1;
		}
	}
	pthread_barrier_destroy(&barrier);
	return ret;
}

static int exercise(const char *dir)
{
	char first[4096];
	char second[4096];
	char shared[4096];
	struct worker different[2];
	struct worker same[2];

	if (snprintf(first, sizeof(first), "%s/parallel-a", dir) >= (int)sizeof(first) ||
	    snprintf(second, sizeof(second), "%s/parallel-b", dir) >= (int)sizeof(second) ||
	    snprintf(shared, sizeof(shared), "%s/ordered-shared", dir) >= (int)sizeof(shared))
		return 1;
	different[0] = (struct worker){ .path = first, .offset = 0, .generation = 1 };
	different[1] = (struct worker){ .path = second, .offset = 0, .generation = 2 };
	if (run_pair(different) || verify_region(first, 0, 1) ||
	    verify_region(second, 0, 2))
		return 1;

	same[0] = (struct worker){ .path = shared, .offset = 0, .generation = 3 };
	same[1] = (struct worker){ .path = shared, .offset = REGION_SIZE, .generation = 4 };
	if (run_pair(same) || verify_region(shared, 0, 3) ||
	    verify_region(shared, REGION_SIZE, 4))
		return 1;
	return 0;
}

static int verify(const char *dir)
{
	char first[4096];
	char second[4096];
	char shared[4096];

	snprintf(first, sizeof(first), "%s/parallel-a", dir);
	snprintf(second, sizeof(second), "%s/parallel-b", dir);
	snprintf(shared, sizeof(shared), "%s/ordered-shared", dir);
	return verify_region(first, 0, 1) || verify_region(second, 0, 2) ||
		verify_region(shared, 0, 3) ||
		verify_region(shared, REGION_SIZE, 4);
}

int main(int argc, char **argv)
{
	if (argc != 3) {
		fprintf(stderr, "usage: %s exercise|verify MOUNTPOINT\n", argv[0]);
		return 2;
	}
	if (!strcmp(argv[1], "exercise"))
		return exercise(argv[2]);
	if (!strcmp(argv[1], "verify"))
		return verify(argv[2]);
	fprintf(stderr, "unknown mode: %s\n", argv[1]);
	return 2;
}
