#!/bin/bash
# Step 55: precise READDIR_DATA d_type plus restricted persistent 0:0 mknod.
# Run only inside a vng guest.
set -euo pipefail
# shellcheck source=_repo_root.sh
# Keep cwd at repo root so daemon/ and kestrelfs/ relative paths stay stable.
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"

test_id=$$
image=/tmp/kestrel-step55-$test_id.img
mnt=/tmp/mnt-kestrelfs-step55-$test_id
data_dir=/tmp/kestrelfs-step55-$test_id
helper=/tmp/kestrel-step55-helper-$test_id
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

fail() {
	local status=$?
	echo "STEP55_POSIX_FAIL: line=$1 status=$status"
	test ! -f "$data_dir/daemon.log" || tail -n 160 "$data_dir/daemon.log"
	dmesg | tail -n 120
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

start_daemon() {
	./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
		>"$data_dir/daemon.log" 2>&1 &
	daemon_pid=$!
	sleep 1
	kill -0 "$daemon_pid"
}

cleanup() {
	set +e
	mountpoint -q "$mnt" && busybox umount "$mnt"
	stop_daemon
	test -d /sys/module/kestrelfs && rmmod kestrelfs
	if test -n "$loopdev"; then losetup -d "$loopdev" 2>/dev/null; fi
	rm -f "$image" "$helper"
}
trap cleanup EXIT

mkdir -p "$data_dir" "$mnt"
cc -O2 -Wall -Wextra -Werror -std=gnu11 tests/test-step55-dtype-mknod.c -o "$helper"
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

printf 'dtype-data\n' >"$mnt/dtype-file"
mkdir "$mnt/dtype-dir"
ln -s dtype-file "$mnt/dtype-link"
"$helper" create "$mnt"
"$helper" check "$mnt"
echo STEP55_DTYPE_INITIAL_PASS
echo STEP55_MKNOD_RESTRICT_PASS

# Make namespace creation writable for uid 65534 so EPERM specifically proves
# CAP_MKNOD enforcement rather than ordinary directory DAC denial.
chmod 0777 "$mnt"
"$helper" unpriv "$mnt"
test ! -e "$mnt/unpriv-whiteout"
echo STEP55_MKNOD_CAP_PASS

busybox umount "$mnt"
stop_daemon
start_daemon
busybox mount -t kestrelfs none "$mnt"
"$helper" check "$mnt"
test "$(stat -c '%F:%t:%T' "$mnt/dtype-whiteout")" = \
	'character special file:0:0'
test "$(cat "$mnt/dtype-file")" = dtype-data
test "$(readlink "$mnt/dtype-link")" = dtype-file
echo STEP55_DTYPE_RESTART_PASS
echo STEP55_MKNOD_RESTART_PASS

dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo STEP55_POSIX_FAIL_KERNEL_DIAGNOSTIC
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
rm -f "$image" "$helper"
echo "STEP55_POSIX: umount_ms=$umount_ms"
echo STEP55_POSIX_DTYPE_MKNOD_PASS
