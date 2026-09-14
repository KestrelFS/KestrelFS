#!/bin/bash
# Phase 4 Step 25 persistent cache transaction/recovery test for virtme-ng.
set -euo pipefail

test_id=$$
image=/tmp/kestrel-step25-cache-$test_id.img
mnt=/tmp/mnt-kestrelfs-step25-$test_id
data_dir=/tmp/kestrelfs-step25-$test_id
io_helper=/tmp/kestrel-step25-cache-io-$test_id
txn_helper=/tmp/kestrel-step25-cache-txn-$test_id
super_backup=$data_dir/super-v4-good.bin
journal_offset=4096
loopdev=
daemon_pid=
last_umount_ms=0
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

report_error() {
	local status=$?

	echo "STEP25_FAIL: line=$1 status=$status"
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

expect_miss() {
	local path=$1
	local label=$2

	if "$io_helper" verify "$path" 0 4096 0 1 \
		>"$data_dir/$label.log" 2>&1; then
		echo "STEP25_FAIL: $label unexpectedly hit"
		exit 1
	fi
}

assert_one_recovery() {
	test "$(cat /sys/module/kestrelfs/parameters/cache_journal_recoveries)" \
		-eq 1
}

cleanup() {
	set +e
	unmount_and_unload
	if [ -n "$loopdev" ]; then
		losetup -d "$loopdev" 2>/dev/null
	fi
	rm -f "$image" "$io_helper" "$txn_helper"
}
trap cleanup EXIT

rm -rf "$data_dir" "$mnt"
rm -f "$image" "$io_helper" "$txn_helper"
mkdir -p "$data_dir" "$mnt"
cc -O2 -Wall -Wextra -Werror test-step22-cache-io.c -o "$io_helper"
cc -O2 -Wall -Wextra -Werror test-step25-cache-txn.c -o "$txn_helper"
truncate -s 16M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
echo "STEP25_CACHE: loop_device=$loopdev namespace=$namespace"

# Format v4 and persist two independent cache entries in slots 0 and 1.
load_cache
start_daemon
busybox mount -t kestrelfs none "$mnt"
"$io_helper" prepare "$mnt/cache-a.dat" 4096
"$io_helper" prepare "$mnt/cache-b.dat" 4096
"$io_helper" verify "$mnt/cache-a.dat" 0 4096 0 1
"$io_helper" verify "$mnt/cache-b.dat" 0 4096 0 1
unmount_and_unload
dd if="$loopdev" of="$super_backup" bs=4096 count=1 status=none

# FILL crash after the new index reached disk: recovery must retire the slot.
"$txn_helper" prepare "$loopdev" fill 2 after
load_cache
assert_one_recovery
rmmod kestrelfs
"$txn_helper" check-zero "$loopdev" 2
echo "STEP25_FILL_RECOVERY_PASS"

# INVALIDATE crash before clearing the old index: A becomes a miss, B survives.
"$txn_helper" prepare "$loopdev" invalidate 0 before
load_cache
assert_one_recovery
start_daemon
busybox mount -t kestrelfs none "$mnt"
"$io_helper" verify "$mnt/cache-b.dat" 0 4096 0 1
test -f "$mnt/cache-a.dat"
stop_daemon
expect_miss "$mnt/cache-a.dat" invalidate-recovery-miss
"$io_helper" verify "$mnt/cache-b.dat" 0 4096 0 1
start_daemon
"$io_helper" verify "$mnt/cache-a.dat" 0 4096 0 1
unmount_and_unload
echo "STEP25_INVALIDATE_RECOVERY_PASS"

# EVICT crash after clearing the victim index: B remains retired, A survives.
"$txn_helper" prepare "$loopdev" evict 1 after
load_cache
assert_one_recovery
start_daemon
busybox mount -t kestrelfs none "$mnt"
"$io_helper" verify "$mnt/cache-a.dat" 0 4096 0 1
test -f "$mnt/cache-b.dat"
stop_daemon
expect_miss "$mnt/cache-b.dat" evict-recovery-miss
"$io_helper" verify "$mnt/cache-a.dat" 0 4096 0 1
start_daemon
"$io_helper" verify "$mnt/cache-b.dat" 0 4096 0 1
unmount_and_unload
echo "STEP25_EVICT_RECOVERY_PASS"

# A torn/non-checksummed journal is never interpreted or replayed.
"$txn_helper" prepare "$loopdev" invalidate 0 before
printf '\377' | dd of="$loopdev" bs=1 seek=$((journal_offset + 128)) \
	conv=notrunc status=none
sync
if load_cache; then
	echo "STEP25_FAIL: torn journal was accepted"
	exit 1
fi
test ! -d /sys/module/kestrelfs
dmesg >"$data_dir/torn-journal-dmesg.log"
grep -F 'invalid or torn cache journal' "$data_dir/torn-journal-dmesg.log"
dd if=/dev/zero of="$loopdev" bs=4096 seek=1 count=1 \
	conv=notrunc status=none
sync
echo "STEP25_TORN_FAIL_CLOSED_PASS"

# Previous v3 metadata is incompatible and rejected without migration/wipe.
printf '\003\000\000\000' | dd of="$loopdev" bs=1 seek=8 \
	conv=notrunc status=none
sync
if load_cache; then
	echo "STEP25_FAIL: cache format v3 was accepted"
	exit 1
fi
test ! -d /sys/module/kestrelfs
dmesg >"$data_dir/v3-rejection-dmesg.log"
grep -F 'cache superblock format or geometry mismatch' \
	"$data_dir/v3-rejection-dmesg.log"
dd if="$super_backup" of="$loopdev" bs=4096 count=1 \
	conv=notrunc status=none
sync
echo "STEP25_V3_REJECT_PASS"

# Superblock geometry is protected by CRC, including previously reserved bytes.
printf '\001' | dd of="$loopdev" bs=1 seek=128 conv=notrunc status=none
sync
if load_cache; then
	echo "STEP25_FAIL: corrupt superblock checksum was accepted"
	exit 1
fi
test ! -d /sys/module/kestrelfs
dmesg >"$data_dir/super-checksum-dmesg.log"
grep -F 'cache superblock format or geometry mismatch' \
	"$data_dir/super-checksum-dmesg.log"
dd if="$super_backup" of="$loopdev" bs=4096 count=1 \
	conv=notrunc status=none
sync
load_cache
rmmod kestrelfs
echo "STEP25_SUPER_CHECKSUM_PASS"

losetup -d "$loopdev"
loopdev=
trap - EXIT
rm -f "$image" "$io_helper" "$txn_helper"
echo "STEP25_CACHE: umount_ms=$last_umount_ms"
echo "STEP25_CACHE_TXN_PASS"
