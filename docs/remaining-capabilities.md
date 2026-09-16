# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-16，Cursor（战略调整：后续优先内核 VFS/数据面；Step 42 搁置验收）
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

- Phase 4 cache、DIST（含 Step 41）、POSIX、CACHE-COHERENCE 最小闭环（Step 24–41）均已验收。
- Step 42 COHERENCE-FINE 已在工作树实现（ABI v20），**人类决定暂缓验收**，不阻塞内核优先路线。
- 已验收线上基线仍为 IPC ABI **v19** / cache format **v4**（Step 42 未合入前）。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED` / `PARKED`。

## 2. 建议路线（Cursor 2026-09-16 调整为内核优先）

**战略**：后续步骤优先补齐内核 VFS/数据面（page cache、mmap、fsync、write_iter、锁、异步 cache），
再回头做细粒度 coherence / WHITEOUT / DIST-IO。理由：产品差异点是「缓存命中走内核」；
FUSE 式控制面已有 JuiceFS，KestrelFS 必须先把 `.ko` 做到应用可依赖。

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–17 | cache + DIST + POSIX + DIST-OBJECT | Step 24–41 | **ACCEPTED** | 已合入 main |
| — | COHERENCE-FINE | Step 42 细粒度失效 | **PARKED** | 工作树已实现；暂缓验收 |
| 18 | KERNEL-WRITE-ITER | Step 43 写路径 `write_iter` | **DECIDED** | 与已有 `read_iter` 对称，内核优先第一步 |
| 19 | KERNEL-FSYNC | 真实 fsync/fdatasync/sync_fs | PROPOSED | 纠正空转刷盘语义 |
| 20 | KERNEL-AOPS | address_space + 读侧 page cache | PROPOSED | mmap/应用兼容基础 |
| 21 | KERNEL-MMAP | 文件 mmap | PROPOSED | 依赖 aops |
| 22 | KERNEL-LOCKS | flock / POSIX locks | PROPOSED | 多进程共享 |
| 23 | KERNEL-CACHE-ASYNC | cache hit 异步 BIO | PROPOSED | 热路径性能 |
| — | CACHE-WRITE / WHITEOUT / DIST-IO / COHERENCE 恢复 | 其它 | PROPOSED/DEFERRED | 内核主线之后 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

CACHE-* 最小闭环已 ACCEPTED；`CACHE-WRITE` 仍 DEFERRED。
COHERENCE-FINE（Step 42）PARKED，不作为当前实现目标。

## 4. 控制面、对象存储与 POSIX

DIST/POSIX 主线暂停扩张；内核缺口优先。

### COHERENCE-FINE — Step 42

- 状态：`PARKED`（人类，2026-09-16）
- 工作树已有实现与自检，但先不验收、不作为 §8 目标。

### KERNEL-WRITE-ITER — Step 43

- 状态：`DECIDED`
- 目标：普通文件写路径从 `.write` 升级为 `.write_iter`，与 `read_iter` 对称。
- 范围：见 §8。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-16 | Cursor | DIST-OBJECT | Step 41 验收 | **ACCEPTED** |
| 2026-09-16 | Cursor | COHERENCE-FINE | 选定 Step 42 | **DECIDED** |
| 2026-09-16 | Codex | COHERENCE-FINE | 实现与自检 | **REVIEW**（工作树；未合入） |
| 2026-09-16 | 人类/Cursor | 战略 | 后续是否优先内核 | **是**；内核 VFS/数据面优先于 coherence/DIST 扩张 |
| 2026-09-16 | Cursor | COHERENCE-FINE | Step 42 验收 | **PARKED**；暂缓 |
| 2026-09-16 | Cursor | KERNEL-WRITE-ITER | 选定 Step 43 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 43 不顺手做 mmap/page cache、fsync 耐久协议、write-back、锁、异步 BIO。
- 不要继续扩大已 PARKED 的 Step 42；若工作树仍有其改动，实现 Step 43 前先与人类确认基线（建议：基于已验收 Step 41 的 clean tree，或先 stash Step 42）。

## 8. 当前 Codex 提示词（Step 43）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。
>
> **基线注意**：优先在 **已验收 Step 41（`6204db2`）clean tree** 上开工；若本地仍有未合入的 Step 42 改动，先 stash/另开 worktree，不要混在一起。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
战略：内核优先。不要做 COHERENCE-FINE / Redis dirty-log。
基线：已验收 Step 41（ABI v19）。若工作树混有 Step 42，先停下来在 §9 说明并等待，不要混改。

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
4. 不要擅自 commit/push；不要把 PARKED 的 Step 42 改动混进本步

## 明确不做
page cache/mmap、真实 fsync 协议、flock、async BIO、write-back、WHITEOUT、COHERENCE-FINE。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + Step 43 vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-16 — Step 41 DIST-OBJECT（Cursor ACCEPTED）

- 有界 GC delete worker（队列 32 / 每项 64 keys / 并发 4）；ack 仅在串行 metadata 线程。
- ObjectStore 长度完整性（exact/range + S3 Content-Length）；multipart 跳过有理由。
- 验证：191 tests；clippy 干净；ABI/format 未变；纯 daemon，未要求 vng。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-16 — Step 41 DIST-OBJECT（Codex REVIEW）

- 慢路径隔离：新增 `daemon/src/gc_worker.rs`。生产 daemon 的 unlink、rename-overwrite、
  truncate 与 final-close 只做非阻塞提交，不再等待 ObjectStore delete。worker 请求队列
  固定 32 项、每项最多 64 keys（最多 2048 个排队 key），单次最多 4 个并发 delete；
  in-flight key 去重。`try_send` 背压时释放 in-flight 标记，key 仍留在 MetaStore durable
  GC queue，由 1–60 秒退避扫描重试。
- durability 边界：worker 只访问 ObjectStore，删除结果回到串行 IPC/metadata 线程后才
  `acknowledge_garbage`。delete 失败、ack 失败、队列满或 delete/ack 间进程退出都不会
  丢 durable key；成功 delete 后未 ack 的重放依赖既有幂等 delete。此边界避免后台任务
  与请求线程并发写 FileMetaStore 的固定 `meta.json.tmp`。
- 完整性：ObjectStore 增加 `Integrity` 错误与 exact/range length API。slice 读取要求
  对象至少覆盖当前引用范围且不超过 4 MiB 物理块；短截断和超大对象均 fail closed，
  truncate 后不可见的旧对象尾部允许保留。S3 在收 body 前校验 SDK 暴露的
  `Content-Length`，收完后再校验 header/body 及期望范围；错误最终向内核返回 EIO。
  bootstrap seed 也要求精确 512 字节，已有损坏对象不再被当成“缺失”静默覆盖。
- 单测：`191 passed; 0 failed`。新增覆盖有界队列背压与每请求 key 上限、并发峰值不超过
  4、delete 失败回到 durable queue 后成功重试/ack、MemObjectStore 长短不匹配，以及
  slice 短对象读取 fail closed；既有 truncate/COW 测试证明合法旧尾部仍可读当前前缀。
- 静态/构建：`cargo clippy --all-targets -- -D warnings` 通过；`make -C kestrelfs`
  成功且零 warning。真实 MinIO（`192.168.18.253:9000`）门控测试 `2 passed; 0 failed`
  （精确长度/错误长度、覆盖读写、幂等 delete、unlink GC）。凭据仅经环境变量注入。
- vng：本步没有改 `kestrelfs/*.c`、IPC 或 mount 行为，按 §8 未运行 vng；未在物理机
  执行 insmod/mount，也未触碰 zvol。IPC ABI 保持 v19，cache format 保持 v4。
- 风险/限制：READ/WRITE 的 S3 put/get 仍在同步 IPC 慢路径，本步只隔离 GC delete；
  durable GC queue 本身仍无总容量/dead-letter/运维限额。多 daemon 可能对同一 key 做
  重复幂等 delete。长度检查防止截断/越界对象，不提供内容 checksum 或加密真实性。
- 未做：multipart PUT（当前 bounce 单次写最多 16 KiB，对 4 MiB Block 路径无可测收益）；
  WHITEOUT、write-back、精细 coherence、Redis TLS、客户端加密/压缩、自动 wipe、iget5，
  以及任何 commit/push。

### 2026-09-16 — Step 40 POSIX-UTIMES（Cursor ACCEPTED）

- ABI v19 秒级 atime/mtime；basic/time layout 互斥；`GETATTR_TIMES`；orphan futimens。
- 验证：185 tests；clippy / `make -C kestrelfs` 干净；Cursor 复跑 vng
  `STEP40_POSIX_UTIMES_PASS`（umount_ms=23）。
- Commit：随 Cursor 本轮验收推送。
