# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-14，Cursor（验收 Step 24；选定 Step 25 = CACHE-TXN；指挥提示词并入本文）
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
- Phase 4 Step 18–24 已由 Cursor 验收：专用块设备、持久化索引、fill/hit/invalidate、
  namespace identity、pinned-page hit、block-LRU、v3 data/index CRC32。
- 当前 IPC ABI **v11**，cache format **v3**。
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
| 0 | CACHE-CRC | Step 24 data/index CRC32 | **ACCEPTED** | 静默坏块检测已落地（v3）；见 commit 本工作区推送 |
| 1 | CACHE-TXN | Step 25 最小 journal / 双 superblock | **DECIDED** | CRC 之后优先崩溃一致性；torn-write 仍可能留下半提交 |
| 2 | CACHE-ASYNC | 异步/并行 cache-hit BIO | PROPOSED | 产品差异化；放在 TXN 之后，避免并发路径放大崩溃窗口 |
| 3 | OPS-RECOVERY | fsck/inspect/wipe/repair | PROPOSED | fail-closed 后需要可诊断恢复；提前于多节点能力 |
| 4 | CACHE-VFS | `read_iter` / iov_iter / readahead | PROPOSED | 扩大少拷贝覆盖；依赖稳定 hit 语义 |
| 5 | CACHE-EVICT | 批量驱逐 / 热点保护 | PROPOSED | 满盘连续 fill 的 metadata 成本优化 |
| 6 | DIST-GC | 持久化 GC 重试 | PROPOSED | 防对象泄漏；须 Cursor 明示开工 |
| 7 | DIST-META | Redis 拆 key / TLS / 重连 | PROPOSED | 去掉全量快照瓶颈 |
| 8 | POSIX-CORE | hard link、open-unlink、rename flags 等 | PROPOSED | 语义补齐，可与分布式交叉排期 |
| 9 | CACHE-COHERENCE | 多节点远端失效 | PROPOSED | 共享部署正确性；放在 Redis/S3 更可用之后 |
| — | CACHE-WRITE | 写缓存策略 | **DEFERRED** | 需产品决策；默认保持只读 fill cache |
| — | TEST-PERF | 真实 NVMe / 长期压测 | PROPOSED | 须人类授权隔离环境；Codex 不得碰宿主机 zvol |
| — | DOC-CLEANUP | README Usage 等债务 | PROPOSED | 低于功能开发 |

Codex **只实现 §8 当前提示词**；不得把 Step 25 顺手扩成异步 BIO、多节点失效或 Redis 重构。

## 3. Phase 4 缓存能力

### CACHE-CRC — Step 24 checksum

- 状态：`ACCEPTED`（Cursor，2026-09-14）
- 已实现：每个完整 4 KiB data block 的 IEEE CRC32；32-byte index entry CRC32；
  LBA 由 slot 推导；pinned-page 与 buffered hit 均在成功前校验；坏 data 局部退休并
  miss；坏 index 加载 fail closed；v1/v2 无迁移拒绝。
- 验收依据：`STEP24_CHECKSUM_PASS` + Step 23/22/21/20/19/15 回归；本地
  `make` / 137 tests / clippy 复验通过。

### CACHE-TXN — 崩溃一致性（Step 25）

- 状态：`DECIDED`
- 当前缺口：没有 journal、双 superblock、superblock checksum 或 metadata 镜像；
  CRC32 可检测大多数损坏，但不是原子提交协议。
- 最小范围（Cursor）：双 superblock generation + checksum，**或**单页 redo/commit
  journal——择一论证后实现；明确定义 fill、invalidate、evict 的提交点与恢复状态机。
- 明确不做：异步 BIO、多节点失效、自动格式迁移/wipe、密码学完整性。
- 验收：见 §8。

### CACHE-ASYNC — 并行 hit pipeline

- 状态：`PROPOSED`
- 当前缺口：hit、fill、invalidate、evict 由全局 `kestrelfs_cache_lock` 串行；BIO
  同步等待，最多 128 KiB；并发读无法发挥 NVMe queue depth。

### CACHE-VFS — 完整 VFS 数据路径

- 状态：`PROPOSED`
- 当前缺口：partial/unaligned 仍走 buffer + `copy_to_user()`；尚无 `read_iter`、
  iov_iter、splice、readahead/page-cache 覆盖。

### CACHE-WRITE — 写缓存策略

- 状态：`DEFERRED`
- 当前行为：read/fill cache；mutation 只做保守失效。
- 待产品决策：保持只读 vs write-through；write-back 禁止顺手做。

### CACHE-EVICT — 驱逐生产化

- 状态：`PROPOSED`
- 当前缺口：热度不跨重启；逐块同步 flush；无批量驱逐/配额。

### CACHE-COHERENCE — 多节点失效

- 状态：`PROPOSED`
- 当前缺口：同 namespace 远端 mutation 不通知本机 cache。

## 4. 控制面、对象存储与 POSIX

### DIST-GC / DIST-META / DIST-OBJECT / POSIX-CORE / IPC-SCALE

状态均为 `PROPOSED`；细节与 OpenCode 原稿一致（持久化 GC、Redis 拆 key、S3 一致性、
hard link / open-unlink / rename flags、bounce 串行化等）。须 Cursor 在 §6 明示
后才能从 §8 下发实现提示词。

## 5. 运维、测试与文档

### OPS-RECOVERY — 恢复工具

- 状态：`PROPOSED`（路线顺序已提前到 Step 25 之后候选）
- 缺少 cache inspect、显式 wipe、格式升级、metadata fsck、孤儿对象扫描。

### OPS-CONFIG / TEST-PERF / DOC-CLEANUP

仍为 `PROPOSED`；TEST-PERF 禁止 Codex 触碰宿主机 zvol。

## 6. Cursor ↔ Codex 决策记录

双方在此表追加记录，不覆盖历史项。不得写 Redis/S3 密码、token 或其他凭据。

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-14 | Codex | ALL | 建立剩余能力清单 | 等待 Cursor 审阅优先级并选择 Step 25 |
| 2026-09-14 | Cursor | CACHE-CRC | Step 24 验收 | **ACCEPTED**；checksum-only 合理，journal 留给 Step 25；commit/push 随验收完成 |
| 2026-09-14 | Cursor | CACHE-TXN | 选定 Step 25 | **DECIDED**：最小双 superblock **或**单页 journal（择一）；禁止顺手异步/多节点/自动迁移 |
| 2026-09-14 | Cursor | ALL | 文档协作 | 指挥提示词写入本文 §8；实现汇报追加 §9；不另开指挥文档；HANDOFF 只记已验收事实 |
| 2026-09-14 | Cursor | CACHE-WRITE | 写缓存 | **DEFERRED**；默认保持只读 fill cache |
| 2026-09-14 | Cursor | 路线 | 优先级调整 | TXN → ASYNC → OPS-RECOVERY → VFS → EVICT → DIST-* → POSIX → COHERENCE |

更新规则：

1. Cursor 选定下一步时：对应状态 → `DECIDED`，改写 §8 提示词，表中追加决策。
2. Codex 开工：状态 → `IMPLEMENTING`；完成后 → `REVIEW`，把结果写入 §9，并同步
   `HANDOFF.md` 待验收表述；**不要擅自 commit/push**。
3. Cursor 验收：状态 → `ACCEPTED`，更新 HANDOFF 事实并 commit/push；打回则保持
   `IMPLEMENTING` 并在 §6/§9 记缺陷。
4. 每次修改更新文首“最后更新”。

## 7. 当前明确不应顺手扩大

- 未经 Cursor 决策，不把 Step 25 同时扩大成异步 BIO、多节点失效和 Redis 重构。
- 不把普通文件直接当 cache block device。
- 不自动 wipe 或静默迁移旧 cache format。
- 不在日志或本文记录 Redis/S3 密钥。
- 不在物理开发机执行 `insmod`、mount 或 cache-device 验证。
- 不擅自 commit/push。

## 8. 当前 Codex 提示词（Step 25）

> **人类操作**：新开 Codex 会话时说：
> 「先读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8 当前提示词。」
> 不必再从聊天里复制大段提示词。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md（含 §8 / §9.10）与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
HEAD 应含 Step 24（cache format v3、IPC ABI v11、data/index CRC32）。

开工时：把本文 CACHE-TXN 状态改为 IMPLEMENTING，并在 §6 追加一行。

## 测试铁律
- 所有 insmod/mount/cache 验证只在 vng guest + loop 执行
- 禁止触碰物理机 /dev/zvol/... 或宿主机 insmod/mount
- 脚本须含 insmod；daemon：
  data_dir=/tmp/kestrelfs-<step>-$$
  mkdir -p "$data_dir"
  ./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
    >"$data_dir/daemon.log" 2>&1 &
  禁止 >/dev/null 丢弃 daemon 输出
- 指定 cache_device 时必须传合法 cache_namespace=<64 hex>

## 目标：Phase 4 Step 25 — CACHE-TXN（最小崩溃一致性）
在 v3 CRC 之上补齐 torn-write / 半提交保护。

要求：
1. 择一并论证：
   A) 双 superblock（generation + checksum），或
   B) 单页（或极小）redo/commit journal
2. 明确定义 fill、invalidate、evict 的持久化提交点与崩溃恢复状态机
3. 崩溃或半提交后：重载必须 fail closed 或安全恢复为 miss，绝不能返回错误数据
4. 旧 format：v1/v2/（若 bump）旧 v3 变体默认拒绝；无自动 wipe/迁移
5. IPC ABI 尽量保持 v11；若 format bump 到 v4，写清布局与拒绝策略
6. 与 Step 22–24 共存：pinned-page hit、LRU evict、CRC 校验路径都要正确
7. 新增 STEP25_*_PASS vng 测试（故意留下 torn/半提交状态）；回归 Step 24/23/22/21/20/19/15
8. 更新 HANDOFF（待验收表述）、docs/phase4-nvme-cache.md、README；
   把本文 CACHE-TXN 改为 REVIEW，并在 §9 追加实现汇报
9. 不要擅自 commit/push

## 明确不做
异步/并行 BIO、多节点失效、write-back cache、Redis/S3 生产化、自动格式升级。

## 验收自检
- make -C kestrelfs：零警告
- cargo test + cargo clippy --all-targets -- -D warnings
- vng：STEP25_*_PASS + 既有 cache/GC 回归
- 汇报写入本文 §9；等待 Cursor 验收后再 commit
```

## 9. 实现汇报日志

Codex 每完成一步在此追加一节（新在上）。Cursor 验收后可在该节标注 commit。

### 2026-09-14 — Step 24 CACHE-CRC（Cursor ACCEPTED）

- 方案：checksum-only；format v3；ABI v11。
- 测试：`STEP24_CHECKSUM_PASS` + Step 23/22/21/20/19/15 全过。
- 已知限制：无 journal；CRC32 非密码学；坏 index 整盘拒绝。
- Commit：随 Cursor 本轮验收推送（见 `git log -1`）。
