#!/bin/bash
# 使用 virtme 在虚拟机中测试 KestrelFS（交互式 shell）

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# 检查 virtme
if ! python3 -c "import virtme" 2>/dev/null; then
    echo "❌ virtme 未安装"
    echo ""
    echo "安装方法："
    echo "  pip3 install --user virtme"
    echo ""
    exit 1
fi

# 编译
echo "🔨 编译模块和 daemon..."
cd "$SCRIPT_DIR/kestrelfs" && make
cd "$SCRIPT_DIR/daemon" && cargo build --release
cd "$SCRIPT_DIR"

# 创建初始化脚本（在 VM 启动时自动执行）
INIT_SCRIPT="$SCRIPT_DIR/.vm-init.sh"
cat > "$INIT_SCRIPT" << 'EOF'
#!/bin/bash
# VM 启动后自动执行的初始化脚本

cd /root || exit 1

echo ""
echo "=========================================="
echo "  KestrelFS 测试环境"
echo "=========================================="
echo ""
echo "模块位置: ./kestrelfs/kestrelfs.ko"
echo "Daemon:   ./daemon/target/release/kestrelfs-daemon"
echo ""
echo "快速测试命令："
echo "  sudo insmod kestrelfs/kestrelfs.ko"
echo "  sudo ./daemon/target/release/kestrelfs-daemon --memory &"
echo "  sudo mkdir -p /mnt/kestrelfs"
echo "  sudo mount -t kestrelfs none /mnt/kestrelfs"
echo "  ls -la /mnt/kestrelfs/"
echo "  echo 'test' | sudo tee /mnt/kestrelfs/testfile"
echo "  sudo umount /mnt/kestrelfs"
echo ""
echo "进入交互式 shell，手动测试..."
echo "=========================================="
echo ""

exec /bin/bash
EOF

chmod +x "$INIT_SCRIPT"

echo ""
echo "🚀 启动 virtme 虚拟机（交互式 shell）..."
echo ""

# 使用 virtme 启动，映射当前目录到 /root
python3 -m virtme.commands.run \
    --installed-kernel \
    --pwd \
    --rwdir "$SCRIPT_DIR/kestrelfs" \
    --rwdir "$SCRIPT_DIR/daemon" \
    --script-sh "cd '$SCRIPT_DIR' && exec bash .vm-init.sh"

rm -f "$INIT_SCRIPT"

echo ""
echo "=========================================="
echo "虚拟机已退出"
echo "=========================================="
