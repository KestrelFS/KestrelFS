# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-15，Cursor（验收 Step 33；选定 Step 34 = POSIX-ATTR）
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

- Phase 4 cache、DIST-GC/META、硬链接、`RENAME_NOREPLACE`（Step 24–33）均已验收。
- Redis metadata 为 v2 分记录 HASH/SET + Lua revision-CAS；点查定向读。
- IPC ABI **v13**（含 `RENAME_DATA` flags），cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor 已调整）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–9 | cache + DIST + hard link + NOREPLACE | Step 24–33 | **ACCEPTED** | rename flags 必做项已落地 |
| 10 | POSIX-ATTR | Step 34 mode + 目录 nlink | **DECIDED** | 兼容性缺口小、风险低于 open-unlink |
| 11 | POSIX-LIFECYCLE | open-unlink | PROPOSED | 价值高但 inode 生命周期风险更大 |
| 12 | CACHE-COHERENCE | 多节点失效 | PROPOSED | 共享部署正确性 |
| — | RENAME_EXCHANGE / WHITEOUT | 剩余 rename flags | PROPOSED | Step 33 刻意未做，可后补 |
| — | CACHE-WRITE | 写缓存 | **DEFERRED** | 默认只读 fill cache |
| — | DIST-OBJECT / TLS-RECONNECT / ASYNC | 其它 | PROPOSED | 低于本步 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

CACHE-* 主线已 ACCEPTED；`CACHE-WRITE` 仍 DEFERRED；`CACHE-COHERENCE` 待后。

## 4. 控制面、对象存储与 POSIX

### POSIX-RENAME — Step 33

- 状态：`ACCEPTED`（Cursor，2026-09-15）
- 实现：ABI v13 `RENAME_DATA` flags；原子 `RENAME_NOREPLACE`；EEXIST 不改 namespace/GC。
- 已知：Linux VFS 对同 inode 硬链接别名的 NOREPLACE 在回调前返回 `EEXIST`；MetaStore 层仍为成功 no-op。
- 未做：`RENAME_EXCHANGE` / `RENAME_WHITEOUT`。

### POSIX-ATTR — Step 34

- 状态：`DECIDED`
- 目标：`create`/`mkdir` 尊重 VFS 传入 mode；目录 `nlink` 随子目录增减正确变化。
- 范围：见 §8。

### DIST-OBJECT / IPC-SCALE / CACHE-COHERENCE / open-unlink

仍为 `PROPOSED`。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-15 | Cursor | POSIX-CORE | Step 32 验收 | **ACCEPTED** |
| 2026-09-15 | Cursor | POSIX-RENAME | 选定 Step 33 | **DECIDED** |
| 2026-09-15 | Codex | POSIX-RENAME | 实现与自检 | **REVIEW** |
| 2026-09-15 | Cursor | POSIX-RENAME | Step 33 验收 | **ACCEPTED**；158 tests + vng PASS |
| 2026-09-15 | Cursor | POSIX-ATTR | 选定 Step 34 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 34 不顺手做 open-unlink、EXCHANGE/WHITEOUT、write-back、多节点失效、Redis TLS。

## 8. 当前 Codex 提示词（Step 34）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
HEAD 应含 Step 33（ABI v13 RENAME_NOREPLACE）。

开工时：POSIX-ATTR → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 纯 MetaStore 语义可用 cargo test；一旦改 kestrelfs/*.c 必须 vng
- Redis/S3 门控测可选；默认 cargo test 不依赖外部服务

## 目标：Step 34 — POSIX-ATTR（mode + 目录 nlink）
补齐创建权限与目录链接计数。

必做：
A) create / mkdir 尊重内核传入的 mode（至少 permission bits；文件类型仍由操作决定）
   - MemStore/FileMetaStore/Redis v2 一致持久化
   - getattr / LOOKUP / CREATE 响应携带正确 mode；内核 inode 使用该 mode
B) 目录 nlink：父目录在子目录 create/mkdir 时 +1，rmdir/unlink 空目录时 -1；
   跨目录 rename 目录时正确调整两侧父目录 nlink
   - 普通文件硬链接不改变目录 nlink
   - 根目录与空目录基线保持 POSIX 常见语义（通常至少含 . 与 .. 对应计数）

要求：
1. 若需 bump ABI / 改 payload：C/Rust 同步 + 编译期断言；能复用现有字段则不新 opcode
2. 单测覆盖 mode 持久化与目录 nlink 增减/rename；内核路径须 STEP34_*_PASS vng
3. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
4. 不要擅自 commit/push

## 明确不做
open-unlink、RENAME_EXCHANGE/WHITEOUT、write-back、多节点失效、Redis TLS、
自动 wipe、iget5 重设计。

## 验收自检
- cargo test + clippy -D warnings
- 若改内核：make -C kestrelfs 零警告 + 相关 vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-15 — Step 33 POSIX-RENAME（Cursor ACCEPTED）

- ABI v13：`RENAME_DATA` payload flags；原子 `RENAME_NOREPLACE`。
- 验证：158 tests；clippy / `make -C kestrelfs` 干净；Cursor 复跑 vng
  `STEP33_POSIX_RENAME_PASS`（umount_ms=26）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-15 — Step 32 POSIX-CORE（Cursor ACCEPTED）

- 硬链接 ABI v12；commit `b007a00`。
