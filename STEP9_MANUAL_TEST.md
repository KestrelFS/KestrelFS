# Phase 3 Step 9: mkdir + unlink 手工验证

## 自动化测试（已通过）

```bash
cd daemon
cargo test      # 99/99 通过 (新增 14 个测试)
cargo clippy    # 无警告

cd ../kestrelfs
make            # 内核模块编译通过
```

## 手工验证步骤

### 方式 1：宿主机测试（需 sudo）

```bash
# 1. 编译
make -C kestrelfs
cargo build --release -p kestrelfs-daemon

# 2. 加载模块
sudo insmod kestrelfs/kestrelfs.ko

# 3. 启动 daemon（持久化模式）
./daemon/target/release/kestrelfs-daemon --data-dir /tmp/kestrel_step9 &
DAEMON_PID=$!
sleep 2

# 4. 挂载
sudo mkdir -p /mnt/kestrelfs
sudo mount -t kestrelfs none /mnt/kestrelfs

# 5. 测试 mkdir
sudo mkdir /mnt/kestrelfs/testdir
ls -la /mnt/kestrelfs/
# 应显示 testdir/

# 6. 在目录内创建文件
echo "Hello from testdir" | sudo tee /mnt/kestrelfs/testdir/file.txt
sudo cat /mnt/kestrelfs/testdir/file.txt
# 应输出 "Hello from testdir"

# 7. 测试嵌套目录
sudo mkdir /mnt/kestrelfs/testdir/subdir
sudo touch /mnt/kestrelfs/testdir/subdir/nested.txt
ls -R /mnt/kestrelfs/testdir/

# 8. 测试 unlink（删除文件）
sudo rm /mnt/kestrelfs/testdir/file.txt
ls /mnt/kestrelfs/testdir/
# file.txt 应消失

# 9. 测试 rmdir（非空目录应失败）
sudo rmdir /mnt/kestrelfs/testdir
# 应报错: rmdir: failed to remove '/mnt/kestrelfs/testdir': Directory not empty

# 10. 删除嵌套文件，清空目录
sudo rm /mnt/kestrelfs/testdir/subdir/nested.txt
sudo rmdir /mnt/kestrelfs/testdir/subdir
sudo rmdir /mnt/kestrelfs/testdir
ls /mnt/kestrelfs/
# testdir 应消失

# 11. 检查持久化
cat /tmp/kestrel_step9/meta.json | jq '.dir_entries["1"]'
# 应显示当前根目录下的文件列表（不包含已删除的 testdir）

# 12. 重启 daemon 验证持久化
sudo umount /mnt/kestrelfs
kill $DAEMON_PID
sleep 1

./daemon/target/release/kestrelfs-daemon --data-dir /tmp/kestrel_step9 &
DAEMON_PID=$!
sleep 2

sudo mount -t kestrelfs none /mnt/kestrelfs
ls -la /mnt/kestrelfs/
# 之前创建和删除的操作应在重启后保持一致

# 13. 清理
sudo umount /mnt/kestrelfs
kill $DAEMON_PID
sudo rmmod kestrelfs
rm -rf /tmp/kestrel_step9
```

### 方式 2：virtme-ng 测试（无需 sudo）

```bash
# 注意：需要先构建内核或使用系统内核
# 参考 test-virtme.sh 脚本
./test-virtme.sh
```

## 预期结果

### 成功场景

✅ `mkdir` 创建目录成功  
✅ 在新目录内创建文件  
✅ 嵌套目录创建和访问  
✅ `rm` 删除文件成功  
✅ `rmdir` 删除空目录成功  
✅ 重启 daemon 后目录树一致  
✅ `meta.json` 正确记录目录结构

### 失败场景（符合预期）

❌ `mkdir` 重复名称 → "File exists"  
❌ `rmdir` 非空目录 → "Directory not empty"  
❌ `rm` 不存在的文件 → "No such file or directory"

## 实现细节

### 元数据持久化

```json
// meta.json 示例（mkdir + create 后）
{
  "inodes": {
    "1": {"ino": 1, "mode": 16877, "size": 0, ...},        // root (S_IFDIR)
    "5": {"ino": 5, "mode": 16877, "size": 0, ...},        // testdir (S_IFDIR)
    "6": {"ino": 6, "mode": 33188, "size": 18, ...}        // file.txt (S_IFREG)
  },
  "dir_entries": {
    "1": {"testdir": 5},                                   // root 包含 testdir
    "5": {"file.txt": 6}                                   // testdir 包含 file.txt
  },
  "slices": {
    "6": {"0": [{"uuid": "...", "offset": 0, "len": 18}]} // file.txt 的数据
  }
}
```

### unlink 行为

- **文件**: 移除 `dir_entries[parent][name]`，删除 `inodes[ino]` 和 `slices[ino]`
- **空目录**: 检查 `dir_entries[ino]` 为空，然后删除
- **非空目录**: 返回 `-ENOTEMPTY` (errno 39)

### 内核 VFS 集成

- `kestrelfs_inode_mkdir`: 发送 `OP_MKDIR` → 创建 VFS inode (S_IFDIR)
- `kestrelfs_inode_unlink`: 发送 `OP_UNLINK` → 调用 `d_delete(dentry)`
- `kestrelfs_inode_rmdir`: 复用 `unlink`（daemon 区分文件/目录）

## 已知限制

- ❌ 不支持 `rename()` (mv 命令会失败)
- ❌ 不支持硬链接 (`ln file link`)
- ❌ 不支持符号链接 (`ln -s`)
- ❌ `stat` 的 `nlink` 固定为 2（目录）/1（文件），不递归计算子目录
- ❌ 写入仍限制为 12 字节/次（payload 大小限制）

## 下一步 (Step 10)

可选方向：
1. **扩大 WRITE payload**: 多轮 IPC 或共享页 → 支持大文件快速写入
2. **实现 rename**: `OP_RENAME` + VFS `.rename` → 支持 `mv` 命令
3. **Redis/S3 元数据**: 替换 JSON 文件 → 支持分布式部署
4. **NVMe 直通**: Phase 4 计划，本地块设备优化
