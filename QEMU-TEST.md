# KestrelFS QEMU 测试指南

## 快速开始（推荐方案）

### 方案 A：virtme-ng（最简单）⭐

```bash
# 1. 安装 virtme-ng（只需一次）
sudo apt install pipx
pipx install virtme-ng

# 2. 运行测试
./test-vm-virtme.sh
```

**优点**：
- 自动共享宿主机文件系统
- 无需手动构建 initramfs
- 测试失败不影响宿主机

---

### 方案 B：完全自包含（无需安装工具）

```bash
./test-vm-rootfs.sh
```

**优点**：
- 无需安装 virtme-ng
- 自动构建 initramfs
- 完全隔离的测试环境

---

## 测试内容

所有脚本都会自动测试：

1. ✅ **Bug A 修复验证**：umount 是否在 5 秒内完成（之前会 CPU 99% 死循环）
2. ✅ **Bug B 修复验证**：ls 是否显示完整文件名（之前截断）
3. ✅ 写入 20 个文件
4. ✅ 读取验证
5. ✅ 模块加载/卸载
6. ✅ 内核日志检查

---

## 测试输出示例

成功的输出：
```
✅ 加载模块...
✅ 挂载文件系统...
✅ 写入 20 个文件完成

测试 Bug A（umount 卡死问题）
✅✅✅ umount 成功！用时 0 秒
✅✅✅ Bug A 已修复！
```

失败的输出：
```
❌❌❌ umount 超时！Bug A 仍存在！
```

---

## 可用测试脚本

| 脚本 | 说明 | 依赖 |
|------|------|------|
| `test-vm-virtme.sh` | 使用 virtme-ng（推荐） | 需要安装 virtme-ng |
| `test-vm-rootfs.sh` | 完全自包含 | 仅需 qemu-system-x86_64 |
| `test-vm.sh` | 使用 9p 共享（可能失败） | 需要 qemu + 9p 支持 |

---

## 当前系统状态检查

```bash
# 检查是否有卡住的进程
ps aux | grep -E "(umount|kestrelfs)" | grep -v grep

# 检查内核模块
lsmod | grep kestrelfs

# 如果有卡住的进程，确认系统需要重启
cat /proc/<PID>/wchan  # 如果显示 0，说明在死循环
```

---

## 验证修复是否成功

测试通过的标准：
1. ✅ `umount` 在 **1 秒内完成**（之前会卡死）
2. ✅ `ls /mnt/kestrelfs/` 显示 **完整文件名**（之前截断）
3. ✅ 无内核 panic/oops/warning

---

## 故障排查

### QEMU 启动失败
```bash
# 检查内核文件
ls -lh /boot/vmlinuz-$(uname -r)

# 检查 QEMU 版本
qemu-system-x86_64 --version
```

### 9p 共享失败
```bash
# 在 QEMU 内手动挂载
mount -t 9p -o trans=virtio hostshare /mnt/host
```

### 模块加载失败
```bash
# 检查模块是否为当前内核编译
modinfo kestrelfs/kestrelfs.ko | grep vermagic
uname -r
```

---

## 清理

```bash
# 清理测试文件
rm -rf vm-test/
rm -f /tmp/kestrelfs-test-*

# 如果 QEMU 卡住
pkill -9 qemu-system
```
