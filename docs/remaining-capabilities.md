# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-16，Cursor（验收 Step 40；选定 Step 41 = DIST-OBJECT）
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

- Phase 4 cache、DIST、POSIX（含 open-unlink/chmod/EXCHANGE/chown/utimes）、CACHE-COHERENCE（Step 24–40）均已验收。
- Redis metadata 为 v2 分记录 HASH/SET + Lua revision-CAS；点查定向读。
- Redis daemon 每 100 ms 轮询 durable revision，经 ABI v14 全 cache 失效。
- 已验收 IPC ABI **v19**（`OP_SETATTR` 时间 union + `GETATTR_TIMES`），cache format **v4**。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor 已调整）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–16 | cache + DIST + POSIX + coherence + attrs | Step 24–40 | **ACCEPTED** | setattr 子集已闭环 |
| 17 | DIST-OBJECT | Step 41 对象存储加固 | **DECIDED** | 转向数据面耐久性，离开长 POSIX 串 |
| — | RENAME_WHITEOUT / COHERENCE-FINE / ASYNC | 其它 | PROPOSED | 可后补 |
| — | CACHE-WRITE | 写缓存 | **DEFERRED** | 默认只读 fill cache |

Codex **只实现 §8 当前提示词**。

## 3. Phase 4 缓存能力

CACHE-* 与 CACHE-COHERENCE 最小闭环已 ACCEPTED；`CACHE-WRITE` 仍 DEFERRED。

## 4. 控制面、对象存储与 POSIX

### POSIX-UTIMES — Step 40

- 状态：`ACCEPTED`（Cursor，2026-09-16）
- 实现：ABI v19 秒级 atime/mtime；`GETATTR_TIMES`；orphan futimens；basic/time layout 互斥。

### DIST-OBJECT — Step 41

- 状态：`DECIDED`
- 目标：加固 ObjectStore（尤其 S3）耐久写读与慢路径隔离。
- 范围：见 §8。

### WHITEOUT / COHERENCE-FINE / ASYNC

仍为 `PROPOSED`。

## 5. 运维、测试与文档

OPS-CONFIG / TEST-PERF / DOC-CLEANUP 仍为 `PROPOSED`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-16 | Cursor | POSIX-CHOWN | Step 39 验收 | **ACCEPTED** |
| 2026-09-16 | Cursor | POSIX-UTIMES | 选定 Step 40 | **DECIDED**；见 §8 |
| 2026-09-16 | Codex | POSIX-UTIMES | 开始实现 Step 40 | **IMPLEMENTING** |
| 2026-09-16 | Codex | POSIX-UTIMES | 实现与自检完成 | **REVIEW**；ABI v19、185 tests、Step 40/39/37 vng PASS |
| 2026-09-16 | Cursor | POSIX-UTIMES | Step 40 验收 | **ACCEPTED**；185 tests + Cursor vng PASS |
| 2026-09-16 | Cursor | DIST-OBJECT | 选定 Step 41 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- Step 41 不顺手做 WHITEOUT、write-back、精细 coherence、Redis TLS、完整加密压缩。

## 8. 当前 Codex 提示词（Step 41）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md 与 docs/remaining-capabilities.md（全文，尤其 §2/§6/§8）。
HEAD 应含 Step 40（ABI v19 OP_SETATTR times + GETATTR_TIMES）。

开工时：DIST-OBJECT → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 涉及内核/mount 的验证只在 vng guest + loop；禁止触碰宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 一旦改 kestrelfs/*.c 或依赖 mount：必须 vng；本步若纯用户态可不强制 vng，但须在 §9 说明
- Redis/S3 门控测可选；默认 cargo test 不依赖外部服务

## 目标：Step 41 — DIST-OBJECT（对象存储加固）
在已有 Mem/LocalFs/S3 ObjectStore 与 Step 30 GC queue 之上，加固对象数据面，降低慢 S3
对 IPC 事件环的拖累，并提高 put/get 可观测完整性。

必做（至少完成下列两项；第三项可选但不要半截）：
1. **慢路径隔离**：GC delete（及若自然扩展则 put/get 中的可异步部分）不要长时间阻塞
   串行 IPC event loop。可用 bounded worker/task 队列；失败仍须回到 durable GC queue；
   在 §9 写清并发上限与背压策略。
2. **完整性检查**：ObjectStore get 后至少校验长度；S3 路径额外校验 ETag/或 content-length
   与预期一致（若 SDK 已暴露）。不匹配 fail closed，不要静默截断。
3. 可选：大对象 multipart PUT（仅当能对现有 4 MiB Block 路径给出可测收益）；否则跳过并说明。

要求：
1. Mem/LocalFs/S3 行为与既有单测兼容；新增单测覆盖 worker 背压/失败回队、完整性失败
2. 真实 S3/MinIO 门控测（有环境则跑）；默认 CI 不依赖外部服务
3. 本步预期 **不 bump IPC ABI**（纯 daemon）。若不得不改内核，须在 §9 强理由说明
4. 更新 HANDOFF（待验收）、README（中文）、本文 → REVIEW + §9
5. 不要擅自 commit/push

## 明确不做
WHITEOUT、write-back、精细 coherence、Redis TLS、客户端加密/压缩、自动 wipe、iget5。

## 验收自检
- cargo test + clippy -D warnings
- 若改了 kestrelfs/*.c：make 零警告 + 相关 vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-16 — Step 40 POSIX-UTIMES（Cursor ACCEPTED）

- ABI v19 秒级 atime/mtime；basic/time layout 互斥；`GETATTR_TIMES`；orphan futimens。
- 验证：185 tests；clippy / `make -C kestrelfs` 干净；Cursor 复跑 vng
  `STEP40_POSIX_UTIMES_PASS`（umount_ms=23）。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-16 — Step 40 POSIX-UTIMES（Codex REVIEW）

- ABI：沿用 `OP_SETATTR` 并 bump 至 v19。MODE/UID/GID 继续使用 basic layout；
  ATIME/MTIME 使用互斥 time layout：inode@0、valid@8、atime 秒@12、mtime 秒@20、
  reserved@28。两种 layout 因字段重叠不可组合。新增 `OP_GETATTR_TIMES`（opcode 24）
  返回 atime@0、mtime@8，供 lookup/getattr 在重启后重建 VFS inode。共享内存和 cache
  format v4 不变。
- 精度与范围：MetaStore 以非负 `u64` Unix 秒持久化 atime/mtime；纳秒输入成功后截断
  为 0，负 epoch 返回 `EOVERFLOW`。旧 File/Redis inode 记录缺少 atime 时通过 serde
  默认恢复为 epoch 0，不做自动格式迁移。
- 内核：支持 `utimensat`、`futimens`、`touch -t`、单独 atime/mtime 及 `ATTR_TOUCH`；
  `setattr_prepare` 继续执行 VFS 权限检查。SIZE 与任何其它持久属性组合，以及 time 与
  MODE/UID/GID 组合返回 `EOPNOTSUPP`。ctime 仍由当前 VFS inode 按 Linux setattr
  流程更新，但本步不持久化 ctime，重新 lookup 后 ctime 仍为 inode 实例化时间。
- MetaStore：`set_attrs` 在 MemStore 单写锁内原子更新时间；FileMetaStore 单次 JSON
  sync；RedisMetaStore 生成单 inode-record diff 并由既有 Lua revision-CAS 提交。
  nlink=0 retained orphan 在 final close 前支持 `futimens`。
- 单测：`185 passed; 0 failed`，覆盖 ABI union/秒字段、旧记录兼容、MemStore orphan、
  FileMetaStore 重启、Redis 单 inode patch、handler 与 `GETATTR_TIMES`；真实 Redis
  门控测试 `1 passed; 0 failed`。
- 静态检查：`cargo clippy --all-targets -- -D warnings` 通过；`make -C kestrelfs`
  零 warning；release daemon 构建通过；`git diff --check` 通过。
- vng（仅 guest+loop，显式 `insmod`、独立 data_dir、保留 daemon.log）：
  `STEP40_FILE_DIR_UTIMES_PASS`、`STEP40_ORPHAN_FUTIMENS_PASS`、
  `STEP40_UTIMES_RESTART_PASS`、`STEP40_POSIX_UTIMES_PASS`（umount 22 ms）。回归
  `STEP39_POSIX_CHOWN_PASS`（27 ms）和 `STEP37_POSIX_CHMOD_PASS`（24 ms）通过；两份
  旧脚本已移除“时间属性必须失败”的过时断言。
- 风险/限制：自动读 atime 更新仍是 mount-local VFS 行为，不同步回 MetaStore；只有
  显式 setattr 时间操作持久化。秒级精度、非负 epoch、ctime 不持久化；旧记录 atime
  为 epoch。额外 `OP_GETATTR_TIMES` 使每次成功 lookup 多一次同步 IPC。
- 未做：`RENAME_WHITEOUT`、write-back、精细 coherence、Redis TLS、自动 wipe、
  iget5、完整 ACL/idmap、纳秒/负 epoch 支持，以及任何 commit/push。

### 2026-09-16 — Step 39 POSIX-CHOWN（Cursor ACCEPTED）

- ABI v18 `SETATTR_UID`/`SETATTR_GID`；与 MODE 可同事务；文件/目录/orphan fchown。
- 验证：181 tests；clippy / `make -C kestrelfs` 干净；Cursor 复跑 vng
  `STEP39_POSIX_CHOWN_PASS`（umount_ms=22）。
- Commit：随 Cursor 本轮验收推送。
