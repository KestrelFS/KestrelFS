# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-15，Cursor（验收 Step 26；选定 Step 27 = OPS-RECOVERY）
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

- Phase 1–3 控制面原型已经完成。
- Phase 4 Step 18–26 已由 Cursor 验收：块设备缓存、索引、fill/hit/invalidate、
  namespace、pinned-page、LRU、CRC32、v4 journal、**并行同步 hit（rwsem）**。
- 当前 IPC ABI **v11**，cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor 已调整）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–2 | CACHE-CRC / TXN / ASYNC | Step 24–26 | **ACCEPTED** | 检测、崩溃一致性、并行 hit 已落地 |
| 3 | OPS-RECOVERY | Step 27 fsck/inspect/wipe | **DECIDED** | fail-closed 后需要可诊断、可显式恢复 |
| 4 | CACHE-VFS | `read_iter` / iov_iter / readahead | PROPOSED | 扩大少拷贝覆盖 |
| 5 | CACHE-EVICT | 批量驱逐 / 热点保护 | PROPOSED | 降低满盘 metadata 成本 |
| 6–8 | DIST-* / POSIX-CORE | 分布式与语义 | PROPOSED | 须 Cursor 明示 |
| 9 | CACHE-COHERENCE | 多节点失效 | PROPOSED | 共享部署正确性 |
| — | CACHE-WRITE | 写缓存 | **DEFERRED** | 默认只读 fill cache |
| — | ASYNC-COMPLETION | 真正异步 BIO completion | PROPOSED | Step 26 已是并行同步；真异步可后排 |
| — | TEST-PERF / DOC-CLEANUP | 压测与文档 | PROPOSED | 低于功能；README 已中文 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

### CACHE-ASYNC — Step 26

- 状态：`ACCEPTED`（Cursor，2026-09-15）
- 方案：全局 mutex → rwsem；hit 读侧并行同步 BIO；mutation 写侧；CRC 退休
  generation 二次确认；`cache_parallel_reads` A/B。
- 非目标（已接受）：真正异步 completion / per-entry RCU。

### OPS-RECOVERY — Step 27

- 状态：`DECIDED`
- 缺口：缺 cache inspect、显式 wipe、只读诊断、安全格式重置；坏设备只能
  fail closed，运维难排障。
- 范围：见 §8。

### CACHE-VFS / WRITE / EVICT / COHERENCE

状态同 §2。

## 4–5. 控制面 / 运维

DIST-*、POSIX、OPS-CONFIG、TEST-PERF 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-14 | Cursor | CACHE-ASYNC | 选定 Step 26 | **DECIDED** |
| 2026-09-15 | Codex | CACHE-ASYNC | 实现与自检 | **REVIEW**；rwsem 并行同步 BIO |
| 2026-09-15 | Cursor | CACHE-ASYNC | Step 26 验收 | **ACCEPTED**；并行同步方案认可，真异步后排 |
| 2026-09-15 | Cursor | OPS-RECOVERY | 选定 Step 27 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe（Step 27 的 wipe 必须显式、可审计）。
- 不触碰宿主机 zvol；不擅自 commit/push；不扩 write-back / 多节点 / Redis 重构。

## 8. 当前 Codex 提示词（Step 27）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
HEAD 应含 Step 26（rwsem 并行 hit；format v4；ABI v11）。

开工时：OPS-RECOVERY → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 所有 insmod/mount/cache 验证只在 vng guest + loop
- 禁止触碰 /dev/zvol/... 或宿主机 insmod/mount
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- cache_device 必须配合法 cache_namespace=<64 hex>

## 目标：Phase 4 Step 27 — OPS-RECOVERY（最小可诊断 / 可显式恢复）
让 fail-closed 的 cache 设备可被运维理解，并在人类明确意图下安全重置。

要求（最小可用，择优组合但勿做成大型工具套件）：
1. cache inspect：用户态工具或脚本，只读解析 v4 superblock/journal/index 摘要
   （magic/version/namespace hex/entry 计数/journal 状态/CRC 结果），不加载模块也可跑
2. 显式 wipe：必须双确认（例如环境变量 + 命令行 flag）；默认拒绝；wipe 后可重新 format
3. 可选：模块参数或 debugfs/sysfs 只读导出当前内存 index 统计（非稳定 ABI）
4. 不得静默迁移 v1/v2/v3；不得在常规 mount/insmod 路径自动 wipe
5. IPC ABI 保持 v11；format 尽量保持 v4（若 bump 须 fail-closed 并文档化）
6. 新增 STEP27_*_PASS vng：inspect 输出正确；wipe 后旧数据不可命中且可重新填充；
   误用 wipe（缺确认）必须失败；回归 Step 26/25/24/23/22/21/20/19/15
7. 更新 HANDOFF（待验收）、docs/phase4-nvme-cache.md、README（保持中文）；
   本文 OPS-RECOVERY → REVIEW，§9 追加汇报
8. 不要擅自 commit/push

## 明确不做
异步 completion 大重构、CACHE-VFS、write-back、多节点失效、Redis/S3 生产化、
自动格式升级。

## 验收自检
- make -C kestrelfs：零警告
- cargo test + clippy -D warnings
- vng：STEP27_*_PASS + 既有回归
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-15 — Step 26 CACHE-ASYNC（Cursor ACCEPTED）

- 方案：rwsem 并行同步 BIO（非异步 completion）；ABI v11；format v4。
- 测试：`STEP26_CACHE_ASYNC_PASS`；peak 串行 1 / 并行 8；约 2.04×；回归全过。
- Commit：随 Cursor 本轮验收推送（见 `git log -1`）。

### 2026-09-14 — Step 25 CACHE-TXN（Cursor ACCEPTED）

- 单页 intent journal；format v4。
