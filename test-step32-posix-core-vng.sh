#!/bin/bash
# Step 32 POSIX-CORE: hard-link, persistent nlink, and last-reference GC.
set -euo pipefail

test_id=$$
image=/tmp/kestrel-step32-cache-$test_id.img
mnt=/tmp/mnt-kestrelfs-step32-$test_id
data_dir=/tmp/kestrelfs-step32-$test_id
before=/tmp/kestrel-step32-before-$test_id
after=/tmp/kestrel-step32-after-$test_id
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

report_error() {
	local status=$?
	echo "STEP32_FAIL: line=$1 status=$status"
	test -f "$data_dir/daemon.log" && tail -n 100 "$data_dir/daemon.log"
	dmesg | tail -n 120
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

objects() {
	find "$data_dir" -type f ! -name meta.json ! -name daemon.log ! -name dmesg.log | sort
}

cleanup() {
	set +e
	mountpoint -q "$mnt" && busybox umount "$mnt"
	stop_daemon
	test -d /sys/module/kestrelfs && rmmod kestrelfs
	if test -n "$loopdev"; then
		losetup -d "$loopdev" 2>/dev/null
	fi
	rm -f "$image" "$before" "$after"
}
trap cleanup EXIT

rm -rf "$data_dir" "$mnt"
rm -f "$image" "$before" "$after"
mkdir -p "$data_dir" "$mnt"
truncate -s 128M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
echo "STEP32_POSIX: loop_device=$loopdev namespace=$namespace"

# Every mount/cache check is inside this vng guest and explicitly loads the
# module against a loop block device with a valid namespace identity.
insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
	cache_namespace="$namespace"
start_daemon
busybox mount -t kestrelfs none "$mnt"

objects >"$before"
printf 'step32-hard-link-payload-over-twelve-bytes\n' >"$mnt/original-long-file-name.dat"
objects >"$after"
object_path=$(comm -13 "$before" "$after")
test "$(printf '%s\n' "$object_path" | sed '/^$/d' | wc -l)" -eq 1
test -f "$object_path"

alias_name=hard-link-alias-name-over-twenty-three-bytes.dat
ln "$mnt/original-long-file-name.dat" "$mnt/$alias_name"
test "$(stat -c %i "$mnt/original-long-file-name.dat")" = \
	"$(stat -c %i "$mnt/$alias_name")"
test "$(stat -c %h "$mnt/original-long-file-name.dat")" -eq 2
cmp "$mnt/original-long-file-name.dat" "$mnt/$alias_name"
echo "STEP32_LINK_CREATE_PASS inode=$(stat -c %i "$mnt/$alias_name") nlink=2"

# FileMetaStore must restore both dirents and nlink after a daemon restart.
busybox umount "$mnt"
stop_daemon
start_daemon
busybox mount -t kestrelfs none "$mnt"
test "$(stat -c %i "$mnt/original-long-file-name.dat")" = \
	"$(stat -c %i "$mnt/$alias_name")"
test "$(stat -c %h "$mnt/$alias_name")" -eq 2
grep -Fx 'step32-hard-link-payload-over-twelve-bytes' "$mnt/$alias_name"
echo "STEP32_LINK_RESTART_PASS nlink=2"

# Removing one name cannot collect shared data; the final name must collect it.
rm "$mnt/original-long-file-name.dat"
test ! -e "$mnt/original-long-file-name.dat"
test "$(stat -c %h "$mnt/$alias_name")" -eq 1
grep -Fx 'step32-hard-link-payload-over-twelve-bytes' "$mnt/$alias_name"
test -f "$object_path"
echo "STEP32_LINK_SURVIVING_REFERENCE_PASS nlink=1 object=$object_path"
rm "$mnt/$alias_name"
test ! -e "$object_path"
echo "STEP32_LINK_FINAL_GC_PASS"

# Rename-overwrite removes one target dirent, not a surviving hard-link inode.
objects >"$before"
printf 'rename-target-hard-link-data\n' >"$mnt/rename-target"
objects >"$after"
rename_object=$(comm -13 "$before" "$after")
test "$(printf '%s\n' "$rename_object" | sed '/^$/d' | wc -l)" -eq 1
ln "$mnt/rename-target" "$mnt/rename-target-alias"
touch "$mnt/rename-source"
mv "$mnt/rename-source" "$mnt/rename-target"
test "$(stat -c %h "$mnt/rename-target-alias")" -eq 1
grep -Fx 'rename-target-hard-link-data' "$mnt/rename-target-alias"
test -f "$rename_object"
rm "$mnt/rename-target-alias"
test ! -e "$rename_object"
rm "$mnt/rename-target"
echo "STEP32_LINK_RENAME_OVERWRITE_PASS"

# Negative VFS cases: destination collision and directory hard link.
touch "$mnt/source" "$mnt/existing"
if ln "$mnt/source" "$mnt/existing" 2>"$data_dir/link-existing.err"; then
	echo 'STEP32_FAIL: hard link replaced an existing destination'
	exit 1
fi
mkdir "$mnt/directory"
if ln "$mnt/directory" "$mnt/directory-link" 2>"$data_dir/link-directory.err"; then
	echo 'STEP32_FAIL: directory hard link unexpectedly succeeded'
	exit 1
fi
echo "STEP32_LINK_NEGATIVE_PASS"

dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo 'STEP32_FAIL: kernel safety diagnostic found'
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
rm -f "$image" "$before" "$after"
echo "STEP32_POSIX: umount_ms=$umount_ms"
echo "STEP32_POSIX_CORE_PASS"
