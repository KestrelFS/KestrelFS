# Phase 3 Step 8: 元数据持久化验证

## 自动化测试（已通过）

```bash
cd daemon
cargo test      # 85 个测试全部通过（包括 5 个新的持久化测试）
cargo clippy    # 无警告
```

## 手工验证步骤

### 方式 1：使用提供的测试脚本（推荐）

```bash
./test-persistence.sh
```

该脚本会：
1. 启动 daemon（持久化模式）并写入 3 个文件
2. 停止 daemon 并检查 `meta.json` 存在
3. 重启 daemon 并验证文件可读
4. 再创建一个文件，第三次重启验证

### 方式 2：手动测试

```bash
# 1. 编译
make -C kestrelfs
cargo build --release -p kestrelfs-daemon

# 2. 加载模块
sudo insmod kestrelfs/kestrelfs.ko

# 3. 启动 daemon（持久化模式，默认数据目录为 ./.kestrelfs-data）
./daemon/target/release/kestrelfs-daemon --data-dir /tmp/test_data &
DAEMON_PID=$!
sleep 2

# 4. 挂载
sudo mkdir -p /mnt/kestrelfs
sudo mount -t kestrelfs none /mnt/kestrelfs

# 5. 写入文件
echo "Hello persistence" | sudo tee /mnt/kestrelfs/test.txt
sudo cat /mnt/kestrelfs/test.txt

# 6. 检查持久化文件
ls -lh /tmp/test_data/meta.json
cat /tmp/test_data/meta.json | head -20

# 7. 重启 daemon
sudo umount /mnt/kestrelfs
kill $DAEMON_PID
sleep 1

./daemon/target/release/kestrelfs-daemon --data-dir /tmp/test_data &
DAEMON_PID=$!
sleep 2

# 8. 验证数据恢复
sudo mount -t kestrelfs none /mnt/kestrelfs
ls -la /mnt/kestrelfs/
sudo cat /mnt/kestrelfs/test.txt  # 应输出 "Hello persistence"

# 9. 清理
sudo umount /mnt/kestrelfs
kill $DAEMON_PID
sudo rmmod kestrelfs
```

## 预期结果

✅ 重启 daemon 后，之前写入的文件仍存在且内容正确  
✅ `meta.json` 包含 inodes、dir_entries、slices、next_inode_id  
✅ 块数据存储在 `$data_dir/<slice_uuid>/` 目录下  
✅ `--memory` 模式下，重启后数据丢失（符合预期）

## 实现细节

- **持久化格式**：JSON（`meta.json`）
- **原子写入**：tmp + fsync + rename
- **启动行为**：
  - 文件存在 → 加载
  - 文件缺失/损坏 → 使用默认 MemStore（root + remote.txt + writable.dat）
- **同步时机**：create/append_slice/truncate 后立即写入
- **CLI 选项**：
  - `--data-dir <path>`：持久化目录（默认 `./.kestrelfs-data`）
  - `--memory`：纯内存模式（元数据 + 块数据都不持久化）

## 新增文件

- `daemon/src/meta_persist.rs`：FileMetaStore 实现 + 单元测试
- `test-persistence.sh`：集成测试脚本
