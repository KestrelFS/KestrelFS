#!/bin/bash
# Phase 4 Step 29 batched LRU eviction and persistent slot-reuse test.
set -euo pipefail
# shellcheck source=_repo_root.sh
# Keep cwd at repo root so daemon/ and kestrelfs/ relative paths stay stable.
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"

test_id=$$
image=/tmp/kestrel-step29-cache-$test_id.img
mnt=/tmp/mnt-kestrelfs-step29-$test_id
data_dir=/tmp/kestrelfs-step29-$test_id
helper=/tmp/kestrel-step29-cache-io-$test_id
txn_helper=/tmp/kestrel-step29-cache-txn-$test_id
file_a_size=$((1024 * 1024))
file_b_size=$((16 * 4096))
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

start_daemon() {
	./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
		>"$data_dir/daemon.log" 2>&1 &
	daemon_pid=$!
	sleep 2
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
		cache_namespace="$namespace" cache_evict_batch=16
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

expect_cache_miss() {
	local label=$1
	local path=$2
	local offset=$3

	if "$helper" verify "$path" "$offset" 4096 0 1 \
		>"$data_dir/$label.log" 2>&1; then
		echo "STEP29_FAIL: cache miss expected for $path offset $offset"
		exit 1
	fi
}

cleanup() {
	set +e
	unmount_and_unload
	if [ -n "$loopdev" ]; then
		losetup -d "$loopdev" 2>/dev/null
	fi
	rm -f "$image" "$helper" "$txn_helper"
}
trap cleanup EXIT

rm -rf "$data_dir" "$mnt"
rm -f "$image" "$helper" "$txn_helper"
mkdir -p "$data_dir" "$mnt"
cc -O2 -Wall -Wextra -Werror tests/test-step22-cache-io.c -o "$helper"
cc -O2 -Wall -Wextra -Werror tests/test-step25-cache-txn.c -o "$txn_helper"
truncate -s 16M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
echo "STEP29_CACHE: loop_device=$loopdev namespace=$namespace"

load_cache
start_daemon
busybox mount -t kestrelfs none "$mnt"

# Fill all 256 slots, then protect A[0] by moving it to the MRU tail.
"$helper" prepare "$mnt/cache-a.dat" "$file_a_size"
"$helper" verify "$mnt/cache-a.dat" 0 "$file_a_size" 0 1
"$helper" verify "$mnt/cache-a.dat" 0 4096 0 1
test "$(cat /sys/module/kestrelfs/parameters/cache_evictions)" -eq 0

# The first B miss retires 16 LRU entries in one journal transaction. Its
# contiguous slots occupy one index page, so 16 clears require one page write.
"$helper" prepare "$mnt/cache-b.dat" "$file_b_size"
"$helper" verify "$mnt/cache-b.dat" 0 "$file_b_size" 0 1
test "$(cat /sys/module/kestrelfs/parameters/cache_evictions)" -eq 16
test "$(cat /sys/module/kestrelfs/parameters/cache_eviction_batches)" -eq 1
test "$(cat /sys/module/kestrelfs/parameters/cache_eviction_batch_slots)" -eq 16
test "$(cat /sys/module/kestrelfs/parameters/cache_eviction_index_writes)" -eq 1
echo "STEP29_BATCH_EVICTION_PASS victims=16 index_writes=1"

# No daemon: replacements and MRU-protected A[0] hit; the oldest A[1] misses.
stop_daemon
"$helper" verify "$mnt/cache-b.dat" 0 "$file_b_size" 0 1
"$helper" verify "$mnt/cache-a.dat" 0 4096 0 1
"$helper" verify "$mnt/cache-a.dat" $((file_a_size - 4096)) 4096 0 1
expect_cache_miss evicted-before-reload "$mnt/cache-a.dat" 4096
echo "STEP29_MRU_PROTECTION_PASS"

busybox umount "$mnt"
rmmod kestrelfs

# Reuse format v4 and prove the cleared victims/replacements survived reload.
load_cache
start_daemon
busybox mount -t kestrelfs none "$mnt"
# Warm VFS dentries while metadata service is available; the following reads
# then prove data-cache hits independently after the daemon is stopped.
echo "STEP29_CACHE: warming reload dentries"
"$helper" verify "$mnt/cache-b.dat" 0 "$file_b_size" 0 1
"$helper" verify "$mnt/cache-a.dat" 0 4096 0 1
stop_daemon
"$helper" verify "$mnt/cache-b.dat" 0 "$file_b_size" 0 1
"$helper" verify "$mnt/cache-a.dat" 0 4096 0 1
expect_cache_miss evicted-after-reload "$mnt/cache-a.dat" 4096
dmesg >"$data_dir/dmesg.log"
grep -E 'reusing cache device=.*|restored 256 cache index entries' \
	"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo "STEP29_FAIL: kernel fault signature found"
	exit 1
fi
echo "STEP29_RELOAD_PASS"

# Simulate a crash after a valid 16-victim PREPARED journal and only half of
# its index clears. Recovery must retire the whole batch before restoring the
# index, so no partially cleared victim can later alias reused data.
busybox umount "$mnt"
rmmod kestrelfs
"$txn_helper" prepare-batch "$loopdev" 1 16 8
set +e
./tools/kestrelfs-cache-admin inspect "$loopdev" \
	>"$data_dir/batch-journal-inspect.log" 2>&1
inspect_status=$?
set -e
test "$inspect_status" -eq 3
grep -Fx 'journal_batch_count=16' "$data_dir/batch-journal-inspect.log"
grep -Fx 'overall_status=recovery-required' \
	"$data_dir/batch-journal-inspect.log"
load_cache
test "$(cat /sys/module/kestrelfs/parameters/cache_journal_recoveries)" -eq 1
start_daemon
busybox mount -t kestrelfs none "$mnt"
test -f "$mnt/cache-a.dat"
test -f "$mnt/cache-b.dat"
stop_daemon
"$helper" verify "$mnt/cache-a.dat" 0 4096 0 1
expect_cache_miss batch-recovery "$mnt/cache-b.dat" 0
dmesg >"$data_dir/dmesg.log"
grep -E 'recovered incomplete cache evict transaction .*count=16 to miss' \
	"$data_dir/dmesg.log"
echo "STEP29_BATCH_RECOVERY_PASS"

start_ns=$(date +%s%N)
busybox umount "$mnt"
end_ns=$(date +%s%N)
umount_ms=$(( (end_ns - start_ns) / 1000000 ))
test "$umount_ms" -lt 1000
rmmod kestrelfs

losetup -d "$loopdev"
loopdev=
trap - EXIT
rm -f "$image" "$helper" "$txn_helper"
echo "STEP29_CACHE: umount_ms=$umount_ms"
echo "STEP29_CACHE_EVICT_PASS"
