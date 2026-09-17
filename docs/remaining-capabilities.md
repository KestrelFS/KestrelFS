# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-17，Cursor（验收 Step 47；选定 Step 48 = KERNEL-CACHE-ASYNC）
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

- Phase 4 至 Step 47（含 write_iter、fsync、aops、mmap、本地文件锁）均已验收。
- 已验收 IPC ABI **v21**，cache format **v4**。
- **战略**：内核优先（write_iter ✅ → fsync ✅ → aops ✅ → mmap ✅ → locks ✅ → cache-async）。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor：内核优先）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–23 | … + LOCKS | Step 24–47 | **ACCEPTED** | 本地文件锁已落地 |
| 24 | KERNEL-CACHE-ASYNC | Step 48 cache hit 异步 BIO | **DECIDED** | 热路径性能 |
| — | WHITEOUT / DIST-IO / CACHE-WRITE | 其它 | PROPOSED/DEFERRED | 内核主线之后 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

读缓存与 fsync 已 ACCEPTED；`CACHE-WRITE`（写回缓存）仍 DEFERRED。

## 4. 内核 VFS

### KERNEL-LOCKS — Step 47

- 状态：`ACCEPTED`（Cursor，2026-09-17）
- 实现：`.flock` / `.lock` → `locks_lock_file_wait`；`F_GETLK` → `posix_test_lock`；
  本地 advisory；flock 与 POSIX/OFD 锁类独立。

### KERNEL-CACHE-ASYNC — Step 48

- 状态：`DECIDED`
- 目标：把 NVMe cache **hit** 路径从同步等待 BIO 改为异步 completion（可重叠多请求）。
- 范围：见 §8。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-17 | Cursor | KERNEL-MMAP | Step 46 验收 | **ACCEPTED** |
| 2026-09-17 | Cursor | KERNEL-LOCKS | 选定 Step 47 | **DECIDED** |
| 2026-09-17 | Codex | KERNEL-LOCKS | 实现与自检 | **REVIEW** |
| 2026-09-17 | Cursor | KERNEL-LOCKS | Step 47 验收 | **ACCEPTED**；195 tests + Cursor vng PASS |
| 2026-09-17 | Cursor | KERNEL-CACHE-ASYNC | 选定 Step 48 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 48 不顺手做 writeback/`CACHE-WRITE`、可写 MAP_SHARED、跨节点锁、WHITEOUT、
  完整 io_uring 用户态出口（除非作为内部提交手段且范围可控）。

## 8. 当前 Codex 提示词（Step 48）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md、docs/remaining-capabilities.md（§2/§6/§8）与 docs/phase4-nvme-cache.md。
HEAD 应含 Step 47（本地文件锁）。战略：内核优先。

开工时：KERNEL-CACHE-ASYNC → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 一旦改 kestrelfs/*.c 或依赖 mount：必须 vng
- Redis/S3 门控测可选；默认 cargo test 不依赖外部服务

## 目标：Step 48 — KERNEL-CACHE-ASYNC（cache hit 异步 BIO）
当前 NVMe cache hit 虽可并行，但仍同步等待 BIO completion。本步把 hit 路径改为
异步 completion，减少调用者在设备上的阻塞时间，并允许同一 inode/多调用者重叠
进行中的读。

必做：
1. 识别同步 BIO 等待点（如 `kestrelfs_cache_read_iter` / 相关 submit+wait），改为
   异步提交 + end_io/completion 唤醒等待者（或等价 modern API）
2. 正确性：与同步路径语义一致——命中数据正确、checksum 失败 fail closed、
   invalidate/epoch 期间不得返回陈旧块
3. 并发：至少支持同一文件多个并发读者等待不同（或相同）进行中的 BIO，无死锁；
   与 Step 45 folio 填充路径共存（冷 folio 仍可先同步，但 hit 侧须异步）
4. 可观测：保留或扩展 hit 计数；§9 说明同步残留路径（若有）
5. 明确不做完整 writeback / 用户态 io_uring 导出

要求：
1. `make -C kestrelfs` 零警告；cargo test 不回归
2. STEP48_*_PASS vng：写→读回正确；drop_caches 后多并发读者读同一/相邻块仍正确；
   可选对比或至少断言异步路径被使用（计数/trace/标志任选其一写清）
3. ABI 若无变化保持 v21；有变化须 bump 并断言
4. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
5. 不要擅自 commit/push

## 明确不做
CACHE-WRITE/脏页写回、可写 MAP_SHARED、分布式锁、WHITEOUT、跨节点一致性增强。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + Step 48 vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-17 — Step 47 KERNEL-LOCKS（Cursor ACCEPTED）

- `.flock` / `.lock` → `locks_lock_file_wait`；`F_GETLK` → `posix_test_lock`；
  本地 advisory；flock 与 POSIX/OFD 锁类独立；无 daemon IPC。
- 验证：195 tests；clippy / make 干净；Cursor 复跑 vng
  `STEP47_KERNEL_LOCKS_PASS`（umount_ms=32）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-17 — Step 47 KERNEL-LOCKS（Codex REVIEW）

- 选择同时实现 A+B：普通文件 `.flock` / `.lock` 接入 Linux 本地锁管理器；
  OFD 锁一并可用；不等待 daemon/bounce。
- 自检：195 tests；vng `STEP47_*_PASS`（Codex umount 115 ms）及 Step 46/36 回归。
- 未做：跨节点锁、lease/pubsub、writeback、可写 MAP_SHARED、async BIO、WHITEOUT。

### 2026-09-17 — Step 46 KERNEL-MMAP（Cursor ACCEPTED）

- `generic_file_mmap` + 自定义 fault；MAP_PRIVATE/只读 SHARED；可写 SHARED
  `EOPNOTSUPP`；映射失效 worker。
- 验证：Cursor vng `STEP46_KERNEL_MMAP_PASS`（umount_ms=31）。
