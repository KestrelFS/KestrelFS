// SPDX-License-Identifier: Apache-2.0
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/sendfile.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#define DATA_SIZE (128 * 1024)
#define STEP_SIZE 4096

static void die(const char *what)
{
	perror(what);
	exit(EXIT_FAILURE);
}

static uint8_t expected(size_t offset)
{
	return (uint8_t)((offset * 37U + offset / 97U + 11U) & 0xffU);
}

static void fill(uint8_t *buffer, size_t base, size_t length)
{
	size_t index;

	for (index = 0; index < length; index++)
		buffer[index] = expected(base + index);
}

static void verify(const uint8_t *buffer, size_t base, size_t length)
{
	size_t index;

	for (index = 0; index < length; index++) {
		if (buffer[index] != expected(base + index)) {
			fprintf(stderr, "data mismatch at %zu: got=%u expected=%u\n",
				base + index, buffer[index], expected(base + index));
			exit(EXIT_FAILURE);
		}
	}
}

static void write_all(int fd, const void *buffer, size_t length)
{
	const uint8_t *cursor = buffer;

	while (length) {
		ssize_t written = write(fd, cursor, length);

		if (written < 0)
			die("write");
		cursor += written;
		length -= (size_t)written;
	}
}

static void read_exact(int fd, void *buffer, size_t length)
{
	uint8_t *cursor = buffer;

	while (length) {
		ssize_t got = read(fd, cursor, length);

		if (got < 0)
			die("read");
		if (!got) {
			fprintf(stderr, "unexpected EOF\n");
			exit(EXIT_FAILURE);
		}
		cursor += got;
		length -= (size_t)got;
	}
}

int main(int argc, char **argv)
{
	char source[4096];
	char destination[4096];
	char sendfile_copy[4096];
	uint8_t buffer[STEP_SIZE];
	int source_fd;
	int destination_fd;
	int copy_fd;
	int pipefd[2];
	size_t offset;

	if (argc != 3) {
		fprintf(stderr, "usage: %s MOUNT TMPDIR\n", argv[0]);
		return EXIT_FAILURE;
	}
	if (snprintf(source, sizeof(source), "%s/splice-source", argv[1]) >=
		(int)sizeof(source) ||
	    snprintf(destination, sizeof(destination), "%s/splice-destination", argv[1]) >=
		(int)sizeof(destination) ||
	    snprintf(sendfile_copy, sizeof(sendfile_copy), "%s/sendfile-copy", argv[2]) >=
		(int)sizeof(sendfile_copy)) {
		fprintf(stderr, "path too long\n");
		return EXIT_FAILURE;
	}

	source_fd = open(source, O_CREAT | O_TRUNC | O_RDWR, 0600);
	if (source_fd < 0)
		die("open source");
	for (offset = 0; offset < DATA_SIZE; offset += sizeof(buffer)) {
		fill(buffer, offset, sizeof(buffer));
		write_all(source_fd, buffer, sizeof(buffer));
	}
	if (fsync(source_fd))
		die("fsync source");

	/* KestrelFS file -> pipe. Consume each page immediately so the test does
	 * not depend on pipe capacity.
	 */
	if (pipe(pipefd))
		die("pipe read");
	if (lseek(source_fd, 0, SEEK_SET) < 0)
		die("lseek source");
	for (offset = 0; offset < DATA_SIZE; offset += sizeof(buffer)) {
		ssize_t moved = splice(source_fd, NULL, pipefd[1], NULL,
				       sizeof(buffer), 0);
		if (moved != (ssize_t)sizeof(buffer))
			die("splice file to pipe");
		read_exact(pipefd[0], buffer, sizeof(buffer));
		verify(buffer, offset, sizeof(buffer));
	}
	close(pipefd[0]);
	close(pipefd[1]);

	/* Pipe -> KestrelFS file, exercising iter_file_splice_write and the same
	 * write-behind/fsync ordering as write(2).
	 */
	destination_fd = open(destination, O_CREAT | O_TRUNC | O_RDWR, 0600);
	if (destination_fd < 0)
		die("open destination");
	if (pipe(pipefd))
		die("pipe write");
	for (offset = 0; offset < DATA_SIZE; offset += sizeof(buffer)) {
		fill(buffer, offset, sizeof(buffer));
		write_all(pipefd[1], buffer, sizeof(buffer));
		if (splice(pipefd[0], NULL, destination_fd, NULL,
			   sizeof(buffer), 0) != (ssize_t)sizeof(buffer))
			die("splice pipe to file");
	}
	close(pipefd[0]);
	close(pipefd[1]);
	if (fsync(destination_fd))
		die("fsync destination");
	if (lseek(destination_fd, 0, SEEK_SET) < 0)
		die("lseek destination");
	for (offset = 0; offset < DATA_SIZE; offset += sizeof(buffer)) {
		read_exact(destination_fd, buffer, sizeof(buffer));
		verify(buffer, offset, sizeof(buffer));
	}

	/* sendfile from KestrelFS must select the filemap splice-read path. */
	copy_fd = open(sendfile_copy, O_CREAT | O_TRUNC | O_RDWR, 0600);
	if (copy_fd < 0)
		die("open sendfile copy");
	if (lseek(source_fd, 0, SEEK_SET) < 0)
		die("lseek source for sendfile");
	for (offset = 0; offset < DATA_SIZE;) {
		ssize_t moved = sendfile(copy_fd, source_fd, NULL, DATA_SIZE - offset);

		if (moved < 0)
			die("sendfile");
		if (!moved) {
			fprintf(stderr, "sendfile made no progress\n");
			return EXIT_FAILURE;
		}
		offset += (size_t)moved;
	}
	if (lseek(copy_fd, 0, SEEK_SET) < 0)
		die("lseek sendfile copy");
	for (offset = 0; offset < DATA_SIZE; offset += sizeof(buffer)) {
		read_exact(copy_fd, buffer, sizeof(buffer));
		verify(buffer, offset, sizeof(buffer));
	}

	close(copy_fd);
	close(destination_fd);
	close(source_fd);
	return EXIT_SUCCESS;
}
