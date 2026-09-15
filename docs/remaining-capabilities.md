# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-15，Cursor（验收 Step 36；选定 Step 37 = POSIX-CHMOD）
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

- Phase 4 cache、DIST、POSIX 子集、CACHE-COHERENCE、open-unlink（Step 24–36）均已验收。
- Redis metadata 为 v2 分记录 HASH/SET + Lua revision-CAS；点查定向读。
- Redis daemon 每 100 ms 轮询 durable revision，经 ABI v14 全 cache 失效。
- IPC ABI **v15**（含 `FINALIZE_ORPHAN`），cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor 已调整）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–12 | cache + DIST + POSIX + coherence + open-unlink | Step 24–36 | **ACCEPTED** | open-unlink 主路径已落地 |
| 13 | POSIX-CHMOD | Step 37 创建后 mode setattr | **DECIDED** | create 已尊重 mode，仍缺 chmod |
| — | RENAME_EXCHANGE / WHITEOUT | 其余 rename flags | PROPOSED | 可后补 |
| — | COHERENCE-FINE / DIST-OBJECT / ASYNC | 其它 | PROPOSED | 低于本步 |
| — | CACHE-WRITE | 写缓存 | **DEFERRED** | 默认只读 fill cache |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

CACHE-* 与 CACHE-COHERENCE 最小闭环已 ACCEPTED；`CACHE-WRITE` 仍 DEFERRED。

## 4. 控制面、对象存储与 POSIX

### POSIX-LIFECYCLE — Step 36

- 状态：`ACCEPTED`（Cursor，2026-09-15）
- 实现：ABI v15；open 计数；nlink=0 orphan；`FINALIZE_ORPHAN`；未恢复 iget5。
- 已知：单挂载 open 计数；final-close IPC 失败则保留 orphan，无自动 sweep。

### POSIX-CHMOD — Step 37

- 状态：`DECIDED`
- 目标：创建后 `chmod` / `setattr` 持久化 mode（至少 permission bits）。
- 范围：见 §8。

### DIST-OBJECT / EXCHANGE / COHERENCE-FINE

仍为 `PROPOSED`。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-15 | Cursor | CACHE-COHERENCE | Step 35 验收 | **ACCEPTED** |
| 2026-09-15 | Cursor | POSIX-LIFECYCLE | 选定 Step 36 | **DECIDED**；禁止 iget5 |
| 2026-09-15 | Codex | POSIX-LIFECYCLE | 实现与自检 | **REVIEW** |
| 2026-09-15 | Cursor | POSIX-LIFECYCLE | Step 36 验收 | **ACCEPTED**；168 tests + vng PASS |
| 2026-09-15 | Cursor | POSIX-CHMOD | 选定 Step 37 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 37 不顺手做 chown/完整 utimes、EXCHANGE、write-back、精细 coherence。

## 8. 当前 Codex 提示词（Step 37）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
HEAD 应含 Step 36（ABI v15 open-unlink）。

开工时：POSIX-CHMOD → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 一旦改 kestrelfs/*.c 或依赖 mount：必须 vng
- Redis/S3 门控测可选；默认 cargo test 不依赖外部服务

## 目标：Step 37 — POSIX-CHMOD（创建后 mode setattr）
实现文件/目录创建后的 `chmod`（及等价 setattr 权限位更新）并持久化。

必做：
1. 内核：实现 `.setattr`（至少处理 `ATTR_MODE`）；拒绝/忽略本步未做的 ATTR_* 时行为要明确
2. MetaStore：Mem/File/Redis 原子更新 mode（保留文件类型位；只改 `0o7777` 权限/特殊位）
3. getattr/lookup 返回更新后的 mode；FileMetaStore 重启后仍正确
4. 与 Step 34 create/mkdir 初始 mode、Step 36 orphan 文件兼容（若对 orphan chmod，语义写清）

要求：
1. 若需新 ABI opcode/字段：bump 版本，C/Rust 同步，编译期断言；能扩展既有 setattr/getattr 则优先
2. 单测 + STEP37_*_PASS vng（chmod 文件与目录；重启恢复）
3. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
4. 不要擅自 commit/push

## 明确不做
完整 chown/时间戳 setattr、EXCHANGE/WHITEOUT、write-back、精细 coherence、
Redis TLS、自动 wipe、iget5。

## 验收自检
- cargo test + clippy -D warnings
- 若改内核：make -C kestrelfs 零警告 + 相关 vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-15 — Step 36 POSIX-LIFECYCLE（Cursor ACCEPTED）

- ABI v15 open-unlink；`FINALIZE_ORPHAN`；未恢复 iget5。
- 验证：168 tests；clippy / `make -C kestrelfs` 干净；Cursor 复跑 vng
  `STEP36_POSIX_LIFECYCLE_PASS`（umount_ms=27）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-15 — Step 35 CACHE-COHERENCE（Cursor ACCEPTED）

- Redis revision 轮询 + ABI v14；commit `cf14675`。
