#!/bin/bash
# Step 44: fsync/fdatasync/syncfs durability barrier and daemon restart.
set -euo pipefail

test_id=$$
image=/tmp/kestrel-step44-$test_id.img
mnt=/tmp/mnt-kestrelfs-step44-$test_id
data_dir=/tmp/kestrelfs-step44-$test_id
helper=/tmp/kestrel-step44-fsync-$test_id
loopdev=
daemon_pid=
last_umount_ms=0
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

report_error() {
	local status=$?
	echo "STEP44_FAIL: line=$1 status=$status"
	if test -f "$data_dir/daemon.log"; then
		tail -n 100 "$data_dir/daemon.log"
	fi
	dmesg | tail -n 100
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
		kill -9 "$daemon_pid"
		wait "$daemon_pid" || true
		daemon_pid=
	fi
}

unmount_and_unload() {
	if mountpoint -q "$mnt"; then
		local start_ns end_ns
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

mkdir -p "$data_dir" "$mnt"
cc -O2 -Wall -Wextra -Werror -std=gnu11 test-step44-fsync.c -o "$helper"
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
"$helper" write "$mnt/sync-data"
"$helper" write-global "$mnt/syncfs-only"

# Kill without unmounting: prove the fsync callback cannot falsely ACK.
stop_daemon
"$helper" offline "$mnt/sync-data"
start_daemon
"$helper" verify "$mnt/sync-data"
"$helper" verify "$mnt/syncfs-only"

dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo 'STEP44_FAIL: kernel diagnostic'
	exit 1
fi
unmount_and_unload
losetup -d "$loopdev"
loopdev=
trap - EXIT
rm -f "$image" "$helper"
echo "STEP44_FSYNC: umount_ms=$last_umount_ms"
echo STEP44_KERNEL_FSYNC_PASS
