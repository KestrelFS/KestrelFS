#!/bin/bash
# Phase 4 Step 24 data/index checksum and v2 rejection test for virtme-ng.
set -euo pipefail

test_id=$$
image=/tmp/kestrel-step24-cache-$test_id.img
mnt=/tmp/mnt-kestrelfs-step24-$test_id
data_dir=/tmp/kestrelfs-step24-$test_id
helper=/tmp/kestrel-step24-cache-io-$test_id
metadata_backup=$data_dir/metadata-v4-good.bin
data_start=$((2 * 1024 * 1024))
index_start=8192
loopdev=
daemon_pid=
last_umount_ms=0
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

report_error() {
	local status=$?

	echo "STEP24_FAIL: line=$1 status=$status"
	if test -f "$data_dir/daemon.log"; then
		tail -n 80 "$data_dir/daemon.log"
	fi
	dmesg | tail -n 80
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

load_cache() {
	insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=8 \
		cache_namespace="$namespace"
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

expect_checksum_miss() {
	local path=$1
	local file_offset=$2
	local length=$3
	local user_shift=$4
	local label=$5

	if "$helper" verify "$path" "$file_offset" "$length" "$user_shift" 1 \
		>"$data_dir/$label.log" 2>&1; then
		echo "STEP24_FAIL: corrupt cache data was returned for $label"
		exit 1
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
cc -O2 -Wall -Wextra -Werror test-step22-cache-io.c -o "$helper"
truncate -s 16M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
echo "STEP24_CACHE: loop_device=$loopdev namespace=$namespace"

# Slot 0 and slot 1 receive one complete checksummed block each.
load_cache
start_daemon
busybox mount -t kestrelfs none "$mnt"
"$helper" prepare "$mnt/checksum-a.dat" 4096
"$helper" prepare "$mnt/checksum-b.dat" 4096
"$helper" verify "$mnt/checksum-a.dat" 0 4096 0 1
"$helper" verify "$mnt/checksum-b.dat" 0 4096 0 1
test "$(cat /sys/module/kestrelfs/parameters/cache_checksum_failures)" -eq 0
unmount_and_unload

# Corrupt A's slot and prove the pinned-page hit detects it while B survives.
printf '\000' | dd of="$loopdev" bs=1 seek="$data_start" \
	conv=notrunc status=none
sync
load_cache
start_daemon
busybox mount -t kestrelfs none "$mnt"
"$helper" verify "$mnt/checksum-b.dat" 0 4096 0 1
test -f "$mnt/checksum-a.dat"
stop_daemon
expect_checksum_miss "$mnt/checksum-a.dat" 0 4096 0 pinned-corruption
"$helper" verify "$mnt/checksum-b.dat" 0 4096 0 1
echo "STEP24_CACHE: pinned checksum_failures=$(cat /sys/module/kestrelfs/parameters/cache_checksum_failures)"
test "$(cat /sys/module/kestrelfs/parameters/cache_checksum_failures)" -eq 1
start_daemon
"$helper" verify "$mnt/checksum-a.dat" 0 4096 0 1
stop_daemon
"$helper" verify "$mnt/checksum-a.dat" 0 4096 0 1
echo "STEP24_CACHE: pinned-page corruption retired and refilled"
unmount_and_unload

# Corrupt B's slot and exercise the partial/unaligned buffered checksum path.
printf '\000' | dd of="$loopdev" bs=1 seek=$((data_start + 4096)) \
	conv=notrunc status=none
sync
load_cache
start_daemon
busybox mount -t kestrelfs none "$mnt"
"$helper" verify "$mnt/checksum-a.dat" 0 4096 0 1
test -f "$mnt/checksum-b.dat"
stop_daemon
expect_checksum_miss "$mnt/checksum-b.dat" 1 4095 1 buffered-corruption
"$helper" verify "$mnt/checksum-a.dat" 0 4096 0 1
echo "STEP24_CACHE: buffered checksum_failures=$(cat /sys/module/kestrelfs/parameters/cache_checksum_failures)"
test "$(cat /sys/module/kestrelfs/parameters/cache_checksum_failures)" -eq 1
start_daemon
"$helper" verify "$mnt/checksum-b.dat" 0 4096 0 1
stop_daemon
"$helper" verify "$mnt/checksum-b.dat" 1 4095 1 1
echo "STEP24_CACHE: buffered corruption retired and refilled"
unmount_and_unload

# Save valid v4 metadata, then corrupt a slot-0 key byte without updating CRC.
dd if="$loopdev" of="$metadata_backup" bs=1M count=2 status=none
printf '\377' | dd of="$loopdev" bs=1 \
	seek=$((index_start + 8)) conv=notrunc status=none
sync
if load_cache; then
	echo "STEP24_FAIL: corrupt index entry was accepted"
	exit 1
fi
test ! -d /sys/module/kestrelfs
dmesg >"$data_dir/index-corruption-dmesg.log"
grep -F 'invalid cache index entry at slot 0' \
	"$data_dir/index-corruption-dmesg.log"
echo "STEP24_CACHE: corrupt index checksum failed closed"

# Restore valid metadata, synthesize an old v2 version, and require rejection.
dd if="$metadata_backup" of="$loopdev" bs=1M count=2 \
	conv=notrunc status=none
printf '\002\000\000\000' | dd of="$loopdev" bs=1 seek=8 \
	conv=notrunc status=none
sync
if load_cache; then
	echo "STEP24_FAIL: cache format v2 was accepted"
	exit 1
fi
test ! -d /sys/module/kestrelfs
dmesg >"$data_dir/v2-rejection-dmesg.log"
grep -F 'cache superblock format or geometry mismatch' \
	"$data_dir/v2-rejection-dmesg.log"
echo "STEP24_CACHE: v2 rejected without migration"

losetup -d "$loopdev"
loopdev=
trap - EXIT
rm -f "$image" "$helper"
echo "STEP24_CACHE: umount_ms=$last_umount_ms"
echo "STEP24_CHECKSUM_PASS"
