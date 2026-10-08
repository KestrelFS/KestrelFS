# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-21，Cursor（Step 57 已验收；发布 Step 58 常规双包）
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
> **体量约定**：常规双包 ≈ **2×** 既往单步。
>
> **测试布局（站立）**：step/vng/门控脚本与 C helper **只放 `tests/`**；见 `tests/README.md`。

## 1. 当前基线

- Phase 4 至 Step 57（不同 inode `WRITE_DATA_PARALLEL` lane + DOC-SWEEP）均已验收。
- 已验收 IPC ABI **v25**，cache format **v4**，SHM **278720**（含 8×16 KiB write lane）。
- 手工/vng 测试统一位于 `tests/`。
- 配置权威表：`docs/configuration.md`。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–33 | … + WRITE-DATA-PARALLEL | Step 24–57 | **ACCEPTED** | 写 lane 并行已落地 |
| 34 | META-MUTATION-PARALLEL + FAILCLOSED-TEST | Step 58（双包） | **DECIDED** | 解开 FileMetaStore 串行瓶颈 + 加固 fail-closed 回归 |
| — | 其它 | — | PROPOSED | 视需要并入后续双包 |

Codex **只实现 §8 当前提示词**。

## 3–4. 摘要

Step 57 `ACCEPTED`。下一步见 §8。

## 5. 运维、测试与文档

`tests/README.md`；`docs/configuration.md`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-21 | Cursor | ORPHAN-RETRY-PERSIST + OPS-METRICS | Step 56 验收 | **ACCEPTED** |
| 2026-09-21 | Cursor | WRITE-DATA-PARALLEL + DOC-SWEEP | 选定 Step 57 | **DECIDED** |
| 2026-09-21 | Codex | WRITE-DATA-PARALLEL + DOC-SWEEP | 实现与自检完成 | **REVIEW**；ABI v25；209 tests |
| 2026-09-21 | Cursor | WRITE-DATA-PARALLEL + DOC-SWEEP | Step 57 验收 | **ACCEPTED**；209 tests + `STEP57_WRITE_PARALLEL_PASS` peak=2 |
| 2026-09-21 | Cursor | META-MUTATION-PARALLEL + FAILCLOSED-TEST | 选定 Step 58 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push；测试只写 `tests/`。
- Step 58 不做完整 io_uring 用户态导出、跨机锁、或过夜级扩包。

## 8. 当前 Codex 提示词（Step 58，常规双包 ≈ 2×）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md、docs/remaining-capabilities.md（§2/§6/§8）、docs/configuration.md、
docs/phase4-nvme-cache.md、tests/README.md。HEAD 应含 Step 57（ABI v25）。
注意：§8 为常规双包（≈ 2×）。新测试只写入 tests/。

开工时：两项均 → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 内核/mount 验证只在 vng guest + loop；禁止宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 改 kestrelfs/*.c 或依赖 mount：必须 vng
- 新脚本/C helper 只放 tests/；source tests/_repo_root.sh
- 默认 cargo test 可不依赖外部服务

## 目标：Step 58 — META-MUTATION-PARALLEL + FAILCLOSED-TEST（双包）

### A) META-MUTATION-PARALLEL
针对 Step 57 已知瓶颈：FileMetaStore 用全局 mutation mutex 串行化并发 append 与
`meta.tmp` 原子替换，限制了 write-lane 并行收益。
1. 允许**不同 inode** 的 metadata mutation 重叠推进（Mem 与 File 至少覆盖；Redis 若已
   由 Lua CAS 天然可并行则补测锁死，勿无 Redis session TTL 猜生命周期）
2. 同一 inode 内 create/write/truncate/unlink/rename 等仍保持正确顺序与耐久
3. FileMetaStore 的 `meta.json` 提交必须保持 crash-safe（tmp+fsync+rename 或等价）；
   损坏/半提交 fail closed，不得静默丢 mutation
4. 可观测：至少一项计数/日志证明不同 inode 的 metadata 路径重叠
5. vng：STEP58_META_PARALLEL_*_PASS；回归 Step 57 write-parallel 与 Step 51 write-behind

### B) FAILCLOSED-TEST
1. 新增或加强自动化回归：至少覆盖两类 fail-closed
   (i) FileMetaStore 状态损坏/半提交 → 启动或操作拒绝，不假装健康
   (ii) write-lane / ABI 不匹配或非法 lane → 明确错误，不越界
2. vng 或 cargo 门控输出 STEP58_FAILCLOSED_*_PASS
3. 同步 HANDOFF 已知限制与 configuration 中相关表述

## 要求
1. make -C kestrelfs 零警告；cargo test + clippy -D warnings 不回归
2. STEP58_*_PASS 覆盖 A+B；回归 Step 57 WRITE_PARALLEL
3. ABI：无布局变化可保持 v25；有变化须 bump
4. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
5. 不要擅自 commit/push；不要在仓库根新增 test-*

## 明确不做
完整 io_uring 用户态导出、跨机分布式锁、Prometheus、自动 wipe、过夜级扩包、
READ_DATA lane 全集（除非实现 A 时极小附带且论证必要）。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + ./tests/test-step58-* vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-21 — Step 57 WRITE-DATA-PARALLEL + DOC-SWEEP（Cursor ACCEPTED）

- ABI **v25**：8×16 KiB write lane；`WRITE_DATA_PARALLEL`；SHM 278720；同 inode 有序。
- 验证：209 tests；clippy / make 干净；
  `STEP57_WRITE_PARALLEL_PASS`（peak=2，umount_ms=18）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-21 — Step 57 WRITE-DATA-PARALLEL + DOC-SWEEP（Codex REVIEW）

- 并行模型：共享区新增 8 lane；opcode 27；同 inode `write_data_lock`；daemon 并发
  write batch、按序响应；resp_claimed 修乱序领取；FileMetaStore mutation mutex 仍串行。
- 测试：209 passed；vng peak=2；回归 Step 51/50/54。DOC-SWEEP 同步 README/HANDOFF/
  phase4/configuration/tests。cache format v4 不变。

### 2026-09-21 — Step 56 ORPHAN-RETRY-PERSIST + OPS-METRICS（Cursor ACCEPTED）

- orphan proof 持久交接 + rmmod guard + `orphan_retry_*`；`STEP56_DOUBLE_PACK_PASS`。
