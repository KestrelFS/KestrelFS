# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-15，Cursor（验收 Step 37；选定 Step 38 = POSIX-EXCHANGE）
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

- Phase 4 cache、DIST、POSIX（含 open-unlink/chmod）、CACHE-COHERENCE（Step 24–37）均已验收。
- Redis metadata 为 v2 分记录 HASH/SET + Lua revision-CAS；点查定向读。
- Redis daemon 每 100 ms 轮询 durable revision，经 ABI v14 全 cache 失效。
- IPC ABI **v16**（含 `OP_SETATTR`），cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor 已调整）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–13 | cache + DIST + POSIX + coherence | Step 24–37 | **ACCEPTED** | chmod 已落地 |
| 14 | POSIX-EXCHANGE | Step 38 `RENAME_EXCHANGE` | **DECIDED** | 补齐 rename flags 主缺口 |
| — | RENAME_WHITEOUT / chown / utimes | 其余 POSIX | PROPOSED | 可后补 |
| — | COHERENCE-FINE / DIST-OBJECT / ASYNC | 其它 | PROPOSED | 低于本步 |
| — | CACHE-WRITE | 写缓存 | **DEFERRED** | 默认只读 fill cache |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

CACHE-* 与 CACHE-COHERENCE 最小闭环已 ACCEPTED；`CACHE-WRITE` 仍 DEFERRED。

## 4. 控制面、对象存储与 POSIX

### POSIX-CHMOD — Step 37

- 状态：`ACCEPTED`（Cursor，2026-09-15）
- 实现：ABI v16 `OP_SETATTR`；持久 mode；orphan fchmod；chown/时间戳 `EOPNOTSUPP`。

### POSIX-EXCHANGE — Step 38

- 状态：`DECIDED`
- 目标：至少 `RENAME_EXCHANGE`；`WHITEOUT` 可选。
- 范围：见 §8。

### DIST-OBJECT / COHERENCE-FINE

仍为 `PROPOSED`。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-15 | Cursor | POSIX-LIFECYCLE | Step 36 验收 | **ACCEPTED** |
| 2026-09-15 | Cursor | POSIX-CHMOD | 选定 Step 37 | **DECIDED** |
| 2026-09-15 | Codex | POSIX-CHMOD | 实现与自检 | **REVIEW** |
| 2026-09-15 | Cursor | POSIX-CHMOD | Step 37 验收 | **ACCEPTED**；172 tests + vng PASS |
| 2026-09-15 | Cursor | POSIX-EXCHANGE | 选定 Step 38 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 38 不顺手做 WHITEOUT（除非完整可测）、chown、write-back、精细 coherence。

## 8. 当前 Codex 提示词（Step 38）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
HEAD 应含 Step 37（ABI v16 OP_SETATTR）。

开工时：POSIX-EXCHANGE → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 一旦改 kestrelfs/*.c 或依赖 mount：必须 vng
- Redis/S3 门控测可选；默认 cargo test 不依赖外部服务

## 目标：Step 38 — POSIX-EXCHANGE（rename exchange）
实现 Linux `renameat2(..., RENAME_EXCHANGE)`：原子交换两个已存在路径的目录项。

必做：
1. 内核：`kestrelfs_inode_rename` 接受 `RENAME_EXCHANGE`（可与既有 NOREPLACE 共存规则写清：二者互斥）
2. MetaStore：Mem/File/Redis 原子交换两个 dirent；不得半交换；目录/文件类型规则符合 POSIX/Linux
   （通常要求两者都存在；目录与非目录交换的拒绝语义写清并测试）
3. 与硬链接、目录 nlink、open-unlink/orphan、cache invalidate 兼容
4. 未知 flags / WHITEOUT：若本步不做 WHITEOUT，继续返回 `EINVAL` 并在 §9 写明

可选：`RENAME_WHITEOUT`（若做不全，不要半截 ABI）

要求：
1. 优先扩展既有 `RENAME_DATA` flags（ABI 可能需 bump）；C/Rust 同步 + 编译期断言
2. 单测 + STEP38_*_PASS vng（文件↔文件、目录↔目录；失败原子性；与 NOREPLACE 互斥）
3. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
4. 不要擅自 commit/push

## 明确不做
chown/utimes、write-back、精细 coherence、Redis TLS、自动 wipe、iget5。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + 相关 vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-15 — Step 37 POSIX-CHMOD（Cursor ACCEPTED）

- ABI v16 `OP_SETATTR`；文件/目录/orphan chmod；chown/时间戳 `EOPNOTSUPP`。
- 验证：172 tests；clippy / `make -C kestrelfs` 干净；Cursor 复跑 vng
  `STEP37_POSIX_CHMOD_PASS`（umount_ms=20）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-15 — Step 36 POSIX-LIFECYCLE（Cursor ACCEPTED）

- ABI v15 open-unlink；commit `05104e7`。
