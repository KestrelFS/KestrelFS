#!/bin/bash
# Step 54 B/C/D: kernel-proven orphan retry, splice/sendfile, and pre-staged
# WRITE_DATA writeback. Run only inside a vng guest.
set -euo pipefail
# shellcheck source=_repo_root.sh
# Keep cwd at repo root so daemon/ and kestrelfs/ relative paths stay stable.
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"

: "${REDIS_URL:?set REDIS_URL to a disposable Redis database}"

test_id=$$
image=/tmp/kestrel-step54-$test_id.img
mnt=/tmp/mnt-kestrelfs-step54-$test_id
data_dir=/tmp/kestrelfs-step54-$test_id
helper=/tmp/kestrel-step54-vfs-$test_id
prefix=kestrelfs:step54:$test_id:$RANDOM
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=redis:%s;objects=local:%s' "$prefix" "$data_dir" |
	sha256sum | awk '{print $1}')

redis_cmd() {
	redis-cli -u "$REDIS_URL" --raw "$@"
}

object_count() {
	find "$data_dir" -type f ! -name daemon.log \
		! -name .orphan-retries-v1.json | wc -l
}

wait_for_count() {
	local expected=$1 attempt

	for attempt in $(seq 1 100); do
		test "$(object_count)" -eq "$expected" && return 0
		sleep 0.1
	done
	echo "object count did not reach $expected (got $(object_count))"
	return 1
}

wait_for_log() {
	local pattern=$1 attempt

	for attempt in $(seq 1 100); do
		grep -E "$pattern" "$data_dir/daemon.log" >/dev/null && return 0
		sleep 0.1
	done
	echo "daemon log did not contain: $pattern"
	return 1
}

fail() {
	local status=$?
	echo "STEP54_VFS_PIPE_FAIL: line=$1 status=$status"
	test ! -f "$data_dir/daemon.log" || tail -n 180 "$data_dir/daemon.log"
	dmesg | tail -n 140
	exit "$status"
}
trap 'fail $LINENO' ERR

stop_daemon() {
	if test -n "$daemon_pid"; then
		kill "$daemon_pid" 2>/dev/null || true
		wait "$daemon_pid" 2>/dev/null || true
		daemon_pid=
	fi
}

cleanup() {
	set +e
	mountpoint -q "$mnt" && busybox umount "$mnt"
	stop_daemon
	test -d /sys/module/kestrelfs && rmmod kestrelfs
	if test -n "$loopdev"; then losetup -d "$loopdev" 2>/dev/null; fi
	keys=$(redis_cmd --scan --pattern "$prefix:meta:*" 2>/dev/null || true)
	if test -n "$keys"; then
		mapfile -t key_array <<<"$keys"
		redis_cmd DEL "${key_array[@]}" >/dev/null 2>&1 || true
	fi
	rm -f "$image" "$helper"
}
trap cleanup EXIT

if ! ip -4 route show default | grep -q '^default '; then
	ip -4 route add default via 10.0.2.2 dev eth0
fi
redis_cmd PING | grep -Fx PONG >/dev/null
mkdir -p "$data_dir" "$mnt"
cc -O2 -Wall -Wextra -Werror -std=gnu11 tests/test-step54-vfs.c -o "$helper"
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
./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
	--meta "$REDIS_URL" --redis-prefix "$prefix" \
	--redis-session-ttl-ms 10000 >"$data_dir/daemon.log" 2>&1 &
daemon_pid=$!
sleep 1
kill -0 "$daemon_pid"
busybox mount -t kestrelfs none "$mnt"

# C: both splice directions plus sendfile, with deterministic byte checks.
"$helper" "$mnt" /tmp
echo STEP54_SPLICE_FILE_TO_PIPE_PASS
echo STEP54_SPLICE_PIPE_TO_FILE_PASS
echo STEP54_SENDFILE_PASS

# D: all folio bytes must be staged before taking the shared bounce mutex;
# the counters are diagnostic ABI and demonstrate the exercised interval.
staged_before=$(cat /sys/module/kestrelfs/parameters/write_pipe_staged_bytes)
submissions_before=$(cat /sys/module/kestrelfs/parameters/write_pipe_submissions)
dd if=/dev/zero of="$mnt/write-pipe.dat" bs=1M count=2 conv=fsync status=none
staged_after=$(cat /sys/module/kestrelfs/parameters/write_pipe_staged_bytes)
submissions_after=$(cat /sys/module/kestrelfs/parameters/write_pipe_submissions)
hold_ns=$(cat /sys/module/kestrelfs/parameters/write_pipe_lock_hold_ns)
wait_ns=$(cat /sys/module/kestrelfs/parameters/write_pipe_lock_wait_ns)
test "$staged_after" -ge $((staged_before + 2 * 1024 * 1024))
test "$submissions_after" -gt "$submissions_before"
test "$hold_ns" -gt 0
echo "STEP54_WRITE_PIPE_COUNTERS staged_delta=$((staged_after-staged_before)) submissions_delta=$((submissions_after-submissions_before)) lock_hold_ns=$hold_ns lock_wait_ns=$wait_ns"
echo STEP54_WRITE_PIPE_PRESTAGE_PASS

# B negative: an unlinked inode with a live fd never enters the retry queue.
printf 'live-open-orphan-payload\n' >"$mnt/live-open"
sync
exec 8<>"$mnt/live-open"
rm "$mnt/live-open"
live_count=$(object_count)
sleep 2
test "$(object_count)" -eq "$live_count"
IFS= read -r live_payload <&8
test "$live_payload" = live-open-orphan-payload
echo STEP54_ORPHAN_SWEEP_OPEN_SKIP_PASS
exec 8>&-
wait_for_count $((live_count - 1))

# B positive: pause the same daemon (TTL is deliberately longer than the IPC
# timeout), close the final fd so FINALIZE_ORPHAN fails and is kernel-queued,
# then resume. Periodic daemon replay must finalize metadata and run normal GC.
printf 'retry-orphan-payload\n' >"$mnt/retry-orphan"
sync
exec 9<>"$mnt/retry-orphan"
rm "$mnt/retry-orphan"
retry_count=$(object_count)
kill -STOP "$daemon_pid"
exec 9>&- || true
kill -CONT "$daemon_pid"
wait_for_count $((retry_count - 1))
wait_for_log 'ORPHAN-SWEEP source=periodic reclaimed=1 proof=durable-kernel-final-close'
echo STEP54_ORPHAN_SWEEP_RETRY_PASS

dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo STEP54_VFS_PIPE_FAIL_KERNEL_DIAGNOSTIC
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
keys=$(redis_cmd --scan --pattern "$prefix:meta:*" 2>/dev/null || true)
if test -n "$keys"; then
	mapfile -t key_array <<<"$keys"
	redis_cmd DEL "${key_array[@]}" >/dev/null
fi
trap - EXIT
rm -f "$image" "$helper"
echo "STEP54_VFS_PIPE: umount_ms=$umount_ms"
echo STEP54_VFS_PIPE_PASS
