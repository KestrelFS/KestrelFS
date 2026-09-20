#!/bin/bash
# Step 38 POSIX-EXCHANGE: atomic RENAME_EXCHANGE across files/directories.
set -euo pipefail

test_id=$$
image=/tmp/kestrel-step38-cache-$test_id.img
mnt=/tmp/mnt-kestrelfs-step38-$test_id
data_dir=/tmp/kestrelfs-step38-$test_id
helper=/tmp/kestrel-step38-renameat2-$test_id
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

report_error() {
	local status=$?
	echo "STEP38_FAIL: line=$1 status=$status"
	test -f "$data_dir/daemon.log" && tail -n 100 "$data_dir/daemon.log"
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
	if test -n "$daemon_pid"; then
		kill "$daemon_pid"
		wait "$daemon_pid" || true
		daemon_pid=
	fi
}

objects() {
	find "$data_dir" -type f ! -name meta.json ! -name daemon.log \
		! -name '*.err' ! -name dmesg.log | sort
}

expect_errno() {
	local expected=$1
	local old_path=$2
	local new_path=$3
	local flags=$4
	local error_file=$5
	local status
	if "$helper" "$old_path" "$new_path" "$flags" 2>"$error_file"; then
		echo "STEP38_FAIL: renameat2 flags=$flags unexpectedly succeeded"
		return 1
	else
		status=$?
	fi
	test "$status" -eq "$expected"
}

cleanup() {
	set +e
	mountpoint -q "$mnt" && busybox umount "$mnt"
	stop_daemon
	test -d /sys/module/kestrelfs && rmmod kestrelfs
	if test -n "$loopdev"; then
		losetup -d "$loopdev" 2>/dev/null
	fi
	rm -f "$image" "$helper"
}
trap cleanup EXIT

rm -rf "$data_dir" "$mnt"
rm -f "$image" "$helper"
mkdir -p "$data_dir" "$mnt"
cc -O2 -Wall -Wextra -Werror test-step33-renameat2.c -o "$helper"
truncate -s 128M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
echo "STEP38_POSIX_EXCHANGE: loop_device=$loopdev namespace=$namespace"

insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
	cache_namespace="$namespace"
start_daemon
busybox mount -t kestrelfs none "$mnt"

# File <-> file: names, inode identities and data swap; no object is reclaimed.
printf 'step38-left-data\n' >"$mnt/file-left-long-name"
printf 'step38-right-data\n' >"$mnt/file-right-long-name"
left_inode=$(stat -c %i "$mnt/file-left-long-name")
right_inode=$(stat -c %i "$mnt/file-right-long-name")
objects_before=$(objects)
"$helper" "$mnt/file-left-long-name" "$mnt/file-right-long-name" 2
test "$(stat -c %i "$mnt/file-left-long-name")" = "$right_inode"
test "$(stat -c %i "$mnt/file-right-long-name")" = "$left_inode"
grep -Fx 'step38-right-data' "$mnt/file-left-long-name"
grep -Fx 'step38-left-data' "$mnt/file-right-long-name"
test "$(objects)" = "$objects_before"
echo 'STEP38_FILE_EXCHANGE_PASS'

# Fill two complete cache ranges, exchange their names, then stop the daemon.
# A pure namespace swap must retain both inode-keyed cache entries.
head -c 8192 /dev/zero | tr '\000' A >"$mnt/cache-left"
head -c 8192 /dev/zero | tr '\000' B >"$mnt/cache-right"
cache_left_hash=$(sha256sum "$mnt/cache-left" | awk '{print $1}')
cache_right_hash=$(sha256sum "$mnt/cache-right" | awk '{print $1}')
"$helper" "$mnt/cache-left" "$mnt/cache-right" 2
stop_daemon
test "$(sha256sum "$mnt/cache-left" | awk '{print $1}')" = "$cache_right_hash"
test "$(sha256sum "$mnt/cache-right" | awk '{print $1}')" = "$cache_left_hash"
start_daemon
echo 'STEP38_CACHE_IDENTITY_PASS'

# Directory <-> directory is allowed even when both are non-empty.
mkdir "$mnt/parent-left" "$mnt/parent-right"
mkdir "$mnt/parent-left/dir-left" "$mnt/parent-right/dir-right"
printf 'left-child\n' >"$mnt/parent-left/dir-left/left-child"
printf 'right-child\n' >"$mnt/parent-right/dir-right/right-child"
left_dir_inode=$(stat -c %i "$mnt/parent-left/dir-left")
right_dir_inode=$(stat -c %i "$mnt/parent-right/dir-right")
"$helper" "$mnt/parent-left/dir-left" "$mnt/parent-right/dir-right" 2
test "$(stat -c %i "$mnt/parent-left/dir-left")" = "$right_dir_inode"
test "$(stat -c %i "$mnt/parent-right/dir-right")" = "$left_dir_inode"
grep -Fx 'right-child' "$mnt/parent-left/dir-left/right-child"
grep -Fx 'left-child' "$mnt/parent-right/dir-right/left-child"
test "$(stat -c %h "$mnt/parent-left")" -eq 3
test "$(stat -c %h "$mnt/parent-right")" -eq 3
echo 'STEP38_DIRECTORY_EXCHANGE_PASS'

# Linux permits unlike types under EXCHANGE. Parent nlink follows which side
# owns the immediate subdirectory after the atomic swap.
printf 'mixed-file-data\n' >"$mnt/parent-left/mixed"
mkdir "$mnt/parent-right/mixed"
printf 'mixed-dir-child\n' >"$mnt/parent-right/mixed/child"
mixed_file_inode=$(stat -c %i "$mnt/parent-left/mixed")
mixed_dir_inode=$(stat -c %i "$mnt/parent-right/mixed")
"$helper" "$mnt/parent-left/mixed" "$mnt/parent-right/mixed" 2
test -d "$mnt/parent-left/mixed"
test -f "$mnt/parent-right/mixed"
test "$(stat -c %i "$mnt/parent-left/mixed")" = "$mixed_dir_inode"
test "$(stat -c %i "$mnt/parent-right/mixed")" = "$mixed_file_inode"
grep -Fx 'mixed-dir-child' "$mnt/parent-left/mixed/child"
grep -Fx 'mixed-file-data' "$mnt/parent-right/mixed"
test "$(stat -c %h "$mnt/parent-left")" -eq 4
test "$(stat -c %h "$mnt/parent-right")" -eq 3
echo 'STEP38_MIXED_TYPE_EXCHANGE_PASS'

# A hard-linked inode keeps both references and its cache/object identity.
printf 'hard-left-data\n' >"$mnt/hard-left"
ln "$mnt/hard-left" "$mnt/hard-alias"
printf 'hard-right-data\n' >"$mnt/hard-right"
hard_left_inode=$(stat -c %i "$mnt/hard-left")
hard_right_inode=$(stat -c %i "$mnt/hard-right")
"$helper" "$mnt/hard-left" "$mnt/hard-right" 2
test "$(stat -c %i "$mnt/hard-left")" = "$hard_right_inode"
test "$(stat -c %i "$mnt/hard-right")" = "$hard_left_inode"
test "$(stat -c %i "$mnt/hard-alias")" = "$hard_left_inode"
test "$(stat -c %h "$mnt/hard-right")" -eq 2
grep -Fx 'hard-left-data' "$mnt/hard-alias"
echo 'STEP38_HARDLINK_EXCHANGE_PASS'

# Missing target, mutually exclusive EXCHANGE combinations, unknown flags and
# ancestry cycles must fail before either dirent changes. WHITEOUT itself is
# covered by the Step 50 script.
expect_errno 2 "$mnt/file-left-long-name" "$mnt/missing" 2 \
	"$data_dir/missing.err"
expect_errno 22 "$mnt/file-left-long-name" "$mnt/file-right-long-name" 3 \
	"$data_dir/conflicting-flags.err"
expect_errno 22 "$mnt/file-left-long-name" "$mnt/file-right-long-name" 6 \
	"$data_dir/exchange-whiteout.err"
expect_errno 22 "$mnt/file-left-long-name" "$mnt/file-right-long-name" 8 \
	"$data_dir/unknown-flags.err"
expect_errno 22 "$mnt/parent-left" "$mnt/parent-left/dir-left" 2 \
	"$data_dir/cycle.err"
test "$(stat -c %i "$mnt/file-left-long-name")" = "$right_inode"
test "$(stat -c %i "$mnt/file-right-long-name")" = "$left_inode"
test -d "$mnt/parent-left/dir-left"
grep -Fx 'step38-right-data' "$mnt/file-left-long-name"
grep -Fx 'step38-left-data' "$mnt/file-right-long-name"
echo 'STEP38_FAILURE_ATOMICITY_PASS'

# FileMetaStore restart keeps the complete exchanged namespace.
busybox umount "$mnt"
stop_daemon
start_daemon
busybox mount -t kestrelfs none "$mnt"
grep -Fx 'step38-right-data' "$mnt/file-left-long-name"
grep -Fx 'step38-left-data' "$mnt/file-right-long-name"
grep -Fx 'right-child' "$mnt/parent-left/dir-left/right-child"
grep -Fx 'left-child' "$mnt/parent-right/dir-right/left-child"
grep -Fx 'mixed-dir-child' "$mnt/parent-left/mixed/child"
grep -Fx 'mixed-file-data' "$mnt/parent-right/mixed"
test "$(stat -c %h "$mnt/hard-right")" -eq 2
echo 'STEP38_EXCHANGE_RESTART_PASS'

dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo 'STEP38_FAIL: kernel safety diagnostic found'
	exit 1
fi

start_ns=$(date +%s%N)
busybox umount "$mnt"
end_ns=$(date +%s%N)
umount_ms=$(( (end_ns - start_ns) / 1000000 ))
test "$umount_ms" -lt 1000
stop_daemon
rmmod kestrelfs
losetup -d "$loopdev"
loopdev=
trap - EXIT
rm -f "$image" "$helper"
echo "STEP38_POSIX_EXCHANGE: umount_ms=$umount_ms"
echo 'STEP38_POSIX_EXCHANGE_PASS'
