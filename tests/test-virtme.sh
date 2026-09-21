#!/bin/bash
# shellcheck source=_repo_root.sh
# Keep cwd at repo root so daemon/ and kestrelfs/ relative paths stay stable.
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"
# KestrelFS virtme-ng 测试脚本（无需 sudo）

set -e

echo "=== 编译内核模块和 daemon ==="
make -C kestrelfs clean && make -C kestrelfs
cargo build --release --manifest-path daemon/Cargo.toml

echo ""
echo "=== 创建测试脚本 ==="
cat > /tmp/kestrel_test_in_vm.sh << 'VMTEST'
#!/bin/bash
set -ex

# 加载模块
insmod kestrelfs/kestrelfs.ko
lsmod | grep kestrelfs

# 准备测试目录
mkdir -p /tmp/test_data /mnt/kestrelfs

# 启动 daemon（持久化模式）
./daemon/target/release/kestrelfs-daemon --data-dir /tmp/test_data &
DAEMON_PID=$!
sleep 2

# 挂载文件系统
mount -t kestrelfs none /mnt/kestrelfs
mount | grep kestrelfs

echo "=== 第一阶段：写入测试文件 ==="
echo "Persistent data from virtme" > /mnt/kestrelfs/test.txt
echo "Another file" > /mnt/kestrelfs/file2.txt
ls -la /mnt/kestrelfs/
cat /mnt/kestrelfs/test.txt

echo ""
echo "=== 检查持久化文件 ==="
ls -lh /tmp/test_data/
test -f /tmp/test_data/meta.json || (echo "ERROR: meta.json not found" && exit 1)
echo "meta.json 前 10 行："
head -10 /tmp/test_data/meta.json

echo ""
echo "=== 第一次重启：停止 daemon ==="
umount /mnt/kestrelfs
kill $DAEMON_PID
sleep 1

echo ""
echo "=== 第一次重启：启动 daemon 并验证数据恢复 ==="
./daemon/target/release/kestrelfs-daemon --data-dir /tmp/test_data &
DAEMON_PID=$!
sleep 2

mount -t kestrelfs none /mnt/kestrelfs
echo "文件列表（应包含 test.txt 和 file2.txt）："
ls -la /mnt/kestrelfs/

echo ""
echo "读取 test.txt 内容（应输出 'Persistent data from virtme'）："
cat /mnt/kestrelfs/test.txt

echo ""
echo "读取 file2.txt 内容（应输出 'Another file'）："
cat /mnt/kestrelfs/file2.txt

echo ""
echo "=== 第二次写入：创建新文件 ==="
echo "Third file after restart" > /mnt/kestrelfs/file3.txt

echo ""
echo "=== 第二次重启：验证所有文件 ==="
umount /mnt/kestrelfs
kill $DAEMON_PID
sleep 1

./daemon/target/release/kestrelfs-daemon --data-dir /tmp/test_data &
DAEMON_PID=$!
sleep 2

mount -t kestrelfs none /mnt/kestrelfs
echo "最终文件列表（应包含 3 个文件）："
ls -la /mnt/kestrelfs/
cat /mnt/kestrelfs/file3.txt

echo ""
echo "=== 清理 ==="
umount /mnt/kestrelfs
kill $DAEMON_PID
rmmod kestrelfs

echo ""
echo "✅ 所有持久化测试通过！"
VMTEST

chmod +x /tmp/kestrel_test_in_vm.sh

echo ""
echo "=== 启动 virtme-ng 虚拟机并运行测试 ==="
virtme-ng --exec /tmp/kestrel_test_in_vm.sh

echo ""
echo "✅ virtme-ng 测试完成"
