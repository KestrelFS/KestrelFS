#!/bin/bash
# Step 52 TEST-PERF: coarse, repeatable vng+loop regression baseline.
set -euo pipefail
# shellcheck source=_repo_root.sh
# Keep cwd at repo root so daemon/ and kestrelfs/ relative paths stay stable.
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"

test_id=$$
image=/tmp/kestrel-step52-perf-$test_id.img
mnt=/tmp/mnt-kestrelfs-step52-perf-$test_id
data_dir=/tmp/kestrelfs-step52-perf-$test_id
helper=/tmp/kestrel-step52-perf-helper-$test_id
loopdev=
daemon_pid=
bytes=$((1024 * 1024))
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

report_error() {
	local status=$?
	echo "STEP52_PERF_FAIL: line=$1 status=$status"
	test ! -f "$data_dir/daemon.log" || tail -n 160 "$data_dir/daemon.log"
	dmesg | tail -n 140
	exit "$status"
}
trap 'report_error $LINENO' ERR

stop_daemon() {
	if test -n "$daemon_pid"; then
		kill "$daemon_pid"
		wait "$daemon_pid" || true
		daemon_pid=
	fi
}

start_daemon() {
	./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
		>"$data_dir/daemon.log" 2>&1 &
	daemon_pid=$!
	sleep 1
	kill -0 "$daemon_pid"
}

cleanup() {
	set +e
	mountpoint -q "$mnt" && busybox umount "$mnt"
	stop_daemon
	test -d /sys/module/kestrelfs && rmmod kestrelfs
	if test -n "$loopdev"; then losetup -d "$loopdev" 2>/dev/null; fi
	rm -f "$image" "$helper"
}
trap cleanup EXIT

mkdir -p "$data_dir" "$mnt"
cc -O2 -Wall -Wextra -Werror -std=gnu11 tests/test-step52-perf.c -o "$helper"
truncate -s 128M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"

echo "STEP52_PERF_ENV kernel=$(uname -r) cache=loop cache_size_mib=64 bytes=$bytes"
insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
	cache_namespace="$namespace"
start_daemon
busybox mount -t kestrelfs none "$mnt"
file=$mnt/perf.dat

# pwrite timing excludes fsync; the second number records the explicit
# durability point separately. This is a regression observation, not an SLA.
"$helper" write "$file" "$bytes"
echo STEP52_PERF_WRITE_BEHIND_PASS

# The file remains resident after write+fsync. Repeated reads measure the VFS
# page-cache hot path and include deterministic byte verification.
"$helper" read-loop "$file" "$bytes" 64 PAGECACHE
echo STEP52_PERF_PAGECACHE_PASS

# First cold read populates the kernel-owned block cache through READ_DATA.
# Drop page cache again, stop the daemon, and require the measured read to use
# asynchronous loop BIO cache hits rather than IPC.
sync
echo 3 >/proc/sys/vm/drop_caches
"$helper" read-loop "$file" "$bytes" 1 FILL
exec 8<"$file"
sync
echo 3 >/proc/sys/vm/drop_caches
async_counter=/sys/module/kestrelfs/parameters/cache_async_hit_submissions
before_async=$(cat "$async_counter")
stop_daemon
"$helper" read-loop-fd 8 "$bytes" 1 NVME
exec 8>&-
after_async=$(cat "$async_counter")
test "$after_async" -gt "$before_async"
echo "STEP52_PERF_NVME_ASYNC_BIOS=$((after_async - before_async))"
echo STEP52_PERF_NVME_PASS

start_daemon
rm "$file"
dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo 'STEP52_PERF_FAIL: kernel safety diagnostic found'
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
echo "STEP52_PERF: umount_ms=$umount_ms"
echo STEP52_PERF_PASS
