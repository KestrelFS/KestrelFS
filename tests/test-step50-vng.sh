#!/bin/bash
# Step 50: writable MAP_SHARED plus persistent rename whiteouts.
set -euo pipefail
# shellcheck source=_repo_root.sh
# Keep cwd at repo root so daemon/ and kestrelfs/ relative paths stay stable.
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"

test_id=$$
image=/tmp/kestrel-step50-$test_id.img
mnt=/tmp/mnt-kestrelfs-step50-$test_id
data_dir=/tmp/kestrelfs-step50-$test_id
mmap_helper=/tmp/kestrel-step50-mmap-$test_id
rename_helper=/tmp/kestrel-step50-rename-$test_id
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

fail() {
	local status=$?
	echo "STEP50_FAIL: line=$1 status=$status"
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
		>"$data_dir/daemon.log" 2>&1 &
	daemon_pid=$!
	sleep 1
	kill -0 "$daemon_pid"
}

expect_errno() {
	local expected=$1 old_path=$2 new_path=$3 flags=$4
	local status
	if "$rename_helper" "$old_path" "$new_path" "$flags" \
		2>"$data_dir/rename-$flags.err"; then
		echo "STEP50_FAIL: rename flags=$flags unexpectedly succeeded"
		return 1
	else
		status=$?
	fi
	test "$status" -eq "$expected"
}

cleanup() {
	set +e
	mountpoint -q "$mnt" && busybox umount "$mnt"
	stop_daemon
	test -d /sys/module/kestrelfs && rmmod kestrelfs
	if test -n "$loopdev"; then losetup -d "$loopdev"; fi
	rm -f "$image" "$mmap_helper" "$rename_helper"
}
trap cleanup EXIT

mkdir -p "$data_dir" "$mnt"
cc -O2 -Wall -Wextra -Werror -std=gnu11 tests/test-step50-map-shared.c \
	-o "$mmap_helper"
cc -O2 -Wall -Wextra -Werror -std=gnu11 tests/test-step33-renameat2.c \
	-o "$rename_helper"
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

"$mmap_helper" write "$mnt/shared-write.dat"
"$mmap_helper" verify "$mnt/shared-write.dat"
grep -q 'OP_WRITE_DATA' "$data_dir/daemon.log"
echo STEP50_MAP_SHARED_WRITE_PASS
echo STEP50_PRIVATE_COW_PASS
echo STEP50_MSYNC_FSYNC_MUNMAP_PASS

printf 'whiteout-payload\n' >"$mnt/whiteout-source"
source_inode=$(stat -c %i "$mnt/whiteout-source")
"$rename_helper" "$mnt/whiteout-source" "$mnt/whiteout-target" 4
test "$(stat -c %i "$mnt/whiteout-target")" = "$source_inode"
grep -Fx 'whiteout-payload' "$mnt/whiteout-target"
test -c "$mnt/whiteout-source"
test "$(stat -c '%t:%T' "$mnt/whiteout-source")" = '0:0'
find "$mnt" -maxdepth 1 -name whiteout-source -print | grep -Fx \
	"$mnt/whiteout-source"
echo STEP50_WHITEOUT_LOOKUP_READDIR_PASS

# WHITEOUT may combine with NOREPLACE. Existing target fails atomically;
# absent target succeeds. EXCHANGE combinations and unknown bits are invalid.
printf 'noreplace-source\n' >"$mnt/whiteout-noreplace-source"
printf 'noreplace-target\n' >"$mnt/whiteout-noreplace-target"
expect_errno 17 "$mnt/whiteout-noreplace-source" \
	"$mnt/whiteout-noreplace-target" 5
grep -Fx 'noreplace-source' "$mnt/whiteout-noreplace-source"
grep -Fx 'noreplace-target' "$mnt/whiteout-noreplace-target"
"$rename_helper" "$mnt/whiteout-noreplace-source" \
	"$mnt/whiteout-noreplace-fresh" 5
test -c "$mnt/whiteout-noreplace-source"
grep -Fx 'noreplace-source' "$mnt/whiteout-noreplace-fresh"
printf 'invalid-source\n' >"$mnt/invalid-source"
expect_errno 22 "$mnt/invalid-source" "$mnt/invalid-target" 6
expect_errno 22 "$mnt/invalid-source" "$mnt/invalid-target" 8
grep -Fx 'invalid-source' "$mnt/invalid-source"
echo STEP50_WHITEOUT_FLAGS_PASS

busybox umount "$mnt"
stop_daemon
start_daemon
busybox mount -t kestrelfs none "$mnt"
"$mmap_helper" verify "$mnt/shared-write.dat"
grep -Fx 'whiteout-payload' "$mnt/whiteout-target"
test -c "$mnt/whiteout-source"
test "$(stat -c '%t:%T' "$mnt/whiteout-source")" = '0:0'
test -c "$mnt/whiteout-noreplace-source"
grep -Fx 'noreplace-source' "$mnt/whiteout-noreplace-fresh"
echo STEP50_RESTART_PERSISTENCE_PASS

rm "$mnt/whiteout-source" "$mnt/whiteout-target" \
	"$mnt/whiteout-noreplace-source" "$mnt/whiteout-noreplace-fresh" \
	"$mnt/whiteout-noreplace-target" "$mnt/invalid-source" \
	"$mnt/shared-write.dat"
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
rm -f "$image" "$mmap_helper" "$rename_helper"
echo "STEP50_DOUBLE_PACK: umount_ms=$umount_ms"
echo STEP50_DOUBLE_PACK_PASS
