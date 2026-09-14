# Phase 4：内核拥有的 NVMe 缓存

## 当前边界（Step 18–21）

KestrelFS 的本地缓存由内核模块拥有。缓存命中时，内核直接把块设备中的
数据交给 VFS 调用者，不进入共享 ring，也不唤醒 Rust daemon。daemon 仍是
远端数据和元数据的权威控制面，只负责缓存未命中时通过现有 `READ_DATA`
路径取回数据，以及后续步骤中的填充协调和失效通知。

Step 18 落地模块参数和 read hook；Step 19 使用独占读写模式真正 claim 专用
块设备，校验 geometry，并格式化/复用最小 cache superblock。Step 20 在不改变
cache format v1 和 IPC ABI v11 的前提下启用盘上索引恢复、READ_DATA miss 后同步
fill、同步块读 hit，以及 rewrite/truncate/unlink/rename-overwrite 失效。当前 hit 是
普通 BIO 加 `copy_to_user()`，不是 DMA/零拷贝最终路径。

Step 21 要求每个 cache device 显式绑定一个 logical filesystem namespace。
namespace 不匹配时在恢复任何 index entry 前 fail closed，从而阻止 inode id 在
不同 FileMetaStore data-dir 或 Redis namespace 中重用造成的错误命中。IPC ABI
未改变，仍为 v11。

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

当前有三个只读模块参数：

```text
cache_device=/dev/loop0
cache_size_mib=4096
cache_namespace=<64 hex digits>
```

`cache_size_mib=0` 表示以整个块设备容量为上限；非零值转换为 MiB 后必须不
超过实际容量。可用容量向下对齐 logical sector，且必须容纳 2 MiB 元数据区和
至少一个 4 KiB data block。logical/physical sector 必须为 2 的幂、logical
至少 512 字节，且两者都必须整除 4 KiB superblock。

只要指定 `cache_device`，就必须同时传入 `cache_namespace`。其值是 logical
filesystem namespace 规范描述的 SHA-256 digest，必须恰好为 64 个十六进制
字符；内核只做 hex 解码和精确比较，不在内核中解析路径、Redis URL 或凭据。
建议部署层使用稳定、无凭据且无歧义的描述，例如：

```text
v1;meta=file:/absolute/data-dir/meta.json;objects=local:/absolute/data-dir
v1;meta=redis:host:6379/db#prefix;objects=s3:endpoint/bucket/prefix
```

路径必须先规范化为绝对路径，endpoint/DB/prefix/bucket/object prefix 也必须采用
部署统一的规范形式；Redis 密码和 S3 secret 不应进入描述。示例：

```bash
cache_namespace=$(printf '%s' \
  'v1;meta=file:/tmp/kestrelfs-debug/meta.json;objects=local:/tmp/kestrelfs-debug' \
  | sha256sum | awk '{print $1}')
```

digest 可出现在 `/sys/module/kestrelfs/parameters/`，但它不是凭据。相同 logical
filesystem 在重启时必须使用同一 digest；修改 MetaStore namespace 或 ObjectStore
dataset 时必须使用不同 digest。

## Step 19–21 磁盘格式

所有多字节字段采用 little-endian，LBA 固定表示 512 字节 sector。磁盘头占
第一个 4 KiB：

| Offset | 字段 | 类型/含义 |
|---:|---|---|
| 0 | magic | u64，字节串 `KFSCACHE` |
| 8 | version | u32，当前为 2 |
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
| 72 | namespace_id | 32 字节 SHA-256 digest |
| 104 | reserved | 补齐至 4096 字节 |

LBA 8 到 4095 是预留的盘上索引区。每个 32 字节条目由
`inode_id + file_offset + lba + generation` 组成；全零条目表示空闲。数据区从
LBA 4096 开始。Step 20 将索引 slot 与 data LBA 固定一一映射：
`lba = data_start_lba + slot * 8`。有效 slot 数是索引容量与实际 4 KiB data block
数量的较小者，因此小设备不会产生越界 LBA。

Step 21 将 cache format 从 v1 bump 到 v2；v1 设备不会自动迁移或清空。自动格式化
只发生在整个 2 MiB 保留元数据区全零时，并只写入 4 KiB
superblock 后 flush。非零但 magic/version/任一 geometry 不匹配时 fail
closed，模块加载返回错误，不覆盖已有内容。用不同 `cache_size_mib` 重新加载
已格式化设备也视为 geometry mismatch；v2 namespace digest 不匹配会输出独立的
`cache namespace identity mismatch` 并拒绝加载。当前没有 `wipe` 模块参数，重用
旧 v1 或其他 namespace 的设备必须由运维显式清空后再加载，避免误操作。

## 命中和未命中路径

```text
VFS read
  -> kestrelfs_cache_lookup(inode, file_offset, length)
       -> hit:  同步 4 KiB BIO -> 内核页 -> copy_to_user（不进 ring）
       -> miss: 返回 -ENODATA
  -> 取得 kestrelfs_data_ipc_lock
  -> READ_DATA + 16 KiB bounce + req/resp ring
  -> daemon 从 ObjectStore 读取
  -> 内核同步 fill 完整的 4 KiB 对齐块
```

hook 位于 `kestrelfs_data_ipc_lock` 之前，因此 hit 不占 bounce buffer，也不会与
单 in-flight data/name IPC 串行化。miss 继续使用 ABI v11，共享内存布局和同步
模型均不改变。只有请求的整个 EOF-clamped 范围都有索引时才按 hit 返回；否则
整次请求安全回退到 READ_DATA，避免把部分结果暴露给调用者。

## 索引模型

索引键为 `(inode_id, 4 KiB-aligned file_offset)`，内存值为
`LBA + generation + slot`。模块加载时按 4 KiB 页扫描 2 MiB 元数据区，校验每个
非空条目的 offset 对齐、slot/LBA 对应关系和非零 generation，再恢复到
`rhashtable` 和 slot bitmap。重复 key、半空条目或越界映射均使模块 fail
closed；不会猜测或覆盖可疑格式。

generation 在当前版本中用于持久化条目版本和恢复后继续单调分配；同一模块
生命周期内另有全局 mutation epoch 防止 miss 与并发 mutation 竞态发布旧 fill。
它还不是跨主机一致性协议或数据校验码。

## 填充、失效与顺序

miss 时 daemon 仍通过 `READ_DATA` 把远端数据放入 bounce。Step 20 在把数据交付
给调用者后同步填充：先写并 flush 4 KiB data block，再写并 flush 32-byte 索引
所在的 4 KiB metadata page，保证索引不会先于数据发布。非对齐 read 的首个
partial block 不填充；EOF 尾块补零后可填充。填充失败不影响已经成功的远端
读取，cache 满时也只退化为后续 miss，当前尚无 eviction。

所有可能改变或删除文件字节的操作都在 daemon mutation 之前同步清空该 inode
的全部索引（保守失效，尚未缩小到范围）：

- rewrite：保守失效该 inode 的全部 block；
- truncate：无论缩小或扩展，保守失效该 inode 的全部 block；
- unlink：失效该 inode 的全部 block；
- rename 覆盖：被覆盖目标 inode 的 extent 按 unlink 语义失效；普通 rename
  不改变 inode id，因此不应误删源 inode 的缓存。

每次 mutation 先推进内存 epoch，再逐条持久化清零索引，之后才释放 slot；任一
metadata BIO 失败都会让相应 VFS mutation 失败，避免权威数据已改变但旧索引仍
可能命中。miss token 记录 lookup 时的 epoch；若任何 mutation 在 READ_DATA
期间发生，返回数据不会再被发布为 cache fill。普通 rename 不改变源 inode
内容，因此只在覆盖已存在目标时失效目标 inode。

## 故障与安全原则

- cache 永远不是唯一数据副本；损坏或不可用时退化为 miss。
- cache 元数据不能被当作 MetaStore/ObjectStore 的权威状态。
- hit data BIO 失败退化为远端 miss；fill 失败忽略。初始化时 superblock/index
  恢复不确定则 fail closed，不让该设备以可疑索引继续加载。
- 失效写失败会拒绝 mutation；这是“宁可写失败、不可脏命中”的选择。
- 当前没有 eviction/LRU、数据 checksum、索引 journal/镜像或 torn-write 检测，
  也没有完整 DMA/零拷贝。
- v2 superblock 已持久化 namespace digest；缺失、非 64 位 hex 或 digest 不匹配
  均拒绝 cache_device 加载。内核无法验证部署层生成 descriptor 时是否规范化正确，
  因而 descriptor 规则仍是配置契约。
- 只观察本机 VFS mutation；其他节点或直接修改 Redis 的操作没有失效通知，
  所以 Step 20 仍是单 kernel/daemon correctness prototype，不可直接作为共享
  Redis+S3 多节点缓存部署。
- 当前格式没有 checksum 或双 superblock；任何已识别字段不匹配都会拒绝，
  但 reserved 区和未来索引项的损坏检测仍属于后续版本。
- exclusive holder 防止其他内核 holder 抢占设备，但不能在所有内核配置下阻止
  root 直接 raw write；设备隔离仍是部署要求。
