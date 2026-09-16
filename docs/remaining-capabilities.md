# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-16，Cursor（验收 Step 42；内核优先继续 Step 43 KERNEL-WRITE-ITER）
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

- Phase 4 cache、DIST、POSIX、CACHE-COHERENCE（含 Step 42 细粒度失效）（Step 24–42）均已验收。
- Redis metadata 为 v2 + durable dirty-inode 日志；正常变化按 inode 批量失效，失败回退全量。
- 已验收 IPC ABI **v20**，cache format **v4**。
- **战略**：后续步骤优先补齐内核 VFS/数据面（write_iter → fsync → aops/page cache → mmap → locks → cache-async）。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor：内核优先）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–18 | cache + DIST + POSIX + COHERENCE-FINE | Step 24–42 | **ACCEPTED** | 细粒度失效已落地 |
| 19 | KERNEL-WRITE-ITER | Step 43 | **DECIDED** | 内核优先第一步 |
| 20 | KERNEL-FSYNC | 真实 fsync/fdatasync/sync_fs | PROPOSED | 纠正空转刷盘 |
| 21 | KERNEL-AOPS | address_space + 读侧 page cache | PROPOSED | mmap 基础 |
| 22 | KERNEL-MMAP | 文件 mmap | PROPOSED | 应用兼容 |
| 23 | KERNEL-LOCKS | flock / POSIX locks | PROPOSED | 多进程共享 |
| 24 | KERNEL-CACHE-ASYNC | cache hit 异步 BIO | PROPOSED | 热路径性能 |
| — | WHITEOUT / DIST-IO / CACHE-WRITE | 其它 | PROPOSED/DEFERRED | 内核主线之后 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

CACHE-* 与 COHERENCE（粗+细）最小闭环已 ACCEPTED；`CACHE-WRITE` 仍 DEFERRED。

## 4. 控制面与内核

### COHERENCE-FINE — Step 42

- 状态：`ACCEPTED`（Cursor，2026-09-16）
- 实现：ABI v20 `INVALIDATE_CACHE_INODES`；Redis dirty log；失败全量回退。

### KERNEL-WRITE-ITER — Step 43

- 状态：`DECIDED`
- 目标：普通文件 `.write_iter`，与 `read_iter` 对称。
- 范围：见 §8。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-16 | Cursor | DIST-OBJECT | Step 41 验收 | **ACCEPTED** |
| 2026-09-16 | Cursor | COHERENCE-FINE | 选定 Step 42 | **DECIDED** |
| 2026-09-16 | Codex | COHERENCE-FINE | 实现与自检 | **REVIEW** |
| 2026-09-16 | 人类/Cursor | 战略 | 后续优先内核 | **是** |
| 2026-09-16 | Cursor | COHERENCE-FINE | Step 42 验收 | **ACCEPTED**；193 tests + Cursor vng PASS |
| 2026-09-16 | Cursor | KERNEL-WRITE-ITER | 选定 Step 43 | **DECIDED**；见 §8；内核主线继续 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 43 不顺手做 mmap/page cache、fsync 耐久协议、write-back、锁、异步 BIO、WHITEOUT。

## 8. 当前 Codex 提示词（Step 43）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。
>
> **基线**：HEAD 含 Step 42（ABI v20）。战略为内核优先，本步只做 write_iter。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
HEAD 应含 Step 42（ABI v20 COHERENCE-FINE）。战略：内核优先。

开工时：KERNEL-WRITE-ITER → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 一旦改 kestrelfs/*.c 或依赖 mount：必须 vng
- Redis/S3 门控测可选；默认 cargo test 不依赖外部服务

## 目标：Step 43 — KERNEL-WRITE-ITER
把普通文件写路径从 `file_operations.write` 升级为 `write_iter`，与已有 `read_iter` 对称，
便于后续 AIO/io_uring/向量写，并为 page cache 写路径打基础。

必做：
1. `kestrelfs_reg_file_ops` / writable ops：实现 `.write_iter`；保留或删除旧 `.write` 须在 §9 写清
2. 语义对齐现有写路径：O_APPEND、按 bounce 分片 WRITE_DATA、更新 i_size、触发 cache invalidate
3. 支持 writev/pwritev 类路径（经 VFS write_iter）；单测或 vng 覆盖向量写与普通 write
4. 错误与部分写语义写清；不要顺手做 fsync 耐久协议或 mmap

要求：
1. `make -C kestrelfs` 零警告；相关 cargo test 不回归
2. STEP43_*_PASS vng：普通写、writev、O_APPEND、写后读回、与 cache invalidate 兼容
3. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
4. 不要擅自 commit/push

## 明确不做
page cache/mmap、真实 fsync 协议、flock、async BIO、write-back、WHITEOUT、扩大 coherence。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + Step 43 vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-16 — Step 42 COHERENCE-FINE（Cursor ACCEPTED）

- ABI v20 `INVALIDATE_CACHE_INODES`；Redis durable dirty log（256 revision / 64 inode）；
  失败/溢出 fail closed 全量失效。
- 验证：193 tests；clippy / `make -C kestrelfs` 干净；Cursor 复跑 vng
  `STEP42_COHERENCE_FINE_PASS`（umount_ms=24）。
- Commit：随 Cursor 本轮验收推送；下一步内核优先 Step 43。

### 2026-09-16 — Step 42 COHERENCE-FINE（Codex REVIEW）

- 方案与窗口：没有采用“首个 daemon probe 后全局清空 Redis SET”，因为多 daemon 会
  相互吞掉通知。Redis v2 mutation 改为在同一个 Lua revision-CAS 事务内，将排序去重的
  dirty inode 写入 `<prefix>:meta:v2:dirty`，保留最近 256 个 revision。每个 daemon
  独立以最后一次成功 probe revision 为游标，合并 `(observed,current]`，因此推进本地
  游标等价于只清空自己的待处理集合。轮询仍为 100 ms，提交到 probe 前仍存在既有最终
  一致窗口；启动时仍先全量失效恢复索引。
- 上界与失败语义：单 revision 最多记录 64 inode；跨 revision 去重后也最多 64。
  单记录 overflow、gap 超过 256、任一记录缺失/损坏、累计超过 64 或 Redis probe 失败
  都走 `INVALIDATE_CACHE_ALL`。正常 probe 通过后才推进 daemon 游标；ioctl 失败令 daemon
  退出，批量内核退休中若 journal 失败则销毁剩余内存索引并禁用 cache，避免远端 mutation
  已生效后继续返回脏 hit。旧式 writer 只推进 revision 而不写 dirty record 时也会全量
  fail closed。Mem/File 仍返回 Disabled，不启用远端轮询。
- ABI/内核：IPC ABI **v19 → v20**；共享内存、opcode 和 cache format **v4** 均未改变。
  新增 520-byte `_IOW` `KESTRELFS_IOC_INVALIDATE_CACHE_INODES`。
- 单测：`193 passed`；Redis 门控测通过；clippy / make 干净。
- vng：`STEP42_*_PASS`（Codex umount 22 ms；Cursor 复跑 24 ms）。
- 备注：旧 Step 15/20 脚本在 truncate/O_TRUNC 见 `EOPNOTSUPP` 属既有 setattr 组合限制，
  非本步引入；未在本步顺手扩大。
- 未做：lease/pubsub、range 失效、Redis TLS、write-back、async BIO。

### 2026-09-16 — Step 41 DIST-OBJECT（Cursor ACCEPTED）

- 有界 GC delete worker；ObjectStore 长度完整性；ABI/format 未变。
