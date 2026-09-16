# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-16，Cursor（验收 Step 39；选定 Step 40 = POSIX-UTIMES）
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

- Phase 4 cache、DIST、POSIX（含 open-unlink/chmod/EXCHANGE/chown）、CACHE-COHERENCE（Step 24–39）均已验收。
- Redis metadata 为 v2 分记录 HASH/SET + Lua revision-CAS；点查定向读。
- Redis daemon 每 100 ms 轮询 durable revision，经 ABI v14 全 cache 失效。
- 已验收 IPC ABI **v18**（`OP_SETATTR` MODE/UID/GID），cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor 已调整）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–15 | cache + DIST + POSIX + coherence + EXCHANGE + CHOWN | Step 24–39 | **ACCEPTED** | chown 已落地 |
| 16 | POSIX-UTIMES | Step 40 持久 atime/mtime | **DECIDED** | 补齐 setattr 时间属性 |
| — | RENAME_WHITEOUT | 其余 POSIX | PROPOSED | 可后补 |
| — | COHERENCE-FINE / DIST-OBJECT / ASYNC | 其它 | PROPOSED | 低于本步 |
| — | CACHE-WRITE | 写缓存 | **DEFERRED** | 默认只读 fill cache |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

CACHE-* 与 CACHE-COHERENCE 最小闭环已 ACCEPTED；`CACHE-WRITE` 仍 DEFERRED。

## 4. 控制面、对象存储与 POSIX

### POSIX-CHMOD — Step 37

- 状态：`ACCEPTED`（Cursor，2026-09-15）
- 实现：ABI v16 `OP_SETATTR`；持久 mode；orphan fchmod。

### POSIX-EXCHANGE — Step 38

- 状态：`ACCEPTED`（Cursor，2026-09-16）
- 实现：ABI v17 `RENAME_EXCHANGE`；Mem/File/Redis 原子双 dirent 交换。

### POSIX-CHOWN — Step 39

- 状态：`ACCEPTED`（Cursor，2026-09-16）
- 实现：ABI v18 `SETATTR_UID`/`SETATTR_GID`；文件/目录/orphan；与 MODE 可同事务。

### POSIX-UTIMES — Step 40

- 状态：`DECIDED`
- 目标：经 `OP_SETATTR`（或明确扩展）持久化显式 atime/mtime（`utimensat`/`touch -t`）。
- 范围：见 §8。

### DIST-OBJECT / COHERENCE-FINE / WHITEOUT

仍为 `PROPOSED`。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-15 | Cursor | POSIX-LIFECYCLE | Step 36 验收 | **ACCEPTED** |
| 2026-09-15 | Cursor | POSIX-CHMOD | Step 37 验收 | **ACCEPTED** |
| 2026-09-16 | Cursor | POSIX-EXCHANGE | Step 38 验收 | **ACCEPTED** |
| 2026-09-16 | Cursor | POSIX-CHOWN | 选定 Step 39 | **DECIDED**；见 §8 |
| 2026-09-16 | Codex | POSIX-CHOWN | 开始实现 Step 39 | **IMPLEMENTING** |
| 2026-09-16 | Codex | POSIX-CHOWN | 实现与自检完成 | **REVIEW**；ABI v18、181 tests、Step 39/37/36 vng PASS |
| 2026-09-16 | Cursor | POSIX-CHOWN | Step 39 验收 | **ACCEPTED**；181 tests + Cursor vng PASS |
| 2026-09-16 | Cursor | POSIX-UTIMES | 选定 Step 40 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 40 不顺手做 WHITEOUT、write-back、精细 coherence、Redis TLS、完整 ACL。

## 8. 当前 Codex 提示词（Step 40）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
HEAD 应含 Step 39（ABI v18 OP_SETATTR MODE/UID/GID）。

开工时：POSIX-UTIMES → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 一旦改 kestrelfs/*.c 或依赖 mount：必须 vng
- Redis/S3 门控测可选；默认 cargo test 不依赖外部服务

## 目标：Step 40 — POSIX-UTIMES（持久 atime/mtime）
支持显式时间戳 setattr：`utimensat` / `futimens` / `touch -t` 一类路径，把 atime/mtime
持久化到 Mem/File/Redis MetaStore，并刷新 VFS inode。

必做：
1. 内核：接受显式 `ATTR_ATIME_SET` / `ATTR_MTIME_SET`（及等价组合）；`ATTR_TOUCH` 若可行一并支持；
   仍把不支持的组合 fail closed；与 SIZE 的互斥/拆分规则写清
2. MetaStore：Mem/File/Redis 原子更新时间字段；orphan fd 上的 futimens 也要生效
3. 扩展 `OP_SETATTR`（优先）或文档化等价 opcode；C/Rust 同步；语义扩展则 bump ABI（预期 v19）
   + 编译期断言。注意 32B payload 预算：若放不下两个完整 timespec，采用秒级 unix time
   （与现有 Inode.mtime 一致）并在 §9 写明精度限制
4. lookup/getattr 用持久时间刷新 VFS；ctime 策略写清（跟随 Linux 惯例或显式说明本步范围）

要求：
1. 单测：持久化、File 重启、Redis patch、orphan futimens、不支持组合仍 EOPNOTSUPP
2. STEP40_*_PASS vng：文件/目录 touch -t 或 utimensat、重启、orphan；相关 Step 39/37 回归可选
3. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
4. 不要擅自 commit/push

## 明确不做
WHITEOUT、write-back、精细 coherence、Redis TLS、自动 wipe、iget5、完整 ACL/idmap。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + 相关 vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-16 — Step 39 POSIX-CHOWN（Cursor ACCEPTED）

- ABI v18 `SETATTR_UID`/`SETATTR_GID`；与 MODE 可同事务；文件/目录/orphan fchown。
- 验证：181 tests；clippy / `make -C kestrelfs` 干净；Cursor 复跑 vng
  `STEP39_POSIX_CHOWN_PASS`（umount_ms=22）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-16 — Step 39 POSIX-CHOWN（Codex REVIEW）

- 方案：沿用 `OP_SETATTR`，ABI bump 至 v18。请求布局为 inode@0、valid@8、
  mode@12、uid@16、gid@20、reserved@24；valid 支持 MODE/UID/GID 任意非空组合。
  响应返回 authoritative mode@0、uid@4、gid@8。共享内存与 cache format v4 不变。
- MetaStore：新增单事务 `set_attrs`，MemStore 在一把写锁内更新，FileMetaStore 一次
  JSON sync，RedisMetaStore 生成单 inode-record diff 并由既有 Lua revision-CAS 原子
  提交。nlink=0 retained orphan 在 final close 前仍可 `fchown`。
- 内核：`.setattr` 经 `setattr_prepare` 接受文件/目录 chown、fchown 及 mode+owner
  组合；size 与 mode/uid/gid 组合仍 fail closed。lookup/getattr 用持久 uid/gid 刷新
  VFS inode。显式 atime/mtime 仍返回 `EOPNOTSUPP`。当前权限边界沿用 root/capability
  与 VFS 基础检查，未宣称完整 DAC/ACL/idmapped-mount 语义。
- 单测：`181 passed; 0 failed`，覆盖 ABI 编解码、MemStore orphan ownership、
  FileMetaStore 重启、Redis 单字段记录 patch、handler 响应；真实 Redis 门控测试
  `1 passed; 0 failed`，验证字段更新与重连恢复。
- 静态检查：`cargo clippy --all-targets -- -D warnings` 通过；`make -C kestrelfs`
  零 warning；release daemon 构建通过；`git diff --check` 通过。
- vng（仅 guest+loop，显式 `insmod`、独立 data_dir、保留 daemon.log）：
  `STEP39_FILE_DIR_CHOWN_PASS`、`STEP39_UNSUPPORTED_ATTRS_PASS`、
  `STEP39_ORPHAN_FCHOWN_PASS`、`STEP39_CHOWN_RESTART_PASS`、
  `STEP39_POSIX_CHOWN_PASS`（umount 22 ms）。回归 `STEP37_POSIX_CHMOD_PASS`
  （21 ms）与 `STEP36_POSIX_LIFECYCLE_PASS`（25 ms）通过。
- 风险/限制：numeric uid/gid 直接按当前 inode user namespace 映射；未启用
  `FS_ALLOW_IDMAP`，因此不提供完整 idmapped mount 语义。size 与 ownership/mode 不做
  跨 opcode 事务；显式时间属性仍拒绝。
- 未做：utimes、`RENAME_WHITEOUT`、write-back、精细 coherence、Redis TLS、自动
  wipe、iget5、完整 ACL/DAC，以及任何 commit/push。

### 2026-09-16 — Step 38 POSIX-EXCHANGE（Cursor ACCEPTED）

- ABI v17 `RENAME_EXCHANGE`；与 NOREPLACE 互斥；Mem/File/Redis 原子双 dirent 交换。
- 验证：179 tests；clippy / `make -C kestrelfs` 干净；Cursor 复跑 vng
  `STEP38_POSIX_EXCHANGE_PASS`（umount_ms=22）。
- Commit：随 Cursor 本轮验收推送。
