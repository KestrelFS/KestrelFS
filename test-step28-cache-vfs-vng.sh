#!/bin/bash
# Phase 4 Step 28 read_iter/iov_iter cache-hit regression.
set -euo pipefail

test_id=$$
image=/tmp/kestrel-step28-cache-$test_id.img
mnt=/tmp/mnt-kestrelfs-step28-$test_id
data_dir=/tmp/kestrelfs-step28-$test_id
helper=/tmp/kestrel-step28-cache-vfs-$test_id
payload_size=$((4 * 4096 + 137))
loopdev=
daemon_pid=
last_umount_ms=0
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

report_error() {
	local status=$?

	echo "STEP28_FAIL: line=$1 status=$status"
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
cc -O2 -Wall -Wextra -Werror -std=gnu11 test-step28-cache-vfs.c \
	-o "$helper"
truncate -s 128M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
echo "STEP28_CACHE: loop_device=$loopdev namespace=$namespace"

insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
	cache_namespace="$namespace"
start_daemon
busybox mount -t kestrelfs none "$mnt"
"$helper" prepare "$mnt/read-iter.dat" "$payload_size"
"$helper" warm "$mnt/read-iter.dat"

# Dentry and every data block are hot; all following preadv calls must work
# without READ_DATA or any daemon process.
stop_daemon
direct_before=$(cat /sys/module/kestrelfs/parameters/cache_direct_hit_blocks)
copy_before=$(cat /sys/module/kestrelfs/parameters/cache_copy_hit_blocks)
"$helper" verify "$mnt/read-iter.dat"
direct_after=$(cat /sys/module/kestrelfs/parameters/cache_direct_hit_blocks)
copy_after=$(cat /sys/module/kestrelfs/parameters/cache_copy_hit_blocks)
test "$direct_after" -gt "$direct_before"
test "$copy_after" -gt "$copy_before"
echo "STEP28_DAEMON_FREE_HIT_PASS direct_delta=$((direct_after - direct_before)) copy_delta=$((copy_after - copy_before))"

dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo "STEP28_FAIL: kernel safety diagnostic found"
	exit 1
fi
unmount_and_unload

losetup -d "$loopdev"
loopdev=
trap - EXIT
rm -f "$image" "$helper"
echo "STEP28_CACHE: umount_ms=$last_umount_ms"
echo "STEP28_CACHE_VFS_PASS"
