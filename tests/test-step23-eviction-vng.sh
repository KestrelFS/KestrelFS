#!/bin/bash
# Phase 4 Step 23 block-LRU eviction and persistent slot-reuse test.
set -euo pipefail
# shellcheck source=_repo_root.sh
# Keep cwd at repo root so daemon/ and kestrelfs/ relative paths stay stable.
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"

test_id=$$
image=/tmp/kestrel-step23-cache-$test_id.img
mnt=/tmp/mnt-kestrelfs-step23-$test_id
data_dir=/tmp/kestrelfs-step23-$test_id
helper=/tmp/kestrel-step23-cache-io-$test_id
file_a_size=$((1024 * 1024))
file_b_size=$((128 * 1024))
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
	insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=3 \
		cache_namespace="$namespace"
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

expect_evicted_miss() {
	local label=$1
	local offset=$2

	if "$helper" verify "$mnt/cache-a.dat" "$offset" 4096 0 1 \
		>"$data_dir/$label.log" 2>&1; then
		echo "STEP23_FAIL: evicted offset $offset remained readable"
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
cc -O2 -Wall -Wextra -Werror tests/test-step22-cache-io.c -o "$helper"
truncate -s 16M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
echo "STEP23_CACHE: loop_device=$loopdev namespace=$namespace"

load_cache
start_daemon
busybox mount -t kestrelfs none "$mnt"

# A 3 MiB usable cache has 2 MiB metadata + exactly 256 data slots.
"$helper" prepare "$mnt/cache-a.dat" "$file_a_size"
"$helper" verify "$mnt/cache-a.dat" 0 "$file_a_size" 0 1
test "$(cat /sys/module/kestrelfs/parameters/cache_evictions)" -eq 0

# Promote A[0] to the MRU end. B's 32 fills must evict A[1]..A[32].
"$helper" verify "$mnt/cache-a.dat" 0 4096 0 1
"$helper" prepare "$mnt/cache-b.dat" "$file_b_size"
"$helper" verify "$mnt/cache-b.dat" 0 "$file_b_size" 0 1
"$helper" verify "$mnt/cache-b.dat" 0 "$file_b_size" 0 1
evictions=$(cat /sys/module/kestrelfs/parameters/cache_evictions)
test "$evictions" -eq 32
test "$(cat /sys/module/kestrelfs/parameters/cache_direct_hit_blocks)" -gt 0
echo "STEP23_CACHE: full-cache fill recycled evictions=$evictions"

# With no daemon, new B and promoted A[0]/A[last] hit; evicted A[1] misses.
stop_daemon
"$helper" verify "$mnt/cache-b.dat" 0 "$file_b_size" 0 1
"$helper" verify "$mnt/cache-a.dat" 0 4096 0 1
"$helper" verify "$mnt/cache-a.dat" $((file_a_size - 4096)) 4096 0 1
expect_evicted_miss evicted-before-reload 4096
echo "STEP23_CACHE: daemon-stopped LRU hit/miss boundary passed"

busybox umount "$mnt"
rmmod kestrelfs

# Reuse the current-format device: cleared victims and replacements persist.
load_cache
start_daemon
busybox mount -t kestrelfs none "$mnt"
"$helper" verify "$mnt/cache-b.dat" 0 "$file_b_size" 0 1
"$helper" verify "$mnt/cache-a.dat" 0 4096 0 1
stop_daemon
"$helper" verify "$mnt/cache-b.dat" 0 "$file_b_size" 0 1
"$helper" verify "$mnt/cache-a.dat" 0 4096 0 1
expect_evicted_miss evicted-after-reload 4096
dmesg >"$data_dir/dmesg.log"
grep -E 'reusing cache device=.*|restored 256 cache index entries' \
	"$data_dir/dmesg.log"
echo "STEP23_CACHE: replacement index survived reload"

start_ns=$(date +%s%N)
busybox umount "$mnt"
end_ns=$(date +%s%N)
umount_ms=$(( (end_ns - start_ns) / 1000000 ))
test "$umount_ms" -lt 1000
rmmod kestrelfs

losetup -d "$loopdev"
loopdev=
trap - EXIT
rm -f "$image" "$helper"
echo "STEP23_CACHE: umount_ms=$umount_ms"
echo "STEP23_EVICTION_PASS"
