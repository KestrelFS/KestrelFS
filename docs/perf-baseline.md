# KestrelFS vng 粗测基线

本文记录可重复的路径级回归基线，不是生产 benchmark、容量规划依据或 SLA。
vng/QEMU 调度方式、宿主负载、loop 后端和内核版本都会显著改变数字；验收首先看
数据校验和 `STEP52_PERF_*_PASS`，数值只用于发现数量级退化。

## Step 52 基线

2026-09-20 的一次完整通过样本：

| 环境/路径 | workload | 结果 |
|---|---|---:|
| vng guest，Linux `6.12.38+deb13-amd64`，64 MiB loop cache | 1 MiB buffered `pwrite`，不含 fsync | 1,530,035 ns；653.58 MiB/s |
| 同上，显式 durability 点 | 上述文件紧接一次 `fsync` | 116,410,856 ns |
| VFS page cache 热读 | 1 MiB × 64，逐字节校验 | 30,411,467 ns；2104.47 MiB/s |
| READ_DATA 冷读并 fill | drop_caches 后 1 MiB，daemon 在线 | 483,247,760 ns；2.07 MiB/s |
| 内核 NVMe/loop cache hit | 再次 drop_caches，daemon 停止，保留已打开 fd | 6,304,203 ns；158.62 MiB/s；256 个 async BIO |

`pwrite` 吞吐只度量普通 write-behind 返回前的 page-cache dirtying；后面的 fsync
时间单独列出，二者不能相加后仍称为“异步写吞吐”。读测试包含确定性数据校验成本。
NVMe 一栏实际使用 loop，目的是验证内核 cache hit 路径及相对数量级，不代表真实
NVMe 设备性能。单次样本不设固定倍率或延迟门槛。


另一次 Cursor 验收样本（同日、同内核/loop 规模）观测到 write-behind ~1324 MiB/s、
page-cache 热读 ~2153 MiB/s、daemon-free NVMe/loop hit ~178 MiB/s（256 async BIO），
`umount_ms=23`。两套数字都只作数量级对照。


复现命令：

```bash
make -C kestrelfs
cargo build --release --manifest-path daemon/Cargo.toml
vng --run --network user --rwdir "$PWD" --cwd "$PWD" \
  --exec "$PWD/tests/test-step52-perf-vng.sh"
```

脚本只在 guest 内创建 loop、执行 `insmod` 和 mount；daemon 使用 PID 隔离的
`/tmp/kestrelfs-step52-perf-*`，日志保存在其 `daemon.log`。NVMe 测量前先保留已打开
fd，避免 daemon 停止后路径 LOOKUP 把元数据 IPC 错误混进 data-cache hit 测量。

## Step 52 两节点数据面配方

`tests/test-step52-dist-vng.sh` 在宿主只负责启动和协调两个并行 vng guest。每个 guest
各有独立 daemon、挂载、data-dir 和 loop cache，但共享随机 Redis prefix 与 S3
prefix。Redis、S3 凭据只经环境变量传入，不写入仓库或 namespace digest。

```bash
export REDIS_URL='redis://USER:PASSWORD@REDIS_HOST:PORT/DB'
export S3_ENDPOINT='http://S3_HOST:PORT'
export S3_BUCKET='EXISTING_TEST_BUCKET'
export AWS_ACCESS_KEY_ID='...'
export AWS_SECRET_ACCESS_KEY='...'
export AWS_REGION='us-east-1'
./tests/test-step52-dist-vng.sh
```

测试顺序是：A 写入版本 1 并 fsync；B 打开同一 inode、读入 page cache/NVMe cache；
A 覆写版本 2 并 fsync；B 在 durable revision probe 后必须看到版本 2，且 inode
coherence batch/entry 计数必须增长。B 随后保留 fd、drop_caches、停止 daemon，仍须
从本地 cache 读回版本 2。2026-09-20 样本输出
`STEP52_DIST_VISIBILITY_LATENCY_MS=10`（Codex）/`35`（Cursor）和 `STEP52_DIST_TWO_NODE_PASS`。

10 ms 只是本次轮询相位下的观测值。协议仍以约 100 ms probe 周期提供最终一致窗口，
没有 lease、pub/sub 或跨 Redis/S3 事务；A 必须完成 fsync，B 必须允许至少一个 probe
周期。probe/dirty history 不可证明时仍按既有逻辑全 cache fail closed。
