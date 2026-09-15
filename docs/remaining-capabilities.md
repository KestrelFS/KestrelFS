# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-15，Codex（Step 29 CACHE-EVICT 实现与自检完成，待 Cursor 验收）
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

- Phase 1–3 控制面原型已完成。
- Phase 4 Step 18–28 已由 Cursor 验收（含 `read_iter` / iov_iter cache 读）。
- IPC ABI **v11**，cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor 已调整）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–4 | … / CACHE-VFS | Step 24–28 | **ACCEPTED** | 正确性、运维、VFS 读路径最小闭环 |
| 5 | CACHE-EVICT | Step 29 批量驱逐 / 热点保护 | **REVIEW** | 16-victim batch、index-page 合并写与崩溃恢复待验收 |
| 6–8 | DIST-* / POSIX-CORE | 分布式与语义 | PROPOSED | 须 Cursor 明示 |
| 9 | CACHE-COHERENCE | 多节点失效 | PROPOSED | 共享部署正确性 |
| — | CACHE-WRITE | 写缓存 | **DEFERRED** | 默认只读 fill cache |
| — | ASYNC-COMPLETION / TEST-PERF / DOC | 其它 | PROPOSED | 低于本步 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

### CACHE-VFS — Step 28

- 状态：`ACCEPTED`（Cursor，2026-09-15）
- 实现：`.read_iter`；单段对齐 pinned BIO；跨段/`copy_to_iter` fallback；miss 可
  `iov_iter_revert` 后完整 READ_DATA。

### CACHE-EVICT — Step 29

- 状态：`REVIEW`
- 实现：默认 16-victim batch journal；按 index page 合并清零；批量崩溃恢复；
  单批最多总槽位 1/16 以保护 MRU 尾部。
- 剩余限制：热度不跨重启；分散 victim 每页仍一次同步写；fill/invalidate 仍逐次 journal。
- 范围：见 §8。

### CACHE-WRITE / COHERENCE

状态同 §2。

## 4–5. 控制面 / 运维

DIST-*、POSIX、OPS-CONFIG、TEST-PERF 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-15 | Cursor | OPS-RECOVERY | Step 27 验收 | **ACCEPTED** |
| 2026-09-15 | Cursor | CACHE-VFS | 选定 Step 28 | **DECIDED** |
| 2026-09-15 | Codex | CACHE-VFS | 实现与自检 | **REVIEW** |
| 2026-09-15 | Cursor | CACHE-VFS | Step 28 验收 | **ACCEPTED**；跨段 copy_to_iter 可接受 |
| 2026-09-15 | Cursor | CACHE-EVICT | 选定 Step 29 | **DECIDED**；见 §8 |
| 2026-09-15 | Codex | CACHE-EVICT | Step 29 开工 | **IMPLEMENTING**；仅执行 §8 |
| 2026-09-15 | Codex | CACHE-EVICT | 实现与自检完成 | **REVIEW**；见 §9 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push；不扩 write-back / 多节点。

## 8. 当前 Codex 提示词（Step 29）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
HEAD 应含 Step 28（read_iter / iov_iter；format v4；ABI v11）。

开工时：CACHE-EVICT → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 所有 insmod/mount/cache 验证只在 vng guest + loop
- 禁止触碰 /dev/zvol/... 或宿主机 insmod/mount
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- cache_device 必须配合法 cache_namespace=<64 hex>

## 目标：Phase 4 Step 29 — CACHE-EVICT（批量 / 更低成本驱逐）
在保持 Step 23–28 正确性（LRU、journal、CRC、rwsem、read_iter）前提下，降低满盘
连续 fill 时逐块 metadata flush 的成本，并可测地回收多个 victim。

要求：
1. 实现可测的批量 eviction（例如一次回收 N 个 LRU 头，或合并 journal/index 写）；
   论证不变量：清旧 index → 再复用 data；崩溃仍安全 miss / fail closed
2. 与并行 hit 共存：写侧仍排他；不得让 reader DMA 中的 slot 被复用
3. 热点保护可选但欢迎：避免刚 touch 的 MRU 立刻被批量扫掉
4. IPC ABI 尽量 v11；format 尽量 v4（bump 则 fail-closed，无自动迁移）
5. 新增 STEP29_*_PASS vng（小 cache 满盘 + 批量回收可观测；旧 victim miss；新数据 hit；
   reload 后一致性）；回归 Step 28/27/26/25/24/23/22/21/20/19/15
6. 更新 HANDOFF（待验收）、docs/phase4-nvme-cache.md、README（保持中文）；
   本文 CACHE-EVICT → REVIEW，§9 追加汇报
7. 不要擅自 commit/push

## 明确不做
write-back、多节点失效、Redis/S3 生产化、自动 wipe、真正异步 completion 大重构、
跨 iovec scatter-gather BIO 大工程。

## 验收自检
- make -C kestrelfs / make -C tools：零警告
- cargo test + clippy -D warnings
- vng：STEP29_*_PASS + 既有回归
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-15 — Step 29 CACHE-EVICT（Codex REVIEW）

- 方案：保持 IPC ABI v11 / cache format v4。`cache_evict_batch` 默认 16（合法 1–64），
  实际单批最多总槽位 1/16；选择 LRU 头部，因此刚 touch 的 MRU 尾部不会被小批量
  扫掉。一次 v4 PREPARED journal 在 reserved 前部记录整批 slot，同一 4 KiB index
  page 上的清零合并成一次 read/modify/write/flush；journal 清零提交后才从
  rhashtable/list/bitmap 释放槽位，随后 fill 才能复用。
- 崩溃/并发不变量：合法半提交 batch journal 重载时清完整批为 miss；torn journal
  CRC fail closed，index page 若波及非 victim 则 entry CRC fail closed。evict 全程持
  cache rwsem 写侧，必须等待 pinned-page reader 的 BIO/CRC/unpin 完成，不会复用
  DMA 中 slot。旧单-entry journal 继续兼容；离线 admin 已能识别 batch journal。
- 可观测性：新增只读 `cache_eviction_batches`、`cache_eviction_batch_slots`、
  `cache_eviction_index_writes`。专项小 cache 实测 16 victims 只产生 1 个合并 index
  page write。
- 测试：`cargo test` 137 passed；clippy `--all-targets -- -D warnings` 通过；
  `make -C kestrelfs` 与 `make -C tools` 零警告。vng 专项输出
  `STEP29_BATCH_EVICTION_PASS victims=16 index_writes=1`、
  `STEP29_MRU_PROTECTION_PASS`、`STEP29_RELOAD_PASS`、
  `STEP29_BATCH_RECOVERY_PASS`、`STEP29_CACHE_EVICT_PASS`（umount 253 ms）。
  Step 28/27/26/25/24/23/22/21/20/19/15 回归均输出对应 PASS。
- 测试波动：数次 vng 在 guest 脚本启动前 exit 255；一次 Step 25 guest daemon
  重启出现 `Transport endpoint is not connected`。均未形成代码失败，立即完整重跑
  后通过。全部 insmod/mount/cache 仅在 vng guest+loop，未触碰宿主机设备。
- 风险/限制：batch 合并收益取决于 victim slot 是否落在相同 index page；分散时
  仍按页同步写。LRU recency 仍不持久化且无租户/分区隔离；fill/invalidate 仍逐条
  journal。未实现 write-back、多节点失效、Redis/S3 生产化、自动 wipe、真正异步
  completion 或跨 iovec scatter-gather BIO。

### 2026-09-15 — Step 28 CACHE-VFS（Cursor ACCEPTED）

- `.read_iter` + iov_iter cache/miss；单段 pinned BIO；跨段 `copy_to_iter`。
- 测试：`STEP28_CACHE_VFS_PASS`；回归 Step 27–15。
- Commit：随 Cursor 本轮验收推送（见 `git log -1`）。

### 2026-09-15 — Step 27 OPS-RECOVERY（Cursor ACCEPTED）

- `kestrelfs-cache-admin` inspect + 双确认 wipe。
