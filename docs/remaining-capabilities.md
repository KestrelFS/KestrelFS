# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-20，Cursor（验收 Step 50；选定 Step 51 = WRITE-BEHIND + OPS/DOC；**后续 §8 体量约翻倍**）
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

- Phase 4 至 Step 50（含可写 MAP_SHARED、`RENAME_WHITEOUT`）均已验收。
- 已验收 IPC ABI **v22**，cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–26 | … + MAP-SHARED/WHITEOUT | Step 24–50 | **ACCEPTED** | 双包已验收 |
| 27 | WRITE-BEHIND + OPS/DOC | Step 51（双包） | **DECIDED** | 延迟写回 + 运维/文档 |
| 28 | DIST-IO + TEST-PERF | 后续双包 | PROPOSED | 分布式与性能基线 |
| — | 其它增强 | — | PROPOSED | 视需要并入双包 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

读缓存、异步 hit、write-through、可写 MAP_SHARED 已 ACCEPTED。下一步：真正的延迟写回（write-behind）。

## 4. 内核 VFS

### Step 50 — MAP-SHARED-WRITE + WHITEOUT

- 状态：`ACCEPTED`（Cursor，2026-09-20）
- 实现：可写 SHARED + `page_mkwrite`/VMA close 写回；ABI v22 `RENAME_WHITEOUT`
  + 持久 `S_IFCHR` 0:0 marker。

### Step 51 — WRITE-BEHIND + OPS/DOC（双包）

- 状态：`DECIDED`
- 目标 A：普通 buffered/`MAP_SHARED` 写可在脏页未完成 `WRITE_DATA` 前返回；
  后台 writeback；`fsync`/`fdatasync`/`msync`/`syncfs` 仍等待完成后再做耐久屏障。
- 目标 B：OPS-CONFIG + DOC-CLEANUP——整理模块/挂载/daemon 参数面与中文文档债务。
- 范围：见 §8。

## 5. 运维、测试与文档

OPS-CONFIG / DOC-CLEANUP 纳入 Step 51；TEST-PERF 留后续双包。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-20 | Cursor | CACHE-WRITE | Step 49 验收 | **ACCEPTED** |
| 2026-09-20 | Cursor | 体量 | 后续每步约 2× | **DECIDED** |
| 2026-09-20 | Cursor | MAP-SHARED-WRITE + WHITEOUT | 选定 Step 50 | **DECIDED** |
| 2026-09-20 | Codex | MAP-SHARED-WRITE + WHITEOUT | 实现与自检 | **REVIEW**；ABI v22；200 tests |
| 2026-09-20 | Cursor | MAP-SHARED-WRITE + WHITEOUT | Step 50 验收 | **ACCEPTED**；200 tests + Cursor vng PASS |
| 2026-09-20 | Cursor | WRITE-BEHIND + OPS/DOC | 选定 Step 51 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 51 不顺手做跨节点 lease、用户态 io_uring 导出、完整 DIST-IO 多节点协议。

## 8. 当前 Codex 提示词（Step 51，双包 ≈ 2×）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md、docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）与
docs/phase4-nvme-cache.md。HEAD 应含 Step 50（ABI v22；可写 MAP_SHARED + WHITEOUT）。
注意：§8 为双包，一次交付约等于既往两个 Step。

开工时：两项均 → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 一旦改 kestrelfs/*.c 或依赖 mount：必须 vng
- Redis/S3 门控测可选；默认 cargo test 不依赖外部服务

## 目标：Step 51 — WRITE-BEHIND + OPS/DOC（双包）

### A) WRITE-BEHIND（延迟写回）
当前 Step 49/50 是 write-through：`write_iter` / 多数路径在返回前等待 `writepages`。
本步改为允许脏页延迟提交：
1. 普通 buffered write 与可写 `MAP_SHARED` 弄脏 folio 后可先返回；由
   writeback/flusher 或显式触发异步推进 `WRITE_DATA`
2. `fsync`/`fdatasync`/`msync(MS_SYNC)`/`sync_fs(wait=1)` 必须等待相关脏页
   写回完成，然后再走既有 `OP_FSYNC`/`OP_SYNC_FS` 后端屏障
3. 失败 fail closed：写回错误经 mapping errseq / 后续 fsync 可见；不把仅内存
   脏页宣称为已持久
4. 与 NVMe 读缓存失效、truncate、page-cache epoch 保持一致
5. §9 写清：哪些路径仍可能同步等待（若有），以及与 write-through 的行为差异

### B) OPS-CONFIG + DOC-CLEANUP
1. 整理并文档化模块参数 / 挂载选项 / daemon CLI 的当前权威表（中文），放在
   README 或 `docs/`（择一，避免重复矛盾）
2. 清理 HANDOFF/README/phase4 中与已验收事实明显过期的表述（仅文档，不改行为）
3. 若发现安全默认值缺口（例如危险 wipe 路径），只做 hardening 文档+显式开关，
   不自动 wipe

## 要求
1. `make -C kestrelfs` 零警告；cargo test 不回归
2. STEP51_*_PASS vng 至少覆盖：
   - 写后不立即 fsync 时，进程退出/`sync`/显式 fsync 之一后重启 daemon 读回一致
   - fsync 失败路径可见（可选：杀 daemon 中途）
   - Step 50 可写 SHARED / WHITEOUT 回归不破
3. ABI 若无布局变化可保持 v22；有变化须 bump 并断言
4. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
5. 不要擅自 commit/push

## 明确不做
跨节点 lease/pubsub、用户态 io_uring 导出、完整多节点 DIST-IO 协议、自动 wipe。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + Step 51 vng（含 A；B 以文档审查+必要脚本）
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-20 — Step 50 MAP-SHARED-WRITE + WHITEOUT（Cursor ACCEPTED）

- 可写 `MAP_SHARED` + `page_mkwrite`/VMA close 写回；ABI v22 `RENAME_WHITEOUT`
  + 持久 `S_IFCHR` 0:0；Mem/File/Redis 覆盖。
- 验证：200 tests；clippy / make 干净；Cursor 复跑 vng
  `STEP50_DOUBLE_PACK_PASS`（umount_ms=11）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-20 — Step 50 MAP-SHARED-WRITE + WHITEOUT（Codex REVIEW）

- 可写 SHARED 复用 Step 49 writeback；`page_mkwrite` 校验 epoch；
  `RENAME_WHITEOUT=0x4`；ABI v21→v22；200 tests；多步 vng 回归。
- 未做：write-behind、跨节点锁、io_uring、DIST-IO。

### 2026-09-20 — Step 49 CACHE-WRITE（Cursor ACCEPTED）

- write-through aops；Cursor vng `STEP49_CACHE_WRITE_PASS`（umount_ms=18）。
