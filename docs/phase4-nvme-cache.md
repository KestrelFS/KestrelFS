# Phase 4：内核拥有的 NVMe 缓存

## Step 18 边界

KestrelFS 的本地缓存由内核模块拥有。缓存命中时，内核直接把块设备中的
数据交给 VFS 调用者，不进入共享 ring，也不唤醒 Rust daemon。daemon 仍是
远端数据和元数据的权威控制面，只负责缓存未命中时通过现有 `READ_DATA`
路径取回数据，以及后续步骤中的填充协调和失效通知。

Step 18 只落地可编译、可加载的边界：模块参数、块设备类型校验，以及 read
路径中恒 miss 的 `kestrelfs_cache_lookup()` hook。它不打开设备、不分配索引，
也不发起任何块 I/O 或 DMA。

## 缓存设备

缓存后端必须是 Linux 块设备节点，例如：

- NVMe raw namespace（如 `/dev/nvme0n1` 或专用分区）；
- zvol（如 `/dev/zvol/pool/kestrel-cache`）；
- 用于开发和 VM 测试的 loop block device。

禁止把普通文件（包括位于 ZFS dataset 中的普通文件路径）直接作为缓存设备。
两者的写回、刷新、生命周期和故障语义不同，普通文件还会造成文件系统递归
和 page-cache 干扰。Step 18 在指定 `cache_device` 时用 inode 类型检查拒绝非
块设备，但不会打开或写入该设备。

当前预留两个只读模块参数：

```text
cache_device=/dev/loop0
cache_size_mib=4096
```

`cache_size_mib=0` 预留表示使用设备容量；Step 18 不实际应用大小限制。后续
真正写盘前还必须加入独占/共享策略、容量和扇区对齐校验、superblock 格式及
崩溃恢复。

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
组织。索引及其并发控制属于后续步骤，Step 18 恒 miss。

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
- Step 18 不实现完整 DMA、direct I/O、写缓存、持久索引或缓存回收。
