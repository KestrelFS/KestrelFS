#!/bin/bash
# Phase 4 Step 43 write_iter/writev/cache-invalidation regression.
set -euo pipefail
# shellcheck source=_repo_root.sh
# Keep cwd at repo root so daemon/ and kestrelfs/ relative paths stay stable.
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"

test_id=$$
image=/tmp/kestrel-step43-write-iter-$test_id.img
mnt=/tmp/mnt-kestrelfs-step43-$test_id
data_dir=/tmp/kestrelfs-step43-$test_id
helper=/tmp/kestrel-step43-write-iter-$test_id
loopdev=
daemon_pid=
last_umount_ms=0
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

report_error() {
	local status=$?

	echo "STEP43_FAIL: line=$1 status=$status"
	if test -f "$data_dir/daemon.log"; then
		tail -n 100 "$data_dir/daemon.log"
	fi
	dmesg | tail -n 120
	exit "$status"
}
trap 'report_error $LINENO' ERR

start_daemon() {
	./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
		>"$data_dir/daemon.log" 2>&1 &
	daemon_pid=$!
	sleep 1
	kill -0 "$daemon_pid"
}

stop_daemon() {
	if [ -n "$daemon_pid" ]; then
		kill "$daemon_pid"
		wait "$daemon_pid" || true
		daemon_pid=
	fi
}

unmount_and_unload() {
	if mountpoint -q "$mnt"; then
		local start_ns
		local end_ns

		start_ns=$(date +%s%N)
		busybox umount "$mnt"
		end_ns=$(date +%s%N)
		last_umount_ms=$(( (end_ns - start_ns) / 1000000 ))
		test "$last_umount_ms" -lt 1000
	fi
	stop_daemon
	if test -d /sys/module/kestrelfs; then
		rmmod kestrelfs
	fi
}

cleanup() {
	set +e
	unmount_and_unload
	if [ -n "$loopdev" ]; then
		losetup -d "$loopdev" 2>/dev/null
	fi
	rm -f "$image" "$helper"
}
trap cleanup EXIT

rm -rf "$data_dir" "$mnt"
rm -f "$image" "$helper"
mkdir -p "$data_dir" "$mnt"
cc -O2 -Wall -Wextra -Werror -std=gnu11 tests/test-step43-write-iter.c \
	-o "$helper"
truncate -s 128M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
echo "STEP43_CACHE: loop_device=$loopdev namespace=$namespace"

insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
	cache_namespace="$namespace"
start_daemon
busybox mount -t kestrelfs none "$mnt"

"$helper" normal "$mnt/normal-write.dat"
"$helper" create "$mnt/vector-write.dat"
# Step 49 writeback retains clean filemap folios. Drop them before warming
# the lower cache from the authoritative daemon.
echo 1 >/proc/sys/vm/drop_caches
"$helper" warm "$mnt/vector-write.dat"

hit_count() {
	local direct
	local copied

	direct=$(cat /sys/module/kestrelfs/parameters/cache_direct_hit_blocks)
	copied=$(cat /sys/module/kestrelfs/parameters/cache_copy_hit_blocks)
	echo $((direct + copied))
}

hot_before=$(hit_count)
# Step 45 adds a read-side page cache; drop only clean file pages so this
# legacy assertion still measures the lower NVMe cache rather than filemap.
echo 1 >/proc/sys/vm/drop_caches
"$helper" warm "$mnt/vector-write.dat"
hot_after=$(hit_count)
test "$hot_after" -gt "$hot_before"
echo "STEP43_CACHE_WARM_HIT_PASS delta=$((hot_after - hot_before))"

"$helper" overwrite "$mnt/vector-write.dat"
invalidate_before=$(hit_count)
"$helper" verify-overwrite "$mnt/vector-write.dat"
invalidate_after=$(hit_count)
test "$invalidate_after" -eq "$invalidate_before"
echo "STEP43_CACHE_INVALIDATE_PASS first_read_hit_delta=0"

echo 1 >/proc/sys/vm/drop_caches
"$helper" verify-overwrite "$mnt/vector-write.dat"
test "$(hit_count)" -eq "$invalidate_after"
echo 1 >/proc/sys/vm/drop_caches
"$helper" verify-overwrite "$mnt/vector-write.dat"
refill_after=$(hit_count)
test "$refill_after" -gt "$invalidate_after"
echo "STEP43_CACHE_REFILL_PASS delta=$((refill_after - invalidate_after))"

dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo "STEP43_FAIL: kernel safety diagnostic found"
	exit 1
fi
unmount_and_unload

losetup -d "$loopdev"
loopdev=
trap - EXIT
rm -f "$image" "$helper"
echo "STEP43_WRITE_ITER: umount_ms=$last_umount_ms"
echo "STEP43_WRITE_ITER_PASS"
