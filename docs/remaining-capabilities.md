# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-14，Cursor（验收 Step 25；README 全中文；选定 Step 26 = CACHE-ASYNC）
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

- Phase 1–3 控制面原型已经完成：动态 VFS、ABI v11 bounce IPC、长名字、symlink、
  truncate、ObjectStore GC、File/Redis MetaStore、LocalFs/S3 ObjectStore。
- Phase 4 Step 18–25 已由 Cursor 验收：专用块设备、持久化索引、fill/hit/invalidate、
  namespace identity、pinned-page hit、block-LRU、data/index CRC32、v4 intent journal。
- 当前 IPC ABI **v11**，cache format **v4**。
- 所有 `insmod`、mount 和 cache block-device 测试只允许在 vng guest + loop 中执行，
  禁止 Codex 触碰宿主机 zvol 或在宿主机加载模块。

状态约定：

- `PROPOSED`：候选能力，尚未排期；
- `DECIDED`：Cursor 已确定方案/顺序，尚未实现；
- `IMPLEMENTING`：Codex 正在实现；
- `REVIEW`：实现和自检完成，等待 Cursor 验收；
- `ACCEPTED`：Cursor 已验收；
- `DEFERRED`：明确延期，并记录原因。

## 2. 建议路线（Cursor 已调整）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0 | CACHE-CRC | Step 24 data/index CRC32 | **ACCEPTED** | v3 起落地；现为 v4 布局一部分 |
| 1 | CACHE-TXN | Step 25 最小 journal | **ACCEPTED** | v4 单页 intent journal + superblock CRC |
| 2 | CACHE-ASYNC | Step 26 异步/并行 cache-hit BIO | **DECIDED** | 产品差异化；TXN 已落地，可安全扩大并发 hit |
| 3 | OPS-RECOVERY | fsck/inspect/wipe/repair | PROPOSED | fail-closed 后需要可诊断恢复 |
| 4 | CACHE-VFS | `read_iter` / iov_iter / readahead | PROPOSED | 扩大少拷贝覆盖 |
| 5 | CACHE-EVICT | 批量驱逐 / 热点保护 | PROPOSED | 降低满盘连续 fill 的 metadata 成本 |
| 6 | DIST-GC | 持久化 GC 重试 | PROPOSED | 防对象泄漏 |
| 7 | DIST-META | Redis 拆 key / TLS / 重连 | PROPOSED | 去掉全量快照瓶颈 |
| 8 | POSIX-CORE | hard link、open-unlink、rename flags 等 | PROPOSED | 语义补齐 |
| 9 | CACHE-COHERENCE | 多节点远端失效 | PROPOSED | 共享部署正确性 |
| — | CACHE-WRITE | 写缓存策略 | **DEFERRED** | 默认保持只读 fill cache |
| — | TEST-PERF | 真实 NVMe / 长期压测 | PROPOSED | 须人类授权；Codex 不得碰宿主机 zvol |
| — | DOC-CLEANUP | 其余文档债务 | PROPOSED | README 已全中文；opcode 全表等仍可补 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

### CACHE-CRC — Step 24 checksum

- 状态：`ACCEPTED`

### CACHE-TXN — 崩溃一致性（Step 25）

- 状态：`ACCEPTED`（Cursor，2026-09-14）
- 方案：单页 intent journal（非双 superblock）；format v4；ABI v11。
- 验收：`STEP25_CACHE_TXN_PASS` 及 fill/invalidate/evict 恢复、torn/v3/super CRC fail-closed；
  回归 Step 24/23/22/21/20/19/15。

### CACHE-ASYNC — 并行 hit pipeline（Step 26）

- 状态：`DECIDED`
- 当前缺口：hit、fill、invalidate、evict 由全局 `kestrelfs_cache_lock` 串行；BIO
  同步等待，最多 128 KiB；并发读无法发挥 NVMe queue depth。
- 最小范围：见 §8。必须与 v4 journal / CRC / LRU / pinned-page 共存且不引入 UAF。

### CACHE-VFS / CACHE-WRITE / CACHE-EVICT / CACHE-COHERENCE

状态同 §2 表；`CACHE-WRITE` 仍为 `DEFERRED`。

## 4. 控制面、对象存储与 POSIX

DIST-GC / DIST-META / DIST-OBJECT / POSIX-CORE / IPC-SCALE 均为 `PROPOSED`；须 Cursor
在 §6 明示后才能写入 §8。

## 5. 运维、测试与文档

OPS-RECOVERY / OPS-CONFIG / TEST-PERF / DOC-CLEANUP 均为 `PROPOSED`。
README 正文已改为全中文（Cursor，2026-09-14）。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-14 | Codex | ALL | 建立剩余能力清单 | 等待 Cursor 审阅优先级并选择 Step 25 |
| 2026-09-14 | Cursor | CACHE-CRC | Step 24 验收 | **ACCEPTED** |
| 2026-09-14 | Cursor | CACHE-TXN | 选定 Step 25 | **DECIDED** → 后由 Codex 实现 |
| 2026-09-14 | Cursor | ALL | 文档协作 | 指挥提示词写入本文 §8；HANDOFF 只记已验收事实 |
| 2026-09-14 | Cursor | CACHE-WRITE | 写缓存 | **DEFERRED** |
| 2026-09-14 | Cursor | 路线 | 优先级调整 | TXN → ASYNC → OPS-RECOVERY → VFS → … |
| 2026-09-14 | Codex | CACHE-TXN | Step 25 实现 | **REVIEW** → 单页 journal / format v4 |
| 2026-09-14 | Cursor | CACHE-TXN | Step 25 验收 | **ACCEPTED**；择 journal 方案认可 |
| 2026-09-14 | Cursor | DOC | README | 改为全中文 |
| 2026-09-14 | Cursor | CACHE-ASYNC | 选定 Step 26 | **DECIDED**；见 §8 |

更新规则：开工 `IMPLEMENTING` → 完成 `REVIEW`+§9 → Cursor 验收 `ACCEPTED` 并 commit；勿擅自 commit/push。

## 7. 当前明确不应顺手扩大

- 不把 Step 26 同时扩大成 write-back、多节点失效、Redis 重构或自动 wipe。
- 不把普通文件直接当 cache block device。
- 不自动 wipe 或静默迁移旧 cache format。
- 不在日志或本文记录 Redis/S3 密钥。
- 不在物理开发机执行 `insmod`、mount 或 cache-device 验证。
- 不擅自 commit/push。

## 8. 当前 Codex 提示词（Step 26）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md（含 §8 / §9.10）与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
HEAD 应含 Step 25（cache format v4、IPC ABI v11、intent journal）。

开工时：把本文 CACHE-ASYNC 状态改为 IMPLEMENTING，并在 §6 追加一行。

## 测试铁律
- 所有 insmod/mount/cache 验证只在 vng guest + loop 执行
- 禁止触碰物理机 /dev/zvol/... 或宿主机 insmod/mount
- 脚本须含 insmod；daemon：
  data_dir=/tmp/kestrelfs-<step>-$$
  mkdir -p "$data_dir"
  ./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
    >"$data_dir/daemon.log" 2>&1 &
  禁止 >/dev/null
- 指定 cache_device 时必须传合法 cache_namespace=<64 hex>

## 目标：Phase 4 Step 26 — CACHE-ASYNC（异步/并行 cache-hit BIO）
在保持 Step 22–25 正确性（pinned-page、LRU、CRC、journal）前提下，降低全局同步
mutex 对 hit 路径的串行化，并允许并行/异步 BIO 发挥队列深度。

要求：
1. 论证并实现一种最小可用方案，例如：
   - 缩短持锁范围（索引查找在锁内，BIO 等待在锁外 + entry refcount/generation），或
   - 有界并行同步 BIO / 异步 completion，同一 entry 不被 invalidate/evict 复用
2. 正确性优先：并发 reader + rewrite/truncate/unlink/evict/corrupt-retire 不得
   返回错数据、UAF、pinned-page 泄漏或 hung task
3. journal 提交点语义不得被破坏；半提交恢复与 CRC fail-closed 仍成立
4. IPC ABI 尽量保持 v11；format 尽量保持 v4（若必须 bump，fail-closed，无自动迁移）
5. 新增 STEP26_*_PASS vng（至少覆盖并发读 + 并发失效）；回归 Step 25/24/23/22/21/20/19/15
6. 若有粗测，在 vng 内与 Step 22/25 同步路径对比并解释
7. 更新 HANDOFF（待验收）、docs/phase4-nvme-cache.md、README（保持中文）；
   本文 CACHE-ASYNC → REVIEW，§9 追加汇报
8. 不要擅自 commit/push

## 明确不做
write-back cache、多节点失效、Redis/S3 生产化、自动 wipe、整页 page-cache 重写
（CACHE-VFS 留给后续）、批量 eviction 大重构（可做最小配套，勿喧宾夺主）。

## 验收自检
- make -C kestrelfs：零警告
- cargo test + cargo clippy --all-targets -- -D warnings
- vng：STEP26_*_PASS + 既有 cache/GC 回归
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-14 — Step 25 CACHE-TXN（Cursor ACCEPTED）

- 方案：单页 4 KiB intent journal + superblock CRC；format v3→v4；ABI v11。
- 恢复：合法 PREPARED → 清目标 index → 安全 miss；torn/v3/坏 super → fail closed。
- 测试：`STEP25_CACHE_TXN_PASS` 及子项；回归 Step 24–15 全过。
- Commit：随 Cursor 本轮验收推送（见 `git log -1`）。

### 2026-09-14 — Step 24 CACHE-CRC（Cursor ACCEPTED）

- 方案：checksum-only；当时 format v3；ABI v11。
- Commit：`8a7d2ce` 等。
