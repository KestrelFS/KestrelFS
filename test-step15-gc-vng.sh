#!/bin/bash
# Phase 3 Step 15 mount-level ObjectStore GC verification for virtme-ng.
set -euo pipefail

test_id=$$
mnt=/tmp/mnt-kestrelfs-step15-$test_id
data_dir=/tmp/kestrelfs-step15-$test_id
gc_before=/tmp/kestrel-step15-before-$test_id
gc_after=/tmp/kestrel-step15-after-$test_id
daemon_pid=

cleanup() {
    set +e
    mountpoint -q "$mnt" && busybox umount "$mnt"
    if [ -n "$daemon_pid" ]; then
        kill "$daemon_pid" 2>/dev/null
        wait "$daemon_pid" 2>/dev/null
    fi
    rmmod kestrelfs 2>/dev/null
	rm -f "$gc_before" "$gc_after"
}
trap cleanup EXIT

objects() {
    find "$data_dir" -type f ! -name meta.json | sort
}

new_object_since() {
    local before=$1
    local after=$2
    local found
    found=$(comm -13 "$before" "$after")
    test "$(printf '%s\n' "$found" | sed '/^$/d' | wc -l)" -eq 1
    printf '%s' "$found"
}

rm -rf "$data_dir"
mkdir -p "$data_dir" "$mnt"
insmod kestrelfs/kestrelfs.ko
./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
	>"$data_dir/daemon.log" 2>&1 &
daemon_pid=$!
sleep 1
kill -0 "$daemon_pid"
busybox mount -t kestrelfs none "$mnt"

echo "VNG_GC: unlink"
objects >"$gc_before"
printf 'unlink-object-payload' >"$mnt/unlink.dat"
objects >"$gc_after"
unlink_object=$(new_object_since "$gc_before" "$gc_after")
test -f "$unlink_object"
rm "$mnt/unlink.dat"
test ! -e "$unlink_object"
echo "VNG_GC: unlink removed $unlink_object"

echo "VNG_GC: rename overwrite"
objects >"$gc_before"
printf 'replace-me' >"$mnt/rename-target.dat"
objects >"$gc_after"
rename_object=$(new_object_since "$gc_before" "$gc_after")
touch "$mnt/rename-source.dat"
mv "$mnt/rename-source.dat" "$mnt/rename-target.dat"
test ! -e "$rename_object"
echo "VNG_GC: rename removed $rename_object"

echo "VNG_GC: truncate COW"
objects >"$gc_before"
head -c 100 /dev/zero | tr '\0' A >"$mnt/truncate.dat"
objects >"$gc_after"
older_object=$(new_object_since "$gc_before" "$gc_after")
cp "$gc_after" "$gc_before"
printf 'BBBBBBBBBBBBBBBBBBBB' | dd of="$mnt/truncate.dat" bs=20 seek=4 conv=notrunc status=none
objects >"$gc_after"
newer_object=$(new_object_since "$gc_before" "$gc_after")
test -f "$older_object"
test -f "$newer_object"
truncate -s 80 "$mnt/truncate.dat"
test -f "$older_object"
test ! -e "$newer_object"
test "$(wc -c <"$mnt/truncate.dat")" -eq 80
truncate -s 100 "$mnt/truncate.dat"
tail -c 20 "$mnt/truncate.dat" | cmp - <(head -c 20 /dev/zero)
echo "VNG_GC: truncate kept $older_object and removed $newer_object"

rm "$mnt/truncate.dat" "$mnt/rename-target.dat"
start_ns=$(date +%s%N)
busybox umount "$mnt"
end_ns=$(date +%s%N)
umount_ms=$(( (end_ns - start_ns) / 1000000 ))
test "$umount_ms" -lt 1000
echo "VNG_GC: umount_ms=$umount_ms"

kill "$daemon_pid"
wait "$daemon_pid" || true
daemon_pid=
rmmod kestrelfs
trap - EXIT
rm -f "$gc_before" "$gc_after"
echo "VNG_GC_PASS"
