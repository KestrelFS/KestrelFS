# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-21，Cursor（Step 53 已验收；发布 Step 54 **过夜大包 ≈ 4× 既往双包**）
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
> **体量约定**：自 Step 50 起常规双包 ≈ 2× 既往单步；**Step 54 为过夜大包 ≈ 4× 既往双包
> （约 8× 单步）**，允许一次落地多条独立能力，仍要求完整自检与 §9 汇报。

## 1. 当前基线

- Phase 4 至 Step 53（含 Pub/Sub 失效提示、Redis 重连、`rediss://` 私有 CA）均已验收。
- 已验收 IPC ABI **v22**，cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。
- 配置权威表：`docs/configuration.md`；粗测方法：`docs/perf-baseline.md`。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–29 | … + DIST-NOTIFY/REDIS-HARDEN | Step 24–53 | **ACCEPTED** | 推送失效 + Redis 硬化已落地 |
| 30 | OVERNIGHT MEGA | Step 54（大包） | **DECIDED** | 过夜一次推进 lease 雏形、orphan 清扫、splice、写路径流水线 |
| — | 其它 | — | PROPOSED | 视 Step 54 验收后拆分 |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

读缓存、异步 hit、write-behind、可写 MAP_SHARED、DIST-IO、DIST-NOTIFY 已 ACCEPTED。

## 4. 分布式 / 内核 / 运维

### Step 53 — DIST-NOTIFY + REDIS-HARDEN（双包）

- 状态：`ACCEPTED`（Cursor，2026-09-21）
- 实现：Pub/Sub revision 提示 + poll 对账；ConnectionManager；`rediss://` + `--redis-ca-cert`。

### Step 54 — OVERNIGHT MEGA（大包 ≈ 4× 双包）

- 状态：四项均为 `DECIDED`（见 §8）
- A DIST-LEASE-MIN：最小会话心跳 / fencing 雏形（**不是**完整分布式锁）
- B ORPHAN-SWEEP：泄漏 orphan 的安全自动清扫
- C VFS-SPLICE：普通文件 splice/sendfile 可行路径
- D ASYNC-WRITE-PIPE：WRITE_DATA / bounce 路径去阻塞化或跨 folio 流水线最小落地
- 范围与验收：严格见 §8；可按 A→B→C→D 顺序实现，但 **四项都要做完** 才标 REVIEW。

## 5. 运维、测试与文档

配置表与 perf 基线已落地；本步新增项必须同步 `docs/configuration.md` 与 HANDOFF 已知限制。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-20 | Cursor | DIST-IO + TEST-PERF | Step 52 验收 | **ACCEPTED** |
| 2026-09-20 | Cursor | DIST-NOTIFY + REDIS-HARDEN | 选定 Step 53 | **DECIDED** |
| 2026-09-20 | Codex | DIST-NOTIFY + REDIS-HARDEN | Step 53 开工 | **IMPLEMENTING** |
| 2026-09-21 | Codex | DIST-NOTIFY + REDIS-HARDEN | 实现与自检完成 | **REVIEW**；见 §9 |
| 2026-09-21 | Cursor | DIST-NOTIFY + REDIS-HARDEN | Step 53 验收 | **ACCEPTED**；202 tests + Cursor TLS/DIST-NOTIFY PASS |
| 2026-09-21 | Cursor | OVERNIGHT MEGA | 选定 Step 54 过夜大包 | **DECIDED**；≈ 4× 双包；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 54 **不要**宣称线性一致、完整 range lease、跨机强制锁、Redis Cluster/Sentinel、
  生产级 io_uring 导出或安全擦除。宁可缩小某子项实现面，也不要编造语义。

## 8. 当前 Codex 提示词（Step 54，过夜大包 ≈ 4× 既往双包）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。
> 本步 intentionally 很大：按 A→B→C→D 推进；全部完成后再标 REVIEW。若某子项阻塞，
> 在 §9 写清阻塞原因与已完成子项，仍尽量交付可测增量，但默认目标是四项齐全。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md、docs/remaining-capabilities.md（§2/§6/§7/§8）、docs/configuration.md、
docs/phase4-nvme-cache.md、docs/perf-baseline.md。HEAD 应含 Step 53（Pub/Sub + Redis harden）。
注意：§8 为过夜大包（约 4× 既往双包）。不要 commit/push。README 保持中文。

开工：四项均 → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 内核/mount 验证只在 vng guest + loop；禁止宿主机 zvol / 物理机 insmod
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 改 kestrelfs/*.c 或依赖 mount：必须 vng
- Redis/S3 门控按需；默认 cargo test 可不依赖外部服务
- Dist 场景继续两并行 vng + 共享 Redis（对象面沿用 Step 52/53 配方）

## 目标：Step 54 — OVERNIGHT MEGA（四项）

### A) DIST-LEASE-MIN（最小会话 / fencing 雏形）
在 Step 42/53 的 revision + Pub/Sub 之上增加**最小**多 daemon 会话语义：
1. 每个 Redis MetaStore daemon 注册带 TTL 的 session（心跳续约）；key/布局写入
   docs/configuration.md 与 §9
2. 写路径：mutation 提交时绑定/检查 session；过期或冲突时 fail closed（返回明确错误），
   不静默写成功
3. 读/失效：对端可观察 session 丢失并触发保守失效或拒绝过期 writer 的后续写
4. vng：两节点场景证明 (i) 正常心跳下 Step 53 通知/可见性仍成立
   (ii) 强杀/停心跳后对端或本地能在有界时间内进入安全状态（记录 STEP54_LEASE_*_PASS）
5. **明确不是**：跨机字节锁、range lease、线性一致、fencing token 全协议。名称可用
   lease/session，但文档必须写清边界

### B) ORPHAN-SWEEP（泄漏 orphan 安全清扫）
针对 HANDOFF 已知限制（final-close IPC 失败可安全泄漏 nlink=0 orphan）：
1. daemon 启动与/或周期任务扫描可证明「无 open 引用」的 orphan，幂等进入既有 GC
2. 绝不可在「可能仍有活 fd」时删除；证据不足则跳过并打日志
3. 覆盖 Mem/File/Redis 至少一条真实路径；Redis 路径优先
4. vng 或集成测：制造泄漏 orphan → sweep → 对象/元数据回收；负例：仍 open 时不删
   （STEP54_ORPHAN_SWEEP_*_PASS）
5. 不引入自动 wipe cache；不改 v4 盘格式除非绝对必要（若必要须 bump 并论证）

### C) VFS-SPLICE（splice/sendfile 可行路径）
1. 为普通文件补齐可行的 splice/sendfile（或明确文档化仍不支持的子集 + 实现最大子集）
2. 优先：从 KestrelFS 文件 splice 到 pipe / 从 pipe splice 进来；与 page-cache /
   write-behind / cache hit 语义兼容，失败时正确回退或返回 errno
3. vng：STEP54_SPLICE_*_PASS，含数据校验；不要求打满所有零拷贝边角
4. 若内核版本/树外限制导致只能部分实现：实现可读路径 + 写清限制，但必须有自动化测

### D) ASYNC-WRITE-PIPE（写回路径去阻塞 / 流水线）
针对 bounce 锁下同步 WRITE_DATA 等已知限制，做**最小**改进（择优，可组合）：
1. 缩短 bounce mutex 持有区间，或允许不同 folio/inode 的 WRITE_DATA 重叠提交
2. 保持 fsync/MS_SYNC/syncfs/coherence-before-invalidate 的耐久与 fail-closed 语义
3. 可观测：至少一项计数/日志证明并发或缩短临界区（类似 cache_async_hit_peak）
4. vng：STEP54_WRITE_PIPE_*_PASS；回归 Step 51 write-behind 与 Step 50 MAP_SHARED
5. 不要引入新的用户态异步 API；不要宣称生产级 io_uring

## 要求
1. make -C kestrelfs 零警告；cargo test + clippy -D warnings 不回归
2. STEP54_*_PASS 覆盖 A+B+C+D；并回归 Step 53 DIST-NOTIFY、Step 52 PERF、Step 51 write-behind
3. ABI 有布局/opcode 变化必须 bump；无变化保持 v22；format 同理
4. 更新 HANDOFF（待验收）、README（中文）、configuration.md、本文 → REVIEW + §9
5. 不要擅自 commit/push

## 明确不做
完整分布式锁、range lease 生产协议、Redis Cluster/Sentinel、自动 wipe、安全擦除、
宣称线性一致、完整用户态 io_uring 导出、任意 mknod 全集（除非 C 子项论证需要极小子集）。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告
- A/B/C/D 各自 STEP54_*_PASS + 关键回归
- §9 分小节写清方案、测例、数字、风险与未做项
```

## 9. 实现汇报日志

### 2026-09-21 — Step 53 DIST-NOTIFY + REDIS-HARDEN（Cursor ACCEPTED）

- Pub/Sub revision 提示唤醒 + 原 `coherence_probe()`；100 ms poll 保留；
  ConnectionManager 重连；`rediss://` + `--redis-ca-cert`。
- 验证：202 tests；clippy / make 干净；
  `redis_url_gated_notification_and_reconnect` PASS；`STEP53_REDIS_TLS_PASS`；
  `STEP53_DIST_NOTIFY_TWO_NODE_PASS`（latency 11 ms，wake 计数增长）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-21 — Step 53 DIST-NOTIFY + REDIS-HARDEN（Codex REVIEW）

- 方案与安全边界：保留 Step 42 durable revision + dirty-inode log 为唯一正确性来源，
  Redis Lua mutation 在 durable 状态提交后同事务 `PUBLISH` revision。订阅任务只用
  eventfd 唤醒同步 IPC poll 线程；线程收到提示仍执行原 `coherence_probe()`，沿用
  inode-list / full fail-closed 失效。100 ms poll 不变，订阅首次建立或重连也主动
  对账，因此丢失、重复、乱序只影响延迟，不会把不确定状态当 cache hit。
- Redis 硬化：命令路径改为 `ConnectionManager` 自动重连；Pub/Sub 以 50 ms 起、
  最高 1 s 退避重订阅。支持 `redis://` 与 `rediss://`，新增
  `--redis-ca-cert <PEM_PATH>` 信任私有/自签 CA；完整 URL 从不写启动日志。故障当次
  请求可能 EIO，服务恢复后的后续请求无需 daemon 重启；不自动重放不确定 mutation。
- 测试：`cargo test` **202 passed**；真实 Redis gate 杀掉 command connection 后，
  读请求自动恢复、后续 mutation/coherence/通知正确；自签 CA 经用户态 TLS 代理输出
  `STEP53_REDIS_TLS_PASS`。双独立 vng guest + loop + 共享 Redis/S3 输出
  `STEP53_DIST_NOTIFY_WAKE_PASS`、`STEP53_DIST_NOTIFY_DAEMON_FREE_HIT_PASS`、
  `STEP53_DIST_NOTIFY_TWO_NODE_PASS`，本轮可见延迟 **11 ms**（低于 100 ms poll，且
  mutation 后订阅唤醒计数增加）。
  Step 52 perf 回归输出 `STEP52_PERF_PASS`（page-cache 2128.54 MiB/s、daemon-free
  loop hit 169.52 MiB/s、256 async BIO、umount 19 ms）；Step 51 输出
  `STEP51_WRITE_BEHIND_PASS`（async write 0 ms、umount 10 ms）。所有 insmod、loop、
  mount 均只在 vng guest，未触碰宿主机模块/zvol/挂载；clippy `-D warnings` 与
  `make -C kestrelfs` 零警告通过。
- ABI/format：无共享内存、opcode、ioctl 或盘布局变化，IPC ABI **v22**、cache format
  **v4** 不变。
- 风险与未做：Pub/Sub 不提供 lease/线性一致；daemon 离线或订阅中断仍可能有最多
  poll/重连窗口；没有 fencing、跨节点锁、range 消息、Redis Cluster/Sentinel、
  不确定 mutation 幂等重放、io_uring/splice、orphan sweep 或自动 wipe。

### 2026-09-20 — Step 52 DIST-IO + TEST-PERF（Cursor ACCEPTED）

- 两并行 vng guest + 共享 Redis/S3；`docs/perf-baseline.md`。
- 验证：200 tests；Cursor `STEP52_PERF_PASS` / `STEP52_DIST_TWO_NODE_PASS`。

### 2026-09-20 — Step 51 WRITE-BEHIND + OPS/DOC（Cursor ACCEPTED）

- write-behind + `docs/configuration.md`；Cursor `STEP51_WRITE_BEHIND_PASS`。
