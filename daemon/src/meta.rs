// SPDX-License-Identifier: Apache-2.0
//! Metadata storage abstraction: the `MetaStore` trait and an
//! in-memory `MemStore` implementation.
//!
//! # Why a trait instead of calling a concrete KV client directly
//!
//! The eventual production backing store for KestrelFS metadata is
//! Redis (per the project roadmap; TiKV is also mentioned as a
//! longer-term option). Defining `MetaStore` as a trait now, with
//! [`MemStore`] as the first (and for this Phase 3 step, only)
//! implementation, means:
//!
//! - The IPC event loop (`main.rs`) and any future VFS-facing logic
//!   can be written and tested against `MemStore` immediately,
//!   without standing up a real Redis instance.
//! - Swapping in a `RedisStore` later (Phase 3's later steps) is a
//!   pure addition - implement the same trait against
//!   `redis`/`fred`/etc, no call site elsewhere in the daemon needs
//!   to change.
//! - Every method is `async` from day one (even though `MemStore`'s
//!   implementations never actually await anything beyond acquiring
//!   an uncontended `RwLock`), so the trait's shape already matches
//!   what a real network-backed store will require - no "make
//!   everything async" refactor later.
//!
//! # Status
//!
//! `lookup()`/`getattr()` are wired into the IPC event loop as of
//! Phase 3 step 2 (see `main.rs`'s `handle_lookup`/`handle_getattr`).
//! `read_slices()` remains deliberately unconnected - per that step's
//! explicit scope, `KESTRELFS_OP_READ_CHUNK` still answers with a
//! synthetic payload rather than querying slice data (see
//! `main.rs::handle_read_chunk`). The `#[allow(dead_code)]` on
//! `read_slices` (and on `MemStore::allocate_inode_id`, unused by any
//! trait method yet) reflects that this is deliberate, temporary
//! scaffolding rather than genuinely dead code - both are exercised
//! by this module's own unit tests (`cargo test`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::fs_model::{Inode, Slice, ROOT_INODE};

/// Errors a [`MetaStore`] implementation can report.
///
/// Deliberately a small, closed set for this bootstrap step - modeled
/// closely enough on POSIX errno semantics that a future VFS call
/// path (kernel side) can map each variant to the matching negative
/// errno almost mechanically (`NotFound` -> `-ENOENT`, `NotADirectory`
/// -> `-ENOTDIR`, etc), the same way [`crate::abi::KestrelfsEvent`]'s
/// `error_code` field is already documented to carry negative errno
/// values.
#[derive(Debug, thiserror::Error)]
pub enum MetaError {
    /// No inode/directory entry exists for the requested id/name.
    #[error("inode or directory entry not found")]
    NotFound,
    /// `lookup()` was called with a `parent` inode that exists but is
    /// not a directory.
    #[error("parent inode is not a directory")]
    NotADirectory,
    /// A name provided to a future `create`/`lookup` call was not
    /// valid UTF-8 or exceeded a length limit. Not yet produced by
    /// any method in this bootstrap step (kept for forward
    /// compatibility with `MetaStore::lookup`'s `name: &str`
    /// parameter, which assumes valid UTF-8 has already been
    /// extracted from whatever raw bytes the kernel sent).
    ///
    /// As of Phase 3 step 2, name validation for `OP_LOOKUP` actually
    /// happens one layer up, at ABI decode time (see
    /// `abi::LookupDecodeError`, checked before `MetaStore::lookup`
    /// is ever called) - this variant remains unconstructed for now,
    /// kept for a future `create()`-like addition that accepts a name
    /// directly rather than via the fixed-width wire format.
    #[allow(dead_code)]
    #[error("invalid name: {0}")]
    InvalidName(String),
}

/// Convenience alias, matching the `Result<T>` naming used throughout
/// the rest of the daemon's modules (e.g. `std::io::Result`).
pub type Result<T> = std::result::Result<T, MetaError>;

/// `MetaStore` - async abstraction over the POSIX metadata backing
/// store.
///
/// # Method selection
///
/// The three methods below are exactly the three read-side metadata
/// operations needed to answer the opcodes already defined in
/// `kestrelfs_ipc.h` (`KESTRELFS_OP_LOOKUP`, `KESTRELFS_OP_GETATTR`,
/// `KESTRELFS_OP_READ_CHUNK`'s data-location half):
///
/// - [`MetaStore::lookup`]: resolves `(parent_inode, name)` to a
///   child inode id - the metadata half of `KESTRELFS_OP_LOOKUP`.
/// - [`MetaStore::getattr`]: resolves an inode id to its full
///   [`Inode`] record - `KESTRELFS_OP_GETATTR`, and also what a
///   `KESTRELFS_OP_LOOKUP` handler calls next once it has the child's
///   id, to return complete attributes in one round trip (mirroring
///   how a real `lookup(2)`-backing VFS operation returns a full
///   `struct stat`, not just an inode number).
/// - [`MetaStore::read_slices`]: resolves `(inode, chunk_index)` to
///   the list of [`Slice`] records describing where that chunk's data
///   physically lives - what a `KESTRELFS_OP_READ_CHUNK` handler
///   needs before it can even begin fetching block data from the
///   object store.
///
/// Write-side methods (`create`, `write`/append-a-slice, `setattr`,
/// `unlink`, ...) are deliberately not part of this trait yet - see
/// the project roadmap: Phase 3's first step is read-side metadata
/// modeling only, matching what `remote.txt` already exercises
/// end-to-end over IPC (Phase 2). Adding write support is a
/// forward-compatible trait extension, not a breaking change to
/// what's defined here.
#[async_trait]
pub trait MetaStore: Send + Sync {
    /// Resolves `(parent, name)` to the child's inode id.
    ///
    /// Mirrors POSIX `lookup(2)`/`->lookup()` VFS semantics: `parent`
    /// must itself already be a known directory inode.
    ///
    /// # Errors
    ///
    /// [`MetaError::NotFound`] if `parent` does not exist, or exists
    /// but has no child named `name`. [`MetaError::NotADirectory`] if
    /// `parent` exists but is a regular file, not a directory.
    async fn lookup(&self, parent: u64, name: &str) -> Result<u64>;

    /// Resolves `inode` to its full [`Inode`] attribute record.
    ///
    /// # Errors
    ///
    /// [`MetaError::NotFound`] if no inode with this id exists.
    async fn getattr(&self, inode: u64) -> Result<Inode>;

    /// Resolves `(inode, chunk_idx)` to the list of [`Slice`] records
    /// making up that chunk's currently-visible data.
    ///
    /// An empty `Vec` is a valid, non-error result: it means this
    /// chunk of the file exists (in the sense that `inode`'s `size`
    /// covers this chunk's byte range) but has never actually been
    /// written to - a sparse hole, which a caller should treat as
    /// reading all zero bytes for that range.
    ///
    /// # Errors
    ///
    /// [`MetaError::NotFound`] if `inode` does not exist.
    ///
    /// Not yet called from `main.rs`'s event loop as of Phase 3 step
    /// 2 - `KESTRELFS_OP_READ_CHUNK` still answers with a synthetic
    /// payload (see `main.rs::handle_read_chunk`) rather than
    /// resolving real slice data. Exercised directly by this module's
    /// own unit tests in the meantime.
    #[allow(dead_code)]
    async fn read_slices(&self, inode: u64, chunk_idx: u32) -> Result<Vec<Slice>>;
}

/// One directory's worth of `name -> child inode id` mappings.
type DirEntries = HashMap<String, u64>;

/// `MemStore` - a [`MetaStore`] implementation backed purely by
/// in-process `HashMap`s behind a [`tokio::sync::RwLock`].
///
/// # Why `tokio::sync::RwLock` rather than `std::sync::RwLock`
///
/// Every `MetaStore` method is `async fn` (required by the trait, to
/// match what a real network-backed store will need). Using
/// `std::sync::RwLock` inside an `async fn` would risk holding a
/// synchronous lock guard across an `.await` point in some future,
/// more complex method implementation, which can deadlock a
/// single-threaded (or work-stealing) async executor. `tokio::sync::RwLock`'s
/// guard is safe to hold across `.await` points by construction, so
/// starting with it now avoids a foot-gun even though today's method
/// bodies happen to never actually await anything while holding the
/// lock.
///
/// # Concurrency model
///
/// A single `RwLock` guards both the inode table and the directory
/// entry table together (rather than two independent locks) - simpler
/// to reason about for this bootstrap step, and sufficient since
/// every method's critical section is extremely short (a handful of
/// `HashMap` operations, no I/O). A production store would likely
/// shard or use a real transactional backend instead of a single
/// process-wide lock, but that concern is delegated entirely to
/// whatever `MetaStore` implementation eventually replaces `MemStore`
/// - callers of the trait are unaffected either way.
pub struct MemStore {
    inner: RwLock<MemStoreInner>,
    /// Source of fresh inode ids for any future `create`-like
    /// operation. Not yet used by any trait method (`MemStore` today
    /// only ever seeds its two hardcoded entries at construction
    /// time via [`MemStore::new`]), but kept ready so a future
    /// `create()` addition to `MetaStore` has an obvious,
    /// already-in-place allocator to call rather than needing to
    /// retrofit one. A plain `AtomicU64` (not behind the `RwLock`)
    /// since id allocation is independent of the inode/dirent tables'
    /// own consistency.
    next_inode_id: AtomicU64,
}

/// The actual mutable state behind `MemStore`'s `RwLock`, split out
/// as its own struct purely so `MemStore`'s single `RwLock<..>` field
/// declaration stays readable rather than becoming a `RwLock<(HashMap<..>, HashMap<..>)>`
/// tuple.
struct MemStoreInner {
    inodes: HashMap<u64, Inode>,
    /// `parent_inode_id -> (child_name -> child_inode_id)`.
    dir_entries: HashMap<u64, DirEntries>,
    /// `inode_id -> (chunk_index -> slices)`. Grouping slices by
    /// chunk index up front (rather than storing one flat
    /// `Vec<Slice>` per inode and filtering by `chunk_index` on every
    /// `read_slices` call) means `read_slices` is a direct double
    /// `HashMap` lookup with no scanning - the same access pattern a
    /// real KV store (Redis key `slices:{inode}:{chunk_idx}`) would
    /// naturally have too.
    slices: HashMap<u64, HashMap<u32, Vec<Slice>>>,
}

/// Fixed inode id for the bootstrap `remote.txt` entry seeded by
/// [`MemStore::new`]. Previously 2 (which collided with the root
/// directory in the kernel's tree_descr array indexing - see
/// kestrelfs/inode.c), now 3 as of Phase 3 step 3 (tree_descr places
/// remote.txt at index [3] to match).
pub const REMOTE_TXT_INODE: u64 = 3;

/// Deterministic UUID for the single Slice [`MemStore::new`] seeds for
/// `remote.txt`, covering the file's entire 512-byte extent. The
/// corresponding block (keyed as `<this UUID>/0` in the ObjectStore -
/// see [`Slice::block_key`] in fs_model.rs) must be populated at
/// daemon startup (see `main.rs`'s call to `seed_remote_txt_block`).
///
/// This is a nil UUID (all zeros) purely for determinism in tests and
/// manual inspection - a real deployment would use `Uuid::new_v4()` for
/// every slice, but this bootstrap step benefits from having the same
/// block key on every daemon restart so `cat /mnt/kestrelfs/remote.txt`
/// always reads the same predictable content.
pub const REMOTE_TXT_SEED_SLICE_ID: uuid::Uuid = uuid::Uuid::nil();


impl MemStore {
    /// Builds a new `MemStore`, pre-seeded with exactly two entries:
    ///
    /// - Inode [`ROOT_INODE`] (1): the root directory `/`.
    /// - Inode [`REMOTE_TXT_INODE`] (2): a regular file named
    ///   `remote.txt`, linked as a child of the root directory.
    ///
    /// This mirrors (at the metadata layer) the static `remote.txt`
    /// entry the kernel side's `tree_descr` table already exposes
    /// (see `kestrelfs/inode.c`) - the two are not yet wired
    /// together (this Phase 3 step is metadata-model-only, per the
    /// task), but sharing the same name/inode-2 convention keeps the
    /// eventual integration step's mapping obvious.
    pub fn new() -> Self {
        let now = current_unix_time();

        let root = Inode::new_root_dir(now);

        // Chosen so remote.txt's declared `size` matches
        // KESTRELFS_REMOTE_FILE_SIZE (512 bytes) from the kernel
        // side's file.c/kestrelfs.h, keeping the two bootstrap
        // "remote.txt" definitions consistent even before they are
        // actually connected.
        const REMOTE_TXT_SIZE: u64 = 512;
        let remote_txt = Inode::new_file(REMOTE_TXT_INODE, REMOTE_TXT_SIZE, now);

        let mut inodes = HashMap::new();
        inodes.insert(ROOT_INODE, root);
        inodes.insert(REMOTE_TXT_INODE, remote_txt);

        let mut root_entries = DirEntries::new();
        root_entries.insert("remote.txt".to_string(), REMOTE_TXT_INODE);

        let mut dir_entries = HashMap::new();
        dir_entries.insert(ROOT_INODE, root_entries);

        // Seed one Slice for remote.txt, covering the entire file
        // (chunk 0, offset 0, length 512 bytes). The corresponding
        // block data (keyed by REMOTE_TXT_SEED_SLICE_ID/0) must be
        // written to the ObjectStore at daemon startup - see main.rs.
        let seed_slice = Slice {
            chunk_index: 0,
            slice_id: REMOTE_TXT_SEED_SLICE_ID,
            chunk_offset: 0,
            length: REMOTE_TXT_SIZE as u32,
            written_at: now,
        };

        let mut slices = HashMap::new();
        let mut remote_txt_chunks = HashMap::new();
        remote_txt_chunks.insert(0, vec![seed_slice]);
        slices.insert(REMOTE_TXT_INODE, remote_txt_chunks);

        MemStore {
            inner: RwLock::new(MemStoreInner {
                inodes,
                dir_entries,
                slices,
            }),
            next_inode_id: AtomicU64::new(REMOTE_TXT_INODE + 1),
        }
    }

    /// Allocates and returns a fresh inode id, guaranteed unique
    /// among every id this `MemStore` instance has ever handed out.
    ///
    /// Not yet called by any `MetaStore` trait method - provided
    /// ahead of time for a future `create()`-like addition. `Relaxed`
    /// ordering suffices: this is a pure counter with no other memory
    /// it needs to synchronize against (unlike, for comparison, the
    /// kernel side's `req_id_counter` in `ipc_ring.c`, which also only
    /// needs atomicity, not cross-field ordering - see that file's
    /// module doc comment for the analogous reasoning).
    #[allow(dead_code)]
    fn allocate_inode_id(&self) -> u64 {
        self.next_inode_id.fetch_add(1, Ordering::Relaxed)
    }
}

impl Default for MemStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns the current wall-clock time as Unix epoch seconds, for
/// stamping newly created [`Inode`]s' `mtime`. Isolated into its own
/// function purely so every call site agrees on how "now" is derived
/// (`SystemTime::now()` can in principle be earlier than
/// `UNIX_EPOCH` on a badly misconfigured clock; centralizing the
/// `unwrap_or_default()` fallback here means that edge case is
/// handled once, not duplicated at every construction site).
fn current_unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[async_trait]
impl MetaStore for MemStore {
    async fn lookup(&self, parent: u64, name: &str) -> Result<u64> {
        let inner = self.inner.read().await;

        let parent_inode = inner.inodes.get(&parent).ok_or(MetaError::NotFound)?;
        if !parent_inode.is_dir() {
            return Err(MetaError::NotADirectory);
        }

        inner
            .dir_entries
            .get(&parent)
            .and_then(|entries| entries.get(name))
            .copied()
            .ok_or(MetaError::NotFound)
    }

    async fn getattr(&self, inode: u64) -> Result<Inode> {
        let inner = self.inner.read().await;
        inner.inodes.get(&inode).cloned().ok_or(MetaError::NotFound)
    }

    async fn read_slices(&self, inode: u64, chunk_idx: u32) -> Result<Vec<Slice>> {
        let inner = self.inner.read().await;

        if !inner.inodes.contains_key(&inode) {
            return Err(MetaError::NotFound);
        }

        Ok(inner
            .slices
            .get(&inode)
            .and_then(|chunks| chunks.get(&chunk_idx))
            .cloned()
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_model::S_IFREG;

    #[tokio::test]
    async fn getattr_root_returns_directory() {
        let store = MemStore::new();
        let root = store.getattr(ROOT_INODE).await.expect("root must exist");
        assert!(root.is_dir());
        assert_eq!(root.inode_id, ROOT_INODE);
    }

    #[tokio::test]
    async fn getattr_remote_txt_returns_regular_file_with_seeded_size() {
        let store = MemStore::new();
        let remote = store
            .getattr(REMOTE_TXT_INODE)
            .await
            .expect("remote.txt must exist");
        assert_eq!(remote.mode & S_IFREG, S_IFREG);
        assert_eq!(remote.size, 512);
    }

    #[tokio::test]
    async fn getattr_unknown_inode_is_not_found() {
        let store = MemStore::new();
        let err = store.getattr(9999).await.unwrap_err();
        assert!(matches!(err, MetaError::NotFound));
    }

    #[tokio::test]
    async fn lookup_finds_remote_txt_under_root() {
        let store = MemStore::new();
        let inode_id = store
            .lookup(ROOT_INODE, "remote.txt")
            .await
            .expect("remote.txt must be a child of root");
        assert_eq!(inode_id, REMOTE_TXT_INODE);
    }

    #[tokio::test]
    async fn lookup_unknown_name_is_not_found() {
        let store = MemStore::new();
        let err = store.lookup(ROOT_INODE, "does_not_exist.txt").await.unwrap_err();
        assert!(matches!(err, MetaError::NotFound));
    }

    #[tokio::test]
    async fn lookup_under_nonexistent_parent_is_not_found() {
        let store = MemStore::new();
        let err = store.lookup(9999, "remote.txt").await.unwrap_err();
        assert!(matches!(err, MetaError::NotFound));
    }

    #[tokio::test]
    async fn lookup_under_regular_file_is_not_a_directory() {
        let store = MemStore::new();
        // remote.txt (inode 2) is a regular file, not a directory -
        // looking anything up "inside" it must be rejected distinctly
        // from a plain NotFound.
        let err = store.lookup(REMOTE_TXT_INODE, "anything").await.unwrap_err();
        assert!(matches!(err, MetaError::NotADirectory));
    }

    #[tokio::test]
    async fn read_slices_on_unwritten_chunk_is_empty_not_error() {
        let store = MemStore::new();
        // remote.txt exists and as of Phase 3 step 3, MemStore::new()
        // seeds one Slice for chunk 0. Chunk 1 (and all other chunks)
        // remain unwritten - this must be a valid "sparse hole" empty
        // result, not a NotFound error (see read_slices's doc
        // comment on MetaStore).
        let slices = store
            .read_slices(REMOTE_TXT_INODE, 1)
            .await
            .expect("existing inode with no slices for this chunk must not error");
        assert!(slices.is_empty());
    }

    #[tokio::test]
    async fn read_slices_on_unknown_inode_is_not_found() {
        let store = MemStore::new();
        let err = store.read_slices(9999, 0).await.unwrap_err();
        assert!(matches!(err, MetaError::NotFound));
    }

    #[tokio::test]
    async fn allocate_inode_id_never_repeats_and_skips_seeded_ids() {
        let store = MemStore::new();
        let a = store.allocate_inode_id();
        let b = store.allocate_inode_id();
        assert_ne!(a, b);
        assert!(a > REMOTE_TXT_INODE);
        assert!(b > REMOTE_TXT_INODE);
    }

    #[tokio::test]
    async fn concurrent_getattr_calls_do_not_deadlock_or_corrupt() {
        // Exercises MemStore's RwLock under real concurrent access
        // via multiple tokio tasks - a sanity check that the chosen
        // locking granularity (one RwLock over both tables) behaves
        // correctly under concurrent readers, not just when called
        // sequentially like every other test in this module.
        let store = std::sync::Arc::new(MemStore::new());
        let mut handles = Vec::new();

        for _ in 0..16 {
            let store = store.clone();
            handles.push(tokio::spawn(async move {
                store.getattr(ROOT_INODE).await.expect("root must exist")
            }));
        }

        for handle in handles {
            let inode = handle.await.expect("task must not panic");
            assert_eq!(inode.inode_id, ROOT_INODE);
        }
    }
}
