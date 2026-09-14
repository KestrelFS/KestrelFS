# KestrelFS 研发交接文档（HANDOFF）

> **最后更新**：Phase 4 Step 21（cache namespace identity，format v2）已由 Cursor 验收并纳入本提交（IPC ABI 仍为 v11）。
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
| 当前阶段 | Phase 4 Step 21 已验收（v2 namespace identity）；DMA/eviction 尚未开始 |

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
                    │    ├─ FileMetaStore (meta.json)   │
                    │    └─ RedisMetaStore (可选原型)   │
                    │  ObjectStore (块数据)             │
                    │    ├─ MemObjectStore (内存)       │
                    │    ├─ LocalFsObjectStore (磁盘)   │
                    │    └─ S3ObjectStore (S3/MinIO)    │
                    └───────────▲──────────────────────┘
                                │ mmap() 共享内存双环 + ioctl/poll
                    ┌───────────▼──────────────────────┐
                    │        Kernel space (C)           │
                    │   kestrelfs.ko (out-of-tree)      │
                    │                                  │
                    │  VFS (super/inode/dir/file ops)  │
                    │  /dev/kestrel_ctl char device     │
                    │ 本地 NVMe 缓存 (Step 21 namespace) │
                    └──────────────────────────────────┘
```

- **内核模块** `kestrelfs.ko`：out-of-tree，注册 VFS 文件系统类型，实现 super/inode/dir/file operations。通过 `/dev/kestrel_ctl` 字符设备与 daemon 通信。
- **字符设备** `/dev/kestrel_ctl`：单个 `mmap()` 共享内存区域（144.2 KiB），内含两条独立无锁 SPSC 环形缓冲区（REQ 环 + RESP 环，各 1024 slot × 64 字节），以及 ring 后方一块 16 KiB data/name bounce buffer。唤醒模型：内核→Rust 用 `wake_up_interruptible()` + `poll()`；Rust→内核用 `KESTRELFS_IOC_NOTIFY_RESP` ioctl。
- **用户态 daemon** `kestrelfs-daemon`：Tokio 异步运行时。`poll()` 驱动事件循环，逐条处理 REQ 事件，批量推回 RESP。MetaStore 管理元数据（inode/dirent/slice），ObjectStore 管理块数据。
- **数据模型**（JuiceFS-like 分层）：File → Chunk（64 MiB 固定窗口）→ Slice（变长写记录，COW 语义）→ Block（4 MiB 物理对象，存于 ObjectStore）。
- **NVMe 缓存边界**：缓存由内核拥有；Step 21 将格式 bump 到 v2，在 superblock 持久化 32-byte namespace SHA-256 identity。指定 cache_device 时必须传 64-hex `cache_namespace`，不匹配则在恢复索引前 fail closed。Step 20 的 4 KiB 持久化索引、READ_DATA fill、同步 BIO hit 和四类失效保持不变。尚无 DMA、eviction、checksum 或多节点失效。禁止把普通文件（包括 ZFS dataset 中的文件）当 cache 设备。详细设计见 `docs/phase4-nvme-cache.md`。

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
│   ├── cache.c                  # Phase 4 v2 namespace-bound 持久化 cache + 同步 fill/hit/失效
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
│       ├── meta_redis.rs        # RedisMetaStore（单 key 快照 + Lua CAS）
│       ├── object_store.rs      # ObjectStore trait + MemObjectStore + LocalFsObjectStore
│       ├── object_store_s3.rs   # S3ObjectStore（AWS SDK、MinIO path-style）
│       ├── fs_model.rs          # Inode / Slice / Block 数据模型
│       ├── device.rs            # /dev/kestrel_ctl 打开/mmap/ABI校验
│       ├── ring.rs              # Rust 侧 ring buffer 读写
│       └── ioctl.rs             # ioctl 号常量
│
├── HANDOFF.md                   # ★ 本文件
├── docs/phase4-nvme-cache.md    # Phase 4 缓存归属、设备、索引与失效设计
├── STEP8_VERIFICATION.md        # Step 8 持久化验证指南
├── STEP9_MANUAL_TEST.md         # Step 9 mkdir/unlink 手工测试指南
├── test-persistence.sh          # 持久化集成测试脚本（需 sudo）
├── test-step19-cache-vng.sh     # Step 19 loop 格式化/复用/fail-closed/mount 回归
├── test-step20-cache-vng.sh     # Step 20/21 loop fill/reload/hit/失效/namespace 回归
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
| **Step 16** | **可切换 RedisMetaStore 原型（单 key 快照 + Lua CAS）** | **11（未变）** | **✅ 已验收** |
| **Step 17** | **可切换 S3ObjectStore 原型（AWS S3 / MinIO）** | **11（未变）** | **✅ 已验收** |
| **Phase 4 Step 18** | **内核拥有的 NVMe 缓存骨架：块设备参数 + 恒 miss read hook** | **11（未变）** | **✅ 已验收** |
| **Phase 4 Step 19** | **独占 claim 块设备 + v1 cache superblock + 内存/盘上索引骨架** | **11（未变）** | **✅ 已验收** |
| **Phase 4 Step 20** | **持久化索引恢复 + READ_DATA fill + 同步 BIO hit + mutation 失效** | **11（未变）** | **✅ 已验收** |
| **Phase 4 Step 21** | **cache namespace identity：v2 superblock + 64-hex 模块参数 + mismatch fail-closed** | **11（未变）** | **✅ 已验收** |

Cursor 对照代码、137 tests、`STEP21_NAMESPACE_PASS` 及 Step 20/19/15 vng 回归确认 Step 21 已验收。
cache format = **v2**；IPC ABI = **v11**。测试仅在 vng+loop；daemon 使用独立 `data_dir` + `"$data_dir/daemon.log"`。

Cursor 对照代码、137 tests、`STEP20_CACHE_PASS` 及 Step 19/15 vng 回归确认 Step 20 已验收。
**测试约束**：cache/mount 验证只在 vng guest + loop；禁止 Codex 触碰物理机 zvol。

Cursor 对照代码、137 tests、`STEP19_CACHE_PASS` 与 GC 回归确认 Step 19 已验收。
宿主机开发缓存盘：`/dev/zvol/nvraid1tank1/kestrel-cache`（已创建）。

Cursor 对照代码、137 tests、vng GC 回归与参数校验（`STEP18_PARAM_PASS`）确认 Step 18 已验收；Step 17 MinIO 门控测此前已验收。

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

| 层 | 内存模式 (`--memory`) | 默认持久化模式 | 可选远端后端 |
|---|---|---|---|
| 元数据 (MetaStore) | `MemStore`（纯 HashMap，重启丢失） | `FileMetaStore`（全量 JSON 到 `{data_dir}/meta.json`） | `RedisMetaStore`（`--meta redis://...`；`{prefix}:meta:v1` 单 key JSON + Lua CAS） |
| 块数据 (ObjectStore) | `MemObjectStore`（纯 HashMap，支持幂等 delete） | `LocalFsObjectStore`（`{data_dir}/{slice_uuid}/{block_idx}`） | `S3ObjectStore`（`--objects s3://bucket/prefix`；AWS S3 或 MinIO） |

**CLI 参数**（`daemon/src/main.rs`）：
- `--data-dir <PATH>`：持久化目录（默认 `./.kestrelfs-data`），meta.json 和块数据均存于此
- `--memory`：纯内存模式（元数据 + 块数据均不持久化）
- `--meta <REDIS_URL>`：将 metadata 切到 Redis，例如 `redis://127.0.0.1:6379/0`；与 `--memory` 冲突
- `--redis-prefix <PREFIX>`：Redis key 命名空间，默认 `kestrelfs`，实际 key 为 `<PREFIX>:meta:v1`
- `--objects <S3_URL>`：将对象数据切到 S3，例如 `s3://bucket/kestrelfs-data`；与 `--memory` 冲突，可与 FileMetaStore 或 RedisMetaStore 任意组合
- `--s3-endpoint <URL>`：可选 S3 兼容 endpoint；设置后强制 path-style，适配 MinIO。未传时读取 `S3_ENDPOINT`，再回退到 AWS SDK 默认 endpoint
- S3 凭据/region：走 AWS SDK provider chain；常用环境变量为 `AWS_ACCESS_KEY_ID`、`AWS_SECRET_ACCESS_KEY`、可选 `AWS_SESSION_TOKEN` 与 `AWS_REGION`

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
| 18 | **RedisMetaStore 是全量快照原型** | 每次读 GET/反序列化整份 JSON；每次写还要全量序列化并 Lua CAS，冲突最多重试 64 次。优点是 rename、inode 分配、slice 裁剪和 GC keys 与元数据在单 key 上线性化；大规模部署需拆 key/索引或采用服务端 Lua 数据模型。 | `daemon/src/meta_redis.rs` |
| 19 | **远端 metadata/object 必须成对配置** | Step 17 已可用 Redis + S3 补齐共享数据面；若只启用 Redis 而仍用不同节点的 LocalFs，或只启用 S3 而各节点使用不同 FileMetaStore，仍会出现 metadata/object 视图不一致。 | `daemon/src/main.rs` 存储选择 |
| 20 | **Redis 快照损坏/丢失时 fail closed** | 与 FileMetaStore 的“损坏后重置”不同，Redis key 已存在但 JSON 非法时 daemon 启动失败；运行中 key 被外部删除时 metadata 操作返回 EIO，避免静默创建新文件系统。 | `daemon/src/meta_redis.rs` |
| 21 | **Redis 连接仍是原型级** | 当前只接受 `redis://`（未启用 `rediss://` TLS），持有一条 multiplexed connection 且未加自动重连 manager；连接故障时请求返回 EIO，需恢复 Redis 后重启 daemon。URL 可能含凭据，因此启动日志不会打印 URL。 | `daemon/src/meta_redis.rs`、`daemon/src/main.rs` |
| 22 | **S3 delete 仍是提交后 best-effort** | Step 15 在 metadata 提交后调用 S3 DeleteObject；成功会真删对象，缺失对象视为成功。网络/权限失败只记录泄漏，不回滚已生效的 unlink/rename/truncate，也没有持久化重试队列。 | `daemon/src/object_store_s3.rs`、`daemon/src/main.rs` |
| 23 | **S3 原型不创建生产 bucket** | daemon 要求 bucket 已存在；只有设置 `S3_CREATE_BUCKET=1` 的门控测试会创建测试 bucket。自定义 endpoint 自动 force path-style；真实 AWS 默认使用 SDK endpoint/addressing。 | `daemon/src/object_store_s3.rs` |
| 24 | **NVMe cache hit 仍是同步原型** | Step 20 以固定 4 KiB block 和同步 BIO + `copy_to_user` 服务完整范围命中；非对齐首块不 fill，部分命中整次回退 READ_DATA。没有 DMA/零拷贝、readahead、LRU/eviction；slot 用尽只会停止新 fill。 | `kestrelfs/cache.c`、`docs/phase4-nvme-cache.md` |
| 25 | **cache v1 缺少崩溃完整性元数据** | 数据 flush 后才发布 index，正常 rmmod/insmod 可恢复；但单 superblock/index entry 没有 checksum、journal 或镜像，掉电 torn write 仍可能 fail closed 或极端情况下形成表面合法的坏条目。失效索引写失败会拒绝对应 mutation。 | `kestrelfs/cache.c` |
| 26 | **cache namespace identity 依赖部署规范化** | Step 21 已在 v2 superblock 绑定 32-byte SHA-256 digest，缺失/非法/mismatch 均拒绝加载；内核不解析 data-dir/Redis/S3 配置，调用方必须对稳定、无凭据、规范化的 MetaStore + ObjectStore descriptor 求 SHA-256。旧 v1 不自动迁移，也没有 wipe 参数。 | `kestrelfs/cache.c`、`docs/phase4-nvme-cache.md` |
| 27 | **多节点失效仍未闭环** | namespace identity 只防止不同 logical filesystem 混用 cache，不处理同 namespace 的远端 mutation。当前只观察本机 VFS mutation，其他节点或直接 Redis mutation 不会通知本内核失效。 | `kestrelfs/cache.c`、`docs/phase4-nvme-cache.md` |

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
cargo test                    # 单元测试 + 集成测试（当前 137 个）
cargo clippy --all-targets -- -D warnings   # 零警告
```

### 7.3 手工验证（需 sudo / 真机或 virtme-ng）

详细步骤见 `STEP8_VERIFICATION.md`（持久化）和 `STEP9_MANUAL_TEST.md`（mkdir/unlink），核心流程：

> **固定测试启动约定**：
> 1. 手工/vng 脚本必须显式 `insmod`（及需要的 `cache_device`/`cache_namespace` 参数）。
> 2. `--data-dir` **不强制** `/tmp/kestrelfs-debug`；由测试自选独立目录（推荐
>    `/tmp/kestrelfs-<step>-$$` 或脚本内 `data_dir=...`），避免用例互相污染。
> 3. daemon 标准输出/错误**不得**直接打到终端；重定向到日志文件（优先
>    `"$data_dir/daemon.log"`），失败时可 `tail` 该文件排查。不要再用
>    `>/dev/null 2>&1` 吞掉日志。
> 4. Redis/S3 等额外参数按用例需要追加；日志重定向规则不变。

```bash
# 1. 加载模块 + 启动 daemon（data_dir 可自选；日志进文件）
data_dir=/tmp/kestrelfs-manual-$$
mkdir -p "$data_dir"
sudo insmod kestrelfs/kestrelfs.ko
./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
  >"$data_dir/daemon.log" 2>&1 &
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
./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
  >"$data_dir/daemon.log" 2>&1 &
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

2026-09-14 对最终代码执行通过：unlink、rename overwrite、truncate COW GC
均通过，`umount_ms=88`，最终输出 `VNG_GC_PASS`。
另用临时 guest 脚本确认普通文件参数使 `insmod` 返回
`Block device required`，无设备时 `cache_size_mib=128` 可加载并最终输出
`STEP18_PARAM_PASS`；临时脚本未纳入仓库。

成功标记为 `VNG_GC_PASS`；guest 脚本任一断言失败均以非零状态退出。

### 7.10 Step 16 RedisMetaStore 集成测试

默认 `cargo test` 不要求本机存在 Redis；设置 `REDIS_URL` 后才执行真实 Redis
语义与重连恢复断言，测试使用随机 prefix 并在成功后删除测试 key：

```bash
cd daemon
REDIS_URL='redis://:<PASSWORD>@192.168.18.253:8379/15' \
  cargo test redis_url_gated_full_semantics_and_restart -- --nocapture
```

该测试覆盖两个 RedisMetaStore 并发 create、mkdir/create、symlink target、
rename 覆盖、truncate/unlink GC keys 以及重新构造 RedisMetaStore 后的恢复。
Redis mutation 先在快照副本上复用
MemStore 语义，再由 Lua 比较旧 JSON 并 `SET` 新 JSON；CAS 失败会从最新快照重试。

2026-09-14 已对 `192.168.18.253:8379` 的真实 Redis 执行上述门控测试：
`1 passed; 0 failed`（约 0.03s）。密码只通过 `REDIS_URL` 环境变量注入，禁止
硬编码进仓库文件。

人类如需补充挂载/重启测试，沿用固定 data-dir 与模块加载约定：

```bash
: "${KESTRELFS_REDIS_PASSWORD:?请先 export KESTRELFS_REDIS_PASSWORD}"
export REDIS_URL="redis://:${KESTRELFS_REDIS_PASSWORD}@192.168.18.253:8379/15"
make -C kestrelfs
cargo build --release --manifest-path daemon/Cargo.toml
sudo insmod kestrelfs/kestrelfs.ko
./daemon/target/release/kestrelfs-daemon --meta "$REDIS_URL" --redis-prefix kestrelfs-step16 --data-dir /tmp/kestrelfs-debug >/dev/null 2>&1 &
daemon_pid=$!
sleep 2
sudo mkdir -p /mnt/kestrelfs
sudo mount -t kestrelfs none /mnt/kestrelfs
sudo rm -f /mnt/kestrelfs/step16-file /mnt/kestrelfs/step16-link
printf 'redis metadata survives restart\n' | sudo tee /mnt/kestrelfs/step16-file >/dev/null
sudo ln -s step16-file /mnt/kestrelfs/step16-link
sudo umount /mnt/kestrelfs
kill "$daemon_pid"; wait "$daemon_pid" || true

./daemon/target/release/kestrelfs-daemon --meta "$REDIS_URL" --redis-prefix kestrelfs-step16 --data-dir /tmp/kestrelfs-debug >/dev/null 2>&1 &
daemon_pid=$!
sleep 2
sudo mount -t kestrelfs none /mnt/kestrelfs
test "$(sudo cat /mnt/kestrelfs/step16-file)" = "redis metadata survives restart"
test "$(sudo readlink /mnt/kestrelfs/step16-link)" = step16-file
sudo rm /mnt/kestrelfs/step16-link /mnt/kestrelfs/step16-file
time sudo umount /mnt/kestrelfs
kill "$daemon_pid"; wait "$daemon_pid" || true
sudo rmmod kestrelfs
```

### 7.11 Step 17 S3ObjectStore / MinIO 集成测试

默认 `cargo test` 不访问 S3；以下环境变量齐全时执行真实 S3/MinIO 测试。
`S3_CREATE_BUCKET=1` 只用于测试，可在 bucket 不存在时创建专用测试 bucket：

```bash
export S3_ENDPOINT=http://192.168.18.253:9000
export S3_BUCKET=kestrelfs-test
export S3_CREATE_BUCKET=1
export AWS_ACCESS_KEY_ID=minioadmin
export AWS_SECRET_ACCESS_KEY=minioadmin
export AWS_REGION=us-east-1
cd daemon
cargo test s3_environment_gated -- --nocapture
```

2026-09-14 已对上述 MinIO 执行：`2 passed; 0 failed`。一项覆盖
put/get/覆盖/幂等 delete，另一项覆盖 handle_write_bytes → unlink → Step 15 GC
→ S3 DeleteObject → GET NotFound。对象使用随机 prefix，测试结束后对象已删除。

人类补充 Redis + S3 挂载/重启烟测时，沿用固定模块和 data-dir 约定：

```bash
: "${KESTRELFS_REDIS_PASSWORD:?请先 export KESTRELFS_REDIS_PASSWORD}"
export REDIS_URL="redis://:${KESTRELFS_REDIS_PASSWORD}@192.168.18.253:8379/15"
export AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin AWS_REGION=us-east-1
sudo insmod kestrelfs/kestrelfs.ko
./daemon/target/release/kestrelfs-daemon --meta "$REDIS_URL" --redis-prefix kestrelfs-step17 --objects s3://kestrelfs-test/step17 --s3-endpoint http://192.168.18.253:9000 --data-dir /tmp/kestrelfs-debug >/dev/null 2>&1 &
daemon_pid=$!
sleep 2
sudo mkdir -p /mnt/kestrelfs
sudo mount -t kestrelfs none /mnt/kestrelfs
sudo rm -f /mnt/kestrelfs/step17-s3-file
printf 'redis plus s3 survives restart\n' | sudo tee /mnt/kestrelfs/step17-s3-file >/dev/null
sudo umount /mnt/kestrelfs
kill "$daemon_pid"; wait "$daemon_pid" || true

./daemon/target/release/kestrelfs-daemon --meta "$REDIS_URL" --redis-prefix kestrelfs-step17 --objects s3://kestrelfs-test/step17 --s3-endpoint http://192.168.18.253:9000 --data-dir /tmp/kestrelfs-debug >/dev/null 2>&1 &
daemon_pid=$!
sleep 2
sudo mount -t kestrelfs none /mnt/kestrelfs
test "$(sudo cat /mnt/kestrelfs/step17-s3-file)" = "redis plus s3 survives restart"
sudo rm /mnt/kestrelfs/step17-s3-file
time sudo umount /mnt/kestrelfs
kill "$daemon_pid"; wait "$daemon_pid" || true
sudo rmmod kestrelfs
```

### 7.12 Phase 4 Step 18 NVMe 缓存骨架验证

本步改动内核 read 路径，必须先执行 mount 级 vng 回归。现有 Step 15 脚本覆盖
模块加载、daemon、读写/truncate/GC，以及小于 1 秒的卸载断言：

```bash
make -C kestrelfs
cargo build --release --manifest-path daemon/Cargo.toml
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step15-gc-vng.sh"
```

人类补充验证参数可见性与恒 miss 回退时，仍沿用固定启动约定。Step 18 不会
打开或写入 `cache_device`；真正测试设备参数时只能传 loop/zvol/raw block
device，不能传普通文件：

```bash
sudo insmod kestrelfs/kestrelfs.ko cache_size_mib=128
test "$(cat /sys/module/kestrelfs/parameters/cache_size_mib)" = 128
./daemon/target/release/kestrelfs-daemon --data-dir /tmp/kestrelfs-debug >/dev/null 2>&1 &
daemon_pid=$!
sleep 2
sudo mkdir -p /mnt/kestrelfs
sudo mount -t kestrelfs none /mnt/kestrelfs
printf 'step18 still uses READ_DATA\n' | sudo tee /mnt/kestrelfs/step18.dat >/dev/null
test "$(sudo cat /mnt/kestrelfs/step18.dat)" = "step18 still uses READ_DATA"
time sudo umount /mnt/kestrelfs
kill "$daemon_pid"; wait "$daemon_pid" || true
sudo rmmod kestrelfs
```

### 7.13 Phase 4 Step 19 块设备格式验证

专用 vng 脚本在 guest 中创建 128 MiB 稀疏文件并挂为 loop 块设备，覆盖：

- 普通文件作为 `cache_device` 时拒绝加载；
- `cache_size_mib=64` 格式化，带 cache claim 挂载后 READ_DATA 读写回归；
- 卸载模块后以相同 geometry 重新加载，generation/superblock hash 不变；
- `cache_size_mib` 不一致及坏 magic 均 fail closed；
- 元数据区清零后 `cache_size_mib=0` 使用完整 128 MiB 设备。

```bash
make -C kestrelfs
cargo build --release --manifest-path daemon/Cargo.toml
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step19-cache-vng.sh"
```

2026-09-14 对最终实现执行通过，挂载回归 `umount_ms=88`，最终输出
`STEP19_CACHE_PASS`；最终版另跑 `test-step15-gc-vng.sh`，GC 全部通过，
`umount_ms=90`，输出 `VNG_GC_PASS`。

宿主机开发盘清单中仍有
`/dev/zvol/nvraid1tank1/kestrel-cache`（4K volblocksize），但按 2026-09-14
最新指令，Codex 不在物理开发机执行 cache/mount 测试；它不能替代 vng 验收。

### 7.14 Phase 4 Step 20 持久化 cache 验证

专用 vng 脚本在 guest 内创建 loop 块设备，并在 guest root 环境执行 `insmod`、
固定 `insmod`（含所需 cache 参数）、按用例自选 `--data-dir`、日志写入
`"$data_dir/daemon.log"`，以及 mount/rmmod。
它覆盖多块 READ_DATA fill、rmmod/insmod 后恢复、daemon 停止时的真实 hit，以及
rewrite/truncate/unlink/rename-overwrite 后禁止脏命中：

```bash
make -C kestrelfs
cargo build --release --manifest-path daemon/Cargo.toml
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step20-cache-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step15-gc-vng.sh"
```

2026-09-14 执行通过：恢复 4 个盘上 index entry；停 daemon 后 reload hit 数据
一致；四类 mutation 失效断言全过；`STEP20_CACHE_PASS`，最终复跑 umount 58 ms。
GC 回归输出 `VNG_GC_PASS`，umount 92 ms。

### 7.15 Phase 4 Step 21 cache namespace identity 验证

`test-step20-cache-vng.sh` 已扩展 namespace 断言：用规范化 local descriptor 的
SHA-256 作为 identity A，在同一 loop 填充并卸载；随后 identity B 加载必须返回
错误且日志包含 `cache namespace identity mismatch`；最后重新用 A 加载并在停掉
daemon 后读出持久化数据。脚本内每次 cache_device `insmod` 都显式传
`cache_namespace`；daemon 按 §7.3 / §8 约定自选 `--data-dir` 并将日志写入
`"$data_dir/daemon.log"`（勿再 `>/dev/null`）。

```bash
make -C kestrelfs
cargo build --release --manifest-path daemon/Cargo.toml
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step20-cache-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step19-cache-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step15-gc-vng.sh"
```

2026-09-14 当前实现执行通过：identity B 被拒绝，identity A 随后恢复 4 个 index
entry 并在 daemon 停止时正确命中；最终输出 `STEP21_NAMESPACE_PASS` 和
`STEP20_CACHE_PASS`，umount 56 ms。同期 Step 19 输出 `STEP19_CACHE_PASS`
（umount 92 ms），Step 15 输出 `VNG_GC_PASS`（umount 100 ms）。

直接使用模块时，部署层先计算无凭据规范 descriptor 的 SHA-256；示例仅用于 vng
guest + loop，不得改成物理机 cache_device：

```bash
cache_namespace=$(printf '%s' \
  'v1;meta=file:/tmp/kestrelfs-debug/meta.json;objects=local:/tmp/kestrelfs-debug' \
  | sha256sum | awk '{print $1}')
insmod kestrelfs/kestrelfs.ko cache_device=/dev/loop0 cache_size_mib=64 \
  cache_namespace="$cache_namespace"
./daemon/target/release/kestrelfs-daemon --data-dir /tmp/kestrelfs-debug >/dev/null 2>&1 &
```

---

## 8. 路线图（未做）

按 Cursor 既定策略的推荐优先级：

| 优先级 | 内容 | 说明 |
|---|---|---|
| 1 | **等待 Cursor 的 Step 22 提示词** | 候选：DMA/零拷贝 hit、eviction、checksum/journal |
| 2 | Phase 4 后续：生产化缓存 | 多节点失效通知等 |
| 3 | 分布式后端生产化 | Redis 拆 key、GC 重试等（须 Cursor 明示） |

> **⚠️ 明确**：在 Cursor 新提示词下达前，不继续扩大 Phase 4 hit/DMA，也不自行改做分布式生产化。

### Codex 自动验证与权限（无交互密码）

**硬门槛是 `insmod`/`rmmod`（需要 root），不是 zvol 的用户态 ACL。**
模块加载后由内核以 root 打开 `cache_device`，因此把用户加入 `disk` 组或 chmod zvol
**不能**单独替代 sudo。

**当前执行策略（2026-09-14 最新指令）**：Codex 的 `insmod`、cache block
device 和 mount 行为验证必须在 vng guest 内完成，禁止在物理开发机执行。guest
内使用 loop block device，不能把 backing 普通文件直接传给 `cache_device`。
宿主机 zvol 路径只作资产记录，除非人类以后明确撤销本条限制，否则不得触碰。

所有新 vng/手工测试脚本仍必须显式包含 `insmod`。daemon 启动约定：

```bash
data_dir=/tmp/kestrelfs-<step>-$$   # 或脚本自定义；勿写死成唯一全局路径
mkdir -p "$data_dir"
./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
  >"$data_dir/daemon.log" 2>&1 &
# 失败时：tail -n 100 "$data_dir/daemon.log"
```

不要把 daemon 日志丢进 `/dev/null`。`--data-dir` 不必固定为 `/tmp/kestrelfs-debug`。

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
5. **物理机测试**：当前禁止 Codex 在物理开发机执行 insmod/mount/cache-device 测试；统一使用 vng guest。
6. **ABI 版本**：新增 opcode 或改变 payload 布局时，必须同时更新 `kestrelfs/kestrelfs_ipc.h` 和 `daemon/src/abi.rs`，并 bump `KESTRELFS_ABI_VERSION` / `ABI_VERSION`。两处必须一致。
7. **内核编码**：不能有编译警告（`-Werror` 级别要求）。`make -C kestrelfs` 输出必须零 warning。
8. **Rust 编码**：`cargo clippy --all-targets -- -D warnings` 必须通过。
9. **vng 站立规则**：凡改动 `kestrelfs/*.c` 或依赖 mount 的行为，必须用 `vng --exec`（当前环境加 `--run`）或演进后的仓库脚本完成自动验证；人类 sudo 只作补充。
10. **daemon 启动**：`--data-dir` 由用例自选；日志重定向到 `"$data_dir/daemon.log"`（或等价日志文件），禁止 `>/dev/null` 丢弃输出。

- [x] Step 21 cache namespace identity 已由 Cursor 验收并提交（format v2）
- [x] IPC ABI = 11；cache format = v2
- [x] Step 8–20 + Phase 4 Step 21 已验收状态已写清
- [x] 下一步明确：等待 Cursor 的 Step 22 提示词
- [x] 测试约束：cache/mount 只在 vng+loop；daemon 日志写 `"$data_dir/daemon.log"`（§7.3 / §8 / §9.10）

---

## 11. 文档债务

以下文档与代码不一致，后续应更新（优先级低于功能开发）：

| 文件 | 问题 | 代码实际值 |
|---|---|---|
| `README.md` Usage 示例 | 部分段落仍偏 Phase 1 只读演示（hello.txt / Permission denied） | 动态可写 VFS 已可用；以 Roadmap + HANDOFF 为准 |
| `README.md` 细节 | 未逐一列举全部 DATA opcode | 完整 opcode 表见 `HANDOFF.md` / `kestrelfs_ipc.h` |
