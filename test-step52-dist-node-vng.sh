#!/bin/bash
# Guest half of the Step 52 two-node Redis+S3 data-plane test.
set -euo pipefail

: "${STEP52_ROLE:?set STEP52_ROLE to A or B}"
: "${STEP52_RUN_ID:?set STEP52_RUN_ID}"
: "${STEP52_COORD:?set STEP52_COORD to the shared coordination directory}"
: "${REDIS_URL:?set REDIS_URL to a disposable Redis database}"
: "${S3_ENDPOINT:?set S3_ENDPOINT}"
: "${S3_BUCKET:?set S3_BUCKET to an existing bucket}"
: "${AWS_ACCESS_KEY_ID:?set AWS_ACCESS_KEY_ID}"
: "${AWS_SECRET_ACCESS_KEY:?set AWS_SECRET_ACCESS_KEY}"

test "$STEP52_ROLE" = A || test "$STEP52_ROLE" = B

test_id=$$
image=/tmp/kestrel-step52-dist-$STEP52_ROLE-$test_id.img
mnt=/tmp/mnt-kestrelfs-step52-dist-$STEP52_ROLE-$test_id
data_dir=/tmp/kestrelfs-step52-dist-$STEP52_ROLE-$test_id
helper=/tmp/kestrel-step52-dist-helper-$STEP52_ROLE-$test_id
prefix=kestrelfs:step52:$STEP52_RUN_ID
object_prefix=step52/$STEP52_RUN_ID
object_url=s3://$S3_BUCKET/$object_prefix
shared_file=$mnt/shared.dat
initial=$STEP52_COORD/initial
ready=$STEP52_COORD/reader-ready
committed=$STEP52_COORD/committed
done=$STEP52_COORD/reader-done
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=redis:%s;objects=s3:%s/%s/%s' \
	"$prefix" "$S3_ENDPOINT" "$S3_BUCKET" "$object_prefix" |
	sha256sum | awk '{print $1}')

redis_cmd() {
	redis-cli -u "$REDIS_URL" --raw "$@"
}

redis_cleanup() {
	local keys

	keys=$(redis_cmd --scan --pattern "$prefix:meta:*" 2>/dev/null || true)
	if test -n "$keys"; then
		mapfile -t key_array <<<"$keys"
		redis_cmd DEL "${key_array[@]}" >/dev/null 2>&1 || true
	fi
}

report_error() {
	local status=$?
	echo "STEP52_DIST_${STEP52_ROLE}_FAIL: line=$1 status=$status"
	test ! -f "$data_dir/daemon.log" || tail -n 160 "$data_dir/daemon.log"
	dmesg | tail -n 140
	exit "$status"
}
trap 'report_error $LINENO' ERR

stop_daemon() {
	if test -n "$daemon_pid"; then
		kill "$daemon_pid"
		wait "$daemon_pid" || true
		daemon_pid=
	fi
}

start_daemon() {
	./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
		--meta "$REDIS_URL" --redis-prefix "$prefix" \
		--objects "$object_url" --s3-endpoint "$S3_ENDPOINT" \
		>"$data_dir/daemon.log" 2>&1 &
	daemon_pid=$!
	sleep 1
	kill -0 "$daemon_pid"
	grep -F 'Redis coherence polling enabled' "$data_dir/daemon.log" >/dev/null
}

wait_marker() {
	local marker=$1
	local attempt

	for attempt in $(seq 1 1200); do
		test ! -e "$marker" || return 0
		sleep 0.05
	done
	echo "timed out waiting for marker $marker"
	return 1
}

cleanup() {
	set +e
	mountpoint -q "$mnt" && busybox umount "$mnt"
	stop_daemon
	test -d /sys/module/kestrelfs && rmmod kestrelfs
	if test -n "$loopdev"; then losetup -d "$loopdev" 2>/dev/null; fi
	test "$STEP52_ROLE" != A || redis_cleanup
	rm -f "$image" "$helper"
}
trap cleanup EXIT

if ! ip -4 route show default | grep -q '^default '; then
	ip -4 route add default via 10.0.2.2 dev eth0
fi
redis_cmd PING | grep -Fx PONG >/dev/null

if test "$STEP52_ROLE" = A; then
	redis_cleanup
	rm -f "$initial" "$ready" "$committed" "$done"
else
	wait_marker "$initial"
fi

mkdir -p "$data_dir" "$mnt"
cc -O2 -Wall -Wextra -Werror -std=gnu11 test-step52-dist-io.c -o "$helper"
truncate -s 128M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
echo "STEP52_DIST_${STEP52_ROLE}: loop_device=$loopdev namespace=$namespace prefix=$prefix"

insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
	cache_namespace="$namespace"
start_daemon
busybox mount -t kestrelfs none "$mnt"

if test "$STEP52_ROLE" = A; then
	"$helper" write "$shared_file" 1
	touch "$initial"
	wait_marker "$ready"
	"$helper" write "$shared_file" 2
	touch "$committed"
	echo STEP52_DIST_A_FSYNC_PASS
	wait_marker "$done"
	rm "$shared_file"
	sync
	sleep 1
else
	inode_batches=/sys/module/kestrelfs/parameters/cache_coherence_inode_batches
	inode_entries=/sys/module/kestrelfs/parameters/cache_coherence_inode_entries
	before_batches=$(cat "$inode_batches")
	before_entries=$(cat "$inode_entries")
	"$helper" watch "$shared_file" "$ready" "$committed"
	test "$(cat "$inode_batches")" -gt "$before_batches"
	test "$(cat "$inode_entries")" -gt "$before_entries"
	echo STEP52_DIST_REMOTE_VISIBLE_PASS
	exec 8<"$shared_file"
	sync
	echo 3 >/proc/sys/vm/drop_caches
	stop_daemon
	"$helper" verify-fd 8 2
	exec 8>&-
	echo STEP52_DIST_DAEMON_FREE_NEW_HIT_PASS
	start_daemon
fi

dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo "STEP52_DIST_${STEP52_ROLE}_FAIL: kernel safety diagnostic found"
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
if test "$STEP52_ROLE" = B; then
	touch "$done"
else
	redis_cleanup
fi
trap - EXIT
rm -f "$image" "$helper"
echo "STEP52_DIST_${STEP52_ROLE}: umount_ms=$umount_ms"
echo "STEP52_DIST_${STEP52_ROLE}_PASS"
