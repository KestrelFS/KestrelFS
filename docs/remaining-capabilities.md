# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-16，Cursor（验收 Step 45；选定 Step 46 = KERNEL-MMAP）
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

- Phase 4 至 Step 45（含 write_iter、fsync、读侧 aops/page cache）均已验收。
- 已验收 IPC ABI **v21**，cache format **v4**。
- **战略**：内核优先（write_iter ✅ → fsync ✅ → aops ✅ → mmap → locks → cache-async）。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor：内核优先）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–21 | … + AOPS | Step 24–45 | **ACCEPTED** | 读侧 page cache 已落地 |
| 22 | KERNEL-MMAP | Step 46 文件 mmap | **DECIDED** | 依赖 aops |
| 23 | KERNEL-LOCKS | flock / POSIX locks | PROPOSED | 多进程共享 |
| 24 | KERNEL-CACHE-ASYNC | cache hit 异步 BIO | PROPOSED | 热路径性能 |
| — | WHITEOUT / DIST-IO / CACHE-WRITE | 其它 | PROPOSED/DEFERRED | 内核主线之后 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

读缓存与 fsync 已 ACCEPTED；`CACHE-WRITE`（写回缓存）仍 DEFERRED。

## 4. 内核 VFS

### KERNEL-AOPS — Step 45

- 状态：`ACCEPTED`（Cursor，2026-09-16）
- 实现：策略 A `generic_file_read_iter` + `read_folio`/`readahead`；写后清页；
  Redis ioctl 推进 page-cache epoch 惰性清页。

### KERNEL-MMAP — Step 46

- 状态：`DECIDED`
- 目标：为普通文件导出可用的 `mmap`（至少可读 / `MAP_PRIVATE`）。
- 范围：见 §8。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-16 | Cursor | KERNEL-WRITE-ITER | Step 43 验收 | **ACCEPTED** |
| 2026-09-16 | Cursor | KERNEL-FSYNC | Step 44 验收 | **ACCEPTED** |
| 2026-09-16 | Cursor | KERNEL-AOPS | 选定 Step 45 | **DECIDED** |
| 2026-09-16 | Codex | KERNEL-AOPS | 实现与自检 | **REVIEW** |
| 2026-09-16 | Cursor | KERNEL-AOPS | Step 45 验收 | **ACCEPTED**；195 tests + Cursor vng PASS |
| 2026-09-16 | Cursor | KERNEL-MMAP | 选定 Step 46 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 46 不顺手做 writeback/脏页写回、flock、async BIO、WHITEOUT、完整 POSIX 共享可写语义（若无 writeback 则明确拒绝或只读共享）。

## 8. 当前 Codex 提示词（Step 46）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
HEAD 应含 Step 45（读侧 aops / generic_file_read_iter）。战略：内核优先。

开工时：KERNEL-MMAP → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 一旦改 kestrelfs/*.c 或依赖 mount：必须 vng
- Redis/S3 门控测可选；默认 cargo test 不依赖外部服务

## 目标：Step 46 — KERNEL-MMAP（文件 mmap）
在 Step 45 读侧 page cache 之上，为普通文件导出可用的 `mmap`。

必做：
1. 普通文件 `.mmap`：优先复用 `generic_file_mmap`（或当前内核等价接口）
2. 至少支持：`MAP_PRIVATE` 读；以及通过 mmap 读到的内容与 `read()`/`pread` 一致
3. 与 Step 45 一致性：同步 `write_iter` / truncate / Redis page-cache epoch 失效后，
   既有映射不得继续看到陈旧内容（invalidate / fault 再填）
4. 若尚无 writeback：`MAP_SHARED`+可写要么明确 `EOPNOTSUPP`/`EINVAL`，要么文档化
   为只读共享；禁止假装脏页已写回 daemon
5. fsync 仍走 Step 44 后端屏障；不要引入假 writeback

要求：
1. `make -C kestrelfs` 零警告；cargo test 不回归
2. STEP46_*_PASS vng：写文件 → mmap 读回一致；写后旧映射不脏读；可选
   MAP_PRIVATE 修改不影响文件；若拒绝 SHARED 写则断言错误码
3. ABI 若无变化保持 v21；有变化须 bump 并断言
4. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
5. 不要擅自 commit/push

## 明确不做
writeback 写缓存、flock、async BIO、WHITEOUT、完整可写 MAP_SHARED 语义（可留到 CACHE-WRITE）。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + Step 46 vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-16 — Step 45 KERNEL-AOPS（Cursor ACCEPTED）

- 策略 A：`generic_file_read_iter` + `read_folio`/`readahead`；写后清页；
  Redis ioctl 推进 page-cache epoch 惰性清页（inode-list 远端失效对 page cache
  退化为全文件保守失效）。
- 验证：195 tests；clippy / make 干净；Cursor 复跑 vng
  `STEP45_KERNEL_AOPS_PASS`（umount_ms=46；backing hit delta=32）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-16 — Step 45 KERNEL-AOPS（Codex REVIEW）

- 选择 §8 策略 A：普通文件 `read_iter` 委托 `generic_file_read_iter`，regular inode
  设置读侧 a_ops；`read_folio` / `readahead` 从现有 NVMe cache hit 或 ABI v21
  `READ_DATA` miss 填充干净 folio，EOF 后清零。warm filemap hit 不访问 daemon/bounce。
- 同步 `write_iter` 不写脏页；在 inode 写锁下提交后、释放 bounce 锁后清理 page cache，
  避免等待持有 folio lock 的读者时死锁；`read_folio` 与写共用 bounce 锁，防止旧填页
  跨过写提交。Step 44 fsync/fdatasync/syncfs 后端屏障不变。
- Redis revision 失效 ioctl 同时推进 page-cache epoch；下次读在 inode 写锁下惰性清页。
  不在 daemon ioctl 中同步等待 folio（可能与待服务的 READ_DATA 形成死锁）。权衡：
  Step 42 的 inode-list 远端失效目前对 page cache 退化为全文件保守失效，NVMe
  索引仍保持细粒度；远端一致性窗口仍由原 100 ms probe 决定。
- ABI **v21**、cache format **v4** 均未变。修改内核 file/inode/chardev/header，新增
  `test-step45-kernel-aops-vng.sh`，调整 Step 43 旧脚本使其显式 drop clean pages 后
  测 NVMe hit；同步 HANDOFF 待验收表述与中文 README。
- 自检：`cargo test` **195 passed**；clippy / make 零警告；Codex vng
  `STEP45_KERNEL_AOPS_PASS`（umount 343 ms）及 Step 44/43 回归。
- 风险：冷 folio 同步 IPC/BIO；Redis page-cache 细粒度失效未优化；旧 Step 20
  `O_TRUNC` 与时间属性组合 `EOPNOTSUPP` 基线未改。完整 mmap/writeback/flock
  未做。

### 2026-09-16 — Step 44 KERNEL-FSYNC（Cursor ACCEPTED）

- ABI v21 `OP_FSYNC`/`OP_SYNC_FS`；File+LocalFs 真 fsync；离线 fail closed；
  fsync=fdatasync（无 page-cache 写）。
- 验证：195 tests；clippy / make 干净；Cursor 复跑 vng `STEP44_KERNEL_FSYNC_PASS`
  （umount_ms=31）。
