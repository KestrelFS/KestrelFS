#!/bin/bash
# Step 58: FileMetaStore disk preparation overlaps for different write inodes.
# Run only inside a vng guest; cache_device is a guest loop device.
set -euo pipefail
# shellcheck source=_repo_root.sh
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"

test_id=$$
image=/tmp/kestrel-step58-$test_id.img
mnt=/tmp/mnt-kestrelfs-step58-$test_id
data_dir=/tmp/kestrelfs-step58-$test_id
helper=/tmp/kestrel-step58-helper-$test_id
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

start_daemon() {
	./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
		>"$data_dir/daemon.log" 2>&1 &
	daemon_pid=$!
	sleep 0.5
	kill -0 "$daemon_pid"
}

stop_daemon() {
	if test -n "$daemon_pid"; then
		kill "$daemon_pid" 2>/dev/null || true
		wait "$daemon_pid" 2>/dev/null || true
		daemon_pid=
	fi
}

fail() {
	local status=$?
	echo "STEP58_FAIL: line=$1 status=$status"
	test ! -f "$data_dir/daemon.log" || tail -n 180 "$data_dir/daemon.log"
	dmesg | tail -n 160
	exit "$status"
}
trap 'fail $LINENO' ERR

cleanup() {
	set +e
	mountpoint -q "$mnt" && busybox umount "$mnt"
	stop_daemon
	test -d /sys/module/kestrelfs && rmmod kestrelfs
	test -z "$loopdev" || losetup -d "$loopdev" 2>/dev/null
	rm -f "$image" "$helper"
}
trap cleanup EXIT

rm -rf "$data_dir" "$mnt"
mkdir -p "$data_dir" "$mnt"
cc -O2 -Wall -Wextra -Werror -std=gnu11 -pthread -DREGION_SIZE=524288U \
	tests/test-step57-write-parallel.c -o "$helper"
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

timeout 60 "$helper" exercise "$mnt"
lane_peak=$(cat /sys/module/kestrelfs/parameters/write_data_parallel_peak)
lane_active=$(cat /sys/module/kestrelfs/parameters/write_data_parallel_active)
test "$lane_peak" -ge 2
test "$lane_active" -eq 0
grep -Eq 'META-PERSIST parallel active=[2-8] peak=[2-8]' "$data_dir/daemon.log"
grep -Eq 'WRITE-DATA-PARALLEL batch=[2-8]' "$data_dir/daemon.log"
echo "STEP58_META_PARALLEL_OVERLAP_PASS lane_peak=$lane_peak"
echo STEP58_META_PARALLEL_DATA_PASS
echo STEP58_META_SAME_INODE_ORDER_PASS

busybox umount "$mnt"
stop_daemon
start_daemon
busybox mount -t kestrelfs none "$mnt"
timeout 60 "$helper" verify "$mnt"
echo STEP58_META_PARALLEL_DURABILITY_PASS

rm "$mnt/parallel-a" "$mnt/parallel-b" "$mnt/ordered-shared"
start_ns=$(date +%s%N)
busybox umount "$mnt"
end_ns=$(date +%s%N)
umount_ms=$(((end_ns - start_ns) / 1000000))
test "$umount_ms" -lt 1000
stop_daemon
rmmod kestrelfs
losetup -d "$loopdev"
loopdev=

dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo STEP58_FAIL_KERNEL_DIAGNOSTIC
	exit 1
fi
trap - EXIT
rm -f "$image" "$helper"
echo "STEP58_META_PARALLEL: lane_peak=$lane_peak umount_ms=$umount_ms"
echo STEP58_META_PARALLEL_PASS
