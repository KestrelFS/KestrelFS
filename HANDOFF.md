# KestrelFS 研发交接文档（HANDOFF）

> **最后更新**：Step 49 CACHE-WRITE 已由 Cursor 验收并纳入本提交（IPC ABI v21、cache format v4）。下一步双包 Step 50 见 `docs/remaining-capabilities.md` §8（后续每步约 2× 体量）。
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
| 当前阶段 | Step 49 write-through 已验收；下一步 Step 50 = 可写 MAP_SHARED + WHITEOUT（双包，见 remaining-capabilities §8） |

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
- **用户态 daemon** `kestrelfs-daemon`：Tokio 异步运行时。`poll()` 驱动事件循环，逐条处理 REQ 事件，批量推回 RESP。MetaStore 管理元数据（inode/dirent/slice）及待删除对象队列，ObjectStore 管理块数据；Step 30 在启动及运行中重试幂等删除；Step 31 把 Redis metadata 拆为 v2 分记录 HASH/SET，并以 Lua revision-CAS 原子提交复合 mutation；Step 42 实现为每个 revision 附加有界 dirty-inode 日志，正常变化按 inode 批量失效，历史不可用或 probe 失败时保守全失效。Step 36 可持久保留 nlink=0 orphan，并在最后 close 后原子进入 GC；Step 37/39 让 Mem/File/Redis 持久更新 inode mode/uid/gid；Step 40 扩展到显式 atime/mtime。
- **数据模型**（JuiceFS-like 分层）：File → Chunk（64 MiB 固定窗口）→ Slice（变长写记录，COW 语义）→ Block（4 MiB 物理对象，存于 ObjectStore）。
- **NVMe 缓存边界**：缓存由内核拥有；v4 superblock 持久化 32-byte namespace SHA-256 identity 并由 CRC32 保护，指定 cache_device 时必须传 64-hex `cache_namespace`，不匹配则在恢复索引前 fail closed。Step 22–26 落地最多 128 KiB pinned-page BIO、block-LRU、CRC32、单页 intent journal 和 rwsem 并行 hit；Step 27 提供离线 inspect/双确认 metadata wipe。Step 28 把动态 regular file 切到 `read_iter`，cache hit 和 READ_DATA miss 直接消费 `iov_iter`；Step 45 已把普通读改经 filemap `read_folio`/`readahead`；Step 46 已落地有限文件 mmap。Step 48 已落地 hit BIO 的异步 completion，同 inode 冷 folio 可并发提交。Step 49 已落地：普通 buffered write 经 aops dirty folio/writepages 写回现有 WRITE_DATA，返回前同步等待并保留 clean filemap 页；仍无可写 MAP_SHARED 或延迟写缓存。Step 29 在 v4 journal reserved 中记录最多 64 个 batch victim（默认 16 且至多总槽位 1/16），按 index page 合并清零，提交后才允许 slot 复用；LRU 尾部近期热点不进入小批次。Step 42 的 ABI v20 ioctl 可在一次写侧临界区退休最多 64 个 inode，仍保留全 cache fail-closed 回退。正常 insmod/mount 路径仍不会自动 wipe/迁移。尚无完整 splice、用户态异步读接口或生产级多节点 lease/pubsub。禁止把普通文件（包括 ZFS dataset 中的文件）当 cache 设备。详细设计见 `docs/phase4-nvme-cache.md`。

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
│       ├── gc_worker.rs         # 有界异步 ObjectStore GC delete worker
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
├── test-step35-cache-coherence-vng.sh # Step 35 Redis revision→全 cache 失效回归
├── test-step36-posix-lifecycle-vng.sh # Step 36 open-unlink/cache/last-close GC 回归
├── test-step36-posix-lifecycle.c # Step 36 fd 生命周期测试助手
├── test-step37-posix-chmod-vng.sh # Step 37 文件/目录 chmod、重启及 orphan 回归
├── test-step37-posix-chmod.c # Step 37 open-unlink fchmod 测试助手
├── test-step38-posix-exchange-vng.sh # Step 38 原子 EXCHANGE/失败原子性/重启回归
├── test-step39-posix-chown-vng.sh # Step 39 文件/目录 chown、重启及 orphan 回归
├── test-step39-posix-chown.c # Step 39 open-unlink fchown 测试助手
├── test-step40-posix-utimes-vng.sh # Step 40 文件/目录时间、重启及 orphan 回归
├── test-step40-posix-utimes.c # Step 40 open-unlink futimens 测试助手
├── test-step45-kernel-aops-vng.sh # Step 45 folio/pagecache 与写后失效回归
├── test-step46-kernel-mmap-vng.sh # Step 46 文件 mmap/失效/共享写拒绝回归
├── test-step46-kernel-mmap.c # Step 46 mmap/COW/截断测试助手
├── test-step47-kernel-locks-vng.sh # Step 47 本地文件锁/进程退出回归
├── test-step47-kernel-locks.c # Step 47 flock/POSIX/OFD 测试助手
├── test-step48-cache-async-vng.sh # Step 48 异步 hit BIO/冷 folio/失效回归
├── test-step48-cache-async.c # Step 48 同 inode 并发读测试助手
├── test-step49-cache-write-vng.sh # Step 49 folio writeback/fsync/失败恢复回归
├── test-step49-cache-write.c # Step 49 写回/重启/错误测试助手
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
| **Phase 4/控制面 Step 35** | **Redis durable revision 轮询 + ABI v14 全 cache 持久失效** | **14** | **✅ 已验收** |
| **Phase 4/控制面 Step 36** | **open-unlink：显式 open 计数、nlink=0 orphan、last-close GC** | **15** | **✅ 已验收** |
| **Phase 4/控制面 Step 37** | **文件/目录持久 chmod；ABI v16 `OP_SETATTR`；orphan fchmod** | **16** | **✅ 已验收** |
| **Phase 4/控制面 Step 38** | **原子 `RENAME_EXCHANGE`；文件/目录/混合类型交换；目录 nlink** | **17** | **✅ 已验收** |
| **Phase 4/控制面 Step 39** | **文件/目录持久 chown；原子 mode/uid/gid setattr；orphan fchown** | **18** | **✅ 已验收** |
| **Phase 4/控制面 Step 40** | **显式 atime/mtime；文件/目录重启恢复；orphan futimens** | **19** | **✅ 已验收** |
| **Phase 4/数据面 Step 41** | **有界 GC delete worker + ObjectStore 长度完整性** | **19（未变）** | **✅ 已验收** |
| **Phase 4/控制面 Step 42** | **Redis revision 有界 dirty-inode 日志 + 批量 inode cache 失效 + 全量回退** | **20** | **✅ 已验收** |
| **Phase 4/内核 Step 43** | **普通文件 write_iter + write/writev/pwritev 统一 iov_iter 写路径** | **20（未变）** | **✅ 已验收** |
| **Phase 4/内核 Step 44** | **文件 fsync/fdatasync 与挂载 syncfs 后端耐久屏障** | **21** | **✅ 已验收** |
| **Phase 4/内核 Step 45** | **普通文件读侧 folio page cache/readahead；同步写后清页** | **21（未变）** | **✅ 已验收** |
| **Phase 4/内核 Step 46** | **文件只读共享/私有 mmap；映射失效与共享写拒绝** | **21（未变）** | **✅ 已验收** |
| **Phase 4/内核 Step 47** | **本地 flock/POSIX/OFD 文件锁与进程退出清理** | **21（未变）** | **✅ 已验收** |
| **Phase 4/内核 Step 48** | **cache hit 异步 BIO completion 与冷 folio 并发命中** | **21（未变）** | **✅ 已验收** |
| **Phase 4/内核 Step 49** | **普通文件 dirty folio + writepages/WRITE_DATA 的最小 write-through** | **21（未变）** | **✅ 已验收** |

Cursor 对照代码、151 tests、Redis 门控测与 `STEP32_POSIX_CORE_PASS` 确认 Step 32 已验收。
硬链接持久 nlink + 末引用 GC；`iget_locked` 同挂载别名共享 VFS inode。ABI **v12**；format **v4**。

Cursor 对照代码、158 tests 与 `STEP33_POSIX_RENAME_PASS` 确认 Step 33 已验收。
`RENAME_DATA` payload flags 支持原子 `RENAME_NOREPLACE`；EXCHANGE/WHITEOUT 仍拒绝。ABI **v13**；format **v4**。

Cursor 对照代码、162 tests 与 `STEP34_POSIX_ATTR_PASS` 确认 Step 34 已验收。
create/mkdir 持久化 `0o7777` 权限位；目录 nlink=`2+子目录数`。ABI **v13**；format **v4**。

Cursor 对照代码、164 tests 与 `STEP35_CACHE_COHERENCE_PASS` 确认 Step 35 已验收。
Redis daemon 轮询 durable revision，经 ABI v14 ioctl 做 v4-journal 全 cache 失效。ABI **v14**；format **v4**。

Cursor 对照代码、168 tests 与 `STEP36_POSIX_LIFECYCLE_PASS` 确认 Step 36 已验收。
显式 open 计数 + nlink=0 orphan + `FINALIZE_ORPHAN`；未恢复 iget5。ABI **v15**；format **v4**。

Cursor 对照代码、172 tests 与 `STEP37_POSIX_CHMOD_PASS` 确认 Step 37 已验收。
ABI v16 `OP_SETATTR` 持久化 mode；uid/gid/时间戳仍 `EOPNOTSUPP`。ABI **v16**；format **v4**。

Cursor 对照代码、179 tests 与 `STEP38_POSIX_EXCHANGE_PASS` 确认 Step 38 已验收。
ABI v17 原子 `RENAME_EXCHANGE`；与 NOREPLACE 互斥；`WHITEOUT` 仍拒绝。ABI **v17**；format **v4**。

Cursor 对照代码、181 tests 与 `STEP39_POSIX_CHOWN_PASS` 确认 Step 39 已验收。
ABI v18 持久 uid/gid；可与 MODE 同事务；orphan fchown。ABI **v18**；format **v4**。

Cursor 对照代码、185 tests 与 `STEP40_POSIX_UTIMES_PASS` 确认 Step 40 已验收。
ABI v19 秒级 atime/mtime + `GETATTR_TIMES`；basic/time layout 互斥。ABI **v19**；format **v4**。

Cursor 对照代码与 191 tests 确认 Step 41 已验收。
有界 GC delete worker + ObjectStore 长度完整性；IPC ABI **v19** 未变；format **v4**。

Cursor 对照代码、193 tests 与 `STEP42_COHERENCE_FINE_PASS` 确认 Step 42 已验收。
ABI v20 批量 inode 失效 + Redis dirty log；失败回退全量。ABI **v20**；format **v4**。

Cursor 对照代码、193 tests 与 `STEP43_WRITE_ITER_PASS` 确认 Step 43 已验收。
`.write_iter` 统一 write/writev/pwritev；ABI **v20** 未变；format **v4**。

Cursor 对照代码、195 tests 与 `STEP44_KERNEL_FSYNC_PASS` 确认 Step 44 已验收。
ABI v21 `OP_FSYNC`/`OP_SYNC_FS`；File+LocalFs 真 fsync。ABI **v21**；format **v4**。

Cursor 对照代码、195 tests 与 `STEP45_KERNEL_AOPS_PASS` 确认 Step 45 已验收。
策略 A：`generic_file_read_iter` + `read_folio`/`readahead`；写后清页；Redis page-cache epoch 惰性失效。ABI **v21** 未变；format **v4**。

Cursor 对照代码、195 tests 与 `STEP46_KERNEL_MMAP_PASS` 确认 Step 46 已验收。
`generic_file_mmap` + 自定义 fault；MAP_PRIVATE/只读 SHARED；可写 SHARED `EOPNOTSUPP`。ABI **v21** 未变；format **v4**。

Cursor 对照代码、195 tests 与 `STEP47_KERNEL_LOCKS_PASS` 确认 Step 47 已验收。
本地 flock/POSIX/OFD advisory 锁；锁类独立；无 daemon IPC。ABI **v21** 未变；format **v4**。

Cursor 对照代码、195 tests 与 `STEP48_KERNEL_CACHE_ASYNC_PASS` 确认 Step 48 已验收。
hit BIO 异步 completion；冷 folio 命中可并发；`cache_async_hit_peak` 可观测。ABI **v21** 未变；format **v4**。

Cursor 对照代码、195 tests 与 `STEP49_CACHE_WRITE_PASS` 确认 Step 49 已验收。
write-through aops；fsync 先写回再 `OP_FSYNC`。ABI **v21** 未变；format **v4**。

下一步：**Step 50**（可写 MAP_SHARED + `RENAME_WHITEOUT` 双包，约 2× 体量），提示词在 `docs/remaining-capabilities.md` §8。
验收前不视为已完成事实。

> **当前 ABI**：`KESTRELFS_ABI_VERSION = 21`（含 `FSYNC` / `SYNC_FS`）

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

**`KESTRELFS_ABI_VERSION = 21`**（内核 `kestrelfs_ipc.h` 与 Rust `abi.rs` 一致）

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
14. Phase 4/控制面 Step 35：新增 daemon→kernel `KESTRELFS_IOC_INVALIDATE_CACHE_ALL`；共享内存与 opcode 布局未变
15. Phase 4/控制面 Step 36：UNLINK_DATA/RENAME_DATA 增加延迟回收语义；新增 `FINALIZE_ORPHAN`
16. Phase 4/控制面 Step 37：新增 `SETATTR`；当前 valid mask 仅支持 MODE，保留类型位并替换 `0o7777`
17. Phase 4/控制面 Step 38：`RENAME_DATA` 接受与 NOREPLACE 互斥的 `RENAME_EXCHANGE`；payload/共享内存布局不变
18. Phase 4/控制面 Step 39：`SETATTR` valid 扩展 UID/GID；请求新增 uid@16/gid@20，响应返回 mode/uid/gid@0/4/8
19. Phase 4/控制面 Step 40：`SETATTR` 增加互斥的 ATIME/MTIME 秒级 union layout；新增 `GETATTR_TIMES` 用于 lookup 后时间重建
20. Phase 4/控制面 Step 42：新增 daemon→kernel `KESTRELFS_IOC_INVALIDATE_CACHE_INODES`，固定 520-byte 参数最多携带 64 个 inode；共享内存/opcode 不变
21. Phase 4/内核 Step 44：新增 `OP_FSYNC` / `OP_SYNC_FS` 耐久屏障 opcode

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
| 13 | `OP_RENAME_DATA` | 从 bounce 读取 old/new name；payload flags 支持 `RENAME_NOREPLACE` / `RENAME_EXCHANGE`（互斥） | 9（v13/v17 扩展 flags） |
| 14 | `OP_LOOKUP_DATA` | 从 bounce buffer 读取名字并查找 | 10 |
| 15 | `OP_CREATE_DATA` | 从 bounce buffer 读取名字并创建文件 | 10 |
| 16 | `OP_MKDIR_DATA` | 从 bounce buffer 读取名字并创建目录 | 10 |
| 17 | `OP_UNLINK_DATA` | 从 bounce buffer 读取名字并删除文件或空目录 | 10 |
| 18 | `OP_READDIR_DATA` | 通过 bounce buffer 批量返回变长目录条目 | 10 |
| 19 | `OP_SYMLINK_DATA` | bounce 中依次传输 link name 与 target，创建符号链接 | 11 |
| 20 | `OP_READLINK_DATA` | daemon 将符号链接 target 返回到 bounce buffer | 11 |
| 21 | `OP_LINK_DATA` | 为现有非目录 inode 创建 bounce 长名硬链接，返回更新后的 nlink | 12 |
| 22 | `OP_FINALIZE_ORPHAN` | last close 后回收 nlink=0 inode，并把对象加入 durable GC | 15 |
| 23 | `OP_SETATTR` | 持久更新 inode；v19 支持 basic 或互斥的秒级 atime/mtime layout | 18（v19 扩展时间） |
| 24 | `OP_GETATTR_TIMES` | 获取持久 atime/mtime，供 VFS inode 重建 | 19 |
| 25 | `OP_FSYNC` | 同步 inode 全部引用对象与元数据；payload inode_id u64@0 | 21 |
| 26 | `OP_SYNC_FS` | 挂载级引用对象与元数据同步；payload 全零 | 21 |
| 64 | `OP_RESULT_OK` | 响应：成功 | 1 |
| 65 | `OP_RESULT_ERROR` | 响应：失败（error_code 携带负 errno） | 1 |

### 5.5 存储现状

| 层 | 内存模式 (`--memory`) | 默认持久化模式 | 可选远端后端 |
|---|---|---|---|
| 元数据 (MetaStore) | `MemStore`（纯 HashMap，重启丢失） | `FileMetaStore`（全量 JSON 到 `{data_dir}/meta.json`） | `RedisMetaStore`（`--meta redis://...`；v2 control/inode/dirent/slice/symlink/GC/dirty HASH/SET + Lua revision-CAS） |
| 块数据 (ObjectStore) | `MemObjectStore`（纯 HashMap，支持幂等 delete） | `LocalFsObjectStore`（`{data_dir}/{slice_uuid}/{block_idx}`） | `S3ObjectStore`（`--objects s3://bucket/prefix`；AWS S3 或 MinIO） |

**CLI 参数**（`daemon/src/main.rs`）：
- `--data-dir <PATH>`：持久化目录（默认 `./.kestrelfs-data`），meta.json 和块数据均存于此
- `--memory`：纯内存模式（元数据 + 块数据均不持久化）
- `--meta <REDIS_URL>`：将 metadata 切到 Redis，例如 `redis://127.0.0.1:6379/0`；与 `--memory` 冲突
- `--redis-prefix <PREFIX>`：Redis key 命名空间，默认 `kestrelfs`，v2 keys 为 `<PREFIX>:meta:v2:{control,inodes,dirents,slices,symlinks,gc,dirty}`
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
| 5 | **按 ino 使用基础 iget，未恢复历史 iget5 自定义方案** | Step 32 为保证硬链接别名共享 VFS `i_nlink`，改用标准 `iget_locked(sb, ino)`；Step 36 的 open-handle 生命周期也只放在该标准 inode 的 `i_private`，没有恢复曾导致卸载死循环的 `iget5_locked()` 自定义 test/set。 | `kestrelfs/inode.c` `kestrelfs_get_inode()` |
| 6 | **inode/open identity 仍是单挂载、daemon inode id** | 同一 superblock 内的硬链接别名复用 inode 与 open 计数；不同 mount 各自维护 VFS inode/open 计数，MetaStore 仍是持久属性权威。Step 36 跨 mount 的最终 unlink 判断尚不具备全局 open 计数。 | 同上、`daemon/src/meta.rs` |
| 7 | **evict_inode 禁止发 IPC** | `kestrelfs_evict_inode()` 只做 `truncate_inode_pages_final` + `clear_inode`，绝不发 IPC（daemon 可能已关闭，会死锁）。 | `kestrelfs/inode.c` |
| 8 | **JSON 全量落盘** | `FileMetaStore` 每次写操作后将整个元数据状态序列化为 JSON 写盘。简单但低效；inode 数量大时性能差。 | `daemon/src/meta_persist.rs` `sync_to_disk()` |
| 9 | **meta.json 损坏 → 数据丢失** | 若 `meta.json` 反序列化失败（JSON 损坏），daemon 回退到全新 `MemStore::new()`（仅含 root + remote.txt + writable.dat），之前用户创建的文件元数据全部丢失。块数据仍在磁盘但无法访问。 | `daemon/src/meta_persist.rs` `FileMetaStore::new()` |
| 10 | **rename flags 尚无 WHITEOUT** | Step 33 支持原子 `RENAME_NOREPLACE`；Step 38 支持与其互斥的 `RENAME_EXCHANGE`，要求两端存在，并按 Linux 语义允许目录与非目录交换。`RENAME_WHITEOUT`、未知位及 NOREPLACE|EXCHANGE 返回 `-EINVAL`。 | `kestrelfs/dir.c`、`daemon/src/meta.rs` |
| 11 | **目录 nlink 只表达直接子目录数** | Step 34 按 POSIX 常见不变量持久化 `2 + immediate_subdirectory_count`，覆盖 mkdir/rmdir 与目录 rename；它不是递归后代计数。不同 mount 的 VFS inode 仍各自刷新 MetaStore 权威值。 | `daemon/src/meta.rs`、`kestrelfs/dir.c` |
| 12 | **READDIR_DATA 每批受 16 KiB 限制** | daemon 按 inode 排序并在 bounce 中打包尽可能多的完整变长条目；大目录仍需分页 IPC，但不再固定每次只返回 1 条。 | `daemon/src/main.rs` `handle_readdir_data()` |
| 13 | **write-through aops 仍同步等待 daemon** | Step 49 已落地：普通 write/writev/pwritev 先经 `write_begin`/`write_end` 更新并弄脏 folio，`writepages` 以 4 KiB folio 经现有 16 KiB bounce `WRITE_DATA` 写回；write_iter 返回前等待完成，成功后保留 clean page cache。O_APPEND 仍由 `generic_write_checks()` 在 inode 锁下串行；没有延迟写缓存、可写共享 mmap 或异步 WRITE_DATA。 | `kestrelfs/file.c`、`kestrelfs/inode.c` |
| 14 | **时间属性为秒级显式持久化** | Step 40 持久化显式 atime/mtime；纳秒截断为 0，负 epoch 返回 `EOVERFLOW`，自动读 atime 与 ctime 不持久化。SIZE+显式时间/其它属性及 time+MODE/UID/GID 组合返回 `EOPNOTSUPP`；普通 truncate 随带的 VFS 隐式 mtime/ctime 由 TRUNCATE 处理。 | `daemon/src/meta.rs`、`kestrelfs/file.c`、`kestrelfs/dir.c` |
| 15 | **symlink target 当前要求 UTF-8 且 ≤4095 字节** | Linux 原生 symlink target 可为任意非 NUL 字节；当前 MetaStore 使用 `String`，ABI 解码拒绝非 UTF-8，target 上限为 4095 字节。悬空链接与相对链接均支持。 | `daemon/src/meta.rs`、`daemon/src/abi.rs` |
| 16 | **GC 引用确认是 O(全量 slice)** | 每次产生删除候选及每次读取待删队列时扫描所有剩余 slice 构建 block key 引用集合，正确处理共享 key，但 inode/slice 或积压队列很大时成本较高；后续可用引用计数优化。 | `daemon/src/meta.rs` `confirmed_garbage_keys()` / `pending_garbage()` |
| 17 | **open-unlink 离线 final-close 只保证不误删** | Step 36 用 mount-local open 计数和持久 nlink=0 orphan 延迟回收；daemon 重启时 open fd 仍可继续。若最终 close 时 daemon/IPC 不可用，当前保留 orphan，不在 `evict_inode` 发 IPC，因此可能安全泄漏且尚无自动 sweep。 | `kestrelfs/file.c`、`daemon/src/meta.rs` |
| 18 | **Redis v2 写放大已降低，mutation 读放大仍在** | Step 31 把 metadata 拆为固定 HASH/SET，`lookup/getattr/read_slices/readlink` 定向读取，mutation 只写发生变化的 fields；但为复用 MemStore 的完整 rename/truncate/引用确认语义，每次 mutation 仍一致读取各聚合 HASH 并在客户端计算 diff，冲突最多重试 64 次。readdir 与 GC 引用确认也仍有聚合扫描。 | `daemon/src/meta_redis.rs` |
| 19 | **远端 metadata/object 必须成对配置** | Step 17 已可用 Redis + S3 补齐共享数据面；若只启用 Redis 而仍用不同节点的 LocalFs，或只启用 S3 而各节点使用不同 FileMetaStore，仍会出现 metadata/object 视图不一致。 | `daemon/src/main.rs` 存储选择 |
| 20 | **Redis schema 异常/旧 v1 fail closed** | 与 FileMetaStore 的“损坏后重置”不同，v2 control/record 非法、版本未知、control 缺失但残留 v2 key 或旧 `<prefix>:meta:v1` 存在时 daemon 启动失败；运行中 schema/control 被删除或破坏时 metadata 操作返回 EIO。没有 v1 自动迁移或自动 wipe。 | `daemon/src/meta_redis.rs` |
| 21 | **Redis 连接仍是原型级** | 当前只接受 `redis://`（未启用 `rediss://` TLS），持有一条 multiplexed connection 且未加自动重连 manager；连接故障时请求返回 EIO，需恢复 Redis 后重启 daemon。URL 可能含凭据，因此启动日志不会打印 URL。 | `daemon/src/meta_redis.rs`、`daemon/src/main.rs` |
| 22 | **S3 GC 是持久队列 + at-least-once delete** | Step 41 用容量 32、每项最多 64 keys、delete 并发 4 的 worker 隔离慢 S3；背压/失败不丢 durable key，成功结果由串行 metadata 线程确认。多 daemon 仍可能重复处理同一 Redis `gc` SET，幂等 delete/revision-CAS ack 可容忍；尚无跨 Redis/S3 事务、dead-letter 或运维限额。 | `daemon/src/gc_worker.rs`、`daemon/src/object_store_s3.rs`、`daemon/src/meta_redis.rs`、`daemon/src/main.rs` |
| 23 | **S3 原型不创建生产 bucket** | daemon 要求 bucket 已存在；只有设置 `S3_CREATE_BUCKET=1` 的门控测试会创建测试 bucket。自定义 endpoint 自动 force path-style；真实 AWS 默认使用 SDK endpoint/addressing。 | `daemon/src/object_store_s3.rs` |
| 24 | **NVMe cache hit 仍需同步等待调用结果** | Step 48 已实现 hit BIO 的异步提交/独立 completion，并让不同冷 folio 脱离 bounce 锁并发提交；但 VFS `read_folio` 仍同步等待自己的 completion，metadata/fill/journal 与 READ_DATA miss 仍同步。Step 49 的写入保留 clean filemap 页；无跨 iovec scatter-gather BIO、可写共享 mmap 或完整 splice；mutation 会等待慢 reader。 | `kestrelfs/file.c`、`kestrelfs/cache.c`、`docs/phase4-nvme-cache.md` |
| 25 | **cache v4 journal 正确但同步 flush 成本高** | 每个完整 4 KiB data block、32-byte index entry、superblock 和 journal 都有 CRC32。单页 intent journal 将 fill/invalidate/evict/坏块退休的半提交状态恢复为安全 miss；torn journal/superblock fail closed。metadata mutation 由 cache rwsem 写侧保证单事务，且每次 index mutation 新增 journal prepare/clear 两次同步写与 flush；仍无双 superblock/metadata 镜像，CRC32 也不是密码学保护。 | `kestrelfs/cache.c` |
| 26 | **cache namespace identity 依赖部署规范化** | Step 21 起 superblock 绑定 32-byte SHA-256 digest，当前 v4 继续沿用；缺失/非法/mismatch 均拒绝加载。内核不解析 data-dir/Redis/S3 配置，调用方必须对稳定、无凭据、规范化的 MetaStore + ObjectStore descriptor 求 SHA-256。旧 v1/v2/v3 不自动迁移；Step 27 工具只提供显式 metadata wipe。 | `kestrelfs/cache.c`、`docs/phase4-nvme-cache.md` |
| 27 | **远端 cache coherence 仍是最终一致原型** | Step 42 用最近 256 revision 的 durable dirty log 将正常变化收窄为最多 64 inode 的 ABI v20 批量失效；记录缺失/损坏、溢出、累计超限或 probe 失败仍全量 fail closed。提交到下一次 probe 前仍有短暂旧 hit 窗口，daemon 离线期间没有 lease，尚无 range/pubsub。 | `daemon/src/main.rs`、`daemon/src/meta_redis.rs`、`kestrelfs/cache.c` |
| 28 | **batch block-LRU 热度仍只在内存** | Step 29 默认一次退休 16 个 LRU victim（至多总槽位 1/16），用一份 journal 并按 index page 合并清零；连续 fill 可消费预回收槽位，MRU 尾部受到小批量保护。为避免破坏 hit 性能，不在每次访问持久化 recency；rmmod/insmod 后仍按 generation 恢复 insertion-order 近似。没有分区配额/租户热点隔离；victim 分散时仍需每个 index page 一次同步写，fill/invalidate 仍逐次 journal。 | `kestrelfs/cache.c`、`docs/phase4-nvme-cache.md` |
| 29 | **Step 27 wipe 不是安全擦除或自动修复** | 工具只清零并 fsync 前 2 MiB cache metadata，使旧 data slot 不再可寻址并允许重新 format；data 区字节仍可能由 raw 取证读到。wipe 要求模块卸载、目标为块设备、exclusive open、环境变量精确匹配设备路径及命令行旗标；不会修复单个 entry、自动迁移旧格式或修改权威 MetaStore/ObjectStore。 | `tools/kestrelfs-cache-admin.c` |
| 31 | **读侧 page cache 的远端失效偏保守** | Redis revision ioctl 推进全局 page-cache epoch：普通读下次访问惰性清页，Step 46 已映射 inode 由异步 worker 撤销 PTE/folio；inode-list 细粒度 NVMe 失效对 VFS page cache 仍退化为挂载级保守失效，避免 ioctl 线程等待 locked folio 与 daemon READ_DATA 死锁。一致性窗口包含原 ~100 ms probe 与 worker 调度。 | `kestrelfs/file.c`、`kestrelfs/chardev.c` |
| 32 | **mmap 仅支持读侧/私有 COW** | Step 46 已支持 MAP_PRIVATE（可 COW）与只读 MAP_SHARED；可写 MAP_SHARED 返回 `EOPNOTSUPP`，只读共享 VMA 清除 `VM_MAYWRITE` 防 `mprotect` 升级。远端 ioctl 后映射清理由异步 worker 完成，除原 ~100 ms probe 外另有 worker 调度窗口；fault 若先遇到待清理 epoch 会等待重试或 fail-closed SIGBUS，不保证生产级强一致。 | `kestrelfs/file.c` |
| 33 | **文件锁仅本地 advisory** | Step 47 已把 flock、POSIX 字节锁与 OFD 锁交给 Linux 本地锁管理器；同一挂载节点上的进程可协调，flock 与 POSIX/OFD 锁类彼此独立。不同挂载或节点不共享锁状态；无跨 daemon 分布式锁、远端 lease 或强制锁。 | `kestrelfs/file.c` |
| 30 | **fsync 的持久性受后端配置限制** | Step 44 fsync 与 fdatasync 同样同步对象和元数据；syncfs 检查所有引用且同步本地对象目录树。Mem 返回仅进程内成功；Redis 仅保证已 ACK 的 mutation 可见，崩溃耐久取决于 AOF/RDB 配置（RDB 不保证逐次 fsync 耐久；本步没有 WAIT/WAITAOF）；S3 以已完成 PUT ACK 为边界；没有分布式跨后端原子事务或目录 file op 的 fsync。 | `daemon/src/main.rs`、`daemon/src/object_store.rs`、`daemon/src/meta_persist.rs` |

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
cargo test                    # 单元测试 + 集成测试（Step 42 验收基线 193 个）
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

### 7.29 Phase 4/控制面 Step 35 CACHE-COHERENCE

选择 daemon 驱动的保守失效：Redis v2 `control.revision` 是每次 Lua mutation 都会
原子递增的 durable namespace 版本。Redis daemon 每 100 ms 读取该字段；revision
变化或 probe 失败时，通过 ABI v14 新增的
`KESTRELFS_IOC_INVALIDATE_CACHE_ALL` 请求内核退休所有本地 cache entry。daemon
启动时在连接 MetaStore 和进入服务循环前也执行一次全失效，防止离线期间发生远端
mutation 后复用旧持久索引。MemStore/FileMetaStore 返回 `None`，不启用轮询。

全失效与 hit/fill/invalidate/evict 共用 cache rwsem 写侧，并先推进 mutation epoch；
每条 entry 继续使用 v4 invalidate intent journal，提交后才从 hash/LRU/bitmap 释放。
若任一持久化退休失败，内核立即销毁其余内存索引并禁用本次模块生命周期的 cache，
宁可全部 miss 而不返回可能过期的数据。共享内存、opcode、payload 和 cache format
均未改变；仅 ioctl 合约令 IPC ABI v13 → v14。只读 sysfs 计数
`cache_coherence_invalidations` 记录成功的 daemon 全失效次数。

Cursor 验收自检（2026-09-15）：

```text
cargo test --manifest-path daemon/Cargo.toml
  164 passed; 0 failed
cargo clippy --manifest-path daemon/Cargo.toml --all-targets -- -D warnings
  Finished successfully; 0 warnings
make -C kestrelfs
  success; 0 warnings
vng --run --network user --rwdir "$PWD" --cwd "$PWD" \
  --exec "env REDIS_URL=redis://10.0.2.2:6379/15 ./test-step35-cache-coherence-vng.sh"
  STEP35_READER_CACHE_HIT_PASS hits_delta=1
  STEP35_REMOTE_REVISION_INVALIDATE_PASS
  STEP35_DAEMON_FREE_NEW_HIT_PASS
  STEP35_CACHE_COHERENCE_PASS (umount_ms=24)
```

Step 35 脚本只在 vng guest 创建 loop、显式 `insmod`、传合法 namespace，并使用
独立 `/tmp/kestrelfs-step35-$$` 与保留的 daemon.log。宿主机未执行模块、mount 或
cache-device 操作。已知限制：100 ms 最终一致窗口、整盘失效粗粒度、非 Redis 后端
不启用轮询。

### 7.30 Phase 4/控制面 Step 36 POSIX-LIFECYCLE

继续使用标准 `iget_locked(sb, ino)`；每个 regular inode 用 `lifecycle_lock +
open_handles` 串行 open/release 与最终 unlink/rename-overwrite。最终目录项消失但
仍有 fd 时，MetaStore 保留 `nlink=0` orphan 及全部 slice；最后 close 先失效 cache，
再经 opcode 22 `FINALIZE_ORPHAN` 原子移除 inode/slice 并进入 durable GC。ABI v15：
`UNLINK_DATA`/`RENAME_DATA` 增加 lifecycle defer 标志。未恢复 `iget5_locked`。

Cursor 验收自检（2026-09-15）：

```text
cargo test --manifest-path daemon/Cargo.toml
  168 passed; 0 failed
cargo clippy --manifest-path daemon/Cargo.toml --all-targets -- -D warnings
  Finished successfully; 0 warnings
make -C kestrelfs
  success; 0 warnings
vng --run --network user --rwdir "$PWD" --cwd "$PWD" \
  --exec ./test-step36-posix-lifecycle-vng.sh
  STEP36_OPEN_UNLINK_RETAIN_PASS
  STEP36_UNLINKED_CACHE_HIT_PASS
  STEP36_LAST_CLOSE_GC_PASS
  STEP36_POSIX_LIFECYCLE_PASS (umount_ms=27)
```

脚本仅在 vng guest + loop、显式 insmod、独立 data_dir/daemon.log。已知限制：
open 计数为单挂载；final-close 时 daemon/IPC 失败则保留 orphan（不误删），尚无
自动 orphan sweep。

### 7.31 Phase 4/控制面 Step 37 POSIX-CHMOD

ABI v16 新增 `OP_SETATTR`（inode@0、valid@8、mode@12）。仅 `SETATTR_MODE`；
MetaStore 保留类型位、原子替换 `0o7777`。内核 `.setattr` 经 `setattr_prepare`；
uid/gid/显式时间与 mode+size 组合返回 `EOPNOTSUPP`。orphan 在 final close 前可
`fchmod`。

Cursor 验收自检（2026-09-15）：

```text
cargo test --manifest-path daemon/Cargo.toml
  172 passed; 0 failed
cargo clippy --manifest-path daemon/Cargo.toml --all-targets -- -D warnings
  Finished successfully; 0 warnings
make -C kestrelfs
  success; 0 warnings
vng --run --network user --rwdir "$PWD" --cwd "$PWD" \
  --exec ./test-step37-posix-chmod-vng.sh
  STEP37_FILE_DIR_CHMOD_PASS
  STEP37_UNSUPPORTED_ATTRS_PASS
  STEP37_ORPHAN_CHMOD_PASS
  STEP37_CHMOD_RESTART_PASS
  STEP37_POSIX_CHMOD_PASS (umount_ms=20)
```

脚本仅在 vng guest + loop、显式 insmod、独立 data_dir/daemon.log。

### 7.32 Phase 4/控制面 Step 38 POSIX-EXCHANGE

复用 `RENAME_DATA` 的 flags@20，ABI v17 增加与 NOREPLACE 互斥的
`RENAME_EXCHANGE`。两端必须存在；MemStore 在同一写锁内交换两个 dirent，FileMetaStore
随同一次 JSON 原子替换持久化，RedisMetaStore 由同一 Lua revision-CAS 同时更新两个
dirent field。交换不删除 inode/slice、不产生 GC 或 lifecycle orphan；cache 按 inode
标识，故保留有效 entry。跨父目录混合类型交换按 Linux 语义允许，并对两个父目录
nlink 作对称调整；两个方向的目录祖先环路均在 mutation 前拒绝。

Cursor 验收自检（2026-09-16）：

```text
cargo test --manifest-path daemon/Cargo.toml
  179 passed; 0 failed
cargo clippy --manifest-path daemon/Cargo.toml --all-targets -- -D warnings
  Finished successfully; 0 warnings
make -C kestrelfs
  success; 0 warnings
REDIS_URL=... cargo test --manifest-path daemon/Cargo.toml \
  redis_url_gated_full_semantics_and_restart -- --nocapture
  1 passed; 0 failed
vng ... --exec ./test-step38-posix-exchange-vng.sh
  STEP38_FILE_EXCHANGE_PASS
  STEP38_CACHE_IDENTITY_PASS
  STEP38_DIRECTORY_EXCHANGE_PASS
  STEP38_MIXED_TYPE_EXCHANGE_PASS
  STEP38_HARDLINK_EXCHANGE_PASS
  STEP38_FAILURE_ATOMICITY_PASS
  STEP38_EXCHANGE_RESTART_PASS
  STEP38_POSIX_EXCHANGE_PASS (umount_ms=22)
```

Cursor 复跑确认上述 STEP38_* marker；Codex 另报告相关回归
`STEP33_POSIX_RENAME_PASS`、`STEP36_POSIX_LIFECYCLE_PASS`、`STEP37_POSIX_CHMOD_PASS`。
全部测试仅在 vng guest + loop 执行，脚本显式 `insmod`，使用独立 data_dir 并保留
daemon.log；未触碰宿主机模块、mount 或 zvol。`RENAME_WHITEOUT` 仍未实现。

### 7.33 Phase 4/控制面 Step 39 POSIX-CHOWN

ABI v18 扩展 `OP_SETATTR`：请求为 inode@0、valid@8、mode@12、uid@16、gid@20，
响应返回 authoritative mode/uid/gid@0/4/8。MODE/UID/GID 可在一个 MetaStore
mutation 中任意组合；MemStore 单写锁、FileMetaStore 单次 sync、RedisMetaStore 单次
Lua revision-CAS。内核 lookup/getattr 同步刷新 VFS inode uid/gid，retained orphan 在
final close 前支持 fchown。size 与属性组合、显式时间属性仍 fail closed；权限模型沿用
VFS root/capability 基础检查，不宣称完整 DAC/ACL/idmapped-mount 语义。

Cursor 验收自检（2026-09-16）：

```text
cargo test --manifest-path daemon/Cargo.toml
  181 passed; 0 failed
cargo clippy --manifest-path daemon/Cargo.toml --all-targets -- -D warnings
  Finished successfully; 0 warnings
make -C kestrelfs
  success; 0 warnings
REDIS_URL=... cargo test --manifest-path daemon/Cargo.toml \
  redis_url_gated_full_semantics_and_restart -- --nocapture
  1 passed; 0 failed
vng ... --exec ./test-step39-posix-chown-vng.sh
  STEP39_FILE_DIR_CHOWN_PASS
  STEP39_UNSUPPORTED_ATTRS_PASS
  STEP39_ORPHAN_FCHOWN_PASS
  STEP39_CHOWN_RESTART_PASS
  STEP39_POSIX_CHOWN_PASS (umount_ms=22)
vng regressions
  STEP37_POSIX_CHMOD_PASS (umount_ms=21)
  STEP36_POSIX_LIFECYCLE_PASS (umount_ms=25)
```

全部 mount/module 测试仅在 vng guest + loop 执行，脚本显式 `insmod`，使用独立
data_dir 并保留 daemon.log；未触碰宿主机模块、mount 或 zvol。

### 7.34 Phase 4/控制面 Step 40 POSIX-UTIMES

ABI v19 沿用 `OP_SETATTR`，增加与 MODE/UID/GID basic layout 互斥的时间 union：
inode@0、valid@8、atime Unix 秒@12、mtime Unix 秒@20、reserved@28。新增 opcode 24
`GETATTR_TIMES`，使成功 lookup 与目录 getattr 能以持久 atime/mtime 刷新 VFS inode。
MetaStore 以 u64 秒保存时间；MemStore 单写锁、FileMetaStore 单次 JSON sync、
RedisMetaStore 单次 Lua revision-CAS。纳秒截断为 0、负 epoch 返回 `EOVERFLOW`；旧记录
缺少 atime 时兼容读为 epoch 0。ctime 只更新当前 VFS inode，不持久化；自动读 atime
也不写回 MetaStore。SIZE+其它字段及 time+basic 组合 fail closed。

Cursor 验收自检（2026-09-16）：

```text
cargo test --manifest-path daemon/Cargo.toml
  185 passed; 0 failed
cargo clippy --manifest-path daemon/Cargo.toml --all-targets -- -D warnings
  Finished successfully; 0 warnings
make -C kestrelfs
  success; 0 warnings
REDIS_URL=... cargo test --manifest-path daemon/Cargo.toml \
  redis_url_gated_full_semantics_and_restart -- --nocapture
  1 passed; 0 failed
vng ... --exec ./test-step40-posix-utimes-vng.sh
  STEP40_FILE_DIR_UTIMES_PASS
  STEP40_ORPHAN_FUTIMENS_PASS
  STEP40_UTIMES_RESTART_PASS
  STEP40_POSIX_UTIMES_PASS (umount_ms=23)
vng regressions
  STEP39_POSIX_CHOWN_PASS (umount_ms=27)
  STEP37_POSIX_CHMOD_PASS (umount_ms=24)
```

全部 mount/module 测试仅在 vng guest + loop 执行，脚本显式 `insmod`，使用独立
data_dir 并保留 daemon.log；未触碰宿主机模块、mount 或 zvol。

### 7.35 Phase 4/数据面 Step 41 DIST-OBJECT

纯 daemon 改动，IPC ABI 保持 v19、cache format 保持 v4。GC delete 从串行 IPC
event loop 移到有界 worker：请求队列容量 32，每项最多 64 keys，同时最多 4 个
ObjectStore delete。队列满、任务失败或进程在 delete/ack 之间退出时，key 仍在
MetaStore durable GC queue；只有成功结果回到串行 metadata 线程后才确认出队，避免
FileMetaStore 并发写 `meta.json.tmp`。

ObjectStore 新增长度完整性接口。读 slice 时要求对象覆盖全部当前引用字节且不超过
4 MiB 物理块；truncate 后不可见的旧对象尾部允许保留但不会返回。S3 在收 body 前
校验响应 `Content-Length`，收完后再校验 header/body 与上述安全区间，不匹配返回
integrity error/EIO。multipart PUT 未实现：当前内核 bounce 单次写最多 16 KiB，现有
4 MiB Block 路径没有可测收益。

Cursor 验收自检（2026-09-16）：

```text
cargo test --manifest-path daemon/Cargo.toml
  191 passed; 0 failed
cargo clippy --manifest-path daemon/Cargo.toml --all-targets -- -D warnings
  Finished successfully; 0 warnings
```

本步未改 `kestrelfs/*.c` 或 mount 行为，按 §8 不运行 vng，也未触碰宿主机
insmod/mount/zvol。Codex 另报告真实 MinIO 门控 `2 passed`；见
`docs/remaining-capabilities.md` §9。

### 7.36 Phase 4/控制面 Step 42 COHERENCE-FINE

Redis v2 mutation 现在把排序去重的 dirty inode 与 revision-CAS、metadata diff 放在同一
Lua 事务，写入 `<prefix>:meta:v2:dirty`。单 revision 最多 64 inode，保留最近 256 个
revision。daemon 以本地成功 probe revision 为游标合并后续记录；这是多 daemon 可各自
消费的有界日志，不执行会令其它 reader 丢通知的全局清空。历史完整且累计不超过 64 时，
通过 ABI v20 的 520-byte `_IOW` 参数一次传入 inode ids；内核在 cache rwsem 写侧、同一
mutation epoch 下逐条用 v4 journal 退休匹配 entry。持久化失败会销毁内存索引并禁用
cache。记录缺失/损坏、overflow、gap 超过 256、累计超限或 probe 失败均回退
`INVALIDATE_CACHE_ALL`；共享内存/opcode 与 cache format v4 不变。

Cursor 验收自检（2026-09-16）：默认 Rust tests `193 passed`；clippy/make 干净。vng：
`1 passed`；clippy/make 结果见 `docs/remaining-capabilities.md` §9。专项 vng + loop 输出
`STEP42_UNCHANGED_INODE_HIT_PASS`、`STEP42_CHANGED_INODE_INVALIDATED_PASS`、
`STEP42_FAILURE_FALLBACK_ALL_PASS`、`STEP42_COHERENCE_FINE_PASS`，umount 22 ms。
所有 insmod/mount/cache_device 操作均在 vng guest；未触碰宿主机 zvol 或模块。

### 7.37 Phase 4/内核 Step 43 KERNEL-WRITE-ITER

普通动态文件已从旧 `.write` 切换为 `.write_iter`，write/writev/pwritev 直接消费
`iov_iter`，继续按 16 KiB bounce buffer 与 64 MiB model chunk 边界发送 ABI v20
`WRITE_DATA`。`generic_write_checks()` 负责 VFS 写入边界和 append 标志；为避免并发
append 在进入全局 data IPC 锁前选择相同 EOF，锁内会再次读取 `i_size`。cache 失效也
移入同一锁并保持在权威写提交之前，使失效前开始的 READ_DATA miss 不能在写后发布旧
epoch 数据。成功块更新 `ki_pos`/`i_size`；后续故障返回已提交字节数，首块前故障返回
负 errno；`copy_from_iter()` 短拷贝只提交实际字节并结束本次调用。同步路径对
`IOCB_NOWAIT` 返回 `EOPNOTSUPP`，不会假装非阻塞；旧 `.write` 已删除。

Cursor 验收自检（2026-09-16）：默认 Rust tests `193 passed`；clippy 与内核模块构建零警告。
专项 vng + loop 输出：
`STEP43_NORMAL_WRITE_PASS`、`STEP43_WRITEV_PASS`、`STEP43_O_APPEND_PASS`、
`STEP43_PWRITEV_PASS`、`STEP43_CACHE_INVALIDATE_PASS first_read_hit_delta=0`、
`STEP43_CACHE_REFILL_PASS delta=7`、`STEP43_WRITE_ITER_PASS`；Cursor 复跑 umount 27 ms。
所有 insmod/mount/cache_device 操作均在 vng guest；未触碰宿主机 zvol 或模块。

### 7.38 Phase 4/内核 Step 44 KERNEL-FSYNC

ABI v21 新增 opcode 25 `FSYNC`（payload inode_id u64@0，剩余零）和 opcode 26
`SYNC_FS`（32 字节 payload 全零），共享内存布局与 cache format v4 不变。普通文件
`.fsync`（`fdatasync` 同处理）以及 `sync_fs(wait=1)` 经同步 IPC 等 daemon 真正完成
屏障；`wait=0` 只作非阻塞第一阶段。原 `.write_inode` 仍不发 IPC：它会在 inode 回收
时调用，且当前没有内核 page-cache 写回数据；显式屏障由上述两个回调负责。fsync
与写同用 data IPC mutex，先同步引用对象再同步 metadata；失败返回错误，daemon
不在时拒绝假成功，不将内核 NVMe 读缓存当作权威写回源。

FileMetaStore 对已有 `meta.json` 执行文件和父目录 fsync（新命名空间无快照则先
创建）；LocalFs 对 inode 的全部 COW slice keys 执行文件和目录 fsync，syncfs
检查所有仍被引用的 keys、遍历 object root 同步文件和目录。缺失对象返回 EIO；
Mem 仅进程内确认。Redis 只等待 mutation ACK 与一致快照可见：本步未实现
WAIT/WAITAOF，进程/节点崩溃耐久取决于 Redis AOF/RDB 配置，RDB 不保证每次 fsync；
S3 以已完成 PUT 的 ACK 为边界；跨后端没有原子事务。

Codex 自检：195 Rust tests passed、clippy `-D warnings` 通过、模块零警告；
vng + loop `STEP44_FSYNC_FDATASYNC_SYNCFS_PASS`、`STEP44_SYNCFS_ONLY_PASS`、
`STEP44_OFFLINE_FAIL_CLOSED_PASS`、两文件 `STEP44_RESTART_READBACK_PASS`、
`STEP44_KERNEL_FSYNC_PASS`（最终复跑 umount 120 ms）；Step 43 vng 回归
`STEP43_WRITE_ITER_PASS`（umount 119 ms）。
Cursor 验收自检（2026-09-16）：默认 Rust tests `195 passed`；clippy 与内核模块构建零警告；
复跑 vng `STEP44_KERNEL_FSYNC_PASS`（umount_ms=31）。
旧 Step 15 GC 脚本的立即 `test ! -e` 在异步 GC 删除完成前失败（`bash -x`
定位于 unlink 阶段）；本步未改变 GC，亦未扩大范围修旧脚本的竞态断言。
只在 vng guest insmod/mount/loop；未触碰宿主机 zvol。

### 7.39 Phase 4/内核 Step 45 KERNEL-AOPS

普通 regular inode 接入读侧 `address_space_operations`：`read_folio`/`readahead`
使用现有 kernel cache hit 或 ABI v21 `READ_DATA` miss 填充干净 folio，EOF 后清零；
普通读走 `generic_file_read_iter`。write_iter 仍直接同步提交 daemon，不创建脏页；
成功写后清理 page cache，fsync 保持 Step 44 后端屏障。Redis 远端 revision ioctl
另推进 page-cache epoch，下次读惰性清页（避免在 daemon ioctl 中等待正由该 daemon
提供数据的 locked folio）；目前远端细粒度失效会保守清理本挂载所有已访问文件的
page cache，性能仍可优化。ABI **v21** 与 cache format **v4** 未变。

Codex 自检：`cargo test` **195 passed**；clippy `-D warnings`、
`make -C kestrelfs` 零警告。vng guest + loop 输出
`STEP45_WRITE_READ_PASS`、`STEP45_PAGECACHE_HIT_PASS`、
`STEP45_BACKING_CACHE_HIT_PASS delta=32`、`STEP45_REWRITE_NO_STALE_PASS`、
`STEP45_KERNEL_AOPS_PASS`（最终复跑 umount 343 ms）；Step 44/43 回归分别输出
`STEP44_KERNEL_FSYNC_PASS` / `STEP43_WRITE_ITER_PASS`。Step 43 旧脚本在测
NVMe hit 前只丢 clean pagecache，避免把新 filemap 命中误判为 NVMe 失效。
额外尝试旧 Step 20 脚本，在已有文件 `O_TRUNC` 阶段返回 `EOPNOTSUPP`；基线
`setattr` 已有 SIZE 与时间属性组合拒绝逻辑，本步未修改，旧脚本未算通过。
Cursor 验收自检（2026-09-16）：195 tests；clippy / make 零警告；复跑 vng
`STEP45_KERNEL_AOPS_PASS`（umount_ms=46；`STEP45_BACKING_CACHE_HIT_PASS delta=32`）。
所有模块/mount/loop 操作只在 vng guest；未触碰宿主机 zvol。

### 7.40 Phase 4/内核 Step 46 KERNEL-MMAP

普通文件 `.mmap` 复用 `generic_file_mmap` 的 filemap/page-cache 读侧路径；
`MAP_PRIVATE` 支持私有 COW，`MAP_SHARED` 只允许只读，显式拒绝可写共享并清除
`VM_MAYWRITE` 防止后续 `mprotect` 升级。同步 `write_iter` 与 truncate 通过已存在的
`truncate_inode_pages` 撤销旧 PTE/folio，使既有未 COW 映射下次访问重新 fault。
为让标准 `ftruncate`/`O_TRUNC` 工作，`setattr` 允许 SIZE 附带 VFS 自动设置的
mtime/ctime，但继续拒绝显式时间+SIZE 组合；daemon 的 TRUNCATE 仍更新持久 mtime。

Redis revision ioctl 不同步等待 locked folio（同线程 daemon 可能还须回答 READ_DATA）；
它推进全局 page-cache epoch 后为已有映射排队清页 worker。worker 在 inode 写锁下
撤销映射 PTE/folio；期间新 fault 释放 mmap/VMA 锁等待完成再重试，无法重试时
fail-closed SIGBUS。ioctl 返回到 worker 完成之间仍有短暂旧 PTE 可见窗口，加上
既有 ~100 ms probe 窗口，不能宣称分布式强一致。页缓存的 inode-list 远端失效仍
保守按所有已映射 inode 清理。ABI **v21**、cache format **v4** 未变；fsync 仍为
Step 44 后端屏障，无脏页/writeback。

Codex 自检：Rust `cargo test` **195 passed**；clippy `-D warnings`、
`make -C kestrelfs` 零警告。vng guest + loop 输出
`STEP46_MMAP_READ_PASS`、`STEP46_PRIVATE_COW_PASS`、
`STEP46_SHARED_WRITE_REJECT_PASS`、`STEP46_REWRITE_INVALIDATE_PASS`、
`STEP46_TRUNCATE_INVALIDATE_PASS`、`STEP46_COHERENCE_REFAULT_PASS`、
`STEP46_KERNEL_MMAP_PASS`（umount 118 ms）；Step 45/44 回归分别输出
`STEP45_KERNEL_AOPS_PASS`（169 ms）/`STEP44_KERNEL_FSYNC_PASS`（132 ms）。
Step 40 属性回归 `STEP40_POSIX_UTIMES_PASS`（74 ms）。
Cursor 验收自检（2026-09-17）：195 tests；clippy / make 零警告；复跑 vng
`STEP46_KERNEL_MMAP_PASS`（umount_ms=31）。
模块/mount/loop 仅在 vng guest；未触碰宿主机 zvol。

### 7.41 Phase 4/内核 Step 47 KERNEL-LOCKS

普通文件 `.flock` 与 `.lock` 复用 Linux 本地锁管理器：BSD flock 使用
`locks_lock_file_wait`，POSIX `F_SETLK/F_SETLKW` 和 OFD 字节锁经同一等待器，
`F_GETLK` 经 `posix_test_lock`。同一挂载的进程可正确互斥，阻塞等待在持有者
释放后唤醒，fd/进程退出由 VFS 自动清理锁。等待期间不持有 data IPC mutex 或
inode 锁，也不调用 daemon；锁是 advisory、本地 inode 状态，flock 与
POSIX/OFD 锁类彼此独立。ABI **v21**、cache format **v4** 未变。

Codex 自检：`cargo test` **195 passed**，clippy `-D warnings`、
`make -C kestrelfs` 零警告。vng guest + loop 输出 `STEP47_FLOCK_PASS`、
`STEP47_POSIX_FCNTL_PASS`、`STEP47_OFD_FCNTL_PASS`、
`STEP47_LOCK_CLASS_PASS`、`STEP47_LIFECYCLE_MMAP_WRITE_PASS`、
`STEP47_KERNEL_LOCKS_PASS`（umount 115 ms）；Step 46/36 回归分别为
`STEP46_KERNEL_MMAP_PASS`（116 ms）/`STEP36_POSIX_LIFECYCLE_PASS`
（31 ms）。
Cursor 验收自检（2026-09-17）：195 tests；clippy / make 零警告；复跑 vng
`STEP47_KERNEL_LOCKS_PASS`（umount_ms=32）。
脚本显式 insmod、独立 data-dir/daemon.log；只在 vng guest 内
操作 loop/mount/module，未触碰物理机 zvol。未实现跨节点/跨挂载分布式锁、
强制锁或远端 lease。

---

### 7.42 Phase 4/内核 Step 48 KERNEL-CACHE-ASYNC

hit 的 buffered/pinned-page BIO 使用 `submit_bio` + 独立 `end_io`/completion；
cache rwsem 读侧保持至完成和 CRC 校验，因此并发 invalidate/rewrite/evict
在复用 slot 前必须等待。Step 45 的冷 folio 先尝试命中，miss 才进入全局
bounce 锁并在锁内重查 cache/epoch。`cache_async_hit_submissions` 和
`cache_async_hit_peak` 暴露异步提交与峰值在途数；文件的 `read_folio` 仍同步
等待自己的结果，metadata/fill/journal 与 `READ_DATA` miss 未改。

Codex 自检：`cargo test` 195 passed；clippy `-D warnings`、
`make -C kestrelfs` 零警告。vng guest + loop：`STEP48_SAME_BLOCK_PASS`、
`STEP48_ADJACENT_BLOCKS_PASS`、`STEP48_ASYNC_BIO_PASS`（17 submissions，
peak 15）、`STEP48_COLD_PAGECACHE_HIT_PASS`、`STEP48_REWRITE_INVALIDATE_PASS`、
`STEP48_CHECKSUM_FALLBACK_PASS`（损坏后 fail-closed、回源再填）、
`STEP48_KERNEL_CACHE_ASYNC_PASS`（umount 335 ms）；Step 45/46/47 vng
回归通过。脚本显式 insmod、独立
data-dir/daemon.log，未触碰宿主机设备。旧 Step 26 脚本在 Step 45 之后因
warm read 被 VFS page cache 吸收，其旧峰值断言不再适用；旧 Step 24 脚本
曾在 checksum miss/refill 后的 `rmmod` 报 module in use；Step 48 独立故障注入
已验证 checksum 回退和模块卸载，旧脚本生命周期问题需复核。
Cursor 验收自检（2026-09-18）：195 tests；clippy / make 零警告；复跑 vng
`STEP48_KERNEL_CACHE_ASYNC_PASS`（umount_ms=31；`STEP48_ASYNC_BIO_PASS submissions=17 peak=13`；
`STEP48_CHECKSUM_FALLBACK_PASS failures=1`）。

### 7.43 Phase 4/内核 Step 49 CACHE-WRITE

普通 `write_iter` 现在走 `write_begin`/`write_end`，将旧字节保留于 partial
folio、更新本地 size 并标 dirty；`writepages` 逐 folio 在全局 bounce 锁下先失效
NVMe 读索引，再经现有 `WRITE_DATA` 同步提交 daemon。superblock 使用支持
writeback 的 BDI。为降低本步的一致性范围，`write_iter` 在返回前等待这些脏页
写回，成功后保留 clean filemap 页（write-through，而非延迟写缓存）。`fsync`/
`fdatasync` 先等待 filemap 写回，再调用既有 `OP_FSYNC` 耐久屏障；close `.flush`
也等待写回但不承诺 fsync 耐久。失败时 folio 重新标 dirty、记录 mapping/sb 错误
并返回错误；daemon 恢复后可重试。truncate 先写回再改变权威 size。

Codex 自检：`cargo test` 195 passed、clippy `-D warnings`、
`make -C kestrelfs` 零警告。vng guest + loop 输出 `STEP49_BUFFERED_FSYNC_PASS`、
`STEP49_PAGECACHE_RETAIN_PASS`、`STEP49_WRITEBACK_ERROR_PASS`、
`STEP49_RETRY_PASS`、`STEP49_RESTART_READBACK_PASS`、
`STEP49_CACHE_WRITE_PASS`（umount 187 ms）；Step 48 回归
`STEP48_KERNEL_CACHE_ASYNC_PASS`（peak 14、checksum failures 1、umount 770 ms），
Step 43 writev/append/pwritev 回归 `STEP43_WRITE_ITER_PASS`（umount 19 ms）。
Step 44 fsync/syncfs 回归 `STEP44_KERNEL_FSYNC_PASS`（umount 138 ms）。
Step 45 folio/NVMe 回归 `STEP45_KERNEL_AOPS_PASS`（umount 208 ms）。
Step 46 mmap 回归 `STEP46_KERNEL_MMAP_PASS`（共享写仍拒绝，umount 133 ms）。
Step 36 open-unlink 回归 `STEP36_POSIX_LIFECYCLE_PASS`（umount 56 ms）。
旧 Step 43/45/48 脚本增加 drop VFS clean folio 后的预热阶段，避免把新 filemap
命中误当作底层 NVMe 命中。模块加载、loop 和挂载仅在 vng guest；
IPC ABI v21、cache format v4 不变。可写 `MAP_SHARED` 仍返回 `EOPNOTSUPP`。
Cursor 验收自检（2026-09-20）：195 tests；clippy / make 零警告；复跑 vng
`STEP49_CACHE_WRITE_PASS`（umount_ms=18）。

## 8. 路线图（未做）

按 Cursor 既定策略的推荐优先级：

| 优先级 | 内容 | 说明 |
|---|---|---|
| 1 | **双包交付** | 自 Step 50 起每步约 2× 既往体量；见 remaining-capabilities §2 |
| 2 | **Step 50 MAP-SHARED-WRITE + WHITEOUT** | 提示词见 `docs/remaining-capabilities.md` §8 |
| 3 | 其后 | DIST-IO、运维/文档、延迟写缓存等（继续双包） |

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

- [x] Step 49 CACHE-WRITE 已由 Cursor 验收并提交
- [x] IPC ABI = 21；cache format = v4
- [x] Step 8–48 + Step 49 已验收状态已写清
- [x] 下一步明确：Step 50 双包（可写 MAP_SHARED + WHITEOUT；`docs/remaining-capabilities.md` §8）
- [x] README 保持中文
- [x] 测试约束：cache/mount 只在 vng+loop；daemon 日志写 `"$data_dir/daemon.log"`（§7.3 / §8 / §9.10）





---

## 11. 文档债务

以下文档与代码不一致，后续应更新（优先级低于功能开发）：

| 文件 | 问题 | 代码实际值 |
|---|---|---|
| `README.md` 细节 | 未逐一列举全部 DATA opcode | 完整 opcode 表见 `HANDOFF.md` / `kestrelfs_ipc.h` |
