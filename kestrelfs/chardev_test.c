/*
 * chardev_test.c - Standalone userspace smoke test for
 * /dev/kestrel_ctl, exercising ONLY the Phase 2 infrastructure that
 * currently exists: open, GET_ABI_VERSION, GET_REGION_SIZE, mmap,
 * reading the magic/abi_version header fields back out of the
 * mapped region, and a non-blocking poll() check.
 *
 * This does NOT exercise any push/pop business logic (there is none
 * yet) - it only proves the chardev bridge itself (device node,
 * ioctl, mmap) is wired correctly end-to-end.
 *
 * Build:  gcc -O2 -Wall -I. -o chardev_test chardev_test.c
 * Run:    sudo ./chardev_test
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/ioctl.h>
#include <poll.h>

#include "kestrelfs_ipc.h"

#define DEV_PATH "/dev/kestrel_ctl"

int main(void)
{
	int fd;
	__u32 abi_version = 0;
	__u64 region_size = 0;
	struct kestrelfs_shared_region *region;
	struct pollfd pfd;
	int poll_ret;

	fd = open(DEV_PATH, O_RDWR);
	if (fd < 0) {
		fprintf(stderr, "open(%s) failed: %s\n", DEV_PATH, strerror(errno));
		return 1;
	}
	printf("[OK] open(%s) -> fd=%d\n", DEV_PATH, fd);

	if (ioctl(fd, KESTRELFS_IOC_GET_ABI_VERSION, &abi_version) < 0) {
		fprintf(stderr, "ioctl(GET_ABI_VERSION) failed: %s\n", strerror(errno));
		close(fd);
		return 1;
	}
	printf("[OK] KESTRELFS_IOC_GET_ABI_VERSION -> %u (expected %u)\n",
	       abi_version, (unsigned)KESTRELFS_ABI_VERSION);
	if (abi_version != KESTRELFS_ABI_VERSION) {
		fprintf(stderr, "[FAIL] ABI version mismatch\n");
		close(fd);
		return 1;
	}

	if (ioctl(fd, KESTRELFS_IOC_GET_REGION_SIZE, &region_size) < 0) {
		fprintf(stderr, "ioctl(GET_REGION_SIZE) failed: %s\n", strerror(errno));
		close(fd);
		return 1;
	}
	printf("[OK] KESTRELFS_IOC_GET_REGION_SIZE -> %llu (expected %llu)\n",
	       (unsigned long long)region_size,
	       (unsigned long long)KESTRELFS_SHM_REGION_SIZE);
	if (region_size != KESTRELFS_SHM_REGION_SIZE) {
		fprintf(stderr, "[FAIL] region size mismatch\n");
		close(fd);
		return 1;
	}

	region = mmap(NULL, region_size, PROT_READ | PROT_WRITE,
		      MAP_SHARED, fd, 0);
	if (region == MAP_FAILED) {
		fprintf(stderr, "mmap failed: %s\n", strerror(errno));
		close(fd);
		return 1;
	}
	printf("[OK] mmap -> %p (%llu bytes)\n", (void *)region,
	       (unsigned long long)region_size);

	printf("[OK] region->magic        = 0x%08x (expected 0x%08x)\n",
	       region->magic, KESTRELFS_SHM_MAGIC);
	printf("[OK] region->abi_version  = %u\n", region->abi_version);
	printf("[OK] region->req_ctrl.head/tail  = %llu/%llu (capacity=%u)\n",
	       (unsigned long long)region->req_ctrl.head,
	       (unsigned long long)region->req_ctrl.tail,
	       region->req_ctrl.capacity);
	printf("[OK] region->resp_ctrl.head/tail = %llu/%llu (capacity=%u)\n",
	       (unsigned long long)region->resp_ctrl.head,
	       (unsigned long long)region->resp_ctrl.tail,
	       region->resp_ctrl.capacity);

	if (region->magic != KESTRELFS_SHM_MAGIC) {
		fprintf(stderr, "[FAIL] magic mismatch\n");
		munmap(region, region_size);
		close(fd);
		return 1;
	}

	/*
	 * Nothing ever pushes into req_ctrl (no in-tree producer yet),
	 * so this poll() is expected to time out with revents == 0.
	 * This exercises kestrelfs_poll()'s registration path without
	 * asserting any readiness.
	 */
	pfd.fd = fd;
	pfd.events = POLLIN;
	pfd.revents = 0;
	poll_ret = poll(&pfd, 1, 500 /* ms */);
	printf("[OK] poll() returned %d, revents=0x%x (expected 0, no producer yet)\n",
	       poll_ret, pfd.revents);

	/* KESTRELFS_IOC_NOTIFY_RESP takes no argument. */
	if (ioctl(fd, KESTRELFS_IOC_NOTIFY_RESP) < 0) {
		fprintf(stderr, "ioctl(NOTIFY_RESP) failed: %s\n", strerror(errno));
		munmap(region, region_size);
		close(fd);
		return 1;
	}
	printf("[OK] KESTRELFS_IOC_NOTIFY_RESP accepted\n");

	munmap(region, region_size);
	close(fd);
	printf("[PASS] all chardev infrastructure checks succeeded\n");
	return 0;
}
