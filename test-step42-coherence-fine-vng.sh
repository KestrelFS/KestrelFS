#!/bin/bash
# Step 42 COHERENCE-FINE: bounded Redis dirty log -> inode cache invalidation.
set -euo pipefail

: "${REDIS_URL:?set REDIS_URL to a disposable Redis database before running}"

test_id=$$
image=/tmp/kestrel-step42-cache-$test_id.img
mnt=/tmp/mnt-kestrelfs-step42-$test_id
data_dir=/tmp/kestrelfs-step42-$test_id
stable=/tmp/kestrel-step42-stable-$test_id
old=/tmp/kestrel-step42-old-$test_id
new=/tmp/kestrel-step42-new-$test_id
actual=/tmp/kestrel-step42-actual-$test_id
prefix=kestrelfs:step42:$test_id
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=redis:step42#%s;objects=local:%s' \
	"$prefix" "$data_dir" | sha256sum | awk '{print $1}')

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
	echo "STEP42_FAIL: line=$1 status=$status"
	test -f "$data_dir/daemon.log" && tail -n 180 "$data_dir/daemon.log"
	dmesg | tail -n 180
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
	local old_value=$2
	local attempt current

	for attempt in $(seq 1 100); do
		current=$(cat "$path")
		if test "$current" -gt "$old_value"; then
			return 0
		fi
		sleep 0.05
	done
	echo "counter did not advance: path=$path old=$old_value current=$current"
	return 1
}

wait_for_revision() {
	local revision=$1
	local attempt

	for attempt in $(seq 1 100); do
		if grep -F -- "-> $revision;" "$data_dir/daemon.log" >/dev/null; then
			return 0
		fi
		sleep 0.05
	done
	echo "daemon did not observe revision $revision"
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
	rm -f "$image" "$stable" "$old" "$new" "$actual"
}
trap cleanup EXIT

if ! ip -4 route show default | grep -q '^default '; then
	ip -4 route add default via 10.0.2.2 dev eth0
fi

redis_cmd PING | grep -Fx PONG
redis_cleanup
rm -rf "$data_dir" "$mnt"
rm -f "$image" "$stable" "$old" "$new" "$actual"
mkdir -p "$data_dir" "$mnt"
head -c 4096 /dev/zero | tr '\0' S >"$stable"
head -c 4096 /dev/zero | tr '\0' A >"$old"
head -c 4096 /dev/zero | tr '\0' B >"$new"
truncate -s 128M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
echo "STEP42_COHERENCE_FINE: loop_device=$loopdev namespace=$namespace prefix=$prefix"

insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
	cache_namespace="$namespace"
start_daemon
busybox mount -t kestrelfs none "$mnt"

full_counter=/sys/module/kestrelfs/parameters/cache_coherence_invalidations
inode_counter=/sys/module/kestrelfs/parameters/cache_coherence_inode_batches

# Local writes publish bounded dirty records too. Wait until the daemon has
# advanced through both revisions before warming stable and changed entries.
cp "$stable" "$mnt/stable.bin"
cp "$old" "$mnt/changed.bin"
local_revision=$(redis_cmd HGET "$prefix:meta:v2:control" revision)
wait_for_revision "$local_revision"
cat "$mnt/stable.bin" >"$actual"
cmp "$stable" "$actual"
cat "$mnt/stable.bin" >"$actual"
cmp "$stable" "$actual"
cat "$mnt/changed.bin" >"$actual"
cmp "$old" "$actual"
cat "$mnt/changed.bin" >"$actual"
cmp "$old" "$actual"

# Simulate a second writer's normal object-first/COW metadata commit and its
# atomically attached dirty-inode record. Only changed.bin may be retired.
changed_inode=$(stat -c %i "$mnt/changed.bin")
control_key=$prefix:meta:v2:control
inodes_key=$prefix:meta:v2:inodes
slices_key=$prefix:meta:v2:slices
dirty_key=$prefix:meta:v2:dirty
slice_field=$changed_inode:0
old_revision=$(redis_cmd HGET "$control_key" revision)
new_revision=$((old_revision + 1))
old_slices=$(redis_cmd HGET "$slices_key" "$slice_field")
old_inode=$(redis_cmd HGET "$inodes_key" "$changed_inode")
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
cp "$new" "$data_dir/$new_uuid/0"
sync "$data_dir/$new_uuid/0"

remote_commit='local r=redis.call("HGET",KEYS[1],"revision"); if r~=ARGV[1] then return 0 end; redis.call("HSET",KEYS[2],ARGV[3],ARGV[4]); redis.call("HSET",KEYS[3],ARGV[5],ARGV[6]); redis.call("HSET",KEYS[4],ARGV[2],ARGV[7]); redis.call("HSET",KEYS[1],"revision",ARGV[2]); return 1'
inode_before=$(cat "$inode_counter")
full_before=$(cat "$full_counter")
dirty_record=$(printf '{"overflow":false,"inode_ids":[%s]}' "$changed_inode")
commit_result=$(redis_cmd EVAL "$remote_commit" 4 \
	"$control_key" "$inodes_key" "$slices_key" "$dirty_key" \
	"$old_revision" "$new_revision" "$changed_inode" "$new_inode" \
	"$slice_field" "$new_slices" "$dirty_record")
test "$commit_result" = 1
wait_for_counter_gt "$inode_counter" "$inode_before"
wait_for_revision "$new_revision"
test "$(cat "$full_counter")" = "$full_before"

# stable.bin survives as a kernel hit. changed.bin's first read must miss and
# fetch B, while its second read becomes a hit after refill.
hits_before=$(cache_hits)
cat "$mnt/stable.bin" >"$actual"
cmp "$stable" "$actual"
hits_after=$(cache_hits)
test "$hits_after" -gt "$hits_before"
echo "STEP42_UNCHANGED_INODE_HIT_PASS hits_delta=$((hits_after - hits_before))"

hits_before=$(cache_hits)
cat "$mnt/changed.bin" >"$actual"
cmp "$new" "$actual"
hits_after=$(cache_hits)
test "$hits_after" = "$hits_before"
cat "$mnt/changed.bin" >"$actual"
cmp "$new" "$actual"
test "$(cache_hits)" -gt "$hits_after"
echo 'STEP42_CHANGED_INODE_INVALIDATED_PASS'

# Advance revision without a corresponding dirty record. The bounded history
# is unavailable, so the poller must use the retained invalidate-all fallback.
fallback_revision=$((new_revision + 1))
full_before=$(cat "$full_counter")
redis_cmd HSET "$control_key" revision "$fallback_revision" >/dev/null
wait_for_counter_gt "$full_counter" "$full_before"
wait_for_revision "$fallback_revision"
grep -F 'dirty history unavailable/overflowed, full local cache invalidated' \
	"$data_dir/daemon.log" >/dev/null
echo 'STEP42_FAILURE_FALLBACK_ALL_PASS'

dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo 'STEP42_FAIL: kernel safety diagnostic found'
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
redis_cleanup
trap - EXIT
rm -f "$image" "$stable" "$old" "$new" "$actual"
echo "STEP42_COHERENCE_FINE: umount_ms=$umount_ms"
echo 'STEP42_COHERENCE_FINE_PASS'
