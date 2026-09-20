#!/bin/bash
# Step 49: buffered folio writeback, fsync/restart and visible failure.
set -euo pipefail

test_id=$$
image=/tmp/kestrel-step49-$test_id.img
mnt=/tmp/mnt-kestrelfs-step49-$test_id
data_dir=/tmp/kestrelfs-step49-$test_id
helper=/tmp/kestrel-step49-helper-$test_id
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

fail() {
	local status=$?
	echo "STEP49_FAIL: line=$1 status=$status"
	test ! -f "$data_dir/daemon.log" || tail -n 100 "$data_dir/daemon.log"
	dmesg | tail -n 80
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
		8>&- 9<&- >"$data_dir/daemon.log" 2>&1 &
	daemon_pid=$!
	sleep 1
	kill -0 "$daemon_pid"
}

cleanup() {
	set +e
	if mountpoint -q "$mnt"; then busybox umount "$mnt"; fi
	stop_daemon
	if test -d /sys/module/kestrelfs; then rmmod kestrelfs; fi
	if test -n "$loopdev"; then losetup -d "$loopdev"; fi
	rm -f "$image" "$helper"
}
trap cleanup EXIT

mkdir -p "$data_dir" "$mnt"
cc -O2 -Wall -Wextra -Werror -std=gnu11 \
	test-step49-cache-write.c -o "$helper"
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

file="$mnt/writeback.dat"
"$helper" write-fsync "$file"
grep -q 'OP_WRITE_DATA' "$data_dir/daemon.log"
echo STEP49_BUFFERED_FSYNC_PASS

# The filemap folios remain clean and readable after WRITE_DATA, without the
# daemon or an NVMe read-cache refill.
exec 9<"$file"
stop_daemon
"$helper" verify-fd 9
echo STEP49_PAGECACHE_RETAIN_PASS
exec 9<&-

start_daemon
error_file="$mnt/failure.dat"
: >"$error_file"
exec 8<>"$error_file"
stop_daemon
"$helper" fail-write-fd 8
echo STEP49_WRITEBACK_ERROR_PASS
start_daemon
# Retry the dirty folio after daemon recovery. The first fsync can report the
# previous errseq; the second must succeed once data has been committed.
"$helper" fsync-fd 8 || true
"$helper" fsync-fd 8
exec 8>&-
"$helper" verify-failure "$error_file"
echo STEP49_RETRY_PASS

start_ns=$(date +%s%N)
busybox umount "$mnt"
end_ns=$(date +%s%N)
umount_ms=$(((end_ns - start_ns) / 1000000))
test "$umount_ms" -lt 1000
stop_daemon
start_daemon
busybox mount -t kestrelfs none "$mnt"
"$helper" verify "$file"
"$helper" verify-failure "$error_file"
echo STEP49_RESTART_READBACK_PASS
busybox umount "$mnt"
stop_daemon
rmmod kestrelfs
losetup -d "$loopdev"
loopdev=
trap - EXIT
rm -f "$image" "$helper"
echo "STEP49_CACHE_WRITE: umount_ms=$umount_ms"
echo STEP49_CACHE_WRITE_PASS
