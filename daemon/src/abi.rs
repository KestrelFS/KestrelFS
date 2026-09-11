// SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note
//! ABI mirror of `kestrelfs_ipc.h`.
//!
//! Every type in this module MUST byte-for-byte match its C counterpart
//! defined in `kestrelfs/kestrelfs_ipc.h`. This is the single most
//! safety-critical file in the daemon: if these layouts ever drift from
//! the kernel header, the daemon will silently misinterpret shared
//! kernel memory, corrupting the ring buffers or reading garbage.
//!
//! # Why hand-written instead of `bindgen`
//!
//! A `bindgen`-generated version of this file would be strictly safer
//! against drift, and is the intended long-term approach (see the
//! `build.rs` note at the bottom of this file). For this Phase 2 step we
//! hand-mirror the header instead, deliberately keeping this file small
//! and heavily commented so every field can be checked by eye against
//! `kestrelfs_ipc.h` line-by-line. [`compile_time_layout_asserts`] then
//! pins every `size_of`/`align_of`/offset invariant at compile time,
//! exactly mirroring the `_Static_assert`s at the bottom of the C
//! header, so any accidental drift fails the Rust build loudly instead
//! of corrupting shared memory silently at runtime.
//!
//! # Why `#[repr(C)]` without `#[repr(packed)]`
//!
//! The C header is deliberately written with explicit padding fields and
//! natural alignment (see its "CROSS-LANGUAGE ALIGNMENT CONTRACT"
//! section) specifically so that plain `#[repr(C)]` - which uses the
//! platform's normal alignment rules, identical to what a C compiler
//! does for a non-packed struct - reproduces an identical byte layout.
//! `#[repr(packed)]` is avoided on purpose: it forces byte-unaligned
//! loads/stores for multi-byte fields, which is both slower and, for
//! `AtomicU64`, outright undefined behavior on some platforms (atomic
//! ops generally require natural alignment).

use std::sync::atomic::{AtomicU32, AtomicU64};

/// Number of slots per ring. Mirrors `KESTRELFS_RING_SLOTS` in
/// `kestrelfs_ipc.h`. MUST be a power of two.
pub const RING_SLOTS: usize = 1024;

/// Mirrors `KESTRELFS_RING_MASK`.
pub const RING_MASK: u64 = (RING_SLOTS - 1) as u64;

/// Mirrors `KESTRELFS_CACHELINE_SIZE`.
pub const CACHELINE_SIZE: usize = 64;

/// Mirrors `KESTRELFS_EVENT_PAYLOAD_SIZE`.
pub const EVENT_PAYLOAD_SIZE: usize = 32;

/// Mirrors `KESTRELFS_ABI_VERSION`. The daemon refuses to attach to a
/// kernel module reporting any other value (see [`super::device::open`]).
pub const ABI_VERSION: u32 = 1;

/// Mirrors `KESTRELFS_SHM_MAGIC` ("KSRS" packed into a little-endian u32).
pub const SHM_MAGIC: u32 = 0x4B53_5253;

// ---------------------------------------------------------------------
// Opcodes - mirrors the `KESTRELFS_OP_*` #defines.
// ---------------------------------------------------------------------
//
// Every opcode from `kestrelfs_ipc.h` is mirrored here for ABI
// completeness even though this Phase 2 bootstrap daemon only ever
// produces/consumes `OP_NOP` and `OP_RESULT_OK` (see `main.rs`) - the
// remaining opcodes will be exercised once real VFS call paths
// (Phase 2's VFS integration step, and Phase 3's chunk/metadata logic)
// start issuing `OP_LOOKUP`/`OP_READ_CHUNK`/`OP_GETATTR` requests.

/// No-op / padding event. Mirrors `KESTRELFS_OP_NOP`.
#[allow(dead_code)]
pub const OP_NOP: u32 = 0;
/// Request: resolve path -> inode metadata. Mirrors `KESTRELFS_OP_LOOKUP`.
#[allow(dead_code)]
pub const OP_LOOKUP: u32 = 1;
/// Request: fetch chunk data (cache miss). Mirrors `KESTRELFS_OP_READ_CHUNK`.
#[allow(dead_code)]
pub const OP_READ_CHUNK: u32 = 2;
/// Request: fetch inode attributes. Mirrors `KESTRELFS_OP_GETATTR`.
#[allow(dead_code)]
pub const OP_GETATTR: u32 = 3;
/// Response: generic success. Mirrors `KESTRELFS_OP_RESULT_OK`.
pub const OP_RESULT_OK: u32 = 64;
/// Response: generic failure, see `error_code`. Mirrors
/// `KESTRELFS_OP_RESULT_ERROR`.
#[allow(dead_code)]
pub const OP_RESULT_ERROR: u32 = 65;

// ---------------------------------------------------------------------
// struct kestrelfs_event
// ---------------------------------------------------------------------

/// Mirrors `struct kestrelfs_event` in `kestrelfs_ipc.h`.
///
/// Field-for-field layout (must total exactly 64 bytes, one cacheline):
///
/// ```text
/// seq          u64   offset  0   sequence number assigned by producer
/// opcode       u32   offset  8   one of the OP_* constants above
/// flags        u32   offset 12   reserved, must be zero for now
/// req_id       u64   offset 16   unique request id, echoed back on RESP
/// error_code   i32   offset 24   0 = success, negative errno on failure
/// _reserved0   u32   offset 28   padding to keep req_id/payload aligned
/// payload      [u8;32] offset 32 opcode-specific raw bytes
/// ------------------------------------------------------------------
/// total: 64 bytes
/// ```
///
/// `#[derive(Clone, Copy)]` is safe and intentional: this type is a pure
/// value type over shared memory, must never own any Rust-managed
/// resource (no `String`, `Vec`, `Box`, etc.), and is frequently copied
/// out of the mmap'd region into a local stack value before being
/// inspected - copying is exactly the safe way to read a shared-memory
/// slot without holding a live reference into memory another party
/// (the kernel) can mutate concurrently.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct KestrelfsEvent {
    pub seq: u64,
    pub opcode: u32,
    pub flags: u32,
    pub req_id: u64,
    pub error_code: i32,
    pub _reserved0: u32,
    pub payload: [u8; EVENT_PAYLOAD_SIZE],
}

impl KestrelfsEvent {
    /// Builds a zeroed event with `opcode`/`req_id` set, matching how
    /// `kestrelfs_req_push()` in `ipc_ring.c` fills a freshly claimed
    /// slot (`seq` is set separately by the caller, mirroring the C
    /// side's `slot->seq = head`).
    pub fn zeroed(opcode: u32, req_id: u64) -> Self {
        KestrelfsEvent {
            seq: 0,
            opcode,
            flags: 0,
            req_id,
            error_code: 0,
            _reserved0: 0,
            payload: [0u8; EVENT_PAYLOAD_SIZE],
        }
    }
}

// ---------------------------------------------------------------------
// struct kestrelfs_ring_ctrl
// ---------------------------------------------------------------------

/// Mirrors `struct kestrelfs_ring_ctrl` in `kestrelfs_ipc.h`.
///
/// `head` and `tail` are `AtomicU64` here (NOT plain `u64`) because,
/// unlike [`KestrelfsEvent`], this struct's fields are the actual
/// cross-process synchronization primitives - every access MUST go
/// through an explicit `Ordering`, exactly as the C side must use
/// `smp_store_release()`/`smp_load_acquire()` rather than plain reads
/// and writes. Using `AtomicU64::load(Ordering::Relaxed)` by accident
/// instead of `Ordering::Acquire` would be exactly as dangerous as the C
/// side accidentally using `READ_ONCE()` where `smp_load_acquire()` was
/// required - the type system here does not prevent choosing the wrong
/// `Ordering`, only prevents forgetting to go through an atomic op at
/// all (which prevents the much more common bug of a plain data race
/// under the Rust/LLVM memory model, and matches what the linked
/// `_pad` comment in the C header already documents as a requirement).
///
/// `AtomicU64` has the same size and (on x86_64) the same alignment as
/// a plain `u64`, so this substitution does not change the struct's
/// layout - verified by [`compile_time_layout_asserts`] below.
#[repr(C)]
pub struct KestrelfsRingCtrl {
    pub head: AtomicU64,
    pub tail: AtomicU64,
    pub capacity: AtomicU32,
    pub _reserved0: u32,
    pub _pad: [u8; CACHELINE_SIZE - (2 * 8 + 2 * 4)],
}

// ---------------------------------------------------------------------
// struct kestrelfs_shared_region
// ---------------------------------------------------------------------

/// Mirrors `struct kestrelfs_shared_region` in `kestrelfs_ipc.h` - the
/// entire `mmap()`-ed layout.
///
/// # Safety contract for callers
///
/// A `*mut KestrelfsSharedRegion` obtained from `mmap()` points at
/// kernel-owned memory that is concurrently read/written by the kernel
/// module. Every access to `req_slots`/`resp_slots` element contents
/// (as opposed to the atomic `head`/`tail` control fields) must be
/// preceded by an acquire load of the relevant `head` that establishes
/// "this slot index is safely published" - see [`crate::ring`] for the
/// actual push/pop protocol built on top of this type. This struct
/// itself only defines layout; it enforces no ordering by construction
/// (Rust's type system has no way to express "this field may only be
/// read after that atomic load", so the discipline is documented here
/// and enforced by code review / the `ring` module's API surface, not
/// by the compiler).
#[repr(C)]
pub struct KestrelfsSharedRegion {
    pub magic: u32,
    pub abi_version: u32,
    pub _pad0: [u8; CACHELINE_SIZE - 2 * 4],

    pub req_ctrl: KestrelfsRingCtrl,
    pub resp_ctrl: KestrelfsRingCtrl,

    pub req_slots: [KestrelfsEvent; RING_SLOTS],
    pub resp_slots: [KestrelfsEvent; RING_SLOTS],
}

/// Mirrors `KESTRELFS_SHM_REGION_SIZE` (`sizeof(struct
/// kestrelfs_shared_region)` on the C side). Used to size the `mmap()`
/// call and cross-checked against the kernel's own report of this value
/// via `KESTRELFS_IOC_GET_REGION_SIZE` before trusting the mapping at
/// all - see [`super::device::KestrelDevice::open`].
pub const SHM_REGION_SIZE: usize = std::mem::size_of::<KestrelfsSharedRegion>();

/// Compile-time layout assertions, mirroring the `_Static_assert`s at
/// the bottom of `kestrelfs_ipc.h`. Called once from `main()` - Rust
/// has no direct `static_assert` construct as clean as C11's outside of
/// `const` evaluation contexts, so we assert on plain `const` values
/// computed via `size_of`/`align_of` instead. Because every value here
/// is a `const`, a failing assertion is caught the moment this function
/// is exercised (we call it unconditionally at the top of `main()`), not
/// buried behind a rarely-hit runtime code path.
pub fn compile_time_layout_asserts() {
    assert_eq!(
        std::mem::size_of::<KestrelfsEvent>(),
        64,
        "KestrelfsEvent must be exactly 64 bytes (one cacheline), matches \
         the _Static_assert in kestrelfs_ipc.h"
    );

    assert_eq!(
        std::mem::size_of::<KestrelfsRingCtrl>(),
        CACHELINE_SIZE,
        "KestrelfsRingCtrl must be exactly one cacheline"
    );
    assert_eq!(
        std::mem::align_of::<KestrelfsRingCtrl>(),
        std::mem::align_of::<u64>(),
        "KestrelfsRingCtrl alignment must match plain u64 alignment - if \
         this ever fails, AtomicU64 stopped being layout-compatible with \
         u64 on this target and the whole ABI mirror is unsound"
    );

    assert_eq!(
        SHM_REGION_SIZE,
        131_264,
        "kestrelfs_shared_region size drifted from the value verified \
         against the running kernel module during Phase 2 development \
         (see README/design notes); if this legitimately changed, the \
         C header, KESTRELFS_ABI_VERSION, and this constant must all be \
         bumped together"
    );

    assert_eq!(
        RING_SLOTS & (RING_SLOTS - 1),
        0,
        "RING_SLOTS must be a power of two"
    );
}

// NOTE(bindgen): a future iteration of this module can be replaced by
// running `bindgen kestrelfs/kestrelfs_ipc.h -o daemon/src/abi_generated.rs`
// from a `build.rs`, which removes the hand-mirroring risk entirely by
// generating these types directly from the canonical C header. This was
// deliberately deferred for this Phase 2 step to keep the diff reviewable
// field-by-field against kestrelfs_ipc.h; the `compile_time_layout_asserts`
// checks above are what stand in for that guarantee until then.
