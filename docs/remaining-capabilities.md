# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-16，Cursor（验收 Step 43；选定 Step 44 = KERNEL-FSYNC）
>
> 用途：供 Cursor 与 Codex 维护尚未完成的产品能力、优先级、方案决策、**当前可执行提示词**和验收结果。
> 本文是规划与协作入口，不替代 `HANDOFF.md` 的已验收事实。发生冲突时，按
> “代码与测试结果 → `HANDOFF.md` 已验收状态 → 本文规划”判断，并立即修正文档。
>
> **文档分工**
> - `HANDOFF.md`：已验收事实、ABI/format、测试配方与站立规则。
> - 本文：未完成能力、优先级、决策记录、**§8 当前 Codex 提示词**、**§9 实现汇报日志**。
> - 不另开指挥文档；人类只需让 Codex「读 `docs/remaining-capabilities.md` §8 并执行」。

## 1. 当前基线

- Phase 4 cache、DIST、POSIX、COHERENCE-FINE、KERNEL-WRITE-ITER（Step 24–43）均已验收。
- 已验收 IPC ABI **v20**，cache format **v4**。
- **战略**：内核优先（write_iter ✅ → fsync → aops/page cache → mmap → locks → cache-async）。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor：内核优先）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–19 | … + WRITE-ITER | Step 24–43 | **ACCEPTED** | write_iter 已落地 |
| 20 | KERNEL-FSYNC | Step 44 真实 fsync | **DECIDED** | 纠正空转刷盘语义 |
| 21 | KERNEL-AOPS | address_space + 读侧 page cache | PROPOSED | mmap 基础 |
| 22 | KERNEL-MMAP | 文件 mmap | PROPOSED | 应用兼容 |
| 23 | KERNEL-LOCKS | flock / POSIX locks | PROPOSED | 多进程共享 |
| 24 | KERNEL-CACHE-ASYNC | cache hit 异步 BIO | PROPOSED | 热路径性能 |
| — | WHITEOUT / DIST-IO / CACHE-WRITE | 其它 | PROPOSED/DEFERRED | 内核主线之后 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

CACHE-* / COHERENCE / WRITE-ITER 已 ACCEPTED；`CACHE-WRITE` 仍 DEFERRED。

## 4. 内核 VFS

### KERNEL-WRITE-ITER — Step 43

- 状态：`ACCEPTED`（Cursor，2026-09-16）
- 实现：`.write_iter`；write/writev/pwritev；O_APPEND；cache invalidate 在锁内提交前。

### KERNEL-FSYNC — Step 44

- 状态：`DECIDED`
- 目标：`fsync`/`fdatasync`/`sync_fs` 真正等待 daemon 侧元数据与对象耐久落盘。
- 范围：见 §8。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-16 | Cursor | COHERENCE-FINE | Step 42 验收 | **ACCEPTED** |
| 2026-09-16 | Cursor | KERNEL-WRITE-ITER | 选定 Step 43 | **DECIDED** |
| 2026-09-16 | Codex | KERNEL-WRITE-ITER | 实现与自检 | **REVIEW** |
| 2026-09-16 | Cursor | KERNEL-WRITE-ITER | Step 43 验收 | **ACCEPTED**；193 tests + Cursor vng PASS |
| 2026-09-16 | Cursor | KERNEL-FSYNC | 选定 Step 44 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 44 不顺手做 mmap/page cache、write-back、flock、async BIO、WHITEOUT。

## 8. 当前 Codex 提示词（Step 44）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
HEAD 应含 Step 43（KERNEL-WRITE-ITER；ABI v20）。战略：内核优先。

开工时：KERNEL-FSYNC → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 一旦改 kestrelfs/*.c 或依赖 mount：必须 vng
- Redis/S3 门控测可选；默认 cargo test 不依赖外部服务

## 目标：Step 44 — KERNEL-FSYNC
纠正当前 `write_inode`/`sync_fs` 近似空转：让 `fsync`/`fdatasync`/`syncfs` 真正等待
daemon 把该 inode（及必要时全局）的元数据与对象数据耐久化。

必做：
1. 内核：为普通文件实现 `.fsync`（或等价 file op）；`sync_fs` 覆盖挂载级 sync。
   `fdatasync` 与 `fsync` 的差异若本步简化，须在 §9 写明。
2. ABI：新增或扩展 opcode（预期 bump 至 v21），例如 `OP_FSYNC` / `OP_SYNC_FS`，
   携带 inode（可选）与 flags；C/Rust 同步 + 编译期断言。
3. Daemon/MetaStore/ObjectStore：
   - FileMetaStore：确保 fsync 路径强制 `meta.json` 落盘（fsync 文件/目录）
   - LocalFsObjectStore：相关对象文件 fsync（至少本 inode 引用的 block；若成本过高可文档化为“全 objects dir sync”并测试）
   - Mem 后端：可空操作成功
   - Redis/S3：至少等待本步已提交的写对客户端可见的耐久点（Redis 可用 WAIT/或文档化“依赖 Redis AOF/RDB 配置”；S3 put 已返回即可视为耐久）。须在 §9 写清每后端语义。
4. 与 write_iter / cache：fsync 不要求写回 cache（仍无 write-back）；但不得让应用以为刷盘成功而 daemon 仍只在内存。

要求：
1. 单测覆盖 opcode/handler；File 后端可用临时目录验证 fsync 后杀进程仍可读（daemon 级）
2. STEP44_*_PASS vng：写后 fsync、杀 daemon 重启（同 data_dir）数据仍在；syncfs 或目录级行为按实现覆盖
3. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
4. 不要擅自 commit/push

## 明确不做
page cache/mmap、flock、async BIO、write-back、WHITEOUT、扩大 coherence。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + Step 44 vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-16 — Step 43 KERNEL-WRITE-ITER（Cursor ACCEPTED）

- `.write_iter` 统一 write/writev/pwritev；O_APPEND；锁内 invalidate；ABI/format 未变。
- 验证：193 tests；clippy / make 干净；Cursor 复跑 vng `STEP43_WRITE_ITER_PASS`
  （umount_ms=27）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-16 — Step 43 KERNEL-WRITE-ITER（Codex REVIEW）

- 路径：`kestrelfs_reg_file_ops` 与 `kestrelfs_writable_file_ops` 删除旧 `.write`，统一
  注册 `.write_iter`。write/writev/pwritev 直接由 `copy_from_iter()` 消费 iovec，仍按
  16 KiB bounce buffer 和 64 MiB model chunk 边界发送同步 `WRITE_DATA`；IPC ABI
  **v20**、共享内存和 cache format **v4** 均未改变。
- append/失效：先由 `generic_write_checks()` 处理 VFS 限制与 `IOCB_APPEND`，再在
  `kestrelfs_data_ipc_lock` 内重新读取 EOF，避免并发 append 选择相同 offset。cache
  invalidate 移到同一锁内、保持在权威写提交之前；这样失效前开始的 READ_DATA miss
  携带旧 epoch，不能在写后发布旧 cache 数据。
- 错误/部分写：首个提交前的 generic check、锁、连接、失效、复制或 IPC 错误返回负
  errno；已有 chunk 提交后若后续失败则返回已写字节数。`copy_from_iter()` 短拷贝只把
  实际复制字节发给 daemon，成功后作为本次短写结束；仅成功提交后更新 `ki_pos` 与
  `i_size`。同步 IPC 不能兑现 `IOCB_NOWAIT`，因此显式返回 `EOPNOTSUPP` 而不阻塞。
- 测试：`193 passed`；clippy / make 干净；vng `STEP43_*_PASS`（umount 26 ms）。
- 未做：page cache/mmap、fsync、flock、async BIO、write-back。

### 2026-09-16 — Step 42 COHERENCE-FINE（Cursor ACCEPTED）

- ABI v20；Cursor vng `STEP42_COHERENCE_FINE_PASS`（umount_ms=24）。
