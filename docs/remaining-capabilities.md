# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-16，Cursor（验收 Step 44；选定 Step 45 = KERNEL-AOPS）
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

- Phase 4 至 Step 44（含 write_iter、fsync）均已验收。
- 已验收 IPC ABI **v21**，cache format **v4**。
- **战略**：内核优先（write_iter ✅ → fsync ✅ → aops/page cache → mmap → locks → cache-async）。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor：内核优先）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–20 | … + FSYNC | Step 24–44 | **ACCEPTED** | fsync 屏障已落地 |
| 21 | KERNEL-AOPS | Step 45 读侧 page cache | **DECIDED** | mmap 前置 |
| 22 | KERNEL-MMAP | 文件 mmap | PROPOSED | 依赖 aops |
| 23 | KERNEL-LOCKS | flock / POSIX locks | PROPOSED | 多进程共享 |
| 24 | KERNEL-CACHE-ASYNC | cache hit 异步 BIO | PROPOSED | 热路径性能 |
| — | WHITEOUT / DIST-IO / CACHE-WRITE | 其它 | PROPOSED/DEFERRED | 内核主线之后 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

读缓存与 fsync 已 ACCEPTED；`CACHE-WRITE`（写回缓存）仍 DEFERRED。

## 4. 内核 VFS

### KERNEL-FSYNC — Step 44

- 状态：`ACCEPTED`（Cursor，2026-09-16）
- 实现：ABI v21 `OP_FSYNC`/`OP_SYNC_FS`；File+LocalFs 真 fsync；离线 fail closed。

### KERNEL-AOPS — Step 45

- 状态：`DECIDED`
- 目标：为普通文件引入 `address_space_operations`，至少打通**读侧** page cache。
- 范围：见 §8。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-16 | Cursor | KERNEL-WRITE-ITER | Step 43 验收 | **ACCEPTED** |
| 2026-09-16 | Cursor | KERNEL-FSYNC | 选定 Step 44 | **DECIDED** |
| 2026-09-16 | Codex | KERNEL-FSYNC | 实现与自检 | **REVIEW** |
| 2026-09-16 | Cursor | KERNEL-FSYNC | Step 44 验收 | **ACCEPTED**；195 tests + Cursor vng PASS |
| 2026-09-16 | Cursor | KERNEL-AOPS | 选定 Step 45 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 45 不顺手做完整 mmap 导出、writeback/脏页写回、flock、async BIO、WHITEOUT。

## 8. 当前 Codex 提示词（Step 45）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
HEAD 应含 Step 44（ABI v21 OP_FSYNC/OP_SYNC_FS）。战略：内核优先。

开工时：KERNEL-AOPS → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 一旦改 kestrelfs/*.c 或依赖 mount：必须 vng
- Redis/S3 门控测可选；默认 cargo test 不依赖外部服务

## 目标：Step 45 — KERNEL-AOPS（读侧 page cache）
为普通文件接入 `address_space_operations`，让读路径可以进入内核 page cache
（为后续 mmap 打基础）。本步以**读**为主；写回/脏页可仍走现有 write_iter。

必做：
1. 给 regular inode 设置 `i_mapping->a_ops`；实现至少 `read_folio`/`readahead`
   （或当前内核版本等价 API：`readpage` 已淘汰则用 folio 接口）
2. 页填充数据来源：可复用现有 cache hit 或 READ_DATA miss 路径；语义与直接
   `read_iter` 一致（同一文件内容）
3. `read_iter` 策略二选一并在 §9 写清：
   A) 切换到 `generic_file_read_iter`（推荐，若 a_ops 完整），或
   B) 保留自定义 read_iter 但在命中 page cache 时走 folio
4. 写路径：本步可不启用 writeback；若保留 `.write_iter` IPC 写，须在写后
   `invalidate`/`truncate` 相关 page cache，避免读到陈旧页
5. fsync：仍走 Step 44 屏障；不要假装 page cache 脏页已写回（若无 writeback）

要求：
1. `make -C kestrelfs` 零警告；cargo test 不回归
2. STEP45_*_PASS vng：写入后读回；同一文件第二次读应体现 page cache 收益或至少
   行为正确（可用 `/proc`/`drop_caches` 或重复 read 断言一致性）；写后旧页不得脏读
3. 若本步引入 ABI 变化须 bump 并断言；否则保持 v21
4. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
5. 不要擅自 commit/push

## 明确不做
完整 `mmap` 导出（可留到 Step 46）、writeback 写缓存、flock、async BIO、WHITEOUT。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + Step 45 vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-16 — Step 44 KERNEL-FSYNC（Cursor ACCEPTED）

- ABI v21 `OP_FSYNC`/`OP_SYNC_FS`；File+LocalFs 真 fsync；离线 fail closed；
  fsync=fdatasync（无 page-cache 写）。
- 验证：195 tests；clippy / make 干净；Cursor 复跑 vng `STEP44_KERNEL_FSYNC_PASS`
  （umount_ms=31）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-16 — Step 44 KERNEL-FSYNC（Codex REVIEW）

- ABI：v20 → **v21**，`OP_FSYNC`/`OP_SYNC_FS`；共享内存与 cache format v4 不变。
- 内核：`.fsync`/`fdatasync` 同路径；`sync_fs(wait=1)` 全挂载屏障；离线 `ENOTCONN`。
- File+LocalFs：对象与 meta.json 真 fsync；缺失引用 `EIO`。
- Mem/Redis/S3 语义差异已文档化。
- 单测 195；vng STEP44_*_PASS（Codex umount 120 ms）。
- 未做：page cache/mmap、flock、async BIO、write-back。

### 2026-09-16 — Step 43 KERNEL-WRITE-ITER（Cursor ACCEPTED）

- `.write_iter`；Cursor vng `STEP43_WRITE_ITER_PASS`（umount_ms=27）。
