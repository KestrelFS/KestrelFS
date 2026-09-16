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

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::fs_model::{Inode, Slice, MODE_PERMISSIONS_MASK, ROOT_INODE, SYMLINK_TARGET_MAX};

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
    #[error("invalid name: {0}")]
    InvalidName(String),
    /// `create()` was called but the parent directory already has a
    /// child with the requested name.
    #[error("file already exists")]
    AlreadyExists,
    /// `unlink()` was called on a non-empty directory.
    #[error("directory not empty")]
    NotEmpty,
    /// `readlink()` was called for an inode whose type is not `S_IFLNK`.
    #[error("inode is not a symbolic link")]
    NotASymlink,
    /// Hard links to directories are forbidden so the namespace remains a tree.
    #[error("hard links to directories are not permitted")]
    IsADirectory,
    /// Incrementing the inode's persistent link count would overflow.
    #[error("too many hard links")]
    TooManyLinks,
    /// A rename flag or flag combination is not supported.
    #[error("unsupported rename flags: {0:#x}")]
    UnsupportedRenameFlags(u32),
    /// I/O error during persistence operations (FileMetaStore).
    #[error("I/O error")]
    Io,
}

/// Convenience alias, matching the `Result<T>` naming used throughout
/// the rest of the daemon's modules (e.g. `std::io::Result`).
pub type Result<T> = std::result::Result<T, MetaError>;

/// Atomic rename must fail with [`MetaError::AlreadyExists`] when the target
/// exists. This value intentionally matches Linux `RENAME_NOREPLACE`.
pub const RENAME_NOREPLACE: u32 = 1;
/// Atomically swap two existing directory entries. This value intentionally
/// matches Linux `RENAME_EXCHANGE`.
pub const RENAME_EXCHANGE: u32 = 2;

/// Result of probing a shared MetaStore for cache-coherence changes after a
/// previously observed durable revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoherenceProbe {
    /// This backend is process-local and needs no remote coherence polling.
    Disabled,
    /// No mutation occurred after the supplied revision.
    Unchanged { revision: u64 },
    /// Every intervening mutation was enumerated and these inode caches must
    /// be retired before advancing the caller's revision cursor.
    Inodes { revision: u64, inode_ids: Vec<u64> },
    /// The intervening dirty set could not be enumerated safely; retire the
    /// complete local cache before advancing the revision cursor.
    Full { revision: u64 },
}

impl CoherenceProbe {
    pub fn revision(&self) -> Option<u64> {
        match self {
            Self::Disabled => None,
            Self::Unchanged { revision }
            | Self::Inodes { revision, .. }
            | Self::Full { revision } => Some(*revision),
        }
    }
}

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

    /// Atomically updates any requested ownership/mode fields and returns the
    /// authoritative inode. File type bits are preserved when mode is set.
    async fn set_attrs(
        &self,
        inode: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        atime: Option<u64>,
        mtime: Option<u64>,
    ) -> Result<Inode>;

    /// Convenience wrapper retained for callers which only change mode.
    #[allow(dead_code)]
    async fn set_mode(&self, inode: u64, mode: u32) -> Result<u32> {
        Ok(self
            .set_attrs(inode, Some(mode), None, None, None, None)
            .await?
            .mode)
    }

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

    /// Creates a new regular file under `parent`. The operation forces the
    /// file type and preserves `mode & 0o7777`. Returns its inode id.
    ///
    /// # Errors
    ///
    /// - [`MetaError::NotFound`] if `parent` does not exist.
    /// - [`MetaError::NotADirectory`] if `parent` is not a directory.
    /// - [`MetaError::AlreadyExists`] if `parent` already has a child named `name`.
    /// - [`MetaError::InvalidName`] if `name` is empty, contains '/', or is too long.
    async fn create(&self, parent: u64, name: &str, mode: u32) -> Result<u64>;

    /// Creates a symbolic link. The target is metadata and is never written to
    /// ObjectStore. Dangling and relative targets are both valid.
    async fn symlink(&self, parent: u64, name: &str, target: &str) -> Result<u64>;

    /// Returns a symbolic link's target string without resolving it.
    async fn readlink(&self, inode: u64) -> Result<String>;

    /// Appends a new [`Slice`] to the given inode's slice list for the
    /// specified chunk. Does not remove or modify existing slices (COW).
    ///
    /// Also updates the inode's `size` to max(current_size, slice_end_offset)
    /// and `mtime` to the current time.
    ///
    /// # Errors
    ///
    /// [`MetaError::NotFound`] if `inode` does not exist.
    async fn append_slice(&self, inode: u64, slice: Slice) -> Result<()>;

    /// Sets the file size to `new_size`, updating mtime.
    ///
    /// This implements POSIX truncate/ftruncate semantics:
    /// - If `new_size < current_size`, the file is shrunk (logically).
    /// - If `new_size > current_size`, the file is extended with zeros.
    ///
    /// Slices wholly beyond the retained EOF are removed; a crossing slice is
    /// shortened so later extension cannot expose discarded bytes. Returns
    /// block keys confirmed unreferenced after the mutation.
    ///
    /// Returns [`MetaError::NotFound`] if `inode` does not exist.
    async fn truncate(&self, inode: u64, new_size: u64) -> Result<Vec<String>>;

    /// Lists directory entries for the given directory inode.
    ///
    /// Returns a vector of `(child_inode_id, name)` pairs. The order is
    /// unspecified (implementation-defined). An empty vector is valid for
    /// an empty directory.
    ///
    /// # Errors
    ///
    /// - [`MetaError::NotFound`] if `inode` does not exist.
    /// - [`MetaError::NotADirectory`] if `inode` is not a directory.
    async fn readdir(&self, inode: u64) -> Result<Vec<(u64, String)>>;

    /// Creates a new directory under `parent`, forcing the directory type and
    /// preserving `mode & 0o7777`. The parent's directory link count grows by
    /// one. Returns the newly allocated inode id.
    ///
    /// # Errors
    ///
    /// - [`MetaError::NotFound`] if `parent` does not exist.
    /// - [`MetaError::NotADirectory`] if `parent` is not a directory.
    /// - [`MetaError::AlreadyExists`] if a child named `name` already exists.
    /// - [`MetaError::InvalidName`] if `name` is empty, contains '/', or > 255 bytes.
    async fn mkdir(&self, parent: u64, name: &str, mode: u32) -> Result<u64>;

    /// Adds `name` in `parent` as another directory entry for `inode` and
    /// returns the atomically updated persistent link count. Directory hard
    /// links are rejected. No ObjectStore reference is created or copied.
    async fn link(&self, parent: u64, name: &str, inode: u64) -> Result<u32>;

    /// Removes a file or empty directory from `parent` directory.
    ///
    /// For non-directories: removes one dirent and decrements `nlink`; only the
    /// final reference removes the inode, slices, and symlink target.
    /// For directories: only succeeds if empty and decrements the parent's
    /// directory link count.
    ///
    /// # Errors
    ///
    /// - [`MetaError::NotFound`] if `parent` does not exist or `name` not found.
    /// - [`MetaError::NotADirectory`] if `parent` is not a directory.
    /// - [`MetaError::InvalidName`] if attempting to unlink "." or "..".
    /// Returns block keys confirmed unreferenced after removal.
    #[allow(dead_code)]
    async fn unlink(&self, parent: u64, name: &str) -> Result<Vec<String>>;

    /// Removes a dirent while optionally retaining a final-link inode as an
    /// orphan. The retained inode remains addressable by id with `nlink=0`.
    async fn unlink_with_lifecycle(
        &self,
        parent: u64,
        name: &str,
        defer_reclaim: bool,
    ) -> Result<Vec<String>>;

    /// Reclaims a retained non-directory orphan and durably queues its GC keys.
    async fn finalize_orphan(&self, inode: u64) -> Result<Vec<String>>;

    /// Renames/moves a file or directory from `(old_parent, old_name)` to `(new_parent, new_name)`.
    ///
    /// Supports:
    /// - Same directory rename: `old_parent == new_parent`, `old_name != new_name`
    /// - Cross-directory move: `old_parent != new_parent`
    /// - Atomic replacement: if `new_name` exists as a regular file, it is replaced (POSIX semantics)
    ///
    /// # Errors
    ///
    /// - [`MetaError::NotFound`] if source does not exist or parent directories missing
    /// - [`MetaError::NotADirectory`] if either parent is not a directory
    /// - [`MetaError::AlreadyExists`] if NOREPLACE is set and a different target exists
    /// - [`MetaError::InvalidName`] if attempting to rename "." or ".."
    /// - [`MetaError::NotEmpty`] if target is a non-empty directory
    ///
    /// Implementation must prevent renaming a directory into its own subtree.
    /// On replacement, returns block keys confirmed unreferenced afterward.
    #[allow(dead_code)]
    async fn rename(
        &self,
        old_parent: u64,
        old_name: &str,
        new_parent: u64,
        new_name: &str,
    ) -> Result<Vec<String>>;

    /// Rename with Linux-compatible flags. [`RENAME_NOREPLACE`] and
    /// [`RENAME_EXCHANGE`] are individually supported and mutually exclusive;
    /// implementations must perform all checks and dirent changes in one
    /// atomic metadata mutation.
    async fn rename_with_flags(
        &self,
        old_parent: u64,
        old_name: &str,
        new_parent: u64,
        new_name: &str,
        flags: u32,
    ) -> Result<Vec<String>>;

    /// Rename with an explicit lifecycle decision for an overwritten final
    /// target link. When deferred, the target survives at nlink zero.
    async fn rename_with_lifecycle(
        &self,
        old_parent: u64,
        old_name: &str,
        new_parent: u64,
        new_name: &str,
        flags: u32,
        defer_reclaim: bool,
    ) -> Result<Vec<String>>;

    /// Returns durable GC candidates that are not referenced by any current
    /// slice. Implementations must commit candidates in the same metadata
    /// transaction that removes their final reference.
    async fn pending_garbage(&self) -> Result<Vec<String>>;

    /// Removes successfully deleted object keys from the durable GC queue.
    /// Repeated acknowledgements are valid so ObjectStore delete + queue ack
    /// form an at-least-once, crash-safe protocol.
    async fn acknowledge_garbage(&self, keys: &[String]) -> Result<()>;

    /// Reports shared-backend mutations after `observed_revision`. Process-
    /// local implementations return [`CoherenceProbe::Disabled`]. A shared
    /// backend must return [`CoherenceProbe::Full`] whenever it cannot prove
    /// that its inode list covers every intervening mutation.
    async fn coherence_probe(&self, _observed_revision: Option<u64>) -> Result<CoherenceProbe> {
        Ok(CoherenceProbe::Disabled)
    }
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
    pub(crate) inner: RwLock<MemStoreInner>,
    /// Source of fresh inode ids for any future `create`-like
    /// operation. Not yet used by any trait method (`MemStore` today
    /// only ever seeds its two hardcoded entries at construction
    /// time via [`MemStore::new`]), but kept ready so a future
    /// `create()` addition to `MetaStore` has an obvious,
    /// already-in-place allocator to call rather than needing to
    /// retrofit one. A plain `AtomicU64` (not behind the `RwLock`)
    /// since id allocation is independent of the inode/dirent tables'
    /// own consistency.
    pub(crate) next_inode_id: AtomicU64,
}

/// The actual mutable state behind `MemStore`'s `RwLock`, split out
/// as its own struct purely so `MemStore`'s single `RwLock<..>` field
/// declaration stays readable rather than becoming a `RwLock<(HashMap<..>, HashMap<..>)>`
/// tuple.
pub(crate) struct MemStoreInner {
    pub(crate) inodes: HashMap<u64, Inode>,
    /// `parent_inode_id -> (child_name -> child_inode_id)`.
    pub(crate) dir_entries: HashMap<u64, DirEntries>,
    /// `inode_id -> (chunk_index -> slices)`. Grouping slices by
    /// chunk index up front (rather than storing one flat
    /// `Vec<Slice>` per inode and filtering by `chunk_index` on every
    /// `read_slices` call) means `read_slices` is a direct double
    /// `HashMap` lookup with no scanning - the same access pattern a
    /// real KV store (Redis key `slices:{inode}:{chunk_idx}`) would
    /// naturally have too.
    pub(crate) slices: HashMap<u64, HashMap<u32, Vec<Slice>>>,
    /// Symbolic-link inode id -> uninterpreted UTF-8 target string.
    pub(crate) symlink_targets: HashMap<u64, String>,
    /// Object keys made unreachable by a committed metadata mutation but not
    /// yet acknowledged as deleted from ObjectStore.
    pub(crate) pending_garbage: HashSet<String>,
}

fn slice_block_keys<'a>(slices: impl Iterator<Item = &'a Slice>) -> HashSet<String> {
    slices
        .flat_map(|slice| (0..slice.block_count()).map(|index| slice.block_key(index)))
        .collect()
}

fn referenced_block_keys(inner: &MemStoreInner) -> HashSet<String> {
    slice_block_keys(
        inner
            .slices
            .values()
            .flat_map(|chunks| chunks.values())
            .flat_map(|slices| slices.iter()),
    )
}

/// Filters mutation candidates against every slice that remains in metadata.
/// This deliberately tolerates duplicate/shared slice ids: a key is returned
/// only when no surviving slice references it.
fn confirmed_garbage_keys(
    inner: &MemStoreInner,
    candidates: HashSet<String>,
) -> Vec<String> {
    let referenced = referenced_block_keys(inner);
    let mut garbage: Vec<_> = candidates.difference(&referenced).cloned().collect();
    garbage.sort();
    garbage
}

/// Confirms candidates against all live slices and durably associates them
/// with the metadata mutation before the mutation's caller can observe
/// success.
fn enqueue_confirmed_garbage(
    inner: &mut MemStoreInner,
    candidates: HashSet<String>,
) -> Vec<String> {
    let garbage = confirmed_garbage_keys(inner, candidates);
    inner.pending_garbage.extend(garbage.iter().cloned());
    garbage
}

/// Applies an immediate-subdirectory delta to a directory's POSIX link count.
/// A valid directory never drops below the `.`/`..` baseline of two links.
fn directory_nlink_after_delta(current: u32, delta: i32) -> Result<u32> {
    let next = if delta >= 0 {
        current
            .checked_add(delta as u32)
            .ok_or(MetaError::TooManyLinks)?
    } else {
        current
            .checked_sub(delta.unsigned_abs())
            .ok_or(MetaError::Io)?
    };
    if next < 2 {
        return Err(MetaError::Io);
    }
    Ok(next)
}

/// Returns true when moving `directory` below `destination_parent` would form
/// a cycle. Directory hard links are forbidden, so scanning parent dirents is
/// sufficient to walk the unique ancestry chain.
fn directory_move_creates_cycle(
    inner: &MemStoreInner,
    directory: u64,
    destination_parent: u64,
) -> bool {
    let mut check_parent = destination_parent;
    loop {
        if check_parent == directory {
            return true;
        }
        if check_parent == ROOT_INODE {
            return false;
        }
        let found_parent = inner.dir_entries.iter().find_map(|(&dir_ino, entries)| {
            entries
                .values()
                .any(|&child| child == check_parent)
                .then_some(dir_ino)
        });
        match found_parent {
            Some(parent) => check_parent = parent,
            None => return false,
        }
    }
}

/// Fixed inode id for the bootstrap `remote.txt` entry seeded by
/// [`MemStore::new`]. Previously 2 (which collided with the root
/// directory in the kernel's tree_descr array indexing - see
/// kestrelfs/inode.c), now 3 as of Phase 3 step 3 (tree_descr places
/// remote.txt at index [3] to match).
pub const REMOTE_TXT_INODE: u64 = 3;

/// Inode number for `/writable.dat`, matching the kernel module's
/// `tree_descr[4]` entry. This file supports both read and write
/// operations via the IPC write path (Phase 3 step 4).
pub const WRITABLE_DAT_INODE: u64 = 4;

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
    /// Builds a new `MemStore`, pre-seeded with exactly three entries:
    ///
    /// - Inode [`ROOT_INODE`] (1): the root directory `/`.
    /// - Inode [`REMOTE_TXT_INODE`] (3): a regular file named
    ///   `remote.txt`, linked as a child of the root directory.
    /// - Inode [`WRITABLE_DAT_INODE`] (4): a regular file named
    ///   `writable.dat`, linked as a child of the root directory.
    ///
    /// This matches the kernel side's `tree_descr` table (see
    /// `kestrelfs/inode.c`), which places remote.txt at index [3] and
    /// writable.dat at index [4]. As of Phase 3 step 4, LOOKUP/GETATTR/
    /// READ_CHUNK/WRITE_CHUNK requests flow from kernel → daemon →
    /// MetaStore/ObjectStore, enabling full read/write functionality.
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

        // writable.dat starts empty (size 0) and grows via write operations
        let writable_dat = Inode::new_file(WRITABLE_DAT_INODE, 0, now);

        let mut inodes = HashMap::new();
        inodes.insert(ROOT_INODE, root);
        inodes.insert(REMOTE_TXT_INODE, remote_txt);
        inodes.insert(WRITABLE_DAT_INODE, writable_dat);

        let mut root_entries = DirEntries::new();
        root_entries.insert("remote.txt".to_string(), REMOTE_TXT_INODE);
        root_entries.insert("writable.dat".to_string(), WRITABLE_DAT_INODE);

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
                symlink_targets: HashMap::new(),
                pending_garbage: HashSet::new(),
            }),
            next_inode_id: AtomicU64::new(WRITABLE_DAT_INODE + 1),
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

    /// Test-only helper: directly inserts an inode, bypassing normal
    /// create() logic. Used by unit tests that need to set up specific
    /// inode states without going through the full MetaStore API.
    #[cfg(test)]
    pub(crate) async fn insert_inode_for_test(&self, inode_id: u64, inode: Inode) {
        self.inner.write().await.inodes.insert(inode_id, inode);
    }

    /// Test-only helper: directly inserts slices for a given inode,
    /// bypassing append_slice(). Used by read_from_slices tests that
    /// need to construct specific overlap scenarios.
    #[cfg(test)]
    pub(crate) async fn insert_slices_for_test(
        &self,
        inode_id: u64,
        chunk_index: u32,
        slices: Vec<Slice>,
    ) {
        self.inner
            .write()
            .await
            .slices
            .entry(inode_id)
            .or_insert_with(HashMap::new)
            .insert(chunk_index, slices);
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
pub fn current_unix_time() -> u64 {
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

    async fn set_attrs(
        &self,
        inode: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        atime: Option<u64>,
        mtime: Option<u64>,
    ) -> Result<Inode> {
        let mut inner = self.inner.write().await;
        let inode_meta = inner.inodes.get_mut(&inode).ok_or(MetaError::NotFound)?;
        if let Some(mode) = mode {
            inode_meta.mode = (inode_meta.mode & !MODE_PERMISSIONS_MASK)
                | (mode & MODE_PERMISSIONS_MASK);
        }
        if let Some(uid) = uid {
            inode_meta.uid = uid;
        }
        if let Some(gid) = gid {
            inode_meta.gid = gid;
        }
        if let Some(atime) = atime {
            inode_meta.atime = atime;
        }
        if let Some(mtime) = mtime {
            inode_meta.mtime = mtime;
        }
        Ok(inode_meta.clone())
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

    async fn create(&self, parent: u64, name: &str, mode: u32) -> Result<u64> {
        // Validate name
        if name.is_empty() || name.contains('/') {
            return Err(MetaError::InvalidName(format!("invalid name: {}", name)));
        }
        if name.len() > 255 {
            return Err(MetaError::InvalidName(format!(
                "name too long: {}",
                name.len()
            )));
        }

        let mut inner = self.inner.write().await;

        // Check parent exists and is a directory
        let parent_inode = inner.inodes.get(&parent).ok_or(MetaError::NotFound)?;
        if !parent_inode.is_dir() {
            return Err(MetaError::NotADirectory);
        }

        // Check if name already exists
        if let Some(entries) = inner.dir_entries.get(&parent) {
            if entries.contains_key(name) {
                return Err(MetaError::AlreadyExists);
            }
        }

        // Allocate new inode
        let new_inode_id = self.allocate_inode_id();
        let now = current_unix_time();
        let new_inode = Inode::new_file_with_mode(new_inode_id, 0, now, mode);

        // Insert inode and directory entry
        inner.inodes.insert(new_inode_id, new_inode);
        inner
            .dir_entries
            .entry(parent)
            .or_insert_with(HashMap::new)
            .insert(name.to_string(), new_inode_id);

        Ok(new_inode_id)
    }

    async fn symlink(&self, parent: u64, name: &str, target: &str) -> Result<u64> {
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return Err(MetaError::InvalidName(format!("invalid name: {name}")));
        }
        if name.len() > 255 {
            return Err(MetaError::InvalidName(format!(
                "name too long: {}",
                name.len()
            )));
        }
        if target.is_empty() || target.len() > SYMLINK_TARGET_MAX {
            return Err(MetaError::InvalidName(format!(
                "invalid symlink target length: {}",
                target.len()
            )));
        }

        let mut inner = self.inner.write().await;
        let parent_inode = inner.inodes.get(&parent).ok_or(MetaError::NotFound)?;
        if !parent_inode.is_dir() {
            return Err(MetaError::NotADirectory);
        }
        if inner
            .dir_entries
            .get(&parent)
            .is_some_and(|entries| entries.contains_key(name))
        {
            return Err(MetaError::AlreadyExists);
        }

        let new_inode_id = self.allocate_inode_id();
        inner.inodes.insert(
            new_inode_id,
            Inode::new_symlink(new_inode_id, target.len(), current_unix_time()),
        );
        inner
            .dir_entries
            .entry(parent)
            .or_insert_with(HashMap::new)
            .insert(name.to_string(), new_inode_id);
        inner
            .symlink_targets
            .insert(new_inode_id, target.to_string());
        Ok(new_inode_id)
    }

    async fn readlink(&self, inode: u64) -> Result<String> {
        let inner = self.inner.read().await;
        let inode_meta = inner.inodes.get(&inode).ok_or(MetaError::NotFound)?;
        if !inode_meta.is_symlink() {
            return Err(MetaError::NotASymlink);
        }
        inner
            .symlink_targets
            .get(&inode)
            .cloned()
            .ok_or(MetaError::Io)
    }

    async fn append_slice(&self, inode: u64, slice: Slice) -> Result<()> {
        let mut inner = self.inner.write().await;

        // Check inode exists
        let inode_meta = inner.inodes.get_mut(&inode).ok_or(MetaError::NotFound)?;

        // Update inode size if this slice extends it
        let slice_end = slice.chunk_index as u64 * crate::fs_model::CHUNK_SIZE
            + slice.chunk_offset as u64
            + slice.length as u64;
        if slice_end > inode_meta.size {
            inode_meta.size = slice_end;
        }

        // Update mtime
        inode_meta.mtime = current_unix_time();

        // Append slice to the chunk's slice list
        inner
            .slices
            .entry(inode)
            .or_insert_with(HashMap::new)
            .entry(slice.chunk_index)
            .or_insert_with(Vec::new)
            .push(slice);

        Ok(())
    }

    async fn truncate(&self, inode: u64, new_size: u64) -> Result<Vec<String>> {
        let mut inner = self.inner.write().await;

        let old_size = inner
            .inodes
            .get(&inode)
            .ok_or(MetaError::NotFound)?
            .size;
        let candidates = inner
            .slices
            .get(&inode)
            .map(|chunks| {
                slice_block_keys(chunks.values().flat_map(|slices| slices.iter()))
            })
            .unwrap_or_default();

        // Data above the old EOF is never logically valid, including when an
        // older daemon retained stale slices and this call extends the file.
        let retained_size = old_size.min(new_size);
        if let Some(chunks) = inner.slices.get_mut(&inode) {
            chunks.retain(|_, slices| {
                slices.retain_mut(|slice| {
                    let start = slice.chunk_index as u64 * crate::fs_model::CHUNK_SIZE
                        + slice.chunk_offset as u64;
                    if start >= retained_size {
                        return false;
                    }
                    let retained_len = retained_size - start;
                    if slice.length as u64 > retained_len {
                        slice.length = retained_len as u32;
                    }
                    slice.length != 0
                });
                !slices.is_empty()
            });
        }

        let inode_meta = inner.inodes.get_mut(&inode).ok_or(MetaError::NotFound)?;
        inode_meta.size = new_size;
        inode_meta.mtime = current_unix_time();

        Ok(enqueue_confirmed_garbage(&mut inner, candidates))
    }

    async fn readdir(&self, inode: u64) -> Result<Vec<(u64, String)>> {
        let inner = self.inner.read().await;

        // Check inode exists
        let inode_meta = inner.inodes.get(&inode).ok_or(MetaError::NotFound)?;

        // Check it's a directory
        if !inode_meta.is_dir() {
            return Err(MetaError::NotADirectory);
        }

        // Get directory entries (may be empty for empty directory)
        let entries = inner
            .dir_entries
            .get(&inode)
            .map(|children| {
                children
                    .iter()
                    .map(|(name, &child_inode)| (child_inode, name.clone()))
                    .collect()
            })
            .unwrap_or_default();

        Ok(entries)
    }

    async fn mkdir(&self, parent: u64, name: &str, mode: u32) -> Result<u64> {
        // Validate name
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return Err(MetaError::InvalidName(format!("invalid name: {}", name)));
        }
        if name.len() > 255 {
            return Err(MetaError::InvalidName(format!(
                "name too long: {}",
                name.len()
            )));
        }

        let mut inner = self.inner.write().await;

        // Check parent exists and is a directory
        let parent_inode = inner.inodes.get(&parent).ok_or(MetaError::NotFound)?;
        if !parent_inode.is_dir() {
            return Err(MetaError::NotADirectory);
        }
        let parent_nlink = parent_inode
            .nlink
            .checked_add(1)
            .ok_or(MetaError::TooManyLinks)?;

        // Check if name already exists
        if let Some(entries) = inner.dir_entries.get(&parent) {
            if entries.contains_key(name) {
                return Err(MetaError::AlreadyExists);
            }
        }

        // Allocate new inode
        let new_inode_id = self.allocate_inode_id();
        let now = current_unix_time();
        let new_inode = Inode::new_dir_with_mode(new_inode_id, now, mode);

        // Insert inode and directory entry
        inner.inodes.insert(new_inode_id, new_inode);
        inner
            .dir_entries
            .entry(parent)
            .or_insert_with(HashMap::new)
            .insert(name.to_string(), new_inode_id);

        // Initialize empty dir_entries for the new directory
        inner.dir_entries.insert(new_inode_id, HashMap::new());
        if let Some(parent_inode) = inner.inodes.get_mut(&parent) {
            parent_inode.nlink = parent_nlink;
            parent_inode.mtime = now;
        }

        Ok(new_inode_id)
    }

    async fn link(&self, parent: u64, name: &str, inode: u64) -> Result<u32> {
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return Err(MetaError::InvalidName(format!("invalid name: {name}")));
        }
        if name.len() > 255 {
            return Err(MetaError::InvalidName(format!(
                "name too long: {}",
                name.len()
            )));
        }

        let mut inner = self.inner.write().await;
        let parent_inode = inner.inodes.get(&parent).ok_or(MetaError::NotFound)?;
        if !parent_inode.is_dir() {
            return Err(MetaError::NotADirectory);
        }
        if inner
            .dir_entries
            .get(&parent)
            .is_some_and(|entries| entries.contains_key(name))
        {
            return Err(MetaError::AlreadyExists);
        }

        let target = inner.inodes.get(&inode).ok_or(MetaError::NotFound)?;
        if target.is_dir() {
            return Err(MetaError::IsADirectory);
        }
        let new_nlink = target
            .nlink
            .checked_add(1)
            .ok_or(MetaError::TooManyLinks)?;

        inner
            .dir_entries
            .entry(parent)
            .or_insert_with(HashMap::new)
            .insert(name.to_owned(), inode);
        let now = current_unix_time();
        if let Some(target) = inner.inodes.get_mut(&inode) {
            target.nlink = new_nlink;
        }
        if let Some(parent_inode) = inner.inodes.get_mut(&parent) {
            parent_inode.mtime = now;
        }
        Ok(new_nlink)
    }

    async fn unlink(&self, parent: u64, name: &str) -> Result<Vec<String>> {
        self.unlink_with_lifecycle(parent, name, false).await
    }

    async fn unlink_with_lifecycle(
        &self,
        parent: u64,
        name: &str,
        defer_reclaim: bool,
    ) -> Result<Vec<String>> {
        // Validate name
        if name.is_empty() || name == "." || name == ".." {
            return Err(MetaError::InvalidName(format!("invalid name: {}", name)));
        }

        let mut inner = self.inner.write().await;

        // Check parent exists and is a directory
        let parent_inode = inner.inodes.get(&parent).ok_or(MetaError::NotFound)?;
        if !parent_inode.is_dir() {
            return Err(MetaError::NotADirectory);
        }
        let parent_nlink = parent_inode.nlink;

        // Find the child in parent's directory entries
        let child_inode_id = inner
            .dir_entries
            .get(&parent)
            .and_then(|entries| entries.get(name).copied())
            .ok_or(MetaError::NotFound)?;

        // Snapshot the fields needed below so subsequent mutations do not
        // retain an immutable borrow into the inode table.
        let child_inode = inner
            .inodes
            .get(&child_inode_id)
            .ok_or(MetaError::NotFound)?;
        let child_is_dir = child_inode.is_dir();
        let child_nlink = child_inode.nlink;
        let parent_nlink_after = if child_is_dir {
            Some(
                parent_nlink
                    .checked_sub(1)
                    .filter(|nlink| *nlink >= 2)
                    .ok_or(MetaError::Io)?,
            )
        } else {
            None
        };

        // If it's a directory, check if it's empty
        if child_is_dir {
            if let Some(child_entries) = inner.dir_entries.get(&child_inode_id) {
                if !child_entries.is_empty() {
                    return Err(MetaError::NotEmpty);
                }
            }
            // Remove empty directory's dir_entries entry
            inner.dir_entries.remove(&child_inode_id);
        }

        // Remove from parent's directory entries
        if let Some(entries) = inner.dir_entries.get_mut(&parent) {
            entries.remove(name);
        }

        // A non-final hard-link removal only drops one name and one reference.
        // The inode, its slices/symlink target, and every ObjectStore key stay
        // live until the final directory entry disappears.
        if !child_is_dir && child_nlink > 1 {
            if let Some(child_inode) = inner.inodes.get_mut(&child_inode_id) {
                child_inode.nlink -= 1;
            }
            return Ok(Vec::new());
        }

        if !child_is_dir && defer_reclaim {
            if let Some(child_inode) = inner.inodes.get_mut(&child_inode_id) {
                child_inode.nlink = 0;
            }
            return Ok(Vec::new());
        }

        // Remove the inode itself
        inner.inodes.remove(&child_inode_id);

        // Remove any slices associated with this inode (for files)
        let candidates = inner
            .slices
            .remove(&child_inode_id)
            .map(|chunks| {
                slice_block_keys(chunks.values().flat_map(|slices| slices.iter()))
            })
            .unwrap_or_default();
        inner.symlink_targets.remove(&child_inode_id);

        if let Some(parent_nlink_after) = parent_nlink_after {
            if let Some(parent_inode) = inner.inodes.get_mut(&parent) {
                parent_inode.nlink = parent_nlink_after;
                parent_inode.mtime = current_unix_time();
            }
        }

        Ok(enqueue_confirmed_garbage(&mut inner, candidates))
    }

    async fn finalize_orphan(&self, inode: u64) -> Result<Vec<String>> {
        let mut inner = self.inner.write().await;
        let target = inner.inodes.get(&inode).ok_or(MetaError::NotFound)?;
        if target.is_dir() || target.nlink != 0 {
            return Err(MetaError::Io);
        }

        inner.inodes.remove(&inode);
        let candidates = inner
            .slices
            .remove(&inode)
            .map(|chunks| {
                slice_block_keys(chunks.values().flat_map(|slices| slices.iter()))
            })
            .unwrap_or_default();
        inner.symlink_targets.remove(&inode);
        Ok(enqueue_confirmed_garbage(&mut inner, candidates))
    }

    async fn rename(
        &self,
        old_parent: u64,
        old_name: &str,
        new_parent: u64,
        new_name: &str,
    ) -> Result<Vec<String>> {
        self.rename_with_flags(old_parent, old_name, new_parent, new_name, 0)
            .await
    }

    async fn rename_with_flags(
        &self,
        old_parent: u64,
        old_name: &str,
        new_parent: u64,
        new_name: &str,
        flags: u32,
    ) -> Result<Vec<String>> {
        self.rename_with_lifecycle(
            old_parent,
            old_name,
            new_parent,
            new_name,
            flags,
            false,
        )
        .await
    }

    async fn rename_with_lifecycle(
        &self,
        old_parent: u64,
        old_name: &str,
        new_parent: u64,
        new_name: &str,
        flags: u32,
        defer_reclaim: bool,
    ) -> Result<Vec<String>> {
        if flags & !(RENAME_NOREPLACE | RENAME_EXCHANGE) != 0
            || flags == (RENAME_NOREPLACE | RENAME_EXCHANGE)
        {
            return Err(MetaError::UnsupportedRenameFlags(flags));
        }

        // Validate names
        if old_name.is_empty() || old_name == "." || old_name == ".." {
            return Err(MetaError::InvalidName(format!("invalid old_name: {}", old_name)));
        }
        if new_name.is_empty() || new_name == "." || new_name == ".." {
            return Err(MetaError::InvalidName(format!("invalid new_name: {}", new_name)));
        }

        let mut inner = self.inner.write().await;

        // Check old_parent exists and is a directory
        let old_parent_inode = inner.inodes.get(&old_parent).ok_or(MetaError::NotFound)?;
        if !old_parent_inode.is_dir() {
            return Err(MetaError::NotADirectory);
        }

        // Check new_parent exists and is a directory
        let new_parent_inode = inner.inodes.get(&new_parent).ok_or(MetaError::NotFound)?;
        if !new_parent_inode.is_dir() {
            return Err(MetaError::NotADirectory);
        }

        // Find source inode
        let source_inode_id = inner
            .dir_entries
            .get(&old_parent)
            .and_then(|entries| entries.get(old_name).copied())
            .ok_or(MetaError::NotFound)?;

        if old_parent == new_parent && old_name == new_name {
            return Ok(Vec::new());
        }

        let source_inode = inner
            .inodes
            .get(&source_inode_id)
            .ok_or(MetaError::NotFound)?;
        let source_is_dir = source_inode.is_dir();

        // Check if we're renaming a directory into its own subtree
        if source_is_dir
            && directory_move_creates_cycle(&inner, source_inode_id, new_parent)
        {
            return Err(MetaError::InvalidName(
                "cannot move directory into its own subtree".to_string(),
            ));
        }

        // Check if target exists
        let target_exists = inner
            .dir_entries
            .get(&new_parent)
            .and_then(|entries| entries.get(new_name).copied());

        if flags & RENAME_EXCHANGE != 0 {
            let target_inode_id = target_exists.ok_or(MetaError::NotFound)?;
            if target_inode_id == source_inode_id {
                return Ok(Vec::new());
            }
            let target_is_dir = inner
                .inodes
                .get(&target_inode_id)
                .ok_or(MetaError::NotFound)?
                .is_dir();
            if target_is_dir
                && directory_move_creates_cycle(&inner, target_inode_id, old_parent)
            {
                return Err(MetaError::InvalidName(
                    "cannot move directory into its own subtree".to_string(),
                ));
            }

            let old_parent_nlink = inner
                .inodes
                .get(&old_parent)
                .ok_or(MetaError::NotFound)?
                .nlink;
            let new_parent_nlink = inner
                .inodes
                .get(&new_parent)
                .ok_or(MetaError::NotFound)?
                .nlink;
            let (old_parent_nlink_after, new_parent_nlink_after) =
                if old_parent == new_parent {
                    (old_parent_nlink, None)
                } else {
                    (
                        directory_nlink_after_delta(
                            old_parent_nlink,
                            target_is_dir as i32 - source_is_dir as i32,
                        )?,
                        Some(directory_nlink_after_delta(
                            new_parent_nlink,
                            source_is_dir as i32 - target_is_dir as i32,
                        )?),
                    )
                };

            inner
                .dir_entries
                .get_mut(&old_parent)
                .expect("source parent entries checked above")
                .insert(old_name.to_string(), target_inode_id);
            inner
                .dir_entries
                .get_mut(&new_parent)
                .expect("target parent entries checked above")
                .insert(new_name.to_string(), source_inode_id);

            let now = current_unix_time();
            if let Some(parent_inode) = inner.inodes.get_mut(&old_parent) {
                parent_inode.mtime = now;
                parent_inode.nlink = old_parent_nlink_after;
            }
            if old_parent != new_parent {
                if let Some(parent_inode) = inner.inodes.get_mut(&new_parent) {
                    parent_inode.mtime = now;
                    parent_inode.nlink = new_parent_nlink_after
                        .expect("cross-directory exchange computed new parent nlink");
                }
            }
            return Ok(Vec::new());
        }

        if target_exists == Some(source_inode_id) {
            return Ok(Vec::new());
        }
        if target_exists.is_some() && flags & RENAME_NOREPLACE != 0 {
            return Err(MetaError::AlreadyExists);
        }

        let target_is_dir = if let Some(target_inode_id) = target_exists {
            let target_inode = inner
                .inodes
                .get(&target_inode_id)
                .ok_or(MetaError::NotFound)?;
            if target_inode.is_dir()
                && inner
                    .dir_entries
                    .get(&target_inode_id)
                    .is_some_and(|entries| !entries.is_empty())
            {
                return Err(MetaError::NotEmpty);
            }
            target_inode.is_dir()
        } else {
            false
        };

        let old_parent_nlink = inner
            .inodes
            .get(&old_parent)
            .ok_or(MetaError::NotFound)?
            .nlink;
        let new_parent_nlink = inner
            .inodes
            .get(&new_parent)
            .ok_or(MetaError::NotFound)?
            .nlink;
        let (old_parent_nlink_after, new_parent_nlink_after) = if old_parent == new_parent {
            (
                directory_nlink_after_delta(old_parent_nlink, -(target_is_dir as i32))?,
                None,
            )
        } else {
            (
                directory_nlink_after_delta(old_parent_nlink, -(source_is_dir as i32))?,
                Some(directory_nlink_after_delta(
                    new_parent_nlink,
                    source_is_dir as i32 - target_is_dir as i32,
                )?),
            )
        };

        let mut candidates = HashSet::new();
        if let Some(target_inode_id) = target_exists {
            let target_inode = inner.inodes.get(&target_inode_id).ok_or(MetaError::NotFound)?;

            if target_is_dir {
                // Remove empty target directory
                inner.dir_entries.remove(&target_inode_id);
                inner.inodes.remove(&target_inode_id);
            } else {
                // Replacing one hard link drops only that reference. Object
                // data becomes collectible only when this was the final name.
                if target_inode.nlink > 1 {
                    inner
                        .inodes
                        .get_mut(&target_inode_id)
                        .expect("target inode checked above")
                        .nlink -= 1;
                } else if defer_reclaim {
                    inner
                        .inodes
                        .get_mut(&target_inode_id)
                        .expect("target inode checked above")
                        .nlink = 0;
                } else {
                    inner.inodes.remove(&target_inode_id);
                    if let Some(chunks) = inner.slices.remove(&target_inode_id) {
                        candidates.extend(slice_block_keys(
                            chunks.values().flat_map(|slices| slices.iter()),
                        ));
                    }
                    inner.symlink_targets.remove(&target_inode_id);
                }
            }
        }

        // Remove from old location
        if let Some(entries) = inner.dir_entries.get_mut(&old_parent) {
            entries.remove(old_name);
        }

        // Add to new location
        inner
            .dir_entries
            .entry(new_parent)
            .or_insert_with(HashMap::new)
            .insert(new_name.to_string(), source_inode_id);

        // Update mtime of both parent directories
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        if let Some(old_parent_inode) = inner.inodes.get_mut(&old_parent) {
            old_parent_inode.mtime = now;
            old_parent_inode.nlink = old_parent_nlink_after;
        }
        if old_parent != new_parent {
            if let Some(new_parent_inode) = inner.inodes.get_mut(&new_parent) {
                new_parent_inode.mtime = now;
                new_parent_inode.nlink = new_parent_nlink_after
                    .expect("cross-directory rename computed new parent nlink");
            }
        }

        Ok(enqueue_confirmed_garbage(&mut inner, candidates))
    }

    async fn pending_garbage(&self) -> Result<Vec<String>> {
        let inner = self.inner.read().await;
        let referenced = referenced_block_keys(&inner);
        let mut pending: Vec<_> = inner
            .pending_garbage
            .difference(&referenced)
            .cloned()
            .collect();
        pending.sort();
        Ok(pending)
    }

    async fn acknowledge_garbage(&self, keys: &[String]) -> Result<()> {
        let mut inner = self.inner.write().await;
        for key in keys {
            inner.pending_garbage.remove(key);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_model::{S_IFDIR, S_IFLNK, S_IFREG};

    #[tokio::test]
    async fn deferred_unlink_keeps_data_through_last_hardlink_until_finalize() {
        let store = MemStore::new();
        let inode = store.create(ROOT_INODE, "open-file", 0o644).await.unwrap();
        let slice = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::new_v4(),
            chunk_offset: 0,
            length: 64,
            written_at: current_unix_time(),
        };
        let key = slice.block_key(0);
        store.append_slice(inode, slice.clone()).await.unwrap();
        store.link(ROOT_INODE, "open-alias", inode).await.unwrap();

        assert!(store
            .unlink_with_lifecycle(ROOT_INODE, "open-file", true)
            .await.unwrap().is_empty());
        assert_eq!(store.getattr(inode).await.unwrap().nlink, 1);
        assert!(store
            .unlink_with_lifecycle(ROOT_INODE, "open-alias", true)
            .await.unwrap().is_empty());
        assert_eq!(store.getattr(inode).await.unwrap().nlink, 0);
        let retained = store.read_slices(inode, 0).await.unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].slice_id, slice.slice_id);
        assert!(store.pending_garbage().await.unwrap().is_empty());

        assert_eq!(store.finalize_orphan(inode).await.unwrap(), vec![key.clone()]);
        assert!(matches!(store.getattr(inode).await, Err(MetaError::NotFound)));
        assert_eq!(store.pending_garbage().await.unwrap(), vec![key]);
    }

    #[tokio::test]
    async fn rename_overwrite_can_retain_open_target_until_finalize() {
        let store = MemStore::new();
        let source = store.create(ROOT_INODE, "source", 0o644).await.unwrap();
        let target = store.create(ROOT_INODE, "target", 0o644).await.unwrap();
        let slice = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::new_v4(),
            chunk_offset: 0,
            length: 8,
            written_at: current_unix_time(),
        };
        let key = slice.block_key(0);
        store.append_slice(target, slice).await.unwrap();
        assert!(store
            .rename_with_lifecycle(
                ROOT_INODE, "source", ROOT_INODE, "target", 0, true,
            )
            .await.unwrap().is_empty());
        assert_eq!(store.lookup(ROOT_INODE, "target").await.unwrap(), source);
        assert_eq!(store.getattr(target).await.unwrap().nlink, 0);
        assert_eq!(store.finalize_orphan(target).await.unwrap(), vec![key]);
    }

    #[tokio::test]
    async fn symlink_lookup_readlink_rename_and_unlink() {
        let store = MemStore::new();
        let target = "../target-directory/a-file.txt";
        let inode = store
            .symlink(ROOT_INODE, "metadata-link", target)
            .await
            .unwrap();
        assert_eq!(store.lookup(ROOT_INODE, "metadata-link").await.unwrap(), inode);
        let attrs = store.getattr(inode).await.unwrap();
        assert_eq!(attrs.mode & 0o170000, S_IFLNK);
        assert_eq!(attrs.size, target.len() as u64);
        assert_eq!(store.readlink(inode).await.unwrap(), target);

        store
            .rename(ROOT_INODE, "metadata-link", ROOT_INODE, "renamed-link")
            .await
            .unwrap();
        assert_eq!(store.readlink(inode).await.unwrap(), target);
        store.unlink(ROOT_INODE, "renamed-link").await.unwrap();
        assert!(matches!(store.readlink(inode).await, Err(MetaError::NotFound)));
    }

    #[tokio::test]
    async fn readlink_rejects_regular_file() {
        let store = MemStore::new();
        assert!(matches!(
            store.readlink(REMOTE_TXT_INODE).await,
            Err(MetaError::NotASymlink)
        ));
    }

    #[tokio::test]
    async fn unlink_reports_shared_block_only_after_last_reference_is_removed() {
        let store = MemStore::new();
        let first = store
            .create(ROOT_INODE, "shared-first", S_IFREG | 0o644)
            .await
            .unwrap();
        let second = store
            .create(ROOT_INODE, "shared-second", S_IFREG | 0o644)
            .await
            .unwrap();
        let shared = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::new_v4(),
            chunk_offset: 0,
            length: 64,
            written_at: current_unix_time(),
        };
        let key = shared.block_key(0);
        store.append_slice(first, shared.clone()).await.unwrap();
        store.append_slice(second, shared).await.unwrap();

        assert!(store.unlink(ROOT_INODE, "shared-first").await.unwrap().is_empty());
        assert!(store.pending_garbage().await.unwrap().is_empty());
        assert_eq!(
            store.unlink(ROOT_INODE, "shared-second").await.unwrap(),
            vec![key.clone()]
        );
        assert_eq!(store.pending_garbage().await.unwrap(), vec![key.clone()]);
        store
            .acknowledge_garbage(std::slice::from_ref(&key))
            .await
            .unwrap();
        // Ack is intentionally idempotent for crash-after-delete recovery.
        store
            .acknowledge_garbage(std::slice::from_ref(&key))
            .await
            .unwrap();
        assert!(store.pending_garbage().await.unwrap().is_empty());
    }

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
        let err = store
            .lookup(ROOT_INODE, "does_not_exist.txt")
            .await
            .unwrap_err();
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
        // remote.txt (inode 3) is a regular file, not a directory -
        // looking anything up "inside" it must be rejected distinctly
        // from a plain NotFound.
        let err = store
            .lookup(REMOTE_TXT_INODE, "anything")
            .await
            .unwrap_err();
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

    #[tokio::test]
    async fn create_new_file_succeeds() {
        let store = MemStore::new();
        let new_inode = store
            .create(ROOT_INODE, "test.txt", S_IFREG | 0o644)
            .await
            .expect("create must succeed");

        // Verify inode exists
        let inode = store
            .getattr(new_inode)
            .await
            .expect("new inode must exist");
        assert_eq!(inode.inode_id, new_inode);
        assert_eq!(inode.size, 0);

        // Verify lookup works
        let looked_up = store
            .lookup(ROOT_INODE, "test.txt")
            .await
            .expect("lookup must succeed");
        assert_eq!(looked_up, new_inode);
    }

    #[tokio::test]
    async fn create_and_mkdir_preserve_permissions_but_force_file_type() {
        let store = MemStore::new();
        let file = store
            .create(ROOT_INODE, "mode-file", S_IFDIR | 0o2640)
            .await
            .unwrap();
        let directory = store
            .mkdir(ROOT_INODE, "mode-dir", S_IFREG | 0o1711)
            .await
            .unwrap();

        assert_eq!(store.getattr(file).await.unwrap().mode, S_IFREG | 0o2640);
        assert_eq!(
            store.getattr(directory).await.unwrap().mode,
            crate::fs_model::S_IFDIR | 0o1711
        );
    }

    #[tokio::test]
    async fn set_attrs_preserve_type_and_update_retained_orphan() {
        let store = MemStore::new();
        let file = store.create(ROOT_INODE, "chmod-file", 0o640).await.unwrap();
        let directory = store.mkdir(ROOT_INODE, "chmod-dir", 0o750).await.unwrap();

        assert_eq!(
            store.set_mode(file, S_IFDIR | 0o6751).await.unwrap(),
            S_IFREG | 0o6751
        );
        assert_eq!(
            store.set_mode(directory, S_IFREG | 0o1770).await.unwrap(),
            S_IFDIR | 0o1770
        );
        assert!(store
            .unlink_with_lifecycle(ROOT_INODE, "chmod-file", true)
            .await.unwrap().is_empty());
        assert_eq!(store.getattr(file).await.unwrap().nlink, 0);
        let attrs = store
            .set_attrs(
                file,
                Some(0o600),
                Some(1234),
                Some(2345),
                Some(1_577_836_800),
                Some(1_577_836_801),
            )
            .await
            .unwrap();
        assert_eq!(attrs.mode, S_IFREG | 0o600);
        assert_eq!((attrs.uid, attrs.gid), (1234, 2345));
        assert_eq!((attrs.atime, attrs.mtime), (1_577_836_800, 1_577_836_801));
        let persisted = store.getattr(file).await.unwrap();
        assert_eq!(persisted.mode, attrs.mode);
        assert_eq!((persisted.uid, persisted.gid), (attrs.uid, attrs.gid));
        assert_eq!((persisted.atime, persisted.mtime), (attrs.atime, attrs.mtime));
        assert!(matches!(store.set_mode(u64::MAX, 0o644).await, Err(MetaError::NotFound)));
    }

    #[tokio::test]
    async fn process_local_store_does_not_request_remote_coherence_polling() {
        assert_eq!(
            MemStore::new().coherence_probe(None).await.unwrap(),
            CoherenceProbe::Disabled
        );
    }

    #[tokio::test]
    async fn directory_nlink_tracks_mkdir_rmdir_and_cross_directory_rename() {
        let store = MemStore::new();
        assert_eq!(store.getattr(ROOT_INODE).await.unwrap().nlink, 2);

        let left = store.mkdir(ROOT_INODE, "left", 0o755).await.unwrap();
        let right = store.mkdir(ROOT_INODE, "right", 0o755).await.unwrap();
        assert_eq!(store.getattr(ROOT_INODE).await.unwrap().nlink, 4);

        let child = store.mkdir(left, "child", 0o700).await.unwrap();
        assert_eq!(store.getattr(left).await.unwrap().nlink, 3);
        assert_eq!(store.getattr(right).await.unwrap().nlink, 2);

        let file = store.create(left, "file", 0o640).await.unwrap();
        store.link(left, "file-alias", file).await.unwrap();
        assert_eq!(store.getattr(left).await.unwrap().nlink, 3);

        store
            .rename(left, "child", right, "moved-child")
            .await
            .unwrap();
        assert_eq!(store.lookup(right, "moved-child").await.unwrap(), child);
        assert_eq!(store.getattr(left).await.unwrap().nlink, 2);
        assert_eq!(store.getattr(right).await.unwrap().nlink, 3);

        store
            .rename(right, "moved-child", right, "renamed-child")
            .await
            .unwrap();
        assert_eq!(store.getattr(right).await.unwrap().nlink, 3);
        store.unlink(right, "renamed-child").await.unwrap();
        assert_eq!(store.getattr(right).await.unwrap().nlink, 2);
    }

    #[tokio::test]
    async fn directory_nlink_accounts_for_rename_over_empty_directory() {
        let store = MemStore::new();
        let left = store.mkdir(ROOT_INODE, "left", 0o755).await.unwrap();
        let right = store.mkdir(ROOT_INODE, "right", 0o755).await.unwrap();
        store.mkdir(left, "source", 0o755).await.unwrap();
        store.mkdir(right, "target", 0o755).await.unwrap();
        assert_eq!(store.getattr(left).await.unwrap().nlink, 3);
        assert_eq!(store.getattr(right).await.unwrap().nlink, 3);

        store
            .rename(left, "source", right, "target")
            .await
            .unwrap();
        assert_eq!(store.getattr(left).await.unwrap().nlink, 2);
        assert_eq!(store.getattr(right).await.unwrap().nlink, 3);

        store.mkdir(right, "other", 0o755).await.unwrap();
        assert_eq!(store.getattr(right).await.unwrap().nlink, 4);
        store
            .rename(right, "target", right, "other")
            .await
            .unwrap();
        assert_eq!(store.getattr(right).await.unwrap().nlink, 3);
    }

    #[tokio::test]
    async fn create_duplicate_name_returns_already_exists() {
        let store = MemStore::new();
        store
            .create(ROOT_INODE, "dup.txt", S_IFREG | 0o644)
            .await
            .expect("first create must succeed");
        let err = store
            .create(ROOT_INODE, "dup.txt", S_IFREG | 0o644)
            .await
            .unwrap_err();
        assert!(matches!(err, MetaError::AlreadyExists));
    }

    #[tokio::test]
    async fn create_under_nonexistent_parent_returns_not_found() {
        let store = MemStore::new();
        let err = store
            .create(999, "test.txt", S_IFREG | 0o644)
            .await
            .unwrap_err();
        assert!(matches!(err, MetaError::NotFound));
    }

    #[tokio::test]
    async fn create_under_regular_file_returns_not_a_directory() {
        let store = MemStore::new();
        let err = store
            .create(REMOTE_TXT_INODE, "test.txt", S_IFREG | 0o644)
            .await
            .unwrap_err();
        assert!(matches!(err, MetaError::NotADirectory));
    }

    #[tokio::test]
    async fn create_with_invalid_name_returns_error() {
        let store = MemStore::new();

        // Empty name
        let err = store
            .create(ROOT_INODE, "", S_IFREG | 0o644)
            .await
            .unwrap_err();
        assert!(matches!(err, MetaError::InvalidName(_)));

        // Name with slash
        let err = store
            .create(ROOT_INODE, "foo/bar", S_IFREG | 0o644)
            .await
            .unwrap_err();
        assert!(matches!(err, MetaError::InvalidName(_)));

        // Name too long
        let long_name = "a".repeat(256);
        let err = store
            .create(ROOT_INODE, &long_name, S_IFREG | 0o644)
            .await
            .unwrap_err();
        assert!(matches!(err, MetaError::InvalidName(_)));
    }

    #[tokio::test]
    async fn append_slice_updates_size_and_mtime() {
        use crate::fs_model::Slice;

        let store = MemStore::new();
        let inode_id = store
            .create(ROOT_INODE, "test.txt", S_IFREG | 0o644)
            .await
            .expect("create must succeed");

        let before = store.getattr(inode_id).await.expect("getattr must succeed");
        assert_eq!(before.size, 0);

        let slice = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::new_v4(),
            chunk_offset: 0,
            length: 100,
            written_at: current_unix_time(),
        };

        store
            .append_slice(inode_id, slice)
            .await
            .expect("append_slice must succeed");

        let after = store.getattr(inode_id).await.expect("getattr must succeed");
        assert_eq!(after.size, 100);
        assert!(after.mtime >= before.mtime);

        // Verify slice is readable
        let slices = store
            .read_slices(inode_id, 0)
            .await
            .expect("read_slices must succeed");
        assert_eq!(slices.len(), 1);
        assert_eq!(slices[0].length, 100);
    }

    #[tokio::test]
    async fn append_slice_cow_preserves_old_slices() {
        use crate::fs_model::Slice;

        let store = MemStore::new();
        let inode_id = store
            .create(ROOT_INODE, "test.txt", S_IFREG | 0o644)
            .await
            .expect("create must succeed");

        let slice1 = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::from_u128(1),
            chunk_offset: 0,
            length: 50,
            written_at: 1000,
        };

        let slice2 = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::from_u128(2),
            chunk_offset: 25,
            length: 50,
            written_at: 2000,
        };

        store
            .append_slice(inode_id, slice1)
            .await
            .expect("first append must succeed");
        store
            .append_slice(inode_id, slice2)
            .await
            .expect("second append must succeed");

        let slices = store
            .read_slices(inode_id, 0)
            .await
            .expect("read_slices must succeed");
        assert_eq!(slices.len(), 2, "both slices must be preserved (COW)");
        assert_eq!(slices[0].slice_id, uuid::Uuid::from_u128(1));
        assert_eq!(slices[1].slice_id, uuid::Uuid::from_u128(2));
    }

    #[tokio::test]
    async fn append_slice_to_nonexistent_inode_returns_not_found() {
        use crate::fs_model::Slice;

        let store = MemStore::new();
        let slice = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::new_v4(),
            chunk_offset: 0,
            length: 100,
            written_at: current_unix_time(),
        };

        let err = store.append_slice(999, slice).await.unwrap_err();
        assert!(matches!(err, MetaError::NotFound));
    }

    #[tokio::test]
    async fn mkdir_creates_directory() {
        use crate::fs_model::S_IFDIR;

        let store = MemStore::new();
        let dir_ino = store
            .mkdir(ROOT_INODE, "testdir", S_IFDIR | 0o755)
            .await
            .expect("mkdir must succeed");

        // Verify it's a directory
        let inode = store.getattr(dir_ino).await.expect("getattr must succeed");
        assert!(inode.is_dir());
        assert_eq!(inode.size, 0);

        // Verify it's in parent's directory entries
        let child_ino = store
            .lookup(ROOT_INODE, "testdir")
            .await
            .expect("lookup must succeed");
        assert_eq!(child_ino, dir_ino);

        // Verify directory is empty
        let entries = store.readdir(dir_ino).await.expect("readdir must succeed");
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn mkdir_rejects_duplicate_name() {
        use crate::fs_model::S_IFDIR;

        let store = MemStore::new();
        store
            .mkdir(ROOT_INODE, "testdir", S_IFDIR | 0o755)
            .await
            .expect("first mkdir must succeed");

        let err = store
            .mkdir(ROOT_INODE, "testdir", S_IFDIR | 0o755)
            .await
            .unwrap_err();
        assert!(matches!(err, MetaError::AlreadyExists));
    }

    #[tokio::test]
    async fn mkdir_rejects_invalid_names() {
        use crate::fs_model::S_IFDIR;

        let store = MemStore::new();

        // Empty name
        let err = store
            .mkdir(ROOT_INODE, "", S_IFDIR | 0o755)
            .await
            .unwrap_err();
        assert!(matches!(err, MetaError::InvalidName(_)));

        // Dot
        let err = store
            .mkdir(ROOT_INODE, ".", S_IFDIR | 0o755)
            .await
            .unwrap_err();
        assert!(matches!(err, MetaError::InvalidName(_)));

        // Dotdot
        let err = store
            .mkdir(ROOT_INODE, "..", S_IFDIR | 0o755)
            .await
            .unwrap_err();
        assert!(matches!(err, MetaError::InvalidName(_)));

        // Slash
        let err = store
            .mkdir(ROOT_INODE, "foo/bar", S_IFDIR | 0o755)
            .await
            .unwrap_err();
        assert!(matches!(err, MetaError::InvalidName(_)));
    }

    #[tokio::test]
    async fn hard_link_keeps_inode_and_objects_until_final_unlink() {
        let store = MemStore::new();
        let inode = store
            .create(ROOT_INODE, "hardlink-source", S_IFREG | 0o644)
            .await
            .unwrap();
        let slice = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::new_v4(),
            chunk_offset: 0,
            length: 32,
            written_at: current_unix_time(),
        };
        let key = slice.block_key(0);
        store.append_slice(inode, slice).await.unwrap();

        assert_eq!(
            store
                .link(ROOT_INODE, "hardlink-alias", inode)
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            store.lookup(ROOT_INODE, "hardlink-alias").await.unwrap(),
            inode
        );
        assert_eq!(store.getattr(inode).await.unwrap().nlink, 2);

        assert!(store
            .unlink(ROOT_INODE, "hardlink-source")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(store.getattr(inode).await.unwrap().nlink, 1);
        assert_eq!(store.read_slices(inode, 0).await.unwrap().len(), 1);
        assert!(store.pending_garbage().await.unwrap().is_empty());

        assert_eq!(
            store
                .unlink(ROOT_INODE, "hardlink-alias")
                .await
                .unwrap(),
            vec![key.clone()]
        );
        assert!(matches!(
            store.getattr(inode).await,
            Err(MetaError::NotFound)
        ));
        assert_eq!(store.pending_garbage().await.unwrap(), vec![key]);
    }

    #[tokio::test]
    async fn hard_link_rejects_directory_and_existing_destination() {
        let store = MemStore::new();
        let file = store
            .create(ROOT_INODE, "existing", S_IFREG | 0o644)
            .await
            .unwrap();
        let directory = store.mkdir(ROOT_INODE, "directory", 0o755).await.unwrap();

        assert!(matches!(
            store.link(ROOT_INODE, "existing", file).await,
            Err(MetaError::AlreadyExists)
        ));
        assert!(matches!(
            store.link(ROOT_INODE, "directory-link", directory).await,
            Err(MetaError::IsADirectory)
        ));
    }

    #[tokio::test]
    async fn rename_over_shared_target_drops_only_replaced_link() {
        let store = MemStore::new();
        let target = store
            .create(ROOT_INODE, "target", S_IFREG | 0o644)
            .await
            .unwrap();
        store.link(ROOT_INODE, "target-alias", target).await.unwrap();
        let source = store
            .create(ROOT_INODE, "source", S_IFREG | 0o644)
            .await
            .unwrap();

        assert!(store
            .rename(ROOT_INODE, "source", ROOT_INODE, "target")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(store.lookup(ROOT_INODE, "target").await.unwrap(), source);
        assert_eq!(
            store.lookup(ROOT_INODE, "target-alias").await.unwrap(),
            target
        );
        assert_eq!(store.getattr(target).await.unwrap().nlink, 1);

        // Renaming one alias over another alias of the same inode is a no-op.
        store.link(ROOT_INODE, "target-alias-2", target).await.unwrap();
        store
            .rename(
                ROOT_INODE,
                "target-alias",
                ROOT_INODE,
                "target-alias-2",
            )
            .await
            .unwrap();
        assert_eq!(store.getattr(target).await.unwrap().nlink, 2);
        store.lookup(ROOT_INODE, "target-alias").await.unwrap();
        store.lookup(ROOT_INODE, "target-alias-2").await.unwrap();
    }

    #[tokio::test]
    async fn rename_noreplace_is_atomic_and_same_inode_is_a_noop() {
        let store = MemStore::new();
        let source = store
            .create(ROOT_INODE, "noreplace-source", S_IFREG | 0o644)
            .await
            .unwrap();
        let victim = store
            .create(ROOT_INODE, "noreplace-victim", S_IFREG | 0o644)
            .await
            .unwrap();
        let victim_slice = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::new_v4(),
            chunk_offset: 0,
            length: 32,
            written_at: current_unix_time(),
        };
        store.append_slice(victim, victim_slice).await.unwrap();

        assert!(matches!(
            store
                .rename_with_flags(
                    ROOT_INODE,
                    "noreplace-source",
                    ROOT_INODE,
                    "noreplace-victim",
                    RENAME_NOREPLACE,
                )
                .await,
            Err(MetaError::AlreadyExists)
        ));
        assert_eq!(
            store.lookup(ROOT_INODE, "noreplace-source").await.unwrap(),
            source
        );
        assert_eq!(
            store.lookup(ROOT_INODE, "noreplace-victim").await.unwrap(),
            victim
        );
        assert!(store.pending_garbage().await.unwrap().is_empty());

        assert_eq!(
            store
                .rename_with_flags(
                    ROOT_INODE,
                    "noreplace-source",
                    ROOT_INODE,
                    "noreplace-new",
                    RENAME_NOREPLACE,
                )
                .await
                .unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(store.lookup(ROOT_INODE, "noreplace-new").await.unwrap(), source);

        store.link(ROOT_INODE, "noreplace-alias", source).await.unwrap();
        store
            .rename_with_flags(
                ROOT_INODE,
                "noreplace-new",
                ROOT_INODE,
                "noreplace-alias",
                RENAME_NOREPLACE,
            )
            .await
            .unwrap();
        assert_eq!(store.getattr(source).await.unwrap().nlink, 2);
        assert_eq!(store.lookup(ROOT_INODE, "noreplace-new").await.unwrap(), source);
        assert_eq!(store.lookup(ROOT_INODE, "noreplace-alias").await.unwrap(), source);
    }

    #[tokio::test]
    async fn rename_exchange_is_atomic_for_files_and_failure_paths() {
        let store = MemStore::new();
        let left = store
            .create(ROOT_INODE, "exchange-left", S_IFREG | 0o640)
            .await
            .unwrap();
        let right = store
            .create(ROOT_INODE, "exchange-right", S_IFREG | 0o600)
            .await
            .unwrap();

        assert!(store
            .rename_with_flags(
                ROOT_INODE,
                "exchange-left",
                ROOT_INODE,
                "missing",
                RENAME_EXCHANGE,
            )
            .await
            .is_err());
        assert_eq!(store.lookup(ROOT_INODE, "exchange-left").await.unwrap(), left);
        assert_eq!(store.lookup(ROOT_INODE, "exchange-right").await.unwrap(), right);

        assert!(matches!(
            store
                .rename_with_flags(
                    ROOT_INODE,
                    "exchange-left",
                    ROOT_INODE,
                    "exchange-right",
                    RENAME_NOREPLACE | RENAME_EXCHANGE,
                )
                .await,
            Err(MetaError::UnsupportedRenameFlags(_))
        ));
        assert_eq!(store.lookup(ROOT_INODE, "exchange-left").await.unwrap(), left);
        assert_eq!(store.lookup(ROOT_INODE, "exchange-right").await.unwrap(), right);

        assert_eq!(
            store
                .rename_with_flags(
                    ROOT_INODE,
                    "exchange-left",
                    ROOT_INODE,
                    "exchange-right",
                    RENAME_EXCHANGE,
                )
                .await
                .unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(store.lookup(ROOT_INODE, "exchange-left").await.unwrap(), right);
        assert_eq!(store.lookup(ROOT_INODE, "exchange-right").await.unwrap(), left);
        assert_eq!(store.getattr(left).await.unwrap().nlink, 1);
        assert_eq!(store.getattr(right).await.unwrap().nlink, 1);
        assert!(store.pending_garbage().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rename_exchange_supports_directories_and_mixed_types_without_cycles() {
        let store = MemStore::new();
        let left_parent = store.mkdir(ROOT_INODE, "left-parent", 0o755).await.unwrap();
        let right_parent = store.mkdir(ROOT_INODE, "right-parent", 0o755).await.unwrap();
        let left_dir = store.mkdir(left_parent, "left-dir", 0o755).await.unwrap();
        let right_dir = store.mkdir(right_parent, "right-dir", 0o755).await.unwrap();
        let child = store.create(left_dir, "child", 0o644).await.unwrap();

        store
            .rename_with_flags(
                left_parent,
                "left-dir",
                right_parent,
                "right-dir",
                RENAME_EXCHANGE,
            )
            .await
            .unwrap();
        assert_eq!(store.lookup(left_parent, "left-dir").await.unwrap(), right_dir);
        assert_eq!(store.lookup(right_parent, "right-dir").await.unwrap(), left_dir);
        assert_eq!(store.lookup(left_dir, "child").await.unwrap(), child);
        assert_eq!(store.getattr(left_parent).await.unwrap().nlink, 3);
        assert_eq!(store.getattr(right_parent).await.unwrap().nlink, 3);

        let mixed_file = store.create(left_parent, "mixed", 0o640).await.unwrap();
        let mixed_dir = store.mkdir(right_parent, "mixed", 0o750).await.unwrap();
        assert_eq!(store.getattr(left_parent).await.unwrap().nlink, 3);
        assert_eq!(store.getattr(right_parent).await.unwrap().nlink, 4);
        store
            .rename_with_flags(
                left_parent,
                "mixed",
                right_parent,
                "mixed",
                RENAME_EXCHANGE,
            )
            .await
            .unwrap();
        assert_eq!(store.lookup(left_parent, "mixed").await.unwrap(), mixed_dir);
        assert_eq!(store.lookup(right_parent, "mixed").await.unwrap(), mixed_file);
        assert_eq!(store.getattr(left_parent).await.unwrap().nlink, 4);
        assert_eq!(store.getattr(right_parent).await.unwrap().nlink, 3);

        assert!(matches!(
            store
                .rename_with_flags(
                    ROOT_INODE,
                    "left-parent",
                    left_parent,
                    "left-dir",
                    RENAME_EXCHANGE,
                )
                .await,
            Err(MetaError::InvalidName(_))
        ));
        assert_eq!(store.lookup(ROOT_INODE, "left-parent").await.unwrap(), left_parent);
        assert_eq!(store.lookup(left_parent, "left-dir").await.unwrap(), right_dir);
    }

    #[tokio::test]
    async fn unlink_removes_file() {
        let store = MemStore::new();
        let file_ino = store
            .create(ROOT_INODE, "test.txt", S_IFREG | 0o644)
            .await
            .expect("create must succeed");

        // Verify file exists
        store.getattr(file_ino).await.expect("file must exist");
        store
            .lookup(ROOT_INODE, "test.txt")
            .await
            .expect("lookup must succeed");

        // Unlink the file
        store
            .unlink(ROOT_INODE, "test.txt")
            .await
            .expect("unlink must succeed");

        // Verify file is gone
        let err = store.getattr(file_ino).await.unwrap_err();
        assert!(matches!(err, MetaError::NotFound));

        let err = store.lookup(ROOT_INODE, "test.txt").await.unwrap_err();
        assert!(matches!(err, MetaError::NotFound));
    }

    #[tokio::test]
    async fn unlink_removes_empty_directory() {
        use crate::fs_model::S_IFDIR;

        let store = MemStore::new();
        let dir_ino = store
            .mkdir(ROOT_INODE, "emptydir", S_IFDIR | 0o755)
            .await
            .expect("mkdir must succeed");

        // Unlink (rmdir) the empty directory
        store
            .unlink(ROOT_INODE, "emptydir")
            .await
            .expect("unlink empty dir must succeed");

        // Verify directory is gone
        let err = store.getattr(dir_ino).await.unwrap_err();
        assert!(matches!(err, MetaError::NotFound));
    }

    #[tokio::test]
    async fn unlink_rejects_non_empty_directory() {
        use crate::fs_model::S_IFDIR;

        let store = MemStore::new();
        let dir_ino = store
            .mkdir(ROOT_INODE, "nonempty", S_IFDIR | 0o755)
            .await
            .expect("mkdir must succeed");

        // Create a file inside the directory
        store
            .create(dir_ino, "file.txt", S_IFREG | 0o644)
            .await
            .expect("create must succeed");

        // Try to unlink the non-empty directory
        let err = store
            .unlink(ROOT_INODE, "nonempty")
            .await
            .unwrap_err();
        assert!(matches!(err, MetaError::NotEmpty));

        // Verify directory still exists
        store.getattr(dir_ino).await.expect("dir must still exist");
    }

    #[tokio::test]
    async fn unlink_rejects_invalid_names() {
        let store = MemStore::new();

        let err = store.unlink(ROOT_INODE, ".").await.unwrap_err();
        assert!(matches!(err, MetaError::InvalidName(_)));

        let err = store.unlink(ROOT_INODE, "..").await.unwrap_err();
        assert!(matches!(err, MetaError::InvalidName(_)));

        let err = store.unlink(ROOT_INODE, "").await.unwrap_err();
        assert!(matches!(err, MetaError::InvalidName(_)));
    }

    #[tokio::test]
    async fn unlink_nonexistent_file_returns_not_found() {
        let store = MemStore::new();
        let err = store
            .unlink(ROOT_INODE, "does_not_exist")
            .await
            .unwrap_err();
        assert!(matches!(err, MetaError::NotFound));
    }

    #[tokio::test]
    async fn rename_same_directory_renames_file() {
        let store = MemStore::new();
        let file_ino = store.create(ROOT_INODE, "oldname.txt", S_IFREG | 0o644).await.unwrap();
        
        // Rename in same directory
        store.rename(ROOT_INODE, "oldname.txt", ROOT_INODE, "newname.txt").await.unwrap();
        
        // Old name should not exist
        let err = store.lookup(ROOT_INODE, "oldname.txt").await.unwrap_err();
        assert!(matches!(err, MetaError::NotFound));
        
        // New name should resolve to same inode
        let found_ino = store.lookup(ROOT_INODE, "newname.txt").await.unwrap();
        assert_eq!(found_ino, file_ino);
    }

    #[tokio::test]
    async fn rename_cross_directory_moves_file() {
        let store = MemStore::new();
        let dir1_ino = store.mkdir(ROOT_INODE, "dir1", 0o755).await.unwrap();
        let dir2_ino = store.mkdir(ROOT_INODE, "dir2", 0o755).await.unwrap();
        let file_ino = store.create(dir1_ino, "file.txt", S_IFREG | 0o644).await.unwrap();
        
        // Move from dir1 to dir2
        store.rename(dir1_ino, "file.txt", dir2_ino, "moved.txt").await.unwrap();
        
        // Should not exist in old location
        let err = store.lookup(dir1_ino, "file.txt").await.unwrap_err();
        assert!(matches!(err, MetaError::NotFound));
        
        // Should exist in new location
        let found_ino = store.lookup(dir2_ino, "moved.txt").await.unwrap();
        assert_eq!(found_ino, file_ino);
    }

    #[tokio::test]
    async fn rename_replaces_existing_file() {
        let store = MemStore::new();
        let file1_ino = store.create(ROOT_INODE, "file1.txt", S_IFREG | 0o644).await.unwrap();
        let file2_ino = store.create(ROOT_INODE, "file2.txt", S_IFREG | 0o644).await.unwrap();
        
        // Rename file1 to file2 (should replace file2)
        store.rename(ROOT_INODE, "file1.txt", ROOT_INODE, "file2.txt").await.unwrap();
        
        // file1.txt should not exist
        let err = store.lookup(ROOT_INODE, "file1.txt").await.unwrap_err();
        assert!(matches!(err, MetaError::NotFound));
        
        // file2.txt should now point to file1's inode
        let found_ino = store.lookup(ROOT_INODE, "file2.txt").await.unwrap();
        assert_eq!(found_ino, file1_ino);
        
        // Old file2 inode should be gone
        let err = store.getattr(file2_ino).await.unwrap_err();
        assert!(matches!(err, MetaError::NotFound));
    }

    #[tokio::test]
    async fn rename_rejects_nonempty_directory_target() {
        let store = MemStore::new();
        let _file_ino = store.create(ROOT_INODE, "file.txt", S_IFREG | 0o644).await.unwrap();
        let dir_ino = store.mkdir(ROOT_INODE, "dir", 0o755).await.unwrap();
        store.create(dir_ino, "child.txt", S_IFREG | 0o644).await.unwrap();
        
        // Try to rename file over non-empty directory
        let err = store.rename(ROOT_INODE, "file.txt", ROOT_INODE, "dir").await.unwrap_err();
        assert!(matches!(err, MetaError::NotEmpty));
    }

    #[tokio::test]
    async fn rename_rejects_directory_into_own_subtree() {
        let store = MemStore::new();
        let dir1_ino = store.mkdir(ROOT_INODE, "dir1", 0o755).await.unwrap();
        let dir2_ino = store.mkdir(dir1_ino, "dir2", 0o755).await.unwrap();
        
        // Try to move dir1 into dir1/dir2 (would create a loop)
        let err = store.rename(ROOT_INODE, "dir1", dir2_ino, "moved").await.unwrap_err();
        assert!(matches!(err, MetaError::InvalidName(_)));
    }

    #[tokio::test]
    async fn rename_rejects_invalid_names() {
        let store = MemStore::new();
        store.create(ROOT_INODE, "file.txt", S_IFREG | 0o644).await.unwrap();
        
        // Try to rename with invalid old_name
        let err = store.rename(ROOT_INODE, "..", ROOT_INODE, "newname").await.unwrap_err();
        assert!(matches!(err, MetaError::InvalidName(_)));
        
        // Try to rename with invalid new_name
        let err = store.rename(ROOT_INODE, "file.txt", ROOT_INODE, ".").await.unwrap_err();
        assert!(matches!(err, MetaError::InvalidName(_)));
    }

    #[tokio::test]
    async fn rename_nonexistent_source_returns_not_found() {
        let store = MemStore::new();
        let err = store.rename(ROOT_INODE, "nonexistent", ROOT_INODE, "newname").await.unwrap_err();
        assert!(matches!(err, MetaError::NotFound));
    }
}
