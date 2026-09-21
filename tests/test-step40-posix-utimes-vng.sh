#!/bin/bash
# Step 40 POSIX-UTIMES: persistent file/directory times and orphan futimens.
set -euo pipefail
# shellcheck source=_repo_root.sh
# Keep cwd at repo root so daemon/ and kestrelfs/ relative paths stay stable.
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"

test_id=$$
image=/tmp/kestrel-step40-cache-$test_id.img
mnt=/tmp/mnt-kestrelfs-step40-$test_id
data_dir=/tmp/kestrelfs-step40-$test_id
helper=/tmp/kestrel-step40-posix-utimes-$test_id
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

report_error() {
	local status=$?
	echo "STEP40_FAIL: line=$1 status=$status"
	test -f "$data_dir/daemon.log" && tail -n 160 "$data_dir/daemon.log"
	dmesg | tail -n 180
	exit "$status"
}
trap 'report_error $LINENO' ERR

start_daemon() {
	./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
		>"$data_dir/daemon.log" 2>&1 &
	daemon_pid=$!
	sleep 1
	kill -0 "$daemon_pid"
}

stop_daemon() {
	if test -n "$daemon_pid"; then
		kill "$daemon_pid"
		wait "$daemon_pid" || true
		daemon_pid=
	fi
}

cleanup() {
	set +e
	mountpoint -q "$mnt" && busybox umount "$mnt"
	stop_daemon
	test -d /sys/module/kestrelfs && rmmod kestrelfs
	if test -n "$loopdev"; then
		losetup -d "$loopdev" 2>/dev/null
	fi
	rm -f "$image" "$helper"
}
trap cleanup EXIT

rm -rf "$data_dir" "$mnt"
rm -f "$image" "$helper"
mkdir -p "$data_dir" "$mnt"
cc -O2 -std=c11 -D_DEFAULT_SOURCE -Wall -Wextra -Werror \
	tests/test-step40-posix-utimes.c -o "$helper"
truncate -s 128M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
echo "STEP40_POSIX_UTIMES: loop_device=$loopdev namespace=$namespace"

# Module, cache device, and mount exist only in this vng guest.
insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
	cache_namespace="$namespace"
start_daemon
busybox mount -t kestrelfs none "$mnt"

printf 'step40 persistent timestamp data\n' >"$mnt/utimes-file-long-name"
mkdir "$mnt/utimes-directory-long-name"

# Exercise independent ATIME/MTIME requests, their combination, and ATTR_TOUCH.
touch -a -d @1577836800 "$mnt/utimes-file-long-name"
touch -m -d @1577836801 "$mnt/utimes-file-long-name"
touch -d @1577836810 "$mnt/utimes-directory-long-name"
test "$(stat -c %X:%Y "$mnt/utimes-file-long-name")" = 1577836800:1577836801
test "$(stat -c %X:%Y "$mnt/utimes-directory-long-name")" = 1577836810:1577836810
before=$(date +%s)
touch "$mnt/utimes-file-long-name"
after=$(date +%s)
touch_time=$(stat -c %Y "$mnt/utimes-file-long-name")
test "$touch_time" -ge "$before"
test "$touch_time" -le "$after"
echo 'STEP40_FILE_DIR_UTIMES_PASS'

# Restore deterministic values for persistence assertions.
touch -a -d @1577836800 "$mnt/utimes-file-long-name"
touch -m -d @1577836801 "$mnt/utimes-file-long-name"
"$helper" "$mnt/utimes-open-orphan"
test ! -e "$mnt/utimes-open-orphan"
echo 'STEP40_ORPHAN_FUTIMENS_PASS'

busybox umount "$mnt"
stop_daemon
start_daemon
busybox mount -t kestrelfs none "$mnt"
test "$(stat -c %X:%Y "$mnt/utimes-file-long-name")" = 1577836800:1577836801
test "$(stat -c %X:%Y "$mnt/utimes-directory-long-name")" = 1577836810:1577836810
echo 'STEP40_UTIMES_RESTART_PASS'

rm "$mnt/utimes-file-long-name"
rmdir "$mnt/utimes-directory-long-name"
dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo 'STEP40_FAIL: kernel safety diagnostic found'
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
echo "STEP40_POSIX_UTIMES: umount_ms=$umount_ms"
echo 'STEP40_POSIX_UTIMES_PASS'
