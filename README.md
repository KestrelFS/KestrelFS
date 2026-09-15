<p align="center">
  <a href="https://github.com/KestrelFS/KestrelFS">
    <img src=".github/assets/kestrelfs-logo.svg" alt="KestrelFS Logo" width="650">
  </a>
</p>

<p align="center">
  <strong>超高性能、内核级加速的云原生分布式文件系统</strong>
</p>

<p align="center">
  <a href="https://github.com/KestrelFS/KestrelFS/actions"><img src="https://img.shields.io/badge/Kernel-Linux%206.x-blue.svg" alt="Kernel"></a>
  <a href="https://github.com/KestrelFS/KestrelFS"><img src="https://img.shields.io/badge/Language-C%20%2F%20Rust-orange.svg" alt="Language"></a>
  <a href="https://github.com/KestrelFS/KestrelFS/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-Apache%202.0-green.svg" alt="License"></a>
</p>

---

**高性能云原生分布式文件系统**：采用务实的 **C 内核模块 + Rust 用户态守护进程** 混合架构，目标在缓存命中路径上超越 JuiceFS。

> ⚠️ **项目状态：早期开发（Phase 4 Step 27 已验收；Step 28 `read_iter` / iov_iter 缓存读待 Cursor 验收；IPC ABI v11；cache format v4）。**
>
> Phase 1–3 已完成。Phase 3 提供可用的控制面原型（动态 VFS、16 KiB bounce
> 数据/名字 IPC、`FileMetaStore`、可选 Redis 元数据原型、`LocalFsObjectStore`、
> 可选 S3/MinIO 对象原型）。Phase 4 已实现：4 KiB 块索引持久化/恢复、READ_DATA
> miss 填充、同步内核 BIO 命中、rewrite/truncate/unlink/rename-overwrite 失效、
> namespace SHA-256 绑定、对齐连续 hit 直达 pinned user pages、满盘 block-LRU、
> data/index CRC32、v4 单页 intent journal（半提交恢复为安全 miss），以及多个
> cache-hit 调用者并行等待同步 BIO，以及 Step 27 离线只读诊断/双确认 metadata
> wipe。Step 28 待验收工作树把动态文件切到 `read_iter`，让 read/pread/readv/preadv
> 共用 iov_iter cache/miss 路径；真正的异步 completion 流水线与多节点失效尚未实现。
> 详见[路线图](#路线图)、`HANDOFF.md` 与
> `docs/remaining-capabilities.md`。

---

## 目录

- [愿景](#愿景)
- [架构](#架构)
- [路线图](#路线图)
- [仓库布局](#仓库布局)
- [环境要求](#环境要求)
- [编译](#编译)
- [使用](#使用)
- [构建验证](#构建验证)
- [设计说明](#设计说明)
- [贡献](#贡献)
- [许可证](#许可证)

---

## 愿景

KestrelFS 目标是成为分布式、POSIX 兼容的云文件系统，并同时具备：

- **内核级少拷贝 / 零上下文切换热路径**：本地 NVMe 缓存命中不进入用户态 daemon。
- **对象存储耐久性**：冷数据落 S3 兼容后端；块级切片与流式上下行。
- **强一致 POSIX 元数据**：由 Redis/TiKV 等支撑，控制面在用户态独立扩展。

明确产品目标：**在缓存命中延迟与吞吐上击败 JuiceFS**——把本地缓存读热路径放进内核，
把易变的业务逻辑（元数据、切片、S3 I/O）留在内存安全的 Rust daemon 中。

---

## 架构

KestrelFS 刻意把**控制面**与**数据面**拆到不同语言与特权边界：

```
                         ┌─────────────────────────────┐
                         │      用户态 (Rust)            │
                         │                               │
                         │   KestrelFS 控制面 Daemon     │
                         │   （Tokio 异步）               │
                         │                               │
                         │  • POSIX 元数据 (Redis/TiKV)  │
                         │  • Chunk/Block 切片            │
                         │  • S3 SDK（对象存储 I/O）      │
                         └───────────────▲───────────────┘
                                         │ mmap() 共享双环
                                         │ + ioctl()/poll()
                         ┌───────────────▼───────────────┐
                         │      内核态 (C)                │
                         │                                 │
                         │   KestrelFS 内核模块            │
                         │                                 │
                         │  • VFS（super/inode/file）      │
                         │  • /dev/kestrel_ctl             │
                         │  • 本地 NVMe 持久缓存           │
                         │  • read_iter / iov 缓存读       │
                         └─────────────────────────────────┘
```

**数据面（内核，C）。** 树外 Linux 内核模块：

- 注册 VFS 文件系统类型，实现挂载与文件服务所需的 inode/dentry/file 操作。
- 拥有本地 NVMe SSD 缓存边界。Step 20–28：索引持久化、miss 填充、namespace
  校验、对齐 hit 直达用户页、LRU 回收、CRC 校验、v4 intent journal，以及共享
  读锁下的并行同步 BIO 命中；离线工具可 inspect 并在双确认后重置 metadata；
  `read_iter` 让普通和 vectored read 共用结构化 iov 路径。
  详见
  [`docs/phase4-nvme-cache.md`](docs/phase4-nvme-cache.md)。
- 仅在必要时（miss、元数据查找）经无锁共享内存 IPC 与 Rust daemon 通信。

**控制面（用户态，Rust）。** Tokio 异步 daemon：

- 拥有全部 POSIX 元数据语义（可接 Redis/TiKV）。
- 执行文件数据的 chunk/block 切片。
- 经 AWS SDK for Rust 访问 S3 兼容对象存储。
- 不阻塞内核：慢 I/O（网络、磁盘）离开 VFS 调用路径。

**桥接。** 内核与 Rust 通过 `/dev/kestrel_ctl` 字符设备通信：内核分配一块可
`mmap()` 的共享内存，内含**两个独立无锁 SPSC 环形队列**（请求/响应），避免经
socket/Netlink 拷贝。跨语言结构在 `kestrelfs_ipc.h` 单一定义，供内核与 Rust
`bindgen` 共用，防止 ABI 静默漂移。详见[设计说明](#设计说明)。

---

## 路线图

开发按四个阶段严格推进。**请勿假设下表未标「已完成」的阶段已经实现。**

| 阶段 | 目标 | 状态 |
|---|---|---|
| **1. 最小 C 内核 VFS 骨架** | 树外模块、VFS 注册、super/inode/file | ✅ 已完成 |
| **2. C↔Rust IPC 桥** | `/dev/kestrel_ctl`、mmap 双 SPSC 环、poll/ioctl、Rust 消费端 | ✅ 已完成 |
| **3. Rust 控制面** | MetaStore + ObjectStore、动态 VFS、bounce I/O、symlink、truncate、GC、本地持久化、可选 Redis/S3 原型（ABI v11 / Step 1–17） | ✅ 原型完成 |
| **4. 内核拥有的 NVMe 缓存** | 内核直访本地块设备；命中绕过 Rust daemon | 🚧 Step 27 已验收；Step 28 CACHE-VFS 待 Cursor 验收（format v4） |

步骤级进度、opcode 与已知限制见 `HANDOFF.md`；后续排期与 Codex 提示词见
`docs/remaining-capabilities.md`。

---

## 仓库布局

```
KestrelFS/   # 本地目录历史上可能叫 FerroFS
├── LICENSE / README.md / HANDOFF.md
├── kestrelfs/                 # 内核模块（C）— 树外构建
│   ├── Makefile, super.c, inode.c, dir.c, file.c, cache.c
│   ├── chardev.c, ipc_ring.c
│   ├── kestrelfs.h, kestrelfs_ipc.h   # ★ ABI 契约（ABI v11）
│   └── chardev_test.c
├── docs/remaining-capabilities.md  # 规划 / 决策 / 当前提示词 / 实现日志
├── docs/phase4-nvme-cache.md       # Phase 4 归属、索引、失效设计
├── tools/kestrelfs-cache-admin.c    # v4 cache 离线诊断与双确认 metadata wipe
├── test-step19-cache-vng.sh … test-step28-cache-vfs-vng.sh
├── test-step28-cache-vfs.c           # preadv / iovec 边界验证辅助程序
└── daemon/                    # Rust 控制面 daemon
    └── src/{main,abi,meta,meta_persist,meta_redis,object_store,object_store_s3,fs_model,device,ring,ioctl}.rs
```

---

## 环境要求

- **Linux 内核 5.x/6.x**，且已安装与当前运行内核匹配的 headers
  （`/lib/modules/$(uname -r)/build` 必须存在）。
  - Debian/Ubuntu：`sudo apt install linux-headers-$(uname -r)`
- **GCC** 与常规内核构建工具（`make`、`bc`、`flex`、`bison` 等）。
- **root 权限** 用于 `insmod`/`rmmod`/`mount`。
- （Phase 3+）**Rust 工具链**（stable，`rustup`）及 `bindgen`。

> KestrelFS **从不**对着完整内核源码树整树编译，也**从不**跑整内核
> `make -j`。一律树外：`make -C $(KDIR) M=$(PWD) modules`。

---

## 编译

```bash
cd kestrelfs
make
```

产出 `kestrelfs.ko`，实际调用当前运行内核的 headers：

```
make -C /lib/modules/$(uname -r)/build M=$(pwd) modules
```

指定其他内核 headers：

```bash
make KDIR=/lib/modules/<other-version>/build
```

清理：

```bash
make clean
```

编译 daemon：

```bash
cargo build --release --manifest-path daemon/Cargo.toml
```

---

## 使用

### 加载模块并挂载（开发机示例）

> **注意**：带 `cache_device` 的正确性/回归测试当前约定只在 **vng guest + loop**
> 中执行；不要在宿主机对物理 zvol 做自动化验收，除非人类明确授权。

```bash
make -C kestrelfs
sudo insmod kestrelfs/kestrelfs.ko
sudo mkdir -p /mnt/kestrelfs
# 另开终端启动 daemon（日志写入 data-dir，勿 >/dev/null）
data_dir=/tmp/kestrelfs-demo
mkdir -p "$data_dir"
sudo ./daemon/target/release/kestrelfs-daemon --data-dir "$data_dir" \
  >"$data_dir/daemon.log" 2>&1 &
sudo mount -t kestrelfs none /mnt/kestrelfs
```

启用内核缓存时（需合法 64-hex `cache_namespace`，且 `cache_device` 必须是块设备）：

```bash
cache_namespace=$(printf 'v1;meta=file:%s/meta.json;objects=local:%s' \
  "$data_dir" "$data_dir" | sha256sum | awk '{print $1}')
sudo insmod kestrelfs/kestrelfs.ko \
  cache_device=/dev/loop0 cache_size_mib=64 \
  cache_namespace="$cache_namespace"
```

当前挂载已是**可写动态 VFS**（create/mkdir/rename/symlink/read/write 等），
不是 Phase 1 的只读演示。

### 卸载

```bash
sudo umount /mnt/kestrelfs
sudo rmmod kestrelfs
```

### 离线检查或显式重置缓存

先卸载使用该 cache device 的模块，再构建只读诊断工具：

```bash
make -C tools
./tools/kestrelfs-cache-admin inspect /dev/loop0
```

确需丢弃 cache 索引时，必须同时给出两个目标一致的确认；普通文件会被拒绝：

```bash
KESTRELFS_CACHE_WIPE_CONFIRM=/dev/loop0 \
  ./tools/kestrelfs-cache-admin wipe /dev/loop0 --yes-really-wipe
```

该动作只清零 2 MiB cache metadata，使旧 slot 不再可寻址并允许下次加载重新格式化；
它不会安全擦除 data 区，也不会修改 MetaStore/ObjectStore 权威数据。

### 检查 IPC 字符设备

```bash
ls -l /dev/kestrel_ctl
gcc -O2 -Wall -I kestrelfs -o kestrelfs/chardev_test kestrelfs/chardev_test.c
sudo kestrelfs/chardev_test
```

### 控制面 daemon 参数（Phase 3+）

```bash
cd daemon
cargo build --release
sudo ./target/release/kestrelfs-daemon --data-dir /var/lib/kestrelfs/objects
```

常用选项：

- `--data-dir <PATH>` — 持久化 `meta.json` 与本地对象目录（默认 `./.kestrelfs-data`）
- `--memory` — 内存 MetaStore + ObjectStore（重启丢失；仅测试）
- `--meta <REDIS_URL>` — 例如 `redis://127.0.0.1:6379/0`；对象仍可用 `--data-dir`
- `--redis-prefix <PREFIX>` — Redis 快照键前缀（默认 `kestrelfs`；完整键
  `<PREFIX>:meta:v1`）；需同时使用 `--meta`
- `--objects <S3_URL>` — 例如 `s3://bucket/kestrelfs-data`；不可与 `--memory` 同用
- `--s3-endpoint <URL>` — MinIO 等自定义 endpoint（path-style）；也可读 `S3_ENDPOINT`

S3 凭据走标准 AWS SDK 链（`AWS_ACCESS_KEY_ID` 等），**从不**作为 CLI 参数或打印到日志。
目标 bucket 须预先存在。

Redis 后端当前是**单 key 全量 JSON 快照 + Lua CAS** 原型。可选集成测试：

```bash
cd daemon
REDIS_URL=redis://127.0.0.1:6379/15 \
  cargo test redis_url_gated_full_semantics_and_restart -- --nocapture
```

S3 门控测试示例：

```bash
S3_ENDPOINT=http://127.0.0.1:9000 S3_BUCKET=kestrelfs-test \
AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin \
AWS_REGION=us-east-1 cargo test s3_environment_gated -- --nocapture
```

**验证持久化：**

1. 启动 daemon（指定 `--data-dir`）并 mount。
2. 写入文件后读回。
3. 重启同一 `--data-dir` 的 daemon，确认数据仍在。

需要 IPC 的路径依赖 daemon 在线；缓存命中路径在填充完成后可在 daemon 停止时仍命中
（见 Phase 4 vng 测试）。

---

## 构建验证

改动 `kestrelfs/` 后的常用手工检查：

```bash
# 1. 编译
make -C kestrelfs
make -C tools
cargo test --manifest-path daemon/Cargo.toml
cargo clippy --manifest-path daemon/Cargo.toml --all-targets -- -D warnings

# 2. 加载
sudo insmod kestrelfs/kestrelfs.ko
lsmod | grep kestrelfs
grep kestrelfs /proc/filesystems
dmesg | tail -5

# 3. 挂载读写（需先启动 daemon）
sudo mkdir -p /mnt/kestrelfs
sudo mount -t kestrelfs none /mnt/kestrelfs

# 4. 字符设备冒烟
gcc -O2 -Wall -I kestrelfs -o kestrelfs/chardev_test kestrelfs/chardev_test.c
sudo kestrelfs/chardev_test

# 5. 干净卸载
sudo umount /mnt/kestrelfs
sudo rmmod kestrelfs
```

任何 `dmesg` 中的 `WARNING:` / `Oops:` / `BUG:` 都应阻断合入。

**缓存回归（仅 vng guest + loop）：**

```bash
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step28-cache-vfs-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step27-ops-recovery-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step25-cache-txn-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step24-checksum-vng.sh"
vng --run --network user --cwd "$PWD" --exec "$PWD/test-step26-cache-async-vng.sh"
# … Step 23/22/20/19/15 等脚本见 HANDOFF.md §7
```

---

## 设计说明

### 内核↔Rust IPC 契约

共享区（`struct kestrelfs_shared_region`，见 `kestrelfs_ipc.h`）布局：

```
offset 0       : header（magic、abi_version、padding）     — 64 B
offset 64      : req_ctrl  (head/tail/capacity)            — 64 B
offset 128     : resp_ctrl (head/tail/capacity)            — 64 B
offset 192     : req_slots[1024]   (内核 → Rust)           — 64 KiB
offset 65728   : resp_slots[1024]  (Rust → 内核)           — 64 KiB
offset 131264  : data_buffer（ABI v8+ bounce，批量 I/O 与长名）— 16 KiB
                                         合计: 147648 B（约 144 KiB）
```

精确大小由 `KESTRELFS_SHM_REGION_SIZE` / `SHM_REGION_SIZE` 在 C/Rust 两侧断言。

每个事件槽 64 字节（一条 cacheline）：序号、opcode、flags、req_id、error_code、
以及 32 字节 opcode 载荷。环控制块按 cacheline 对齐并物理分离，避免伪共享。

字段使用 `<linux/types.h>` 定宽类型，显式填充与自然对齐（不 packed）。结构不变量
用 `_Static_assert` / Rust 侧断言在编译期检查。

**唤醒模型**（无忙等）：

- **内核 → Rust**：推入请求环后 `wake_up_interruptible()`，唤醒在
  `/dev/kestrel_ctl` 上 `poll`/`epoll_wait` 的 Rust 线程。
- **Rust → 内核**：推入响应环后发 `KESTRELFS_IOC_NOTIFY_RESP` ioctl，内核
  `wake_up_all()` 等待中的内核线程。

更多协议说明见 `kestrelfs/kestrelfs_ipc.h` 与 `kestrelfs/chardev.c`
（`vmalloc_user()` / `remap_vmalloc_range()`）。

### 已知限制

Phase 3 已支持 create/mkdir/unlink/rmdir/rename/symlink、16 KiB bounce 读写、
truncate/`O_TRUNC`、批量 readdir、255 字节文件名（ABI v11）。仍缺：

- 尚无 hard link；symlink 目标目前要求 UTF-8，最长 4095 字节。
- GC delete 失败可能泄漏对象（meta 先提交；尚无持久重试队列）。
- 数据/名字 IPC 由一把全局 mutex 串行化。
- Redis 元数据仍是单 key 全量快照原型；S3 delete 失败同样可泄漏。
- Phase 4 仍是单节点原型：v4 绑定 namespace；CRC32 非密码学；半提交可安全
  miss，但无双 superblock/metadata 镜像；不同 reader 的 hit 可并行，但每个 BIO
  仍同步等待，mutation 会等待在途 reader；无多节点远端失效；journal 为每次
  index 变更增加 prepare/clear flush 成本。
- Step 28 已覆盖 read/pread/readv/preadv；pinned BIO 只在单个当前 iovec 段内合并，
  跨段或非对齐范围仍用 `copy_to_iter`。尚无 page-cache/readahead/splice 全覆盖。
- Step 27 wipe 只清零前 2 MiB cache metadata，让旧 data slot 不再可寻址；它不是
  数据区安全擦除。操作必须离线、目标必须是块设备，并同时提供环境变量和命令行
  旗标确认；不会自动迁移旧格式。

权威细节见 `HANDOFF.md` §6；后续排期见 `docs/remaining-capabilities.md`。

### 编码规范

- 内核 C 遵循 Linux 内核编码风格，**仅树外构建**，不修改/重编整棵内核树。
- 内核代码刻意精简，故障路径防御性检查；复杂业务逻辑下沉到 Rust。
- 跨 C/Rust 结构只在双用途头文件中定义一次，两侧编译期断言布局。

---

## 贡献

项目按阶段门控开发。请在动手实现**当前路线图阶段以外**的工作前先开 issue；
乱序贡献在前置阶段落地前通常不会合并。

协作约定：Cursor 验收与提示词写入 `docs/remaining-capabilities.md` §8；
实现方读取该节执行，汇报写入 §9；已验收事实写入 `HANDOFF.md`。

---

## 许可证

采用 [Apache License, Version 2.0](LICENSE)。
