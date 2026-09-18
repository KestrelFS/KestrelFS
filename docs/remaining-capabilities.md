# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-18，Cursor（验收 Step 48；选定 Step 49 = CACHE-WRITE）
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

- Phase 4 至 Step 48（含 write_iter、fsync、aops、mmap、locks、cache hit 异步 BIO）均已验收。
- 已验收 IPC ABI **v21**，cache format **v4**。
- **战略**：内核优先主线（write_iter → fsync → aops → mmap → locks → cache-async）已完成；
  下一步进入写回缓存 / 其余 POSIX / 分布式能力。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–24 | … + CACHE-ASYNC | Step 24–48 | **ACCEPTED** | 内核优先主线完成 |
| 25 | CACHE-WRITE | Step 49 脏页写回 / 写缓存 | **DECIDED** | 解锁可写 MAP_SHARED 前置 |
| 26 | WHITEOUT | rename WHITEOUT | PROPOSED | POSIX 缺口 |
| 27 | DIST-IO | 分布式数据面增强 | PROPOSED | 多节点 |
| — | OPS-CONFIG / TEST-PERF / DOC-CLEANUP | 运维文档 | PROPOSED | 可并行 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

读缓存、fsync、异步 hit BIO 已 ACCEPTED；`CACHE-WRITE` 现为 Step 49。

## 4. 内核 VFS

### KERNEL-CACHE-ASYNC — Step 48

- 状态：`ACCEPTED`（Cursor，2026-09-18）
- 实现：hit BIO `submit_bio` + `end_io`；冷 folio 命中脱离 bounce 锁并发；
  `cache_async_hit_submissions` / `cache_async_hit_peak`。

### CACHE-WRITE — Step 49

- 状态：`DECIDED`
- 目标：为普通文件引入最小可用的脏页写回（或等价写缓存），使 fsync/munmap
  路径能把 page cache 脏数据提交到 daemon，并为后续可写 MAP_SHARED 铺路。
- 范围：见 §8。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-17 | Cursor | KERNEL-LOCKS | Step 47 验收 | **ACCEPTED** |
| 2026-09-17 | Cursor | KERNEL-CACHE-ASYNC | 选定 Step 48 | **DECIDED** |
| 2026-09-17 | Codex | KERNEL-CACHE-ASYNC | 实现与自检 | **REVIEW** |
| 2026-09-18 | Cursor | KERNEL-CACHE-ASYNC | Step 48 验收 | **ACCEPTED**；195 tests + Cursor vng PASS |
| 2026-09-18 | Cursor | CACHE-WRITE | 选定 Step 49 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 49 不顺手做完整分布式一致性、WHITEOUT、用户态 io_uring 导出、跨节点锁。
- 可写 MAP_SHARED 可在本步若自然落地则一并做；否则明确留到后续并保持拒绝。

## 8. 当前 Codex 提示词（Step 49）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md、docs/remaining-capabilities.md（§2/§6/§8）与 docs/phase4-nvme-cache.md。
HEAD 应含 Step 48（cache hit 异步 BIO）。内核优先主线已完成；本步做写回。

开工时：CACHE-WRITE → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 一旦改 kestrelfs/*.c 或依赖 mount：必须 vng
- Redis/S3 门控测可选；默认 cargo test 不依赖外部服务

## 目标：Step 49 — CACHE-WRITE（脏页写回 / 写缓存）
在读侧 page cache（Step 45/46）与 fsync 屏障（Step 44）之上，让普通文件的
**脏页**能够写回到 daemon（经现有 WRITE_DATA 或合理新路径），而不再只能
“同步 write_iter + 立刻清页”。

必做（择优其一并在 §9 写清）：
1. 最小 aops 写回：`write_begin`/`write_end` 或 `dirty_folio` + `writepages`/
   `writepage`（按当前内核 API），使 buffered write 或显式脏页可落盘到 daemon
2. 或：保留 `.write_iter` 为主要用户写路径，但允许 mmap/私有脏页经 writeback
   提交；须与 Step 44 fsync 联动——`fsync`/`fdatasync` 等待脏页写回完成后再做
   后端对象/元数据屏障
3. 写后 NVMe 读缓存与 page cache 一致性：invalidate/更新策略写清，禁止脏读
4. 若仍拒绝可写 MAP_SHARED：保持 `EOPNOTSUPP` 并文档化；若本步启用，须有 vng
5. 失败路径 fail closed；不把“仅内存脏页”假装成已持久

要求：
1. `make -C kestrelfs` 零警告；cargo test 不回归
2. STEP49_*_PASS vng：buffered/mmap 路径产生脏页 → fsync 后重启 daemon 读回一致；
   写回失败可见错误；与 Step 48 并发 hit 不回归
3. ABI 若需扩展须 bump 并断言；否则保持 v21
4. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
5. 不要擅自 commit/push

## 明确不做
WHITEOUT、跨节点分布式锁/lease、完整用户态 io_uring 导出、自动 wipe。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + Step 49 vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-18 — Step 48 KERNEL-CACHE-ASYNC（Cursor ACCEPTED）

- hit BIO `submit_bio` + `end_io`；冷 folio 命中脱离 bounce 锁并发；
  `cache_async_hit_submissions` / `cache_async_hit_peak`。
- 验证：195 tests；clippy / make 干净；Cursor 复跑 vng
  `STEP48_KERNEL_CACHE_ASYNC_PASS`（umount_ms=31；`submissions=17 peak=13`；
  checksum fallback failures=1）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-17 — Step 48 KERNEL-CACHE-ASYNC（Codex REVIEW）

- cache hit buffered/pinned BIO 改为异步 completion；读侧 rwsem 覆盖完成与 CRC；
  冷 folio 仅 miss 时取 bounce 锁并重查。
- 自检：195 tests；vng `STEP48_*_PASS`（Codex umount 335 ms；peak=15）。
- 旧 Step 26 在 page cache 后断言失效；旧 Step 24 rmmod-in-use 未定位；
  本步独立 checksum 故障注入通过。
- ABI v21 / format v4 未变。未做 writeback、可写 MAP_SHARED、用户态异步读。

### 2026-09-17 — Step 47 KERNEL-LOCKS（Cursor ACCEPTED）

- 本地 flock/POSIX/OFD；Cursor vng `STEP47_KERNEL_LOCKS_PASS`（umount_ms=32）。
