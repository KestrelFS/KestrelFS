// SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note
//! Lock-free ring buffer consume/produce protocol, mirroring the C
//! side's `ipc_ring.c`.
//!
//! This module is the Rust-side counterpart to the memory-ordering
//! contract documented at the top of `kestrelfs/ipc_ring.c`. Read that
//! file's module doc comment first - every `Ordering` choice below is a
//! direct translation of an `smp_store_release()`/`smp_load_acquire()`
//! call on the C side, and the two must be understood as one single
//! cross-language protocol, not two independent implementations that
//! happen to agree.
//!
//! # Why raw pointers instead of `&KestrelfsSharedRegion`
//!
//! This module deliberately operates on a raw `*mut KestrelfsSharedRegion`
//! rather than a safe `&`/`&mut KestrelfsSharedRegion` reference, for two
//! concrete reasons:
//!
//! 1. **Concurrent external mutation.** The kernel module writes into
//!    `req_slots` (and reads `resp_slots`) *concurrently* with this
//!    process, entirely outside Rust's borrow checker's knowledge. A
//!    safe `&T` reference asserts to the compiler "nothing mutates this
//!    memory for the reference's lifetime" (enabling aggressive
//!    optimizations like caching a loaded value across calls) - which
//!    is exactly false here. Only raw pointer access, which makes no
//!    such promise, is sound for memory a foreign, non-Rust-visible
//!    actor can modify.
//! 2. **Mixed read/write access to the same struct.** [`push_response`]
//!    must *write* into `resp_slots` (plain data) while other code may
//!    concurrently need to read `req_ctrl`'s atomics. A single safe
//!    `&mut KestrelfsSharedRegion` covering the whole region would make
//!    that impossible to express without `unsafe` splitting anyway;
//!    raw pointers sidestep the issue entirely since they carry no
//!    aliasing contract to violate.
//!
//! Every individual field access is still as narrowly scoped and
//! documented as possible: atomic fields (`head`/`tail`) are accessed
//! by taking a short-lived `&AtomicU64` (which is always sound - that
//! is the entire purpose of atomics), and slot data is read/written via
//! single-expression raw pointer dereferences that never materialize a
//! persisting `&`/`&mut` reference over an entire array or struct.
//!
//! # Roles on each ring
//!
//! - REQ ring: kernel is the producer, **this Rust daemon is the sole
//!   consumer**. [`drain_requests`] implements that consumer side.
//! - RESP ring: **this Rust daemon is the sole producer**, kernel is
//!   the consumer. [`push_response`] implements that producer side.
//!
//! Because this daemon is a single-threaded event loop in this Phase 2
//! step (see `main.rs`), there is no equivalent here of the C side's
//! `req_push_lock` spinlock - that lock exists only to linearize the
//! kernel's *multiple concurrent VFS threads* into one logical
//! producer. On the Rust side there is exactly one producer thread and
//! exactly one consumer thread (the same OS thread, in fact, in this
//! bootstrap version), so no additional mutual exclusion is needed
//! beyond the atomic operations themselves.

use crate::abi::{KestrelfsEvent, KestrelfsSharedRegion, RING_MASK};
use std::sync::atomic::Ordering;

/// Drains every currently-published-but-unconsumed slot from the REQ
/// ring (kernel -> Rust) and invokes `handler` once per event, in FIFO
/// order.
///
/// # Safety
///
/// `region` must be a non-null pointer to a `KestrelfsSharedRegion`
/// obtained from a successful `mmap()` of `/dev/kestrel_ctl`, valid for
/// reads for the entire call, and must remain valid (i.e. not yet
/// `munmap()`-ed) until this function returns. The caller must ensure
/// no other thread is concurrently calling [`drain_requests`] on the
/// same `region` (this daemon's single-threaded event loop guarantees
/// this trivially; a future multi-threaded consumer would need to
/// externally serialize calls, exactly as the kernel's
/// `req_push_lock` serializes its multiple producer threads).
///
/// # Ordering
///
/// 1. `head` is read with [`Ordering::Acquire`]. This pairs with the
///    kernel's `smp_store_release(&region->req_ctrl.head, ...)` in
///    `kestrelfs_req_push()` (see `ipc_ring.c`): observing a given
///    `head` value via an acquire load guarantees every store the
///    kernel performed *before* its release store - i.e. the full
///    contents of every slot up to (but not including) that `head` -
///    is already visible to this thread. Without `Acquire` here, the
///    compiler or CPU could in principle reorder the slot-content
///    reads below to before this load, risking observing a stale
///    (pre-publication) version of a slot the kernel has *already*
///    told us is ready.
/// 2. Each slot in `[tail, head)` is read via a single-expression raw
///    pointer dereference (an implicit copy, since `KestrelfsEvent` is
///    `Copy`) - never through a persisting `&KestrelfsEvent` reference.
///    This is safe precisely because step 1's acquire load already
///    established that these bytes are fully published and the kernel
///    will not touch them again until we advance `tail` past them (see
///    step 3).
/// 3. `tail` is advanced with [`Ordering::Release`] after every slot in
///    the drained range has been copied out. This is the mirror image
///    of step 1: it publishes to the kernel that these slots are now
///    free to be overwritten by a new request. Using anything weaker
///    than `Release` here would let the "slot is free again" signal
///    become visible to the kernel *before* our reads of the old
///    contents actually complete, which could let the kernel start
///    overwriting a slot we are still in the middle of reading.
///
/// Returns the number of events drained.
pub unsafe fn drain_requests<F>(region: *const KestrelfsSharedRegion, mut handler: F) -> u64
where
    F: FnMut(&KestrelfsEvent),
{
    debug_assert!(!region.is_null(), "drain_requests: null region pointer");

    // SAFETY: caller contract guarantees `region` is valid for reads;
    // `req_ctrl.head` is an `AtomicU64` field, so taking a reference to
    // it specifically (not to the whole struct) and calling `.load()`
    // is always sound regardless of what else concurrently touches the
    // rest of the struct - that is the defining property of atomics.
    let head = unsafe { (*region).req_ctrl.head.load(Ordering::Acquire) };
    // SAFETY: same reasoning as above, applied to `req_ctrl.tail`. We
    // use `Relaxed` for this particular load because we are the only
    // writer of `tail` (single-threaded consumer), so there is no
    // concurrent modification of `tail` to synchronize with here - we
    // only need *our own* last-written value back, which a plain
    // `Relaxed` load on a location only this thread writes always
    // provides correctly.
    let mut tail = unsafe { (*region).req_ctrl.tail.load(Ordering::Relaxed) };

    let mut drained = 0u64;

    while tail != head {
        let idx = (tail & RING_MASK) as usize;

        // SAFETY: `idx` is masked into `[0, RING_SLOTS)` and
        // `req_slots` has exactly `RING_SLOTS` elements (guaranteed by
        // the `#[repr(C)]` layout mirroring the C array of the same
        // fixed size), so this index is always in bounds. The slot's
        // contents are safe to read per the acquire-load-on-head
        // argument in the function doc comment: we have already
        // observed (via the `Acquire` load of `head` before this loop)
        // that the kernel published this slot's data before advancing
        // `head` past `tail`. This is a single-expression read that
        // implicitly copies the `Copy` value out - no `&KestrelfsEvent`
        // reference into the shared region is ever retained.
        let event: KestrelfsEvent = unsafe { (*region).req_slots[idx] };

        handler(&event);

        tail = tail.wrapping_add(1);
        drained += 1;
    }

    if drained > 0 {
        // SAFETY: `req_ctrl.tail` is an `AtomicU64`; storing to it via
        // a short-lived `&AtomicU64` reference is always sound. This
        // publishes our consumption progress: `Release` ensures the
        // slot reads above happen-before the kernel can validly
        // consider these slots free again (see function doc comment,
        // ordering step 3).
        unsafe {
            (*region).req_ctrl.tail.store(tail, Ordering::Release);
        }
    }

    drained
}

/// Publishes one response event into the RESP ring (Rust -> kernel).
///
/// This is the direct mirror of `kestrelfs_req_push()` in `ipc_ring.c`,
/// with the producer/consumer roles reversed: here the Rust daemon is
/// the ring's sole producer.
///
/// # Safety
///
/// `region` must be a non-null pointer to a `KestrelfsSharedRegion`
/// obtained from a successful `mmap()` of `/dev/kestrel_ctl`, valid for
/// reads *and writes* for the entire call, and must remain valid until
/// this function returns. The caller must ensure no other thread is
/// concurrently calling [`push_response`] on the same `region` (see the
/// module-level doc comment: this bootstrap daemon has exactly one
/// producer thread for the RESP ring).
///
/// # Ordering
///
/// 1. `head` is read with plain [`Ordering::Relaxed`] purely to compute
///    "where does my new slot go" - safe because, in this bootstrap
///    single-threaded daemon, only this function ever advances
///    `resp_ctrl.head`, so there is no concurrent writer whose stores
///    we need to synchronize with via this particular load. (Were a
///    second Rust-side producer thread introduced later, this would
///    need to become a proper compare-and-swap loop or be moved behind
///    a mutex - exactly analogous to why the kernel's `req_push_lock`
///    exists only because the kernel has multiple concurrent producer
///    threads.)
/// 2. `tail` is read with [`Ordering::Acquire`] to get an up-to-date
///    view of the kernel consumer's progress for the capacity check
///    below - this pairs with the kernel's own
///    `smp_store_release(&region->resp_ctrl.tail, ...)` in
///    `kestrelfs_check_resp()`.
/// 3. The full event is written into `resp_slots[idx]` via a single
///    raw-pointer place-expression assignment (an ordinary, non-atomic
///    store) - the direct mirror of the kernel's `memcpy()` into
///    `req_slots[idx]` in `kestrelfs_req_push()`.
/// 4. `head` is published with [`Ordering::Release`] - this is the
///    Rust-side half of the pairing described in `kestrelfs_ipc.h`'s
///    "SYNCHRONIZATION / MEMORY ORDERING" section, and is what
///    `kestrelfs_check_resp()`'s
///    `smp_load_acquire(&region->resp_ctrl.head)` on the kernel side
///    pairs with: the kernel is guaranteed to see a fully-formed slot
///    for every index below whatever `head` value it observes.
///
/// # Capacity
///
/// Mirrors the kernel's own `-EAGAIN` check in `kestrelfs_req_push()`:
/// if the ring is full (`head - tail >= RING_SLOTS`), returns `false`
/// and does not touch the ring at all. The caller (see `main.rs`) is
/// expected to notify the kernel via `ioctl(NOTIFY_RESP)` only when this
/// returns `true`.
pub unsafe fn push_response(region: *mut KestrelfsSharedRegion, mut event: KestrelfsEvent) -> bool {
    debug_assert!(!region.is_null(), "push_response: null region pointer");

    // SAFETY: `resp_ctrl.head`/`resp_ctrl.tail` are `AtomicU64` fields;
    // taking a short-lived reference to each and loading is always
    // sound regardless of concurrent access to the rest of the struct.
    let head = unsafe { (*region).resp_ctrl.head.load(Ordering::Relaxed) };
    let tail = unsafe { (*region).resp_ctrl.tail.load(Ordering::Acquire) };

    if head.wrapping_sub(tail) >= crate::abi::RING_SLOTS as u64 {
        return false;
    }

    let idx = (head & RING_MASK) as usize;
    event.seq = head;

    // SAFETY: `idx` is masked into `[0, RING_SLOTS)`, matching
    // `resp_slots`'s fixed size. The kernel only ever reads
    // `resp_slots` after observing this slot's index as published via
    // an acquire load of `resp_ctrl.head` (see `kestrelfs_check_resp()`
    // in `ipc_ring.c`), which cannot happen before the `Release` store
    // below - so no concurrent reader can observe this slot mid-write.
    // This is a single place-expression assignment, not a persisting
    // `&mut` borrow over the array.
    unsafe {
        (*region).resp_slots[idx] = event;
    }

    // SAFETY: see ordering step 4 above; `resp_ctrl.head` is an
    // `AtomicU64`, so storing through a short-lived reference to it is
    // always sound.
    unsafe {
        (*region)
            .resp_ctrl
            .head
            .store(head.wrapping_add(1), Ordering::Release);
    }

    true
}
