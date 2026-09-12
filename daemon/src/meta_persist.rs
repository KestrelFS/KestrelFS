// SPDX-License-Identifier: Apache-2.0
//! Metadata persistence: JSON-based local file storage for MetaStore state.
//!
//! # Design
//!
//! `FileMetaStore` wraps an in-memory `MemStore` and synchronizes its state
//! to disk after every mutating operation (create/append_slice/truncate).
//! The on-disk format is a single JSON file containing:
//! - All inodes (HashMap<u64, Inode>)
//! - All directory entries (HashMap<u64, HashMap<String, u64>>)
//! - All slices (HashMap<u64, HashMap<u32, Vec<Slice>>>)
//!
//! # Atomicity
//!
//! Writes use the standard atomic rename pattern:
//! 1. Serialize to `{path}.tmp`
//! 2. `fsync()` the temp file
//! 3. Rename `{path}.tmp` -> `{path}` (atomic on POSIX)
//!
//! # Recovery
//!
//! On daemon startup:
//! - If `meta.json` exists: load and populate MemStore
//! - If missing or invalid: start with fresh MemStore (root + bootstrap files)
//!
//! # Performance
//!
//! This is intentionally simple (serialize entire state on every write) because:
//! - Metadata is small (< 1MB for thousands of files)
//! - Writes are infrequent in the bootstrap model
//! - JSON is human-readable for debugging
//!
//! A production system would use Redis/TiKV instead (see meta.rs module docs).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::fs_model::{Inode, Slice};
use crate::meta::{MetaError, MetaStore, MemStore, Result};
#[cfg(test)]
use crate::meta::current_unix_time;

/// Serializable snapshot of MemStore's internal state.
///
/// Mirrors `MemStoreInner` exactly (meta.rs:259), but owned/public
/// so serde can serialize it without exposing MemStore's internals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MetaSnapshot {
    inodes: HashMap<u64, Inode>,
    dir_entries: HashMap<u64, HashMap<String, u64>>,
    slices: HashMap<u64, HashMap<u32, Vec<Slice>>>,
    next_inode_id: u64,
}

/// A MetaStore implementation that persists to a local JSON file.
///
/// Wraps a MemStore and syncs to disk after every mutation.
pub struct FileMetaStore {
    mem: MemStore,
    path: PathBuf,
}

impl FileMetaStore {
    /// Creates a new FileMetaStore, loading from `path` if it exists.
    ///
    /// If the file doesn't exist or is invalid JSON, starts with a fresh
    /// MemStore (root + bootstrap files: remote.txt, writable.dat).
    pub async fn new(path: PathBuf) -> std::io::Result<Self> {
        let mem = if path.exists() {
            match Self::load_from_disk(&path).await {
                Ok(loaded) => {
                    eprintln!("[meta_persist] Loaded metadata from {}", path.display());
                    loaded
                }
                Err(e) => {
                    eprintln!(
                        "[meta_persist] Failed to load {} ({}), starting fresh",
                        path.display(),
                        e
                    );
                    MemStore::new()
                }
            }
        } else {
            eprintln!(
                "[meta_persist] No existing metadata at {}, starting fresh",
                path.display()
            );
            MemStore::new()
        };

        Ok(FileMetaStore { mem, path })
    }

    /// Loads a MemStore from disk (internal helper).
    async fn load_from_disk(path: &Path) -> std::io::Result<MemStore> {
        let data = fs::read(path).await?;
        let snapshot: MetaSnapshot = serde_json::from_slice(&data)?;

        Ok(MemStore::from_snapshot(snapshot))
    }

    /// Atomically writes current state to disk (tmp + rename).
    async fn sync_to_disk(&self) -> std::io::Result<()> {
        let snapshot = self.mem.snapshot().await;
        let json = serde_json::to_vec_pretty(&snapshot)?;

        let tmp_path = self.path.with_extension("tmp");
        let mut file = fs::File::create(&tmp_path).await?;
        file.write_all(&json).await?;
        file.sync_all().await?;
        drop(file);

        fs::rename(&tmp_path, &self.path).await?;

        Ok(())
    }
}

#[async_trait]
impl MetaStore for FileMetaStore {
    async fn lookup(&self, parent: u64, name: &str) -> Result<u64> {
        self.mem.lookup(parent, name).await
    }

    async fn getattr(&self, inode: u64) -> Result<Inode> {
        self.mem.getattr(inode).await
    }

    async fn read_slices(&self, inode: u64, chunk_idx: u32) -> Result<Vec<Slice>> {
        self.mem.read_slices(inode, chunk_idx).await
    }

    async fn create(&self, parent: u64, name: &str, mode: u32) -> Result<u64> {
        let inode_id = self.mem.create(parent, name, mode).await?;
        self.sync_to_disk()
            .await
            .map_err(|_| MetaError::NotFound)?;
        Ok(inode_id)
    }

    async fn append_slice(&self, inode: u64, slice: Slice) -> Result<()> {
        self.mem.append_slice(inode, slice).await?;
        self.sync_to_disk()
            .await
            .map_err(|_| MetaError::NotFound)?;
        Ok(())
    }

    async fn truncate(&self, inode: u64, new_size: u64) -> Result<()> {
        self.mem.truncate(inode, new_size).await?;
        self.sync_to_disk()
            .await
            .map_err(|_| MetaError::NotFound)?;
        Ok(())
    }

    async fn readdir(&self, inode: u64) -> Result<Vec<(u64, String)>> {
        self.mem.readdir(inode).await
    }
}

impl MemStore {
    /// Extracts a snapshot of current state (for serialization).
    pub(crate) async fn snapshot(&self) -> MetaSnapshot {
        let inner = self.inner.read().await;
        MetaSnapshot {
            inodes: inner.inodes.clone(),
            dir_entries: inner.dir_entries.clone(),
            slices: inner.slices.clone(),
            next_inode_id: self.next_inode_id.load(std::sync::atomic::Ordering::Relaxed),
        }
    }

    /// Restores a MemStore from a snapshot (for deserialization).
    pub(crate) fn from_snapshot(snapshot: MetaSnapshot) -> Self {
        use std::sync::atomic::AtomicU64;
        use tokio::sync::RwLock;

        MemStore {
            inner: RwLock::new(crate::meta::MemStoreInner {
                inodes: snapshot.inodes,
                dir_entries: snapshot.dir_entries,
                slices: snapshot.slices,
            }),
            next_inode_id: AtomicU64::new(snapshot.next_inode_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_model::{ROOT_INODE, S_IFREG};
    use crate::meta::{REMOTE_TXT_INODE, WRITABLE_DAT_INODE};
    use tempfile::TempDir;

    #[tokio::test]
    async fn new_file_meta_store_starts_with_bootstrap_files() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");

        let store = FileMetaStore::new(path).await.unwrap();

        let root = store.getattr(ROOT_INODE).await.unwrap();
        assert!(root.is_dir());

        let remote = store.getattr(REMOTE_TXT_INODE).await.unwrap();
        assert_eq!(remote.mode & S_IFREG, S_IFREG);
        assert_eq!(remote.size, 512);

        let writable = store.getattr(WRITABLE_DAT_INODE).await.unwrap();
        assert_eq!(writable.mode & S_IFREG, S_IFREG);
        assert_eq!(writable.size, 0);
    }

    #[tokio::test]
    async fn create_and_reload_preserves_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");

        {
            let store = FileMetaStore::new(path.clone()).await.unwrap();
            let new_ino = store.create(ROOT_INODE, "test.txt", 0o644).await.unwrap();
            assert!(new_ino > WRITABLE_DAT_INODE);
        }

        let store2 = FileMetaStore::new(path).await.unwrap();
        let found = store2.lookup(ROOT_INODE, "test.txt").await.unwrap();
        let inode = store2.getattr(found).await.unwrap();
        assert_eq!(inode.mode & 0o170000, S_IFREG);  // Check only file type bits
        assert_eq!(inode.size, 0);
    }

    #[tokio::test]
    async fn append_slice_and_reload_preserves_slices() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");

        let test_slice = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::new_v4(),
            chunk_offset: 0,
            length: 128,
            written_at: current_unix_time(),
        };

        {
            let store = FileMetaStore::new(path.clone()).await.unwrap();
            let new_ino = store.create(ROOT_INODE, "data.bin", 0o644).await.unwrap();
            store.append_slice(new_ino, test_slice.clone()).await.unwrap();
        }

        let store2 = FileMetaStore::new(path).await.unwrap();
        let found = store2.lookup(ROOT_INODE, "data.bin").await.unwrap();
        let slices = store2.read_slices(found, 0).await.unwrap();

        assert_eq!(slices.len(), 1);
        assert_eq!(slices[0].slice_id, test_slice.slice_id);
        assert_eq!(slices[0].length, 128);

        let inode = store2.getattr(found).await.unwrap();
        assert_eq!(inode.size, 128);
    }

    #[tokio::test]
    async fn truncate_and_reload_preserves_size() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");

        {
            let store = FileMetaStore::new(path.clone()).await.unwrap();
            let new_ino = store.create(ROOT_INODE, "truncate.txt", 0o644).await.unwrap();
            store.truncate(new_ino, 1024).await.unwrap();
        }

        let store2 = FileMetaStore::new(path).await.unwrap();
        let found = store2.lookup(ROOT_INODE, "truncate.txt").await.unwrap();
        let inode = store2.getattr(found).await.unwrap();
        assert_eq!(inode.size, 1024);
    }

    #[tokio::test]
    async fn invalid_json_falls_back_to_fresh_store() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");

        fs::write(&path, b"{ invalid json }").await.unwrap();

        let store = FileMetaStore::new(path).await.unwrap();

        let root = store.getattr(ROOT_INODE).await.unwrap();
        assert!(root.is_dir());
    }
}
