#!/bin/bash
# Phase 4 Step 27 offline inspect and explicitly confirmed cache wipe test.
set -euo pipefail
# shellcheck source=_repo_root.sh
# Keep cwd at repo root so daemon/ and kestrelfs/ relative paths stay stable.
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"

test_id=$$
image=/tmp/kestrel-step27-cache-$test_id.img
mnt=/tmp/mnt-kestrelfs-step27-$test_id
data_dir=/tmp/kestrelfs-step27-$test_id
admin=/tmp/kestrel-step27-cache-admin-$test_id
io_helper=/tmp/kestrel-step27-cache-io-$test_id
txn_helper=/tmp/kestrel-step27-cache-txn-$test_id
loopdev=
daemon_pid=
last_umount_ms=0
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

report_error() {
	local status=$?

	echo "STEP27_FAIL: line=$1 status=$status"
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
	insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=32 \
		cache_namespace="$namespace"
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

cleanup() {
	set +e
	unmount_and_unload
	if [ -n "$loopdev" ]; then
		losetup -d "$loopdev" 2>/dev/null
	fi
	rm -f "$image" "$admin" "$io_helper" "$txn_helper"
}
trap cleanup EXIT

rm -rf "$data_dir" "$mnt"
rm -f "$image" "$admin" "$io_helper" "$txn_helper"
mkdir -p "$data_dir" "$mnt"
cc -O2 -Wall -Wextra -Werror -std=gnu11 \
	tools/kestrelfs-cache-admin.c -o "$admin"
cc -O2 -Wall -Wextra -Werror tests/test-step22-cache-io.c -o "$io_helper"
cc -O2 -Wall -Wextra -Werror tests/test-step25-cache-txn.c -o "$txn_helper"
truncate -s 64M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
echo "STEP27_CACHE: loop_device=$loopdev namespace=$namespace"

# Format and publish one cache entry, then prove it is a real daemon-free hit.
load_cache
start_daemon
busybox mount -t kestrelfs none "$mnt"
"$io_helper" prepare "$mnt/recovery.dat" 4096
"$io_helper" verify "$mnt/recovery.dat" 0 4096 0 1
stop_daemon
"$io_helper" verify "$mnt/recovery.dat" 0 4096 0 1
unmount_and_unload

# Clean v4 inspection reports the immutable geometry, namespace and one entry.
"$admin" inspect "$loopdev" >"$data_dir/inspect-clean.log"
grep -Fx 'super_magic=0x454843414353464b' "$data_dir/inspect-clean.log"
grep -Fx 'super_version=4' "$data_dir/inspect-clean.log"
grep -Fx 'super_crc=ok' "$data_dir/inspect-clean.log"
grep -Fx "namespace=$namespace" "$data_dir/inspect-clean.log"
grep -Fx 'journal_state=clean' "$data_dir/inspect-clean.log"
grep -Fx 'index_entries=1' "$data_dir/inspect-clean.log"
grep -Fx 'index_status=ok' "$data_dir/inspect-clean.log"
grep -Fx 'overall_status=ok' "$data_dir/inspect-clean.log"

# A valid PREPARED record is diagnosable without module recovery or mutation.
"$txn_helper" prepare "$loopdev" invalidate 0 before
inspect_status=0
"$admin" inspect "$loopdev" >"$data_dir/inspect-prepared.log" || \
	inspect_status=$?
test "$inspect_status" -eq 3
grep -Fx 'journal_state=prepared' "$data_dir/inspect-prepared.log"
grep -Fx 'journal_crc=ok' "$data_dir/inspect-prepared.log"
grep -Fx 'journal_operation=invalidate' "$data_dir/inspect-prepared.log"
grep -Fx 'journal_slot=0' "$data_dir/inspect-prepared.log"
grep -Fx 'overall_status=recovery-required' \
	"$data_dir/inspect-prepared.log"

# Corrupt the journal without updating CRC; inspect must diagnose, not repair.
printf '\377' | dd of="$loopdev" bs=1 seek=$((4096 + 128)) \
	conv=notrunc status=none
sync
inspect_status=0
"$admin" inspect "$loopdev" >"$data_dir/inspect-torn.log" || \
	inspect_status=$?
test "$inspect_status" -eq 2
grep -Fx 'journal_crc=bad' "$data_dir/inspect-torn.log"
grep -Fx 'overall_status=invalid' "$data_dir/inspect-torn.log"
echo "STEP27_INSPECT_PASS"

# Either missing half of the two-part confirmation must refuse without writes.
if "$admin" wipe "$loopdev" --yes-really-wipe \
	>"$data_dir/wipe-no-env.log" 2>&1; then
	echo "STEP27_FAIL: wipe accepted without environment confirmation"
	exit 1
fi
grep -F 'wipe refused' "$data_dir/wipe-no-env.log"
if KESTRELFS_CACHE_WIPE_CONFIRM="$loopdev" \
	"$admin" wipe "$loopdev" --not-confirmed \
	>"$data_dir/wipe-no-flag.log" 2>&1; then
	echo "STEP27_FAIL: wipe accepted without command-line confirmation"
	exit 1
fi
grep -F 'wipe refused' "$data_dir/wipe-no-flag.log"
if KESTRELFS_CACHE_WIPE_CONFIRM="$image" \
	"$admin" wipe "$image" --yes-really-wipe \
	>"$data_dir/wipe-regular.log" 2>&1; then
	echo "STEP27_FAIL: wipe accepted a regular backing file"
	exit 1
fi
grep -F 'DEVICE must be a block device' "$data_dir/wipe-regular.log"
echo "STEP27_WIPE_GUARD_PASS"

# Explicit wipe clears only the 2 MiB metadata area; authoritative data remains.
KESTRELFS_CACHE_WIPE_CONFIRM="$loopdev" \
	"$admin" wipe "$loopdev" --yes-really-wipe \
	>"$data_dir/wipe.log"
grep -Fx 'wipe_status=ok' "$data_dir/wipe.log"
"$admin" inspect "$loopdev" >"$data_dir/inspect-wiped.log"
grep -Fx 'super_state=zero' "$data_dir/inspect-wiped.log"
grep -Fx 'metadata_state=zero' "$data_dir/inspect-wiped.log"
grep -Fx 'overall_status=unformatted' "$data_dir/inspect-wiped.log"
echo "STEP27_WIPE_PASS"

# The kernel can format the clean metadata again. Before refill the old cache
# cannot serve data with the daemon stopped; afterward the new entry can.
dmesg -c >/dev/null || true
load_cache
dmesg >"$data_dir/reformat-dmesg.log"
grep -F "formatted cache device=$loopdev" "$data_dir/reformat-dmesg.log"
start_daemon
busybox mount -t kestrelfs none "$mnt"
test -f "$mnt/recovery.dat"
stop_daemon
if "$io_helper" verify "$mnt/recovery.dat" 0 4096 0 1 \
	>"$data_dir/pre-refill-read.log" 2>&1; then
	echo "STEP27_FAIL: wiped cache served its old entry"
	exit 1
fi
start_daemon
"$io_helper" verify "$mnt/recovery.dat" 0 4096 0 1
stop_daemon
"$io_helper" verify "$mnt/recovery.dat" 0 4096 0 1
echo "STEP27_REFILL_PASS"

dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo "STEP27_FAIL: kernel safety diagnostic found"
	exit 1
fi
unmount_and_unload

"$admin" inspect "$loopdev" >"$data_dir/inspect-refilled.log"
grep -Fx 'journal_state=clean' "$data_dir/inspect-refilled.log"
grep -Fx 'index_entries=1' "$data_dir/inspect-refilled.log"
grep -Fx 'overall_status=ok' "$data_dir/inspect-refilled.log"

losetup -d "$loopdev"
loopdev=
trap - EXIT
rm -f "$image" "$admin" "$io_helper" "$txn_helper"
echo "STEP27_CACHE: umount_ms=$last_umount_ms"
echo "STEP27_OPS_RECOVERY_PASS"
