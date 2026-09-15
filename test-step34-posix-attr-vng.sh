#!/bin/bash
# Step 34 POSIX-ATTR: create/mkdir mode and directory nlink persistence.
set -euo pipefail

test_id=$$
image=/tmp/kestrel-step34-cache-$test_id.img
mnt=/tmp/mnt-kestrelfs-step34-$test_id
data_dir=/tmp/kestrelfs-step34-$test_id
helper=/tmp/kestrel-step34-posix-attr-$test_id
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

report_error() {
	local status=$?
	echo "STEP34_FAIL: line=$1 status=$status"
	test -f "$data_dir/daemon.log" && tail -n 120 "$data_dir/daemon.log"
	dmesg | tail -n 140
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
cc -O2 -Wall -Wextra -Werror test-step34-posix-attr.c -o "$helper"
truncate -s 128M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
echo "STEP34_POSIX_ATTR: loop_device=$loopdev namespace=$namespace"

# All mount/cache operations run inside this vng guest. The module is loaded
# explicitly against a loop block device with a valid namespace identity.
insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
	cache_namespace="$namespace"
start_daemon
busybox mount -t kestrelfs none "$mnt"

# The root starts at the POSIX directory baseline. Regular-file create keeps
# its supplied mode but does not alter the containing directory's nlink.
test "$(stat -c %h "$mnt")" -eq 2
"$helper" file "$mnt/mode-file-long-name" 0640
test -f "$mnt/mode-file-long-name"
test "$(stat -c %a "$mnt/mode-file-long-name")" = 640
test "$(stat -c %h "$mnt")" -eq 2
ln "$mnt/mode-file-long-name" "$mnt/mode-file-hardlink"
test "$(stat -c %h "$mnt/mode-file-long-name")" -eq 2
test "$(stat -c %h "$mnt")" -eq 2
echo 'STEP34_FILE_MODE_PASS'

# Each immediate subdirectory contributes exactly one to its parent's nlink.
"$helper" dir "$mnt/mode-directory-long-name" 0711
test -d "$mnt/mode-directory-long-name"
test "$(stat -c %a "$mnt/mode-directory-long-name")" = 711
test "$(stat -c %h "$mnt/mode-directory-long-name")" -eq 2
test "$(stat -c %h "$mnt")" -eq 3
"$helper" dir "$mnt/mode-directory-long-name/nested" 0700
test "$(stat -c %h "$mnt/mode-directory-long-name")" -eq 3
rmdir "$mnt/mode-directory-long-name/nested"
test "$(stat -c %h "$mnt/mode-directory-long-name")" -eq 2
echo 'STEP34_MKDIR_MODE_NLINK_PASS'

# A cross-directory move transfers the child's link contribution. A
# same-directory rename does not change it; replacing an empty directory
# removes exactly one contribution.
"$helper" dir "$mnt/left-parent" 0750
"$helper" dir "$mnt/right-parent" 0751
test "$(stat -c %h "$mnt")" -eq 5
"$helper" dir "$mnt/left-parent/moving-child" 0705
test "$(stat -c %h "$mnt/left-parent")" -eq 3
test "$(stat -c %h "$mnt/right-parent")" -eq 2
mv "$mnt/left-parent/moving-child" "$mnt/right-parent/moved-child"
test "$(stat -c %h "$mnt/left-parent")" -eq 2
test "$(stat -c %h "$mnt/right-parent")" -eq 3
mv "$mnt/right-parent/moved-child" "$mnt/right-parent/renamed-child"
test "$(stat -c %h "$mnt/right-parent")" -eq 3
"$helper" dir "$mnt/right-parent/replaced-child" 0701
test "$(stat -c %h "$mnt/right-parent")" -eq 4
mv -T "$mnt/right-parent/renamed-child" "$mnt/right-parent/replaced-child"
test "$(stat -c %h "$mnt/right-parent")" -eq 3
test "$(stat -c %a "$mnt/right-parent/replaced-child")" = 705
echo 'STEP34_RENAME_DIR_NLINK_PASS'

# FileMetaStore recovery must retain modes and authoritative directory nlinks,
# including the root inode that was initially materialized by fill_super().
busybox umount "$mnt"
stop_daemon
start_daemon
busybox mount -t kestrelfs none "$mnt"
test "$(stat -c %a "$mnt/mode-file-long-name")" = 640
test "$(stat -c %h "$mnt/mode-file-long-name")" -eq 2
test "$(stat -c %a "$mnt/mode-directory-long-name")" = 711
test "$(stat -c %h "$mnt/mode-directory-long-name")" -eq 2
test "$(stat -c %h "$mnt/left-parent")" -eq 2
test "$(stat -c %h "$mnt/right-parent")" -eq 3
test "$(stat -c %h "$mnt")" -eq 5
echo 'STEP34_ATTR_RESTART_PASS'

rm "$mnt/mode-file-long-name" "$mnt/mode-file-hardlink"
rmdir "$mnt/right-parent/replaced-child"
test "$(stat -c %h "$mnt/right-parent")" -eq 2
rmdir "$mnt/right-parent" "$mnt/left-parent" "$mnt/mode-directory-long-name"
test "$(stat -c %h "$mnt")" -eq 2
echo 'STEP34_DIR_NLINK_CLEANUP_PASS'

dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo 'STEP34_FAIL: kernel safety diagnostic found'
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
echo "STEP34_POSIX_ATTR: umount_ms=$umount_ms"
echo 'STEP34_POSIX_ATTR_PASS'
