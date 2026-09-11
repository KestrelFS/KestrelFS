// SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note
//! KestrelFS control-plane daemon.
//!
//! # Status
//!
//! This binary is **not yet the real control-plane daemon** described
//! in the project roadmap (no Redis, no S3). It proves two things
//! end-to-end:
//!
//! 1. (Phase 2) The kernel<->Rust IPC bridge itself: open
//!    `/dev/kestrel_ctl`, validate the ABI, `mmap()` the shared
//!    ring-buffer region, then run a `poll()`-driven loop that drains
//!    REQ events pushed by the kernel and pushes back a matching RESP
//!    event for each one.
//! 2. (Phase 3 step 2) That REQ events can be answered from a real
//!    (if in-memory) metadata store rather than synthesized data:
//!    `KESTRELFS_OP_LOOKUP`/`KESTRELFS_OP_GETATTR` requests are now
//!    answered by querying a [`meta::MemStore`] - see
//!    [`build_response`]. `KESTRELFS_OP_READ_CHUNK` deliberately still
//!    answers with the same synthetic, deterministic string it always
//!    has (see [`handle_read_chunk`]) - wiring it to
//!    `MetaStore::read_slices`/real block storage is out of scope for
//!    this step (see the Phase 3 step 2 task description).
//!
//! # Why a Tokio runtime for an otherwise-synchronous poll() loop
//!
//! [`meta::MetaStore`]'s methods are `async fn` (see that trait's doc
//! comment for why: the eventual production backing store is a
//! network-attached Redis, so every method must be async from day
//! one). The IPC event loop itself, however, is fundamentally
//! synchronous: `libc::poll()` is a blocking syscall with no async
//! equivalent in play here (there is deliberately no async I/O
//! runtime integration for the character device fd itself - see
//! `ring.rs`'s module doc comment on the single-producer/single-consumer
//! design). Rather than restructure the whole event loop around an
//! async runtime's I/O reactor (unnecessary complexity for a
//! `MemStore` that never actually awaits on real I/O), this daemon
//! keeps `main()`/`event_loop()` fully synchronous and creates one
//! [`tokio::runtime::Runtime`], calling [`tokio::runtime::Runtime::block_on`]
//! only at the single call site (inside [`build_response`]) that
//! needs to invoke an `async fn`. This preserves the event loop's
//! existing single-consumer/single-producer ring invariant exactly
//! (see `ring.rs`) - `drain_requests`/`push_response` are still only
//! ever called from this one synchronous thread, `block_on` merely
//! drives a `Future` to completion inline before returning control to
//! that same thread, it does not hand any ring access off to a
//! different runtime-managed thread.

mod abi;
mod device;
mod fs_model;
mod ioctl;
mod meta;
mod object_store;
mod ring;

use abi::{AttrFields, KestrelfsEvent};
use device::KestrelDevice;
use fs_model::{Inode, Slice};
use meta::{MetaError, MetaStore};
use object_store::ObjectStore;
use std::io;
use std::sync::Arc;

/// Seeds the single block backing `remote.txt`'s initial content.
///
/// The block key is derived from [`meta::REMOTE_TXT_SEED_SLICE_ID`]
/// (a nil UUID) and block index 0 (since the entire 512-byte file fits
/// in one block), following the [`Slice::block_key`] convention. The
/// content is a deterministic, human-readable pattern repeated to fill
/// exactly 512 bytes, chosen so `cat /mnt/kestrelfs/remote.txt` shows
/// something obviously different from the old Phase 2 synthetic string
/// ("KestrelFS remote chunk @ offset=...") that was generated on the fly.
///
/// This function is called exactly once at daemon startup, before
/// entering the event loop.
async fn seed_remote_txt_block(store: &Arc<dyn ObjectStore>) -> io::Result<()> {
    const BLOCK_SIZE: usize = 512;
    const PATTERN: &[u8] = b"Phase3-seed-data! ";

    let mut data = Vec::with_capacity(BLOCK_SIZE);
    while data.len() < BLOCK_SIZE {
        let remaining = BLOCK_SIZE - data.len();
        let chunk = &PATTERN[..remaining.min(PATTERN.len())];
        data.extend_from_slice(chunk);
    }

    let seed_slice = Slice {
        chunk_index: 0,
        slice_id: meta::REMOTE_TXT_SEED_SLICE_ID,
        chunk_offset: 0,
        length: BLOCK_SIZE as u32,
        written_at: 0,
    };
    let block_key = seed_slice.block_key(0);

    store
        .put(block_key, data)
        .await
        .map_err(|e| io::Error::other(e.to_string()))?;

    Ok(())
}

fn main() -> io::Result<()> {
    abi::compile_time_layout_asserts();

    println!("kestrelfs-daemon: opening /dev/kestrel_ctl ...");
    let dev = KestrelDevice::open()?;
    println!(
        "kestrelfs-daemon: attached OK (abi_version={}, region_size={} bytes)",
        abi::ABI_VERSION,
        abi::SHM_REGION_SIZE
    );

    // A single, small multi-thread-capable Tokio runtime, created
    // once and kept alive for the process lifetime purely to
    // `block_on()` MetaStore calls from the synchronous event loop
    // below - see this module's doc comment for why the event loop
    // itself is not restructured to be async. `rt-multi-thread` is
    // enabled in Cargo.toml even though this bootstrap step's
    // MetaStore (MemStore) never spawns additional work onto it,
    // purely so a future MetaStore implementation that DOES need
    // background tasks (e.g. a real Redis client's connection
    // management) does not require touching this runtime
    // construction again.
    let runtime = tokio::runtime::Runtime::new()?;

    let object_store: Arc<dyn ObjectStore> = Arc::new(object_store::MemObjectStore::new());
    let store: Arc<dyn MetaStore> = Arc::new(meta::MemStore::new());

    // Seed the block data for remote.txt's single Slice (see
    // meta::REMOTE_TXT_SEED_SLICE_ID). MemStore already created the
    // Slice metadata (inode 3, chunk 0, offset 0, length 512); we now
    // populate the corresponding block in the ObjectStore so reads can
    // actually return data instead of NotFound.
    runtime.block_on(seed_remote_txt_block(&object_store))?;

    println!("kestrelfs-daemon: MemStore initialized (seeded: /, /remote.txt)");

    println!("kestrelfs-daemon: entering poll() event loop, waiting for REQ events ...");

    event_loop(&dev, &runtime, &store, &object_store)
}

/// Blocks in `poll()` on the device fd until the kernel wakes us up
/// (via `wake_up_interruptible()` in `kestrelfs_req_push()`), then
/// drains and answers every pending REQ event, forever.
///
/// This is deliberately a simple, single-threaded loop for this
/// bootstrap step - see the module doc comment on `ring.rs` for why
/// that is sufficient for the current single-producer/single-consumer
/// role split.
fn event_loop(
    dev: &KestrelDevice,
    runtime: &tokio::runtime::Runtime,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
) -> io::Result<()> {
    let mut pfd = libc::pollfd {
        fd: dev.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };

    loop {
        pfd.revents = 0;

        // SAFETY: `&mut pfd` points at a single valid `libc::pollfd`
        // on the stack, matching the `nfds = 1` argument. `-1` as the
        // timeout requests an indefinite block, matching this
        // function's documented "wait forever" behavior. `poll()`
        // itself performs no memory access beyond reading/writing
        // through this one pointer, which is safe C-ABI FFI as long
        // as the pointer and count agree, which they do here.
        let ret = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1, -1) };

        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                // A signal (e.g. during interactive Ctrl-C testing)
                // interrupted the syscall; retry rather than treating
                // this as a fatal error.
                continue;
            }
            return Err(err);
        }

        if pfd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(io::Error::other(format!(
                "kestrelfs-daemon: /dev/kestrel_ctl fd reported POLLERR/POLLNVAL (revents=0x{:x})",
                pfd.revents
            )));
        }

        if pfd.revents & libc::POLLIN == 0 {
            // Spurious wakeup (or POLLHUP with nothing readable);
            // nothing to drain, go back to sleep.
            continue;
        }

        drain_and_respond(dev, runtime, store, object_store);
    }
}

/// Drains every currently-pending REQ event and pushes back one
/// matching RESP event per request, then notifies the kernel once via
/// `KESTRELFS_IOC_NOTIFY_RESP` if at least one response was published.
///
/// Batching the single `notify_resp()` call after the whole drain
/// (rather than one ioctl per event) mirrors how the kernel's own
/// `kestrelfs_wake_req_waiters()` is called once per
/// `kestrelfs_req_push()` rather than per-slot - both sides favor
/// "notify once after doing a batch of ring work" over "notify on
/// every single slot", trading a small amount of response latency for
/// far fewer syscalls under load.
fn drain_and_respond(
    dev: &KestrelDevice,
    runtime: &tokio::runtime::Runtime,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
) {
    // SAFETY: `dev.region_ptr()` returns the pointer established by
    // `KestrelDevice::open()`'s successful `mmap()`, which remains
    // valid for as long as `dev` (borrowed here) is alive - guaranteed
    // by this function's `&KestrelDevice` parameter outliving the
    // call. This is the daemon's single consumer thread for the REQ
    // ring (see `ring.rs` module doc comment), satisfying
    // `drain_requests`'s "no concurrent caller" safety requirement.
    // `runtime.block_on()` inside the closure below runs entirely on
    // this same thread (see this module's doc comment on why a Tokio
    // runtime is used here at all) - it never hands ring access to a
    // different thread, preserving that same single-consumer
    // invariant for the duration of this whole drain.
    let mut responded = 0u64;
    let drained = unsafe {
        ring::drain_requests(dev.region_ptr(), |event: &KestrelfsEvent| {
            println!(
                "kestrelfs-daemon: <- REQ  seq={} req_id={} opcode={} flags={}",
                event.seq, event.req_id, event.opcode, event.flags
            );

            let response = runtime.block_on(build_response(event, store, object_store));

            // SAFETY: same reasoning as the `drain_requests` call
            // below - `dev.region_ptr()` is valid for the duration of
            // this call, and this daemon is the RESP ring's sole
            // producer thread, satisfying `push_response`'s safety
            // requirement. This closure body is already lexically
            // inside the `unsafe` block wrapping `drain_requests`
            // below, so no additional `unsafe { }` is needed (and
            // rustc rightly warns if one is added).
            let pushed = ring::push_response(dev.region_ptr(), response);

            if pushed {
                responded += 1;
                println!(
                    "kestrelfs-daemon: -> RESP req_id={} opcode={}",
                    event.req_id, response.opcode
                );
            } else {
                eprintln!(
                    "kestrelfs-daemon: RESP ring full, dropping response for req_id={}",
                    event.req_id
                );
            }
        })
    };

    if drained > 0 {
        println!("kestrelfs-daemon: drained {drained} REQ event(s)");
    }

    if responded > 0 {
        if let Err(e) = dev.notify_resp() {
            eprintln!("kestrelfs-daemon: KESTRELFS_IOC_NOTIFY_RESP failed: {e}");
        }
    }
}

/// Builds the appropriate response event for one drained REQ event,
/// dispatching on `event.opcode`.
///
/// - `OP_LOOKUP`/`OP_GETATTR` are answered by querying `store` (see
///   [`handle_lookup`]/[`handle_getattr`]) - this is this daemon's
///   first real metadata-backed request handling (Phase 3 step 2).
/// - `OP_READ_CHUNK` still answers with the same deterministic,
///   synthetic payload it always has (see [`handle_read_chunk`]) -
///   wiring it to `store.read_slices()`/real block storage is
///   explicitly out of scope for this step.
/// - `OP_NOP` still answers with a bare `OP_RESULT_OK` (no payload
///   needed).
async fn build_response(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
) -> KestrelfsEvent {
    match event.opcode {
        abi::OP_LOOKUP => handle_lookup(event, store).await,
        abi::OP_GETATTR => handle_getattr(event, store).await,
        abi::OP_READ_CHUNK => handle_read_chunk(event, store, object_store).await,
        abi::OP_NOP => KestrelfsEvent::zeroed(abi::OP_RESULT_OK, event.req_id),
        other => {
            eprintln!(
                "kestrelfs-daemon: unhandled opcode {other} for req_id={}, replying with generic OK",
                event.req_id
            );
            KestrelfsEvent::zeroed(abi::OP_RESULT_OK, event.req_id)
        }
    }
}

/// Converts a [`MetaError`] into the negative errno-style value the
/// event header's `error_code` field carries on a
/// `KESTRELFS_OP_RESULT_ERROR` response (see `kestrelfs_ipc.h`'s doc
/// comment on why errors never use the payload).
///
/// Uses `libc`'s errno constants (already a dependency for the
/// `ioctl`/`poll`/`mmap` FFI calls elsewhere in this daemon) rather
/// than hand-rolled magic numbers, so this mapping stays correct
/// automatically across any target `libc` is itself built for.
fn meta_error_to_errno(err: &MetaError) -> i32 {
    match err {
        MetaError::NotFound => -libc::ENOENT,
        MetaError::NotADirectory => -libc::ENOTDIR,
        MetaError::InvalidName(_) => -libc::ENAMETOOLONG,
    }
}

/// Converts a fully-resolved [`Inode`] into the wire-format
/// [`AttrFields`] every `OP_LOOKUP`/`OP_GETATTR` success response is
/// built from.
///
/// A small, explicit conversion function - deliberately not a `From`/
/// `Into` impl living in `abi.rs` itself, to keep that module fully
/// decoupled from `fs_model` (see `AttrFields`'s doc comment in
/// `abi.rs` for the reasoning); this function is the one place that
/// bridges the two.
fn inode_to_attr_fields(inode: &Inode) -> AttrFields {
    AttrFields {
        size: inode.size,
        mode: inode.mode,
        uid: inode.uid,
        gid: inode.gid,
        nlink: inode.nlink,
        mtime: inode.mtime,
    }
}

/// Answers a `KESTRELFS_OP_LOOKUP` request: decodes `(parent_inode,
/// name)` from the request payload (see
/// [`KestrelfsEvent::decode_lookup_req`]), resolves the child's inode
/// id via [`MetaStore::lookup`], then immediately fetches that
/// child's full attributes via [`MetaStore::getattr`] so the response
/// carries complete attributes in one round trip - mirroring how a
/// real `lookup(2)`-backing VFS operation returns a full `struct
/// stat`, not just an inode number (see `kestrelfs_ipc.h`'s doc
/// comment on `MetaStore` method selection).
///
/// # Error mapping
///
/// - A malformed request payload (name too long for the wire format,
///   or not valid UTF-8 - see [`abi::LookupDecodeError`]) is mapped to
///   `-ENAMETOOLONG`/`-EINVAL` respectively, without ever calling into
///   `store` at all.
/// - [`MetaError::NotFound`] (no such parent, or no child with this
///   name) -> `-ENOENT`.
/// - [`MetaError::NotADirectory`] (parent exists but is a regular
///   file) -> `-ENOTDIR`.
async fn handle_lookup(event: &KestrelfsEvent, store: &Arc<dyn MetaStore>) -> KestrelfsEvent {
    let req = match event.decode_lookup_req() {
        Ok(req) => req,
        Err(abi::LookupDecodeError::NameTooLong(len)) => {
            eprintln!(
                "kestrelfs-daemon:    OP_LOOKUP req_id={} malformed: name_len={len} exceeds LOOKUP_NAME_MAX",
                event.req_id
            );
            return KestrelfsEvent::error_response(event.req_id, -libc::ENAMETOOLONG);
        }
        Err(abi::LookupDecodeError::InvalidUtf8) => {
            eprintln!(
                "kestrelfs-daemon:    OP_LOOKUP req_id={} malformed: name is not valid UTF-8",
                event.req_id
            );
            return KestrelfsEvent::error_response(event.req_id, -libc::EINVAL);
        }
    };

    println!(
        "kestrelfs-daemon:    OP_LOOKUP parent={} name={:?}",
        req.parent_inode, req.name
    );

    let child_inode_id = match store.lookup(req.parent_inode, &req.name).await {
        Ok(id) => id,
        Err(err) => {
            let errno = meta_error_to_errno(&err);
            eprintln!(
                "kestrelfs-daemon:    OP_LOOKUP parent={} name={:?} -> error: {err} (errno {errno})",
                req.parent_inode, req.name
            );
            return KestrelfsEvent::error_response(event.req_id, errno);
        }
    };

    let inode = match store.getattr(child_inode_id).await {
        Ok(inode) => inode,
        Err(err) => {
            // The child id we just resolved a moment ago no longer
            // has attributes - only possible under concurrent
            // mutation of the store (not yet possible with the
            // current read-only MetaStore trait, but handled
            // defensively rather than assumed impossible).
            let errno = meta_error_to_errno(&err);
            eprintln!(
                "kestrelfs-daemon:    OP_LOOKUP resolved child_inode_id={child_inode_id} but getattr failed: {err} (errno {errno})"
            );
            return KestrelfsEvent::error_response(event.req_id, errno);
        }
    };

    println!(
        "kestrelfs-daemon:    OP_LOOKUP parent={} name={:?} -> inode={child_inode_id} size={} mode={:o}",
        req.parent_inode, req.name, inode.size, inode.mode
    );

    KestrelfsEvent::lookup_response(event.req_id, child_inode_id, inode_to_attr_fields(&inode))
}

/// Answers a `KESTRELFS_OP_GETATTR` request: decodes `inode_id` from
/// the request payload (see [`KestrelfsEvent::decode_getattr_req`])
/// and resolves its attributes via [`MetaStore::getattr`].
///
/// # Error mapping
///
/// [`MetaError::NotFound`] -> `-ENOENT`. `NotADirectory`/`InvalidName`
/// are not reachable from `getattr()` per the `MetaStore` trait's own
/// contract (only `lookup()` can produce them), but
/// [`meta_error_to_errno`] maps them anyway for completeness/future-proofing
/// rather than this function special-casing which variants it
/// "expects".
async fn handle_getattr(event: &KestrelfsEvent, store: &Arc<dyn MetaStore>) -> KestrelfsEvent {
    let req = event.decode_getattr_req();

    println!("kestrelfs-daemon:    OP_GETATTR inode={}", req.inode_id);

    let inode = match store.getattr(req.inode_id).await {
        Ok(inode) => inode,
        Err(err) => {
            let errno = meta_error_to_errno(&err);
            eprintln!(
                "kestrelfs-daemon:    OP_GETATTR inode={} -> error: {err} (errno {errno})",
                req.inode_id
            );
            return KestrelfsEvent::error_response(event.req_id, errno);
        }
    };

    println!(
        "kestrelfs-daemon:    OP_GETATTR inode={} -> size={} mode={:o} nlink={}",
        req.inode_id, inode.size, inode.mode, inode.nlink
    );

    KestrelfsEvent::getattr_response(event.req_id, inode_to_attr_fields(&inode))
}

/// Answers a `KESTRELFS_OP_READ_CHUNK` request by fetching the
/// requested byte range from the backing store (MetaStore for slice
/// metadata + ObjectStore for block data), introduced in Phase 3 step 3.
///
/// This replaces the Phase 2 stub that synthesized "KestrelFS remote
/// chunk @ offset=..." strings on the fly. As of this step:
/// - `req.inode_id` (added in KESTRELFS_ABI_VERSION 2) identifies which
///   file to read.
/// - `req.offset`/`req.count` specify the byte range (already clamped
///   by the kernel to fit in one response payload - see
///   `kestrelfs_remote_read()` in kestrelfs/file.c).
/// - We look up the file's slices in MetaStore, find which slice(s)
///   cover the requested range, fetch the corresponding block(s) from
///   ObjectStore, and copy the relevant bytes into the response payload.
/// - Any byte within the file's declared `size` that is NOT covered by
///   a slice (a "hole") is returned as zero (sparse-file semantics).
/// - If `req.inode_id` does not exist, or `req.offset` is beyond the
///   file's size, return `-ENOENT`.
///
/// For this Phase 3 step, only `remote.txt` (inode 3) has any slices
/// seeded (one Slice covering 0..512), and only MemObjectStore is wired
/// up (no Redis/S3 yet). A future Phase 4 will generalize this to
/// arbitrary files, multi-block slices, and network-backed stores.
async fn handle_read_chunk(
    event: &KestrelfsEvent,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
) -> KestrelfsEvent {
    let req = event.decode_read_chunk_req();

    // Step 1: getattr to validate the inode exists and get its size
    let inode = match store.getattr(req.inode_id).await {
        Ok(inode) => inode,
        Err(MetaError::NotFound) => {
            println!(
                "kestrelfs-daemon:    OP_READ_CHUNK inode={} offset={} count={} -> ENOENT (inode not found)",
                req.inode_id, req.offset, req.count
            );
            return KestrelfsEvent::error_response(event.req_id, meta_error_to_errno(&MetaError::NotFound));
        }
        Err(e) => {
            eprintln!(
                "kestrelfs-daemon:    OP_READ_CHUNK inode={} offset={} count={} -> EIO (getattr failed: {e})",
                req.inode_id, req.offset, req.count
            );
            return KestrelfsEvent::error_response(event.req_id, -libc::EIO);
        }
    };

    // Step 2: EOF check (offset beyond file size -> return 0 bytes,
    // which the kernel interprets as EOF)
    if req.offset >= inode.size {
        println!(
            "kestrelfs-daemon:    OP_READ_CHUNK inode={} offset={} count={} -> 0 bytes (EOF, size={})",
            req.inode_id, req.offset, req.count, inode.size
        );
        return KestrelfsEvent::read_chunk_response(event.req_id, &[]);
    }

    // Step 3: Clamp count to not read past EOF
    let clamped_count = ((inode.size - req.offset) as u32).min(req.count);

    // Step 4: read_from_slices helper does the heavy lifting (find
    // covering slices, fetch blocks, assemble bytes)
    match read_from_slices(req.inode_id, req.offset, clamped_count, store, object_store).await {
        Ok(data) => {
            println!(
                "kestrelfs-daemon:    OP_READ_CHUNK inode={} offset={} count={} -> {} bytes",
                req.inode_id, req.offset, req.count, data.len()
            );
            KestrelfsEvent::read_chunk_response(event.req_id, &data)
        }
        Err(e) => {
            eprintln!(
                "kestrelfs-daemon:    OP_READ_CHUNK inode={} offset={} count={} -> EIO ({e})",
                req.inode_id, req.offset, req.count
            );
            KestrelfsEvent::error_response(event.req_id, -libc::EIO)
        }
    }
}

/// Reads `count` bytes starting at file offset `file_offset` from inode
/// `inode_id`, consulting the MetaStore's slice metadata and fetching
/// block data from the ObjectStore.
///
/// Returns a `Vec<u8>` of exactly `count` bytes. Any byte not covered
/// by a slice (a "hole" in the file) is returned as 0. If multiple
/// slices overlap the same byte, the slice with the greatest
/// `written_at` timestamp wins (copy-on-write / last-write-wins
/// semantics, per the fs_model.rs module doc).
///
/// # Errors
///
/// Returns `Err` if the MetaStore's `read_slices` call fails, or if any
/// required block is missing from the ObjectStore. The caller should
/// surface these as `-EIO` to the kernel.
async fn read_from_slices(
    inode_id: u64,
    file_offset: u64,
    count: u32,
    store: &Arc<dyn MetaStore>,
    object_store: &Arc<dyn ObjectStore>,
) -> Result<Vec<u8>, String> {
    let mut result = vec![0u8; count as usize];

    // Identify which chunk(s) this read spans (Phase 3 bootstrap: we
    // only ever read within one chunk since kernel clamps to 32 bytes,
    // but the logic here is written generically for future expansion).
    let start_chunk = Inode::chunk_index_for_offset(file_offset);
    let end_offset = file_offset + count as u64;
    let end_chunk = if end_offset == 0 {
        start_chunk
    } else {
        Inode::chunk_index_for_offset(end_offset - 1)
    };

    for chunk_idx in start_chunk..=end_chunk {
        let slices = store
            .read_slices(inode_id, chunk_idx)
            .await
            .map_err(|e| format!("read_slices(inode={inode_id}, chunk={chunk_idx}) failed: {e}"))?;

        if slices.is_empty() {
            continue;
        }

        // For each byte in this chunk that our read range touches, find
        // the covering slice with the greatest written_at (if any)
        let chunk_base = chunk_idx as u64 * fs_model::CHUNK_SIZE;
        let read_start_in_chunk = file_offset.saturating_sub(chunk_base);
        let read_end_in_chunk = (end_offset.saturating_sub(chunk_base)).min(fs_model::CHUNK_SIZE);

        for offset_in_chunk in read_start_in_chunk..read_end_in_chunk {
            let covering_slice = slices
                .iter()
                .filter(|s| {
                    let s_start = s.chunk_offset as u64;
                    let s_end = s_start + s.length as u64;
                    offset_in_chunk >= s_start && offset_in_chunk < s_end
                })
                .max_by_key(|s| s.written_at);

            if let Some(slice) = covering_slice {
                let byte_in_slice = offset_in_chunk - slice.chunk_offset as u64;
                let block_idx = (byte_in_slice / fs_model::BLOCK_SIZE) as u32;
                let byte_in_block = (byte_in_slice % fs_model::BLOCK_SIZE) as usize;

                let block_key = slice.block_key(block_idx);
                let block_data = object_store
                    .get(&block_key)
                    .await
                    .map_err(|e| format!("ObjectStore::get({block_key}) failed: {e}"))?;

                if byte_in_block < block_data.len() {
                    let file_byte_idx = (chunk_base + offset_in_chunk - file_offset) as usize;
                    result[file_byte_idx] = block_data[byte_in_block];
                }
            }
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use meta::{MemStore, REMOTE_TXT_INODE};

    /// Builds a raw `OP_LOOKUP` request event, encoding `(parent_inode,
    /// name)` by hand at the exact wire offsets documented in
    /// `kestrelfs_ipc.h` - deliberately independent of any
    /// encoder/decoder in `abi.rs`, so these `build_response` tests
    /// validate against the wire format itself, not against whatever
    /// `abi.rs` happens to (correctly or incorrectly) produce.
    fn raw_lookup_req(req_id: u64, parent_inode: u64, name: &str) -> KestrelfsEvent {
        let mut event = KestrelfsEvent::zeroed(abi::OP_LOOKUP, req_id);
        event.payload[0..8].copy_from_slice(&parent_inode.to_le_bytes());
        let name_bytes = name.as_bytes();
        event.payload[8] = name_bytes.len() as u8;
        event.payload[9..9 + name_bytes.len()].copy_from_slice(name_bytes);
        event
    }

    /// Builds a raw `OP_GETATTR` request event.
    fn raw_getattr_req(req_id: u64, inode_id: u64) -> KestrelfsEvent {
        let mut event = KestrelfsEvent::zeroed(abi::OP_GETATTR, req_id);
        event.payload[0..8].copy_from_slice(&inode_id.to_le_bytes());
        event
    }

    /// Shared test fixture: a fresh runtime + `MemStore`, exactly
    /// mirroring how `main()` constructs both.
    fn test_fixture() -> (tokio::runtime::Runtime, Arc<dyn MetaStore>, Arc<dyn ObjectStore>) {
        let runtime = tokio::runtime::Runtime::new().expect("runtime construction must not fail in tests");
        let store: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let object_store: Arc<dyn ObjectStore> = Arc::new(object_store::MemObjectStore::new());
        (runtime, store, object_store)
    }

    #[test]
    fn lookup_remote_txt_under_root_succeeds() {
        let (runtime, store, object_store) = test_fixture();
        let req = raw_lookup_req(1, fs_model::ROOT_INODE, "remote.txt");

        let resp = runtime.block_on(build_response(&req, &store, &object_store));

        assert_eq!(resp.opcode, abi::OP_RESULT_OK);
        assert_eq!(resp.req_id, 1);
        assert_eq!(resp.error_code, 0);

        let child_inode_id = u64::from_le_bytes(resp.payload[0..8].try_into().unwrap());
        let size = u64::from_le_bytes(resp.payload[8..16].try_into().unwrap());
        assert_eq!(child_inode_id, REMOTE_TXT_INODE);
        assert_eq!(size, 512);
    }

    #[test]
    fn lookup_unknown_name_returns_enoent() {
        let (runtime, store, object_store) = test_fixture();
        let req = raw_lookup_req(2, fs_model::ROOT_INODE, "does_not_exist.txt");

        let resp = runtime.block_on(build_response(&req, &store, &object_store));

        assert_eq!(resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(resp.req_id, 2);
        assert_eq!(resp.error_code, -libc::ENOENT);
    }

    #[test]
    fn lookup_under_nonexistent_parent_returns_enoent() {
        let (runtime, store, object_store) = test_fixture();
        let req = raw_lookup_req(3, 9999, "anything");

        let resp = runtime.block_on(build_response(&req, &store, &object_store));

        assert_eq!(resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(resp.error_code, -libc::ENOENT);
    }

    #[test]
    fn lookup_under_regular_file_returns_enotdir() {
        let (runtime, store, object_store) = test_fixture();
        // remote.txt (inode 2) is a regular file, not a directory.
        let req = raw_lookup_req(4, REMOTE_TXT_INODE, "anything");

        let resp = runtime.block_on(build_response(&req, &store, &object_store));

        assert_eq!(resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(resp.error_code, -libc::ENOTDIR);
    }

    #[test]
    fn lookup_with_name_too_long_returns_enametoolong_without_querying_store() {
        let (runtime, store, object_store) = test_fixture();
        let mut req = KestrelfsEvent::zeroed(abi::OP_LOOKUP, 5);
        req.payload[0..8].copy_from_slice(&fs_model::ROOT_INODE.to_le_bytes());
        req.payload[8] = (abi::LOOKUP_NAME_MAX + 1) as u8;

        let resp = runtime.block_on(build_response(&req, &store, &object_store));

        assert_eq!(resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(resp.error_code, -libc::ENAMETOOLONG);
    }

    #[test]
    fn getattr_root_succeeds() {
        let (runtime, store, object_store) = test_fixture();
        let req = raw_getattr_req(6, fs_model::ROOT_INODE);

        let resp = runtime.block_on(build_response(&req, &store, &object_store));

        assert_eq!(resp.opcode, abi::OP_RESULT_OK);
        assert_eq!(resp.error_code, 0);

        let mode = u32::from_le_bytes(resp.payload[8..12].try_into().unwrap());
        assert_eq!(mode & fs_model::S_IFDIR, fs_model::S_IFDIR);
    }

    #[test]
    fn getattr_remote_txt_succeeds() {
        let (runtime, store, object_store) = test_fixture();
        let req = raw_getattr_req(7, REMOTE_TXT_INODE);

        let resp = runtime.block_on(build_response(&req, &store, &object_store));

        assert_eq!(resp.opcode, abi::OP_RESULT_OK);
        assert_eq!(resp.error_code, 0);

        let size = u64::from_le_bytes(resp.payload[0..8].try_into().unwrap());
        let mode = u32::from_le_bytes(resp.payload[8..12].try_into().unwrap());
        assert_eq!(size, 512);
        assert_eq!(mode & fs_model::S_IFREG, fs_model::S_IFREG);
    }

    #[test]
    fn getattr_unknown_inode_returns_enoent() {
        let (runtime, store, object_store) = test_fixture();
        let req = raw_getattr_req(8, 9999);

        let resp = runtime.block_on(build_response(&req, &store, &object_store));

        assert_eq!(resp.opcode, abi::OP_RESULT_ERROR);
        assert_eq!(resp.error_code, -libc::ENOENT);
    }

    #[test]
    fn read_chunk_returns_seeded_data_from_object_store() {
        // Phase 3 step 3: READ_CHUNK now fetches real data from
        // MetaStore (slice metadata) + ObjectStore (block bytes),
        // replacing the old synthetic "KestrelFS remote chunk @ offset=..."
        // stub. This test verifies the end-to-end path for remote.txt
        // (inode 3), which MemStore::new() seeds with one Slice covering
        // bytes 0..512, and test_fixture() seeds the corresponding block
        // in MemObjectStore.
        let (runtime, store, object_store) = test_fixture();

        // Seed the block so the test has data to read
        runtime.block_on(async {
            seed_remote_txt_block(&object_store).await.expect("seed failed");
        });

        // Construct OP_READ_CHUNK request for inode=3, offset=0, count=32
        // (the ABI_VERSION 2 layout: inode_id@0, offset@8, count@16)
        let mut req = KestrelfsEvent::zeroed(abi::OP_READ_CHUNK, 10);
        req.payload[0..8].copy_from_slice(&meta::REMOTE_TXT_INODE.to_le_bytes());
        req.payload[8..16].copy_from_slice(&0u64.to_le_bytes());
        req.payload[16..20].copy_from_slice(&32u32.to_le_bytes());

        let resp = runtime.block_on(build_response(&req, &store, &object_store));

        assert_eq!(resp.opcode, abi::OP_RESULT_OK);
        // The seeded pattern is "Phase3-seed-data! " repeated - verify
        // the response payload starts with that pattern
        let data = &resp.payload[..32];
        let expected_start = b"Phase3-seed-data! Phase3-seed-da";
        assert_eq!(data, expected_start);
    }

    #[test]
    fn nop_still_returns_ok() {
        let (runtime, store, object_store) = test_fixture();
        let req = KestrelfsEvent::zeroed(abi::OP_NOP, 10);

        let resp = runtime.block_on(build_response(&req, &store, &object_store));

        assert_eq!(resp.opcode, abi::OP_RESULT_OK);
        assert_eq!(resp.req_id, 10);
    }
}
