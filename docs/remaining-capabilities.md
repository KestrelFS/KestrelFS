# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-15，Cursor（验收 Step 30；选定 Step 31 = DIST-META）
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

- Phase 1–3 控制面原型已完成；Phase 4 cache Step 18–29 已验收。
- Step 30 DIST-GC 已验收：`pending_garbage` 与 meta mutation 同事务；启动/运行期重试。
- IPC ABI **v11**，cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor 已调整）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–6 | cache + DIST-GC | Step 24–30 | **ACCEPTED** | 命中路径、运维、对象回收重试 |
| 7 | DIST-META | Step 31 Redis 拆 key / 连接生产化 | **DECIDED** | 单 key 全量快照是规模瓶颈 |
| 8 | POSIX-CORE | hard link、open-unlink、rename flags | PROPOSED | 语义补齐 |
| 9 | CACHE-COHERENCE | 多节点失效 | PROPOSED | 共享部署正确性 |
| — | CACHE-WRITE | 写缓存 | **DEFERRED** | 默认只读 fill cache |
| — | DIST-OBJECT / ASYNC / TEST-PERF | 其它 | PROPOSED | 低于本步 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

CACHE-* 主线已 ACCEPTED；`CACHE-WRITE` 仍 DEFERRED。

## 4. 控制面、对象存储与 POSIX

### DIST-GC — Step 30

- 状态：`ACCEPTED`（Cursor，2026-09-15）
- 实现：`pending_garbage` 入队与 mutation 同快照；`pending_garbage()` 过滤仍引用 key；
  delete 后 `acknowledge_garbage`；启动立即重放 + 1–60s 退避。

### DIST-META — Step 31

- 状态：`DECIDED`
- 缺口：单 Redis key 全量 JSON；无 TLS/`rediss://`；无连接恢复。
- 范围：见 §8。

### DIST-OBJECT / POSIX-CORE / IPC-SCALE

仍为 `PROPOSED`。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-15 | Cursor | CACHE-EVICT | Step 29 验收 | **ACCEPTED** |
| 2026-09-15 | Cursor | DIST-GC | 选定 Step 30 | **DECIDED** |
| 2026-09-15 | Codex | DIST-GC | 实现与自检 | **REVIEW** |
| 2026-09-15 | Cursor | DIST-GC | Step 30 验收 | **ACCEPTED**；同事务队列 + 退避重试认可 |
| 2026-09-15 | Cursor | DIST-META | 选定 Step 31 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 31 不顺手做 write-back、多节点 cache 失效、完整 S3 reconciliation。

## 8. 当前 Codex 提示词（Step 31）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
HEAD 应含 Step 30（DIST-GC pending_garbage；ABI v11）。

开工时：DIST-META → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 纯 daemon 改动可用 cargo test；改内核/mount 才必须 vng+loop
- 禁止触碰宿主机 zvol；daemon 日志写 "$data_dir/daemon.log"；禁止 >/dev/null
- Redis 集成测保持门控（REDIS_URL）；默认 CI 不依赖本机 Redis
- 绝不在日志/文档打印含凭据的 Redis URL

## 目标：Step 31 — DIST-META（RedisMetaStore 生产化起步）
把单 key 全量快照原型推进到可扩展、可运维的最小可用形态。

要求（择优，但必须论证；勿一次做完所有）：
1. 默认优先：按 inode / dirent / slice（或等价）拆 key，并用 Lua/事务保持
   rename、inode 分配、truncate、GC queue 的原子性与 Step 30 语义
2. 或若拆 key 过大：先做连接生产化（rediss:// TLS、重连、超时、健康检查）+
   schema 版本字段，并写清为何拆 key 留后续——但 Cursor 更倾向本步至少落地拆 key
   的最小可用子集（例如 inode+dirent 拆分，slice 仍可暂存聚合，需论证）
3. 保留 MemStore/FileMetaStore；CLI `--meta` / `--redis-prefix` 行为清晰
4. 与 Step 30 `pending_garbage` 共存：入队/ack 不得丢队列或误删仍引用对象
5. 单测覆盖：rename 原子性、崩溃/重启后一致性、GC queue；可选 REDIS_URL 门控测
6. IPC ABI 尽量 v11；不改 cache format
7. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
8. 不要擅自 commit/push

## 明确不做
完整跨 Redis/S3 两阶段事务、write-back cache、多节点 cache 失效、自动 wipe、
POSIX hard link 大工程（留给 POSIX-CORE）。

## 验收自检
- cargo test（默认 无需 Redis）+ clippy -D warnings
- 若有 REDIS_URL：跑门控测并在 §9 记录
- make -C kestrelfs / tools（若未改内核可注明跳过理由）
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-15 — Step 30 DIST-GC（Cursor ACCEPTED）

- `pending_garbage` 同事务；启动/退避重试；141 tests。
- Commit：随 Cursor 本轮验收推送（见 `git log -1`）。

### 2026-09-15 — Step 29 CACHE-EVICT（Cursor ACCEPTED）

- batch journal；commit `bd5f36b` 等。
