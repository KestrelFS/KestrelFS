# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-20，Cursor（验收 Step 51；选定 Step 52 = DIST-IO + TEST-PERF；**后续 §8 体量约翻倍**）
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
> **体量约定（2026-09-20）**：自 Step 50 起，每个 §8 提示词按约 **2× 既往单步** 打包。

## 1. 当前基线

- Phase 4 至 Step 51（含 write-behind、权威配置表）均已验收。
- 已验收 IPC ABI **v22**，cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。
- 配置权威表：`docs/configuration.md`。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–27 | … + WRITE-BEHIND/OPS | Step 24–51 | **ACCEPTED** | write-behind 已落地 |
| 28 | DIST-IO + TEST-PERF | Step 52（双包） | **DECIDED** | 多节点最小闭环 + 性能基线 |
| — | 其它增强 | — | PROPOSED | 视需要并入双包 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

读缓存、异步 hit、write-behind、可写 MAP_SHARED 已 ACCEPTED。

## 4. 内核 VFS

### Step 51 — WRITE-BEHIND + OPS/DOC

- 状态：`ACCEPTED`（Cursor，2026-09-20）
- 实现：普通 write 可先返回；BDI/显式同步推进 writepages；`docs/configuration.md`。

### Step 52 — DIST-IO + TEST-PERF（双包）

- 状态：`DECIDED`
- 目标 A（DIST-IO）：两 daemon / 两挂载共享同一 Redis+对象后端时，写后读在既有
  revision/coherence 机制下可观察一致（最小分布式数据面闭环；可含小改进）。
- 目标 B（TEST-PERF）：可重复的 vng 粗测脚本，记录 write-behind / cache-hit /
  page-cache 热路径数量级，写入文档基线。
- 范围：见 §8。

## 5. 运维、测试与文档

配置权威表已落地；TEST-PERF 纳入 Step 52。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-20 | Cursor | MAP-SHARED-WRITE + WHITEOUT | Step 50 验收 | **ACCEPTED** |
| 2026-09-20 | Cursor | WRITE-BEHIND + OPS/DOC | 选定 Step 51 | **DECIDED** |
| 2026-09-20 | Codex | WRITE-BEHIND + OPS/DOC | 实现与自检 | **REVIEW** |
| 2026-09-20 | Cursor | WRITE-BEHIND + OPS/DOC | Step 51 验收 | **ACCEPTED**；200 tests + Cursor vng PASS |
| 2026-09-20 | Cursor | DIST-IO + TEST-PERF | 选定 Step 52 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 52 不顺手做完整 lease/pubsub 协议、用户态 io_uring、跨机强一致事务。

## 8. 当前 Codex 提示词（Step 52，双包 ≈ 2×）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md、docs/remaining-capabilities.md（§2/§6/§8）、docs/configuration.md、
docs/phase4-nvme-cache.md。HEAD 应含 Step 51（write-behind + configuration.md）。
注意：§8 为双包。

开工时：两项均 → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 一旦改 kestrelfs/*.c 或依赖 mount：必须 vng
- Redis 门控测本步 DIST-IO 需要；默认 cargo test 仍可不依赖外部服务

## 目标：Step 52 — DIST-IO + TEST-PERF（双包）

### A) DIST-IO（最小多节点数据面闭环）
在**同一 Redis MetaStore + 同一 ObjectStore** 上跑两个独立 daemon（可用两个
vng guest，或同一 guest 两个 mount/两个 namespace 绑定——择优并在 §9 论证）：
1. 节点 A 写入并 fsync；节点 B 在既有 revision/coherence 窗口后读到新数据
2. 明确记录：当前 ~100ms probe / page-cache epoch / NVMe invalidate 的行为是否
   足够；若不够，做**最小**改进（例如收紧 probe、扩大 dirty-inode 覆盖、或
   文档化必须 fsync+等待的配方），不要擅自上完整 lease
3. 失败 fail closed；不假装跨节点强一致
4. vng 脚本 STEP52_DIST_*_PASS

### B) TEST-PERF（可重复粗测基线）
1. 新增 vng 粗测脚本：至少覆盖 (i) write-behind 异步写吞吐/延迟数量级
   (ii) page-cache 热读 (iii) NVMe cache hit（drop_caches 后）
2. 结果写入 `docs/`（例如 `docs/perf-baseline.md`）与 §9；注明内核/loop/vng、
   非生产 benchmark
3. 不把数字写成 SLA；只作回归对照基线

## 要求
1. `make -C kestrelfs` 零警告；cargo test 不回归
2. STEP52_*_PASS 覆盖 A+B；Step 51 write-behind 回归不破
3. ABI 无布局变化可保持 v22；有变化须 bump
4. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
5. 不要擅自 commit/push；配置变更同步 `docs/configuration.md`

## 明确不做
完整 lease/pubsub、跨机分布式锁、io_uring 用户态导出、自动 wipe、宣称生产级 perf。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + Step 52 vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-20 — Step 51 WRITE-BEHIND + OPS/DOC（Cursor ACCEPTED）

- 普通 write 可先返回；BDI/显式同步推进 writepages；coherence worker 先写回再退休；
  `docs/configuration.md` 权威配置表。
- 验证：200 tests；clippy / make 干净；Cursor 复跑 vng
  `STEP51_WRITE_BEHIND_PASS`（umount_ms=11；async write latency 0 ms）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-20 — Step 51 WRITE-BEHIND + OPS/DOC（Codex REVIEW）

- 去掉 write_iter 强制写回；durability 点仍同步；配置文档落地。
- 自检：200 tests；vng STEP51_*_PASS（Codex umount 14 ms）及多步回归。
- 未做：lease、io_uring、DIST-IO、跨 folio 异步 WRITE_DATA。

### 2026-09-20 — Step 50 MAP-SHARED-WRITE + WHITEOUT（Cursor ACCEPTED）

- ABI v22；Cursor vng `STEP50_DOUBLE_PACK_PASS`（umount_ms=11）。
