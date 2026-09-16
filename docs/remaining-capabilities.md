# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-16，Cursor（验收 Step 41；选定 Step 42 = COHERENCE-FINE）
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

- Phase 4 cache、DIST（含 Step 41 ObjectStore GC worker）、POSIX、CACHE-COHERENCE 最小闭环（Step 24–41）均已验收。
- Redis metadata 为 v2 分记录 HASH/SET + Lua revision-CAS；点查定向读。
- Redis daemon 每 100 ms 轮询 durable revision，经 ABI v14 **全 cache** 失效（Step 35）。
- 已验收 IPC ABI **v19**，cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor 已调整）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–17 | cache + DIST + POSIX + DIST-OBJECT | Step 24–41 | **ACCEPTED** | GC 慢路径已隔离 |
| 18 | COHERENCE-FINE | Step 42 细粒度 cache 失效 | **DECIDED** | 收窄 Step 35 全量 wipe |
| — | RENAME_WHITEOUT / ASYNC / DIST-IO | 其它 | PROPOSED | 可后补 |
| — | CACHE-WRITE | 写缓存 | **DEFERRED** | 默认只读 fill cache |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

CACHE-* 与 CACHE-COHERENCE 最小闭环已 ACCEPTED；`CACHE-WRITE` 仍 DEFERRED。
Step 42 目标是把“revision 变化 → 全 cache 失效”推进到可按 inode（或小批量）失效。

## 4. 控制面、对象存储与 POSIX

### DIST-OBJECT — Step 41

- 状态：`ACCEPTED`（Cursor，2026-09-16）
- 实现：有界 GC delete worker；ObjectStore 长度完整性；ABI/format 未变。

### COHERENCE-FINE — Step 42

- 状态：`DECIDED`
- 目标：远端 metadata mutation 后尽量按 inode 失效本地 cache，仅在无法枚举时回退全量。
- 范围：见 §8。

### WHITEOUT / ASYNC / DIST-IO（同步 put/get 离环）

仍为 `PROPOSED`。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-16 | Cursor | POSIX-UTIMES | Step 40 验收 | **ACCEPTED** |
| 2026-09-16 | Cursor | DIST-OBJECT | 选定 Step 41 | **DECIDED**；见 §8 |
| 2026-09-16 | Codex | DIST-OBJECT | 开始实现 Step 41 | **IMPLEMENTING** |
| 2026-09-16 | Codex | DIST-OBJECT | 实现与自检完成 | **REVIEW**；191 tests、MinIO 2 tests、ABI 未变 |
| 2026-09-16 | Cursor | DIST-OBJECT | Step 41 验收 | **ACCEPTED**；191 tests + clippy |
| 2026-09-16 | Cursor | COHERENCE-FINE | 选定 Step 42 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 42 不顺手做 WHITEOUT、write-back、真正 async BIO、Redis TLS、生产级 pub/sub lease。

## 8. 当前 Codex 提示词（Step 42）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）以及
docs/phase4-nvme-cache.md 中失效相关设计。
HEAD 应含 Step 41（DIST-OBJECT；IPC ABI 仍为 v19）。

开工时：COHERENCE-FINE → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 一旦改 kestrelfs/*.c 或依赖 mount：必须 vng
- Redis/S3 门控测可选；默认 cargo test 不依赖外部服务

## 目标：Step 42 — COHERENCE-FINE（细粒度 cache 失效）
在 Step 35“Redis durable revision 轮询 → 全 cache 失效”之上，尽量改为按受影响 inode
失效，减少无关命中被清掉。探测失败或脏集不可用时仍必须 fail closed 回退全量失效。

必做：
1. **脏集来源**：为 Redis MetaStore 记录“自上次成功 probe 以来变更的 inode 集合”
   （或等价：revision 附带有界 dirty-inode 列表）。容量溢出 / 无法枚举时标记为
   “必须全量失效”。Mem/File 单机路径可不做，但勿破坏既有行为。
2. **内核接口**：新增或扩展 daemon→kernel ioctl，支持批量按 inode 失效
  （可复用既有单 inode invalidate 原语）。保留 `INVALIDATE_CACHE_ALL` 作回退。
   C/Rust 同步；若布局扩展则 bump ABI（预期 v20）+ 编译期断言。
3. **daemon 轮询**：revision 变化时若脏集可得且未溢出 → 批量 inode 失效并清空脏集；
   否则走全量失效（与 Step 35 同等安全）。在 §9 写清窗口、上界与失败语义。
4. 与 rewrite/truncate/unlink 本地失效路径兼容；不要静默漏失效。

要求：
1. 单测覆盖：脏集累积、溢出→全量、probe 失败→全量、批量 ioctl 编解码
2. STEP42_*_PASS vng（需要 Redis 时可在 vng 内起临时 Redis，或门控说明）：
   远端 mutation 后，未改 inode 的 cache 仍可命中；被改 inode 必须失效；失败回退全量
3. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
4. 不要擅自 commit/push

## 明确不做
WHITEOUT、write-back、真正 async BIO completion、Redis TLS、生产级 pub/sub lease、
客户端加密/压缩、自动 wipe、iget5。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + 相关 vng
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
