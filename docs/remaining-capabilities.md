# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-15，Cursor（验收 Step 34；选定 Step 35 = CACHE-COHERENCE）
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

- Phase 4 cache、DIST、硬链接、`RENAME_NOREPLACE`、mode/目录 nlink（Step 24–34）均已验收。
- Redis metadata 为 v2 分记录 HASH/SET + Lua revision-CAS；点查定向读。
- IPC ABI **v13**，cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor 已调整）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–10 | cache + DIST + POSIX 子集 | Step 24–34 | **ACCEPTED** | 单节点 POSIX 主缺口已明显收窄 |
| 11 | CACHE-COHERENCE | Step 35 多节点/远端失效 | **DECIDED** | 共享 MetaStore 部署下的正确性门槛 |
| 12 | POSIX-LIFECYCLE | open-unlink | PROPOSED | 价值高但 inode 生命周期风险更大 |
| — | RENAME_EXCHANGE / WHITEOUT / chmod | 其余 POSIX | PROPOSED | 可后补 |
| — | CACHE-WRITE | 写缓存 | **DEFERRED** | 默认只读 fill cache |
| — | DIST-OBJECT / TLS-RECONNECT / ASYNC | 其它 | PROPOSED | 低于本步 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

CACHE-* 主线已 ACCEPTED；`CACHE-WRITE` 仍 DEFERRED；本步推进 `CACHE-COHERENCE`。

## 4. 控制面、对象存储与 POSIX

### POSIX-ATTR — Step 34

- 状态：`ACCEPTED`（Cursor，2026-09-15）
- 实现：create/mkdir 持久化 `0o7777`；目录 nlink=`2+子目录数`；getattr 刷新。
- 未做：chmod/chown、open-unlink、EXCHANGE/WHITEOUT。

### CACHE-COHERENCE — Step 35

- 状态：`DECIDED`
- 缺口：本机只观察本 VFS mutation；其他节点或直接 Redis mutation 不会通知本内核失效。
- 范围：见 §8（允许最小可测闭环，不必一次做完生产级 pub/sub）。

### open-unlink / DIST-OBJECT / IPC-SCALE

仍为 `PROPOSED`。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-15 | Cursor | POSIX-RENAME | Step 33 验收 | **ACCEPTED** |
| 2026-09-15 | Cursor | POSIX-ATTR | 选定 Step 34 | **DECIDED** |
| 2026-09-15 | Codex | POSIX-ATTR | 实现与自检 | **REVIEW** |
| 2026-09-15 | Cursor | POSIX-ATTR | Step 34 验收 | **ACCEPTED**；162 tests + vng PASS |
| 2026-09-15 | Cursor | CACHE-COHERENCE | 选定 Step 35 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 35 不顺手做 write-back、open-unlink、完整分布式锁、Redis TLS 大工程。

## 8. 当前 Codex 提示词（Step 35）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）以及
docs/phase4-nvme-cache.md 中与 namespace / invalidation 相关的章节。
HEAD 应含 Step 34（ABI v13；mode + 目录 nlink）。

开工时：CACHE-COHERENCE → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 一旦改 kestrelfs/*.c 或依赖 mount/cache：必须 vng
- Redis/S3 门控测可选；默认 cargo test 不依赖外部服务

## 目标：Step 35 — CACHE-COHERENCE（最小可测闭环）
让“远端 metadata mutation”能够使本机 NVMe cache 安全 miss，避免读到过期数据。

最低可接受交付（择一完整做通，并在 §9 写清选型）：
A) **daemon 驱动失效**：本机 daemon 观察到（或被通知到）某 inode/范围变更后，
   通过既有或新增 IPC 通知内核 `cache_invalidate_*`；
   至少覆盖 truncate / overwrite-write / unlink 最终回收 三类之一的端到端证明。
B) **共享 MetaStore 世代/版本探测**：内核 miss 或周期路径对照 MetaStore 的
   inode generation / content epoch；过期则失效。要求可测且 fail-closed。

要求：
1. 同 namespace 下至少构造“写侧变更 + 读侧曾命中”的 A/B：变更后不得继续脏命中
2. 不破坏 Step 21 namespace identity；不自动 wipe；旧 cache 半提交仍安全 miss
3. 若需新 ABI：bump 版本，C/Rust 同步，编译期断言
4. 单测（能测的）+ STEP35_*_PASS vng（guest+loop，显式 insmod）
5. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
6. 不要擅自 commit/push

## 明确不做
完整多节点生产 pub/sub 运维、write-back、open-unlink、chmod、EXCHANGE/WHITEOUT、
Redis TLS 大重构、自动 wipe、iget5 重设计。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + 相关 vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-15 — Step 34 POSIX-ATTR（Cursor ACCEPTED）

- create/mkdir 持久化 mode；目录 nlink=`2+子目录数`；getattr 刷新。
- 验证：162 tests；clippy / `make -C kestrelfs` 干净；Cursor 复跑 vng
  `STEP34_POSIX_ATTR_PASS`（umount_ms=25）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-15 — Step 33 POSIX-RENAME（Cursor ACCEPTED）

- ABI v13 `RENAME_NOREPLACE`；commit `27a15ae`。
