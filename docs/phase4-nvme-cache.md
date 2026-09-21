# Phase 4：内核拥有的 NVMe 缓存

## 当前边界（Step 18–54）

KestrelFS 的本地缓存由内核模块拥有。缓存命中时，内核直接把块设备中的
数据交给 VFS 调用者，不进入共享 ring，也不唤醒 Rust daemon。daemon 仍是
远端数据和元数据的权威控制面，只负责缓存未命中时通过现有 `READ_DATA`
路径取回数据，以及后续步骤中的填充协调和失效通知。

Step 18 落地模块参数和 read hook；Step 19 使用独占读写模式真正 claim 专用
块设备，校验 geometry，并格式化/复用最小 cache superblock。Step 20 在不改变
cache format v1 和 IPC ABI v11 的前提下启用盘上索引恢复、READ_DATA miss 后同步
fill、同步块读 hit，以及 rewrite/truncate/unlink/rename-overwrite 失效。该步的 hit
是普通 BIO 加 `copy_to_user()`；Step 22 在满足约束时绕过这次复制。

Step 21 要求每个 cache device 显式绑定一个 logical filesystem namespace。
namespace 不匹配时在恢复任何 index entry 前 fail closed，从而阻止 inode id 在
不同 FileMetaStore data-dir 或 Redis namespace 中重用造成的错误命中。IPC ABI
未改变，仍为 v11。

Step 22 在不改 IPC ABI 和盘上格式的前提下加速 hit：完整、块设备对齐且 cache
LBA 连续的 4 KiB block，会合并为最多 128 KiB 的同步 BIO，直接写入
`pin_user_pages_fast(FOLL_WRITE)` 固定的调用者页；不再先读入临时内核页再
`copy_to_user()`。非对齐 head/tail、用户地址不满足 logical/DMA alignment，或
pin/BIO 构造失败时，按 block 回退到 Step 20 buffered-copy 路径。miss/fill 和
daemon 路径没有变化。

Step 23 在 cache 满时按 4 KiB block 回收内存 LRU 头部，而不再停止 fill。运行期
成功 hit 或命中已有 entry 的 fill 会把 entry 移到 LRU 尾部；模块重载后用盘上
index 已有的 generation 恢复插入顺序，避免在热 hit 上增加持久化写。驱逐先清零
并 flush 旧盘上 index，随后才移除内存 key、覆写 data slot、发布新 index，保证
被回收 key 不会指向复用后的错误数据。

Step 24 把 cache format bump 到 v3，为每个完整 4 KiB data block 持久化 IEEE
CRC32，同时为每个非空 index entry 持久化独立 CRC32。buffered 和 pinned-page
两条 hit 路径都在返回成功前校验完整 data block。data CRC 不匹配时只退休对应
entry 并回退远端 miss，其他 entry 继续可用；加载时 index CRC 不匹配则拒绝整个
cache device，因为损坏的 key/slot 身份不能安全地局部猜测。IPC ABI 仍为 v11。
若退休坏 entry 的持久化清零失败，内存中立即停止命中，但该 slot 在本次模块加载
期间保持占用且不复用；这是用容量降级换取 crash/reload 后不会发生 key 别名。

Step 25 把 cache format bump 到 v4，并在 superblock 后增加一个 4 KiB intent
journal。fill、invalidate、evict 和损坏 entry 退休在修改 index 前先持久化
`PREPARED`；index 变更落盘后再清零 journal 作为提交点。重载看到合法
`PREPARED` 时不猜测操作完成度，而是清零其 slot 的 index 并清 journal，使对应
范围安全退化为 miss；非零但 CRC/字段无效的 journal 则拒绝加载。v4 同时给
superblock 增加 CRC32，防止 torn geometry/namespace 被当成有效格式。

Step 26 选择“并行同步 BIO”作为最小并发方案，而不是在本步引入异步 completion
对象：cache hit 在 pin 用户页、提交/等待 BIO、CRC 和 unpin 的完整生命周期持有
`kestrelfs_cache_lock` 读侧，多个 reader 因而能并发；fill、invalidate、evict、
journal 和损坏 entry 退休持有写侧。写侧取得锁之前会等待所有旧 reader 完成，
所以 entry/slot 不会在 DMA 期间释放或复用。并发 reader 的 LRU touch 与运行期
计数另由短时 spinlock 串行。CRC 失败的 reader 先记录 key+generation，释放读锁后
取得写锁，仅在 generation 仍相同时退休 entry，避免误删期间重填的新版本。该步
不改变 IPC ABI v11 或 cache format v4。

Step 48 在上述 rwsem 生命周期内，把 read hit 的 buffered
4 KiB BIO 与 pinned-page BIO 改为 `submit_bio`、独立 `end_io`/completion 和
按请求唤醒；`cache_async_hit_submissions`/`cache_async_hit_peak` 统计提交数
与峰值在途 BIO。CRC 仍在 completion 后校验，损坏条目按原 generation 检查退休；
写侧 invalidate/fill/evict 直到所有持读锁的 BIO 完成后才可复用 slot。Step 45
的冷 folio 先不持 bounce 锁查 cache，miss 进入 bounce 锁后重查并执行同步
`READ_DATA`/fill，允许同 inode 不同 folio 真正重叠提交 hit BIO。VFS
`read_folio` 仍同步等待自己的 completion；metadata/index/journal/fill 写入仍用
同步等待，未提供用户态异步接口或单请求多 BIO 流水线。IPC ABI v21、format v4
未变；验证见 `test-step48-cache-async-vng.sh`。

Step 49（已验收）让普通 write/writev 经 VFS folio 的
`write_begin`/`write_end` 标 dirty；注册可写回 BDI 后，`writepages` 对每个
folio 先失效 NVMe read index，再通过已有 16 KiB bounce `WRITE_DATA` 提交
权威 daemon。`write_iter` 在返回前等待 writeback，成功后保留 clean filemap
页，因此是最小 write-through aops，不是异步/延迟写缓存。`fsync` 先等待脏页
写回，再调用 daemon 对象/元数据屏障；close `.flush` 也等待写回但不是
fsync。失败时 redirty folio、记录 mapping/superblock 错误并返回错误，
不宣称仅内存脏页已经持久。写入 dirtying 前后与实际提交前都按 inode 保守
失效 NVMe 读索引；truncate 先写回再更改 size。Step 49 本身仍拒绝可写
`MAP_SHARED`。
IPC ABI v21、cache format v4 不变；测试见 `test-step49-cache-write-vng.sh`。

Step 50 在相同 write-through aops 上启用可写 `MAP_SHARED`。共享 VMA 的
`page_mkwrite` 在 filemap 页首次变脏前检查挂载 page-cache epoch；若 Redis
revision 或本地 mutation 已令映射过期，则先调度既有 mapping 清理并重试，清理
失败时以 `SIGBUS` fail closed。共享脏 folio 继续由 `writepages` 失效 NVMe 读索引，
再经 `WRITE_DATA` 同步写入权威 daemon；`msync`/`fsync` 使用标准 filemap 写回，
writable VMA 的 close 也同步等待映射范围，防止普通 `munmap` 静默遗留脏页。
后者没有向 `munmap` 返回 errno 的 VFS 通道，因此失败会保留 mapping errseq/脏页，
由后续 `fsync`/`flush` 暴露，而不会标记为已持久。IPC ABI 因同一步的 whiteout
inode 类型变为 v22；缓存磁盘布局不变，仍为 format v4。测试见
`test-step50-vng.sh` 与 `test-step50-map-shared.c`。

Step 51 将普通 buffered write 从 write-through 改为 write-behind：
`write_iter` 在 folio 成功标脏后不再主动 `filemap_write_and_wait()`，由内核 BDI
flusher、内存回收或显式同步路径调用既有 `writepages` → `WRITE_DATA`。因此普通
异步 write 可以在 daemon 暂时离线时先从 page cache 返回；这不代表数据已持久化。
`O_SYNC`/`O_DSYNC` 仍经 `generic_write_sync()` 等待；`fsync`/`fdatasync`、
`msync(MS_SYNC)`、writable VMA close、truncate 与 close `.flush` 也会同步写回。
`syncfs` 由 VFS 先写回该 superblock 的 dirty mapping，再由 `sync_fs(wait=1)` 发
`OP_SYNC_FS`。WRITE_DATA 失败会 redirty folio 并记录 mapping/superblock errseq，
由同步调用或后续同步点报告；NVMe read-cache 的写前/提交前失效及 page-cache epoch
仍保持顺序：远端 epoch worker 先撤销 writable PTE、写回 dirty folio，成功后才
退休旧 page-cache 页；写回失败保留 dirty 页并让 read/SIGBUS fail closed，避免
静默丢弃本地延迟写。这里复用内核通用 flusher，并未增加异步 WRITE_DATA opcode
或自有线程。
IPC ABI 仍为 v22，cache format 仍为 v4；验证见
`test-step51-write-behind-vng.sh`。

Step 52 没有改变数据路径或一致性协议，而是用两个并行 vng guest 验证现有最小
多节点闭环。两个节点各有独立 daemon、挂载、page cache 与 loop cache，共享同一
Redis MetaStore 和 S3 ObjectStore。A 的 write+fsync 先完成对象写入和 Redis
revision-CAS；B 的约 100 ms probe 消费 durable dirty-inode log，推进 page-cache
epoch 并退休对应 NVMe entry，随后从共享 ObjectStore 读取新 slice 并重新 fill。
B 停止 daemon 后仍能从新 entry 命中，证明旧 entry 没有越过失效边界。该验证不把
轮询窗口包装成线性一致：提交到下一次 probe 前仍可能读到旧页；probe 失败或历史
不完整时继续全量 fail closed。IPC ABI v22、cache format v4 均未改变。粗测方法与
环境见 `perf-baseline.md`。

Step 54 不改 cache format。普通文件新增 filemap splice-read 与 iter splice-write，
所以 file→pipe、pipe→file 和 sendfile 继续经过同一 page cache、write-behind、
fsync 与 coherence epoch 规则。writepages 在取得全局 bounce mutex 前把锁定 folio
复制到私有 staging；不同 inode/folio 的准备阶段可重叠，mutex 内只保留 cache
ordering、bounce memcpy 与同步 WRITE_DATA。它缩短临界区但没有让单一 bounce 上的
WRITE_DATA 并发，亦未增加异步 opcode。新增 orphan retry peek/ack ioctl 使 IPC ABI
bump 到 v23；共享内存/event/opcode 和 cache format v4 均未变化。

Step 27 增加独立用户态 `kestrelfs-cache-admin`，不改模块正常加载路径。`inspect`
以只读方式解析 v4 superblock、journal 和完整 index 区；块设备还会请求 exclusive
open，避免模块持有期间读取不一致快照。它报告 namespace、geometry、
entry 数、CRC/layout/重复 key 统计，以及 clean / recovery-required / invalid /
unformatted 状态。`wipe` 是显式离线恢复动作：仅接受块设备，使用 exclusive open，
且必须同时传 `--yes-really-wipe` 并令 `KESTRELFS_CACHE_WIPE_CONFIRM` 精确等于设备
参数；它只清零并 fsync 前 2 MiB cache metadata，使旧 data slot 不再可寻址，但
不承诺安全擦除 data 区。IPC ABI 仍为 v11，cache format 仍为 v4。

Step 28 把动态 regular file 的读入口从 `.read` 切到 `.read_iter`，让普通
read/pread 与 readv/preadv 共享一条结构化 `iov_iter` 路径。cache hit 只在完整
4 KiB block 位于当前单个用户 iovec 段、且地址满足既有 DMA/logical alignment 时
直接 pin 该段用户页；连续 cache LBA 仍可在段内合并到 128 KiB。跨 iovec 边界、
partial/unaligned、kernel-backed iter 或 pin/BIO 构造失败时，读取完整 cache block
并经 `copy_to_iter` 分发。READ_DATA miss 也用 `copy_to_iter` 写入同一 iterator。
IPC ABI 仍为 v11，cache format 仍为 v4。

Step 29 在 format v4 journal 的保留区加入向后安全的 batch-evict 扩展：cache
首次满盘时默认选择 16 个 LRU 头（且单批最多为总槽位的 1/16），一份 PREPARED
journal 记录整批 slot，再把同一 4 KiB index page 上的多个清零合并成一次
read/modify/write/flush。journal 提交后才从内存 hash/LRU/bitmap 释放槽位，随后一次
fill 使用其中一个，其余供连续 fill 直接使用。LRU 尾部的近期热点不会进入小批量
victim 集合；rwsem 写侧仍等待 pinned-page reader 完成。IPC ABI v11、superblock、
index entry 和 cache format v4 均未改变。

Step 35 选择 daemon 驱动的最小远端 coherence 闭环。Redis v2 的每次 Lua
metadata mutation 都会原子递增 durable `control.revision`；daemon 启动时先请求
内核全失效恢复的本地索引，之后每 100 ms 探测 revision。revision 变化或探测失败
时，daemon 经 `KESTRELFS_IOC_INVALIDATE_CACHE_ALL` 请求内核在 cache rwsem 写侧、
用既有 v4 invalidate journal 逐条退休全部 entry。若持久化退休失败，内核销毁
所有内存索引并禁用该模块生命周期的 hit/fill，宁可 miss 而不返回可能过期的数据。
共享内存/opcode/payload 与 cache format v4 不变；新增 daemon→kernel ioctl 使 IPC
ABI v13 bump 到 v14。这个方案用粗粒度和轮询延迟换取不依赖易丢事件的 durable
检测，不是生产级 lease 或按 inode/range pub/sub。

Step 42 在上述全量回退之上增加 durable 细粒度路径。Redis v2 每次成功 Lua mutation
除 revision-CAS 和字段 diff 外，还在 `<prefix>:meta:v2:dirty` HASH 中以新 revision
为 field 原子写入排序去重后的 inode 列表。每条最多 64 inode，并只保留最近 256 个
revision；这是一份多 reader 可独立消费的 revision log，不是由首个 daemon 全局清空
的集合，因此多个 daemon 不会互相丢失通知。各 daemon 的成功 probe 游标等价于清空
自己的待处理集合。probe 合并 `(observed,current]`：历史完整且累计不超过 64 时经 ABI
v20 `KESTRELFS_IOC_INVALIDATE_CACHE_INODES` 在一次 cache rwsem 写侧临界区退休相关
inode；任一记录缺失/损坏、单记录 overflow、revision gap 超过 256、累计超过 64 或
Redis probe 失败，都保留 ABI v14 的 invalidate-all 路径 fail closed。远端提交到
100 ms probe 前的最终一致窗口仍存在；没有 lease/pubsub 或 range 级消息。共享内存、
opcode 与 cache format v4 均未改变。

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

模块输入参数、只读观测计数、namespace descriptor 规则与安全默认值统一维护在
[`configuration.md`](configuration.md)。本设计文档不再复制参数表，避免默认值和
新增计数发生漂移。

## Step 19–27 磁盘格式

所有多字节字段采用 little-endian，LBA 固定表示 512 字节 sector。磁盘头占
第一个 4 KiB：

| Offset | 字段 | 类型/含义 |
|---:|---|---|
| 0 | magic | u64，字节串 `KFSCACHE` |
| 8 | version | u32，当前为 4 |
| 12 | header_size | u32，4096 |
| 16 | logical_block_size | u32，格式化时设备值 |
| 20 | physical_block_size | u32，格式化时设备值 |
| 24 | cache_block_size | u32，4096 |
| 28 | index_entry_size | u32，32 |
| 32 | usable_sectors | u64，`cache_size_mib` 生效后的容量 |
| 40 | index_start_lba | u64，当前为 16 |
| 48 | index_capacity | u64，当前为 65280 |
| 56 | data_start_lba | u64，当前为 4096（2 MiB） |
| 64 | format_generation | u64，格式化时生成且非零 |
| 72 | namespace_id | 32 字节 SHA-256 digest |
| 104 | journal_start_lba | u64，当前为 8 |
| 112 | journal_size | u32，4096 |
| 116 | feature_flags | u32，当前必须为 0 |
| 120 | reserved | 必须全零，补齐至 offset 4092 |
| 4092 | super_crc32 | u32，覆盖前 4092 字节 |

LBA 8 到 15 是单页 transaction journal，LBA 16 到 4095 是盘上索引区。v4
沿用每个 32 字节条目：
`inode_id:u64 + file_offset:u64 + generation:u64 + data_crc32:u32 +
entry_crc32:u32` 组成；entry CRC 覆盖其前 28 字节，全零条目表示空闲。没有重复
持久化 LBA：索引 slot 与 data LBA 固定一一映射，
`lba = data_start_lba + slot * 8`。这样在增加两个 checksum 后仍保持 32 字节条目
和 65280 个索引容量。有效 slot 数是索引容量与实际 4 KiB data block 数量的
较小者，因此小设备不会产生越界 LBA。

journal 页布局也是 little-endian：`magic:u64("KFSJOURN") + version:u32(1) +
state:u32(PREPARED) + sequence:u64 + operation:u32 + slot:u32 + old_entry:32B +
new_entry:32B + reserved + crc32:u32`。CRC 覆盖前 4092 字节。一般事务的 reserved
必须全零；Step 29 的多 victim evict 可在其前部编码
`magic:u32("BTCH") + count:u32 + slots[count]:u32[]`，未使用部分仍必须全零，count
范围 2–64 且 slot 不得重复；单 victim evict 继续使用全零 reserved。
fill 要求 old 为空/new 有效，invalidate、evict、retire-corrupt 要求 old 有效/new
为空。全零页表示当前无事务。单页足够是因为所有 cache metadata mutation 都取得
`kestrelfs_cache_lock` 写侧，任一时刻最多一个事务；并行 hit 只持有读侧，不能修改
journal 或盘上 index。

Step 21 将 cache format 从 v1 bump 到 v2；v1 设备不会自动迁移或清空。自动格式化
只发生在整个 2 MiB 保留元数据区全零时，并只写入 4 KiB
superblock 后 flush。非零但 magic/version/任一 geometry 不匹配时 fail
closed，模块加载返回错误，不覆盖已有内容。用不同 `cache_size_mib` 重新加载
已格式化设备也视为 geometry mismatch；v2 namespace digest 不匹配会输出独立的
`cache namespace identity mismatch` 并拒绝加载。当前没有 `wipe` 模块参数或自动
清空路径；重用旧 v1 或其他 namespace 的设备只能在卸载模块后，由运维用 Step 27
工具双确认显式清空 metadata，避免误操作。
Step 24 再从 v2 bump 到 v3；v1/v2 都默认拒绝，不自动 wipe、迁移或重算 checksum。
Step 25 从 v3 bump 到 v4；v1/v2/v3 均默认拒绝，不自动 wipe、迁移或补写 journal。

## 命中和未命中路径

```text
VFS read/readv -> read_iter
  -> kestrelfs_cache_read_iter(inode, iov_iter, file_offset)
       -> 取得 cache rwsem 读侧（不同 reader 可并行）
       -> aligned single-iovec hit: pin 用户页 -> 连续块合并 BIO -> 用户页 -> 逐块 CRC
       -> cross-iovec/partial/fallback: 同步 4 KiB BIO -> 内核页 -> CRC -> copy_to_iter
       -> miss: 返回 -ENODATA
  -> 取得 kestrelfs_data_ipc_lock
  -> READ_DATA + 16 KiB bounce + req/resp ring
  -> daemon 从 ObjectStore 读取
  -> 内核同步 fill 完整的 4 KiB 对齐块
```

hook 位于 `kestrelfs_data_ipc_lock` 之前，因此 hit 不占 bounce buffer，也不会与
单 in-flight data/name IPC 串行化。miss 沿用既有 `READ_DATA` 编码（当前整体协议为
ABI v14），共享内存布局和同步模型均不改变。只有请求的整个 EOF-clamped 范围都
有索引时才按 hit 返回；否则整次请求安全回退到 READ_DATA，避免把部分结果暴露给
调用者。

直达路径只处理 read 范围完整覆盖、且完全位于当前单个用户 iovec 段的 cache block，
绝不把 4 KiB BIO 指向只允许修改其中一部分或属于下一段的用户区间。连续的 file
block 还必须映射到连续 cache LBA 才能合并；每个 BIO 上限 128 KiB，以限制 GUP
页数和 BIO vector 资源。用户地址不满足块设备 logical sector 与 DMA mask 时自动
使用 buffered fallback。提交过 BIO 的
用户页在完成后按 dirty unpin，包括设备报告错误的情形，因为失败 BIO 也可能已经
修改部分页；之后 buffered path 会覆盖请求区间，若 cache 设备仍失败则回远端 miss。
pinned-page 路径在页仍固定时对每个 4 KiB block 计算 CRC；若失败，系统调用不会
把这批字节作为成功结果返回，条目先被退休，再由同一次 READ_DATA miss 覆盖完整
请求区间。buffered 路径则在 `copy_to_iter()` 前完成校验，因此坏字节从不复制给
调用者。

## 索引模型

索引键为 `(inode_id, 4 KiB-aligned file_offset)`，内存值为
`LBA + generation + slot + data_crc32`。模块加载时按 4 KiB 页扫描 2 MiB 元数据区，
先校验每个非空条目的 entry CRC，再检查 offset 对齐、slot 映射和非零 generation，
然后恢复到 `rhashtable` 和 slot bitmap。重复 key、半空条目、坏 CRC 或越界映射
均使模块 fail closed；不会猜测或覆盖可疑格式。

generation 在当前版本中用于持久化条目版本和恢复后继续单调分配；同一模块
生命周期内另有全局 mutation epoch 防止 miss 与并发 mutation 竞态发布旧 fill。
Step 23 还用 generation 在重载时把 LRU list 排成最老插入在前；hit 只更新内存
list，不同步写 generation，因此重启会丢失精确访问热度并退化为 insertion-order
近似。generation 仍不是跨主机一致性协议或数据校验码。

## 填充、失效与顺序

miss 时 daemon 仍通过 `READ_DATA` 把远端数据放入 bounce。fill 对 EOF 尾块补零，
计算完整 4 KiB data CRC，然后按 `PREPARED journal → data block → index entry →
zero journal` 的顺序逐步写入并 flush；journal 清零成功是持久化提交点，之后才把
entry 发布到内存 bitmap/LRU。非对齐 read 的首个 partial block 不填充；EOF
尾块补零后可填充。填充失败不影响已经成功的远端读取。cache 满时 Step 29 批量
选择 LRU 头部，并执行 `batch PREPARED journal → 按 index page 合并 zero entries →
zero journal`；成功后才从 rhashtable/list/bitmap 移除整批并允许 slot 复用。随后的
replacement fill 是独立事务。合法的半提交 batch journal 会在重载时再次清空整批
slot；若某个 index page torn 并波及无关 entry，其 entry CRC 会使设备 fail closed。
任一步骤失败最多保留旧 entry、留下未索引空槽或在恢复时产生 miss，不会恢复旧
key 后读取新 key 的字节。

所有可能改变或删除文件字节的操作都在 daemon mutation 之前同步清空该 inode
的全部索引（保守失效，尚未缩小到范围）：

- rewrite：保守失效该 inode 的全部 block；
- truncate：无论缩小或扩展，保守失效该 inode 的全部 block；
- unlink：失效该 inode 的全部 block；
- rename 覆盖：被覆盖目标 inode 的 extent 按 unlink 语义失效；普通 rename
  不改变 inode id，因此不应误删源 inode 的缓存。

每次 mutation 先推进内存 epoch，再逐条用 invalidate journal 持久化清零索引，
journal 清零提交后才释放 slot；任一 metadata BIO 失败都会让相应 VFS mutation
失败，避免权威数据已改变但旧索引仍
可能命中。miss token 记录 lookup 时的 epoch；若任何 mutation 在 READ_DATA
期间发生，返回数据不会再被发布为 cache fill。普通 rename 不改变源 inode
内容，因此只在覆盖已存在目标时失效目标 inode。

Redis 共享 MetaStore 还有一条 daemon→kernel 远端失效路径：daemon 启动时先全
失效；运行中看到 `control.revision` 与上次观测值不同，或无法确定当前 revision，
即发 `INVALIDATE_CACHE_ALL`。该操作先推进同一 mutation epoch，阻止并发 miss 返回
的旧数据再次发布，再按 v4 journal 提交每个索引清零。全失效与本机 inode 失效、
fill、evict 共享 rwsem 写侧，所以 pinned-page hit 要么在失效前完整结束，要么在
失效后 miss。FileMetaStore/MemStore 没有共享 revision，不启用这一路径。

`kestrelfs_cache_lock` 是 rwsem。hit 的读侧覆盖 index 检查、用户页 pin、整个 BIO
completion、CRC 和 unpin；invalidate/fill/evict/journal mutation 必须取得写侧。因此正在
进行的 hit 要么在 mutation 前完整读到旧版本，要么在失效完成后看不到条目；slot
不会在 DMA 期间被驱逐、释放或复用。LRU touch 和 reader/counter 更新用独立短时
spinlock 保护，使读侧持锁的多个 BIO 能实际重叠。Step 48 的 hit
`end_io`/completion 让请求独立提交与唤醒，但 VFS 读仍等待自己的结果；代价是
mutation 会等待慢 reader，也没有 per-entry refcount/RCU。

## 故障与安全原则

- cache 永远不是唯一数据副本；损坏或不可用时退化为 miss。
- cache 元数据不能被当作 MetaStore/ObjectStore 的权威状态。
- hit data BIO 失败退化为远端 miss；fill 失败忽略。初始化时 superblock/index
  恢复不确定则 fail closed，不让该设备以可疑索引继续加载。
- 失效写失败会拒绝 mutation；这是“宁可写失败、不可脏命中”的选择。
- Step 29 是 block 粒度、单锁内存 LRU 的小批量回收；重启后仍只恢复 generation
  insertion order，不持久化精确 hit recency，也没有分区配额或租户级热点隔离。
  batch 可减少连续满盘 fill 的 eviction transaction 与 index-page flush 数，但 fill
  自身及 invalidate 仍各自使用同步 journal；victim 分散到多个 index page 时每页
  仍需一次同步写。
- v4 的逐 data block CRC 可局部退休坏数据，index entry CRC 在恢复时 fail closed；
  superblock 和单页 journal 也有 CRC。合法 `PREPARED` 总是恢复为清空 slot 的
  安全 miss；torn journal/superblock 拒绝设备。CRC32 不是密码学完整性保护，仍
  存在碰撞概率；当前没有双 superblock 或 metadata 镜像，单 journal 还会给每次
  metadata mutation 增加两次同步写/flush。
- Step 22/26 是 read hit 的受限少拷贝并行路径：完整、对齐、连续块可以直达用户
  页，Step 48 改为多个调用者异步提交 BIO、分别等待 completion；Step 28 通过 `read_iter` 覆盖
  read/pread/readv/preadv，但跨 iovec、partial block 和不能 pin/对齐的 buffer 仍有
  一次 `copy_to_iter()`。Step 45 的普通读已改经 page-cache/readahead；Step 54
  覆盖普通文件的 file→pipe、pipe→file 与 sendfile，但没有跨 iovec scatter-gather
  BIO、用户态异步接口或单次请求内的多 BIO pipeline。
- v4 superblock 沿用持久化 namespace digest；缺失、非 64 位 hex 或 digest 不匹配
  均拒绝 cache_device 加载。内核无法验证部署层生成 descriptor 时是否规范化正确，
  因而 descriptor 规则仍是配置契约。
- Step 42 能以有界 durable dirty log 将正常 Redis mutation 收窄到 inode 批量失效，
  但仍每 100 ms 轮询：远端提交到下一次 probe 前可能读到旧 hit，daemon 离线期间
  也没有 lease/通知，因此还不是线性一致的共享 Redis+S3 多节点缓存。历史缺失或
  超限会全量失效；尚无 range 级消息或生产级 pub/sub/reconnect 运维。
- v1/v2/v3 cache 默认拒绝且不自动迁移；没有自动或模块参数 wipe。Step 27 仅提供
  离线、块设备专用、双确认的 metadata wipe。
- exclusive holder 防止其他内核 holder 抢占设备，但不能在所有内核配置下阻止
  root 直接 raw write；设备隔离仍是部署要求。

## Step 22 vng 验证与粗测

`test-step22-cache-vng.sh` 只在 vng guest 内创建 loop、加载模块和挂载，daemon
使用按 PID 隔离的 data-dir 并写 `daemon.log`。辅助程序用 4 KiB 对齐用户 buffer
验证完整块直达，也用 `file_offset=1`、`user_shift=1` 和越过 EOF 的请求覆盖非对齐
head/tail 与 EOF clamp；随后停 daemon 再读，证明 hit 不经过 IPC。

脚本在同一当前格式 cache、相同 namespace 和同一 1 MiB × 64 次 `pread()` workload 上，
分别以 `cache_direct_io=0/1` 重载模块。2026-09-14 的 TCG vng 粗测为：buffered
copy 3.80 s（16.84 MiB/s），pinned-page direct 0.73 s（87.34 MiB/s），约 5.2×。
这是 loop + TCG 下的路径级对比，不代表真实 NVMe 性能；辅助程序的数据校验成本
也包含在两组数字中。验收看明确 PASS、命中计数和数据一致性，不把固定倍数作为
门槛。

## Step 23 vng 满盘回收验证

`test-step23-eviction-vng.sh` 在 vng guest 内用 `cache_size_mib=3` 创建只有 256 个
data slot 的 loop cache。它先用 1 MiB 文件 A 填满全部 slot，再命中 A[0] 将其提升
为 MRU，随后以 128 KiB 文件 B 触发恰好 32 次回收。停 daemon 后验证 B、A[0] 和
A 尾块仍命中，而被驱逐的 A[1] 必须读取失败；rmmod/insmod 后重复边界断言，并
确认恢复 256 个 entry。

2026-09-14 在 v4 工作树格式上复跑输出 `cache_evictions=32`、
`restored 256 cache index entries`、`STEP23_EVICTION_PASS`，umount 68 ms。脚本自身
包含 `insmod`、合法 64-hex namespace、PID 隔离的 data-dir 和 daemon.log；只允许
通过 vng+loop 执行。

## Step 24 vng 数据完整性验证

`test-step24-checksum-vng.sh` 在 vng guest 的 16 MiB loop 上填充两个独立 entry，
分别 raw 修改 slot 0 和 slot 1 的 data byte。它覆盖 pinned-page 完整块以及
`file_offset=1/user_shift=1` 的 buffered 非对齐读取：daemon 停止时坏 entry 必须
miss、未损坏 entry 仍必须命中；daemon 恢复后坏范围从 ObjectStore 重填，随后
再次停 daemon 仍可命中。两条路径都断言 `cache_checksum_failures=1`。

脚本随后修改 slot 0 的 `file_offset` 而不更新 entry CRC，要求模块加载在恢复索引
时失败；最后把 superblock version 改成 v2，要求无迁移拒绝。2026-09-14 最终
自检输出 `STEP24_CHECKSUM_PASS`，最后一次 umount 53 ms。所有 load 均含 `insmod`、
合法 namespace、PID 隔离 data-dir 和保留的 daemon.log，仅通过 vng+loop 执行。

## Step 25 vng 崩溃恢复验证

`test-step25-cache-txn-vng.sh` 只在 vng guest 中建立 loop 设备并加载模块。辅助程序
`test-step25-cache-txn.c` 离线构造与内核完全相同的 v4 `PREPARED` journal 和
index 状态，分别模拟 fill 在 index 已落盘后、invalidate 在 index 清零前、evict
在 index 清零后的崩溃。重载后均断言 `cache_journal_recoveries=1`，目标 slot 被
清空并 miss，未涉及 entry 仍可在 daemon 停止时命中；daemon 恢复后 miss 可正常
重填。

脚本还故意破坏 journal reserved byte、把 version 改回 v3、破坏 superblock
reserved byte而不更新 CRC，三者必须 fail closed；恢复合法 superblock 后仍可
加载。2026-09-14 自检依次输出 `STEP25_FILL_RECOVERY_PASS`、
`STEP25_INVALIDATE_RECOVERY_PASS`、`STEP25_EVICT_RECOVERY_PASS`、
`STEP25_TORN_FAIL_CLOSED_PASS`、`STEP25_V3_REJECT_PASS`、
`STEP25_SUPER_CHECKSUM_PASS` 和 `STEP25_CACHE_TXN_PASS`，最终复跑 umount 62 ms。

## Step 26 vng 并发 hit 与失效验证

`test-step26-cache-async-vng.sh` 只在 vng guest 的 loop cache 上运行。它用同一 build、
同一持久 cache 和相同 1 MiB × 16 次 × 8 reader workload，先以
`cache_parallel_reads=0` 验证串行峰值为 1，再以默认并行模式验证峰值至少为 2。
随后两个 reader 同时进入 pinned-page hit 区间，rewrite 在写侧等待；旧 reader
必须得到完整旧版本，rewrite 返回后新读必须得到完整新版本，停 daemon 后仍能命中。
脚本还检查 dmesg 不含 BUG/KASAN/UAF/general-protection/hung-task 诊断。

2026-09-15 TCG vng 粗测为：串行 877,968,083 ns，并行 430,707,430 ns，约 2.04×；
峰值分别为 1 和 8。数字包含辅助程序逐字节校验且受 TCG/loop 调度影响，只证明路径
可重现地并行，不代表真实 NVMe 上限。自检输出 `STEP26_PARALLEL_HIT_PASS`、
`STEP26_CONCURRENT_INVALIDATE_PASS`、`STEP26_CACHE_ASYNC_PASS`，umount 53 ms。

## Step 27 离线诊断与显式恢复

构建和只读检查：

```bash
make -C tools
./tools/kestrelfs-cache-admin inspect /dev/loop0
```

`inspect` 可读块设备或离线 image；对干净 v4 返回 0，对未格式化的全零 metadata
也返回 0，对结构/CRC 损坏返回 2，对合法 PREPARED journal 返回 3 并输出
`overall_status=recovery-required`。它校验 super/journal/index entry CRC、固定 geometry、
slot 边界和重复 `(inode_id,file_offset)`；为保持检查有界，不扫描 data 区，明确输出
`data_crc=not-scanned`。块设备以只读 exclusive open 检查，仍被模块 claim 时会失败；
普通文件只用于检查离线 image/snapshot，不能作为 cache device 或 wipe 目标。

wipe 必须同时给出两个独立且目标一致的确认：

```bash
KESTRELFS_CACHE_WIPE_CONFIRM=/dev/loop0 \
  ./tools/kestrelfs-cache-admin wipe /dev/loop0 --yes-really-wipe
```

缺环境变量、值与参数不一致、缺旗标、目标是普通文件，或设备仍被占用时均拒绝。
wipe 只清前 2 MiB metadata：这是“丢弃 cache 索引并允许内核重新 format”，不是整盘
安全擦除；MetaStore/ObjectStore 权威数据不受影响。

`test-step27-ops-recovery-vng.sh` 在 vng guest+loop 中填充一个真实 entry，检查 clean
v4 摘要和合法 PREPARED journal，再损坏 journal 验证 CRC 诊断。脚本覆盖两种缺确认
拒绝、普通文件拒绝、显式 wipe 后 unformatted、重新加载 format、daemon 停止时旧
entry 不可命中、恢复 daemon 后重填以及再次停 daemon 命中。2026-09-15 自检输出
`STEP27_INSPECT_PASS`、`STEP27_WIPE_GUARD_PASS`、`STEP27_WIPE_PASS`、
`STEP27_REFILL_PASS`、`STEP27_OPS_RECOVERY_PASS`，最终复跑 umount 50 ms。

## Step 28 read_iter / iov_iter 验证

`test-step28-cache-vfs-vng.sh` 在 vng guest+loop 中显式加载模块，并用独立 data-dir
保留 daemon.log。辅助程序 `test-step28-cache-vfs.c` 经真实 `preadv()` 构造：

- 两个页对齐 iovec，其中第二段跨两个用户页，确认三个 block 走 pinned BIO；
- file offset、用户地址与 iovec 边界均非对齐的三段读，确认安全 buffered fallback；
- 跨 EOF 的三段读，确认返回正确短读，且每段未返回区域保持 sentinel；
- 每个 iovec 前后各 64-byte guard，确认 direct/fallback 都没有段外覆盖。

warm 填充后脚本停止 daemon，再执行上述全部 vectored reads，因此成功只能来自本地
cache。2026-09-15 自检输出 `STEP28_IOVEC_PASS`、`STEP28_UNALIGNED_PASS`、
`STEP28_EOF_PASS`、`STEP28_DAEMON_FREE_HIT_PASS direct_delta=3 copy_delta=5`、
`STEP28_CACHE_VFS_PASS`，umount 83 ms。Step 27/26/25/24/23/22/21/20/19/15
回归均通过；所有 cache/mount 操作只在 vng guest+loop 执行。

## Step 29 批量驱逐验证

`test-step29-cache-evict-vng.sh` 在 vng guest+loop 中以 `cache_size_mib=3` 建立
256 个 data slot，并显式传 `cache_evict_batch=16`。脚本用 1 MiB 文件 A 填满
cache，命中 A[0] 将其移到 MRU 尾部，再读取 16-block 文件 B。它要求一次 batch
事务退休 16 个 victim，且这些连续 slot 的清零合并为一次 index-page 写；对应
sysfs 断言为 `cache_evictions=16`、`cache_eviction_batches=1`、
`cache_eviction_batch_slots=16`、`cache_eviction_index_writes=1`。

随后脚本停止 daemon，验证 B 与 MRU A[0]/A 尾块仍命中、旧 LRU A[1] 已 miss；
rmmod/insmod 后再次停止 daemon，重复新数据 hit 与旧 victim miss，并确认恢复
256 个 entry。脚本再离线构造 16-victim PREPARED journal、仅清 8 个 index，要求
admin 报告 `journal_batch_count=16` / recovery-required；模块恢复整批为 miss，未涉及
热点仍命中。2026-09-15 工作树自检输出 `STEP29_BATCH_EVICTION_PASS`、
`STEP29_MRU_PROTECTION_PASS`、`STEP29_RELOAD_PASS`、
`STEP29_BATCH_RECOVERY_PASS`、`STEP29_CACHE_EVICT_PASS`，umount 253 ms。全部模块、挂载和 cache_device 操作只在
vng guest 内执行；脚本显式 insmod、使用 loop、合法 namespace、独立 data-dir，
并保留 daemon.log。

## Step 35 远端 coherence 验证

`test-step35-cache-coherence-vng.sh` 需要 `REDIS_URL` 指向一次性测试数据库；脚本用
随机 prefix，不把 URL 或凭据写入仓库。所有模块、mount 和 cache-device 操作仍只
在 vng guest 进行：创建 PID 隔离的 loop image/data-dir，显式 `insmod` 并传合法
namespace，daemon 输出保留到 `daemon.log`。

脚本先经本机 VFS 写入 4 KiB `A`，等待本次 revision 被 poll 后连续读取，要求
cache hit 计数上升。随后模拟另一控制面 writer：先在共享 LocalFs ObjectStore 写入
新 immutable block，再用 Lua CAS 原子替换 Redis v2 inode/slice 字段并递增
revision。reader daemon 必须观察新 revision，`cache_coherence_invalidations` 上升，
旧 `A` entry 不得继续命中；第一次读取应从新 slice 得到 `B` 并 fill，第二次命中
`B`。最后停止 daemon 再读仍得到 `B`，证明新填充的 kernel cache 不依赖 IPC。

2026-09-15 Codex vng 自检输出：

```text
STEP35_READER_CACHE_HIT_PASS hits_delta=1
STEP35_REMOTE_REVISION_INVALIDATE_PASS
STEP35_DAEMON_FREE_NEW_HIT_PASS
STEP35_CACHE_COHERENCE: umount_ms=23
STEP35_CACHE_COHERENCE_PASS
```

同一工作树还复跑 `STEP25_CACHE_TXN_PASS`、`STEP21_NAMESPACE_PASS`、
`STEP20_CACHE_PASS` 和 `STEP34_POSIX_ATTR_PASS`。本轮提供的远端 Redis 凭据返回
`WRONGPASS`，因此测试临时使用退出即删除、无持久卷的 Redis 7.4.11 容器；guest
经 QEMU user-network host gateway 连接，容器在测试后停止删除。

## Step 52 两节点闭环与性能基线

`test-step52-dist-vng.sh` 并行启动两个 vng guest；guest 脚本显式 `insmod`，只使用
独立 loop cache，并把 daemon 输出保存在各自 PID 隔离 data-dir 的 `daemon.log`。
2026-09-20 的共享 Redis+MinIO 自检输出：

```text
STEP52_DIST_A_FSYNC_PASS
STEP52_DIST_VISIBILITY_LATENCY_MS=10
STEP52_DIST_REMOTE_VISIBLE_PASS
STEP52_DIST_DAEMON_FREE_NEW_HIT_PASS
STEP52_DIST_TWO_NODE_PASS
```

`test-step52-perf-vng.sh` 在单 guest 中分别测 write-behind 返回、page-cache 热读与
drop_caches 后 daemon-free cache hit，并输出 `STEP52_PERF_*_PASS`。完整 workload、
复现命令、一次通过样本和非 SLA 边界见 [`perf-baseline.md`](perf-baseline.md)。

## Step 54 splice、orphan retry 与写回预暂存验证

`test-step54-vfs-pipe-vng.sh` 在单个 vng guest + loop 中显式 insmod，使用共享 Redis
metadata 与本地对象目录。它逐字节校验 file→pipe、pipe→file 和 sendfile；再写入
2 MiB 并要求 `write_pipe_staged_bytes` 增长至少 2 MiB、WRITE_DATA submission 增长。
orphan 负例保持 fd 打开两秒，确认对象不被清扫且 fd 仍可读；正例暂停 daemon，令
final-close IPC 超时并由内核排队，恢复 daemon 后要求 periodic replay 与对象删除。
2026-09-21 自检输出 `STEP54_VFS_PIPE_PASS`，本轮 512 submissions、umount 163 ms。

`test-step54-lease-vng.sh` 复用两个独立 vng guest + Redis/S3 配方。正常 session
心跳下 Step 53 Pub/Sub/dirty revision 可见性仍通过；随后删除 B 的精确 session key，
B 的下一次 create 必须失败且路径不存在。自检输出
`STEP54_LEASE_HEARTBEAT_NOTIFY_PASS`、`STEP54_LEASE_EXPIRED_FAIL_CLOSED_PASS` 和
`STEP54_LEASE_PASS`，本轮远端可见延迟 11 ms。
