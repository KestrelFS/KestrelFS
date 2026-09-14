#!/bin/bash
# Phase 4 Step 19 cache-device/superblock verification for virtme-ng.
set -euo pipefail

image=/tmp/kestrel-cache.img
regular=/tmp/kestrel-cache.regular
mnt=/tmp/mnt-kestrelfs
data_dir=/tmp/kestrelfs-debug
loopdev=
daemon_pid=

cleanup() {
	set +e
	mountpoint -q "$mnt" && busybox umount "$mnt"
	if [ -n "$daemon_pid" ]; then
		kill "$daemon_pid" 2>/dev/null
		wait "$daemon_pid" 2>/dev/null
	fi
	test -d /sys/module/kestrelfs && rmmod kestrelfs
	if [ -n "$loopdev" ]; then
		losetup -d "$loopdev" 2>/dev/null
	fi
	rm -f "$image" "$regular"
}
trap cleanup EXIT

rm -rf "$data_dir"
rm -f "$image" "$regular"
mkdir -p "$data_dir" "$mnt"
: >"$regular"

if insmod kestrelfs/kestrelfs.ko cache_device="$regular"; then
	echo "STEP19_FAIL: regular file was accepted as cache_device"
	exit 1
fi
test ! -d /sys/module/kestrelfs
echo "STEP19_CACHE: regular file rejected"

truncate -s 128M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
echo "STEP19_CACHE: loop_device=$loopdev"

dmesg -c >/dev/null || true
insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64
dmesg >/tmp/step19-dmesg
grep -F "formatted cache device=$loopdev" /tmp/step19-dmesg

./daemon/target/release/kestrelfs-daemon --data-dir /tmp/kestrelfs-debug >/dev/null 2>&1 &
daemon_pid=$!
sleep 1
busybox mount -t kestrelfs none "$mnt"
printf 'step19 still uses READ_DATA\n' >"$mnt/step19.dat"
test "$(cat "$mnt/step19.dat")" = "step19 still uses READ_DATA"
start_ns=$(date +%s%N)
busybox umount "$mnt"
end_ns=$(date +%s%N)
umount_ms=$(( (end_ns - start_ns) / 1000000 ))
test "$umount_ms" -lt 1000
kill "$daemon_pid"
wait "$daemon_pid" || true
daemon_pid=
rmmod kestrelfs
first_hash=$(dd if="$loopdev" bs=4096 count=1 status=none | sha256sum | awk '{print $1}')
test -n "$first_hash"
echo "STEP19_CACHE: format_mount_umount_ms=$umount_ms"

dmesg -c >/dev/null || true
insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64
dmesg >/tmp/step19-dmesg
grep -F "reusing cache device=$loopdev" /tmp/step19-dmesg
second_hash=$(dd if="$loopdev" bs=4096 count=1 status=none | sha256sum | awk '{print $1}')
echo "STEP19_CACHE: first_hash=$first_hash second_hash=$second_hash"
test "$second_hash" = "$first_hash"
echo "STEP19_CACHE: hash stable"
rmmod kestrelfs
echo "STEP19_CACHE: existing superblock reused"

if insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=32; then
	echo "STEP19_FAIL: mismatched geometry was accepted"
	exit 1
fi
test ! -d /sys/module/kestrelfs
echo "STEP19_CACHE: mismatched geometry rejected"

printf 'BADMAGIC' | dd of="$loopdev" bs=1 conv=notrunc status=none
if insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64; then
	echo "STEP19_FAIL: corrupt superblock was accepted"
	exit 1
fi
test ! -d /sys/module/kestrelfs
echo "STEP19_CACHE: corrupt superblock rejected"

dd if=/dev/zero of="$loopdev" bs=1M count=2 conv=notrunc \
	oflag=direct,dsync status=none
dmesg -c >/dev/null || true
insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=0
dmesg >/tmp/step19-dmesg
grep -F "cache geometry bytes=134217728" /tmp/step19-dmesg
rmmod kestrelfs
echo "STEP19_CACHE: cache_size_mib=0 used full device"

losetup -d "$loopdev"
loopdev=
rm -f "$image" "$regular"
trap - EXIT
echo "STEP19_CACHE_PASS"
