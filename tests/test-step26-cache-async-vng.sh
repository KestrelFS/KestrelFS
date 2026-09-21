#!/bin/bash
# Phase 4 Step 26 parallel cache-hit and concurrent invalidation test.
set -euo pipefail
# shellcheck source=_repo_root.sh
# Keep cwd at repo root so daemon/ and kestrelfs/ relative paths stay stable.
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"

test_id=$$
image=/tmp/kestrel-step26-cache-$test_id.img
mnt=/tmp/mnt-kestrelfs-step26-$test_id
data_dir=/tmp/kestrelfs-step26-$test_id
io_helper=/tmp/kestrel-step26-cache-io-$test_id
race_helper=/tmp/kestrel-step26-cache-race-$test_id
payload_size=$((1024 * 1024))
workers=8
iterations=16
loopdev=
daemon_pid=
last_umount_ms=0
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

report_error() {
	local status=$?

	echo "STEP26_FAIL: line=$1 status=$status"
	if test -f "$data_dir/daemon.log"; then
		tail -n 100 "$data_dir/daemon.log"
	fi
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
	if [ -n "$daemon_pid" ]; then
		kill "$daemon_pid"
		wait "$daemon_pid" || true
		daemon_pid=
	fi
}

load_cache() {
	local parallel=$1

	insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
		cache_namespace="$namespace" cache_parallel_reads="$parallel"
}

unmount_and_unload() {
	if mountpoint -q "$mnt"; then
		local start_ns
		local end_ns

		start_ns=$(date +%s%N)
		busybox umount "$mnt"
		end_ns=$(date +%s%N)
		last_umount_ms=$(( (end_ns - start_ns) / 1000000 ))
		test "$last_umount_ms" -lt 1000
	fi
	stop_daemon
	if test -d /sys/module/kestrelfs; then
		rmmod kestrelfs
	fi
}

run_read_group() {
	local label=$1
	local start_ns
	local end_ns
	local pids=()
	local i

	start_ns=$(date +%s%N)
	for ((i = 0; i < workers; i++)); do
		"$io_helper" verify "$mnt/parallel.dat" 0 "$payload_size" 0 \
			"$iterations" >"$data_dir/$label-reader-$i.log" 2>&1 &
		pids+=("$!")
	done
	for i in "${pids[@]}"; do
		wait "$i"
	done
	end_ns=$(date +%s%N)
	echo $((end_ns - start_ns))
}

cleanup() {
	set +e
	unmount_and_unload
	if [ -n "$loopdev" ]; then
		losetup -d "$loopdev" 2>/dev/null
	fi
	rm -f "$image" "$io_helper" "$race_helper"
}
trap cleanup EXIT

rm -rf "$data_dir" "$mnt"
rm -f "$image" "$io_helper" "$race_helper"
mkdir -p "$data_dir" "$mnt"
cc -O2 -Wall -Wextra -Werror tests/test-step22-cache-io.c -o "$io_helper"
cc -O2 -Wall -Wextra -Werror -pthread \
	tests/test-step26-cache-concurrency.c -o "$race_helper"
truncate -s 128M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
echo "STEP26_CACHE: loop_device=$loopdev namespace=$namespace"

# Same build, old exclusive-hit mode: concurrent callers must serialize.
load_cache 0
start_daemon
busybox mount -t kestrelfs none "$mnt"
"$io_helper" prepare "$mnt/parallel.dat" "$payload_size"
"$io_helper" verify "$mnt/parallel.dat" 0 "$payload_size" 0 1
"$io_helper" verify "$mnt/parallel.dat" 0 "$payload_size" 0 1
serial_ns=$(run_read_group serial)
serial_peak=$(cat /sys/module/kestrelfs/parameters/cache_parallel_hit_peak)
test "$serial_peak" -eq 1
unmount_and_unload

# Shared-hit mode: identical persisted cache/workload must overlap BIO readers.
load_cache 1
start_daemon
busybox mount -t kestrelfs none "$mnt"
"$io_helper" verify "$mnt/parallel.dat" 0 "$payload_size" 0 1
parallel_ns=$(run_read_group parallel)
parallel_peak=$(cat /sys/module/kestrelfs/parameters/cache_parallel_hit_peak)
test "$parallel_peak" -ge 2
echo "STEP26_PARALLEL_HIT_PASS peak=$parallel_peak"

# Two readers enter the shared hit section before rewrite invalidation requests
# exclusive ownership. Both must finish with old valid data; the rewrite then
# commits, refills, and remains a valid cache hit after the daemon is stopped.
"$race_helper" rewrite-race "$mnt/parallel.dat" "$payload_size" 2 \
	/sys/module/kestrelfs/parameters/cache_active_hit_readers
test "$(cat /sys/module/kestrelfs/parameters/cache_parallel_hit_peak)" -ge 2
stop_daemon
"$race_helper" verify-new "$mnt/parallel.dat" "$payload_size"
echo "STEP26_CONCURRENT_INVALIDATE_PASS"

dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo "STEP26_FAIL: kernel safety diagnostic found"
	exit 1
fi

unmount_and_unload

losetup -d "$loopdev"
loopdev=
trap - EXIT
rm -f "$image" "$io_helper" "$race_helper"
echo "STEP26_PERF: serial_ns=$serial_ns parallel_ns=$parallel_ns workload=${payload_size}B_x${iterations}_x${workers}"
echo "STEP26_CACHE: serial_peak=$serial_peak parallel_peak=$parallel_peak umount_ms=$last_umount_ms"
echo "STEP26_CACHE_ASYNC_PASS"
