# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-20，Cursor（验收 Step 49；选定 Step 50 = MAP-SHARED-WRITE + WHITEOUT；**后续 §8 体量约翻倍**）
>
> 用途：供 Cursor 与 Codex 维护尚未完成的产品能力、优先级、方案决策、**当前可执行提示词**和验收结果。
> 本文是规划与协作入口，不替代 `HANDOFF.md` 的已验收事实。发生冲突时，按
> “代码与测试结果 → `HANDOFF.md` 已验收状态 → 本文规划”判断，并立即修正文档。
>
> **文档分工**
> - `HANDOFF.md`：已验收事实、ABI/format、测试配方与站立规则。
> - 本文：未完成能力、优先级、决策记录、**§8 当前 Codex 提示词**、**§9 实现汇报日志**。
> - 不另开指挥文档；人类只需让 Codex「读 `docs/remaining-capabilities.md` §8 并执行」。
>
> **体量约定（2026-09-20）**：自 Step 50 起，每个 §8 提示词按约 **2× 既往单步** 打包
> （通常合并 2 个原可独立 Step 的能力），减少往返验收次数。

## 1. 当前基线

- Phase 4 至 Step 49（含 write_iter、fsync、aops、mmap、locks、async hit、write-through）均已验收。
- 已验收 IPC ABI **v21**，cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–25 | … + CACHE-WRITE | Step 24–49 | **ACCEPTED** | write-through 已落地 |
| 26 | MAP-SHARED-WRITE + WHITEOUT | Step 50（双包） | **DECIDED** | 可写共享 mmap + rename WHITEOUT |
| 27 | DIST-IO + OPS/DOC | 后续双包 | PROPOSED | 分布式与运维 |
| — | TEST-PERF / 延迟写缓存等 | 增强 | PROPOSED | 视需要并入双包 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

读缓存、异步 hit、write-through writeback 已 ACCEPTED。可写 MAP_SHARED 纳入 Step 50。

## 4. 内核 VFS

### CACHE-WRITE — Step 49

- 状态：`ACCEPTED`（Cursor，2026-09-20）
- 实现：`write_begin`/`write_end`/`writepages` → `WRITE_DATA`；write-through；
  fsync 先写回再 `OP_FSYNC`；可写 MAP_SHARED 仍拒绝。

### Step 50 — MAP-SHARED-WRITE + WHITEOUT（双包）

- 状态：`DECIDED`
- 目标 A：在 Step 49 写回之上启用可写 `MAP_SHARED`（脏页经 writepages / fsync 落 daemon）。
- 目标 B：实现 `RENAME_WHITEOUT`（内核 + daemon + 持久语义）。
- 范围：见 §8。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 可并入后续双包。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-18 | Cursor | KERNEL-CACHE-ASYNC | Step 48 验收 | **ACCEPTED** |
| 2026-09-18 | Cursor | CACHE-WRITE | 选定 Step 49 | **DECIDED** |
| 2026-09-18 | Codex | CACHE-WRITE | 实现与自检 | **REVIEW** |
| 2026-09-20 | Cursor | CACHE-WRITE | Step 49 验收 | **ACCEPTED**；195 tests + Cursor vng PASS |
| 2026-09-20 | Cursor | 体量 | 后续每步约 2× | **DECIDED**；自 Step 50 起 §8 双包 |
| 2026-09-20 | Cursor | MAP-SHARED-WRITE + WHITEOUT | 选定 Step 50 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 50 不顺手做跨节点锁/lease、用户态 io_uring 导出、完整延迟写缓存（write-behind）、DIST-IO。

## 8. 当前 Codex 提示词（Step 50，双包 ≈ 2×）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md、docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）与
docs/phase4-nvme-cache.md。HEAD 应含 Step 49（write-through aops）。
注意：自本步起 §8 为双包，一次交付约等于既往两个 Step。

开工时：两项均 → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 一旦改 kestrelfs/*.c 或依赖 mount：必须 vng
- Redis/S3 门控测可选；默认 cargo test 不依赖外部服务

## 目标：Step 50 — MAP-SHARED-WRITE + RENAME_WHITEOUT（双包）

### A) 可写 MAP_SHARED
在 Step 49 `writepages`/`WRITE_DATA` 之上，允许普通文件 `mmap(..., PROT_WRITE,
MAP_SHARED)`：
1. 去掉（或收窄）对可写 SHARED 的 `EOPNOTSUPP`；只读 SHARED 的 `VM_MAYWRITE`
   策略按可写语义调整
2. 脏页经现有 writeback 提交 daemon；`msync`/`fsync`/`munmap` 路径不得丢失脏数据
3. 与 NVMe 读缓存、page-cache epoch、truncate/rewrite 失效保持一致
4. 失败 fail closed；不把仅内存脏页假装已持久

### B) RENAME_WHITEOUT
补齐长期拒绝的 `RENAME_WHITEOUT`：
1. 内核 `rename`/`RENAME_DATA` 接受 WHITEOUT（与 NOREPLACE/EXCHANGE 互斥规则
   按 Linux 语义实现并在 §9 写清）
2. daemon MetaStore（Mem/File，Redis 若可测）持久 whiteout 语义；lookup/readdir
   行为符合预期（至少覆盖 overlay 常用：目标被 whiteout 遮挡）
3. 若需新 inode 类型/ABI 字段：bump ABI 并断言；否则保持 v21 并文档化布局
4. vng + 单测覆盖成功路径与非法 flag 组合

## 要求
1. `make -C kestrelfs` 零警告；cargo test 不回归
2. STEP50_*_PASS vng 至少覆盖：
   - 可写 SHARED：mmap 写 → msync/fsync → 重启 daemon 读回一致；与 private COW 对照
   - WHITEOUT：rename 带 WHITEOUT 后 lookup/readdir 语义正确；非法组合拒绝
3. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
4. 不要擅自 commit/push

## 明确不做
跨节点锁/lease、用户态 io_uring 导出、完整 write-behind 延迟写缓存、DIST-IO、自动 wipe。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + Step 50 vng（含 A+B）
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-20 — Step 49 CACHE-WRITE（Cursor ACCEPTED）

- write-through aops：`write_begin`/`write_end`/`writepages` → `WRITE_DATA`；
  fsync 先写回再 `OP_FSYNC`；失败 redirty；可写 MAP_SHARED 仍拒绝。
- 验证：195 tests；clippy / make 干净；Cursor 复跑 vng
  `STEP49_CACHE_WRITE_PASS`（umount_ms=18）。
- Commit：随 Cursor 本轮验收推送。
- 决策：后续 §8 体量约翻倍（双包）。

### 2026-09-18 — Step 49 CACHE-WRITE（Codex REVIEW）

- 保守 write-through；保留 clean filemap；fsync/flush 等待写回；
  NVMe 读缓存在 dirtying/WRITE_DATA 前失效；truncate 先写回。
- 自检：195 tests；vng `STEP49_*_PASS`（Codex umount 187 ms）及多步回归。
- 未做：可写 MAP_SHARED、WHITEOUT、延迟写缓存、分布式锁。

### 2026-09-18 — Step 48 KERNEL-CACHE-ASYNC（Cursor ACCEPTED）

- hit BIO 异步 completion；Cursor vng peak=13。
