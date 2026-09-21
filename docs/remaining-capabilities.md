# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-21，Cursor（Step 56 已验收；发布 Step 57 常规双包）
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

- Phase 4 至 Step 56（orphan retry 持久交接 + rmmod guard + 最小运维指标）均已验收。
- 已验收 IPC ABI **v24**，cache format **v4**。
- 手工/vng 测试统一位于 `tests/`。
- 配置权威表：`docs/configuration.md`。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–32 | … + ORPHAN/METRICS | Step 24–56 | **ACCEPTED** | orphan 持久交接与观测已落地 |
| 33 | WRITE-DATA-PARALLEL + DOC-SWEEP | Step 57（双包） | **DECIDED** | 推进写回并行度并清理文档债务 |
| — | 其它 | — | PROPOSED | 视需要并入后续双包 |

Codex **只实现 §8 当前提示词**。

## 3–4. 摘要

Step 56 `ACCEPTED`。下一步见 §8。

## 5. 运维、测试与文档

`tests/README.md`；`docs/configuration.md`（含观测专节）。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-21 | Cursor | POSIX-DTYPE + MKNOD-MIN | Step 55 验收 | **ACCEPTED** |
| 2026-09-21 | Cursor | ORPHAN-RETRY-PERSIST + OPS-METRICS | 选定 Step 56 | **DECIDED** |
| 2026-09-21 | Codex | ORPHAN-RETRY-PERSIST + OPS-METRICS | 实现与自检完成 | **REVIEW**；208 tests |
| 2026-09-21 | Cursor | ORPHAN-RETRY-PERSIST + OPS-METRICS | Step 56 验收 | **ACCEPTED**；208 tests + `STEP56_DOUBLE_PACK_PASS` |
| 2026-09-21 | Cursor | WRITE-DATA-PARALLEL + DOC-SWEEP | 选定 Step 57 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push；测试只写 `tests/`。
- Step 57 不做完整用户态 io_uring 导出、跨机锁、或过夜级扩包。

## 8. 当前 Codex 提示词（Step 57，常规双包 ≈ 2×）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md、docs/remaining-capabilities.md（§2/§6/§8）、docs/configuration.md、
docs/phase4-nvme-cache.md、tests/README.md。HEAD 应含 Step 56（ABI v24）。
注意：§8 为常规双包（≈ 2×）。新测试只写入 tests/。

开工时：两项均 → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 内核/mount 验证只在 vng guest + loop；禁止宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 改 kestrelfs/*.c 或依赖 mount：必须 vng
- 新脚本/C helper 只放 tests/；source tests/_repo_root.sh
- 默认 cargo test 可不依赖外部服务

## 目标：Step 57 — WRITE-DATA-PARALLEL + DOC-SWEEP（双包）

### A) WRITE-DATA-PARALLEL
针对 HANDOFF 已知限制：Step 54 已把 folio→staging 移出 bounce mutex，但同步
WRITE_DATA 仍全局单 in-flight。
1. 允许**不同 inode**（或论证后的更细粒度）的 WRITE_DATA 重叠提交/等待；
   同一 inode 内仍保持正确的 pagecache/NVMe 失效与耐久顺序
2. 保持 fsync/MS_SYNC/syncfs/coherence-before-invalidate 的 fail-closed 语义
3. 可观测：新增或扩展计数证明并行度（例如 concurrent submissions peak / 重叠 hold）
4. vng：STEP57_WRITE_PARALLEL_*_PASS；回归 Step 51 write-behind、Step 54 write_pipe、
   Step 50 MAP_SHARED
5. 不要引入用户态异步 API / io_uring 导出

### B) DOC-SWEEP
1. 清理 HANDOFF §11 文档债务中可立即对齐的条目（过时路径、ABI/shm 尺寸、测试路径等）
2. 同步 README / phase4 / configuration 中与 Step 54–56 不一致的表述
3. 不借机扩功能；纯文档修正也须在 §9 列出改动清单

## 要求
1. make -C kestrelfs 零警告；cargo test + clippy -D warnings 不回归
2. STEP57_*_PASS 覆盖 A；B 以文档 diff 与 §9 清单验收
3. ABI 无布局变化可保持 v24；有变化须 bump
4. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
5. 不要擅自 commit/push；不要在仓库根新增 test-*

## 明确不做
完整 io_uring 用户态导出、跨机分布式锁、Prometheus、自动 wipe、过夜级扩包。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + ./tests/test-step57-* vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-21 — Step 56 ORPHAN-RETRY-PERSIST + OPS-METRICS（Cursor ACCEPTED）

- daemon `data_dir` 原子持久 orphan proof；未 ACK 时模块引用阻止正常 rmmod；
  `orphan_retry_{queued,acked,pending}` + configuration 观测专节。
- 验证：208 tests；clippy / make 干净；
  `STEP56_DOUBLE_PACK_PASS`（open-skip、rmmod-guard、metrics delta=1、reload-GC）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-21 — Step 56 ORPHAN-RETRY-PERSIST + OPS-METRICS（Codex REVIEW）

- A / 持久 retry：daemon `data_dir` versioned 集合；内核 proof + 模块引用；
  temp fsync→rename→dir fsync 后 ACK；启动/每秒回放。
- B / 运维观测：`orphan_retry_*` 与 configuration 专节。
- 测试：208 passed；`STEP56_DOUBLE_PACK_PASS`；回归 Step 54/55。ABI v24 不变。

### 2026-09-21 — Step 55 POSIX-DTYPE + MKNOD-MIN（Cursor ACCEPTED）

- ABI v24；`STEP55_POSIX_DTYPE_MKNOD_PASS`；测试迁入 `tests/`。
