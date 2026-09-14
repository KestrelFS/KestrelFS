# Phase 4：内核拥有的 NVMe 缓存

## 当前边界（Step 18–19）

KestrelFS 的本地缓存由内核模块拥有。缓存命中时，内核直接把块设备中的
数据交给 VFS 调用者，不进入共享 ring，也不唤醒 Rust daemon。daemon 仍是
远端数据和元数据的权威控制面，只负责缓存未命中时通过现有 `READ_DATA`
路径取回数据，以及后续步骤中的填充协调和失效通知。

Step 18 落地模块参数和 read 路径中恒 miss 的
`kestrelfs_cache_lookup()` hook。Step 19 使用独占读写模式真正 claim 专用
块设备，校验 geometry，格式化/复用最小 cache superblock，并初始化空的内存
哈希索引。它仍不填充索引、不读缓存数据，也不实现 DMA hit 路径。

## 缓存设备

缓存后端必须是 Linux 块设备节点，例如：

- NVMe raw namespace（如 `/dev/nvme0n1` 或专用分区）；
- zvol（如 `/dev/zvol/pool/kestrel-cache`）；
- 用于开发和 VM 测试的 loop block device。

禁止把普通文件（包括位于 ZFS dataset 中的普通文件路径）直接作为缓存设备。
两者的写回、刷新、生命周期和故障语义不同，普通文件还会造成文件系统递归
和 page-cache 干扰。Step 19 的块设备 open/claim API 会拒绝非块设备，并让
其他独占使用者得到 busy；模块卸载或初始化失败时释放 claim。
模块同时请求 `BLK_OPEN_RESTRICT_WRITES`，但启用
`CONFIG_BLK_DEV_WRITE_MOUNTED` 的内核仍允许未参与 holder 协议的 raw writer；
部署侧必须保证该专用设备不被其他进程写入。

当前预留两个只读模块参数：

```text
cache_device=/dev/loop0
cache_size_mib=4096
```

`cache_size_mib=0` 表示以整个块设备容量为上限；非零值转换为 MiB 后必须不
超过实际容量。可用容量向下对齐 logical sector，且必须容纳 2 MiB 元数据区和
至少一个 4 KiB data block。logical/physical sector 必须为 2 的幂、logical
至少 512 字节，且两者都必须整除 4 KiB superblock。

## Step 19 磁盘格式

所有多字节字段采用 little-endian，LBA 固定表示 512 字节 sector。磁盘头占
第一个 4 KiB：

| Offset | 字段 | 类型/含义 |
|---:|---|---|
| 0 | magic | u64，字节串 `KFSCACHE` |
| 8 | version | u32，当前为 1 |
| 12 | header_size | u32，4096 |
| 16 | logical_block_size | u32，格式化时设备值 |
| 20 | physical_block_size | u32，格式化时设备值 |
| 24 | cache_block_size | u32，4096 |
| 28 | index_entry_size | u32，32 |
| 32 | usable_sectors | u64，`cache_size_mib` 生效后的容量 |
| 40 | index_start_lba | u64，当前为 8 |
| 48 | index_capacity | u64，当前为 65408 |
| 56 | data_start_lba | u64，当前为 4096（2 MiB） |
| 64 | format_generation | u64，格式化时生成且非零 |
| 72 | reserved | 补齐至 4096 字节 |

LBA 8 到 4095 是预留的盘上索引区。每个 32 字节条目由
`inode_id + file_offset + lba + generation` 组成；Step 19 只固定格式，不写
条目。数据区从 LBA 4096 开始。

自动格式化只发生在整个 2 MiB 保留元数据区全零时，并只写入 4 KiB
superblock 后 flush。非零但 magic/version/任一 geometry 不匹配时 fail
closed，模块加载返回错误，不覆盖已有内容。用不同 `cache_size_mib` 重新加载
已格式化设备也视为 geometry mismatch。

## 命中和未命中路径

```text
VFS read
  -> kestrelfs_cache_lookup(inode, file_offset, length)
       -> hit:  内核块 I/O / DMA -> 用户缓冲区（后续 Step）
       -> miss: 返回 -ENODATA
  -> 取得 kestrelfs_data_ipc_lock
  -> READ_DATA + 16 KiB bounce + req/resp ring
  -> daemon 从 ObjectStore 读取
```

hook 特意位于 `kestrelfs_data_ipc_lock` 之前。这样未来的 hit 不需要 bounce
buffer，也不会与单 in-flight data/name IPC 串行化。miss 继续使用 ABI v11，
共享内存布局和同步模型均不改变。

## 索引模型

计划中的索引键是 `(inode_id, file_offset_range)`，值至少包含：

- cache device 上的起始 LBA 和长度；
- 文件/extent generation，用来拒绝陈旧映射；
- 有效位和校验信息；
- 填充状态，避免读取尚未完整写入的 extent。

查找必须覆盖请求范围或安全地返回部分命中；第一版可以按固定对齐 extent
组织。Step 19 已初始化空 `rhashtable`，内存条目键为
`(inode_id, file_offset)`，值预留 `LBA + generation + length`；盘上条目格式如
上。索引尚不装载/落盘，`kestrelfs_cache_lookup()` 仍恒 miss。

## 填充、失效与顺序

miss 时 daemon 仍通过 `READ_DATA` 把远端数据放入 bounce。后续填充可在
数据交付后由内核异步写入 cache device；只有数据和校验信息落稳后才能发布
索引项。填充失败不影响已经成功的远端读取。

所有可能改变或删除文件字节的操作都必须在可见性上先使旧缓存失效：

- rewrite：失效与写入范围相交的 extent；
- truncate：失效新 EOF 之后以及与边界相交的 extent；
- unlink：失效该 inode 的全部 extent；
- rename 覆盖：被覆盖目标 inode 的 extent 按 unlink 语义失效；普通 rename
  不改变 inode id，因此不应误删源 inode 的缓存。

推荐使用 inode generation/epoch 让读侧先检测陈旧项，再异步回收 LBA，避免
失效和并发命中之间误读旧数据。daemon 的通知只能辅助回收；内核 VFS 的
rewrite/truncate/unlink 路径必须能同步阻止旧项继续命中。

## 故障与安全原则

- cache 永远不是唯一数据副本；损坏或不可用时退化为 miss。
- cache 元数据不能被当作 MetaStore/ObjectStore 的权威状态。
- 设备断开、校验失败或恢复不确定时 fail open 到远端读取，而不是返回陈旧数据。
- Step 19 仅对 superblock 使用同步 BIO；不实现 data block I/O、完整 DMA、
  写缓存、索引恢复/落盘、填充、失效或缓存回收。
- 当前格式没有 checksum 或双 superblock；任何已识别字段不匹配都会拒绝，
  但 reserved 区和未来索引项的损坏检测仍属于后续版本。
- exclusive holder 防止其他内核 holder 抢占设备，但不能在所有内核配置下阻止
  root 直接 raw write；设备隔离仍是部署要求。
