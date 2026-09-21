# KestrelFS 剩余能力与决策同步

> 最后更新：2026-09-21，Cursor（Step 55 已验收；测试迁入 `tests/`；发布 Step 56 常规双包）
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
> **体量约定**：常规双包 ≈ **2×** 既往单步（Step 54 过夜大包为一次性例外）。
>
> **测试布局（站立）**：step/vng/门控脚本与 C helper **只放 `tests/`**；见 `tests/README.md`。
> 禁止在仓库根新增 `test-*`。新脚本必须 source `tests/_repo_root.sh`。

## 1. 当前基线

- Phase 4 至 Step 55（精确 `d_type` + 受限 whiteout `mknod`）均已验收。
- 已验收 IPC ABI **v24**，cache format **v4**。
- 手工/vng 测试统一位于 `tests/`。
- cache/mount 测试只允许在 vng guest + loop；禁止触碰宿主机 zvol。
- 配置权威表：`docs/configuration.md`。

状态约定：`PROPOSED` / `DECIDED` / `IMPLEMENTING` / `REVIEW` / `ACCEPTED` / `DEFERRED`。

## 2. 建议路线（Cursor）

| 顺序 | ID | 能力 | 当前状态 | 理由 |
|---:|---|---|---|---|
| 0–31 | … + POSIX-DTYPE/MKNOD | Step 24–55 | **ACCEPTED** | d_type + 受限 mknod 已落地 |
| 32 | ORPHAN-RETRY-PERSIST + OPS-METRICS | Step 56（双包） | **DECIDED** | 补齐 rmmod 丢队列缺口 + 可观测性整理 |
| — | 其它 | — | PROPOSED | 视需要并入后续双包 |

Codex **只实现 §8 当前提示词**。

## 3–4. 摘要

Step 55 `ACCEPTED`。下一步见 §8。

## 5. 运维、测试与文档

`tests/README.md` 为测试布局权威说明；配置表见 `docs/configuration.md`。

## 6. Cursor ↔ Codex 决策记录

| 日期 | 记录者 | ID | 决策/问题 | 结论或待办 |
|---|---|---|---|---|
| 2026-09-21 | Cursor | OVERNIGHT MEGA | Step 54 验收 | **ACCEPTED** |
| 2026-09-21 | Cursor | POSIX-DTYPE + MKNOD-MIN | 选定 Step 55 | **DECIDED** |
| 2026-09-21 | Codex | POSIX-DTYPE + MKNOD-MIN | 实现与自检完成 | **REVIEW**；ABI v24 |
| 2026-09-21 | Cursor | POSIX-DTYPE + MKNOD-MIN | Step 55 验收 | **ACCEPTED**；206 tests + vng PASS |
| 2026-09-21 | Cursor | TEST-LAYOUT | 测试脚本迁入 `tests/` | **ACCEPTED**（Cursor 随验收提交）；站立规则写入 HANDOFF/`tests/README.md` |
| 2026-09-21 | Cursor | ORPHAN-RETRY-PERSIST + OPS-METRICS | 选定 Step 56 | **DECIDED**；见 §8 |

## 7. 不应顺手扩大

- 不自动 wipe；不触碰宿主机 zvol；不擅自 commit/push。
- 不把测试脚本写回仓库根。
- Step 56 不做完整跨 mount 全局 open-ref、完整监控栈或过夜级扩包。

## 8. 当前 Codex 提示词（Step 56，常规双包 ≈ 2×）

> **人类操作**：对 Codex 说「读 `HANDOFF.md` 与 `docs/remaining-capabilities.md`，只执行 §8」。

```text
你是 KestrelFS 实现 agent。路径：/home/roots/work/code/KestrelFS。
先读 HANDOFF.md、docs/remaining-capabilities.md（§2/§6/§8）、docs/configuration.md、
tests/README.md。HEAD 应含 Step 55（ABI v24）且测试已在 tests/。
注意：§8 为常规双包（≈ 2×）。新测试只写入 tests/。

开工时：两项均 → IMPLEMENTING，§6 追加一行。

## 测试铁律
- 内核/mount 验证只在 vng guest + loop；禁止宿主机 zvol
- daemon：独立 data_dir + >"$data_dir/daemon.log"；禁止 >/dev/null
- 改 kestrelfs/*.c 或依赖 mount：必须 vng
- 新脚本/C helper 只放 tests/；source tests/_repo_root.sh；互相调用用 ./tests/...
- 默认 cargo test 可不依赖外部服务

## 目标：Step 56 — ORPHAN-RETRY-PERSIST + OPS-METRICS（双包）

### A) ORPHAN-RETRY-PERSIST
针对 HANDOFF 已知限制：orphan peek/ack retry 队列在 rmmod 时丢失。
1. 把「内核已证明 open_handles==0 且 FINALIZE 失败」的 retry 集合做成可恢复状态
   （优先：随模块/超级块旁路的小持久区，或 daemon data_dir 侧持久队列 + 启动对账；
   择优并在 §9 论证。禁止用 Redis session TTL 猜测 fd 仍存活）
2. rmmod/insmod 或 daemon 重启后，未 ack 的 orphan 仍会被继续 finalize→GC
3. 仍 open 的 inode 绝不可被 sweep；证据不足 skip + 日志
4. vng：STEP56_ORPHAN_PERSIST_*_PASS（制造泄漏 → rmmod/重启 → 恢复后清掉；
   负例：仍 open 不删）

### B) OPS-METRICS
1. 整理并文档化现有只读可观测项（至少覆盖 write_pipe_*、coherence inode batch/entry、
   cache_async_hit_peak、session 相关若已有），写入 docs/configuration.md 专节
2. 若缺关键计数：补最小 sysfs/module param 或 daemon 日志计数（不要上完整 Prometheus）
3. vng 或脚本断言至少一个指标在对应负载下递增（STEP56_METRICS_*_PASS）

## 要求
1. make -C kestrelfs 零警告；cargo test + clippy -D warnings 不回归
2. STEP56_*_PASS 覆盖 A+B；回归 Step 54 VFS_PIPE orphan 路径与 Step 55 dtype/mknod
3. ABI：有布局/ioctl 变化须 bump；无则保持 v24；format 同理
4. 更新 HANDOFF（待验收）、README（中文）、configuration.md、本文 → REVIEW + §9
5. 不要擅自 commit/push；不要在仓库根新增 test-*

## 明确不做
跨 mount 全局 open-ref、完整 APM/Prometheus、自动 wipe、过夜级扩包、通用设备 mknod。

## 验收自检
- cargo test + clippy -D warnings
- make -C kestrelfs 零警告 + ./tests/test-step56-* vng
- 汇报写入本文 §9
```

## 9. 实现汇报日志

### 2026-09-21 — Step 55 POSIX-DTYPE + MKNOD-MIN（Cursor ACCEPTED）

- `READDIR_DATA` 12-byte header + `DT_*`；受限 `mknod`（`S_IFCHR` 0:0，需 CAP_MKNOD）。
- ABI **v23 → v24**；format **v4**。
- 验证：206 tests；clippy / make 干净；Redis gate PASS；
  `STEP55_POSIX_DTYPE_MKNOD_PASS`（umount 14 ms；迁入 `tests/` 后复跑通过）。
- 同提交：根目录 step/vng 脚本迁入 `tests/`，新增 `tests/README.md` 与 `_repo_root.sh`。
- Commit：随 Cursor 本轮验收推送。

### 2026-09-21 — Step 55 POSIX-DTYPE + MKNOD-MIN（Codex REVIEW）

- A / POSIX-DTYPE：`READDIR_DATA` 条目从 10-byte header 扩为 12 bytes：
  `inode_id@0`、`name_len@8`、Linux `DT_*@10`、zero reserved@11，名字从 offset 12
  开始。MetaStore readdir 在同一 metadata snapshot 中返回 inode/name/mode；daemon 将
  regular/directory/symlink/whiteout 编为 `DT_REG/DT_DIR/DT_LNK/DT_CHR`，内核拒绝未知
  类型或非零 reserved。
- B / MKNOD-MIN：目录 inode ops 新增 `.mknod`，显式要求 `CAP_MKNOD`，且只接受
  `S_IFCHR` 与 `rdev=0:0`；其它返回 `EOPNOTSUPP`。复用 `CREATE_DATA` mode 字段；
  Mem/File/Redis 持久化为与 `RENAME_WHITEOUT` 一致的 mode-000 marker。
- 测试：206 passed；vng `STEP55_*_PASS`；回归 Step 50/54。未做通用设备/FIFO/socket。

### 2026-09-21 — Step 54 OVERNIGHT MEGA（Cursor ACCEPTED）

- session fencing；orphan peek/ack（ABI v23）；splice/sendfile；WRITE_DATA prestage。
