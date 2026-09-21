#!/bin/bash
# Step 51: delayed folio writeback, explicit durability and visible errors.
set -euo pipefail
# shellcheck source=_repo_root.sh
# Keep cwd at repo root so daemon/ and kestrelfs/ relative paths stay stable.
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"

test_id=$$
image=/tmp/kestrel-step51-$test_id.img
mnt=/tmp/mnt-kestrelfs-step51-$test_id
data_dir=/tmp/kestrelfs-step51-$test_id
helper=/tmp/kestrel-step51-helper-$test_id
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

fail() {
	local status=$?
	echo "STEP51_FAIL: line=$1 status=$status"
	test ! -f "$data_dir/daemon.log" || tail -n 120 "$data_dir/daemon.log"
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

start_daemon() {
	./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
		8>&- 9>&- >"$data_dir/daemon.log" 2>&1 &
	daemon_pid=$!
	sleep 1
	kill -0 "$daemon_pid"
}

cleanup() {
	set +e
	exec 8>&-
	exec 9>&-
	mountpoint -q "$mnt" && busybox umount "$mnt"
	stop_daemon
	test -d /sys/module/kestrelfs && rmmod kestrelfs
	if test -n "$loopdev"; then losetup -d "$loopdev"; fi
	rm -f "$image" "$helper"
}
trap cleanup EXIT

mkdir -p "$data_dir" "$mnt"
cc -O2 -Wall -Wextra -Werror -std=gnu11 -I kestrelfs \
	tests/test-step51-write-behind.c -o "$helper"
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

file="$mnt/write-behind.dat"
"$helper" prepare "$file"
exec 8<>"$file"
"$helper" preload-fd 8 1

# A cached full-page buffered write must return without WRITE_DATA even while
# the daemon is offline. The following explicit fsync must expose the failure.
stop_daemon
"$helper" async-write-fd 8 2
"$helper" verify-fd 8 2
"$helper" fsync-fail-fd 8
echo STEP51_ASYNC_RETURN_PASS
echo STEP51_FSYNC_ERROR_PASS

# The failed writeback redirties the folio. Recovery plus explicit fsync must
# retry it, then the daemon durability barrier makes it restart-safe.
start_daemon
"$helper" fsync-recover-fd 8
"$helper" fsync-fd 8
echo STEP51_FSYNC_RECOVERY_PASS

# Exercise the mount-wide syncfs ordering on a second dirty generation.
"$helper" async-write-fd 8 3
"$helper" syncfs-recover-fd 8
"$helper" syncfs-fd 8
echo STEP51_SYNCFS_BARRIER_PASS

# A remote-revision style page-cache retirement must write a dirty shared
# mapping before dropping it, then refault the committed bytes.
"$helper" mmap-coherence-fd 8 4
"$helper" fsync-fd 8
echo STEP51_DIRTY_COHERENCE_PASS
exec 8>&-

busybox umount "$mnt"
stop_daemon
start_daemon
busybox mount -t kestrelfs none "$mnt"
exec 9<>"$file"
"$helper" verify-fd 9 4
exec 9>&-
echo STEP51_RESTART_READBACK_PASS

rm "$file"
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
echo "STEP51_WRITE_BEHIND: umount_ms=$umount_ms"
echo STEP51_WRITE_BEHIND_PASS
