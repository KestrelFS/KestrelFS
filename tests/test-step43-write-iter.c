// SPDX-License-Identifier: GPL-2.0
/* write/writev/pwritev/O_APPEND verifier for Phase 4 Step 43. */
#define _GNU_SOURCE

#include <fcntl.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <unistd.h>

#define VECTOR_BYTES (6U * 4096U + 733U)
#define OVERWRITE_OFFSET 4096U
#define OVERWRITE_BYTES 4096U

static const char append_text[] = "::step43-append-vector::";

static unsigned char original_byte(size_t offset)
{
	return (unsigned char)((offset * UINT64_C(2654435761) +
				 offset / 97 + UINT64_C(0x39)) & UINT64_C(0xff));
}

static unsigned char overwritten_byte(size_t offset)
{
	return (unsigned char)(UINT64_C(0xe7) ^
			       (offset * UINT64_C(131) & UINT64_C(0xff)));
}

static int write_all(int fd, const void *buffer, size_t length)
{
	const unsigned char *bytes = buffer;
	size_t done = 0;

	while (done < length) {
		ssize_t written = write(fd, bytes + done, length - done);

		if (written < 0) {
			perror("write");
			return 1;
		}
		if (!written) {
			fprintf(stderr, "zero-length write\n");
			return 1;
		}
		done += (size_t)written;
	}
	return 0;
}

static int read_all_at(int fd, void *buffer, size_t length, off_t offset)
{
	unsigned char *bytes = buffer;
	size_t done = 0;

	while (done < length) {
		ssize_t got = pread(fd, bytes + done, length - done,
				    offset + (off_t)done);

		if (got < 0) {
			perror("pread");
			return 1;
		}
		if (!got) {
			fprintf(stderr, "unexpected EOF at %zu/%zu\n", done, length);
			return 1;
		}
		done += (size_t)got;
	}
	return 0;
}

static int test_normal_write(const char *path)
{
	static const char content[] = "step43 ordinary write(2) through write_iter\n";
	char actual[sizeof(content) - 1];
	int fd;

	fd = open(path, O_CREAT | O_EXCL | O_RDWR, 0644);
	if (fd < 0) {
		perror("open normal");
		return 1;
	}
	if (write_all(fd, content, sizeof(content) - 1) ||
	    read_all_at(fd, actual, sizeof(actual), 0) ||
	    memcmp(actual, content, sizeof(actual))) {
		fprintf(stderr, "ordinary write readback mismatch\n");
		close(fd);
		return 1;
	}
	if (close(fd)) {
		perror("close normal");
		return 1;
	}
	puts("STEP43_NORMAL_WRITE_PASS");
	return 0;
}

static int create_vector_file(const char *path)
{
	const size_t lengths[] = { 3071U, 8193U, 7777U, 6268U };
	unsigned char *buffer;
	struct iovec iov[4];
	size_t cursor = 0;
	size_t i;
	ssize_t written;
	int fd;

	buffer = malloc(VECTOR_BYTES);
	if (!buffer)
		return 1;
	for (i = 0; i < VECTOR_BYTES; i++)
		buffer[i] = original_byte(i);
	for (i = 0; i < 4; i++) {
		iov[i].iov_base = buffer + cursor;
		iov[i].iov_len = lengths[i];
		cursor += lengths[i];
	}
	if (cursor != VECTOR_BYTES) {
		fprintf(stderr, "bad vector fixture length: %zu\n", cursor);
		free(buffer);
		return 1;
	}

	fd = open(path, O_CREAT | O_EXCL | O_WRONLY, 0644);
	if (fd < 0) {
		perror("open vector");
		free(buffer);
		return 1;
	}
	written = writev(fd, iov, 4);
	if (written != (ssize_t)VECTOR_BYTES) {
		if (written < 0)
			perror("writev");
		else
			fprintf(stderr, "short writev: %zd/%u\n", written,
				VECTOR_BYTES);
		close(fd);
		free(buffer);
		return 1;
	}
	if (close(fd)) {
		perror("close vector");
		free(buffer);
		return 1;
	}
	free(buffer);
	puts("STEP43_WRITEV_PASS");
	return 0;
}

static int append_vector(const char *path)
{
	struct iovec iov[3];
	struct stat st;
	ssize_t written;
	int fd;

	fd = open(path, O_WRONLY | O_APPEND);
	if (fd < 0) {
		perror("open append");
		return 1;
	}
	if (lseek(fd, 1, SEEK_SET) != 1) {
		perror("lseek append");
		close(fd);
		return 1;
	}
	iov[0].iov_base = (void *)append_text;
	iov[0].iov_len = 4;
	iov[1].iov_base = (void *)append_text + 4;
	iov[1].iov_len = 9;
	iov[2].iov_base = (void *)append_text + 13;
	iov[2].iov_len = sizeof(append_text) - 1 - 13;
	written = writev(fd, iov, 3);
	if (written != (ssize_t)(sizeof(append_text) - 1)) {
		if (written < 0)
			perror("append writev");
		else
			fprintf(stderr, "short append: %zd\n", written);
		close(fd);
		return 1;
	}
	if (fstat(fd, &st)) {
		perror("fstat append");
		close(fd);
		return 1;
	}
	if (st.st_size != (off_t)(VECTOR_BYTES + sizeof(append_text) - 1)) {
		fprintf(stderr, "append size mismatch: %jd\n", (intmax_t)st.st_size);
		close(fd);
		return 1;
	}
	if (close(fd)) {
		perror("close append");
		return 1;
	}
	puts("STEP43_O_APPEND_PASS");
	return 0;
}

static int overwrite_vector(const char *path)
{
	unsigned char first[1537];
	unsigned char second[OVERWRITE_BYTES - sizeof(first)];
	struct iovec iov[2];
	size_t i;
	ssize_t written;
	int fd;

	for (i = 0; i < sizeof(first); i++)
		first[i] = overwritten_byte(i);
	for (i = 0; i < sizeof(second); i++)
		second[i] = overwritten_byte(sizeof(first) + i);
	iov[0].iov_base = first;
	iov[0].iov_len = sizeof(first);
	iov[1].iov_base = second;
	iov[1].iov_len = sizeof(second);
	fd = open(path, O_WRONLY);
	if (fd < 0) {
		perror("open pwritev");
		return 1;
	}
	written = pwritev(fd, iov, 2, OVERWRITE_OFFSET);
	if (written != OVERWRITE_BYTES) {
		if (written < 0)
			perror("pwritev");
		else
			fprintf(stderr, "short pwritev: %zd/%u\n", written,
				OVERWRITE_BYTES);
		close(fd);
		return 1;
	}
	if (close(fd)) {
		perror("close pwritev");
		return 1;
	}
	puts("STEP43_PWRITEV_PASS");
	return 0;
}

static int verify_vector_file(const char *path, int overwritten)
{
	const size_t total = VECTOR_BYTES + sizeof(append_text) - 1;
	unsigned char *actual;
	struct stat st;
	size_t i;
	int fd;
	int ret = 1;

	actual = malloc(total);
	if (!actual)
		return 1;
	fd = open(path, O_RDONLY);
	if (fd < 0) {
		perror("open verify");
		goto out_free;
	}
	if (fstat(fd, &st)) {
		perror("fstat verify");
		goto out_close;
	}
	if (st.st_size != (off_t)total || read_all_at(fd, actual, total, 0))
		goto out_close;
	for (i = 0; i < VECTOR_BYTES; i++) {
		unsigned char expected = original_byte(i);

		if (overwritten && i >= OVERWRITE_OFFSET &&
		    i < OVERWRITE_OFFSET + OVERWRITE_BYTES)
			expected = overwritten_byte(i - OVERWRITE_OFFSET);
		if (actual[i] != expected) {
			fprintf(stderr, "data mismatch at %zu: %u != %u\n", i,
				actual[i], expected);
			goto out_close;
		}
	}
	if (memcmp(actual + VECTOR_BYTES, append_text, sizeof(append_text) - 1)) {
		fprintf(stderr, "append payload mismatch\n");
		goto out_close;
	}
	ret = 0;

out_close:
	if (close(fd) && !ret) {
		perror("close verify");
		ret = 1;
	}
out_free:
	free(actual);
	return ret;
}

int main(int argc, char **argv)
{
	if (argc == 3 && !strcmp(argv[1], "normal"))
		return test_normal_write(argv[2]);
	if (argc == 3 && !strcmp(argv[1], "create"))
		return create_vector_file(argv[2]) || append_vector(argv[2]) ||
			verify_vector_file(argv[2], 0);
	if (argc == 3 && !strcmp(argv[1], "warm"))
		return verify_vector_file(argv[2], 0);
	if (argc == 3 && !strcmp(argv[1], "overwrite"))
		return overwrite_vector(argv[2]);
	if (argc == 3 && !strcmp(argv[1], "verify-overwrite"))
		return verify_vector_file(argv[2], 1);
	fprintf(stderr,
		"usage: %s {normal|create|warm|overwrite|verify-overwrite} PATH\n",
		argv[0]);
	return 2;
}
