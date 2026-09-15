# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-15，Cursor（验收 Step 31；选定 Step 32 = POSIX-CORE）
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

- Phase 4 cache Step 18–29、DIST-GC Step 30、DIST-META Step 31 均已验收。
- Redis metadata 为 v2 分记录 HASH/SET + Lua revision-CAS；点查定向读。
- IPC ABI **v11**，cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor 已调整）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–7 | cache + DIST-GC + DIST-META | Step 24–31 | **ACCEPTED** | 控制面 Redis 可扩展起步已落地 |
| 8 | POSIX-CORE | Step 32 主要 POSIX 语义缺口 | **DECIDED** | hard link / open-unlink / rename flags / mode·nlink |
| 9 | CACHE-COHERENCE | 多节点失效 | PROPOSED | 共享部署正确性 |
| — | CACHE-WRITE | 写缓存 | **DEFERRED** | 默认只读 fill cache |
| — | DIST-OBJECT / TLS-RECONNECT / ASYNC | 其它 | PROPOSED | 低于本步；TLS 可并入后续 DIST 运维步 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

CACHE-* 主线已 ACCEPTED；`CACHE-WRITE` 仍 DEFERRED；`CACHE-COHERENCE` 待后。

## 4. 控制面、对象存储与 POSIX

### DIST-META — Step 31

- 状态：`ACCEPTED`（Cursor，2026-09-15）
- 实现：v2 schema；Lua revision-CAS；v1 fail closed；无自动迁移。
- 已知限制：mutation 仍聚合读；无 `rediss://`/自动重连。

### POSIX-CORE — Step 32

- 状态：`DECIDED`
- 缺口：hard link；open-unlink 延迟回收；`RENAME_NOREPLACE` / `EXCHANGE` / `WHITEOUT`；
  `create()` 忽略 mode；目录 `nlink` 固定为 2；symlink 非任意字节。
- 范围：见 §8（本步允许择优子集，但必须端到端可测）。

### DIST-OBJECT / IPC-SCALE

仍为 `PROPOSED`。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-15 | Cursor | DIST-GC | Step 30 验收 | **ACCEPTED** |
| 2026-09-15 | Cursor | DIST-META | 选定 Step 31 | **DECIDED** |
| 2026-09-15 | Codex | DIST-META | 实现与自检 | **REVIEW** |
| 2026-09-15 | Cursor | DIST-META | Step 31 验收 | **ACCEPTED**；拆 key 优先于 TLS 的选择认可 |
| 2026-09-15 | Cursor | POSIX-CORE | 选定 Step 32 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 32 不顺手做 write-back、多节点 cache 失效、Redis TLS 大工程。

## 8. 当前 Codex 提示词（Step 32）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
HEAD 应含 Step 31（Redis meta v2；ABI v11）。

开工时：POSIX-CORE → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 纯 MetaStore 语义可用 cargo test；一旦改 kestrelfs/*.c 必须 vng
- Redis/S3 门控测可选；默认 cargo test 不依赖外部服务

## 目标：Step 32 — POSIX-CORE（主要语义缺口，允许子集）
补齐最影响兼容性的 POSIX 缺口。本步不必一次做完列表全部项，但必须交付
**至少一个端到端可测的完整能力**，并在 §9 写清做了什么、刻意未做什么。

优先推荐（按价值排序，择 1–2 项深入做完）：
A) hard link：MetaStore nlink + 多 dirent 同 inode；unlink 末引用才 GC；内核 .link
B) open-unlink：已打开 fd 在 unlink 后仍可读写，最后 close 才回收（需慎重设计 inode 生命周期）
C) rename flags：至少 `RENAME_NOREPLACE`；`EXCHANGE`/`WHITEOUT` 可选
D) create/mkdir 尊重 mode；目录 nlink 随子目录正确变化

要求：
1. MemStore + FileMetaStore 必须正确；Redis v2 若本步触及 mutation，须保持原子性与 GC queue
2. 若需新 ABI opcode / payload：bump ABI，C/Rust 同步，编译期断言
3. 单测 +（若有内核路径）STEP32_*_PASS vng
4. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
5. 不要擅自 commit/push

## 明确不做
write-back cache、多节点失效、Redis TLS/重连大重构、自动 wipe、完整 iget5 重设计
（除非 B 项论证必须且能证明不引入历史 umount 死锁）。

## 验收自检
- cargo test + clippy -D warnings
- 若改内核：make -C kestrelfs 零警告 + 相关 vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-15 — Step 31 DIST-META（Cursor ACCEPTED）

- Redis schema v2 分记录 + Lua revision-CAS；144 tests；v1 fail closed。
- Commit：随 Cursor 本轮验收推送（见 `git log -1`）。

### 2026-09-15 — Step 30 DIST-GC（Cursor ACCEPTED）

- `pending_garbage`；commit `be89352` 等。
