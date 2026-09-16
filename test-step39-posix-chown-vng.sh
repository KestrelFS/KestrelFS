#!/bin/bash
# Step 39 POSIX-CHOWN: persistent file/directory ownership and orphan fchown.
set -euo pipefail

test_id=$$
image=/tmp/kestrel-step39-cache-$test_id.img
mnt=/tmp/mnt-kestrelfs-step39-$test_id
data_dir=/tmp/kestrelfs-step39-$test_id
helper=/tmp/kestrel-step39-posix-chown-$test_id
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

report_error() {
	local status=$?
	echo "STEP39_FAIL: line=$1 status=$status"
	test -f "$data_dir/daemon.log" && tail -n 140 "$data_dir/daemon.log"
	dmesg | tail -n 160
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
	test-step39-posix-chown.c -o "$helper"
truncate -s 128M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
echo "STEP39_POSIX_CHOWN: loop_device=$loopdev namespace=$namespace"

# Module, cache device, and mount exist only in this vng guest.
insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
	cache_namespace="$namespace"
start_daemon
busybox mount -t kestrelfs none "$mnt"

printf 'step39 persistent owner data\n' >"$mnt/chown-file-long-name"
mkdir "$mnt/chown-directory-long-name"
ln "$mnt/chown-file-long-name" "$mnt/chown-file-alias"
chown 1234 "$mnt/chown-file-long-name"
chgrp 2345 "$mnt/chown-file-long-name"
chown 2234:3345 "$mnt/chown-directory-long-name"
test "$(stat -c %u:%g "$mnt/chown-file-long-name")" = 1234:2345
test "$(stat -c %u:%g "$mnt/chown-file-alias")" = 1234:2345
test "$(stat -c %u:%g "$mnt/chown-directory-long-name")" = 2234:3345
echo 'STEP39_FILE_DIR_CHOWN_PASS'

# Explicit timestamps remain outside Step 39 and must fail closed.
if touch -t 202001010000 "$mnt/chown-file-long-name" 2>/dev/null; then
	echo 'STEP39_FAIL: explicit timestamp setattr unexpectedly succeeded'
	exit 1
fi
echo 'STEP39_UNSUPPORTED_ATTRS_PASS'

# The retained orphan remains addressable by inode until its final close.
"$helper" "$mnt/chown-open-orphan"
test ! -e "$mnt/chown-open-orphan"
echo 'STEP39_ORPHAN_FCHOWN_PASS'

# FileMetaStore restart must reconstruct persistent owners for files and dirs.
busybox umount "$mnt"
stop_daemon
start_daemon
busybox mount -t kestrelfs none "$mnt"
test "$(stat -c %u:%g "$mnt/chown-file-long-name")" = 1234:2345
test "$(stat -c %u:%g "$mnt/chown-file-alias")" = 1234:2345
test "$(stat -c %u:%g "$mnt/chown-directory-long-name")" = 2234:3345
test "$(cat "$mnt/chown-file-long-name")" = 'step39 persistent owner data'
echo 'STEP39_CHOWN_RESTART_PASS'

rm "$mnt/chown-file-alias" "$mnt/chown-file-long-name"
rmdir "$mnt/chown-directory-long-name"
dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo 'STEP39_FAIL: kernel safety diagnostic found'
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
echo "STEP39_POSIX_CHOWN: umount_ms=$umount_ms"
echo 'STEP39_POSIX_CHOWN_PASS'
