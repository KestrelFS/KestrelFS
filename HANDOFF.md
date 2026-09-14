# KestrelFS 研发交接文档（HANDOFF）

> **最后更新**：Phase 3 Step 15 ObjectStore 孤儿块 GC 已由 Cursor 验收并纳入本提交（ABI 仍为 v11）。
> **核对应法**：以 `git log --oneline -5` 与本文件进度表为准；若与代码冲突，以代码为准并更新本文档。
> **核对应法**：以 `git log --oneline -5` 与本文件进度表为准；若与代码冲突，以代码为准并更新本文档。

---

## 1. 项目身份

| 项 | 值 |
|---|---|
| 产品名 | **KestrelFS** |
| 本地仓库目录名 | 可能叫 **FerroFS**（历史目录名），产品名和 GitHub 仓库名均为 KestrelFS |
| 一句话定位 | 高性能云原生分布式文件系统；C 内核模块 + Rust daemon 混合架构；对标/超越 JuiceFS（缓存命中路径零上下文切换） |
| License | Apache-2.0 |
| 上游 | `https://github.com/KestrelFS/KestrelFS`（以 README 为准） |
| 当前阶段 | Phase 3（控制面真实化）进行中；Phase 4（内核 NVMe 缓存）未开始 |

---

## 2. 协作角色（固定）

| 角色 | 职责 |
|---|---|
| **Cursor** | 本交接的验收方：定路线、写提示词、验收、打回。Codex 只按 Cursor 粘贴的提示词改代码。 |
| **Codex** | 实现 agent：接收 Cursor 的提示词，实现代码变更，汇报结果。不自行决定路线。 |
| **人类** | 搬运提示词（Cursor → Codex）、跑需 sudo 的真机/虚拟机测试。 |
| **OpenCode** | 历史实现至 Phase 3 Step 10 与 `HANDOFF.md`；现迁移到 Codex。 |

---

## 3. 架构摘要

```
                    ┌─────────────────────────────────┐
                    │        Userspace (Rust)          │
                    │   kestrelfs-daemon (Tokio)       │
                    │                                  │
                    │  MetaStore (元数据)               │
                    │    ├─ MemStore (内存)             │
                    │    └─ FileMetaStore (meta.json)   │
                    │  ObjectStore (块数据)             │
                    │    ├─ MemObjectStore (内存)       │
                    │    └─ LocalFsObjectStore (磁盘)   │
                    └───────────▲──────────────────────┘
                                │ mmap() 共享内存双环 + ioctl/poll
                    ┌───────────▼──────────────────────┐
                    │        Kernel space (C)           │
                    │   kestrelfs.ko (out-of-tree)      │
                    │                                  │
                    │  VFS (super/inode/dir/file ops)  │
                    │  /dev/kestrel_ctl char device     │
                    │  本地 NVMe 缓存 (Phase 4, 未开始)  │
                    └──────────────────────────────────┘
```

- **内核模块** `kestrelfs.ko`：out-of-tree，注册 VFS 文件系统类型，实现 super/inode/dir/file operations。通过 `/dev/kestrel_ctl` 字符设备与 daemon 通信。
- **字符设备** `/dev/kestrel_ctl`：单个 `mmap()` 共享内存区域（144.2 KiB），内含两条独立无锁 SPSC 环形缓冲区（REQ 环 + RESP 环，各 1024 slot × 64 字节），以及 ring 后方一块 16 KiB data/name bounce buffer。唤醒模型：内核→Rust 用 `wake_up_interruptible()` + `poll()`；Rust→内核用 `KESTRELFS_IOC_NOTIFY_RESP` ioctl。
- **用户态 daemon** `kestrelfs-daemon`：Tokio 异步运行时。`poll()` 驱动事件循环，逐条处理 REQ 事件，批量推回 RESP。MetaStore 管理元数据（inode/dirent/slice），ObjectStore 管理块数据。
- **数据模型**（JuiceFS-like 分层）：File → Chunk（64 MiB 固定窗口）→ Slice（变长写记录，COW 语义）→ Block（4 MiB 物理对象，存于 ObjectStore）。
- **明确**：NVMe 缓存是 Phase 4、由内核拥有；当前阶段不存在任何 NVMe cache 代码。不要把宿主机文件路径（如 ZFS pool）当 cache 设备。

---

## 4. 仓库布局

```
FerroFS/                         # 仓库根目录（产品名 KestrelFS）
├── kestrelfs/                   # 内核模块 (C)
│   ├── Makefile                 # kbuild wrapper: make -C $(KDIR) M=$(PWD) modules
│   ├── super.c                  # module_init/exit, register_filesystem
│   ├── inode.c                  # super_operations, kestrelfs_get_inode, fill_super, kill_sb
│   ├── dir.c                    # inode_operations（含 symlink/get_link）+ readdir
│   ├── file.c                   # file_operations (read/write/setattr) + IPC sync call helper
│   ├── chardev.c                # /dev/kestrel_ctl: mmap/poll/ioctl
│   ├── ipc_ring.c               # ring buffer push/pop primitives
│   ├── kestrelfs.h              # 内部跨文件声明
│   └── kestrelfs_ipc.h          # ★ ABI 合约（C/Rust 共享，opcode/payload/struct 定义）
│
├── daemon/                      # Rust daemon
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs              # CLI 参数、事件循环、handle_* 函数、opcode 分发
│       ├── abi.rs               # ★ ABI mirror of kestrelfs_ipc.h（opcode 常量、编解码、编译时断言）
│       ├── meta.rs              # MetaStore trait + MemStore 实现
│       ├── meta_persist.rs      # FileMetaStore（JSON 持久化）
│       ├── object_store.rs      # ObjectStore trait + MemObjectStore + LocalFsObjectStore
│       ├── fs_model.rs          # Inode / Slice / Block 数据模型
│       ├── device.rs            # /dev/kestrel_ctl 打开/mmap/ABI校验
│       ├── ring.rs              # Rust 侧 ring buffer 读写
│       └── ioctl.rs             # ioctl 号常量
│
├── HANDOFF.md                   # ★ 本文件
├── STEP8_VERIFICATION.md        # Step 8 持久化验证指南
├── STEP9_MANUAL_TEST.md         # Step 9 mkdir/unlink 手工测试指南
├── test-persistence.sh          # 持久化集成测试脚本（需 sudo）
├── test-vm-virtme.sh            # virtme-ng 虚拟机测试脚本
├── test-vm-interactive.sh       # QEMU 交互式测试脚本（busybox initramfs）
├── QEMU-TEST.md                 # QEMU 测试说明
└── README.md                    # 项目 README（已滞后，见第 11 节文档债务）
```

---

## 5. 进度

### 5.1 按 Phase/Step

| Phase / Step | 内容 | ABI 版本 | 状态 |
|---|---|---|---|
| Phase 1 | VFS 骨架（super.c/inode.c/file.c, register_filesystem, hello.txt） | — | ✅ 已验收 |
| Phase 2 | IPC 桥（chardev mmap/poll/ioctl, 双环, Rust consumer bootstrap） | 1→2 | ✅ 已验收 |
| Phase 3 Step 1–3 | MetaStore trait + MemStore + ObjectStore + Inode/Slice/Block 模型 | 2 | ✅ 已验收 |
| Phase 3 Step 4 | WRITE_CHUNK（每次 ≤12 字节） | 3 | ✅ 已验收 |
| Phase 3 Step 5 | TRUNCATE / setattr（O_TRUNC, ftruncate） | 4 | ✅ 已验收 |
| Phase 3 Step 6 | LocalFsObjectStore（块数据持久化到本地磁盘） | 4 | ✅ 已验收 |
| Phase 3 Step 7 | ABI v5：CREATE + READDIR opcode | 5 | ✅ 已验收 |
| Phase 3 Step 7b | 内核动态 LOOKUP/CREATE/READDIR，移除 simple_fill_super | 5 | ✅ 已验收 |
| **Step 8** | **FileMetaStore（meta.json 持久化）+ --data-dir CLI** | 5 | **✅ 已验收** |
| **Step 9** | **mkdir / unlink / rmdir** | **6** | **✅ 已验收** |
| **Step 10** | **rename（同目录改名、跨目录移动、原子覆盖）** | **7** | **✅ 已验收** |
| **Step 11** | **16 KiB bounce buffer 扩大 READ/WRITE 数据面** | **8** | **✅ 已验收** |
| **Step 12** | **借 bounce buffer 放大 rename 名字** | **9** | **✅ 已验收** |
| **Step 13** | **统一长名字数据面 + 批量 READDIR** | **10** | **✅ 已验收** |
| **Step 14** | **符号链接（MetaStore 持有 target，VFS symlink/get_link）** | **11** | **✅ 已验收** |
| **Step 15** | **unlink / rename 覆盖 / truncate 的无引用 ObjectStore block GC** | **11（未变）** | **✅ 已验收** |

Cursor 对照代码、130 tests 与 vng（`VNG_GC_PASS`，umount≈92ms）确认 Step 15 已验收。

> **当前 ABI**：`KESTRELFS_ABI_VERSION = 11`（活跃名字/数据/symlink 路径使用 bounce buffer）

### 5.2 关键 Bug 修复（按时间倒序）

| Commit | 问题 | 修复 |
|---|---|---|
| `e3610d2` | dir.c 格式字符串警告（`%llu` vs `unsigned long`） | 改用 `%lu` + 直接引用 `dir->i_ino` |
| `fec790e` | unlink 后 `d_delete()` 导致 NULL deref panic（`ihold()` 崩溃） | 移除 `d_delete()`，改用 `drop_nlink()` + 时间戳更新 |
| `2d9cae8` | mkdir 时 `memcpy(&mode, ..., sizeof(u32))` 越界（`umode_t` 是 u16） | 先 `u32 mode32 = (u32)mode` 再 memcpy |
| `023ba2d` | readdir 重复条目 + umount 卡死 | `dir_emit_dots()` 替换手写 dot/dotdot；添加 `write_inode`/`sync_fs`/`kill_anon_super` |
| `7787a6a` | `iget5_locked()` 导致内核死循环 | 回退到 `new_inode()` + `insert_inode_hash()` |
| `1db4c07` | umount 卡住 + READDIR 截断/重复 | daemon 断开时快速失败；READDIR 改为每响应 1 条目 |
| `b7a61ee` | umount 卡住（daemon 断开时无唤醒） | 快速失败 + 唤醒等待者 |

### 5.3 当前 ABI 版本

**`KESTRELFS_ABI_VERSION = 11`**（内核 `kestrelfs_ipc.h` 与 Rust `abi.rs` 一致）

版本演进：
1. 初始 Phase 2 桥接
2. READ_CHUNK 增加 inode_id
3. Phase 3 Step 4：WRITE_CHUNK
4. Phase 3 Step 5：TRUNCATE
5. Phase 3 Step 7：CREATE + READDIR
6. Phase 3 Step 9：MKDIR + UNLINK
7. Phase 3 Step 10：RENAME
8. Phase 3 Step 11：ring 后新增 16 KiB data bounce buffer；新增 WRITE_DATA / READ_DATA
9. Phase 3 Step 12：新增 RENAME_DATA；两个名字依次放入 bounce buffer，单名上限 255 字节
10. Phase 3 Step 13：新增 LOOKUP/CREATE/MKDIR/UNLINK/READDIR_DATA；单名统一为 255 字节，READDIR 批量打包变长条目
11. Phase 3 Step 14：新增 SYMLINK_DATA / READLINK_DATA；link name 与 target 通过同一 16 KiB bounce 传输，target 仅存 MetaStore

### 5.4 已实现 Opcode 列表

| 编号 | 常量 | 用途 | 引入版本 |
|---|---|---|---|
| 0 | `OP_NOP` | 无操作/填充 | 1 |
| 1 | `OP_LOOKUP` | 解析 (parent, name) → child inode | 1 |
| 2 | `OP_READ_CHUNK` | 获取块数据（缓存未命中） | 1（v2 加 inode_id） |
| 3 | `OP_GETATTR` | 获取 inode 属性 | 1 |
| 4 | `OP_WRITE_CHUNK` | 写数据到文件（≤12 字节/次） | 3 |
| 5 | `OP_TRUNCATE` | 设置文件大小 | 4 |
| 6 | `OP_CREATE` | 创建新文件 | 5 |
| 7 | `OP_READDIR` | 列出目录条目 | 5 |
| 8 | `OP_MKDIR` | 创建新目录 | 6 |
| 9 | `OP_UNLINK` | 删除文件或空目录 | 6 |
| 10 | `OP_RENAME` | 重命名/移动文件或目录 | 7 |
| 11 | `OP_WRITE_DATA` | 从 16 KiB bounce buffer 写入文件 | 8 |
| 12 | `OP_READ_DATA` | 将文件数据读入 16 KiB bounce buffer | 8 |
| 13 | `OP_RENAME_DATA` | 从 16 KiB bounce buffer 读取 old/new name 并重命名 | 9 |
| 14 | `OP_LOOKUP_DATA` | 从 bounce buffer 读取名字并查找 | 10 |
| 15 | `OP_CREATE_DATA` | 从 bounce buffer 读取名字并创建文件 | 10 |
| 16 | `OP_MKDIR_DATA` | 从 bounce buffer 读取名字并创建目录 | 10 |
| 17 | `OP_UNLINK_DATA` | 从 bounce buffer 读取名字并删除文件或空目录 | 10 |
| 18 | `OP_READDIR_DATA` | 通过 bounce buffer 批量返回变长目录条目 | 10 |
| 19 | `OP_SYMLINK_DATA` | bounce 中依次传输 link name 与 target，创建符号链接 | 11 |
| 20 | `OP_READLINK_DATA` | daemon 将符号链接 target 返回到 bounce buffer | 11 |
| 64 | `OP_RESULT_OK` | 响应：成功 | 1 |
| 65 | `OP_RESULT_ERROR` | 响应：失败（error_code 携带负 errno） | 1 |

### 5.5 存储现状

| 层 | 内存模式 (`--memory`) | 持久化模式 (`--data-dir <path>`) |
|---|---|---|
| 元数据 (MetaStore) | `MemStore`（纯 HashMap，重启丢失） | `FileMetaStore`（包装 MemStore，每次写操作后全量序列化到 `{data_dir}/meta.json`，原子 tmp+fsync+rename） |
| 块数据 (ObjectStore) | `MemObjectStore`（纯 HashMap，支持幂等 delete） | `LocalFsObjectStore`（每个 block 存为 `{data_dir}/{slice_uuid}/{block_idx}` 文件，原子 tmp+rename；GC 真删文件及空 UUID 目录） |

**CLI 参数**（`daemon/src/main.rs`）：
- `--data-dir <PATH>`：持久化目录（默认 `./.kestrelfs-data`），meta.json 和块数据均存于此
- `--memory`：纯内存模式（元数据 + 块数据均不持久化）

---

## 6. 已知限制与坑

| # | 限制/坑 | 说明 | 代码位置 |
|---|---|---|---|
| 1 | **旧 CHUNK opcode 仍受小 payload 限制** | `WRITE_CHUNK` 仍最多 12 字节、`READ_CHUNK` 仍最多 32 字节，仅为兼容既有测试保留；普通文件内核路径已切换到 16 KiB bounce buffer 的 `WRITE_DATA` / `READ_DATA`，单次 read/write 会在内核内循环完成。 | `kestrelfs_ipc.h`、`kestrelfs/file.c` |
| 2 | **旧 RENAME 仍为每名 ≤7 字节** | opcode 10 仅为兼容既有测试保留；普通 VFS rename 已切到 opcode 13 `RENAME_DATA`，单名上限 255 字节，两个名字依次位于 bounce buffer。 | `kestrelfs_ipc.h` RENAME / RENAME_DATA 布局 |
| 3 | **旧名字 opcode 仍有短 payload 上限** | opcode 1/6/7/8/9 仅为兼容既有测试保留；普通 VFS 的 lookup/create/readdir/mkdir/unlink 已切换到 ABI v10 DATA opcode，统一支持 255 字节名字。 | `kestrelfs_ipc.h` 各 legacy / DATA opcode 布局 |
| 4 | **GC 为安全的提交后 best-effort** | unlink、rename 覆盖和 truncate 先提交/持久化元数据，再删除经全局 slice 引用扫描确认无引用的 block。delete 失败只记录日志并保留泄漏，不把已经生效的命名空间操作伪装成失败；当前没有持久化重试队列。 | `daemon/src/meta.rs`、`daemon/src/main.rs` `delete_garbage_objects()` |
| 5 | **new_inode() 而非 iget5_locked()** | 曾尝试 `iget5_locked()` 做严格 inode 缓存，导致 umount 时内核死循环（commit `7787a6a`）。已回退为 `new_inode()` + `insert_inode_hash()`。**不要轻易重试 iget5_locked 方案**，除非彻底解决 I_FREEING 竞态。 | `kestrelfs/inode.c` `kestrelfs_get_inode()` |
| 6 | **同一 ino 可能有多实例 inode** | 使用 `new_inode()` 意味着每次 lookup 都创建新 inode 对象（而非复用哈希表中已有实例）。dentry cache 保证路径唯一性，但同一文件通过不同路径访问时内核中可能有多个 inode 对象。 | 同上 |
| 7 | **evict_inode 禁止发 IPC** | `kestrelfs_evict_inode()` 只做 `truncate_inode_pages_final` + `clear_inode`，绝不发 IPC（daemon 可能已关闭，会死锁）。 | `kestrelfs/inode.c` |
| 8 | **JSON 全量落盘** | `FileMetaStore` 每次写操作后将整个元数据状态序列化为 JSON 写盘。简单但低效；inode 数量大时性能差。 | `daemon/src/meta_persist.rs` `sync_to_disk()` |
| 9 | **meta.json 损坏 → 数据丢失** | 若 `meta.json` 反序列化失败（JSON 损坏），daemon 回退到全新 `MemStore::new()`（仅含 root + remote.txt + writable.dat），之前用户创建的文件元数据全部丢失。块数据仍在磁盘但无法访问。 | `daemon/src/meta_persist.rs` `FileMetaStore::new()` |
| 10 | **rename 不支持 flags** | `kestrelfs_inode_rename()` 对 `flags != 0` 直接返回 `-EINVAL`。不支持 `RENAME_NOREPLACE` / `RENAME_EXCHANGE` / `RENAME_WHITEOUT`。 | `kestrelfs/dir.c` |
| 11 | **nlink 不递归计算** | 目录 nlink 固定为 2（`.` + 父目录回链），文件 nlink 固定为 1。不随子目录增减而更新。 | `kestrelfs/inode.c` `kestrelfs_get_inode()` |
| 12 | **READDIR_DATA 每批受 16 KiB 限制** | daemon 按 inode 排序并在 bounce 中打包尽可能多的完整变长条目；大目录仍需分页 IPC，但不再固定每次只返回 1 条。 | `daemon/src/main.rs` `handle_readdir_data()` |
| 13 | **O_APPEND 手动处理** | 内核用 `f_op->write` 而非 `write_iter`，VFS 不会自动 seek 到 EOF。代码中手动检查 `O_APPEND` 并更新 `*ppos`。 | `kestrelfs/file.c` `kestrelfs_writable_write()` |
| 14 | **create() 忽略 kernel 传入的 mode** | `MemStore::create()` 内部用 `Inode::new_file()` 的默认 mode（`S_IFREG | 0o644`），忽略 kernel 传入的 mode 参数。 | `daemon/src/meta.rs` `create()` |
| 15 | **symlink target 当前要求 UTF-8 且 ≤4095 字节** | Linux 原生 symlink target 可为任意非 NUL 字节；当前 MetaStore 使用 `String`，ABI 解码拒绝非 UTF-8，target 上限为 4095 字节。悬空链接与相对链接均支持。 | `daemon/src/meta.rs`、`daemon/src/abi.rs` |
| 16 | **GC 引用确认是 O(全量 slice)** | 每次产生删除候选时扫描所有剩余 slice 构建 block key 引用集合，正确处理共享 key，但 inode/slice 很多时成本较高；未来 Redis/S3 后端需要引用计数或持久化 GC 队列。 | `daemon/src/meta.rs` `confirmed_garbage_keys()` |
| 17 | **未实现 open-unlink 延迟回收** | 当前没有 open handle/refcount ABI；unlink 会立即移除 inode/slice 并回收块，已打开 fd 在 unlink 后继续读写的完整 POSIX 语义尚未建模。 | `daemon/src/meta.rs` `unlink()` |

> ABI v8 起共享内存区域为 **147648 字节**（ring 后含 16 KiB data bounce buffer）；README 中旧的 **131264 字节**描述已过时。

> rename 成功后内核不手动调用 `d_move()`；dentry 更新交给 VFS rename 流程完成。

---

## 7. 构建与验收命令

### 7.1 编译

```bash
# 内核模块（需要当前内核 headers: /lib/modules/$(uname -r)/build）
make -C kestrelfs

# Rust daemon
cd daemon && cargo build --release
```

### 7.2 自动化测试

```bash
cd daemon
cargo test                    # 单元测试 + 集成测试（当前 130 个）
cargo clippy --all-targets -- -D warnings   # 零警告
```

### 7.3 手工验证（需 sudo / 真机或 virtme-ng）

详细步骤见 `STEP8_VERIFICATION.md`（持久化）和 `STEP9_MANUAL_TEST.md`（mkdir/unlink），核心流程：

> **固定测试启动约定**：以后提供的手工测试命令必须包含
> `sudo insmod kestrelfs/kestrelfs.ko`；daemon 固定使用
> `--data-dir /tmp/kestrelfs-debug >/dev/null 2>&1 &` 启动。接入 S3、Redis
> 后再调整此约定。

```bash
# 1. 加载模块 + 启动 daemon
sudo insmod kestrelfs/kestrelfs.ko
./daemon/target/release/kestrelfs-daemon --data-dir /tmp/kestrelfs-debug >/dev/null 2>&1 &
daemon_pid=$!
sleep 2

# 2. 挂载 + 测试
sudo mkdir -p /mnt/kestrelfs
sudo mount -t kestrelfs none /mnt/kestrelfs
# ... mkdir / touch / echo / cat / mv / rm / rmdir ...

# 3. 验证 umount 不卡死
time sudo umount /mnt/kestrelfs   # 应 <1s 完成

# 4. 验证持久化（杀 daemon → 同 data-dir 重启 → 文件仍在）
kill "$daemon_pid"; wait "$daemon_pid" || true
./daemon/target/release/kestrelfs-daemon --data-dir /tmp/kestrelfs-debug >/dev/null 2>&1 &
daemon_pid=$!
sleep 2
sudo mount -t kestrelfs none /mnt/kestrelfs
ls -la /mnt/kestrelfs/   # 应与重启前一致

# 5. 清理
sudo umount /mnt/kestrelfs
kill "$daemon_pid"
wait "$daemon_pid" || true
sudo rmmod kestrelfs
```

### 7.4 virtme-ng 测试（无需 sudo）

```bash
# 需要预装 virtme-ng 和可用内核
vng --network user -r --pwd
# 进入 VM 后直接 insmod / mount / 测试
echo b > /proc/sysrq-trigger  # 退出 VM
```

当前 vng 1.41 在此环境需显式传 `--run`；guest 根目录是只读 9p，因此专用脚本使用 guest `/tmp` 作为挂载点，并用 BusyBox mount。

### 7.5 Step 11 大数据手工验证（需 sudo）

模块、daemon 与挂载点准备好后执行：

```bash
payload=$(printf '0123456789abcdef%.0s' {1..8})
printf '%s' "$payload" | sudo tee /mnt/kestrelfs/bulk.dat >/dev/null
test "$(sudo cat /mnt/kestrelfs/bulk.dat)" = "$payload"
test "$(sudo wc -c < /mnt/kestrelfs/bulk.dat)" -eq 128
time sudo umount /mnt/kestrelfs   # 应 <1s
```

### 7.6 Step 12 长名 rename 与持久化手工验证（需 sudo）

沿用同一个 `--data-dir` 启动 daemon。由于 CREATE 仍限 19 字节，先创建短名，rename 成长名后再对长名执行 `touch`：

```bash
mnt=/mnt/kestrelfs
data_dir=/tmp/kestrelfs-step12
long_src=long-source-name-20xx
long_dst=long-target-name-20xx

sudo touch "$mnt/seed"
sudo mv "$mnt/seed" "$mnt/$long_src"
sudo touch "$mnt/$long_src"
printf 'step12 persistent data\n' | sudo tee "$mnt/$long_src" >/dev/null
sudo mkdir "$mnt/d1" "$mnt/d2"
sudo mv "$mnt/$long_src" "$mnt/d1/$long_dst"
sudo mv "$mnt/d1/$long_dst" "$mnt/d2/$long_src"
test "$(sudo cat "$mnt/d2/$long_src")" = "step12 persistent data"
time sudo umount "$mnt"                     # 应 <1s

kill "$daemon_pid"
wait "$daemon_pid" || true
./daemon/target/debug/kestrelfs-daemon --data-dir "$data_dir" &
daemon_pid=$!
sudo mount -t kestrelfs none "$mnt"
sudo ls "$mnt/d2"
test "$(sudo cat "$mnt/d2/$long_src")" = "step12 persistent data"
```

### 7.7 Step 13 端到端长名字与重启验证（需 sudo）

模块和 ABI v10 daemon 准备好后，使用同一个 `--data-dir`：

```bash
mnt=/mnt/kestrelfs
long_dir=directory-name-that-is-over-twenty-three-bytes
long_file=file-name-that-is-definitely-over-twenty-three-bytes.txt
long_new=renamed-file-that-is-still-over-twenty-three-bytes.txt

sudo insmod kestrelfs/kestrelfs.ko
./daemon/target/release/kestrelfs-daemon --data-dir /tmp/kestrelfs-debug >/dev/null 2>&1 &
daemon_pid=$!
sudo mkdir -p "$mnt"
sudo mount -t kestrelfs none "$mnt"

sudo mkdir "$mnt/$long_dir"
sudo touch "$mnt/$long_dir/$long_file"
printf 'step13 persistent data\n' | sudo tee "$mnt/$long_dir/$long_file" >/dev/null
sudo ls "$mnt/$long_dir" | grep -Fx "$long_file"
sudo mv "$mnt/$long_dir/$long_file" "$mnt/$long_dir/$long_new"
test "$(sudo cat "$mnt/$long_dir/$long_new")" = "step13 persistent data"
time sudo umount "$mnt"                     # 应 <1s

kill "$daemon_pid"
wait "$daemon_pid" || true
./daemon/target/release/kestrelfs-daemon --data-dir /tmp/kestrelfs-debug >/dev/null 2>&1 &
daemon_pid=$!
sudo mount -t kestrelfs none "$mnt"
sudo ls "$mnt/$long_dir" | grep -Fx "$long_new"
test "$(sudo cat "$mnt/$long_dir/$long_new")" = "step13 persistent data"
sudo rm "$mnt/$long_dir/$long_new"
sudo rmdir "$mnt/$long_dir"
time sudo umount "$mnt"
kill "$daemon_pid"
wait "$daemon_pid" || true
```

### 7.8 Step 14 symlink 与持久化重启验证（需 sudo）

以下命令包含固定 data-dir、模块加载、目标跟随、`readlink`、`ls -l` 与 daemon 重启恢复：

```bash
make -C kestrelfs
(cd daemon && cargo build --release)
sudo insmod kestrelfs/kestrelfs.ko
./daemon/target/release/kestrelfs-daemon --data-dir /tmp/kestrelfs-debug >/dev/null 2>&1 &
daemon_pid=$!
sleep 2

mnt=/mnt/kestrelfs
target=step14-target.txt
link=step14-symbolic-link-with-a-long-name
renamed=step14-renamed-symbolic-link-with-a-long-name
sudo mkdir -p "$mnt"
sudo mount -t kestrelfs none "$mnt"
sudo rm -f "$mnt/$link" "$mnt/$renamed" "$mnt/$target"
printf 'step14 persistent target\n' | sudo tee "$mnt/$target" >/dev/null
sudo ln -s "$target" "$mnt/$link"
test "$(sudo readlink "$mnt/$link")" = "$target"
sudo ls -l "$mnt/$link"
test "$(sudo cat "$mnt/$link")" = "step14 persistent target"
sudo mv "$mnt/$link" "$mnt/$renamed"
time sudo umount "$mnt"

kill "$daemon_pid"
wait "$daemon_pid" || true
./daemon/target/release/kestrelfs-daemon --data-dir /tmp/kestrelfs-debug >/dev/null 2>&1 &
daemon_pid=$!
sleep 2
sudo mount -t kestrelfs none "$mnt"
test "$(sudo readlink "$mnt/$renamed")" = "$target"
sudo ls -l "$mnt/$renamed"
test "$(sudo cat "$mnt/$renamed")" = "step14 persistent target"
sudo rm "$mnt/$renamed" "$mnt/$target"
time sudo umount "$mnt"
kill "$daemon_pid"
wait "$daemon_pid" || true
sudo rmmod kestrelfs
```

### 7.9 Step 15 ObjectStore GC vng 自动验证

脚本覆盖写入后 unlink、rename 覆盖目标、COW 两 slice truncate、零填充扩展及 `<1s` umount：

```bash
make -C kestrelfs
cargo build --release --manifest-path daemon/Cargo.toml
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step15-gc-vng.sh"
```

成功标记为 `VNG_GC_PASS`；guest 脚本任一断言失败均以非零状态退出。

---

## 8. 路线图（未做）

按 Cursor 既定策略的推荐优先级：

| 优先级 | 内容 | 说明 |
|---|---|---|
| 1 | **等待 Cursor 的 Step 16 提示词** | 候选：RedisMetaStore 原型；S3 ObjectStore 可其后 |
| 2 | Redis MetaStore + S3 ObjectStore | 分布式控制面 |
| 3 | Phase 4：内核 NVMe 缓存 | 内核直接 I/O 本地 NVMe |

| 2 | Redis MetaStore + S3 ObjectStore | 本地 POSIX 子集已较完整后启动 |
| 3 | Phase 4：内核 NVMe 缓存 | 内核直接 I/O 本地 NVMe 块设备 |

> **⚠️ 明确**：在 Cursor 新提示词下达前，Codex **不要**自行开始 Redis/S3/Phase 4 等任何方向。只做 Cursor 提示词范围内的事。

---

## 9. Codex 工作方式

1. **每次新会话第一步**：读本 `HANDOFF.md` + `git log --oneline -20` + 相关源码文件。不要假设有任何前序对话上下文。
2. **只实现提示词范围**：提示词外的问题先问 Cursor 或跳过，不要自行扩大范围。
3. **完成后用中文汇报**：
   - 改动摘要（改了哪些文件、做了什么）
   - 测试输出（`cargo test` 结果、`cargo clippy` 结果、`make` 结果）
   - 风险（可能影响什么、有什么不确定的）
   - 未做项（提示词要求但没做的部分，以及原因）
4. **禁止事项**：
   - ❌ `git push --force`
   - ❌ 修改 `git config`
   - ❌ 擅自 commit（除非 Cursor 明确要求）
   - ❌ 擅自扩大范围（做提示词没要求的功能）
5. **真机 sudo 测试**：由人类执行。Codex 只需给出可复制的命令序列。
6. **ABI 版本**：新增 opcode 或改变 payload 布局时，必须同时更新 `kestrelfs/kestrelfs_ipc.h` 和 `daemon/src/abi.rs`，并 bump `KESTRELFS_ABI_VERSION` / `ABI_VERSION`。两处必须一致。
7. **内核编码**：不能有编译警告（`-Werror` 级别要求）。`make -C kestrelfs` 输出必须零 warning。
8. **Rust 编码**：`cargo clippy --all-targets -- -D warnings` 必须通过。
9. **vng 站立规则**：凡改动 `kestrelfs/*.c` 或依赖 mount 的行为，必须用 `vng --exec`（当前环境加 `--run`）或演进后的仓库脚本完成自动验证；人类 sudo 只作补充。

---

## 10. 交接检查清单

- [x] Step 15 ObjectStore GC 已由 Cursor 验收并提交（130 tests + vng）
- [x] ABI 版本核对无误（内核 = Rust = 11）
- [x] Step 8–15 已验收状态已写清
- [x] 下一步明确：等待 Cursor 的 Step 16 提示词
- [x] 已知限制与坑已列出（第 6 节）
- [x] `test-step15-gc-vng.sh` 已入库；vng 站立规则见 §9
---

## 11. 文档债务

以下文档与代码不一致，后续应更新（优先级低于功能开发）：

| 文件 | 问题 | 代码实际值 |
|---|---|---|
| `README.md` Usage 示例 | 部分段落仍偏 Phase 1 只读演示（hello.txt / Permission denied） | 动态可写 VFS 已可用；以 Roadmap + HANDOFF 为准 |
| `README.md` 细节 | 未逐一列举全部 DATA opcode | 完整 opcode 表见 `HANDOFF.md` / `kestrelfs_ipc.h` |
