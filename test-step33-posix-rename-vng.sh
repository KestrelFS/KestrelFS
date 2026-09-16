#!/bin/bash
# Step 33 POSIX-RENAME: RENAME_NOREPLACE success, EEXIST atomicity, hard links.
set -euo pipefail

test_id=$$
image=/tmp/kestrel-step33-cache-$test_id.img
mnt=/tmp/mnt-kestrelfs-step33-$test_id
data_dir=/tmp/kestrelfs-step33-$test_id
helper=/tmp/kestrel-step33-renameat2-$test_id
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

report_error() {
	local status=$?
	echo "STEP33_FAIL: line=$1 status=$status"
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
	find "$data_dir" -type f ! -name meta.json ! -name daemon.log ! -name dmesg.log | sort
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
echo "STEP33_POSIX_RENAME: loop_device=$loopdev namespace=$namespace"

insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
	cache_namespace="$namespace"
start_daemon
busybox mount -t kestrelfs none "$mnt"

# Absent destination: NOREPLACE performs a normal atomic rename.
printf 'step33-success-data\n' >"$mnt/success-source-long-name"
"$helper" "$mnt/success-source-long-name" "$mnt/success-target-long-name" 1
test ! -e "$mnt/success-source-long-name"
grep -Fx 'step33-success-data' "$mnt/success-target-long-name"
echo 'STEP33_NOREPLACE_SUCCESS_PASS'

# Existing destination: exact EEXIST, both names/data and both object keys remain.
printf 'step33-source-preserved\n' >"$mnt/collision-source-long-name"
printf 'step33-target-preserved\n' >"$mnt/collision-target-long-name"
mapfile -t collision_objects < <(objects)
if "$helper" "$mnt/collision-source-long-name" "$mnt/collision-target-long-name" 1 \
	2>"$data_dir/noreplace.err"; then
	echo 'STEP33_FAIL: NOREPLACE overwrote an existing destination'
	exit 1
else
	status=$?
fi
test "$status" -eq 17
grep -F 'File exists' "$data_dir/noreplace.err"
grep -Fx 'step33-source-preserved' "$mnt/collision-source-long-name"
grep -Fx 'step33-target-preserved' "$mnt/collision-target-long-name"
for object_path in "${collision_objects[@]}"; do
	test -f "$object_path"
done
echo 'STEP33_NOREPLACE_EEXIST_ATOMIC_PASS'

# Linux VFS applies NOREPLACE's existence check before the filesystem callback,
# so two hard-link aliases return EEXIST at syscall level. Verify that this
# VFS-level result is still a namespace/data no-op; MetaStore's backend-level
# same-inode success rule is covered by Rust tests.
printf 'step33-hardlink-data\n' >"$mnt/hardlink-source-long-name"
ln "$mnt/hardlink-source-long-name" "$mnt/hardlink-alias-long-name"
if "$helper" "$mnt/hardlink-source-long-name" "$mnt/hardlink-alias-long-name" 1 \
	2>"$data_dir/hardlink-noreplace.err"; then
	echo 'STEP33_FAIL: VFS unexpectedly bypassed NOREPLACE existence check'
	exit 1
else
	status=$?
fi
test "$status" -eq 17
test "$(stat -c %i "$mnt/hardlink-source-long-name")" = \
	"$(stat -c %i "$mnt/hardlink-alias-long-name")"
test "$(stat -c %h "$mnt/hardlink-source-long-name")" -eq 2
grep -Fx 'step33-hardlink-data' "$mnt/hardlink-alias-long-name"
echo 'STEP33_NOREPLACE_HARDLINK_VFS_EEXIST_NOOP_PASS'

# Unsupported WHITEOUT is rejected, and neither namespace entry changes.
if "$helper" "$mnt/collision-source-long-name" "$mnt/collision-target-long-name" 4 \
	2>"$data_dir/whiteout.err"; then
	echo 'STEP33_FAIL: unsupported WHITEOUT unexpectedly succeeded'
	exit 1
else
	status=$?
fi
test "$status" -eq 22
grep -Fx 'step33-source-preserved' "$mnt/collision-source-long-name"
grep -Fx 'step33-target-preserved' "$mnt/collision-target-long-name"
echo 'STEP33_UNSUPPORTED_FLAGS_PASS'

# FileMetaStore recovery preserves both successful and rejected operations.
busybox umount "$mnt"
stop_daemon
start_daemon
busybox mount -t kestrelfs none "$mnt"
test ! -e "$mnt/success-source-long-name"
grep -Fx 'step33-success-data' "$mnt/success-target-long-name"
grep -Fx 'step33-source-preserved' "$mnt/collision-source-long-name"
grep -Fx 'step33-target-preserved' "$mnt/collision-target-long-name"
test "$(stat -c %h "$mnt/hardlink-source-long-name")" -eq 2
echo 'STEP33_NOREPLACE_RESTART_PASS'

dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo 'STEP33_FAIL: kernel safety diagnostic found'
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
echo "STEP33_POSIX_RENAME: umount_ms=$umount_ms"
echo 'STEP33_POSIX_RENAME_PASS'
