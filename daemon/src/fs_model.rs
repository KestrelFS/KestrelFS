// SPDX-License-Identifier: Apache-2.0
//! Filesystem data model: `Inode`, `Chunk`, `Slice`, `Block`.
//!
//! # Design overview (JuiceFS-inspired layered chunking)
//!
//! KestrelFS splits a file's logical byte stream into three nested
//! layers, exactly mirroring JuiceFS's own chunk/slice/block model
//! (see JuiceFS's "How JuiceFS Stores Files" documentation) because it
//! is a well-proven design for exactly the problem we have: files can
//! be arbitrarily large and are written non-sequentially/repeatedly,
//! but the underlying object store (S3) only wants immutable,
//! write-once objects.
//!
//! ```text
//!   File (logical byte stream, arbitrary length)
//!     |
//!     +-- Chunk 0   [0, CHUNK_SIZE)         <- fixed-size logical window
//!     +-- Chunk 1   [CHUNK_SIZE, 2*CHUNK_SIZE)
//!     +-- Chunk N   ...
//!           |
//!           +-- Slice A  (chunk_index=N, offset=0,    length=...)
//!           +-- Slice B  (chunk_index=N, offset=...,  length=...)
//!                 |         ^ slices within one chunk can overlap in
//!                 |           their logical [offset, offset+length)
//!                 |           ranges - the LAST slice written to a
//!                 |           given byte wins on read (copy-on-write
//!                 |           semantics), exactly like JuiceFS. This
//!                 |           is what makes small, repeated writes
//!                 |           cheap: each write just appends a new
//!                 |           slice instead of rewriting the whole
//!                 |           chunk's backing object.
//!                 |
//!                 +-- Block(s) (physical, fixed-size objects actually
//!                               stored in S3, referenced by the
//!                               slice's `slice_id` + a block index)
//! ```
//!
//! - **Chunk**: a fixed-size (64 MiB, [`CHUNK_SIZE`]) logical window
//!   into a file. Purely a coordinate system for addressing "which
//!   64 MiB window of this file does this byte range fall into" - a
//!   `Chunk` itself holds no data or slice list; [`crate::meta::MetaStore::read_slices`]
//!   is how the slice list for a given `(inode, chunk_index)` pair is
//!   looked up.
//! - **Slice**: a variable-length logical write record within one
//!   chunk. Every `write()` (once Phase 3+ implements writes) appends
//!   exactly one new `Slice` rather than mutating existing data in
//!   place - this is what lets the backing object store be
//!   write-once/immutable. A chunk accumulates slices over time; a
//!   background compaction process (not yet implemented) would
//!   eventually merge many small slices back into one for read
//!   efficiency, exactly as JuiceFS does.
//! - **Block**: the actual physical unit stored in the object store.
//!   A single `Slice` longer than [`BLOCK_SIZE`] (4 MiB) is split into
//!   multiple blocks at upload time; [`Slice::block_count`] computes
//!   how many. Blocks are named deterministically from
//!   `(slice_id, block_index)` so no separate "which blocks make up
//!   this slice" index needs to be stored anywhere - see
//!   [`Slice::block_key`].
//!
//! # Why these specific sizes
//!
//! 64 MiB chunks / 4 MiB blocks are the exact values JuiceFS itself
//! defaults to, chosen there (and reused here) as a reasonable
//! middle ground: large enough that a full read-through of a large
//! file does not require tracking an enormous number of chunk
//! coordinates, small enough that a single block upload/download to
//! S3 is a reasonably-sized HTTP request (multi-GB single objects
//! would make partial reads and retries expensive).
//!
//! # Status
//!
//! `Inode` and its `getattr`/`lookup`-relevant fields are wired into
//! the IPC event loop as of Phase 3 step 2 (see `main.rs`'s
//! `handle_lookup`/`handle_getattr`). `Slice`/`Block` and the
//! `CHUNK_SIZE`/`BLOCK_SIZE`/`chunk_index_for_offset`/`block_count`/
//! `block_key` machinery remain deliberately unconnected - per that
//! step's explicit scope, `KESTRELFS_OP_READ_CHUNK` still answers
//! with a synthetic payload (see `main.rs::handle_read_chunk`)
//! rather than calling `MetaStore::read_slices`. The
//! `#[allow(dead_code)]` annotations below each still-unconnected
//! item reflect that this is deliberate, temporary scaffolding rather
//! than genuinely dead code - every type and method here is exercised
//! by this module's own unit tests (`cargo test`).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Fixed logical chunk size: 64 MiB. See the module doc comment for
/// why this specific value was chosen.
#[allow(dead_code)]
pub const CHUNK_SIZE: u64 = 64 * 1024 * 1024;

/// Fixed physical block size: 4 MiB. A `Slice` longer than this is
/// split into multiple blocks at upload time (see
/// [`Slice::block_count`]).
#[allow(dead_code)]
pub const BLOCK_SIZE: u64 = 4 * 1024 * 1024;

/// The inode number of the filesystem root directory. Mirrors the
/// convention `simple_fill_super()` itself uses on the C/kernel side
/// (see `fs/libfs.c`: "because the root inode is 1, the files array
/// must not contain an entry at index 1") - kept identical here so
/// that inode numbers are consistent in spirit across the kernel's
/// in-memory Phase 1/2 tree and this metadata layer, even though the
/// two are not yet wired together.
pub const ROOT_INODE: u64 = 1;

/// POSIX file type + permission bits, mirroring the standard `mode_t`
/// encoding (`S_IFREG`, `S_IFDIR`, permission bits, etc). Stored as a
/// plain `u32` rather than a richer enum since this value is meant to
/// be handed back almost verbatim to a `KESTRELFS_OP_GETATTR`
/// response's payload (see `kestrelfs_ipc.h`), which itself just
/// carries raw bytes - there is no benefit to an intermediate typed
/// representation the kernel side would have to be translated out of
/// again.
pub type FileMode = u32;

/// `S_IFDIR` from `<linux/stat.h>` - directory file type bits.
pub const S_IFDIR: FileMode = 0o040000;
/// `S_IFREG` from `<linux/stat.h>` - regular file type bits.
pub const S_IFREG: FileMode = 0o100000;
/// `S_IFLNK` from `<linux/stat.h>` - symbolic-link file type bits.
pub const S_IFLNK: FileMode = 0o120000;

/// Maximum UTF-8 byte length stored for a symbolic-link target. This mirrors
/// the ABI limit and leaves one byte for the NUL terminator returned to VFS.
pub const SYMLINK_TARGET_MAX: usize = 4095;

/// `Inode` - basic POSIX metadata for one filesystem object (file or
/// directory).
///
/// This intentionally only carries the subset of POSIX attributes
/// KestrelFS actually needs to answer `KESTRELFS_OP_GETATTR`/
/// `KESTRELFS_OP_LOOKUP` requests (see `kestrelfs_ipc.h`) - there is
/// no `atime`/`ctime` split yet, no extended attributes, no ACLs.
/// Those can be added additively later without breaking the
/// `#[derive(Serialize, Deserialize)]` contract as long as new fields
/// carry `#[serde(default)]` for backward compatibility with
/// already-persisted Redis entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inode {
    /// Unique, filesystem-wide identifier. [`ROOT_INODE`] (1) is
    /// reserved for the root directory.
    pub inode_id: u64,
    /// Logical file size in bytes. For a regular file, this is the
    /// authoritative length used to compute EOF - NOT derived from
    /// summing slice lengths, since sparse files / holes created by
    /// `truncate()` (not yet implemented) can extend size beyond any
    /// written slice.
    pub size: u64,
    /// POSIX mode bits: file type ([`S_IFDIR`]/[`S_IFREG`]) OR'd with
    /// permission bits (e.g. `0o644`).
    pub mode: FileMode,
    /// Owning user ID.
    pub uid: u32,
    /// Owning group ID.
    pub gid: u32,
    /// Hard link count. Always `1` for regular files in this
    /// bootstrap model (no hard link support yet); `2` is the POSIX
    /// convention for an otherwise-empty directory (`.` and the
    /// parent's entry pointing back at it).
    pub nlink: u32,
    /// Last modification time, Unix epoch seconds. Deliberately a
    /// single timestamp (no separate `atime`/`ctime`) for this
    /// bootstrap model - see the struct doc comment.
    pub mtime: u64,
}

impl Inode {
    /// Builds the root directory's `Inode` (inode_id ==
    /// [`ROOT_INODE`]), with the given `mtime` (seconds since the
    /// Unix epoch).
    pub fn new_root_dir(mtime: u64) -> Self {
        Inode {
            inode_id: ROOT_INODE,
            size: 0,
            mode: S_IFDIR | 0o755,
            uid: 0,
            gid: 0,
            nlink: 2,
            mtime,
        }
    }

    /// Builds a regular file's `Inode`.
    pub fn new_file(inode_id: u64, size: u64, mtime: u64) -> Self {
        Inode {
            inode_id,
            size,
            mode: S_IFREG | 0o644,
            uid: 0,
            gid: 0,
            nlink: 1,
            mtime,
        }
    }

    pub fn new_dir(inode_id: u64, mtime: u64) -> Self {
        Inode {
            inode_id,
            size: 0,
            mode: S_IFDIR | 0o755,
            uid: 0,
            gid: 0,
            nlink: 2,
            mtime,
        }
    }

    /// Builds a symbolic-link inode. The target bytes live in MetaStore, not
    /// ObjectStore; `size` follows Linux convention and reports target length.
    pub fn new_symlink(inode_id: u64, target_len: usize, mtime: u64) -> Self {
        Inode {
            inode_id,
            size: target_len as u64,
            mode: S_IFLNK | 0o777,
            uid: 0,
            gid: 0,
            nlink: 1,
            mtime,
        }
    }

    /// Returns `true` if this inode represents a directory.
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFDIR == S_IFDIR
    }

    /// Returns `true` if this inode represents a symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.mode & S_IFLNK == S_IFLNK
    }

    /// Returns the [`CHUNK_SIZE`]-indexed chunk number that byte
    /// offset `offset` falls into. Pure coordinate arithmetic - see
    /// the module doc comment's ASCII diagram.
    #[allow(dead_code)]
    pub fn chunk_index_for_offset(offset: u64) -> u32 {
        (offset / CHUNK_SIZE) as u32
    }
}

/// `Slice` - one variable-length logical write record within a single
/// chunk.
///
/// A `Slice` is the unit `MetaStore::read_slices` returns: to resolve
/// "what data lives at file offset X", the caller first computes
/// `chunk_index = Inode::chunk_index_for_offset(X)`, fetches that
/// chunk's slice list, then picks whichever slice's
/// `[chunk_offset, chunk_offset + length)` range covers `X`'s
/// position *within that chunk* - preferring the most recently
/// written slice on overlap (copy-on-write semantics; see the module
/// doc comment).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Slice {
    /// Which chunk (see [`Inode::chunk_index_for_offset`]) this slice
    /// belongs to. Redundant with whatever key `read_slices` was
    /// queried with in a `MetaStore` that stores slices already
    /// grouped by chunk (like [`crate::meta::MemStore`]), but kept as
    /// an explicit field so a `Slice` remains self-describing if it
    /// is ever handled outside that context (e.g. logged, or stored
    /// in a flat KV representation instead of chunk-grouped).
    pub chunk_index: u32,
    /// Unique identifier for this slice's backing data, also used as
    /// the object-store key prefix for the block(s) making up this
    /// slice - see [`Slice::block_key`]. A `Uuid` (rather than a
    /// sequential counter) is used specifically so slice IDs can be
    /// generated client-side (by this daemon) without any
    /// coordination with a central allocator - important once
    /// multiple KestrelFS daemon instances exist across a cluster.
    pub slice_id: Uuid,
    /// Byte offset *within the chunk* (i.e. in `[0, CHUNK_SIZE)`,
    /// NOT a whole-file offset) where this slice's data logically
    /// begins.
    pub chunk_offset: u32,
    /// Length of this slice's data, in bytes.
    pub length: u32,
    /// Unix epoch seconds when this slice was written. Used to break
    /// ties when multiple slices within a chunk overlap the same
    /// byte range - the slice with the greatest `written_at` wins
    /// (see the module doc comment's copy-on-write note).
    pub written_at: u64,
}

impl Slice {
    /// Number of fixed-size [`BLOCK_SIZE`] physical blocks needed to
    /// store this slice's `length` bytes (the final block may be
    /// shorter than `BLOCK_SIZE`).
    #[allow(dead_code)]
    pub fn block_count(&self) -> u32 {
        self.length.div_ceil(BLOCK_SIZE as u32)
    }

    /// Deterministically derives the object-store key for the
    /// `block_index`-th physical block making up this slice, of the
    /// form `<slice_id>/<block_index>`.
    ///
    /// Deriving block keys purely from `(slice_id, block_index)` -
    /// rather than storing an explicit list of block keys in the
    /// slice itself - means the metadata layer never needs to persist
    /// or look up a separate "which objects make up this slice"
    /// index: any caller holding a `Slice` can always reconstruct
    /// every one of its block keys purely from `self.block_count()`
    /// and this function.
    #[allow(dead_code)]
    pub fn block_key(&self, block_index: u32) -> String {
        format!("{}/{block_index}", self.slice_id)
    }
}

/// `Block` - one physical, fixed-size (at most [`BLOCK_SIZE`]) object
/// as actually stored in the backing object store.
///
/// Unlike [`Inode`]/[`Slice`], `Block` is not itself persisted as a
/// metadata record anywhere (there is no `MetaStore` method that
/// returns a `Block`) - it exists purely as a typed description of
/// "one HTTP PUT/GET worth of data" for the future S3 upload/download
/// code path (Phase 3's later steps) to construct on demand from a
/// `Slice` via [`Slice::block_count`]/[`Slice::block_key`], rather
/// than being looked up from metadata the way an `Inode` or `Slice`
/// is.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Block {
    /// Object-store key, as produced by [`Slice::block_key`].
    pub key: String,
    /// This block's payload. `Vec<u8>` (owned, heap-allocated) rather
    /// than a borrowed slice: blocks are the unit that eventually
    /// crosses an actual network I/O boundary (an S3 PUT/GET body),
    /// where an owned buffer is the natural representation.
    pub data: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_inode_is_directory() {
        let root = Inode::new_root_dir(1_700_000_000);
        assert_eq!(root.inode_id, ROOT_INODE);
        assert!(root.is_dir());
        assert_eq!(root.nlink, 2);
    }

    #[test]
    fn regular_file_is_not_directory() {
        let file = Inode::new_file(42, 1024, 1_700_000_000);
        assert!(!file.is_dir());
        assert_eq!(file.mode & S_IFREG, S_IFREG);
        assert_eq!(file.nlink, 1);
    }

    #[test]
    fn symlink_inode_reports_link_type_and_target_size() {
        let link = Inode::new_symlink(43, 37, 1_700_000_000);
        assert!(link.is_symlink());
        assert!(!link.is_dir());
        assert_eq!(link.mode & 0o170000, S_IFLNK);
        assert_eq!(link.mode & 0o777, 0o777);
        assert_eq!(link.size, 37);
        assert_eq!(link.nlink, 1);
    }

    #[test]
    fn chunk_index_for_offset_matches_chunk_boundaries() {
        assert_eq!(Inode::chunk_index_for_offset(0), 0);
        assert_eq!(Inode::chunk_index_for_offset(CHUNK_SIZE - 1), 0);
        assert_eq!(Inode::chunk_index_for_offset(CHUNK_SIZE), 1);
        assert_eq!(Inode::chunk_index_for_offset(CHUNK_SIZE * 3 + 5), 3);
    }

    #[test]
    fn slice_block_count_rounds_up() {
        let slice = Slice {
            chunk_index: 0,
            slice_id: Uuid::new_v4(),
            chunk_offset: 0,
            length: (BLOCK_SIZE as u32) + 1,
            written_at: 0,
        };
        // One full block plus one byte must still require a second
        // block - block_count() must round UP, never truncate.
        assert_eq!(slice.block_count(), 2);
    }

    #[test]
    fn slice_block_count_exact_multiple() {
        let slice = Slice {
            chunk_index: 0,
            slice_id: Uuid::new_v4(),
            chunk_offset: 0,
            length: (BLOCK_SIZE as u32) * 3,
            written_at: 0,
        };
        assert_eq!(slice.block_count(), 3);
    }

    #[test]
    fn block_key_is_deterministic_and_unique_per_index() {
        let slice = Slice {
            chunk_index: 0,
            slice_id: Uuid::new_v4(),
            chunk_offset: 0,
            length: 100,
            written_at: 0,
        };
        let key0 = slice.block_key(0);
        let key1 = slice.block_key(1);
        assert_ne!(key0, key1);
        // Deterministic: calling again with the same index reproduces
        // the exact same key (no hidden randomness beyond slice_id).
        assert_eq!(key0, slice.block_key(0));
        assert!(key0.starts_with(&slice.slice_id.to_string()));
    }

    #[test]
    fn inode_serde_roundtrip() {
        let original = Inode::new_file(7, 999, 1_700_000_000);
        let json = serde_json::to_string(&original).expect("serialize");
        let restored: Inode = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original.inode_id, restored.inode_id);
        assert_eq!(original.size, restored.size);
        assert_eq!(original.mode, restored.mode);
    }

    #[test]
    fn slice_serde_roundtrip() {
        let original = Slice {
            chunk_index: 3,
            slice_id: Uuid::new_v4(),
            chunk_offset: 128,
            length: 4096,
            written_at: 1_700_000_000,
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let restored: Slice = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original.slice_id, restored.slice_id);
        assert_eq!(original.chunk_offset, restored.chunk_offset);
        assert_eq!(original.length, restored.length);
    }
}
