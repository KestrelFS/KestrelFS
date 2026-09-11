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

**A high-performance, cloud-native distributed filesystem — built with a pragmatic C + Rust hybrid architecture, engineered to outperform JuiceFS.**

> ⚠️ **Project status: early development (Phase 1/4 + Phase 2 in progress).**
> The current codebase implements a minimal read-only kernel VFS skeleton and
> the foundational kernel↔userspace IPC bridge. It is **not yet a usable
> filesystem** — see [Roadmap](#roadmap) for what exists today versus what's
> planned.

---

## Table of Contents

- [Vision](#vision)
- [Architecture](#architecture)
- [Roadmap](#roadmap)
- [Repository Layout](#repository-layout)
- [Requirements](#requirements)
- [Building](#building)
- [Usage](#usage)
- [Verifying the Build](#verifying-the-build)
- [Design Notes](#design-notes)
- [Contributing](#contributing)
- [License](#license)

---

## Vision

KestrelFS aims to be a distributed, POSIX-compatible cloud filesystem that
combines:

- **Kernel-level zero-copy I/O** for local NVMe cache hits — no context
  switch to userspace on the hot path.
- **Object-storage-backed durability** (S3-compatible backends) for cold
  data, with chunk/block-level deduplication and streaming upload/download.
- **Strongly consistent POSIX metadata**, backed by Redis/TiKV, served from
  a userspace control plane that can scale independently of the kernel data
  plane.

The explicit design goal is to **beat JuiceFS on cache-hit latency and
throughput** by moving the fast path (local cache read) entirely into the
kernel, while keeping all the complex, rapidly-iterating business logic
(metadata, chunking, S3 I/O) in a safe, memory-safe userspace daemon written
in Rust.

---

## Architecture

KestrelFS deliberately separates **control plane** and **data plane** across
a language and privilege boundary:

```
                         ┌─────────────────────────────┐
                         │      Userspace (Rust)        │
                         │                               │
                         │   KestrelFS Control-Plane     │
                         │   Daemon (Tokio async)        │
                         │                               │
                         │  • POSIX metadata (Redis/TiKV)│
                         │  • Chunk/Block slicing         │
                         │  • S3 SDK (object storage I/O) │
                         └───────────────▲───────────────┘
                                         │ mmap() shared ring buffers
                                         │ + ioctl()/poll() signalling
                         ┌───────────────▼───────────────┐
                         │      Kernel space (C)          │
                         │                                 │
                         │   KestrelFS Kernel Module       │
                         │                                 │
                         │  • VFS registration (super/inode/file) │
                         │  • /dev/kestrel_ctl char device  │
                         │  • Local NVMe cache (planned)     │
                         │  • Zero-copy read/write (planned) │
                         └─────────────────────────────────┘
```

**Data plane (kernel, C).** An out-of-tree Linux kernel module that:
- Registers a VFS filesystem type and implements the inode/dentry/file
  operations needed to mount and serve files.
- Owns the local NVMe SSD cache and performs zero-copy reads on cache hits
  (planned — see [Roadmap](#roadmap)).
- Talks to the Rust daemon only when necessary (cache miss, metadata
  lookup) via a lock-free shared-memory IPC bridge.

**Control plane (userspace, Rust).** An async daemon (Tokio) that:
- Owns all POSIX metadata semantics, backed by Redis/TiKV.
- Performs chunk/block slicing of file data.
- Talks to S3-compatible object storage via the AWS SDK for Rust.
- Never blocks the kernel — all slow I/O (network, disk) happens off the
  VFS call path.

**The bridge.** Kernel and Rust communicate through a `/dev/kestrel_ctl`
character device: the kernel allocates a single `mmap()`-able shared memory
region containing **two independent lock-free SPSC ring buffers** (request
and response), avoiding any data copy through the socket/Netlink layer. Every
cross-language structure is defined once, in a dual-purpose C header
(`kestrelfs_ipc.h`) consumable by both the kernel module and `bindgen` on the
Rust side, so the ABI can never silently drift between the two
implementations. See [Design Notes](#design-notes) for the full memory
layout and synchronization model.

---

## Roadmap

Development proceeds in four strictly sequential phases. **Do not assume any
phase beyond what's marked "done" below is implemented.**

| Phase | Goal | Status |
|---|---|---|
| **1. Minimal C kernel VFS skeleton** | `super.c`/`inode.c`/`file.c`, `register_filesystem()`, in-memory read-only mount exposing `hello.txt`. Out-of-tree `Makefile`. | ✅ Done |
| **2. C↔Rust IPC bridge** | Shared-memory protocol (`kestrelfs_ipc.h`): two mmap'd lock-free ring buffers, `/dev/kestrel_ctl` char device with `.mmap`/`.poll`/`.ioctl`. Rust-side consumer (`#[repr(C)]` mirrors, mmap, ring push/pop). | 🚧 In progress — kernel-side infrastructure (allocation, mmap, poll, ioctl wakeups) is done; ring push/pop business logic and the Rust consumer are not yet written. |
| **3. Rust daemon closed loop** | Real chunk/block data flow via Tokio + AWS S3 SDK, Redis-backed metadata. Cache misses go over the network end-to-end. | ⏳ Not started |
| **4. Kernel-owned NVMe cache** | Direct I/O against a local NVMe block device from kernel space. Cache hits perform a DMA copy straight back to the VFS caller, bypassing the Rust daemon entirely. | ⏳ Not started |

---

## Repository Layout

```
FerroFS/
├── LICENSE                  # Apache License 2.0
├── README.md                # this file
└── kestrelfs/                # kernel module (C) — out-of-tree build
    ├── Makefile               # kbuild wrapper, never builds the kernel itself
    ├── super.c                # module init/exit, filesystem_type registration
    ├── inode.c                # superblock ops, simple_fill_super()-based tree
    ├── file.c                 # hello.txt read-only file_operations
    ├── chardev.c               # /dev/kestrel_ctl: mmap + poll + ioctl bridge
    ├── kestrelfs.h             # internal cross-file declarations
    ├── kestrelfs_ipc.h         # kernel↔Rust ABI contract (ring buffers, ioctls)
    └── chardev_test.c          # standalone userspace smoke test for the chardev
```

The Rust control-plane daemon does not exist yet; it will land under a
`daemon/` directory starting in Phase 2's Rust-consumer step.

---

## Requirements

- **Linux kernel 5.x/6.x** with matching headers installed for your running
  kernel (`/lib/modules/$(uname -r)/build` must exist).
  - Debian/Ubuntu: `sudo apt install linux-headers-$(uname -r)`
- **GCC** and standard kernel build tooling (`make`, `bc`, `flex`, `bison` —
  usually already present if headers are installed).
- **Root privileges** for `insmod`/`rmmod`/`mount` (module loading is a
  privileged operation on any stock kernel).
- (Later phases) **Rust toolchain** (stable, via `rustup`) and `bindgen` for
  the control-plane daemon.

> KestrelFS never builds against a full kernel source tree and never runs a
> whole-kernel `make -j`. All builds are strictly out-of-tree, driven by
> `make -C $(KDIR) M=$(PWD) modules`.

---

## Building

```bash
cd kestrelfs
make
```

This produces `kestrelfs.ko` by invoking kbuild against your currently
running kernel's headers:

```
make -C /lib/modules/$(uname -r)/build M=$(pwd) modules
```

To build against a different kernel's headers, override `KDIR`:

```bash
make KDIR=/lib/modules/<other-version>/build
```

Clean build artifacts:

```bash
make clean
```

---

## Usage

### Load the module and mount

```bash
cd kestrelfs
make
sudo insmod kestrelfs.ko
sudo mkdir -p /mnt/kestrelfs
sudo mount -t kestrelfs none /mnt/kestrelfs
```

### Read the sample file

```bash
cat /mnt/kestrelfs/hello.txt
# Hello from KestrelFS Kernel Module (Phase 1)!
```

The mount is intentionally **read-only** in Phase 1 — write attempts fail
with `EROFS`/`EACCES` by design; there is no on-disk or object-storage
backing yet.

### Unmount and unload

```bash
sudo umount /mnt/kestrelfs
sudo rmmod kestrelfs
```

### Inspect the IPC char device

Loading the module also registers `/dev/kestrel_ctl`, the Phase 2 IPC bridge
to the (not-yet-implemented) Rust daemon:

```bash
ls -l /dev/kestrel_ctl
```

A standalone smoke test exercises the chardev's `open`/`ioctl`/`mmap`/`poll`
surface without requiring the Rust daemon:

```bash
gcc -O2 -Wall -I kestrelfs -o kestrelfs/chardev_test kestrelfs/chardev_test.c
sudo kestrelfs/chardev_test
```

Expected output includes ABI version / shared-region-size confirmation and a
successful `mmap()` of the ring-buffer region — see
[Verifying the Build](#verifying-the-build) for the full expected transcript.

---

## Verifying the Build

A full manual verification pass, useful after any change to `kestrelfs/`:

```bash
# 1. Build
cd kestrelfs && make

# 2. Load
sudo insmod kestrelfs.ko
lsmod | grep kestrelfs
cat /proc/filesystems | grep kestrelfs
dmesg | tail -5      # expect: "filesystem + chardev registered"

# 3. Mount and read
sudo mkdir -p /mnt/kestrelfs
sudo mount -t kestrelfs none /mnt/kestrelfs
ls -la /mnt/kestrelfs
cat /mnt/kestrelfs/hello.txt

# 4. Confirm read-only enforcement
echo test > /mnt/kestrelfs/hello.txt   # expect: Permission denied
touch /mnt/kestrelfs/new_file           # expect: Read-only file system

# 5. Exercise the chardev bridge
gcc -O2 -Wall -I . -o chardev_test chardev_test.c
sudo ./chardev_test                     # expect "[PASS] all chardev infrastructure checks succeeded"
dmesg | tail -5                          # expect: "chardev shared region allocated (131264 bytes, 1024 slots/ring)"

# 6. Unmount and unload cleanly
sudo umount /mnt/kestrelfs
sudo rmmod kestrelfs
dmesg | tail -5      # expect: "unloaded, filesystem + chardev unregistered", no WARNING/Oops/BUG
lsmod | grep kestrelfs   # expect: no output
```

Any `WARNING:`, `Oops:`, or `BUG:` line in `dmesg` at any point indicates a
regression and should block a change from landing.

---

## Design Notes

### Kernel↔Rust IPC contract

The shared memory region (`struct kestrelfs_shared_region` in
`kestrelfs_ipc.h`) is laid out as:

```
offset 0      : header (magic, abi_version, padding)     — 64 B
offset 64     : req_ctrl  (head/tail/capacity)             — 64 B (own cacheline)
offset 128    : resp_ctrl (head/tail/capacity)              — 64 B (own cacheline)
offset 192    : req_slots[1024]   (kernel  -> Rust requests)  — 64 KiB
offset 65728  : resp_slots[1024]  (Rust -> kernel responses)   — 64 KiB
                                                    total: 131264 B (~128 KiB)
```

Each event slot is exactly 64 bytes (one cacheline): a sequence number,
opcode, flags, request ID (echoed back on the matching response), an error
code, and a 32-byte opcode-specific payload. Ring control blocks are
cacheline-aligned and physically separated from each other to avoid false
sharing between producer and consumer indices.

All fields use fixed-width types (`__u32`/`__u64`/...) from
`<linux/types.h>`, with explicit padding and natural alignment (no packed
structs) so the Rust side's `#[repr(C)]` mirror is byte-identical without
needing `#[repr(packed)]`. Every structural invariant is enforced at compile
time via `_Static_assert`, checked under both kernel-C and plain userspace
GCC.

**Wakeup model** (no busy-spinning):
- **Kernel → Rust**: after pushing into the request ring, the kernel calls
  `wake_up_interruptible()` on an internal wait queue, waking any Rust
  thread parked in `poll()`/`epoll_wait()` on `/dev/kestrel_ctl`.
- **Rust → Kernel**: after pushing into the response ring, Rust issues the
  `KESTRELFS_IOC_NOTIFY_RESP` ioctl, which the kernel driver turns into
  `wake_up_all()` on the wait queue backing any kernel thread blocked on a
  pending response.

See the extensive comments at the top of `kestrelfs/kestrelfs_ipc.h` for the
full protocol rationale, and `kestrelfs/chardev.c` for the allocation
(`vmalloc_user()`) and mapping (`remap_vmalloc_range()`) strategy.

### Opcode payload layouts (metadata operations)

`KESTRELFS_OP_LOOKUP` and `KESTRELFS_OP_GETATTR` each define a fixed,
little-endian byte layout within the 32-byte payload (see
`kestrelfs_ipc.h` for the authoritative, byte-offset-by-byte-offset
documentation and matching `_Static_assert`s): `LOOKUP` requests carry
`parent_inode(u64)` + `name_len(u8)` + up to 23 bytes of `name`;
`LOOKUP`/`GETATTR` success responses each pack a different, fully
payload-filling set of attribute fields (`child_inode_id`/`size`/`mode`/
`uid`/`gid`/`nlink` for `LOOKUP`; `size`/`mode`/`uid`/`gid`/`nlink`/`mtime`
for `GETATTR`, trading the repeated inode id for an `mtime` field since the
requester already supplied that id and matches the response via `req_id`).
Failures never use the payload - they are always carried in the event
header's `error_code` field. This did not require an `KESTRELFS_ABI_VERSION`
bump: it only adds interpretation rules for previously-reserved payload
bytes of two opcodes that had no producer/consumer code on either side yet,
without changing any existing struct's size, alignment, or field offsets.

### Known Limitations

**Write path (Phase 3 step 4)**: The current implementation does **not** yet
handle file truncation or `O_TRUNC` semantics. When shell redirection
(`> file`) overwrites a file with shorter content, the file size remains at
its previous (larger) value, and subsequent reads may return stale tail bytes
from earlier writes.

**Root cause**: The kernel module does not detect or communicate `O_TRUNC` to
the daemon, and the daemon's `append_slice()` method only ever grows file
size (`max(old_size, new_end)`), never shrinks it. The COW (copy-on-write)
slice model preserves all historical writes; reads correctly select the newest
slice for each byte range, but the reported file size stays at the maximum
offset ever written.

**Example failure**:
```bash
echo "LONGCONTENT" > /mnt/kestrelfs/writable.dat  # size=11
echo "SHORT" > /mnt/kestrelfs/writable.dat        # size still 11
cat /mnt/kestrelfs/writable.dat                   # reads "SHORTONTENT" (stale tail)
```

**Workaround** (until `truncate` support is added in a future step):
```bash
truncate -s 0 /mnt/kestrelfs/writable.dat  # manually shrink to 0 first
echo "newdata" > /mnt/kestrelfs/writable.dat
```

Or ensure every overwrite is at least as long as the previous file size.

A future step will add `KESTRELFS_OP_TRUNCATE` (IPC opcode 5) and kernel-side
`O_TRUNC` detection to properly implement POSIX file truncation semantics.

### Coding standards

- Kernel C code strictly follows the Linux kernel coding style and is built
  exclusively out-of-tree — the kernel source tree itself is never modified
  or rebuilt.
- Kernel code is kept deliberately minimal, with defensive null/bounds
  checks everywhere a fault could otherwise panic the kernel; all non-trivial
  business logic is pushed to the Rust control plane.
- Any structure shared across the C/Rust boundary is defined once in a
  dual-purpose header and validated with compile-time layout assertions on
  both sides — cross-language memory layout is never taken on faith.

---

## Contributing

This project is under active, phase-gated development. Please open an issue
before starting work on anything beyond the current phase in the
[Roadmap](#roadmap) — out-of-order contributions are unlikely to be merged
until their prerequisite phase lands.

---

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
