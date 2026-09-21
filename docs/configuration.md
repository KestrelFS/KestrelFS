# KestrelFS 配置参考

本文是 KestrelFS 当前**唯一权威的配置参数表**。README、HANDOFF 和 Phase 4
设计文档只说明使用场景并链接到这里；参数默认值、互斥关系或安全约束冲突时，
以代码和本文为准。

## 内核模块参数

所有输入参数均为 `0444`，只能在 `insmod` 时设置；运行中不能通过 sysfs 修改。
不指定 `cache_device` 时，本地 NVMe 读缓存关闭，其余 cache 参数不会启用设备。

| 参数 | 默认值 | 含义与约束 |
|---|---:|---|
| `cache_device=<PATH>` | 未设置 | 专用 cache **块设备**。可用 loop、zvol 或 raw block；普通文件（包括 ZFS dataset 内的文件）会被拒绝。|
| `cache_size_mib=<N>` | `0` | 可用容量上限，单位 MiB；`0` 表示在格式/geometry 约束内使用整盘。非零值不得超过设备容量，并须容纳 2 MiB metadata 与至少一个 4 KiB data slot。|
| `cache_namespace=<HEX>` | 未设置 | 使用 `cache_device` 时必填的 64 个十六进制字符（SHA-256）。盘上 identity 不匹配时 fail closed。|
| `cache_direct_io=<0|1>` | `1` | 允许满足对齐条件的 cache hit BIO 直达 pinned 用户页；`0` 强制 buffered-copy 回退。|
| `cache_parallel_reads=<0|1>` | `1` | 允许多个 cache-hit reader 在 rwsem 读侧并行；`0` 用于串行 A/B 验证。|
| `cache_evict_batch=<N>` | `16` | 满盘时一批退休的 LRU victim 数，合法范围 `1..64`；实际还限制为总 slot 数的 `1/16`（至少 1）。|

`cache_namespace` 应由部署层对稳定、规范化、无凭据的 MetaStore + ObjectStore
描述求 SHA-256。路径先转绝对规范路径；Redis DB/prefix、S3 endpoint/bucket/prefix
采用固定表示；密码和 access key 不得进入描述。例如：

```bash
data_dir=/tmp/kestrelfs-demo-$$
descriptor="v1;meta=file:$data_dir/meta.json;objects=local:$data_dir"
cache_namespace=$(printf '%s' "$descriptor" | sha256sum | awk '{print $1}')
```

## 只读可观测性

下列同名 `0444` 参数是只读运行期计数，模块加载时从 0 开始，仅供诊断，**不是稳定
用户 ABI**：

| 计数 | 含义 |
|---|---|
| `cache_direct_hit_blocks` / `cache_copy_hit_blocks` | 直达用户页 / buffered-copy 的 4 KiB hit 数 |
| `cache_direct_fallbacks` | 直达尝试退回 buffered-copy 的次数 |
| `cache_evictions` | 为 slot 复用而退休的 block 数 |
| `cache_eviction_batches` / `cache_eviction_batch_slots` / `cache_eviction_index_writes` | 批量回收事务、victim 与合并后 index-page 写次数 |
| `cache_checksum_failures` / `cache_journal_recoveries` | data CRC 拒绝数 / 未完成事务恢复为 miss 的次数 |
| `cache_active_hit_readers` / `cache_parallel_hit_peak` | 当前 / 峰值并发 hit reader |
| `cache_async_hit_submissions` / `cache_async_hit_peak` | 异步 completion BIO 提交数 / 峰值在途数 |
| `cache_coherence_invalidations` | daemon 请求并成功完成的全 cache 失效数 |
| `cache_coherence_inode_batches` / `cache_coherence_inode_entries` | 有界 inode 失效批次 / 退休 entry 数 |
| `write_pipe_staged_bytes` / `write_pipe_submissions` | 在 bounce mutex 外预暂存的 folio 字节 / 随后提交的 WRITE_DATA chunk 数 |
| `write_pipe_lock_wait_ns` / `write_pipe_lock_hold_ns` | 写回等待 / 持有全局 bounce mutex 的累计纳秒数 |
| `orphan_retry_queued` / `orphan_retry_acked` | 本次模块加载后产生的内核 final-close proof / 已由 daemon 持久接收的累计数 |
| `orphan_retry_pending` | 尚未由 daemon 持久接收的 proof 数；非零时模块持有引用，正常 `rmmod` 会被拒绝 |

计数路径为 `/sys/module/kestrelfs/parameters/<name>`。

daemon 目前以结构化文本日志提供其余最小观测，不提供 Prometheus/APM endpoint：

| 日志前缀/字段 | 含义 |
|---|---|
| `ORPHAN-SWEEP ... persisted=N` / `reclaimed=N` | kernel proof 已原子写入 data-dir 队列 / 持久 proof 已完成 finalize |
| `DIST-OBJECT ... totals attempted=... deleted=... failures=...` | 本进程 ObjectStore GC 累计尝试、成功与失败 |
| `coherence revision A -> B; invalidated N dirty inode caches` | Redis durable revision 的细粒度 inode 失效 |
| `writer session registered ... ttl_ms=...` | Redis writer session 已注册；日志只含随机 session id，不含 URL/凭据 |
| `writer session was fenced or expired` / `heartbeat failed` | writer 已 fail closed / 心跳暂时失败并重试 |

`orphan_retry_pending` 是安全相关 gauge：它不为零时不要强制卸载模块。daemon 把 proof
写入 `{data-dir}/.orphan-retries-v1.json`，执行 temp-file `fsync`、原子 rename 与目录
`fsync` 后才 ACK 内核；部署必须在 daemon 重启时复用同一 `--data-dir`。状态文件损坏
或版本未知会使 daemon 启动失败（fail closed），不会清空后继续运行。仍打开的 inode
不会进入该文件；Redis session TTL 也不会被当作 fd 已关闭的证据。

## 挂载参数

当前 KestrelFS 不解析专用 mount option；`mount_nodev()` 会忽略传入的 `data`。
cache 设备配置必须在加载模块时给出，不要用 `mount -o` 传递：

```bash
mount -t kestrelfs none /mnt/kestrelfs
```

这不是稳定 mount-option ABI；未来若新增选项，须在本文登记其默认值与兼容语义。

## daemon CLI

| 参数 | 默认值 | 含义与约束 |
|---|---:|---|
| `--data-dir <PATH>` | `./.kestrelfs-data` | FileMetaStore 的 `meta.json`、LocalFsObjectStore 根目录及 `.orphan-retries-v1.json` 运维状态；目录不存在时创建。Redis/S3 组合也必须保留同一 data-dir 以恢复 orphan proof。|
| `--memory` | 关闭 | MetaStore 与 ObjectStore 均置于内存，进程退出即丢失；与 `--meta`、`--redis-prefix`、`--redis-ca-cert`、`--objects`、`--s3-endpoint` 冲突。|
| `--meta <REDIS_URL>` | 未设置 | 使用 RedisMetaStore；接受明文 `redis://` 或 TLS `rediss://`，未设置则使用 `{data-dir}/meta.json`。URL 可含凭据，daemon 不打印它。|
| `--redis-prefix <PREFIX>` | `kestrelfs` | Redis v2 key namespace；要求同时给出 `--meta`。|
| `--redis-ca-cert <PEM_PATH>` | 未设置 | 为 `rediss://` 增加私有/自签 CA trust anchor；要求同时给出 `--meta`，对 `redis://` 使用会 fail closed。未设置时 `rediss://` 使用系统 trust store。|
| `--redis-session-ttl-ms <N>` | `3000` | Redis writer session TTL（最小 300 ms）；每 TTL/3 心跳续约。session 过期或被 fence 后 mutation 返回 `ESTALE`，不会静默提交。要求同时给出 `--meta`。|
| `--objects <S3_URL>` | 未设置 | 使用 `s3://bucket/optional/prefix` 的 S3ObjectStore；未设置则使用 `data-dir` 下的 LocalFsObjectStore。|
| `--s3-endpoint <URL>` | 未设置 | MinIO 等 S3-compatible endpoint，要求同时给出 `--objects`；优先于环境变量 `S3_ENDPOINT`，自定义 endpoint 使用 path-style。|

File/Redis metadata 可分别与 LocalFs/S3 objects 组合；但多节点共享时必须同时选择
共享 metadata 与共享 objects，否则各节点看到的 namespace 不一致。目标 S3 bucket
必须预先存在。

## 环境变量与凭据

| 变量 | 作用 |
|---|---|
| `S3_ENDPOINT` | daemon 未传 `--s3-endpoint` 时的 endpoint fallback |
| `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` | AWS SDK 凭据 |
| `AWS_SESSION_TOKEN` | 可选临时凭据 token |
| `AWS_REGION` | AWS SDK region |
| `S3_BUCKET` / `S3_PREFIX` / `S3_CREATE_BUCKET=1` | 仅用于门控 S3 集成测试；不是 daemon 运行配置 |
| `REDIS_URL` | 仅用于门控 Redis 明文、通知与重连集成测试；daemon 运行时须用 `--meta` |
| `REDIS_TLS_URL` / `REDIS_TLS_CA_CERT` | 仅用于自签 CA TLS 门控测试；daemon 对应参数为 `--meta` / `--redis-ca-cert` |

不要把凭据写进日志、`cache_namespace` descriptor 或提交到仓库。生产环境还应避免
把 secret 直接留在可被其他用户读取的 shell history/process listing 中。

Redis 命令路径使用自动重连 connection manager。连接中断时，当次无法确认结果的
请求可能返回 `EIO`；Redis 恢复后，后续请求会在无需重启 daemon 的情况下重建连接。
这不把不确定 mutation 自动重放成“恰好一次”，调用方仍应按操作语义处理错误。
Pub/Sub 订阅也会以 50 ms 起、最多 1 s 的退避重连；重订阅成功会立即请求一次 durable
revision 对账。通知只缩短可见性延迟，约 100 ms revision poll 始终保留，所以丢失、
重复、乱序通知不会绕过 dirty-inode/full fail-closed 规则。

每个 Redis daemon 还注册
`<PREFIX>:meta:v2:sessions:<32-hex-session-id>` 字符串 key，值为同一 session id，
并按 `--redis-session-ttl-ms` 设置 TTL。mutation Lua 除 revision-CAS 外还校验该
精确 key/value；过期、删除或冲突返回 `ESTALE`，心跳只允许续约现存的同 token key，
不会把已失效 writer 复活。这是最小 writer-session fencing，不提供读 lease、单调
fencing generation、跨节点锁、range lease 或线性一致性。

## 安全与恢复边界

- cache 只能使用独占的块设备；自动化 insmod/mount/cache 测试只允许在 vng guest
  + loop 中运行，不触碰宿主机 zvol。
- 全零的前 2 MiB metadata 区可初始化为当前 format；非零的未知/损坏格式、旧格式、
  namespace mismatch 均默认拒绝，绝不自动迁移或 wipe。
- `kestrelfs-cache-admin wipe` 只能离线操作块设备，并要求
  `KESTRELFS_CACHE_WIPE_CONFIRM=<device>` 与 `--yes-really-wipe` 双确认。它仅清除
  cache metadata，使旧 slot 不可寻址，**不是 data 区安全擦除**。
- daemon 测试使用独立 `data_dir=/tmp/kestrelfs-<step>-$$`，并将输出保存在
  `"$data_dir/daemon.log"`；禁止丢到 `/dev/null`。

cache format v4 的磁盘布局、不变量与恢复协议见
[`phase4-nvme-cache.md`](phase4-nvme-cache.md)。
