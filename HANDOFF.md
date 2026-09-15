# KestrelFS 研发交接文档（HANDOFF）

> **最后更新**：Step 34 POSIX-ATTR（create/mkdir mode + 目录 nlink）已由 Cursor 验收并纳入本提交（IPC ABI v13、cache format v4）。下一步见 `docs/remaining-capabilities.md` §8。
> **核对应法**：以 `git log --oneline -5` 与本文件进度表为准；若与代码冲突，以代码为准并更新本文档。规划/决策以 `docs/remaining-capabilities.md` 为准。

---

## 1. 项目身份

| 项 | 值 |
|---|---|
| 产品名 | **KestrelFS** |
| 本地仓库目录名 | 可能叫 **FerroFS**（历史目录名），产品名和 GitHub 仓库名均为 KestrelFS |
| 一句话定位 | 高性能云原生分布式文件系统；C 内核模块 + Rust daemon 混合架构；对标/超越 JuiceFS（缓存命中路径零上下文切换） |
| License | Apache-2.0 |
| 上游 | `https://github.com/KestrelFS/KestrelFS`（以 README 为准） |
| 当前阶段 | Step 34 POSIX-ATTR 已验收；下一步 Step 35 = CACHE-COHERENCE（见 remaining-capabilities §8） |

---

## 2. 协作角色（固定）

| 角色 | 职责 |
|---|---|
| **Cursor** | 本交接的验收方：定路线、写提示词、验收、打回。提示词写入 `docs/remaining-capabilities.md` §8。 |
| **Codex** | 实现 agent：读 HANDOFF + `docs/remaining-capabilities.md` §8 执行；结果写入该文档 §9。不自行决定路线。 |
| **人类** | 让 Codex「执行 remaining-capabilities §8」；跑需授权的真机测试。 |
| **OpenCode** | 历史实现至 Phase 3 Step 10；亦可按 §8 接活。 |

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
                    │    └─ RedisMetaStore (v2 分记录)  │
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
                    │ 本地 NVMe 缓存 (Step 29 batch LRU) │
                    └──────────────────────────────────┘
```

- **内核模块** `kestrelfs.ko`：out-of-tree，注册 VFS 文件系统类型，实现 super/inode/dir/file operations。通过 `/dev/kestrel_ctl` 字符设备与 daemon 通信。
- **字符设备** `/dev/kestrel_ctl`：单个 `mmap()` 共享内存区域（144.2 KiB），内含两条独立无锁 SPSC 环形缓冲区（REQ 环 + RESP 环，各 1024 slot × 64 字节），以及 ring 后方一块 16 KiB data/name bounce buffer。唤醒模型：内核→Rust 用 `wake_up_interruptible()` + `poll()`；Rust→内核用 `KESTRELFS_IOC_NOTIFY_RESP` ioctl。
- **用户态 daemon** `kestrelfs-daemon`：Tokio 异步运行时。`poll()` 驱动事件循环，逐条处理 REQ 事件，批量推回 RESP。MetaStore 管理元数据（inode/dirent/slice）及待删除对象队列，ObjectStore 管理块数据；Step 30 在启动及运行中重试幂等删除；Step 31 把 Redis metadata 拆为 v2 分记录 HASH/SET，并以 Lua revision-CAS 原子提交复合 mutation。
- **数据模型**（JuiceFS-like 分层）：File → Chunk（64 MiB 固定窗口）→ Slice（变长写记录，COW 语义）→ Block（4 MiB 物理对象，存于 ObjectStore）。
- **NVMe 缓存边界**：缓存由内核拥有；v4 superblock 持久化 32-byte namespace SHA-256 identity 并由 CRC32 保护，指定 cache_device 时必须传 64-hex `cache_namespace`，不匹配则在恢复索引前 fail closed。Step 22–26 落地最多 128 KiB pinned-page BIO、block-LRU、CRC32、单页 intent journal 和 rwsem 并行同步 hit；Step 27 提供离线 inspect/双确认 metadata wipe。Step 28 把动态 regular file 切到 `read_iter`，cache hit 和 READ_DATA miss 直接消费 `iov_iter`。Step 29 在 v4 journal reserved 中记录最多 64 个 batch victim（默认 16 且至多总槽位 1/16），按 index page 合并清零，提交后才允许 slot 复用；LRU 尾部近期热点不进入小批次。正常 insmod/mount 路径仍不会自动 wipe/迁移。尚无 page-cache/readahead/splice 全覆盖、真正异步 completion 或多节点失效。禁止把普通文件（包括 ZFS dataset 中的文件）当 cache 设备。详细设计见 `docs/phase4-nvme-cache.md`。

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
│   ├── cache.c                  # Phase 4 v4 cache + journal/checksum/并行 pinned hit/batch LRU/失效
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
│       ├── meta_redis.rs        # RedisMetaStore（v2 分记录 HASH/SET + Lua revision-CAS）
│       ├── object_store.rs      # ObjectStore trait + MemObjectStore + LocalFsObjectStore
│       ├── object_store_s3.rs   # S3ObjectStore（AWS SDK、MinIO path-style）
│       ├── fs_model.rs          # Inode / Slice / Block 数据模型
│       ├── device.rs            # /dev/kestrel_ctl 打开/mmap/ABI校验
│       ├── ring.rs              # Rust 侧 ring buffer 读写
│       └── ioctl.rs             # ioctl 号常量
│
├── HANDOFF.md                   # ★ 本文件（已验收事实）
├── docs/remaining-capabilities.md # ★ 规划/决策/当前 Codex 提示词/实现日志
├── docs/phase4-nvme-cache.md    # Phase 4 缓存归属、设备、索引与失效设计
├── tools/                       # cache 离线运维工具（不进入内核/IPC）
│   ├── Makefile
│   └── kestrelfs-cache-admin.c  # v4 inspect + 双确认 metadata wipe
├── STEP8_VERIFICATION.md        # Step 8 持久化验证指南
├── STEP9_MANUAL_TEST.md         # Step 9 mkdir/unlink 手工测试指南
├── test-persistence.sh          # 持久化集成测试脚本（需 sudo）
├── test-step19-cache-vng.sh     # Step 19 loop 格式化/复用/fail-closed/mount 回归
├── test-step20-cache-vng.sh     # Step 20/21 loop fill/reload/hit/失效/namespace 回归
├── test-step22-cache-vng.sh     # Step 22 pinned-page hit/fallback/A-B vng 回归
├── test-step22-cache-io.c       # Step 22 对齐 IO 校验/粗测辅助程序
├── test-step23-eviction-vng.sh  # Step 23 小 cache 满盘/LRU/reload 回归
├── test-step24-checksum-vng.sh  # Step 24 data/index 破坏与 v2 拒绝回归
├── test-step25-cache-txn-vng.sh # Step 25 半提交/journal/superblock fail-closed 回归
├── test-step25-cache-txn.c      # Step 25/29 v4 单条与 batch journal 故障注入辅助程序
├── test-step26-cache-async-vng.sh # Step 26 串行/并行 A-B 与并发失效回归
├── test-step26-cache-concurrency.c # Step 26 reader/rewrite 数据一致性辅助程序
├── test-step27-ops-recovery-vng.sh # Step 27 inspect/wipe/reformat/refill 回归
├── test-step28-cache-vfs-vng.sh # Step 28 read_iter/iovec/EOF/daemon-free hit 回归
├── test-step28-cache-vfs.c      # Step 28 preadv 与 iovec guard 辅助程序
├── test-step29-cache-evict-vng.sh # Step 29 批量 LRU/index 合并写/崩溃恢复回归
├── test-step32-posix-core-vng.sh # Step 32 硬链接/重启/nlink/末引用 GC 回归
├── test-step33-posix-rename-vng.sh # Step 33 RENAME_NOREPLACE / EEXIST 原子性
├── test-step33-renameat2.c       # renameat2 小助手（供 Step 33 vng 使用）
├── test-step34-posix-attr-vng.sh # Step 34 mode/目录 nlink/重启恢复回归
├── test-step34-posix-attr.c      # open/mkdir 原始 mode 测试辅助程序
├── test-vm-virtme.sh            # virtme-ng 虚拟机测试脚本
├── test-vm-interactive.sh       # QEMU 交互式测试脚本（busybox initramfs）
├── QEMU-TEST.md                 # QEMU 测试说明
└── README.md                    # 项目中文概览与使用说明
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
| **Phase 4 Step 22** | **对齐连续 cache hit 合并 BIO 直达 pinned user pages；partial/unaligned 安全回退** | **11（未变）** | **✅ 已验收** |
| **Phase 4 Step 23** | **满盘 block-LRU：安全清旧 index、复用 slot、持久化 replacement** | **11（未变）** | **✅ 已验收** |
| **Phase 4 Step 24** | **v3 data/index CRC32：坏 data 局部退休并 miss，坏 index 加载失败** | **11（未变）** | **✅ 已验收** |
| **Phase 4 Step 25** | **v4 单页 intent journal + superblock CRC：半提交恢复为安全 miss** | **11（未变）** | **✅ 已验收** |
| **Phase 4 Step 26** | **cache rwsem：多个 pinned/buffered hit 并行同步 BIO，mutation 写侧排他** | **11（未变）** | **✅ 已验收** |
| **Phase 4 Step 27** | **离线 v4 inspect + 块设备双确认 metadata wipe + reformat/refill** | **11（未变）** | **✅ 已验收** |
| **Phase 4 Step 28** | **regular file read_iter + iov_iter cache/miss 路径 + 安全跨段 fallback** | **11（未变）** | **✅ 已验收** |
| **Phase 4 Step 29** | **批量 LRU victim journal + 同页 index 合并清零 + MRU 保护/崩溃恢复** | **11（未变）** | **✅ 已验收** |
| **Phase 4/控制面 Step 30** | **metadata 同事务持久化 GC queue + 启动/运行期指数退避重试** | **11（未变）** | **✅ 已验收** |
| **Phase 4/控制面 Step 31** | **Redis v2 分记录 schema + 字段级 diff + Lua revision-CAS 原子 mutation** | **11（未变）** | **✅ 已验收** |
| **Phase 4/控制面 Step 32** | **硬链接：持久化 nlink、多 dirent 同 inode、末引用 GC、VFS `.link`** | **12** | **✅ 已验收** |
| **Phase 4/控制面 Step 33** | **rename flags：原子 `RENAME_NOREPLACE`；EXCHANGE/WHITEOUT 仍拒绝** | **13** | **✅ 已验收** |
| **Phase 4/控制面 Step 34** | **create/mkdir mode + 持久化目录 nlink + VFS 属性刷新** | **13（未变）** | **✅ 已验收** |

Cursor 对照代码、151 tests、Redis 门控测与 `STEP32_POSIX_CORE_PASS` 确认 Step 32 已验收。
硬链接持久 nlink + 末引用 GC；`iget_locked` 同挂载别名共享 VFS inode。ABI **v12**；format **v4**。

Cursor 对照代码、158 tests 与 `STEP33_POSIX_RENAME_PASS` 确认 Step 33 已验收。
`RENAME_DATA` payload flags 支持原子 `RENAME_NOREPLACE`；EXCHANGE/WHITEOUT 仍拒绝。ABI **v13**；format **v4**。

Cursor 对照代码、162 tests 与 `STEP34_POSIX_ATTR_PASS` 确认 Step 34 已验收。
create/mkdir 持久化 `0o7777` 权限位；目录 nlink=`2+子目录数`。ABI **v13**；format **v4**。

下一步：**Step 35 CACHE-COHERENCE**，提示词在 `docs/remaining-capabilities.md` §8。

> **当前 ABI**：`KESTRELFS_ABI_VERSION = 13`（含 `RENAME_DATA` flags）

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

**`KESTRELFS_ABI_VERSION = 13`**（内核 `kestrelfs_ipc.h` 与 Rust `abi.rs` 一致）

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
12. Phase 4/控制面 Step 32：新增 LINK_DATA；payload 携带 parent inode、现有 inode 与 name_len，新名字位于 bounce，响应返回持久化 nlink
13. Phase 4/控制面 Step 33：扩展 RENAME_DATA payload，offset 20 增加 u32 rename flags；支持 `RENAME_NOREPLACE`

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
| 13 | `OP_RENAME_DATA` | 从 bounce 读取 old/new name；ABI v13 payload flags 支持 `RENAME_NOREPLACE` | 9（v13 扩展 flags） |
| 14 | `OP_LOOKUP_DATA` | 从 bounce buffer 读取名字并查找 | 10 |
| 15 | `OP_CREATE_DATA` | 从 bounce buffer 读取名字并创建文件 | 10 |
| 16 | `OP_MKDIR_DATA` | 从 bounce buffer 读取名字并创建目录 | 10 |
| 17 | `OP_UNLINK_DATA` | 从 bounce buffer 读取名字并删除文件或空目录 | 10 |
| 18 | `OP_READDIR_DATA` | 通过 bounce buffer 批量返回变长目录条目 | 10 |
| 19 | `OP_SYMLINK_DATA` | bounce 中依次传输 link name 与 target，创建符号链接 | 11 |
| 20 | `OP_READLINK_DATA` | daemon 将符号链接 target 返回到 bounce buffer | 11 |
| 21 | `OP_LINK_DATA` | 为现有非目录 inode 创建 bounce 长名硬链接，返回更新后的 nlink | 12 |
| 64 | `OP_RESULT_OK` | 响应：成功 | 1 |
| 65 | `OP_RESULT_ERROR` | 响应：失败（error_code 携带负 errno） | 1 |

### 5.5 存储现状

| 层 | 内存模式 (`--memory`) | 默认持久化模式 | 可选远端后端 |
|---|---|---|---|
| 元数据 (MetaStore) | `MemStore`（纯 HashMap，重启丢失） | `FileMetaStore`（全量 JSON 到 `{data_dir}/meta.json`） | `RedisMetaStore`（`--meta redis://...`；v2 control/inode/dirent/slice/symlink/GC HASH/SET + Lua revision-CAS） |
| 块数据 (ObjectStore) | `MemObjectStore`（纯 HashMap，支持幂等 delete） | `LocalFsObjectStore`（`{data_dir}/{slice_uuid}/{block_idx}`） | `S3ObjectStore`（`--objects s3://bucket/prefix`；AWS S3 或 MinIO） |

**CLI 参数**（`daemon/src/main.rs`）：
- `--data-dir <PATH>`：持久化目录（默认 `./.kestrelfs-data`），meta.json 和块数据均存于此
- `--memory`：纯内存模式（元数据 + 块数据均不持久化）
- `--meta <REDIS_URL>`：将 metadata 切到 Redis，例如 `redis://127.0.0.1:6379/0`；与 `--memory` 冲突
- `--redis-prefix <PREFIX>`：Redis key 命名空间，默认 `kestrelfs`，v2 keys 为 `<PREFIX>:meta:v2:{control,inodes,dirents,slices,symlinks,gc}`
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
| 4 | **GC 为持久化 at-least-once 删除** | Step 30 让 unlink、rename 覆盖和 truncate 在提交 metadata 时一并持久化无引用 block key；delete 成功后才确认出队，失败不回滚命名空间并在启动/运行期重试。永久后端故障会使队列增长，尚无容量上限/dead-letter/管理接口。 | `daemon/src/meta.rs`、`daemon/src/meta_persist.rs`、`daemon/src/meta_redis.rs`、`daemon/src/main.rs` |
| 5 | **按 ino 使用基础 iget，未恢复历史 iget5 自定义方案** | Step 32 为保证硬链接别名共享 VFS `i_nlink`，改用标准 `iget_locked(sb, ino)`；没有恢复曾导致卸载死循环的 `iget5_locked()` 自定义 test/set 路径，也没有引入 open-handle 生命周期。 | `kestrelfs/inode.c` `kestrelfs_get_inode()` |
| 6 | **inode identity 仍是单挂载、daemon inode id** | 同一 superblock 内的硬链接别名复用一个 inode；不同 mount 各自维护 VFS inode 实例，MetaStore 仍是持久属性权威。open-unlink 生命周期仍未建模。 | 同上、`daemon/src/meta.rs` |
| 7 | **evict_inode 禁止发 IPC** | `kestrelfs_evict_inode()` 只做 `truncate_inode_pages_final` + `clear_inode`，绝不发 IPC（daemon 可能已关闭，会死锁）。 | `kestrelfs/inode.c` |
| 8 | **JSON 全量落盘** | `FileMetaStore` 每次写操作后将整个元数据状态序列化为 JSON 写盘。简单但低效；inode 数量大时性能差。 | `daemon/src/meta_persist.rs` `sync_to_disk()` |
| 9 | **meta.json 损坏 → 数据丢失** | 若 `meta.json` 反序列化失败（JSON 损坏），daemon 回退到全新 `MemStore::new()`（仅含 root + remote.txt + writable.dat），之前用户创建的文件元数据全部丢失。块数据仍在磁盘但无法访问。 | `daemon/src/meta_persist.rs` `FileMetaStore::new()` |
| 10 | **rename flags 仅支持 NOREPLACE** | Step 33 支持原子 `RENAME_NOREPLACE`；`RENAME_EXCHANGE` / `RENAME_WHITEOUT` / 未知位返回 `-EINVAL`。Linux VFS 对已存在目标（包括同 inode 硬链接别名）会在 `.rename` 回调前返回 `EEXIST`；MetaStore 层同 inode 仍为成功 no-op。 | `kestrelfs/dir.c`、`daemon/src/meta.rs` |
| 11 | **目录 nlink 只表达直接子目录数** | Step 34 按 POSIX 常见不变量持久化 `2 + immediate_subdirectory_count`，覆盖 mkdir/rmdir 与目录 rename；它不是递归后代计数。不同 mount 的 VFS inode 仍各自刷新 MetaStore 权威值。 | `daemon/src/meta.rs`、`kestrelfs/dir.c` |
| 12 | **READDIR_DATA 每批受 16 KiB 限制** | daemon 按 inode 排序并在 bounce 中打包尽可能多的完整变长条目；大目录仍需分页 IPC，但不再固定每次只返回 1 条。 | `daemon/src/main.rs` `handle_readdir_data()` |
| 13 | **O_APPEND 手动处理** | 内核用 `f_op->write` 而非 `write_iter`，VFS 不会自动 seek 到 EOF。代码中手动检查 `O_APPEND` 并更新 `*ppos`。 | `kestrelfs/file.c` `kestrelfs_writable_write()` |
| 14 | **尚无 chmod/chown 属性 mutation** | Step 34 已让 create/mkdir 持久化传入的 `0o7777` 位并由操作强制文件类型；创建后的 chmod/chown 与完整时间属性修改仍未实现。 | `daemon/src/meta.rs`、`kestrelfs/dir.c` |
| 15 | **symlink target 当前要求 UTF-8 且 ≤4095 字节** | Linux 原生 symlink target 可为任意非 NUL 字节；当前 MetaStore 使用 `String`，ABI 解码拒绝非 UTF-8，target 上限为 4095 字节。悬空链接与相对链接均支持。 | `daemon/src/meta.rs`、`daemon/src/abi.rs` |
| 16 | **GC 引用确认是 O(全量 slice)** | 每次产生删除候选及每次读取待删队列时扫描所有剩余 slice 构建 block key 引用集合，正确处理共享 key，但 inode/slice 或积压队列很大时成本较高；后续可用引用计数优化。 | `daemon/src/meta.rs` `confirmed_garbage_keys()` / `pending_garbage()` |
| 17 | **未实现 open-unlink 延迟回收** | 当前没有 open handle/refcount ABI；unlink 会立即移除 inode/slice 并回收块，已打开 fd 在 unlink 后继续读写的完整 POSIX 语义尚未建模。 | `daemon/src/meta.rs` `unlink()` |
| 18 | **Redis v2 写放大已降低，mutation 读放大仍在** | Step 31 把 metadata 拆为固定 HASH/SET，`lookup/getattr/read_slices/readlink` 定向读取，mutation 只写发生变化的 fields；但为复用 MemStore 的完整 rename/truncate/引用确认语义，每次 mutation 仍一致读取各聚合 HASH 并在客户端计算 diff，冲突最多重试 64 次。readdir 与 GC 引用确认也仍有聚合扫描。 | `daemon/src/meta_redis.rs` |
| 19 | **远端 metadata/object 必须成对配置** | Step 17 已可用 Redis + S3 补齐共享数据面；若只启用 Redis 而仍用不同节点的 LocalFs，或只启用 S3 而各节点使用不同 FileMetaStore，仍会出现 metadata/object 视图不一致。 | `daemon/src/main.rs` 存储选择 |
| 20 | **Redis schema 异常/旧 v1 fail closed** | 与 FileMetaStore 的“损坏后重置”不同，v2 control/record 非法、版本未知、control 缺失但残留 v2 key 或旧 `<prefix>:meta:v1` 存在时 daemon 启动失败；运行中 schema/control 被删除或破坏时 metadata 操作返回 EIO。没有 v1 自动迁移或自动 wipe。 | `daemon/src/meta_redis.rs` |
| 21 | **Redis 连接仍是原型级** | 当前只接受 `redis://`（未启用 `rediss://` TLS），持有一条 multiplexed connection 且未加自动重连 manager；连接故障时请求返回 EIO，需恢复 Redis 后重启 daemon。URL 可能含凭据，因此启动日志不会打印 URL。 | `daemon/src/meta_redis.rs`、`daemon/src/main.rs` |
| 22 | **S3 GC 是持久队列 + at-least-once delete** | Step 30 将候选存入所选 File/Redis MetaStore；S3 DeleteObject 成功或对象已缺失后确认出队，网络/权限失败按 1–60 秒退避重试且不回滚命名空间。Redis v2 `gc` SET 由同一 Lua mutation 入队/ack，多个 daemon 可看到同一队列；幂等 delete/revision-CAS ack 可容忍重复处理，但尚无跨 Redis/S3 原子事务或队列限流。 | `daemon/src/object_store_s3.rs`、`daemon/src/meta_redis.rs`、`daemon/src/main.rs` |
| 23 | **S3 原型不创建生产 bucket** | daemon 要求 bucket 已存在；只有设置 `S3_CREATE_BUCKET=1` 的门控测试会创建测试 bucket。自定义 endpoint 自动 force path-style；真实 AWS 默认使用 SDK endpoint/addressing。 | `daemon/src/object_store_s3.rs` |
| 24 | **NVMe cache hit 是并行但仍同步的受限少拷贝原型** | Step 28 已用 `read_iter` / `iov_iter` 覆盖普通 read/pread/readv/preadv。完整、对齐且位于单个当前用户 iovec 段的连续 4 KiB blocks 最多合并 128 KiB 并直达 pinned pages；跨段、partial/unaligned、kernel-backed iter 或 GUP/BIO 构造失败仍走同步 BIO + `copy_to_iter`。没有真正异步 completion、跨 iovec scatter-gather BIO、page-cache/readahead 或 splice 全覆盖；mutation 会等待慢 reader。 | `kestrelfs/file.c`、`kestrelfs/cache.c`、`docs/phase4-nvme-cache.md` |
| 25 | **cache v4 journal 正确但同步 flush 成本高** | 每个完整 4 KiB data block、32-byte index entry、superblock 和 journal 都有 CRC32。单页 intent journal 将 fill/invalidate/evict/坏块退休的半提交状态恢复为安全 miss；torn journal/superblock fail closed。metadata mutation 由 cache rwsem 写侧保证单事务，且每次 index mutation 新增 journal prepare/clear 两次同步写与 flush；仍无双 superblock/metadata 镜像，CRC32 也不是密码学保护。 | `kestrelfs/cache.c` |
| 26 | **cache namespace identity 依赖部署规范化** | Step 21 起 superblock 绑定 32-byte SHA-256 digest，当前 v4 继续沿用；缺失/非法/mismatch 均拒绝加载。内核不解析 data-dir/Redis/S3 配置，调用方必须对稳定、无凭据、规范化的 MetaStore + ObjectStore descriptor 求 SHA-256。旧 v1/v2/v3 不自动迁移；Step 27 工具只提供显式 metadata wipe。 | `kestrelfs/cache.c`、`docs/phase4-nvme-cache.md` |
| 27 | **多节点失效仍未闭环** | namespace identity 只防止不同 logical filesystem 混用 cache，不处理同 namespace 的远端 mutation。当前只观察本机 VFS mutation，其他节点或直接 Redis mutation 不会通知本内核失效。 | `kestrelfs/cache.c`、`docs/phase4-nvme-cache.md` |
| 28 | **batch block-LRU 热度仍只在内存** | Step 29 默认一次退休 16 个 LRU victim（至多总槽位 1/16），用一份 journal 并按 index page 合并清零；连续 fill 可消费预回收槽位，MRU 尾部受到小批量保护。为避免破坏 hit 性能，不在每次访问持久化 recency；rmmod/insmod 后仍按 generation 恢复 insertion-order 近似。没有分区配额/租户热点隔离；victim 分散时仍需每个 index page 一次同步写，fill/invalidate 仍逐次 journal。 | `kestrelfs/cache.c`、`docs/phase4-nvme-cache.md` |
| 29 | **Step 27 wipe 不是安全擦除或自动修复** | 工具只清零并 fsync 前 2 MiB cache metadata，使旧 data slot 不再可寻址并允许重新 format；data 区字节仍可能由 raw 取证读到。wipe 要求模块卸载、目标为块设备、exclusive open、环境变量精确匹配设备路径及命令行旗标；不会修复单个 entry、自动迁移旧格式或修改权威 MetaStore/ObjectStore。 | `tools/kestrelfs-cache-admin.c` |

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
cargo test                    # 单元测试 + 集成测试（Step 34 验收基线 162 个）
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

### 7.10 Step 16/31 RedisMetaStore 集成测试

默认 `cargo test` 不要求本机存在 Redis；设置 `REDIS_URL` 后才执行真实 Redis
语义与重连恢复断言，测试使用随机 prefix 并在成功后删除测试 key：

```bash
cd daemon
REDIS_URL='redis://:<PASSWORD>@192.168.18.253:8379/15' \
  cargo test redis_url_gated_full_semantics_and_restart -- --nocapture
```

该测试当前覆盖两个 RedisMetaStore 并发 create、mkdir/create、symlink target、
rename 覆盖、truncate/unlink GC keys、重新构造 RedisMetaStore 后的恢复，以及旧 v1
schema fail-closed。Step 31 起 Redis mutation 先从 v2 HASH/SET 读取一致状态并
复用 MemStore 语义，再由 Lua 校验 revision 并原子应用字段级 diff；CAS 失败会从
最新 revision 重试。

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
data_dir=/tmp/kestrelfs-step31-$$
mkdir -p "$data_dir"
./daemon/target/release/kestrelfs-daemon --meta "$REDIS_URL" \
  --redis-prefix kestrelfs-step31 --data-dir "$data_dir" \
  >"$data_dir/daemon.log" 2>&1 &
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
data_dir=/tmp/kestrelfs-step21-demo
mkdir -p "$data_dir"
cache_namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
  "$data_dir" "$data_dir" | sha256sum | awk '{print $1}')
insmod kestrelfs/kestrelfs.ko cache_device=/dev/loop0 cache_size_mib=64 \
  cache_namespace="$cache_namespace"
./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
  >"$data_dir/daemon.log" 2>&1 &
```

### 7.16 Phase 4 Step 22 pinned-user-page cache hit 验证

`test-step22-cache-vng.sh` 在 guest 内编译 `test-step22-cache-io.c`，创建独立 loop、
data-dir 和 daemon.log，并显式传 `cache_device`、`cache_namespace`。它覆盖：

- 1 MiB+123 B 文件从 READ_DATA miss 填充后，对齐连续 block 直达 pinned pages；
- `file_offset=1` + `user_shift=1` + 超过 EOF 的请求，验证 head/tail fallback 与 EOF；
- 停止 daemon 后仍可读取持久化 hit；
- 同一 cache/namespace/workload 以 `cache_direct_io=0/1` 重载做 A/B；
- sysfs 计数证明 direct 与 buffered 分支均实际执行，umount <1 s。

```bash
make -C kestrelfs
cargo build --release --manifest-path daemon/Cargo.toml
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step22-cache-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step20-cache-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step19-cache-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step15-gc-vng.sh"
```

2026-09-14 自检通过：新用例观察到 `direct_blocks=511`、`copied_blocks=3`，
daemon 停止 hit 成功，最终复跑 umount 69 ms，`STEP22_CACHE_HIT_PASS`。1 MiB × 64
粗测：copy 3.88 s / 16.51 MiB/s，direct 0.54 s / 117.65 MiB/s（TCG+loop，仅作
路径对比，不把倍数作为验收门槛）。Step 20/21 回归 umount 57 ms，Step 19 为
88 ms，Step 15 为 87 ms，均输出对应 PASS。

### 7.17 Phase 4 Step 23 block-LRU eviction 验证

`test-step23-eviction-vng.sh` 只在 vng guest 内创建 16 MiB loop，并以
`cache_size_mib=3` 得到 2 MiB metadata + 256 个 4 KiB data slot。1 MiB 文件 A
填满 cache 后再命中 A[0]，随后读取 128 KiB 文件 B，必须观察到恰好 32 次 eviction；
停 daemon 后 B、A[0]、A 尾块仍命中，A[1] 必须 miss。模块重载后还要恢复 256 个
entry 并重复 hit/miss 边界。

```bash
make -C kestrelfs
cargo build --release --manifest-path daemon/Cargo.toml
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step23-eviction-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step22-cache-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step20-cache-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step19-cache-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step15-gc-vng.sh"
```

2026-09-14 自检通过：`cache_evictions=32`、`restored 256 cache index entries`、
`STEP23_EVICTION_PASS`，最终复跑 umount 67 ms。同期 Step 22 输出
`STEP22_CACHE_HIT_PASS`（umount 67 ms），Step 20/21 输出
`STEP20_CACHE_PASS` / `STEP21_NAMESPACE_PASS`（77 ms），Step 19 输出
`STEP19_CACHE_PASS`（89 ms），Step 15 输出 `VNG_GC_PASS`（91 ms）。

### 7.18 Phase 4 Step 24 data/index checksum 验证

`test-step24-checksum-vng.sh` 在 vng guest 的 loop 上填充两个 4 KiB entry，再用
raw block write 分别损坏 data slot。完整对齐读取覆盖 pinned-page CRC，
`file_offset=1/user_shift=1` 覆盖 buffered CRC；daemon 停止时坏 entry 必须 miss，
另一个 entry 仍命中，恢复 daemon 后可从权威 ObjectStore 重填。随后分别破坏
index 字段、伪造 v2 version，要求模块加载 fail closed 且不迁移。

```bash
make -C kestrelfs
cargo build --release --manifest-path daemon/Cargo.toml
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step24-checksum-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step23-eviction-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step22-cache-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step20-cache-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step19-cache-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step15-gc-vng.sh"
```

2026-09-14 v3 最终格式自检通过：pinned 与 buffered 两段均观察到
`cache_checksum_failures=1`，损坏 entry 可退休并重填，坏 index 和旧 v2 均被拒绝，
输出 `STEP24_CHECKSUM_PASS`（umount 53 ms）。回归输出
`STEP23_EVICTION_PASS`（evictions=32、restored=256、68 ms）、
`STEP22_CACHE_HIT_PASS`（direct=511、copy=3、66 ms）、
`STEP20_CACHE_PASS` / `STEP21_NAMESPACE_PASS`（73 ms）、
`STEP19_CACHE_PASS`（format mount 90 ms）及 `VNG_GC_PASS`（91 ms）。

### 7.19 Phase 4 Step 25 CACHE-TXN 验证

`test-step25-cache-txn-vng.sh` 在 guest loop 上格式化 v4 并填充两个独立 entry，
再用 `test-step25-cache-txn.c` 离线构造合法 `PREPARED` journal，模拟 fill 在 index
落盘后、invalidate 在 index 清零前、evict 在 index 清零后的 crash point。重载
必须把目标 slot 恢复为 miss，未涉及 entry 仍命中；另外破坏 journal CRC、回退
version=v3、破坏 superblock CRC，均须 fail closed。

```bash
make -C kestrelfs
cargo build --release --manifest-path daemon/Cargo.toml
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step25-cache-txn-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step24-checksum-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step23-eviction-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step22-cache-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step20-cache-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step19-cache-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step15-gc-vng.sh"
```

2026-09-14 工作树自检输出 `STEP25_FILL_RECOVERY_PASS`、
`STEP25_INVALIDATE_RECOVERY_PASS`、`STEP25_EVICT_RECOVERY_PASS`、
`STEP25_TORN_FAIL_CLOSED_PASS`、`STEP25_V3_REJECT_PASS`、
`STEP25_SUPER_CHECKSUM_PASS`、`STEP25_CACHE_TXN_PASS`（最终复跑 umount 62 ms）。同期回归
`STEP24_CHECKSUM_PASS`（53 ms）、`STEP23_EVICTION_PASS`（32 次 evict，71 ms）、
`STEP22_CACHE_HIT_PASS`（direct 89.28 MiB/s vs copy 16.96 MiB/s，66 ms）、
`STEP20_CACHE_PASS` / `STEP21_NAMESPACE_PASS`（63 ms）、`STEP19_CACHE_PASS` 及
`VNG_GC_PASS`（100 ms）全过。首次 Step 20 VM 在执行 guest 脚本前以 255 退出，
立即重跑通过；没有在物理机执行 insmod/mount/cache 测试。

### 7.20 Phase 4 Step 26 CACHE-ASYNC 验证

`test-step26-cache-async-vng.sh` 在 guest loop 上用同一 build、持久 cache 和
1 MiB × 16 次 × 8 reader workload 对比 `cache_parallel_reads=0/1`。串行模式必须
观察到 `cache_parallel_hit_peak=1`，并行模式必须至少为 2。随后两个 reader 同时
进入 pinned-page hit，rewrite 取得写侧前必须等待；reader 校验完整旧数据，rewrite
后校验完整新数据，停 daemon 再读仍须命中。脚本检查 dmesg 中没有 BUG、KASAN、
UAF、general protection fault 或 hung task。

```bash
make -C kestrelfs
cargo build --release --manifest-path daemon/Cargo.toml
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step26-cache-async-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step25-cache-txn-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step24-checksum-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step23-eviction-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step22-cache-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step20-cache-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step19-cache-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step15-gc-vng.sh
```

2026-09-15 工作树自检输出 `STEP26_PARALLEL_HIT_PASS peak=8`、
`STEP26_CONCURRENT_INVALIDATE_PASS`、`STEP26_CACHE_ASYNC_PASS`。同一 TCG+loop
workload 串行 877,968,083 ns、并行 430,707,430 ns，约 2.04×，最终 umount 53 ms；
该数字只说明并发路径生效，不代表真实 NVMe 性能。同期回归输出
`STEP25_CACHE_TXN_PASS`、`STEP24_CHECKSUM_PASS`、`STEP23_EVICTION_PASS`、
`STEP22_CACHE_HIT_PASS`、`STEP21_NAMESPACE_PASS` / `STEP20_CACHE_PASS`、
`STEP19_CACHE_PASS`、`VNG_GC_PASS`。Step 25 首次 vng 在 guest 脚本启动前偶发退出
255，重跑完整通过；另一次 Step 24 命令因脚本名输入错误只报 not found，使用正确
文件名后完整通过。全程未在物理机执行 insmod/mount/cache 操作。

### 7.21 Phase 4 Step 27 OPS-RECOVERY 验证

`tools/kestrelfs-cache-admin.c` 是离线工具：`inspect` 只读解析 v4 superblock、journal
和全部 index entry，并对块设备请求 exclusive open；`wipe` 只在模块卸载后对块设备
执行 2 MiB metadata 清零。
构建、检查与双确认 wipe 语法：

```bash
make -C tools
./tools/kestrelfs-cache-admin inspect /dev/loop0
KESTRELFS_CACHE_WIPE_CONFIRM=/dev/loop0 \
  ./tools/kestrelfs-cache-admin wipe /dev/loop0 --yes-really-wipe
```

`inspect` 返回 0 表示 clean v4 或 unformatted，2 表示 invalid，3 表示合法
PREPARED journal / recovery-required。wipe 环境变量必须精确等于 DEVICE 参数；缺少
任一确认、普通文件、过小或已被占用的块设备均拒绝。它不是 data 区安全擦除。

完整自动验证只允许在 vng guest+loop：

```bash
make -C kestrelfs
make -C tools
cargo build --release --manifest-path daemon/Cargo.toml
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step27-ops-recovery-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step26-cache-async-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step25-cache-txn-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step24-checksum-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step23-eviction-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step22-cache-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step20-cache-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step19-cache-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step15-gc-vng.sh
```

2026-09-15 工作树自检输出 `STEP27_INSPECT_PASS`、
`STEP27_WIPE_GUARD_PASS`、`STEP27_WIPE_PASS`、`STEP27_REFILL_PASS`、
`STEP27_OPS_RECOVERY_PASS`，最终复跑 umount 50 ms。inspect 覆盖 clean、合法 PREPARED、
坏 journal CRC 与 unformatted；wipe 后停 daemon 证明旧 entry 不可命中，恢复 daemon
后重新填充并再次停 daemon 命中。Step 26/25/24/23/22/21/20/19/15 回归均输出对应
PASS。Step 26 与 Step 19 各有一次 vng 在 guest 脚本输出前偶发退出 255，立即重跑
完整通过；全程未在物理机执行 insmod/mount/cache 操作。

### 7.22 Phase 4 Step 28 CACHE-VFS 验证

动态 regular file 已从 `.read` 切到 `.read_iter`。`kestrelfs_cache_read_iter()` 直接
消费 VFS `iov_iter`：完整、对齐、落在单个当前用户 iovec 段内的 block 保持
pinned-page BIO；跨段、partial/unaligned 或不能 pin 的范围使用同步块读加
`copy_to_iter`。READ_DATA miss 同样直接写入 iterator，ABI/format 不变。

专用脚本显式 `insmod`、创建 loop、传合法 namespace，并用独立 data-dir 保留
daemon.log：

```bash
make -C kestrelfs
make -C tools
cargo build --release --manifest-path daemon/Cargo.toml
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step28-cache-vfs-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step27-ops-recovery-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step26-cache-async-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step25-cache-txn-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step24-checksum-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step23-eviction-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step22-cache-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step20-cache-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step19-cache-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step15-gc-vng.sh
```

`test-step28-cache-vfs.c` 用真实 `preadv()` 验证页对齐多 iovec、单段跨页、非对齐
多段与 EOF 短读，并检查每个 iovec 前后 guard 未被覆盖。2026-09-15 工作树自检
输出 `STEP28_IOVEC_PASS`、`STEP28_UNALIGNED_PASS`、`STEP28_EOF_PASS`、
`STEP28_DAEMON_FREE_HIT_PASS direct_delta=3 copy_delta=5` 和
`STEP28_CACHE_VFS_PASS`，umount 83 ms。Step 27/26/25/24/23/22/21/20/19/15
均输出对应 PASS；Step 23 与 Step 20 各有一次 vng 在 guest 脚本输出前退出 255，
立即完整重跑通过。全程未在物理机执行 insmod/mount/cache 操作。

### 7.23 Phase 4 Step 29 CACHE-EVICT 验证

`test-step29-cache-evict-vng.sh` 在 guest loop 上用 `cache_size_mib=3` 建立 256 个
data slot，填满后把 A[0] 提升为 MRU，再以 16-block 文件 B 触发默认批量回收。
sysfs 必须观测 `cache_evictions=16`、`cache_eviction_batches=1`、
`cache_eviction_batch_slots=16`、`cache_eviction_index_writes=1`；停止 daemon 后 B、
A[0]/A 尾块仍 hit，而旧 LRU A[1] miss。正常 reload 后重复该边界并恢复 256 entries。

脚本还用扩展后的 `test-step25-cache-txn.c` 构造 16-victim PREPARED batch journal，
只预清 8 个 index 后模拟 crash。离线 admin 必须报告
`journal_batch_count=16` / `recovery-required`；模块重载必须输出 `count=16 to miss`，
清完整批且保留未涉及的 A[0] hit。

```bash
make -C kestrelfs
make -C tools
cargo build --release --manifest-path daemon/Cargo.toml
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step29-cache-evict-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step28-cache-vfs-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step27-ops-recovery-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step26-cache-async-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step25-cache-txn-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step24-checksum-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step23-eviction-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step22-cache-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step20-cache-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step19-cache-vng.sh
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec ./test-step15-gc-vng.sh
```

2026-09-15 工作树自检输出 `STEP29_BATCH_EVICTION_PASS victims=16 index_writes=1`、
`STEP29_MRU_PROTECTION_PASS`、`STEP29_RELOAD_PASS`、
`STEP29_BATCH_RECOVERY_PASS`、`STEP29_CACHE_EVICT_PASS`，最终 umount 253 ms。
Step 28/27/26/25/24/23/22/21/20/19/15 均输出对应 PASS；cargo test 为 137 passed，
clippy `-D warnings`、内核模块和 tools 均零警告。vng 有若干次 guest 脚本启动前
exit 255，另有一次 Step 25 daemon 重启时 `Transport endpoint is not connected`，
均立即完整重跑通过。全程未在物理机执行 insmod/mount/cache-device 操作。

### 7.24 Phase 4/控制面 Step 30 DIST-GC 验证

GC queue 是 MetaStore 持久状态的一部分。unlink、rename-overwrite、truncate 在移除
最后 slice 引用的同一次 mutation 中把 key 加入 `pending_garbage`：FileMetaStore 使用
同一次 tmp + fsync + rename；Step 31 起 RedisMetaStore 使用同一次 v2 Lua
revision-CAS 将候选写入 `gc` SET（Step 30 验收基线为单 key Lua CAS）。delete 成功后
才以第二次 metadata mutation 确认出队，因此进程在 metadata commit 后或 delete/ack
之间退出，重启都只会产生安全的幂等重试。

daemon 启动时立即执行一轮；运行期由原同步 event loop 的有限 poll timeout 驱动，
失败从 1 秒指数退避到 60 秒，新 IPC 活动会重置为 1 秒。这样不引入并发
FileMetaStore writer；日志输出每轮及进程累计 attempted/deleted/failures 计数。

本步没有修改 `kestrelfs/*.c`、IPC 或 mount 行为，因此按 §8 规则未启动 vng，且没有
执行任何物理机 insmod/mount/cache-device 操作。2026-09-15 自检：

```text
cargo test --manifest-path daemon/Cargo.toml
  141 passed; 0 failed
cargo clippy --manifest-path daemon/Cargo.toml --all-targets -- -D warnings
  Finished successfully; 0 warnings
make -C tools
  Nothing to be done for 'all'; 0 warnings
REDIS_URL=... cargo test redis_url_gated_full_semantics_and_restart -- --nocapture
  1 passed; 0 failed
S3_ENDPOINT=... cargo test s3_environment_gated -- --nocapture
  2 passed; 0 failed
```

新增测试覆盖 MemObjectStore 首次 delete 故障后保留队列并成功重试、共享 block 只在
最后引用移除后入队、FileMetaStore 模拟 commit 后 crash 并在重启时向 LocalFs
重放、ack 再重启仍为空，以及真实 Redis queue 重启/ack 与 MinIO DeleteObject。

### 7.25 Phase 4/控制面 Step 31 DIST-META 验证

Redis schema v2 使用六个固定结构：`control` HASH 记录 `schema_version=2`、revision
和 next inode id；`inodes`、`dirents`、`slices`、`symlinks` 为分记录 HASH；`gc`
为待删对象 SET。dirent field 是 `parent_inode:hex(UTF-8 name)`，slice field 是
`inode_id:chunk_index`。point reads 定向读取对应 field；复合 mutation 在 Redis
`MULTI/EXEC` 一致快照上复用 MemStore 语义，计算字段级 diff，再用单个 Lua 脚本做
revision-CAS 并原子更新全部相关结构。旧 `<prefix>:meta:v1`、未知 schema 或无 control
的半布局均拒绝启动，不自动迁移/wipe。

2026-09-15 Cursor 验收自检（与 Codex 汇报一致）：

```text
cargo test --manifest-path daemon/Cargo.toml
  144 passed; 0 failed
cargo clippy --manifest-path daemon/Cargo.toml --all-targets -- -D warnings
  Finished successfully; 0 warnings
REDIS_URL=... cargo test redis_url_gated_full_semantics_and_restart -- --nocapture
  1 passed; 0 failed
```

门控测试覆盖两个 store 并发 inode 分配、rename-overwrite 提交后的 old/new 原子结果、
truncate/unlink GC 入队、store 重建后的 namespace/symlink/GC queue 恢复、ack 以及旧
v1 schema fail-closed。此步只修改 daemon 与文档，未改 `kestrelfs/*.c`、tools、IPC
或 mount 行为，因此按规则未运行 make/vng；全程没有在物理机执行
insmod/mount/cache-device 操作。

### 7.26 Phase 4/控制面 Step 32 POSIX-CORE（硬链接）

本步择优完成优先级 A 硬链接：ABI v12 新增 `LINK_DATA`（parent u64@0、target
inode u64@8、name_len u16@16，名字在 16 KiB bounce，成功响应 nlink u32@0）。
Mem/File/Redis MetaStore 在同一次 mutation 中新增 dirent 并递增 inode nlink；unlink
与 rename-overwrite 只有移除最后引用时才删除 inode/slices、提交 GC key。内核 `.link`
使用同一 data IPC mutex，`iget_locked()` 保证同 superblock 的不同别名共享 VFS inode。

Cursor 验收自检（2026-09-15）：

```text
cargo test --manifest-path daemon/Cargo.toml
  151 passed; 0 failed
cargo clippy --manifest-path daemon/Cargo.toml --all-targets -- -D warnings
  Finished successfully; 0 warnings
make -C kestrelfs
  success; 0 warnings
vng --run --network user --rwdir "$PWD" --cwd "$PWD" \
  --exec ./test-step32-posix-core-vng.sh
  STEP32_LINK_CREATE_PASS
  STEP32_LINK_RESTART_PASS
  STEP32_LINK_SURVIVING_REFERENCE_PASS
  STEP32_LINK_FINAL_GC_PASS
  STEP32_LINK_RENAME_OVERWRITE_PASS
  STEP32_LINK_NEGATIVE_PASS
  STEP32_POSIX_CORE_PASS (umount_ms=25)
```

vng 用例仅在 guest 内创建 loop cache device，并显式 `insmod`；daemon 使用独立
`/tmp/kestrelfs-step32-$$` 且保留 `daemon.log`。未触碰宿主机模块、mount 或 zvol。

### 7.27 Phase 4/控制面 Step 33 POSIX-RENAME

`RENAME_DATA` payload offset 20..24 定义为 little-endian u32 flags，ABI v13 支持
`RENAME_NOREPLACE`。Mem/File/Redis 的目标存在检查均位于原子 metadata mutation 内；
失败返回 `EEXIST`，不改变 namespace/nlink/slice/GC queue。NOREPLACE 失败路径不触碰
目标 inode 的 cache invalidate。

Cursor 验收自检（2026-09-15）：

```text
cargo test --manifest-path daemon/Cargo.toml
  158 passed; 0 failed
cargo clippy --manifest-path daemon/Cargo.toml --all-targets -- -D warnings
  Finished successfully; 0 warnings
make -C kestrelfs
  success; 0 warnings
vng --run --network user --rwdir "$PWD" --cwd "$PWD" \
  --exec ./test-step33-posix-rename-vng.sh
  STEP33_NOREPLACE_SUCCESS_PASS
  STEP33_NOREPLACE_EEXIST_ATOMIC_PASS
  STEP33_NOREPLACE_HARDLINK_VFS_EEXIST_NOOP_PASS
  STEP33_UNSUPPORTED_FLAGS_PASS
  STEP33_NOREPLACE_RESTART_PASS
  STEP33_POSIX_RENAME_PASS (umount_ms=26)
```

Linux VFS 在 filesystem `.rename` 前执行 NOREPLACE 的目标存在检查，因此同 inode
硬链接别名的 `renameat2(..., RENAME_NOREPLACE)` syscall 也返回 `EEXIST`，且不会进入
KestrelFS/daemon；vng 验证两个别名与 nlink 不变。MetaStore/daemon 内部仍按要求将
同 inode 两名处理为成功 no-op。测试全在 vng guest + loop，显式 `insmod`、独立
data_dir 并保留 daemon.log；未触碰宿主机模块、mount 或 zvol。

### 7.28 Phase 4/控制面 Step 34 POSIX-ATTR

复用 ABI v13 的 create/mkdir mode 与 LOOKUP/CREATE/GETATTR attribute 字段，cache
format 仍为 v4。MetaStore 屏蔽调用者的文件类型位、保留 `0o7777`，并由操作强制
regular file/directory 类型。目录 nlink 的持久不变量为
`2 + immediate_subdirectory_count`：mkdir/rmdir、同目录 rename 覆盖空目录及跨目录
移动/覆盖目录在同一次 MemStore mutation 中预计算后更新；FileMetaStore 随 JSON
快照落盘，Redis v2 随 Lua revision-CAS patch 原子提交。普通文件创建/硬链接不改变
父目录 nlink。

内核 create 使用 daemon 返回的完整 mode；mkdir/rmdir/rename 同步维护当前 VFS inode
nlink；目录 `.getattr` 通过已有 `OP_GETATTR` 刷新 MetaStore 权威 mode/nlink，因此
root 在 daemon 重启和重新挂载后也能恢复正确计数。

Cursor 验收自检（2026-09-15）：

```text
cargo test --manifest-path daemon/Cargo.toml
  162 passed; 0 failed
cargo clippy --manifest-path daemon/Cargo.toml --all-targets -- -D warnings
  Finished successfully; 0 warnings
make -C kestrelfs
  success; 0 warnings
vng --run --network user --rwdir "$PWD" --cwd "$PWD" \
  --exec ./test-step34-posix-attr-vng.sh
  STEP34_FILE_MODE_PASS
  STEP34_MKDIR_MODE_NLINK_PASS
  STEP34_RENAME_DIR_NLINK_PASS
  STEP34_ATTR_RESTART_PASS
  STEP34_DIR_NLINK_CLEANUP_PASS
  STEP34_POSIX_ATTR_PASS (umount_ms=25)
```

Step 34 脚本显式 `insmod`，仅在 vng guest 创建 loop，使用独立
`/tmp/kestrelfs-step34-$$` 并将 daemon 输出保留到 daemon.log；未触碰宿主机模块、
mount 或 zvol。尚未实现 chmod/chown/时间属性 mutation；open-unlink、
EXCHANGE/WHITEOUT 等范围也未扩大。

---

## 8. 路线图（未做）

按 Cursor 既定策略的推荐优先级：

| 优先级 | 内容 | 说明 |
|---|---|---|
| 1 | **Step 35 CACHE-COHERENCE** | 多节点 / 远端 mutation 的本地 cache 失效；见 §8 |
| 2 | 其余 POSIX | open-unlink、EXCHANGE/WHITEOUT、chmod… |
| 3 | 其它 | 须 Cursor 在 remaining-capabilities §6 明示 |

> **⚠️ 明确**：规划与 Codex 提示词以 `docs/remaining-capabilities.md` 为准；本文件只保留已验收事实摘要。未下发新提示词前，不扩大范围。

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

1. **每次新会话第一步**：读本 `HANDOFF.md` + `docs/remaining-capabilities.md`（尤其 §8 当前提示词）+ `git log --oneline -20` + 相关源码。不要假设有任何前序对话上下文。
2. **只实现 §8 提示词范围**：提示词外的问题先问 Cursor 或跳过，不要自行扩大范围。完成后把汇报追加到 remaining-capabilities §9，状态改为 `REVIEW`。
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

---

## 10. 交接检查清单

- [x] Step 34 POSIX-ATTR（mode + 目录 nlink）已由 Cursor 验收并提交
- [x] IPC ABI = 13；cache format = v4
- [x] Step 8–33 + Step 34 已验收状态已写清
- [x] 下一步明确：Step 35 CACHE-COHERENCE（`docs/remaining-capabilities.md` §8）
- [x] README 保持中文
- [x] 测试约束：cache/mount 只在 vng+loop；daemon 日志写 `"$data_dir/daemon.log"`（§7.3 / §8 / §9.10）

---

## 11. 文档债务

以下文档与代码不一致，后续应更新（优先级低于功能开发）：

| 文件 | 问题 | 代码实际值 |
|---|---|---|
| `README.md` 细节 | 未逐一列举全部 DATA opcode | 完整 opcode 表见 `HANDOFF.md` / `kestrelfs_ipc.h` |
