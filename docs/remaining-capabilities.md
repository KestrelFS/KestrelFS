# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-17，Cursor（验收 Step 46；选定 Step 47 = KERNEL-LOCKS）
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

- Phase 4 至 Step 46（含 write_iter、fsync、aops、mmap）均已验收。
- 已验收 IPC ABI **v21**，cache format **v4**。
- **战略**：内核优先（write_iter ✅ → fsync ✅ → aops ✅ → mmap ✅ → locks → cache-async）。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor：内核优先）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–22 | … + MMAP | Step 24–46 | **ACCEPTED** | 文件 mmap 已落地 |
| 23 | KERNEL-LOCKS | Step 47 flock / POSIX locks | **DECIDED** | 多进程共享 |
| 24 | KERNEL-CACHE-ASYNC | cache hit 异步 BIO | PROPOSED | 热路径性能 |
| — | WHITEOUT / DIST-IO / CACHE-WRITE | 其它 | PROPOSED/DEFERRED | 内核主线之后 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

读缓存与 fsync 已 ACCEPTED；`CACHE-WRITE`（写回缓存）仍 DEFERRED。

## 4. 内核 VFS

### KERNEL-MMAP — Step 46

- 状态：`ACCEPTED`（Cursor，2026-09-17）
- 实现：`generic_file_mmap` + 自定义 fault；MAP_PRIVATE/只读 SHARED；可写 SHARED
  `EOPNOTSUPP`；映射失效 worker；SIZE+隐式 mtime 放行。

### KERNEL-LOCKS — Step 47

- 状态：`DECIDED`
- 目标：为普通文件提供本地 `flock` 与/或 POSIX `fcntl` 字节锁（单挂载节点）。
- 范围：见 §8。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-16 | Cursor | KERNEL-AOPS | Step 45 验收 | **ACCEPTED** |
| 2026-09-16 | Cursor | KERNEL-MMAP | 选定 Step 46 | **DECIDED** |
| 2026-09-16 | Codex | KERNEL-MMAP | 实现与自检 | **REVIEW** |
| 2026-09-17 | Cursor | KERNEL-MMAP | Step 46 验收 | **ACCEPTED**；195 tests + Cursor vng PASS |
| 2026-09-17 | Cursor | KERNEL-LOCKS | 选定 Step 47 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 47 不顺手做跨节点分布式锁、lease/pubsub、writeback、async BIO、WHITEOUT、可写 MAP_SHARED。

## 8. 当前 Codex 提示词（Step 47）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
HEAD 应含 Step 46（文件 mmap）。战略：内核优先。

开工时：KERNEL-LOCKS → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 一旦改 kestrelfs/*.c 或依赖 mount：必须 vng
- Redis/S3 门控测可选；默认 cargo test 不依赖外部服务

## 目标：Step 47 — KERNEL-LOCKS（本地文件锁）
为普通文件提供本挂载节点上可用的文件锁，覆盖多进程协作最小集。

必做：
1. 至少实现其一并在 §9 写清选择：
   A) `flock`（`.flock` / `locks_lock_file_wait` 等），或
   B) POSIX `fcntl` F_SETLK/F_SETLKW/F_GETLK（`.lock` / `posix_lock_file`）
   推荐：两者都接（若工作量可控），至少 A 或 B 完整可用
2. 语义：同挂载内多进程互斥正确；进程退出自动释放；不与现有 open-unlink /
   mmap / write_iter 路径死锁
3. 范围声明：本步是**本地 VFS 锁**，不做跨节点/跨 daemon 分布式锁；若将来需要
   远端协调，另开步骤
4. 若只实现子集：在 HANDOFF 已知限制写清未支持项（如 OFD locks、强制锁）

要求：
1. `make -C kestrelfs` 零警告；cargo test 不回归
2. STEP47_*_PASS vng：两进程争用同一文件锁；持有者释放后等待者获得；
   进程异常退出后锁可再获取；可选 flock 与 fcntl 互斥关系按所选语义断言
3. ABI 若无变化保持 v21；有变化须 bump 并断言
4. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
5. 不要擅自 commit/push

## 明确不做
跨节点分布式锁、lease/pubsub、writeback、可写 MAP_SHARED、async BIO、WHITEOUT。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + Step 47 vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-17 — Step 46 KERNEL-MMAP（Cursor ACCEPTED）

- `generic_file_mmap` + 自定义 fault；MAP_PRIVATE/只读 SHARED；可写 SHARED
  `EOPNOTSUPP`；映射失效 worker；SIZE+隐式 mtime 放行。
- 验证：195 tests；clippy / make 干净；Cursor 复跑 vng
  `STEP46_KERNEL_MMAP_PASS`（umount_ms=31）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-16 — Step 46 KERNEL-MMAP（Codex REVIEW）

- 普通文件 `.mmap` 复用 `generic_file_mmap`/Step 45 folio；`MAP_PRIVATE` 支持私有
  COW，`MAP_SHARED` 只读。可写共享返回 `EOPNOTSUPP`，并从只读共享 VMA 清除
  `VM_MAYWRITE`，防止后续 `mprotect` 升级。
- Step 45 同步写后 `truncate_inode_pages` 撤销旧 PTE；SIZE truncate 同样经
  `truncate_setsize` 清理映射。为允许真实 `ftruncate`/`O_TRUNC`，仅放行 VFS 随
  SIZE 自动携带的 mtime/ctime，继续拒绝显式时间与 SIZE 组合。
- Redis revision ioctl 非阻塞：推进 epoch 后为已映射 inode 排队 worker；fault
  遇待清 epoch 释放锁等待重试或 SIGBUS。
- ABI **v21**、cache format **v4** 未变。vng：`STEP46_*_PASS`（Codex umount 118 ms）。
- 未做：可写 MAP_SHARED、writeback、flock、async BIO、WHITEOUT。

### 2026-09-16 — Step 45 KERNEL-AOPS（Cursor ACCEPTED）

- 策略 A：`generic_file_read_iter` + `read_folio`/`readahead`；写后清页；
  Redis page-cache epoch 惰性清页。
- 验证：195 tests；Cursor vng `STEP45_KERNEL_AOPS_PASS`（umount_ms=46）。
