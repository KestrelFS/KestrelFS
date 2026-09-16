# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-16，Cursor（验收 Step 38；选定 Step 39 = POSIX-CHOWN）
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

- Phase 4 cache、DIST、POSIX（含 open-unlink/chmod/EXCHANGE）、CACHE-COHERENCE（Step 24–38）均已验收。
- Redis metadata 为 v2 分记录 HASH/SET + Lua revision-CAS；点查定向读。
- Redis daemon 每 100 ms 轮询 durable revision，经 ABI v14 全 cache 失效。
- 已验收 IPC ABI **v17**（`RENAME_EXCHANGE`），cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor 已调整）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–14 | cache + DIST + POSIX + coherence + EXCHANGE | Step 24–38 | **ACCEPTED** | EXCHANGE 已落地 |
| 15 | POSIX-CHOWN | Step 39 `OP_SETATTR` uid/gid | **DECIDED** | 补齐 chmod 后的属主属性 |
| — | RENAME_WHITEOUT / utimes | 其余 POSIX | PROPOSED | 可后补 |
| — | COHERENCE-FINE / DIST-OBJECT / ASYNC | 其它 | PROPOSED | 低于本步 |
| — | CACHE-WRITE | 写缓存 | **DEFERRED** | 默认只读 fill cache |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

CACHE-* 与 CACHE-COHERENCE 最小闭环已 ACCEPTED；`CACHE-WRITE` 仍 DEFERRED。

## 4. 控制面、对象存储与 POSIX

### POSIX-CHMOD — Step 37

- 状态：`ACCEPTED`（Cursor，2026-09-15）
- 实现：ABI v16 `OP_SETATTR`；持久 mode；orphan fchmod；chown/时间戳 `EOPNOTSUPP`。

### POSIX-EXCHANGE — Step 38

- 状态：`ACCEPTED`（Cursor，2026-09-16）
- 实现：ABI v17 `RENAME_EXCHANGE`；Mem/File/Redis 原子双 dirent 交换；与 NOREPLACE 互斥；`WHITEOUT` 仍 `EINVAL`。

### POSIX-CHOWN — Step 39

- 状态：`DECIDED`
- 目标：经既有 `OP_SETATTR` 持久化 uid/gid（chown/fchown）。
- 范围：见 §8。

### DIST-OBJECT / COHERENCE-FINE / WHITEOUT / utimes

仍为 `PROPOSED`。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-15 | Cursor | POSIX-LIFECYCLE | Step 36 验收 | **ACCEPTED** |
| 2026-09-15 | Cursor | POSIX-CHMOD | 选定 Step 37 | **DECIDED** |
| 2026-09-15 | Codex | POSIX-CHMOD | 实现与自检 | **REVIEW** |
| 2026-09-15 | Cursor | POSIX-CHMOD | Step 37 验收 | **ACCEPTED**；172 tests + vng PASS |
| 2026-09-15 | Cursor | POSIX-EXCHANGE | 选定 Step 38 | **DECIDED**；见 §8 |
| 2026-09-15 | Codex | POSIX-EXCHANGE | 开始实现 Step 38 | **IMPLEMENTING**；严格按 §8 执行 |
| 2026-09-15 | Codex | POSIX-EXCHANGE | 实现与自检完成 | **REVIEW**；ABI v17、179 tests、Step 38/33/36/37 vng PASS |
| 2026-09-16 | Cursor | POSIX-EXCHANGE | Step 38 验收 | **ACCEPTED**；179 tests + Cursor vng PASS |
| 2026-09-16 | Cursor | POSIX-CHOWN | 选定 Step 39 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 39 不顺手做 utimes、WHITEOUT、write-back、精细 coherence、Redis TLS。

## 8. 当前 Codex 提示词（Step 39）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
HEAD 应含 Step 38（ABI v17 RENAME_EXCHANGE）。

开工时：POSIX-CHOWN → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 一旦改 kestrelfs/*.c 或依赖 mount：必须 vng
- Redis/S3 门控测可选；默认 cargo test 不依赖外部服务

## 目标：Step 39 — POSIX-CHOWN（持久 uid/gid）
经既有 ABI `OP_SETATTR` 支持 `chown`/`fchown`（及目录 `chown`），把 uid/gid 持久化到 Mem/File/Redis MetaStore，并刷新 VFS inode。

必做：
1. 内核：`setattr` 接受 `ATTR_UID` / `ATTR_GID`（可单独或组合）；仍拒绝未支持的时间戳类属性；
   与 `ATTR_SIZE` 的互斥/拆分规则写清（mode+uid/gid 同事务可支持；size 与其它仍可按既有策略拒绝或拆分）
2. MetaStore：Mem/File/Redis 持久更新 uid/gid；orphan 打开文件的 fchown 也要生效
3. 扩展 `OP_SETATTR` valid mask：至少 `SETATTR_UID` / `SETATTR_GID`（可与既有 MODE 组合）；
   C/Rust 同步；若语义扩展则 bump ABI（预期 v18）+ 编译期断言
4. 权限策略：本步可先按 root/capability 简化（与当前 chmod 一致即可），在 §9 写明；不要假装完整 Linux DAC

要求：
1. 单测覆盖 uid/gid 持久化、重启恢复（File）、Redis 字段 patch、orphan fchown、不支持属性仍 EOPNOTSUPP
2. STEP39_*_PASS vng：文件/目录 chown、重启、open-unlink orphan fchown；相关 Step 37/36 回归可选
3. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
4. 不要擅自 commit/push

## 明确不做
utimes/完整时间属性、WHITEOUT、write-back、精细 coherence、Redis TLS、自动 wipe、iget5。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + 相关 vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-16 — Step 38 POSIX-EXCHANGE（Cursor ACCEPTED）

- ABI v17 `RENAME_EXCHANGE`；与 NOREPLACE 互斥；Mem/File/Redis 原子双 dirent 交换。
- 验证：179 tests；clippy / `make -C kestrelfs` 干净；Cursor 复跑 vng
  `STEP38_POSIX_EXCHANGE_PASS`（umount_ms=22）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-15 — Step 38 POSIX-EXCHANGE（Codex REVIEW）

- 方案：复用 `RENAME_DATA` 的 flags@20，新增 `RENAME_EXCHANGE=2` 并与
  `RENAME_NOREPLACE=1` 互斥。虽然 payload/共享内存布局不变，仍将 IPC ABI bump 至
  v17，避免旧 v16 daemon 与新内核在“版本相同、能力不同”时静默组合；cache format
  保持 v4。
- 内核：`.rename` 接受 EXCHANGE、要求目标为正 dentry，不进入 overwrite lifecycle
  defer/GC/cache invalidate。交换后两个 inode 都保留；跨父目录混合类型交换按 Linux
  语义允许，并对父目录 nlink 作对称调整。VFS 继续负责 dentry exchange。
- MetaStore：MemStore 在单一写锁内先完成两方向祖先环与存在性校验，再同时替换两个
  dirent；FileMetaStore 以一次 JSON sync 持久化；RedisMetaStore 生成两个 dirent field
  的单次 diff，并由同一个 Lua revision-CAS 原子提交。交换不删除 inode/slice、不产生
  GC key，也不改变硬链接 nlink。
- 单测：179 passed，覆盖 ABI flags/互斥/lifecycle 拒绝、文件交换、非空目录交换、
  文件↔目录交换及父 nlink、环路/缺失目标失败原子性、FileMetaStore 重启、Redis 双
  dirent patch 与真实 Redis 重建恢复。真实 Redis 门控测试 `1 passed; 0 failed`。
- 静态检查：`cargo clippy --all-targets -- -D warnings` 通过；`make -C kestrelfs`
  零 warning；release daemon 构建通过；`git diff --check` 通过。
- vng（仅 guest+loop，显式 `insmod`、独立 data_dir、保留 daemon.log）：
  `STEP38_FILE_EXCHANGE_PASS`、`STEP38_CACHE_IDENTITY_PASS`、
  `STEP38_DIRECTORY_EXCHANGE_PASS`、`STEP38_MIXED_TYPE_EXCHANGE_PASS`、
  `STEP38_HARDLINK_EXCHANGE_PASS`、`STEP38_FAILURE_ATOMICITY_PASS`、
  `STEP38_EXCHANGE_RESTART_PASS`、`STEP38_POSIX_EXCHANGE_PASS`（umount 70 ms）。相关
  回归 `STEP33_POSIX_RENAME_PASS`（68 ms）、`STEP36_POSIX_LIFECYCLE_PASS`
  （117 ms）、`STEP37_POSIX_CHMOD_PASS`（60 ms）均通过。旧 Step 33 脚本的 flags=2
  负例已迁到仍不支持的 WHITEOUT flags=4。
- 风险/限制：目录祖先判断沿用 O(目录项) 父链扫描；不同 mount 的 VFS inode/open
  identity 限制未改变。EXCHANGE 不失效 inode-keyed cache，这是必要且已由 daemon
  停止后的 cache hit 验证；未来若 cache key 改含路径必须同步调整。
- 未做：`RENAME_WHITEOUT`、chown/utimes、write-back、精细 coherence、Redis TLS、
  自动 wipe、iget5，以及任何 commit/push。

### 2026-09-15 — Step 37 POSIX-CHMOD（Cursor ACCEPTED）

- ABI v16 `OP_SETATTR`；文件/目录/orphan chmod；chown/时间戳 `EOPNOTSUPP`。
- 验证：172 tests；clippy / `make -C kestrelfs` 干净；Cursor 复跑 vng
  `STEP37_POSIX_CHMOD_PASS`（umount_ms=20）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-15 — Step 36 POSIX-LIFECYCLE（Cursor ACCEPTED）

- ABI v15 open-unlink；commit `05104e7`。
