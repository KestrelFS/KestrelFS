# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-15，Cursor（验收 Step 35；选定 Step 36 = POSIX-LIFECYCLE）
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

- Phase 4 cache、DIST、POSIX 子集、CACHE-COHERENCE（Step 24–35）均已验收。
- Redis metadata 为 v2 分记录 HASH/SET + Lua revision-CAS；点查定向读。
- Redis daemon 每 100 ms 轮询 durable revision，经 ABI v14 全 cache 失效。
- IPC ABI **v14**，cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor 已调整）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–11 | cache + DIST + POSIX + coherence | Step 24–35 | **ACCEPTED** | 最小远端失效闭环已落地 |
| 12 | POSIX-LIFECYCLE | Step 36 open-unlink | **DECIDED** | 兼容性高价值；须避开历史 iget5 死锁 |
| — | RENAME_EXCHANGE / WHITEOUT / chmod | 其余 POSIX | PROPOSED | 可后补 |
| — | COHERENCE-FINE | 按 inode/range / pubsub | PROPOSED | Step 35 粗粒度可接受，后优化 |
| — | CACHE-WRITE | 写缓存 | **DEFERRED** | 默认只读 fill cache |
| — | DIST-OBJECT / TLS-RECONNECT / ASYNC | 其它 | PROPOSED | 低于本步 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

CACHE-* 主线与 CACHE-COHERENCE 最小闭环已 ACCEPTED；`CACHE-WRITE` 仍 DEFERRED。

## 4. 控制面、对象存储与 POSIX

### CACHE-COHERENCE — Step 35

- 状态：`ACCEPTED`（Cursor，2026-09-15）
- 实现：Redis revision 轮询 + ABI v14 `INVALIDATE_CACHE_ALL`；启动先失效；journal fail-closed。
- 已知：≤100 ms 旧 hit 窗口；整盘失效；非 Redis 后端不启用。

### POSIX-LIFECYCLE — Step 36

- 状态：`DECIDED`
- 目标：open-unlink —— 已打开 fd 在 unlink 后仍可读写，最后 close 才回收对象。
- 范围：见 §8。**禁止**恢复历史 `iget5_locked` 自定义匹配方案。

### DIST-OBJECT / IPC-SCALE / EXCHANGE

仍为 `PROPOSED`。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-15 | Cursor | POSIX-ATTR | Step 34 验收 | **ACCEPTED** |
| 2026-09-15 | Cursor | CACHE-COHERENCE | 选定 Step 35 | **DECIDED** |
| 2026-09-15 | Codex | CACHE-COHERENCE | 实现与自检 | **REVIEW** |
| 2026-09-15 | Cursor | CACHE-COHERENCE | Step 35 验收 | **ACCEPTED**；164 tests + vng PASS |
| 2026-09-15 | Cursor | POSIX-LIFECYCLE | 选定 Step 36 | **DECIDED**；见 §8；禁止 iget5 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 36 不顺手做 write-back、精细 coherence pub/sub、Redis TLS、EXCHANGE/WHITEOUT。

## 8. 当前 Codex 提示词（Step 36）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
HEAD 应含 Step 35（ABI v14 CACHE-COHERENCE）。

开工时：POSIX-LIFECYCLE → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 一旦改 kestrelfs/*.c 或依赖 mount：必须 vng
- Redis/S3 门控测可选；默认 cargo test 不依赖外部服务

## 目标：Step 36 — POSIX-LIFECYCLE（open-unlink）
实现：文件仍被打开时 unlink 只移除目录项；inode/数据在最后 close 前保持可读可写；
最后引用释放后才把无引用对象纳入 GC queue。

硬约束：
1. **禁止**恢复历史 `iget5_locked()` 自定义 test/set 方案（曾导致 umount 死循环）
2. 可继续使用 `iget_locked(sb, ino)` / 显式 open-handle 计数；须证明 umount <1s 且无 hung task
3. MemStore + FileMetaStore 必须正确；Redis v2 若触及 mutation，保持原子性与 GC queue
4. 与硬链接语义兼容：nlink 与 open-handle 都归零才回收
5. cache：unlink 后若仍有 open fd，读路径仍正确；最终回收时失效

要求：
1. 若需新 ABI（open/close/handle 或 unlink 语义扩展）：bump 版本，C/Rust 同步，编译期断言
2. 单测 + STEP36_*_PASS vng：至少覆盖 open→unlink→仍可读→close→对象回收；以及 hard link 交叉
3. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
4. 不要擅自 commit/push

## 明确不做
write-back、精细 coherence pub/sub、chmod、EXCHANGE/WHITEOUT、Redis TLS、
自动 wipe、iget5 重设计。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + 相关 vng（含 umount 时限）
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-15 — Step 35 CACHE-COHERENCE（Cursor ACCEPTED）

- Redis revision 轮询 + ABI v14 全 cache 失效；启动先失效；journal fail-closed。
- 验证：164 tests；clippy / `make -C kestrelfs` 干净；Cursor 复跑 vng
  `STEP35_CACHE_COHERENCE_PASS`（umount_ms=24）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-15 — Step 34 POSIX-ATTR（Cursor ACCEPTED）

- create/mkdir mode + 目录 nlink；commit `7edf3b6`。
