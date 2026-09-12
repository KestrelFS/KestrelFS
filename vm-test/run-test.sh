#!/bin/bash
set -e

echo "=========================================="
echo "KestrelFS 虚拟机隔离测试"
echo "=========================================="

# 挂载 9p 共享目录
mount -t 9p -o trans=virtio,version=9p2000.L hostshare /mnt/host

cd /mnt/host

# 1. 加载模块
echo "✅ 加载内核模块..."
insmod kestrelfs.ko
lsmod | grep kestrelfs

# 2. 启动 daemon（后台）
echo "✅ 启动 daemon..."
./kestrelfs-daemon --memory &
DAEMON_PID=$!
sleep 1

# 3. 挂载文件系统
echo "✅ 挂载文件系统..."
mkdir -p /mnt/kestrelfs
mount -t kestrelfs none /mnt/kestrelfs

# 4. 测试 Bug B：长文件名
echo "✅ 测试 Bug B（长文件名显示）..."
ls -la /mnt/kestrelfs/
cat /mnt/kestrelfs/remote.txt
echo ""
echo "❓ remote.txt 文件名是否完整显示？（应该是 'remote.txt'，不是 'remote.tx'）"

# 5. 写入测试文件
echo "✅ 写入测试文件..."
for i in {1..20}; do
    echo "content $i" > /mnt/kestrelfs/file$i
done

# 6. 验证写入
echo "✅ 验证写入..."
cat /mnt/kestrelfs/file10

# 7. 测试 Bug A：umount 是否卡死
echo "✅ 测试 Bug A（umount 卡死问题）..."
echo "开始 umount（如果 5 秒内未完成则失败）..."

# 使用 timeout 防止永久卡住
if timeout 5 umount /mnt/kestrelfs; then
    echo "✅✅✅ umount 成功！Bug A 已修复！"
else
    echo "❌❌❌ umount 超时或失败！Bug A 仍然存在！"
    # 不要 exit 1，让虚拟机正常关闭
fi

# 8. 卸载模块
echo "✅ 卸载模块..."
kill $DAEMON_PID 2>/dev/null || true
sleep 1
rmmod kestrelfs

# 9. 检查内核日志
echo "✅ 内核日志（最后 30 行）："
dmesg | tail -30

echo "=========================================="
echo "测试完成，虚拟机将自动关闭"
echo "=========================================="

# 关闭虚拟机
poweroff
