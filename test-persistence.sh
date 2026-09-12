#!/bin/bash
# Phase 3 Step 8: 元数据持久化验证测试
#
# 验证目标：
# 1. daemon 写入文件后，meta.json 和块数据存在
# 2. daemon 重启后，文件仍可读取（元数据和块数据恢复成功）

set -ex

# 清理之前的状态
sudo umount /mnt/kestrelfs 2>/dev/null || true
sudo rmmod kestrelfs 2>/dev/null || true
pkill -f kestrelfs-daemon || true
sleep 1

# 创建测试目录
TEST_DATA=/tmp/kestrel_persist_test_$$
mkdir -p $TEST_DATA
sudo mkdir -p /mnt/kestrelfs

echo "=========================================="
echo "Phase 1: 写入文件并验证持久化"
echo "=========================================="

# 加载模块
sudo insmod kestrelfs/kestrelfs.ko

# 启动 daemon（持久化模式）
./daemon/target/release/kestrelfs-daemon --data-dir $TEST_DATA &
DAEMON_PID=$!
sleep 2

# 挂载
sudo mount -t kestrelfs none /mnt/kestrelfs

# 写入测试文件
echo "Hello, persistent world!" | sudo tee /mnt/kestrelfs/persistent.txt
echo "Second file content" | sudo tee /mnt/kestrelfs/file2.txt
echo "Data in file3" | sudo tee /mnt/kestrelfs/file3.txt

echo ""
echo "=== Files written ==="
sudo ls -la /mnt/kestrelfs/
sudo cat /mnt/kestrelfs/persistent.txt

# 检查元数据文件
echo ""
echo "=== Metadata file ==="
ls -lh $TEST_DATA/meta.json
echo ""
echo "=== Block data directory ==="
ls -lR $TEST_DATA/ | head -20

# 卸载并停止 daemon
sudo umount /mnt/kestrelfs
kill $DAEMON_PID
wait $DAEMON_PID 2>/dev/null || true
sleep 2

echo ""
echo "=========================================="
echo "Phase 2: 重启 daemon 并验证数据恢复"
echo "=========================================="

# 重启 daemon（加载持久化数据）
./daemon/target/release/kestrelfs-daemon --data-dir $TEST_DATA &
DAEMON_PID=$!
sleep 2

# 重新挂载
sudo mount -t kestrelfs none /mnt/kestrelfs

# 验证数据仍在
echo "=== Files after restart ==="
sudo ls -la /mnt/kestrelfs/

echo ""
echo "=== Verifying file contents ==="
echo -n "persistent.txt: "
sudo cat /mnt/kestrelfs/persistent.txt

echo -n "file2.txt: "
sudo cat /mnt/kestrelfs/file2.txt

echo -n "file3.txt: "
sudo cat /mnt/kestrelfs/file3.txt

# 额外验证：创建新文件后再次重启
echo ""
echo "=== Creating new file after restart ==="
echo "New file after restart" | sudo tee /mnt/kestrelfs/file4.txt

sudo umount /mnt/kestrelfs
kill $DAEMON_PID
wait $DAEMON_PID 2>/dev/null || true
sleep 2

# 第三次启动
./daemon/target/release/kestrelfs-daemon --data-dir $TEST_DATA &
DAEMON_PID=$!
sleep 2
sudo mount -t kestrelfs none /mnt/kestrelfs

echo ""
echo "=== All files after second restart ==="
sudo ls -la /mnt/kestrelfs/
sudo cat /mnt/kestrelfs/file4.txt

# 清理
sudo umount /mnt/kestrelfs
kill $DAEMON_PID
wait $DAEMON_PID 2>/dev/null || true
sudo rmmod kestrelfs

echo ""
echo "=========================================="
echo "✅ Persistence test PASSED!"
echo "=========================================="
echo ""
echo "Metadata was persisted to: $TEST_DATA/meta.json"
echo "Block data was persisted to: $TEST_DATA/<slice_uuids>/"
echo ""
echo "Cleanup: rm -rf $TEST_DATA"
