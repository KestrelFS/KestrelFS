# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-15，Cursor（验收 Step 32 硬链接；选定 Step 33 = POSIX-RENAME）
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

- Phase 4 cache Step 18–29、DIST-GC/META Step 30–31、POSIX 硬链接 Step 32 均已验收。
- Redis metadata 为 v2 分记录 HASH/SET + Lua revision-CAS；点查定向读。
- IPC ABI **v12**（含 `LINK_DATA`），cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor 已调整）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–8 | cache + DIST + hard link | Step 24–32 | **ACCEPTED** | 硬链接端到端已落地 |
| 9 | POSIX-RENAME | Step 33 rename flags | **DECIDED** | 先 `RENAME_NOREPLACE`；EXCHANGE/WHITEOUT 可选 |
| 10 | POSIX-LIFECYCLE | open-unlink / mode·目录 nlink | PROPOSED | 价值高但 open-unlink 风险更大 |
| 11 | CACHE-COHERENCE | 多节点失效 | PROPOSED | 共享部署正确性 |
| — | CACHE-WRITE | 写缓存 | **DEFERRED** | 默认只读 fill cache |
| — | DIST-OBJECT / TLS-RECONNECT / ASYNC | 其它 | PROPOSED | 低于本步 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

CACHE-* 主线已 ACCEPTED；`CACHE-WRITE` 仍 DEFERRED；`CACHE-COHERENCE` 待后。

## 4. 控制面、对象存储与 POSIX

### POSIX-CORE / hard link — Step 32

- 状态：`ACCEPTED`（Cursor，2026-09-15）
- 实现：ABI v12 `LINK_DATA`；持久 nlink；末引用 GC；`iget_locked` 同挂载别名共享。
- 刻意未做：open-unlink、rename flags、create mode、目录 nlink、symlink 任意字节。

### POSIX-RENAME — Step 33

- 状态：`DECIDED`
- 目标：至少 `RENAME_NOREPLACE`（目标存在 → `EEXIST`）；`EXCHANGE` / `WHITEOUT` 可选。
- 范围：见 §8。

### DIST-OBJECT / IPC-SCALE / CACHE-COHERENCE

仍为 `PROPOSED`。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-15 | Cursor | DIST-META | Step 31 验收 | **ACCEPTED** |
| 2026-09-15 | Cursor | POSIX-CORE | 选定 Step 32 | **DECIDED** |
| 2026-09-15 | Codex | POSIX-CORE | 硬链接子集实现 | **REVIEW** |
| 2026-09-15 | Cursor | POSIX-CORE | Step 32 验收 | **ACCEPTED**；151 tests + vng PASS |
| 2026-09-15 | Cursor | POSIX-RENAME | 选定 Step 33 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 33 不顺手做 open-unlink、write-back、多节点失效、Redis TLS。

## 8. 当前 Codex 提示词（Step 33）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
HEAD 应含 Step 32（ABI v12 hard link）。

开工时：POSIX-RENAME → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 纯 MetaStore 语义可用 cargo test；一旦改 kestrelfs/*.c 必须 vng
- Redis/S3 门控测可选；默认 cargo test 不依赖外部服务

## 目标：Step 33 — POSIX-RENAME（rename flags）
补齐 rename 标志位语义。必须完成：

**必做**：`RENAME_NOREPLACE`
- 内核：`kestrelfs_inode_rename` 接受该 flag，不再对 flags!=0 一律 -EINVAL
- MetaStore：目标名已存在时返回 AlreadyExists / EEXIST，且不得覆盖、不得 GC
- 与既有硬链接语义兼容：同 inode 两别名互相 rename 仍为成功空操作（即使带 NOREPLACE）
- MemStore + FileMetaStore 必须正确；若触及 Redis mutation，保持 v2 原子性与 GC queue

**可选**（时间允许且能端到端测）：`RENAME_EXCHANGE` 和/或 `RENAME_WHITEOUT`
- 若做不全，§9 写清未做项；不要半截 ABI

要求：
1. 若需新 ABI 字段/opcode：bump ABI，C/Rust 同步，编译期断言；能在现有 RENAME_DATA 上扩展 flags 则优先不新 opcode
2. 单测覆盖 NOREPLACE 成功/失败；内核路径须 STEP33_*_PASS vng
3. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
4. 不要擅自 commit/push

## 明确不做
open-unlink、create/mkdir mode、目录 nlink、write-back、多节点失效、Redis TLS、
自动 wipe、iget5 重设计。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + 相关 vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-15 — Step 32 POSIX-CORE（Cursor ACCEPTED）

- 硬链接：ABI v12 `LINK_DATA`；持久 nlink；末引用 GC；`iget_locked`。
- 验证：151 tests；clippy 干净；`make -C kestrelfs` 零警告；Cursor 复跑 vng
  `STEP32_POSIX_CORE_PASS`（umount_ms=25）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-15 — Step 31 DIST-META（Cursor ACCEPTED）

- Redis schema v2；commit `53ae881` / docs `805da12`。
