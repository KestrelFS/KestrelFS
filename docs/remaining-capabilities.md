# KestrelFS 剩余能力与决策同步

> 最后更新：2026-10-08，Cursor（Step 58 已验收；发布 Step 59 常规双包）
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

- Phase 4 至 Step 58（FileMetaStore 持久阶段并行 + fail-closed 回归）均已验收。
- 已验收 IPC ABI **v25**，cache format **v4**，SHM **278720**。
- 手工/vng 测试统一位于 `tests/`。
- 配置权威表：`docs/configuration.md`。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–34 | … + META-MUTATION-PARALLEL | Step 24–58 | **ACCEPTED** | 写 lane + FileMeta 并行已落地 |
| 35 | READ-DATA-PARALLEL + DOC-SWEEP | Step 59（双包） | **DECIDED** | 对称放开不同 inode 的 READ_DATA 重叠 |
| — | 其它 | — | PROPOSED | 视需要并入后续双包 |

Codex **只实现 §8 当前提示词**。

## 3–4. 摘要

Step 58 `ACCEPTED`。下一步见 §8。

## 5. 运维、测试与文档

`tests/README.md`；`docs/configuration.md`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-21 | Cursor | WRITE-DATA-PARALLEL + DOC-SWEEP | Step 57 验收 | **ACCEPTED** |
| 2026-09-21 | Cursor | META-MUTATION-PARALLEL + FAILCLOSED-TEST | 选定 Step 58 | **DECIDED** |
| 2026-10-08 | Codex | META-MUTATION-PARALLEL + FAILCLOSED-TEST | 实现与自检完成 | **REVIEW**；214 tests |
| 2026-10-08 | Cursor | META-MUTATION-PARALLEL + FAILCLOSED-TEST | Step 58 验收 | **ACCEPTED**；214 tests + META/FAILCLOSED PASS |
| 2026-10-08 | Cursor | READ-DATA-PARALLEL + DOC-SWEEP | 选定 Step 59 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push；测试只写 `tests/`。
- Step 59 不做完整 io_uring 用户态导出、跨机锁、或过夜级扩包。

## 8. 当前 Codex 提示词（Step 59，常规双包 ≈ 2×）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md、docs/remaining-capabilities.md（§2/§6/§8）、docs/configuration.md、
docs/phase4-nvme-cache.md、tests/README.md。HEAD 应含 Step 58（ABI v25）。
注意：§8 为常规双包（≈ 2×）。新测试只写入 tests/。

开工时：两项均 → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 内核/mount 验证只在 vng guest + loop；禁止宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 改 kestrelfs/*.c 或依赖 mount：必须 vng
- 新脚本/C helper 只放 tests/；source tests/_repo_root.sh
- 默认 cargo test 可不依赖外部服务

## 目标：Step 59 — READ-DATA-PARALLEL + DOC-SWEEP（双包）

### A) READ-DATA-PARALLEL
对称 Step 57 的写 lane：当前 READ_DATA 仍共用单一 bounce/data buffer，限制不同
inode 冷读重叠。
1. 允许**不同 inode** 的 READ_DATA（或等价 miss 填充路径）重叠提交/等待
2. 同 inode 内保持正确填充/失效顺序；与 page-cache / NVMe cache / coherence 语义兼容
3. 方案择优并在 §9 论证：复用/扩展 write lane 模型、独立 read lane pool，或证明
   更小改动即可安全重叠；非法 lane/越界必须 fail closed
4. 可观测：active/peak（或等价）证明并行度
5. vng：STEP59_READ_PARALLEL_*_PASS；回归 Step 57 WRITE_PARALLEL、Step 45/48 热/冷读路径
6. 不要引入用户态异步 API / io_uring 导出

### B) DOC-SWEEP
1. 同步 README / HANDOFF / phase4 / configuration 中与 Step 57–58 及本步不一致的表述
2. 更新 SHM/ABI/lane 相关描述；清理过时 bounce-only 说法
3. 不借机扩功能；§9 列出文档改动清单

## 要求
1. make -C kestrelfs 零警告；cargo test + clippy -D warnings 不回归
2. STEP59_*_PASS 覆盖 A；B 以文档 diff + §9 清单验收
3. ABI：有布局/opcode 变化须 bump；无则保持 v25；format 同理
4. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
5. 不要擅自 commit/push；不要在仓库根新增 test-*

## 明确不做
完整 io_uring 用户态导出、跨机锁、Prometheus、自动 wipe、过夜级扩包、FileMeta WAL/分片。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + ./tests/test-step59-* vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-10-08 — Step 58 META-MUTATION-PARALLEL + FAILCLOSED-TEST（Cursor ACCEPTED）

- FileMetaStore：prepare 短串行 + JSON/temp/fsync 并行 + 单调 publish；损坏/半提交 fail closed。
- 验证：214 tests；clippy / make 干净；`STEP58_FAILCLOSED_PASS`；
  Redis + `STEP58_META_REDIS_CAS_CONCURRENT_PASS`；
  `STEP58_META_PARALLEL_PASS`（lane_peak=2，umount 12 ms）。
- ABI **v25** / format **v4** 未变。
- Commit：随 Cursor 本轮验收推送。

### 2026-10-08 — Step 58 META-MUTATION-PARALLEL + FAILCLOSED-TEST（Codex REVIEW）

- FileMetaStore 持久阶段并行；Redis Lua CAS 并发门控；corrupt/half-commit/lane fail closed。
- 214 tests；vng overlap peak≥2；回归 Step 57/51。未做 WAL/分片/io_uring。

### 2026-09-21 — Step 57 WRITE-DATA-PARALLEL + DOC-SWEEP（Cursor ACCEPTED）

- ABI v25 write lane；`STEP57_WRITE_PARALLEL_PASS` peak=2。
