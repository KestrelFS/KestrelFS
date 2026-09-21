#!/bin/bash
# Step 45: read-side folio cache and synchronous-write invalidation.
set -euo pipefail
# shellcheck source=_repo_root.sh
# Keep cwd at repo root so daemon/ and kestrelfs/ relative paths stay stable.
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"

test_id=$$
image=/tmp/kestrel-step45-$test_id.img
mnt=/tmp/mnt-kestrelfs-step45-$test_id
data_dir=/tmp/kestrelfs-step45-$test_id
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

fail() {
	local status=$?
	echo "STEP45_FAIL: line=$1 status=$status"
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
	rm -f "$image"
}
trap cleanup EXIT

start_daemon() {
	./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
		>"$data_dir/daemon.log" 2>&1 &
	daemon_pid=$!
	sleep 1
	kill -0 "$daemon_pid"
}

hit_count() {
	local direct copied
	direct=$(cat /sys/module/kestrelfs/parameters/cache_direct_hit_blocks)
	copied=$(cat /sys/module/kestrelfs/parameters/cache_copy_hit_blocks)
	echo $((direct + copied))
}

mkdir -p "$data_dir" "$mnt"
head -c 131072 /dev/urandom >"$data_dir/old"
head -c 131072 /dev/urandom >"$data_dir/new"
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

cp "$data_dir/old" "$mnt/folio.dat"
cmp "$data_dir/old" "$mnt/folio.dat"
echo STEP45_WRITE_READ_PASS

# Step 49 retains clean written folios, so force a daemon read to warm NVMe
# before testing daemon-offline fallback through that backing cache.
echo 1 >/proc/sys/vm/drop_caches
cmp "$data_dir/old" "$mnt/folio.dat"

stop_daemon
before=$(hit_count)
cmp "$data_dir/old" "$mnt/folio.dat"
after=$(hit_count)
test "$after" -eq "$before"
echo STEP45_PAGECACHE_HIT_PASS

# Drop clean file pages only; dropping dentries/inodes while daemon is offline
# would require LOOKUP IPC before the backing-cache check.
echo 1 >/proc/sys/vm/drop_caches
cmp "$data_dir/old" "$mnt/folio.dat"
after_drop=$(hit_count)
test "$after_drop" -gt "$after"
echo "STEP45_BACKING_CACHE_HIT_PASS delta=$((after_drop - after))"

start_daemon
dd if="$data_dir/new" of="$mnt/folio.dat" bs=4096 conv=notrunc status=none
before_rewrite=$(hit_count)
cmp "$data_dir/new" "$mnt/folio.dat"
after_rewrite=$(hit_count)
test "$after_rewrite" -eq "$before_rewrite"
echo STEP45_REWRITE_NO_STALE_PASS

stop_daemon
start_ns=$(date +%s%N)
busybox umount "$mnt"
end_ns=$(date +%s%N)
umount_ms=$(((end_ns - start_ns) / 1000000))
test "$umount_ms" -lt 1000
rmmod kestrelfs
losetup -d "$loopdev"
loopdev=
trap - EXIT
rm -f "$image"
echo "STEP45_AOPS: umount_ms=$umount_ms"
echo STEP45_KERNEL_AOPS_PASS
