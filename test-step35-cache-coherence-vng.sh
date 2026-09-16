#!/bin/bash
# Step 35 CACHE-COHERENCE: Redis revision polling -> durable full-cache miss.
set -euo pipefail

: "${REDIS_URL:?set REDIS_URL to a disposable Redis database before running}"

test_id=$$
image=/tmp/kestrel-step35-cache-$test_id.img
mnt=/tmp/mnt-kestrelfs-step35-$test_id
data_dir=/tmp/kestrelfs-step35-$test_id
expected_old=/tmp/kestrel-step35-old-$test_id
expected_new=/tmp/kestrel-step35-new-$test_id
actual=/tmp/kestrel-step35-actual-$test_id
prefix=kestrelfs:step35:$test_id
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=redis:step35#%s;objects=local:%s' \
	"$prefix" "$data_dir" | sha256sum | awk '{print $1}')

redis_cmd() {
	redis-cli -u "$REDIS_URL" --raw "$@"
}

redis_cleanup() {
	local keys

	keys=$(redis_cmd --scan --pattern "$prefix:meta:*" 2>/dev/null || true)
	if test -n "$keys"; then
		# Test-owned random prefix: whitespace is forbidden by RedisMetaStore,
		# so one key per line can safely become DEL arguments.
		mapfile -t key_array <<<"$keys"
		redis_cmd DEL "${key_array[@]}" >/dev/null 2>&1 || true
	fi
}

report_error() {
	local status=$?
	echo "STEP35_FAIL: line=$1 status=$status"
	test -f "$data_dir/daemon.log" && tail -n 140 "$data_dir/daemon.log"
	dmesg | tail -n 160
	exit "$status"
}
trap 'report_error $LINENO' ERR

start_daemon() {
	./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
		--meta "$REDIS_URL" --redis-prefix "$prefix" \
		>"$data_dir/daemon.log" 2>&1 &
	daemon_pid=$!
	sleep 1
	kill -0 "$daemon_pid"
	grep -F 'Redis coherence polling enabled' "$data_dir/daemon.log"
}

stop_daemon() {
	if test -n "$daemon_pid"; then
		kill "$daemon_pid"
		wait "$daemon_pid" || true
		daemon_pid=
	fi
}

wait_for_counter_gt() {
	local path=$1
	local old=$2
	local attempt current

	for attempt in $(seq 1 80); do
		current=$(cat "$path")
		if test "$current" -gt "$old"; then
			return 0
		fi
		sleep 0.05
	done
	echo "counter did not advance: path=$path old=$old current=$current"
	return 1
}

wait_for_log() {
	local pattern=$1
	local attempt

	for attempt in $(seq 1 80); do
		if grep -F -- "$pattern" "$data_dir/daemon.log" >/dev/null; then
			return 0
		fi
		sleep 0.05
	done
	echo "daemon log pattern not observed: $pattern"
	return 1
}

cache_hits() {
	local direct copy

	direct=$(cat /sys/module/kestrelfs/parameters/cache_direct_hit_blocks)
	copy=$(cat /sys/module/kestrelfs/parameters/cache_copy_hit_blocks)
	echo $((direct + copy))
}

cleanup() {
	set +e
	mountpoint -q "$mnt" && busybox umount "$mnt"
	stop_daemon
	test -d /sys/module/kestrelfs && rmmod kestrelfs
	if test -n "$loopdev"; then
		losetup -d "$loopdev" 2>/dev/null
	fi
	redis_cleanup
	rm -f "$image" "$expected_old" "$expected_new" "$actual"
}
trap cleanup EXIT

# This vng image's udhcpc hook assigns 10.0.2.15 but does not install the
# QEMU user-network default route.  Keep network setup inside the guest so the
# Redis-backed coherence path is exercised without host mount/module access.
if ! ip -4 route show default | grep -q '^default '; then
	ip -4 route add default via 10.0.2.2 dev eth0
fi

redis_cmd PING | grep -Fx PONG
redis_cleanup
rm -rf "$data_dir" "$mnt"
rm -f "$image" "$expected_old" "$expected_new" "$actual"
mkdir -p "$data_dir" "$mnt"
head -c 4096 /dev/zero | tr '\0' A >"$expected_old"
head -c 4096 /dev/zero | tr '\0' B >"$expected_new"
truncate -s 128M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
echo "STEP35_CACHE_COHERENCE: loop_device=$loopdev namespace=$namespace prefix=$prefix"

insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
	cache_namespace="$namespace"
start_daemon
busybox mount -t kestrelfs none "$mnt"

coherence_counter=/sys/module/kestrelfs/parameters/cache_coherence_invalidations
inode_coherence_counter=/sys/module/kestrelfs/parameters/cache_coherence_inode_batches
test "$(cat "$coherence_counter")" -ge 1

# The local write advances Redis revision. Wait for that revision to be
# observed (which conservatively clears the startup/write-era cache), then
# warm one current entry and prove the second read is a real kernel hit.
before_local=$(cat "$inode_coherence_counter")
cp "$expected_old" "$mnt/coherence-data.bin"
local_revision=$(redis_cmd HGET "$prefix:meta:v2:control" revision)
wait_for_counter_gt "$inode_coherence_counter" "$before_local"
wait_for_log "-> $local_revision; invalidated"
cat "$mnt/coherence-data.bin" >"$actual"
cmp "$expected_old" "$actual"
hits_before=$(cache_hits)
cat "$mnt/coherence-data.bin" >"$actual"
cmp "$expected_old" "$actual"
hits_after=$(cache_hits)
test "$hits_after" -gt "$hits_before"
echo "STEP35_READER_CACHE_HIT_PASS hits_delta=$((hits_after - hits_before))"

# Simulate a second daemon's normal immutable-object/COW metadata commit:
# write the new object first, then atomically append a newer slice, update
# inode mtime, and advance Redis's durable revision. The reader daemon has no
# involvement in this mutation and learns about it only through revision poll.
inode=$(stat -c %i "$mnt/coherence-data.bin")
control_key=$prefix:meta:v2:control
inodes_key=$prefix:meta:v2:inodes
slices_key=$prefix:meta:v2:slices
slice_field=$inode:0
old_revision=$(redis_cmd HGET "$control_key" revision)
new_revision=$((old_revision + 1))
old_slices=$(redis_cmd HGET "$slices_key" "$slice_field")
old_inode=$(redis_cmd HGET "$inodes_key" "$inode")
test -n "$old_slices"
test -n "$old_inode"
new_uuid=$(cat /proc/sys/kernel/random/uuid)
remote_mtime=$(( $(date +%s) + 1000 ))
new_slices=$(jq -c --arg uuid "$new_uuid" --argjson mtime "$remote_mtime" \
	'. + [{"chunk_index":0,"slice_id":$uuid,"chunk_offset":0,"length":4096,"written_at":$mtime}]' \
	<<<"$old_slices")
new_inode=$(jq -c --argjson mtime "$remote_mtime" '.mtime = $mtime' \
	<<<"$old_inode")
mkdir -p "$data_dir/$new_uuid"
cp "$expected_new" "$data_dir/$new_uuid/0"
sync "$data_dir/$new_uuid/0"

remote_commit='local r=redis.call("HGET",KEYS[1],"revision"); if r~=ARGV[1] then return 0 end; redis.call("HSET",KEYS[2],ARGV[3],ARGV[4]); redis.call("HSET",KEYS[3],ARGV[5],ARGV[6]); redis.call("HSET",KEYS[1],"revision",ARGV[2]); return 1'
counter_before_remote=$(cat "$coherence_counter")
commit_result=$(redis_cmd EVAL "$remote_commit" 3 \
	"$control_key" "$inodes_key" "$slices_key" \
	"$old_revision" "$new_revision" "$inode" "$new_inode" \
	"$slice_field" "$new_slices")
test "$commit_result" = 1
wait_for_counter_gt "$coherence_counter" "$counter_before_remote"
wait_for_log "coherence revision $old_revision -> $new_revision; dirty history unavailable/overflowed, full local cache invalidated"
echo 'STEP35_REMOTE_REVISION_INVALIDATE_PASS'

# The first post-invalidation read must miss and fetch B through READ_DATA;
# the next read fills/hits B. Stopping the daemon proves no stale A entry
# survived and the newly filled entry is locally usable without IPC.
cat "$mnt/coherence-data.bin" >"$actual"
cmp "$expected_new" "$actual"
cat "$mnt/coherence-data.bin" >"$actual"
cmp "$expected_new" "$actual"
stop_daemon
cat "$mnt/coherence-data.bin" >"$actual"
cmp "$expected_new" "$actual"
echo 'STEP35_DAEMON_FREE_NEW_HIT_PASS'

dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo 'STEP35_FAIL: kernel safety diagnostic found'
	exit 1
fi

start_ns=$(date +%s%N)
busybox umount "$mnt"
end_ns=$(date +%s%N)
umount_ms=$(( (end_ns - start_ns) / 1000000 ))
test "$umount_ms" -lt 1000
rmmod kestrelfs
losetup -d "$loopdev"
loopdev=
redis_cleanup
trap - EXIT
rm -f "$image" "$expected_old" "$expected_new" "$actual"
echo "STEP35_CACHE_COHERENCE: umount_ms=$umount_ms"
echo 'STEP35_CACHE_COHERENCE_PASS'
