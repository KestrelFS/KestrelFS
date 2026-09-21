#!/bin/bash
# shellcheck source=_repo_root.sh
# Keep cwd at repo root so daemon/ and kestrelfs/ relative paths stay stable.
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"
# 使用 virtme-ng 在虚拟机中测试 KestrelFS（交互式 shell）

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# 检查 vng
if ! command -v vng &>/dev/null; then
    echo "❌ vng (virtme-ng) 未安装"
    echo ""
    echo "请先安装 virtme-ng"
    echo ""
    exit 1
fi

# 编译
echo "🔨 编译模块和 daemon..."
cd "$SCRIPT_DIR/kestrelfs" && make
cd "$SCRIPT_DIR/daemon" && cargo build --release
cd "$SCRIPT_DIR"

echo ""
echo "🚀 启动 virtme-ng 虚拟机（交互式 shell，带网络）..."
echo ""
echo "快速测试命令（进入 VM 后执行）："
echo "  sudo insmod kestrelfs/kestrelfs.ko"
echo "  sudo ./daemon/target/release/kestrelfs-daemon --memory &"
echo "  sudo mkdir -p /mnt/kestrelfs"
echo "  sudo mount -t kestrelfs none /mnt/kestrelfs"
echo "  ls -la /mnt/kestrelfs/"
echo "  echo 'test' | sudo tee /mnt/kestrelfs/testfile"
echo "  sudo umount /mnt/kestrelfs"
echo ""

# 使用 vng 启动
vng --network user --run

echo ""
echo "=========================================="
echo "虚拟机已退出"
echo "=========================================="
