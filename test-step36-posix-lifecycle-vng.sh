#!/bin/bash
# Step 36 POSIX-LIFECYCLE: open-unlink, hard-link crossing, cache, and final GC.
set -euo pipefail

test_id=$$
image=/tmp/kestrel-step36-cache-$test_id.img
mnt=/tmp/mnt-kestrelfs-step36-$test_id
data_dir=/tmp/kestrelfs-step36-$test_id
helper=/tmp/kestrel-step36-lifecycle-$test_id
ready=/tmp/kestrel-step36-ready-$test_id
final_unlinked=/tmp/kestrel-step36-final-unlinked-$test_id
update_done=/tmp/kestrel-step36-update-done-$test_id
read_again=/tmp/kestrel-step36-read-again-$test_id
read_done=/tmp/kestrel-step36-read-done-$test_id
close_now=/tmp/kestrel-step36-close-$test_id
before=/tmp/kestrel-step36-before-$test_id
after=/tmp/kestrel-step36-after-$test_id
loopdev=
daemon_pid=
helper_pid=
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

report_error() {
	local status=$?
	echo "STEP36_FAIL: line=$1 status=$status"
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

objects() {
	find "$data_dir" -type f ! -name meta.json ! -name daemon.log \
		! -name dmesg.log | sort
}

wait_for_path() {
	local path=$1
	for attempt in $(seq 1 200); do
		test -e "$path" && return 0
		sleep 0.025
	done
	echo "timed out waiting for $path"
	return 1
}

wait_for_objects_gone() {
	for attempt in $(seq 1 100); do
		objects >"$after"
		cmp -s "$before" "$after" && return 0
		sleep 0.05
	done
	echo 'orphan object files were not reclaimed'
	comm -13 "$before" "$after"
	return 1
}

cleanup() {
	set +e
	test -n "$helper_pid" && kill "$helper_pid" 2>/dev/null
	mountpoint -q "$mnt" && busybox umount "$mnt"
	stop_daemon
	test -d /sys/module/kestrelfs && rmmod kestrelfs
	if test -n "$loopdev"; then
		losetup -d "$loopdev" 2>/dev/null
	fi
	rm -f "$image" "$helper" "$ready" "$final_unlinked" "$update_done" \
		"$read_again" "$read_done" \
		"$close_now" "$before" "$after"
}
trap cleanup EXIT

rm -rf "$data_dir" "$mnt"
rm -f "$image" "$helper" "$ready" "$final_unlinked" "$update_done" \
	"$read_again" "$read_done" \
	"$close_now" "$before" "$after"
mkdir -p "$data_dir" "$mnt"
cc -O2 -std=c11 -D_DEFAULT_SOURCE -Wall -Wextra -Werror \
	test-step36-posix-lifecycle.c -o "$helper"
truncate -s 128M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
echo "STEP36_POSIX_LIFECYCLE: loop_device=$loopdev namespace=$namespace"

insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
	cache_namespace="$namespace"
start_daemon
busybox mount -t kestrelfs none "$mnt"

objects >"$before"
printf 'STEP36_ORIGINAL_0123456789abcdef\n' >"$mnt/lifecycle-original"
ln "$mnt/lifecycle-original" "$mnt/lifecycle-alias"
test "$(stat -c %h "$mnt/lifecycle-original")" -eq 2
cat "$mnt/lifecycle-original" >/dev/null

"$helper" "$mnt/lifecycle-alias" "$ready" "$final_unlinked" "$update_done" \
	"$read_again" "$read_done" "$close_now" &
helper_pid=$!
wait_for_path "$ready"
test ! -e "$mnt/lifecycle-alias"
test "$(stat -c %h "$mnt/lifecycle-original")" -eq 1
rm "$mnt/lifecycle-original"
test ! -e "$mnt/lifecycle-original"
touch "$final_unlinked"
wait_for_path "$update_done"
objects >"$after"
test "$(comm -13 "$before" "$after" | sed '/^$/d' | wc -l)" -ge 2
echo 'STEP36_OPEN_UNLINK_RETAIN_PASS'

# The helper filled the updated block after pwrite. With daemon stopped, its
# unlinked fd must still read that block through the kernel cache.
stop_daemon
touch "$read_again"
wait_for_path "$read_done"
echo 'STEP36_UNLINKED_CACHE_HIT_PASS'

# Restart before close so FINALIZE_ORPHAN can commit metadata + durable GC.
start_daemon
touch "$close_now"
wait "$helper_pid"
helper_pid=
wait_for_objects_gone
echo 'STEP36_LAST_CLOSE_GC_PASS'

dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo 'STEP36_FAIL: kernel safety diagnostic found'
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
rm -f "$image" "$helper" "$ready" "$final_unlinked" "$update_done" \
	"$read_again" "$read_done" \
	"$close_now" "$before" "$after"
echo "STEP36_POSIX_LIFECYCLE: umount_ms=$umount_ms"
echo 'STEP36_POSIX_LIFECYCLE_PASS'
