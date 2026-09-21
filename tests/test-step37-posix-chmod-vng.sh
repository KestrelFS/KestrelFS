#!/bin/bash
# Step 37 POSIX-CHMOD: persistent file/directory mode setattr and orphan fchmod.
set -euo pipefail
# shellcheck source=_repo_root.sh
# Keep cwd at repo root so daemon/ and kestrelfs/ relative paths stay stable.
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"

test_id=$$
image=/tmp/kestrel-step37-cache-$test_id.img
mnt=/tmp/mnt-kestrelfs-step37-$test_id
data_dir=/tmp/kestrelfs-step37-$test_id
helper=/tmp/kestrel-step37-posix-chmod-$test_id
loopdev=
daemon_pid=
namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
	"$data_dir" "$data_dir" | sha256sum | awk '{print $1}')

report_error() {
	local status=$?
	echo "STEP37_FAIL: line=$1 status=$status"
	test -f "$data_dir/daemon.log" && tail -n 140 "$data_dir/daemon.log"
	dmesg | tail -n 160
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
	if test -n "$daemon_pid"; then
		kill "$daemon_pid"
		wait "$daemon_pid" || true
		daemon_pid=
	fi
}

cleanup() {
	set +e
	mountpoint -q "$mnt" && busybox umount "$mnt"
	stop_daemon
	test -d /sys/module/kestrelfs && rmmod kestrelfs
	if test -n "$loopdev"; then
		losetup -d "$loopdev" 2>/dev/null
	fi
	rm -f "$image" "$helper"
}
trap cleanup EXIT

rm -rf "$data_dir" "$mnt"
rm -f "$image" "$helper"
mkdir -p "$data_dir" "$mnt"
cc -O2 -std=c11 -D_DEFAULT_SOURCE -Wall -Wextra -Werror \
	tests/test-step37-posix-chmod.c -o "$helper"
truncate -s 128M "$image"
modprobe loop 2>/dev/null || true
test -c /dev/loop-control || mknod /dev/loop-control c 10 237
for minor in 0 1 2 3 4 5 6 7; do
	test -b "/dev/loop$minor" || mknod "/dev/loop$minor" b 7 "$minor"
done
loopdev=$(losetup -fP --show "$image")
test -b "$loopdev"
echo "STEP37_POSIX_CHMOD: loop_device=$loopdev namespace=$namespace"

# Module, cache device, and mount exist only in this vng guest.
insmod kestrelfs/kestrelfs.ko cache_device="$loopdev" cache_size_mib=64 \
	cache_namespace="$namespace"
start_daemon
busybox mount -t kestrelfs none "$mnt"

printf 'step37 persistent chmod data\n' >"$mnt/chmod-file-long-name"
mkdir "$mnt/chmod-directory-long-name"
ln "$mnt/chmod-file-long-name" "$mnt/chmod-file-alias"
chmod 6751 "$mnt/chmod-file-long-name"
chmod 1770 "$mnt/chmod-directory-long-name"
test -f "$mnt/chmod-file-long-name"
test "$(stat -c %a "$mnt/chmod-file-long-name")" = 6751
test "$(stat -c %a "$mnt/chmod-file-alias")" = 6751
test -d "$mnt/chmod-directory-long-name"
test "$(stat -c %a "$mnt/chmod-directory-long-name")" = 1770
echo 'STEP37_FILE_DIR_CHMOD_PASS'

# An open-unlinked inode remains metadata-addressable until final close, so
# fchmod updates the retained orphan; close then uses Step 36 finalization.
"$helper" "$mnt/chmod-open-orphan"
test ! -e "$mnt/chmod-open-orphan"

# FileMetaStore restart must retain the new modes and lookup must reconstruct
# the correct file types. Hard-link aliases observe the same inode mode.
busybox umount "$mnt"
stop_daemon
start_daemon
busybox mount -t kestrelfs none "$mnt"
test -f "$mnt/chmod-file-long-name"
test "$(stat -c %a "$mnt/chmod-file-long-name")" = 6751
test "$(stat -c %a "$mnt/chmod-file-alias")" = 6751
test "$(cat "$mnt/chmod-file-long-name")" = 'step37 persistent chmod data'
test -d "$mnt/chmod-directory-long-name"
test "$(stat -c %a "$mnt/chmod-directory-long-name")" = 1770
echo 'STEP37_CHMOD_RESTART_PASS'

rm "$mnt/chmod-file-alias" "$mnt/chmod-file-long-name"
rmdir "$mnt/chmod-directory-long-name"
dmesg >"$data_dir/dmesg.log"
if grep -E 'BUG:|KASAN:|use-after-free|general protection fault|hung task' \
	"$data_dir/dmesg.log"; then
	echo 'STEP37_FAIL: kernel safety diagnostic found'
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
echo "STEP37_POSIX_CHMOD: umount_ms=$umount_ms"
echo 'STEP37_POSIX_CHMOD_PASS'
