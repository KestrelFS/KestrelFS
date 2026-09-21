#!/bin/bash
# Phase 4 Step 20/21 persistent cache and namespace test for virtme-ng.
set -euo pipefail
# shellcheck source=_repo_root.sh
# Keep cwd at repo root so daemon/ and kestrelfs/ relative paths stay stable.
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"

image=/tmp/kestrel-step20-cache.img
mnt=/tmp/mnt-kestrelfs
data_dir=/tmp/kestrelfs-step20-$$
expected=/tmp/kestrel-step20.expected
actual=/tmp/kestrel-step20.actual
loopdev=
daemon_pid=
namespace_a=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')
namespace_b=$(printf 'v1;meta=file:%s-other/meta.json;objects=local:%s-other' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

start_daemon() {
	./daemon/target/release/kestrelfs-daemon \
		--data-dir "$data_dir" >"$data_dir/daemon.log" 2>&1 &
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

expect_fd_cache_miss() {
	if cat <&3 >"$actual" 2>/tmp/kestrel-step20-read.err; then
		echo "STEP20_FAIL: invalidated fd returned cached data"
		exit 1
	fi
	exec 3<&-
}

cleanup() {
	set +e
	exec 3<&- 2>/dev/null
	mountpoint -q "$mnt" && busybox umount "$mnt"
	stop_daemon
	test -d /sys/module/kestrelfs && rmmod kestrelfs
	if [ -n "$loopdev" ]; then
		losetup -d "$loopdev" 2>/dev/null
	fi
	rm -f "$image" "$expected" "$actual"
}
trap cleanup EXIT

rm -rf "$data_dir"
rm -f "$image" "$expected" "$actual"
mkdir -p "$data_dir" "$mnt"
truncate -s 128M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
echo "STEP20_CACHE: loop_device=$loopdev"

dmesg -c >/dev/null || true
insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
	cache_namespace="$namespace_a"
start_daemon
busybox mount -t kestrelfs none "$mnt"

# Use more than one cache block so restore and multi-block hit are exercised.
{
	printf 'step20-persistent-cache\n'
	head -c 12288 /dev/zero | tr '\0' P
} >"$expected"
cp "$expected" "$mnt/persist.dat"
cat "$mnt/persist.dat" >"$actual"
cmp "$expected" "$actual"
echo "STEP20_CACHE: miss filled multiple blocks"

start_ns=$(date +%s%N)
busybox umount "$mnt"
end_ns=$(date +%s%N)
umount_ms=$(( (end_ns - start_ns) / 1000000 ))
test "$umount_ms" -lt 1000
stop_daemon
rmmod kestrelfs

# A cache populated for namespace A must fail closed before index restore when
# the same device is presented as namespace B.
dmesg -c >/dev/null || true
if insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
	cache_namespace="$namespace_b"; then
	echo "STEP21_FAIL: namespace B accepted namespace A cache"
	exit 1
fi
test ! -d /sys/module/kestrelfs
dmesg >/tmp/kestrel-step21-dmesg
grep -F 'cache namespace identity mismatch' /tmp/kestrel-step21-dmesg
echo "STEP21_CACHE: mismatched namespace rejected"

dmesg -c >/dev/null || true
insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
	cache_namespace="$namespace_a"
start_daemon
busybox mount -t kestrelfs none "$mnt"
exec 3<"$mnt/persist.dat"
stop_daemon
cmp "$expected" <&3
exec 3<&-
dmesg >/tmp/kestrel-step20-dmesg
grep -E 'reusing cache device=.*|restored [1-9][0-9]* cache index entries' \
	/tmp/kestrel-step20-dmesg
echo "STEP20_CACHE: reload served persisted hit with daemon stopped"

# Rewriting must remove all old blocks before the authoritative mutation.
start_daemon
printf 'rewrite-invalidates-old-cache\n' >"$mnt/persist.dat"
exec 3<"$mnt/persist.dat"
stop_daemon
expect_fd_cache_miss
start_daemon
test "$(cat "$mnt/persist.dat")" = "rewrite-invalidates-old-cache"
echo "STEP20_CACHE: rewrite invalidation passed"

# Truncate invalidates the inode, including blocks wholly below the new EOF.
exec 3<"$mnt/persist.dat"
truncate -s 12 "$mnt/persist.dat"
stop_daemon
expect_fd_cache_miss
start_daemon
test "$(cat "$mnt/persist.dat")" = "rewrite-inva"
echo "STEP20_CACHE: truncate invalidation passed"

# Atomic rename replacement invalidates the overwritten inode held by an fd.
printf 'old-rename-target\n' >"$mnt/rename-target.dat"
cat "$mnt/rename-target.dat" >"$actual"
printf 'new-rename-source\n' >"$mnt/rename-source.dat"
exec 3<"$mnt/rename-target.dat"
mv "$mnt/rename-source.dat" "$mnt/rename-target.dat"
stop_daemon
expect_fd_cache_miss
start_daemon
test "$(cat "$mnt/rename-target.dat")" = "new-rename-source"
echo "STEP20_CACHE: rename-overwrite invalidation passed"

# Unlink invalidates blocks even while the VFS inode remains pinned by an fd.
printf 'unlink-cache-target\n' >"$mnt/unlink.dat"
cat "$mnt/unlink.dat" >"$actual"
exec 3<"$mnt/unlink.dat"
rm "$mnt/unlink.dat"
stop_daemon
expect_fd_cache_miss
start_daemon
test ! -e "$mnt/unlink.dat"
echo "STEP20_CACHE: unlink invalidation passed"

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
rm -f "$image" "$expected" "$actual"
echo "STEP20_CACHE: umount_ms=$umount_ms"
echo "STEP21_NAMESPACE_PASS"
echo "STEP20_CACHE_PASS"
