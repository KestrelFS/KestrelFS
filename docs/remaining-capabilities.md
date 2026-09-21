# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-21，Codex（Step 55 常规双包实现完成，等待 Cursor 验收）
>
> 用途：供 Cursor 与 Codex 维护尚未完成的产品能力、优先级、方案决策、**当前可执行提示词**和验收结果。
> 本文是规划与协作入口，不替代 `HANDOFF.md` 的已验收事实。发生冲突时，按
> “代码与测试结果 → `HANDOFF.md` 已验收状态 → 本文规划”判断，并立即修正文档。
>
> **文档分工**
> - `HANDOFF.md`：已验收事实、ABI/format、测试配方与站立规则。
> - 本文：未完成能力、优先级、决策记录、**§8 当前 Codex 提示词**、**§9 实现汇报日志**。
> - 不另开指挥文档；人类只需让 Codex「读 `docs/remaining-capabilities.md` §8 并执行」。
>
> **体量约定**：自 Step 50 起常规双包 ≈ **2×** 既往单步。Step 54 过夜大包为一次性例外；
> **自 Step 55 起恢复常规双包体量**，不要再按过夜大包扩写。

## 1. 当前基线

- Phase 4 至 Step 54（含 session fencing、orphan peek/ack、splice/sendfile、WRITE_DATA
  prestage）均已验收。
- 已验收 IPC ABI **v23**，cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。
- 配置权威表：`docs/configuration.md`；粗测方法：`docs/perf-baseline.md`。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–30 | … + OVERNIGHT MEGA | Step 24–54 | **ACCEPTED** | 过夜四项已落地 |
| 31 | POSIX-DTYPE + MKNOD-MIN | Step 55（双包） | **REVIEW** | 两项实现与自检完成，见 §9 |
| — | 其它 | — | PROPOSED | 视需要并入后续双包 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

读缓存、异步 hit、write-behind、splice/sendfile、WRITE_DATA prestage 已 ACCEPTED。

## 4. VFS / POSIX

### Step 54 — OVERNIGHT MEGA

- 状态：`ACCEPTED`（Cursor，2026-09-21）
- 实现：session fencing；orphan peek/ack（ABI v23）；splice/sendfile；write prestage。

### Step 55 — POSIX-DTYPE + MKNOD-MIN（双包 ≈ 2×）

- 状态：POSIX-DTYPE `REVIEW`；MKNOD-MIN `REVIEW`
- 目标 A：readdir 报告正确 `d_type`（不再一律 `DT_UNKNOWN`）
- 目标 B：开放**受限** `mknod`：仅允许创建与 Step 50 whiteout 相同的 `S_IFCHR 0:0`
  marker（需 `CAP_MKNOD`）；拒绝其它设备节点
- 范围：见 §8。

## 5. 运维、测试与文档

配置表已落地；本步若新增 CLI/参数须同步 `docs/configuration.md`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-21 | Cursor | DIST-NOTIFY + REDIS-HARDEN | Step 53 验收 | **ACCEPTED** |
| 2026-09-21 | Cursor | OVERNIGHT MEGA | 选定 Step 54 过夜大包 | **DECIDED** |
| 2026-09-21 | Codex | OVERNIGHT MEGA | Step 54 四项开工 | **IMPLEMENTING** |
| 2026-09-21 | Codex | OVERNIGHT MEGA | Step 54 实现与自检完成 | **REVIEW**；见 §9 |
| 2026-09-21 | Cursor | OVERNIGHT MEGA | Step 54 验收 | **ACCEPTED**；203 tests + Cursor LEASE/VFS_PIPE PASS |
| 2026-09-21 | Cursor | POSIX-DTYPE + MKNOD-MIN | 选定 Step 55；体量回归常规双包 | **DECIDED**；见 §8 |
| 2026-09-21 | Codex | POSIX-DTYPE + MKNOD-MIN | Step 55 两项开工 | **IMPLEMENTING**；严格按 §8 双包范围 |
| 2026-09-21 | Codex | POSIX-DTYPE + MKNOD-MIN | Step 55 实现与自检完成 | **REVIEW**；见 §9 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 55 **不要**做完整块/字符设备、FIFO/socket 全集、udev 集成、或过夜级第四五项。
- 不要把 Step 55 再扩成大包。

## 8. 当前 Codex 提示词（Step 55，常规双包 ≈ 2×）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md、docs/remaining-capabilities.md（§2/§6/§8）、docs/configuration.md。
HEAD 应含 Step 54（ABI v23）。注意：§8 为常规双包（≈ 2×），不是过夜大包。

开工时：两项均 → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 内核/mount 验证只在 vng guest + loop；禁止宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 改 kestrelfs/*.c 或依赖 mount：必须 vng
- 默认 cargo test 可不依赖外部服务

## 目标：Step 55 — POSIX-DTYPE + MKNOD-MIN（双包）

### A) POSIX-DTYPE
1. readdir/getdents 对普通文件、目录、符号链接、whiteout marker 返回正确 d_type
   （不再一律 DT_UNKNOWN）
2. 与 lookup/stat 对 whiteout `S_IFCHR 0:0` 的识别一致
3. vng：STEP55_DTYPE_*_PASS，覆盖至少 file/dir/symlink/whiteout

### B) MKNOD-MIN
1. 开放受限 `.mknod`：仅允许创建 whiteout 风格的字符设备 marker（major=0, minor=0，
   S_IFCHR）；其它类型/设备号返回 -EPERM 或 -EOPNOTSUPP（择一并测锁）
2. 需要 CAP_MKNOD；持久化到 Mem/File/Redis MetaStore；重启后 lookup/stat/readdir 仍可见
3. 与既有 RENAME_WHITEOUT 语义兼容，不破坏 Step 50 回归
4. vng：STEP55_MKNOD_*_PASS（成功创建 marker；拒绝任意块设备/FIFO 等）

## 要求
1. make -C kestrelfs 零警告；cargo test + clippy -D warnings 不回归
2. STEP55_*_PASS 覆盖 A+B；回归 Step 50 WHITEOUT 与 Step 54 VFS_PIPE 关键路径
3. ABI：若仅内核 VFS/meta 语义变化且无共享内存/ioctl/opcode 布局变化，可保持 v23；
   有布局变化须 bump
4. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
5. 不要擅自 commit/push；不要扩成第三/第四大项

## 明确不做
完整 mknod 设备节点、FIFO/socket、udev、range lease、io_uring、过夜级扩包。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + Step 55 vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-21 — Step 55 POSIX-DTYPE + MKNOD-MIN（Codex REVIEW）

- A / POSIX-DTYPE：`READDIR_DATA` 条目从 10-byte header 扩为 12 bytes：
  `inode_id@0`、`name_len@8`、Linux `DT_*@10`、zero reserved@11，名字从 offset 12
  开始。MetaStore readdir 在同一 metadata snapshot 中返回 inode/name/mode，避免
  Redis 每项额外 lookup 及类型竞态；daemon 将 regular/directory/symlink/whiteout
  编为 `DT_REG/DT_DIR/DT_LNK/DT_CHR`，内核拒绝未知类型或非零 reserved。
- B / MKNOD-MIN：目录 inode ops 新增 `.mknod`，显式要求 `CAP_MKNOD`，且只接受
  `S_IFCHR` 与 `rdev=0:0`；block、FIFO 和其它字符设备均返回 `EOPNOTSUPP`。
  合法请求复用 `CREATE_DATA` 的既有 mode 字段，不新增 opcode；daemon/MetaStore 只将
  精确 `S_IFCHR` 建成 mode-000、size-0、nlink-1 whiteout marker。Mem/File/Redis 均
  复用原子 create 持久化路径，与 `RENAME_WHITEOUT` 的 inode 表示完全一致。
- ABI/format：因 `READDIR_DATA` record layout 改变，IPC ABI **v23 → v24**；共享内存
  总大小、opcode、ioctl 与 event payload 不变，cache format 保持 **v4**。
- 测试：默认 `cargo test` **206 passed**；真实 Redis
  `redis_url_gated_full_semantics_and_restart` **1 passed**；clippy
  `--all-targets -- -D warnings` 通过；`make -C kestrelfs` 零警告。
  vng+loop 输出 `STEP55_DTYPE_INITIAL_PASS`、`STEP55_DTYPE_RESTART_PASS`、
  `STEP55_MKNOD_RESTRICT_PASS`、`STEP55_MKNOD_CAP_PASS`、
  `STEP55_MKNOD_RESTART_PASS`、`STEP55_POSIX_DTYPE_MKNOD_PASS`（umount 56 ms）。
  回归输出 `STEP50_DOUBLE_PACK_PASS`（含 WHITEOUT，umount 51 ms）与
  `STEP54_VFS_PIPE_PASS`（umount 156 ms）。所有 insmod/mount/loop/cache 操作仅在
  vng guest，未触碰宿主机模块、zvol 或挂载。
- 风险与未做：`d_type` 只覆盖当前可持久化的四种 inode 类型，遇到损坏/未来未知
  mode 会 `EPROTO/EIO` fail closed。mknod 权限位被规范化为与 rename whiteout 相同的
  mode-000 marker；没有实现普通字符/块设备、FIFO、socket、udev 或设备 I/O。

### 2026-09-21 — Step 54 OVERNIGHT MEGA（Cursor ACCEPTED）

- session fencing；orphan peek/ack（ABI v23）；splice/sendfile；WRITE_DATA prestage。
- 验证：203 tests；clippy / make 干净；Redis 三项 gate PASS；
  `STEP54_VFS_PIPE_PASS`（staged_delta=2097152，submissions=512，umount 130 ms）；
  `STEP54_LEASE_PASS`（notify latency 10 ms；fenced writer）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-21 — Step 54 OVERNIGHT MEGA（Codex REVIEW）

- A / DIST-LEASE-MIN：RedisMetaStore 每实例注册
  `<prefix>:meta:v2:sessions:<32-hex-id>`，值为同一 id，默认 TTL 3000 ms，按 TTL/3
  用“token 相同才 PEXPIRE”的 Lua 心跳续约。新增 `--redis-session-ttl-ms`（最小
  300）；所有 mutation Lua 及语义失败的 revision 确认都校验 session key/token，
  缺失或冲突返回 `MetaError::StaleSession`，内核收到 `ESTALE`，已失效 session 不会
  自动复活。Step 42 durable dirty log、Step 53 Pub/Sub + 100 ms poll 均保留。
  这是 writer session/fail-closed fencing 雏形，不是 fencing generation、读 lease、
  range lease、跨机锁或线性一致协议。
- B / ORPHAN-SWEEP：没有采用“session TTL 过期即可删除”的初版思路，因为 daemon
  消亡不能证明内核旧 fd 已关闭。最终方案只接受更强的内核证据：mount-local
  `open_handles==0` 且 last-close `FINALIZE_ORPHAN` 失败时，inode 进入模块生命周期
  retry list。ABI v23 新增 PEEK/ACK 两个 ioctl；daemon 启动及每秒回放，metadata
  finalize 成功（或已 NotFound）后 ack，再交既有 durable GC。peek 后崩溃仍会幂等
  重试；活 fd 从不入队。rmmod 会丢失未处理队列，跨 mount 引用仍未聚合，因此没有
  扩大成全局 open-ref 协议。
- C / VFS-SPLICE：动态普通文件注册 `filemap_splice_read` 与
  `iter_file_splice_write`，覆盖 file→pipe、pipe→file 和 sendfile；仍使用现有 filemap
  aops、write-behind、fsync/MS_SYNC、cache/page-cache coherence 与 errseq 语义。
- D / ASYNC-WRITE-PIPE：writeback 在 folio lock/writeback 保护下先复制到私有
  staging，再竞争全局 bounce mutex；不同 inode/folio 可重叠准备，mutex 内缩小为
  cache ordering、staging→bounce memcpy 与同步 WRITE_DATA。新增只读诊断计数
  `write_pipe_staged_bytes`、`write_pipe_submissions`、`write_pipe_lock_wait_ns`、
  `write_pipe_lock_hold_ns`。没有新增异步 opcode，单 bounce 的 WRITE_DATA 仍只有一个
  in-flight，所有显式耐久/fail-closed 顺序保持。
- ABI/format：新增 daemon↔kernel orphan retry ioctl，IPC ABI **v22 → v23**；共享内存、
  event/opcode payload 均未改。cache format 保持 **v4**，无迁移或 wipe。
- 测试：默认 `cargo test` **203 passed**；clippy / make 通过；Redis gate 3 passed；
  `STEP54_LEASE_*` / `STEP54_VFS_PIPE_*` 及 Step 50/51/52 回归通过。所有 mount 仅 vng。
- 风险与未做：见 Codex 原稿；未做完整分布式锁/range lease/Cluster/io_uring 等。

### 2026-09-21 — Step 53 DIST-NOTIFY + REDIS-HARDEN（Cursor ACCEPTED）

- Pub/Sub + reconnect + rediss；Cursor `STEP53_DIST_NOTIFY_TWO_NODE_PASS` / TLS PASS。
