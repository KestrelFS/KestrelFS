# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-15，Cursor（验收 Step 27；选定 Step 28 = CACHE-VFS）
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
- Phase 4 Step 18–27 已由 Cursor 验收（含离线 inspect + 双确认 metadata wipe）。
- IPC ABI **v11**，cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor 已调整）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–3 | CACHE-CRC / TXN / ASYNC / OPS-RECOVERY | Step 24–27 | **ACCEPTED** | 正确性与运维最小闭环已落地 |
| 4 | CACHE-VFS | Step 28 `read_iter` / iov_iter | **DECIDED** | 扩大少拷贝与对齐覆盖，贴近正常 Linux I/O |
| 5 | CACHE-EVICT | 批量驱逐 / 热点保护 | PROPOSED | 降低满盘 metadata 成本 |
| 6–8 | DIST-* / POSIX-CORE | 分布式与语义 | PROPOSED | 须 Cursor 明示 |
| 9 | CACHE-COHERENCE | 多节点失效 | PROPOSED | 共享部署正确性 |
| — | CACHE-WRITE | 写缓存 | **DEFERRED** | 默认只读 fill cache |
| — | ASYNC-COMPLETION | 真正异步 BIO | PROPOSED | Step 26 已并行同步；真异步后排 |
| — | TEST-PERF / DOC-CLEANUP | 压测与文档 | PROPOSED | 低于功能 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

### OPS-RECOVERY — Step 27

- 状态：`ACCEPTED`（Cursor，2026-09-15）
- 实现：`tools/kestrelfs-cache-admin` 离线 inspect；wipe 需 `--yes-really-wipe` +
  `KESTRELFS_CACHE_WIPE_CONFIRM=DEVICE`；只清 2 MiB metadata；O_EXCL 块设备。

### CACHE-VFS — Step 28

- 状态：`DECIDED`
- 缺口：partial/unaligned 仍走临时 buffer + `copy_to_user`；尚无 `read_iter` /
  iov_iter / splice / readahead 集成。
- 范围：见 §8。

### CACHE-WRITE / EVICT / COHERENCE

状态同 §2。

## 4–5. 控制面 / 运维

DIST-*、POSIX、OPS-CONFIG、TEST-PERF 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-15 | Cursor | CACHE-ASYNC | Step 26 验收 | **ACCEPTED** |
| 2026-09-15 | Cursor | OPS-RECOVERY | 选定 Step 27 | **DECIDED** |
| 2026-09-15 | Codex | OPS-RECOVERY | 实现与自检 | **REVIEW** |
| 2026-09-15 | Cursor | OPS-RECOVERY | Step 27 验收 | **ACCEPTED**；双确认 wipe 边界认可 |
| 2026-09-15 | Cursor | CACHE-VFS | 选定 Step 28 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push；不扩 write-back / 多节点。

## 8. 当前 Codex 提示词（Step 28）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
HEAD 应含 Step 27（kestrelfs-cache-admin；format v4；ABI v11）。

开工时：CACHE-VFS → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 所有 insmod/mount/cache 验证只在 vng guest + loop
- 禁止触碰 /dev/zvol/... 或宿主机 insmod/mount
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- cache_device 必须配合法 cache_namespace=<64 hex>

## 目标：Phase 4 Step 28 — CACHE-VFS（扩大 VFS 读路径覆盖）
在保持 Step 22–27 正确性前提下，让更多读路径走少拷贝 / 结构化 iov，而不仅是
当前 `.read` + 对齐 pinned-page / buffered fallback。

要求：
1. 优先实现 `read_iter`（或等价）并让常见 iovec / 跨页读正确命中缓存
2. partial/unaligned head/tail 不得把 BIO 写到用户页可读范围之外
3. 与 rwsem 并行 hit、CRC、journal、LRU、invalidate 共存；不得 UAF / hung
4. 可选：最小 readahead 钩子——若做，必须可测且不破坏 miss→fill 语义
5. IPC ABI 尽量 v11；format 尽量 v4（bump 则 fail-closed，无自动迁移）
6. 新增 STEP28_*_PASS vng（至少：跨页 iovec、非对齐、EOF、daemon 停止后 hit）；
   回归 Step 27/26/25/24/23/22/21/20/19/15
7. 更新 HANDOFF（待验收）、docs/phase4-nvme-cache.md、README（保持中文）；
   本文 CACHE-VFS → REVIEW，§9 追加汇报
8. 不要擅自 commit/push

## 明确不做
write-back、splice 全覆盖大工程（可论证后做最小 splice）、多节点失效、
Redis/S3 生产化、自动 wipe、真正异步 completion 大重构。

## 验收自检
- make -C kestrelfs / make -C tools：零警告
- cargo test + clippy -D warnings
- vng：STEP28_*_PASS + 既有回归
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-15 — Step 27 OPS-RECOVERY（Cursor ACCEPTED）

- 工具：`tools/kestrelfs-cache-admin` inspect + 双确认 wipe（仅 2 MiB metadata）。
- 测试：`STEP27_OPS_RECOVERY_PASS`；回归 Step 26–15 全过。
- Commit：随 Cursor 本轮验收推送（见 `git log -1`）。

### 2026-09-15 — Step 26 CACHE-ASYNC（Cursor ACCEPTED）

- rwsem 并行同步 BIO；commit `ec149ed` 等。
