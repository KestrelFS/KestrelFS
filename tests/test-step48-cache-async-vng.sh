#!/bin/bash
# Step 48: asynchronous cache-hit BIO completion and concurrent cold folios.
set -euo pipefail
# shellcheck source=_repo_root.sh
# Keep cwd at repo root so daemon/ and kestrelfs/ relative paths stay stable.
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"

test_id=$$
image=/tmp/kestrel-step48-$test_id.img
mnt=/tmp/mnt-kestrelfs-step48-$test_id
data_dir=/tmp/kestrelfs-step48-$test_id
io_helper=/tmp/kestrel-step48-io-$test_id
async_helper=/tmp/kestrel-step48-async-$test_id
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

fail() {
	local status=$?
	echo "STEP48_FAIL: line=$1 status=$status"
	test ! -f "$data_dir/daemon.log" || tail -n 100 "$data_dir/daemon.log"
	dmesg | tail -n 100
	exit "$status"
}
trap 'fail $LINENO' ERR

stop_daemon() {
	if test -n "$daemon_pid"; then
		kill "$daemon_pid"
		wait "$daemon_pid" || true
		daemon_pid=
	fi
}

start_daemon() {
	./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
		>"$data_dir/daemon.log" 2>&1 &
	daemon_pid=$!
	sleep 1
	kill -0 "$daemon_pid"
}

drop_file_pages() {
	sync
	echo 3 >/proc/sys/vm/drop_caches
}

cleanup() {
	set +e
	if mountpoint -q "$mnt"; then busybox umount "$mnt"; fi
	stop_daemon
	if test -d /sys/module/kestrelfs; then rmmod kestrelfs; fi
	if test -n "$loopdev"; then losetup -d "$loopdev"; fi
	rm -f "$image" "$io_helper" "$async_helper"
}
trap cleanup EXIT

mkdir -p "$data_dir" "$mnt"
cc -O2 -Wall -Wextra -Werror -std=gnu11 \
	tests/test-step22-cache-io.c -o "$io_helper"
cc -O2 -Wall -Wextra -Werror -std=gnu11 -pthread \
	tests/test-step48-cache-async.c -o "$async_helper"
truncate -s 128M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
	cache_namespace="$namespace"
start_daemon
busybox mount -t kestrelfs none "$mnt"
file="$mnt/async.dat"
"$io_helper" prepare "$file" 1048576
# Step 49 retains clean filemap folios after writeback. Drop them once so
# this verification fetches from the daemon and warms the NVMe read cache.
drop_file_pages
"$io_helper" verify "$file" 0 1048576 0 1

drop_file_pages
"$async_helper" same "$file"
echo STEP48_SAME_BLOCK_PASS
drop_file_pages
"$async_helper" adjacent "$file"
echo STEP48_ADJACENT_BLOCKS_PASS
submissions=$(cat /sys/module/kestrelfs/parameters/cache_async_hit_submissions)
peak=$(cat /sys/module/kestrelfs/parameters/cache_async_hit_peak)
test "$submissions" -gt 0
test "$peak" -ge 2
echo "STEP48_ASYNC_BIO_PASS submissions=$submissions peak=$peak"

drop_file_pages
"$io_helper" verify "$file" 0 1048576 0 1
echo STEP48_COLD_PAGECACHE_HIT_PASS
"$async_helper" rewrite "$file"
drop_file_pages
"$async_helper" verify-new "$file"
drop_file_pages
"$async_helper" verify-new "$file"
echo STEP48_REWRITE_INVALIDATE_PASS

dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo 'STEP48_FAIL: kernel diagnostic'
	exit 1
fi
start_ns=$(date +%s%N)
busybox umount "$mnt"
end_ns=$(date +%s%N)
umount_ms=$(((end_ns - start_ns) / 1000000))
test "$umount_ms" -lt 1000
stop_daemon
rmmod kestrelfs

# Rewriting the inode above invalidates all prior slots; its first block is
# refilled into slot 0. Corrupt that persisted block. The hit BIO must fail
# checksum validation, retire only that entry, and fall back to READ_DATA.
printf '\000' | dd of="$loopdev" bs=1 seek=$((2 * 1024 * 1024)) \
	conv=notrunc status=none
sync
insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
	cache_namespace="$namespace"
start_daemon
busybox mount -t kestrelfs none "$mnt"
"$async_helper" verify-new "$file"
failures=$(cat /sys/module/kestrelfs/parameters/cache_checksum_failures)
test "$failures" -gt 0
drop_file_pages
"$async_helper" verify-new "$file"
echo "STEP48_CHECKSUM_FALLBACK_PASS failures=$failures"
busybox umount "$mnt"
stop_daemon
rmmod kestrelfs

losetup -d "$loopdev"
loopdev=
trap - EXIT
rm -f "$image" "$io_helper" "$async_helper"
echo "STEP48_KERNEL_CACHE_ASYNC: umount_ms=$umount_ms"
echo STEP48_KERNEL_CACHE_ASYNC_PASS
