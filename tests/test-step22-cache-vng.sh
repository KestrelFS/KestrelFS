#!/bin/bash
# Phase 4 Step 22 pinned-user-page cache-hit and A/B performance test.
set -euo pipefail
# shellcheck source=_repo_root.sh
# Keep cwd at repo root so daemon/ and kestrelfs/ relative paths stay stable.
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"

test_id=$$
image=/tmp/kestrel-step22-cache-$test_id.img
mnt=/tmp/mnt-kestrelfs-step22-$test_id
data_dir=/tmp/kestrelfs-step22-$test_id
helper=/tmp/kestrel-step22-cache-io-$test_id
payload_size=$((1024 * 1024 + 123))
benchmark_size=$((1024 * 1024))
benchmark_iterations=64
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

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
	local direct=$1

	insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
		cache_namespace="$namespace" cache_direct_io="$direct"
}

unmount_and_unload() {
	if mountpoint -q "$mnt"; then
		busybox umount "$mnt"
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
cc -O2 -Wall -Wextra -Werror tests/test-step22-cache-io.c -o "$helper"
truncate -s 128M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
echo "STEP22_CACHE: loop_device=$loopdev namespace=$namespace"

# Format, populate from READ_DATA miss, then exercise direct and partial paths.
load_cache 1
start_daemon
busybox mount -t kestrelfs none "$mnt"
"$helper" prepare "$mnt/direct-hit.dat" "$payload_size"
"$helper" verify "$mnt/direct-hit.dat" 0 "$payload_size" 0 1
"$helper" verify "$mnt/direct-hit.dat" 0 "$payload_size" 0 1
"$helper" verify "$mnt/direct-hit.dat" 1 $((payload_size + 4096)) 1 1
direct_blocks=$(cat /sys/module/kestrelfs/parameters/cache_direct_hit_blocks)
copied_blocks=$(cat /sys/module/kestrelfs/parameters/cache_copy_hit_blocks)
test "$direct_blocks" -gt 0
test "$copied_blocks" -gt 0
echo "STEP22_CACHE: aligned/partial/unaligned direct_blocks=$direct_blocks copied_blocks=$copied_blocks"

# The hot path bypasses the daemon after dentries and persistent blocks exist.
stop_daemon
"$helper" verify "$mnt/direct-hit.dat" 0 "$benchmark_size" 0 1
echo "STEP22_CACHE: daemon-stopped direct hit passed"
unmount_and_unload

# Reload the same current-format device and time the Step 20 buffered-copy path.
load_cache 0
start_daemon
busybox mount -t kestrelfs none "$mnt"
"$helper" verify "$mnt/direct-hit.dat" 0 "$benchmark_size" 0 1 >/dev/null
copy_result=$("$helper" verify "$mnt/direct-hit.dat" 0 "$benchmark_size" 0 \
	"$benchmark_iterations")
copy_ns=$(printf '%s\n' "$copy_result" | sed -n 's/.*elapsed_ns=\([0-9][0-9]*\).*/\1/p')
test -n "$copy_ns"
test "$(cat /sys/module/kestrelfs/parameters/cache_direct_hit_blocks)" -eq 0
test "$(cat /sys/module/kestrelfs/parameters/cache_copy_hit_blocks)" -gt 0
unmount_and_unload

# Reload once more with Step 22 enabled and run the identical aligned workload.
load_cache 1
start_daemon
busybox mount -t kestrelfs none "$mnt"
"$helper" verify "$mnt/direct-hit.dat" 0 "$benchmark_size" 0 1 >/dev/null
direct_result=$("$helper" verify "$mnt/direct-hit.dat" 0 "$benchmark_size" 0 \
	"$benchmark_iterations")
direct_ns=$(printf '%s\n' "$direct_result" | sed -n 's/.*elapsed_ns=\([0-9][0-9]*\).*/\1/p')
test -n "$direct_ns"
test "$(cat /sys/module/kestrelfs/parameters/cache_direct_hit_blocks)" -gt 0

start_ns=$(date +%s%N)
busybox umount "$mnt"
end_ns=$(date +%s%N)
umount_ms=$(( (end_ns - start_ns) / 1000000 ))
test "$umount_ms" -lt 1000
stop_daemon
rmmod kestrelfs

echo "STEP22_PERF: copy {$copy_result}"
echo "STEP22_PERF: direct {$direct_result}"
echo "STEP22_PERF: copy_ns=$copy_ns direct_ns=$direct_ns workload=${benchmark_size}B_x${benchmark_iterations}"
echo "STEP22_CACHE: umount_ms=$umount_ms"

losetup -d "$loopdev"
loopdev=
trap - EXIT
rm -f "$image" "$helper"
echo "STEP22_CACHE_HIT_PASS"
