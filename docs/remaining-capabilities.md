# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-15，Codex（Step 30 DIST-GC 实现完成，待 Cursor 验收）
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

- Phase 1–3 控制面原型已完成。
- Phase 4 Step 18–29 已由 Cursor 验收（含 batch eviction）。
- IPC ABI **v11**，cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor 已调整）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–5 | Phase 4 cache 主线 | Step 24–29 | **ACCEPTED** | 命中路径与运维最小闭环 |
| 6 | DIST-GC | Step 30 持久化 GC 重试 | **REVIEW** | 工作树已实现，等待 Cursor 对照代码与测试验收 |
| 7 | DIST-META | Redis 拆 key / TLS / 重连 | PROPOSED | 去掉全量快照瓶颈 |
| 8 | POSIX-CORE | hard link、open-unlink、rename flags | PROPOSED | 语义补齐 |
| 9 | CACHE-COHERENCE | 多节点失效 | PROPOSED | 共享部署正确性 |
| — | CACHE-WRITE | 写缓存 | **DEFERRED** | 默认只读 fill cache |
| — | ASYNC-COMPLETION / TEST-PERF / DOC | 其它 | PROPOSED | 低于本步 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

### CACHE-EVICT — Step 29

- 状态：`ACCEPTED`（Cursor，2026-09-15）
- 实现：`cache_evict_batch` 默认 16；journal reserved 内 batch；同页合并清零；
  半提交整批恢复为 miss；单批 ≤ 总槽位 1/16。

### CACHE-WRITE / COHERENCE

状态同 §2。

## 4. 控制面、对象存储与 POSIX

### DIST-GC — Step 30

- 状态：`REVIEW`
- 实现：GC 候选与 metadata mutation 同快照提交；启动立即重放，运行期 1–60 秒
  指数退避；delete 成功后才确认出队。
- 范围：见 §8。

### DIST-META / DIST-OBJECT / POSIX-CORE / IPC-SCALE

仍为 `PROPOSED`。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-15 | Cursor | CACHE-VFS | Step 28 验收 | **ACCEPTED** |
| 2026-09-15 | Cursor | CACHE-EVICT | 选定 Step 29 | **DECIDED** |
| 2026-09-15 | Codex | CACHE-EVICT | 实现与自检 | **REVIEW** |
| 2026-09-15 | Cursor | CACHE-EVICT | Step 29 验收 | **ACCEPTED**；v4 reserved batch 扩展认可 |
| 2026-09-15 | Cursor | DIST-GC | 选定 Step 30 | **DECIDED**；见 §8 |
| 2026-09-15 | Codex | DIST-GC | Step 30 开工 | **IMPLEMENTING**；仅执行 §8 |
| 2026-09-15 | Codex | DIST-GC | 实现与自检完成 | **REVIEW**；详见 §9 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 30 不顺手做 Redis 拆 key、S3 全量 reconciliation、write-back 或多节点失效。

## 8. 当前 Codex 提示词（Step 30）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
HEAD 应含 Step 29（batch eviction；format v4；ABI v11）。

开工时：DIST-GC → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证仍只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 若本步纯 daemon 单测为主，可跳过 insmod；一旦改 kestrelfs/*.c 或依赖 mount，必须 vng

## 目标：Phase 4/控制面 Step 30 — DIST-GC（持久化对象回收重试）
在保持「metadata 先提交、不因 GC 失败回滚」前提下，避免 ObjectStore delete 失败
造成永久泄漏。

要求：
1. 为 GC 候选（unlink / rename-overwrite / truncate 丢弃的无引用 block keys）提供
   持久化 delete queue（落盘或与 FileMetaStore 同目录；Redis/S3 组合也要说清）
2. 启动与运行中重试：幂等 delete；指数退避或可配置间隔；可观测（日志/计数）
3. 正确性：仍被引用的 key 不得删；重复 GC 安全；daemon 崩溃后队列不丢
4. LocalFs 与 Mem 必须覆盖；S3 门控测可选但欢迎
5. IPC ABI 尽量保持 v11；不要为 GC 硬塞新内核 opcode（除非论证必须）
6. 单测 +（若有 mount 路径）STEP30_*_PASS / 扩展 Step 15 GC vng
7. 更新 HANDOFF（待验收）、相关 docs、README（保持中文）；
   本文 DIST-GC → REVIEW，§9 追加汇报
8. 不要擅自 commit/push

## 明确不做
Redis 拆 key / TLS 大重构、完整跨后端事务、自动 wipe、write-back、多节点 cache 失效、
批量 eviction 再大改。

## 验收自检
- make -C kestrelfs（若改内核）/ make -C tools：零警告
- cargo test + clippy -D warnings
- 相关 vng / 单测 PASS
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-15 — Step 30 DIST-GC（Codex REVIEW）

- **事务设计**：在 `MemStoreInner` / `MetaSnapshot` 增加 `pending_garbage`。unlink、
  rename-overwrite、truncate 先对全量现存 slice 做引用确认，再在同一个 MetaStore
  mutation 内入队。FileMetaStore 的 namespace 改动与队列经同一次 tmp + fsync +
  rename 落入 `meta.json`；RedisMetaStore 则经同一次 `{prefix}:meta:v1` Lua CAS 发布。
  这比 metadata commit 后另写队列消除了 crash-before-enqueue 窗口，旧 JSON/Redis
  snapshot 通过 `serde(default)` 兼容为空队列。
- **删除协议**：ObjectStore delete 保持幂等；每轮重新检查 queued key 当前确实无
  slice 引用，只确认成功删除的 key。delete 后、ack 前崩溃会在重启后重复 delete，
  不会丢队列或返回旧数据。namespace mutation 已成功时，GC 故障仍只记日志、不向
  VFS 伪报失败。
- **恢复与退避**：daemon 启动在进入 IPC loop 前立即重放；运行期由同一同步 event
  loop 的 poll timeout 重试，失败从 1 秒指数退避到 60 秒，新请求活动重置到 1 秒。
  没有并发 FileMetaStore writer。日志带单轮与进程累计 attempted/deleted/failures。
- **后端组合**：FileMeta + LocalFs/S3 的队列在 `meta.json`；Redis + LocalFs/S3 的
  队列在 Redis snapshot。Redis+S3 多 daemon 可能重复 delete/ack，但二者均幂等且
  ack 使用 CAS。`--memory` 的 queue 与 MemObjectStore 一样只在进程内，重启时对象
  与队列同时消失。
- **测试**：`cargo test --manifest-path daemon/Cargo.toml` 为 **141 passed**；新增
  Mem delete 故障→保留→重试、共享 key 最后引用保护、FileMeta commit 后模拟 crash
  →重启向 LocalFs 重放、持久 ack。`cargo clippy --manifest-path daemon/Cargo.toml
  --all-targets -- -D warnings` 通过；`make -C tools` 零警告。真实 Redis 门控测试
  **1 passed**（含 queue 重启/ack），MinIO/S3 门控测试 **2 passed**（含 GC delete）。
  本步未改内核 C/IPC/mount，按 §8 未运行 vng，也未在物理机执行 insmod/mount。
- **ABI/format**：IPC ABI 保持 **v11**；cache format 保持 **v4**。
- **已知限制**：永久 ObjectStore 故障会让队列持续增长，尚无容量上限、dead-letter
  或管理接口；引用确认与待删读取仍为 O(全量 slice)；Redis 入队/ack 仍重写整个
  snapshot；同步 event loop 中的远端 delete/retry 可能增加 IPC 尾延迟。未实现
  Redis 拆 key/TLS/重连、全量 S3 reconciliation、跨 Redis/S3 原子事务、自动 wipe、
  write-back、多节点 cache 失效或 eviction 再改。

### 2026-09-15 — Step 29 CACHE-EVICT（Cursor ACCEPTED）

- batch journal in v4 reserved；同页合并清零；`STEP29_CACHE_EVICT_PASS`。
- Commit：随 Cursor 本轮验收推送（见 `git log -1`）。

### 2026-09-15 — Step 28 CACHE-VFS（Cursor ACCEPTED）

- `read_iter` / iov_iter；commit `746e731` 等。
