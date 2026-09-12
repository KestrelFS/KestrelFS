#!/bin/bash
# KestrelFS QEMU 交互式测试（集成 busybox + 9p 共享）

set -e

# 捕获退出信号，自动 reset 终端
trap 'reset 2>/dev/null || true' EXIT

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
KERNEL="/boot/vmlinuz-$(uname -r)"

if [[ ! -f "$KERNEL" ]]; then
    echo "❌ 找不到内核: $KERNEL"
    exit 1
fi

# 下载静态 busybox
BUSYBOX_URL="https://busybox.net/downloads/binaries/1.35.0-x86_64-linux-musl/busybox"
BUSYBOX_BIN="/tmp/busybox-static"

if [[ ! -f "$BUSYBOX_BIN" ]]; then
    echo "📦 下载静态 busybox..."
    wget -q -O "$BUSYBOX_BIN" "$BUSYBOX_URL"
    chmod +x "$BUSYBOX_BIN"
fi

# 编译模块和 daemon（静态链接）
echo "🔨 编译模块和 daemon（静态链接）..."
cd "$SCRIPT_DIR/kestrelfs" && make
cd "$SCRIPT_DIR/daemon"
# 安装 musl 工具链（如果没有）
if ! rustup target list --installed | grep -q x86_64-unknown-linux-musl; then
    echo "📦 安装 musl 工具链..."
    rustup target add x86_64-unknown-linux-musl
fi
cargo build --release --target x86_64-unknown-linux-musl
cd "$SCRIPT_DIR"

# 创建 initramfs
INITRAMFS_DIR="/tmp/kestrelfs-initramfs-$$"
mkdir -p "$INITRAMFS_DIR"/{bin,sbin,proc,sys,dev,work}

echo "📦 创建 initramfs（集成 busybox）..."

# 安装 busybox
cp "$BUSYBOX_BIN" "$INITRAMFS_DIR/bin/busybox"
cd "$INITRAMFS_DIR/bin"
for cmd in sh ash bash ls cat mount umount mkdir echo sleep kill ps top time insmod rmmod lsmod dmesg poweroff reboot; do
    ln -sf busybox "$cmd"
done
cd "$SCRIPT_DIR"

# 复制编译好的模块和 daemon 到 initramfs
echo "📦 复制 kestrelfs.ko 和 daemon 到 initramfs..."
cp "$SCRIPT_DIR/kestrelfs/kestrelfs.ko" "$INITRAMFS_DIR/work/"
cp "$SCRIPT_DIR/daemon/target/x86_64-unknown-linux-musl/release/kestrelfs-daemon" "$INITRAMFS_DIR/work/"
chmod +x "$INITRAMFS_DIR/work/kestrelfs-daemon"

# 复制测试脚本
if [[ -f "$SCRIPT_DIR/vm-auto-test.sh" ]]; then
    cp "$SCRIPT_DIR/vm-auto-test.sh" "$INITRAMFS_DIR/work/"
    chmod +x "$INITRAMFS_DIR/work/vm-auto-test.sh"
fi

# 创建 init 脚本（启动到 shell）
cat > "$INITRAMFS_DIR/init" << 'INITEOF'
#!/bin/sh

export PATH=/bin:/sbin

echo "=========================================="
echo "KestrelFS 交互式测试环境"
echo "=========================================="

# 挂载基本文件系统
mount -t proc none /proc
mount -t sysfs none /sys
mount -t devtmpfs none /dev 2>/dev/null || mknod -m 666 /dev/null c 1 3

echo ""
echo "=========================================="
echo "环境准备完成！"
echo "=========================================="
echo ""
echo "📁 工作目录：/work"
echo "   - kestrelfs.ko"
echo "   - kestrelfs-daemon"
echo "   - vm-auto-test.sh"
echo ""
echo "快速测试命令："
echo "  cd /work"
echo "  insmod kestrelfs.ko"
echo "  ./kestrelfs-daemon &"
echo "  sleep 1"
echo "  mkdir /mnt/kestrelfs"
echo "  mount -t kestrelfs none /mnt/kestrelfs"
echo "  echo 'hello' > /mnt/kestrelfs/test.txt"
echo "  cat /mnt/kestrelfs/test.txt"
echo "  ls -la /mnt/kestrelfs/"
echo "  time umount /mnt/kestrelfs"
echo "  dmesg | tail -30"
echo "  rmmod kestrelfs"
echo "  echo b > /proc/sysrq-trigger  # 强制重启"
echo ""
echo "=========================================="
echo ""

# 启动交互式 shell
exec /bin/sh
INITEOF

chmod +x "$INITRAMFS_DIR/init"

# 打包 initramfs（准备输出路径）
INITRAMFS_FILE="/tmp/kestrelfs-initramfs-$$.cpio.gz"

# 创建设备节点并打包（使用 fakeroot 避免 sudo）
echo "📦 创建设备节点并打包 initramfs..."
cat > "$INITRAMFS_DIR/create_devs.sh" << 'DEVEOF'
#!/bin/sh
cd dev
mknod -m 666 console c 5 1
mknod -m 666 null c 1 3
mknod -m 666 zero c 1 5
mknod -m 666 tty c 5 0
DEVEOF
chmod +x "$INITRAMFS_DIR/create_devs.sh"
cd "$INITRAMFS_DIR"
fakeroot -- sh -c './create_devs.sh && find . -print0 | cpio --null -ov --format=newc 2>/dev/null' | gzip -9 > "$INITRAMFS_FILE"
cd "$SCRIPT_DIR"

# 清理临时脚本
rm -f "$INITRAMFS_DIR/create_devs.sh"

# 启动 QEMU
echo ""
echo "🚀 启动 QEMU 虚拟机（交互式模式）..."
echo ""

qemu-system-x86_64 \
    -kernel "$KERNEL" \
    -initrd "$INITRAMFS_FILE" \
    -m 2G \
    -smp 2 \
    -nographic \
    -no-reboot \
    -append "console=ttyS0 panic=1 sysrq_always_enabled=1" \
    || true

# 清理
echo ""
echo "清理临时文件..."
rm -rf "$INITRAMFS_DIR"
rm -f "$INITRAMFS_FILE"

# 重置终端（恢复正常显示）
reset

echo ""
echo "=========================================="
echo "虚拟机已退出，终端已重置"
echo "=========================================="
