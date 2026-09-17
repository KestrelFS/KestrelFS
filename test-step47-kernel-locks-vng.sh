#!/bin/bash
# Step 47: local flock/POSIX/OFD contention, blocking wakeup, exit cleanup.
set -euo pipefail

test_id=$$
image=/tmp/kestrel-step47-$test_id.img
mnt=/tmp/mnt-kestrelfs-step47-$test_id
data_dir=/tmp/kestrelfs-step47-$test_id
helper=/tmp/kestrel-step47-locks-$test_id
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

fail() {
	local status=$?
	echo "STEP47_FAIL: line=$1 status=$status"
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
	test-step47-kernel-locks.c -o "$helper"
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
./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
	>"$data_dir/daemon.log" 2>&1 &
daemon_pid=$!
sleep 1
kill -0 "$daemon_pid"
busybox mount -t kestrelfs none "$mnt"
"$helper" "$mnt/locks.dat"

dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo 'STEP47_FAIL: kernel diagnostic'
	exit 1
fi
start_ns=$(date +%s%N)
busybox umount "$mnt"
end_ns=$(date +%s%N)
umount_ms=$(((end_ns - start_ns) / 1000000))
test "$umount_ms" -lt 1000
stop_daemon
rmmod kestrelfs
losetup -d "$loopdev"
loopdev=
trap - EXIT
rm -f "$image" "$helper"
echo "STEP47_KERNEL_LOCKS: umount_ms=$umount_ms"
echo STEP47_KERNEL_LOCKS_PASS
