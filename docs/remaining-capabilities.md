# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-20，Cursor（Step 52 DIST-IO + TEST-PERF 已验收；发布 Step 53 双包）
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

- Phase 4 至 Step 52（含 DIST-IO 两节点闭环与 vng 粗测基线）均已验收。
- 已验收 IPC ABI **v22**，cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。
- 配置权威表：`docs/configuration.md`；粗测方法：`docs/perf-baseline.md`。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–28 | … + DIST-IO/TEST-PERF | Step 24–52 | **ACCEPTED** | 多节点最小闭环 + 粗测基线已落地 |
| 29 | DIST-NOTIFY + REDIS-HARDEN | Step 53（双包） | **DECIDED** | 收窄 probe 窗口 + Redis 连接原型硬化 |
| — | 其它增强 | — | PROPOSED | 视需要并入双包（完整 lease、io_uring、splice、orphan sweep） |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

读缓存、异步 hit、write-behind、可写 MAP_SHARED、DIST-IO 闭环已 ACCEPTED。

## 4. 内核 / 分布式

### Step 52 — DIST-IO + TEST-PERF（双包）

- 状态：`ACCEPTED`（Cursor，2026-09-20）
- 实现：两并行 vng guest + 共享 Redis/S3；`docs/perf-baseline.md`。

### Step 53 — DIST-NOTIFY + REDIS-HARDEN（双包）

- 状态：DIST-NOTIFY `DECIDED`；REDIS-HARDEN `DECIDED`
- 目标 A（DIST-NOTIFY）：在既有 dirty-inode / revision 模型上，为远端失效增加
  **推送通知**（Redis Pub/Sub 或等价轻量通道），使可见性不再只依赖约 100 ms
  poll；poll 仍作兜底；失败 fail closed；**不是**完整 lease/分布式锁。
- 目标 B（REDIS-HARDEN）：Redis 连接自动重连（或 connection manager）；可选
  `rediss://` TLS；同步 `docs/configuration.md`；不破坏现有 `redis://` 配方。
- 范围：见 §8。

## 5. 运维、测试与文档

配置权威表与 perf 基线已落地；本步硬化 Redis 运维面并写入配置表。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-20 | Cursor | MAP-SHARED-WRITE + WHITEOUT | Step 50 验收 | **ACCEPTED** |
| 2026-09-20 | Cursor | WRITE-BEHIND + OPS/DOC | 选定 Step 51 | **DECIDED** |
| 2026-09-20 | Codex | WRITE-BEHIND + OPS/DOC | 实现与自检 | **REVIEW** |
| 2026-09-20 | Cursor | WRITE-BEHIND + OPS/DOC | Step 51 验收 | **ACCEPTED**；200 tests + Cursor vng PASS |
| 2026-09-20 | Cursor | DIST-IO + TEST-PERF | 选定 Step 52 | **DECIDED**；见 §8 |
| 2026-09-20 | Codex | DIST-IO + TEST-PERF | Step 52 双包开工 | **IMPLEMENTING**；两并行 vng guest + 共享 Redis/S3，另建可重复 vng 粗测基线 |
| 2026-09-20 | Codex | DIST-IO + TEST-PERF | 实现与自检完成 | **REVIEW**；双 guest Redis+S3 闭环、vng perf 基线及 Step 51 回归通过，见 §9 |
| 2026-09-20 | Cursor | DIST-IO + TEST-PERF | Step 52 验收 | **ACCEPTED**；200 tests + Cursor `STEP52_DIST_TWO_NODE_PASS` / `STEP52_PERF_PASS` |
| 2026-09-20 | Cursor | DIST-NOTIFY + REDIS-HARDEN | 选定 Step 53 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 53 不顺手做完整 inode/range lease 协议、跨机分布式锁、io_uring 用户态导出、
  splice 全覆盖、orphan 自动全局 sweep，或宣称线性一致。

## 8. 当前 Codex 提示词（Step 53，双包 ≈ 2×）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md、docs/remaining-capabilities.md（§2/§6/§8）、docs/configuration.md、
docs/phase4-nvme-cache.md、docs/perf-baseline.md。HEAD 应含 Step 52（DIST-IO + TEST-PERF）。
注意：§8 为双包。

开工时：两项均 → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 一旦改 kestrelfs/*.c 或依赖 mount：必须 vng
- Redis 门控测本步需要；默认 cargo test 仍可不依赖外部服务
- Dist 场景继续用两并行 vng guest + 共享 Redis（S3 可沿用 Step 52 配方，若本步不改对象面可不强制）

## 目标：Step 53 — DIST-NOTIFY + REDIS-HARDEN（双包）

### A) DIST-NOTIFY（推送失效，收窄 probe 窗口）
在既有 durable revision + dirty-inode 日志之上：
1. mutation 提交成功后，向 Redis 发布轻量通知（Pub/Sub 或文档化的等价通道），
   携带足够信息让对端推进失效（至少 revision，最好含 dirty inode 集合摘要）
2. 订阅端收到通知后尽快执行与 poll 路径同等语义的失效（page-cache epoch /
   NVMe inode 退休 / 全量 fail-closed 回退规则不变）
3. 保留原有 ~100 ms poll 作兜底与对账；通知丢失/乱序不得比今天更不安全
4. vng：相对 Step 52，在同等负载下证明通知路径可将可见延迟压到明显低于一个
   完整 poll 周期（记录 STEP53_DIST_NOTIFY_*_PASS 与 latency 样本）；不宣称 lease
5. 失败 fail closed；不要实现完整 lease、fencing token、或跨机分布式字节锁

### B) REDIS-HARDEN（连接原型硬化）
1. 为 Redis MetaStore 增加断线自动重连（connection manager 或等价），短暂故障后
   请求可恢复，而不是永久卡在坏连接上必须重启 daemon
2. 支持可选 `rediss://` TLS（至少能在测试中用自签/测试证书跑通一条路径）；
   现有 `redis://` 配方必须继续可用
3. 更新 `docs/configuration.md`：URL 形态、TLS 相关参数/环境变量、重连语义与限制
4. 门控测 + 单元/集成测覆盖：重连后 mutation/coherence 仍正确；TLS 路径至少一条
5. 启动日志仍不得打印含凭据的完整 URL

## 要求
1. `make -C kestrelfs` 零警告；cargo test 不回归
2. STEP53_*_PASS 覆盖 A+B；Step 52 DIST/PERF 与 Step 51 write-behind 回归不破
3. ABI 无布局变化可保持 v22；有变化须 bump
4. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
5. 不要擅自 commit/push

## 明确不做
完整 lease/fencing、跨机强制锁、io_uring 用户态导出、splice 全覆盖、orphan 全局
sweep、自动 wipe、宣称线性一致或多 AZ 生产可用性。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + Step 53 vng（含通知延迟样本）
- Redis/TLS 门控测（环境允许时）
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-20 — Step 52 DIST-IO + TEST-PERF（Cursor ACCEPTED）

- 两并行 vng guest + 共享 Redis/S3 数据面闭环；`docs/perf-baseline.md`。
- 验证：200 tests；clippy / make 干净；Cursor 复跑
  `STEP52_PERF_PASS`（write-behind ~1324 MiB/s；page-cache ~2153 MiB/s；
  NVMe/loop hit ~178 MiB/s / async bios=256；umount_ms=23）与
  `STEP52_DIST_TWO_NODE_PASS`（visibility latency 35 ms；Redis/MinIO via
  `10.0.2.2`）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-20 — Step 52 DIST-IO + TEST-PERF（Codex REVIEW）

- 方案选择：采用两个并行 vng guest，而不是同 guest 双 mount。每个节点有独立
  daemon、挂载、data-dir、page cache 和 loop cache，共享随机 Redis prefix 与 S3
  prefix，因而真实覆盖跨 daemon revision/coherence；宿主编排器不执行 insmod/mount。
- DIST-IO：A 写版本 1，B 先缓存旧版本；A 覆写版本 2 并 fsync 后，B 的 durable
  dirty-inode probe 推进 page-cache epoch 并退休 1 个 NVMe entry。B 观察版本 2 后
  保留 fd、drop_caches、停止 daemon，仍从新 cache entry 读回版本 2。本轮可见延迟
  10 ms（轮询相位样本）；契约仍是约 100 ms 最终一致窗口，不是 lease/线性一致。
- TEST-PERF：新增 `test-step52-perf-vng.sh`/C helper 与 `docs/perf-baseline.md`。本轮
  vng 6.12.38 + 64 MiB loop 的 1 MiB 样本：write-behind 1.53 ms / 653.58 MiB/s，
  fsync 116.41 ms；page-cache 64 MiB 2104.47 MiB/s；daemon-free NVMe/loop hit
  6.30 ms / 158.62 MiB/s、256 async BIO。均为含校验的粗测，不是 SLA。
- ABI/format：无布局或 opcode 变化，IPC ABI **v22**、cache format **v4** 不变。
- 验证：`cargo test` 200 passed；clippy `-D warnings` 通过；`make -C kestrelfs`
  零警告；真实 Redis 与 MinIO 门控测试各 1 passed；vng 输出
  `STEP52_DIST_TWO_NODE_PASS`、`STEP52_PERF_PASS`；Step 51 回归输出
  `STEP51_WRITE_BEHIND_PASS`（umount 15 ms）。Step 42 fail-closed 回归在二次读前
  drop page cache 以适配 Step 45 后的 VFS 热读路径，输出
  `STEP42_FAILURE_FALLBACK_ALL_PASS` / `STEP42_COHERENCE_FINE_PASS`。
  所有模块、loop、mount 均只在 guest。
- 风险/未做：probe 前仍可能短暂旧读；Redis/S3 无跨后端事务，服务故障沿既有路径
  fail closed；未实现 lease/pubsub、分布式锁、io_uring、自动 wipe 或生产 benchmark。

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
