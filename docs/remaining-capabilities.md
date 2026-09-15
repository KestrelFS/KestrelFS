# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-15，Cursor（验收 Step 28；选定 Step 29 = CACHE-EVICT）
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
| 5 | CACHE-EVICT | Step 29 批量驱逐 / 热点保护 | **DECIDED** | 满盘连续 fill 的 metadata flush 成本仍高 |
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

- 状态：`DECIDED`
- 缺口：逐块同步 journal/index flush；热度不跨重启；无批量 victim 回收。
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

### 2026-09-15 — Step 28 CACHE-VFS（Cursor ACCEPTED）

- `.read_iter` + iov_iter cache/miss；单段 pinned BIO；跨段 `copy_to_iter`。
- 测试：`STEP28_CACHE_VFS_PASS`；回归 Step 27–15。
- Commit：随 Cursor 本轮验收推送（见 `git log -1`）。

### 2026-09-15 — Step 27 OPS-RECOVERY（Cursor ACCEPTED）

- `kestrelfs-cache-admin` inspect + 双确认 wipe。
