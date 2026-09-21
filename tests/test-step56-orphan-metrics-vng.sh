#!/bin/bash
# Step 56: persistent kernel-proven orphan handoff and minimal ops metrics.
# Run only inside a vng guest; the cache device is always a guest loop device.
set -euo pipefail
# shellcheck source=_repo_root.sh
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"

test_id=$$
image=/tmp/kestrel-step56-$test_id.img
mnt=/tmp/mnt-kestrelfs-step56-$test_id
data_dir=/tmp/kestrelfs-step56-$test_id
state_file=$data_dir/.orphan-retries-v1.json
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

objects() {
	find "$data_dir" -type f ! -name meta.json ! -name daemon.log \
		! -name dmesg.log ! -name .orphan-retries-v1.json | sort
}

wait_for_log() {
	local pattern=$1 attempt
	for attempt in $(seq 1 200); do
		grep -E "$pattern" "$data_dir/daemon.log" >/dev/null 2>&1 && return 0
		sleep 0.025
	done
	echo "daemon log did not contain: $pattern"
	return 1
}

wait_for_pending() {
	local expected=$1 attempt actual
	for attempt in $(seq 1 200); do
		actual=$(cat /sys/module/kestrelfs/parameters/orphan_retry_pending)
		test "$actual" -eq "$expected" && return 0
		sleep 0.025
	done
	echo "orphan_retry_pending did not reach $expected (got $actual)"
	return 1
}

wait_for_gone() {
	local path=$1 attempt
	for attempt in $(seq 1 200); do
		test ! -e "$path" && return 0
		sleep 0.025
	done
	echo "object was not reclaimed: $path"
	return 1
}

start_daemon() {
	: >"$data_dir/daemon.log"
	./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
		>"$data_dir/daemon.log" 2>&1 &
	daemon_pid=$!
	sleep 0.25
	kill -0 "$daemon_pid"
}

stop_daemon() {
	if test -n "$daemon_pid"; then
		kill "$daemon_pid" 2>/dev/null || true
		wait "$daemon_pid" 2>/dev/null || true
		daemon_pid=
	fi
}

fail() {
	local status=$?
	echo "STEP56_FAIL: line=$1 status=$status"
	test ! -f "$data_dir/daemon.log" || tail -n 180 "$data_dir/daemon.log"
	dmesg | tail -n 160
	exit "$status"
}
trap 'fail $LINENO' ERR

cleanup() {
	set +e
	mountpoint -q "$mnt" && busybox umount "$mnt"
	stop_daemon
	test -d /sys/module/kestrelfs && rmmod kestrelfs
	test -z "$loopdev" || losetup -d "$loopdev" 2>/dev/null
	rm -f "$image"
}
trap cleanup EXIT

rm -rf "$data_dir" "$mnt"
mkdir -p "$data_dir" "$mnt"
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

# Negative proof: unlinking a still-open inode must neither queue nor reclaim it.
objects >/tmp/kestrel-step56-before-$test_id
printf 'step56-open-proof\n' >"$mnt/still-open"
sync
objects >/tmp/kestrel-step56-after-$test_id
open_object=$(comm -13 /tmp/kestrel-step56-before-$test_id \
	/tmp/kestrel-step56-after-$test_id)
test -n "$open_object" && test -f "$open_object"
exec 8<>"$mnt/still-open"
rm "$mnt/still-open"
sleep 2
test -f "$open_object"
test "$(cat /sys/module/kestrelfs/parameters/orphan_retry_pending)" -eq 0
IFS= read -r open_payload <&8
test "$open_payload" = step56-open-proof
echo STEP56_ORPHAN_PERSIST_OPEN_SKIP_PASS
exec 8>&-
wait_for_gone "$open_object"

# Positive proof: last close with daemon stopped queues the inode and pins the
# module. After durable intake, unload/reload is safe and startup finalizes it.
objects >/tmp/kestrel-step56-before-$test_id
printf 'step56-persist-proof\n' >"$mnt/persist-orphan"
sync
objects >/tmp/kestrel-step56-after-$test_id
retry_object=$(comm -13 /tmp/kestrel-step56-before-$test_id \
	/tmp/kestrel-step56-after-$test_id)
test -n "$retry_object" && test -f "$retry_object"
exec 9<>"$mnt/persist-orphan"
rm "$mnt/persist-orphan"
queued_before=$(cat /sys/module/kestrelfs/parameters/orphan_retry_queued)
acked_before=$(cat /sys/module/kestrelfs/parameters/orphan_retry_acked)
stop_daemon
exec 9>&- || true
wait_for_pending 1
queued_after=$(cat /sys/module/kestrelfs/parameters/orphan_retry_queued)
test "$queued_after" -eq $((queued_before + 1))

busybox umount "$mnt"
if rmmod kestrelfs 2>"$data_dir/rmmod-pinned.log"; then
	echo 'module unloaded with an unpersisted orphan proof'
	exit 1
fi
test -d /sys/module/kestrelfs
echo STEP56_ORPHAN_PERSIST_RMMOD_GUARD_PASS

start_daemon
wait_for_log 'ORPHAN-SWEEP source=startup persisted=1 proof=kernel-final-close'
wait_for_pending 0
acked_after=$(cat /sys/module/kestrelfs/parameters/orphan_retry_acked)
test "$acked_after" -eq $((acked_before + 1))
test -f "$state_file"
grep -Eq '^[[:space:]]*[1-9][0-9]*,?[[:space:]]*$' "$state_file"
test -f "$retry_object"
echo "STEP56_METRICS_ORPHAN_RETRY_PASS queued_delta=$((queued_after-queued_before)) acked_delta=$((acked_after-acked_before))"

# Stop before the one-second periodic finalize pass, then prove module reload
# does not lose the daemon-side durable proof.
stop_daemon
rmmod kestrelfs
insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
	cache_namespace="$namespace"
start_daemon
wait_for_log 'ORPHAN-SWEEP source=startup reclaimed=1 proof=durable-kernel-final-close'
wait_for_gone "$retry_object"
grep -Eq '"inode_ids"[[:space:]]*:[[:space:]]*\[[[:space:]]*\]' "$state_file"
echo STEP56_ORPHAN_PERSIST_RELOAD_GC_PASS

busybox mount -t kestrelfs none "$mnt"
test ! -e "$mnt/persist-orphan"
busybox umount "$mnt"
stop_daemon
rmmod kestrelfs
losetup -d "$loopdev"
loopdev=

dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo STEP56_FAIL_KERNEL_DIAGNOSTIC
	exit 1
fi
trap - EXIT
rm -f "$image" /tmp/kestrel-step56-before-$test_id \
	/tmp/kestrel-step56-after-$test_id
echo STEP56_ORPHAN_PERSIST_PASS
echo STEP56_METRICS_PASS
echo STEP56_DOUBLE_PACK_PASS
