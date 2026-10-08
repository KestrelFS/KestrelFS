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
//! - All symbolic-link targets (HashMap<u64, String>)
//! - Object keys pending idempotent GC deletion (HashSet<String>)
//!
//! # Atomicity
//!
//! Writes use versioned temporary files and the standard atomic rename pattern:
//! 1. Apply one mutation and capture its ordered in-memory snapshot.
//! 2. Serialize/write/fsync `{path}.tmp.<sequence>`; different mutations may
//!    perform this expensive stage concurrently.
//! 3. Under a short publish mutex, rename the newest prepared snapshot to
//!    `{path}`, then fsync the containing directory. A snapshot that was
//!    superseded by a newer published sequence is discarded, never allowed to
//!    overwrite newer state.
//!
//! # Recovery
//!
//! On daemon startup:
//! - If `meta.json` exists: load, validate, and populate MemStore.
//! - If missing: start with fresh MemStore (root + bootstrap files).
//! - Invalid JSON/state or a leftover temporary commit fails closed. It is not
//!   silently replaced with a fresh namespace.
//!
//! # Performance
//!
//! This is intentionally simple (serialize entire state on every write) because:
//! - Metadata is small (< 1MB for thousands of files)
//! - Writes are infrequent in the bootstrap model
//! - JSON is human-readable for debugging
//!
//! A production system would use Redis/TiKV instead (see meta.rs module docs).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
#[cfg(test)]
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

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
    pub(crate) inodes: HashMap<u64, Inode>,
    pub(crate) dir_entries: HashMap<u64, HashMap<String, u64>>,
    pub(crate) slices: HashMap<u64, HashMap<u32, Vec<Slice>>>,
    /// Default keeps ABI-v10-era snapshots loadable after symlink support lands.
    #[serde(default)]
    pub(crate) symlink_targets: HashMap<u64, String>,
    /// Default keeps pre-Step-30 snapshots loadable with an empty GC queue.
    #[serde(default)]
    pub(crate) pending_garbage: HashSet<String>,
    pub(crate) next_inode_id: u64,
}

/// A MetaStore implementation that persists to a local JSON file.
///
/// Wraps a MemStore and syncs to disk after every mutation.
pub struct FileMetaStore {
    mem: MemStore,
    path: PathBuf,
    /// Orders only the fast in-memory mutation + snapshot capture. Slow JSON
    /// encoding, file write, and fsync happen after this lock is released.
    prepare_sync: Mutex<()>,
    /// Serializes the final rename and prevents an older snapshot from
    /// replacing a newer one.
    publish_sync: Mutex<PublishState>,
    next_sequence: AtomicU64,
    persist_active: AtomicUsize,
    persist_peak: AtomicUsize,
    #[cfg(test)]
    persist_barrier: StdMutex<Option<Arc<tokio::sync::Barrier>>>,
}

#[derive(Default)]
struct PublishState {
    published_sequence: u64,
}

struct PersistActivity<'a> {
    active: &'a AtomicUsize,
}

impl Drop for PersistActivity<'_> {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

impl FileMetaStore {
    /// Creates a new FileMetaStore, loading from `path` if it exists.
    ///
    /// If the file doesn't exist, starts with a fresh MemStore (root +
    /// bootstrap files: remote.txt, writable.dat). Existing invalid or
    /// half-committed state is rejected.
    pub async fn new(path: PathBuf) -> std::io::Result<Self> {
        Self::reject_incomplete_commit(&path).await?;
        let mem = if path.exists() {
            let loaded = Self::load_from_disk(&path).await.map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "metadata {} failed validation; refusing fresh fallback: {error}",
                        path.display()
                    ),
                )
            })?;
            eprintln!("[meta_persist] Loaded metadata from {}", path.display());
            loaded
        } else {
            eprintln!(
                "[meta_persist] No existing metadata at {}, starting fresh",
                path.display()
            );
            MemStore::new()
        };

        Ok(FileMetaStore {
            mem,
            path,
            prepare_sync: Mutex::new(()),
            publish_sync: Mutex::new(PublishState::default()),
            next_sequence: AtomicU64::new(1),
            persist_active: AtomicUsize::new(0),
            persist_peak: AtomicUsize::new(0),
            #[cfg(test)]
            persist_barrier: StdMutex::new(None),
        })
    }

    async fn reject_incomplete_commit(path: &Path) -> std::io::Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = path.file_name().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "metadata path has no file name")
        })?;
        let numbered_prefix = format!("{}.tmp.", file_name.to_string_lossy());
        let legacy_tmp = path.with_extension("tmp");
        if legacy_tmp.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("incomplete metadata commit remains at {}", legacy_tmp.display()),
            ));
        }
        let mut entries = match fs::read_dir(parent).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        while let Some(entry) = entries.next_entry().await? {
            if entry.file_name().to_string_lossy().starts_with(&numbered_prefix) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "incomplete metadata commit remains at {}",
                        entry.path().display()
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Loads a MemStore from disk (internal helper).
    async fn load_from_disk(path: &Path) -> std::io::Result<MemStore> {
        let data = fs::read(path).await?;
        let snapshot: MetaSnapshot = serde_json::from_slice(&data)?;
        Self::validate_snapshot(&snapshot)?;

        Ok(MemStore::from_snapshot(snapshot))
    }

    fn validate_snapshot(snapshot: &MetaSnapshot) -> std::io::Result<()> {
        let invalid = |message: String| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, message)
        };
        let root = snapshot
            .inodes
            .get(&crate::fs_model::ROOT_INODE)
            .ok_or_else(|| invalid("metadata snapshot has no root inode".into()))?;
        if !root.is_dir() {
            return Err(invalid("metadata root inode is not a directory".into()));
        }
        if snapshot
            .inodes
            .keys()
            .max()
            .is_some_and(|maximum| snapshot.next_inode_id <= *maximum)
        {
            return Err(invalid(
                "metadata next_inode_id does not exceed existing inode ids".into(),
            ));
        }
        for (parent, entries) in &snapshot.dir_entries {
            if !snapshot.inodes.get(parent).is_some_and(Inode::is_dir) {
                return Err(invalid(format!(
                    "directory entry parent {parent} is absent or not a directory"
                )));
            }
            for (name, inode) in entries {
                if name.is_empty() || name.contains('/') || name == "." || name == ".." {
                    return Err(invalid(format!("invalid persisted directory name {name:?}")));
                }
                if !snapshot.inodes.contains_key(inode) {
                    return Err(invalid(format!(
                        "directory entry {parent}/{name} references missing inode {inode}"
                    )));
                }
            }
        }
        for inode in snapshot.slices.keys() {
            if !snapshot.inodes.contains_key(inode) {
                return Err(invalid(format!("slices reference missing inode {inode}")));
            }
        }
        for (inode, target) in &snapshot.symlink_targets {
            if target.is_empty()
                || !snapshot
                    .inodes
                    .get(inode)
                    .is_some_and(Inode::is_symlink)
            {
                return Err(invalid(format!(
                    "symlink target references invalid inode {inode}"
                )));
            }
        }
        for (inode, metadata) in &snapshot.inodes {
            if metadata.is_symlink() && !snapshot.symlink_targets.contains_key(inode) {
                return Err(invalid(format!("symlink inode {inode} has no target")));
            }
        }
        Ok(())
    }

    fn begin_persist_activity(&self, sequence: u64) -> PersistActivity<'_> {
        let active = self.persist_active.fetch_add(1, Ordering::AcqRel) + 1;
        let previous_peak = self.persist_peak.fetch_max(active, Ordering::AcqRel);
        if active > previous_peak {
            println!(
                "kestrelfs-daemon: META-PERSIST parallel active={active} peak={active} sequence={sequence}"
            );
        }
        PersistActivity {
            active: &self.persist_active,
        }
    }

    fn temporary_path(&self, sequence: u64) -> PathBuf {
        let mut name = self
            .path
            .file_name()
            .expect("validated metadata path has a file name")
            .to_os_string();
        name.push(format!(".tmp.{sequence}"));
        self.path.with_file_name(name)
    }

    async fn persist_snapshot(
        &self,
        sequence: u64,
        snapshot: MetaSnapshot,
    ) -> std::io::Result<()> {
        let _activity = self.begin_persist_activity(sequence);
        #[cfg(test)]
        {
            let barrier = self.persist_barrier.lock().unwrap().clone();
            if let Some(barrier) = barrier {
                barrier.wait().await;
            }
        }
        let json = serde_json::to_vec_pretty(&snapshot)?;
        let tmp_path = self.temporary_path(sequence);
        let write_result = async {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)
                .await?;
            file.write_all(&json).await?;
            file.sync_all().await?;
            drop(file);
            Ok::<(), std::io::Error>(())
        }
        .await;
        if let Err(error) = write_result {
            let _ = fs::remove_file(&tmp_path).await;
            return Err(error);
        }

        let mut publish = self.publish_sync.lock().await;
        if sequence <= publish.published_sequence {
            fs::remove_file(&tmp_path).await?;
            return Ok(());
        }
        fs::rename(&tmp_path, &self.path).await?;
        // Once rename succeeds, never permit an older snapshot to overwrite
        // it, even if the directory fsync below reports an uncertain result.
        publish.published_sequence = sequence;
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::File::open(parent).await?.sync_all().await
    }

    async fn mutate_and_persist<T, F>(&self, mutation: F) -> Result<T>
    where
        F: Future<Output = Result<T>>,
    {
        let prepare = self.prepare_sync.lock().await;
        let value = mutation.await?;
        let snapshot = self.mem.snapshot().await;
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        drop(prepare);
        self.persist_snapshot(sequence, snapshot)
            .await
            .map_err(|error| {
                eprintln!(
                    "kestrelfs-daemon: META-PERSIST sequence={sequence} failed: {error}"
                );
                MetaError::Io
            })?;
        Ok(value)
    }

    async fn persist_current_snapshot(&self) -> std::io::Result<()> {
        let prepare = self.prepare_sync.lock().await;
        let snapshot = self.mem.snapshot().await;
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        drop(prepare);
        self.persist_snapshot(sequence, snapshot).await
    }

    async fn sync_existing(&self) -> std::io::Result<()> {
        if !self.path.exists() {
            self.persist_current_snapshot().await?;
        }
        let _publish = self.publish_sync.lock().await;
        fs::File::open(&self.path).await?.sync_all().await?;
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::File::open(parent).await?.sync_all().await
    }

    #[cfg(test)]
    fn set_persist_barrier(&self, barrier: Option<Arc<tokio::sync::Barrier>>) {
        *self.persist_barrier.lock().unwrap() = barrier;
    }

    #[cfg(test)]
    fn persistence_peak(&self) -> usize {
        self.persist_peak.load(Ordering::Acquire)
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

    async fn set_attrs(
        &self,
        inode: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        atime: Option<u64>,
        mtime: Option<u64>,
    ) -> Result<crate::fs_model::Inode> {
        self.mutate_and_persist(self.mem.set_attrs(inode, mode, uid, gid, atime, mtime))
            .await
    }

    async fn read_slices(&self, inode: u64, chunk_idx: u32) -> Result<Vec<Slice>> {
        self.mem.read_slices(inode, chunk_idx).await
    }

    async fn referenced_keys(&self, inode: u64) -> Result<Vec<String>> {
        self.mem.referenced_keys(inode).await
    }

    async fn all_referenced_keys(&self) -> Result<Vec<String>> {
        self.mem.all_referenced_keys().await
    }

    async fn sync_persistence(&self) -> Result<()> {
        self.sync_existing().await.map_err(|_| MetaError::Io)
    }

    async fn create(&self, parent: u64, name: &str, mode: u32) -> Result<u64> {
        self.mutate_and_persist(self.mem.create(parent, name, mode))
            .await
    }

    async fn symlink(&self, parent: u64, name: &str, target: &str) -> Result<u64> {
        self.mutate_and_persist(self.mem.symlink(parent, name, target))
            .await
    }

    async fn readlink(&self, inode: u64) -> Result<String> {
        self.mem.readlink(inode).await
    }

    async fn append_slice(&self, inode: u64, slice: Slice) -> Result<()> {
        self.mutate_and_persist(self.mem.append_slice(inode, slice))
            .await
    }

    async fn truncate(&self, inode: u64, new_size: u64) -> Result<Vec<String>> {
        self.mutate_and_persist(self.mem.truncate(inode, new_size))
            .await
    }

    async fn readdir(&self, inode: u64) -> Result<Vec<crate::meta::DirectoryEntry>> {
        self.mem.readdir(inode).await
    }

    async fn mkdir(&self, parent: u64, name: &str, mode: u32) -> Result<u64> {
        self.mutate_and_persist(self.mem.mkdir(parent, name, mode))
            .await
    }

    async fn link(&self, parent: u64, name: &str, inode: u64) -> Result<u32> {
        self.mutate_and_persist(self.mem.link(parent, name, inode))
            .await
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
        self.mutate_and_persist(
            self.mem
                .unlink_with_lifecycle(parent, name, defer_reclaim),
        )
        .await
    }

    async fn finalize_orphan(&self, inode: u64) -> Result<Vec<String>> {
        self.mutate_and_persist(self.mem.finalize_orphan(inode))
            .await
    }

    async fn rename(
        &self,
        old_parent: u64,
        old_name: &str,
        new_parent: u64,
        new_name: &str,
    ) -> Result<Vec<String>> {
        self.mutate_and_persist(self.mem.rename(old_parent, old_name, new_parent, new_name))
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
            old_parent, old_name, new_parent, new_name, flags, false,
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
        self.mutate_and_persist(
            self.mem.rename_with_lifecycle(
                old_parent, old_name, new_parent, new_name, flags, defer_reclaim,
            ),
        )
        .await
    }

    async fn pending_garbage(&self) -> Result<Vec<String>> {
        self.mem.pending_garbage().await
    }

    async fn acknowledge_garbage(&self, keys: &[String]) -> Result<()> {
        self.mutate_and_persist(self.mem.acknowledge_garbage(keys))
            .await
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
            symlink_targets: inner.symlink_targets.clone(),
            pending_garbage: inner.pending_garbage.clone(),
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
                symlink_targets: snapshot.symlink_targets,
                pending_garbage: snapshot.pending_garbage,
            }),
            next_inode_id: AtomicU64::new(snapshot.next_inode_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_model::{ROOT_INODE, S_IFCHR, S_IFDIR, S_IFLNK, S_IFREG};
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
    async fn mknod_whiteout_and_readdir_type_survive_reload() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");
        let marker;
        {
            let store = FileMetaStore::new(path.clone()).await.unwrap();
            marker = store
                .create(ROOT_INODE, "mknod-whiteout", S_IFCHR)
                .await
                .unwrap();
        }

        let restored = FileMetaStore::new(path).await.unwrap();
        assert_eq!(
            restored.lookup(ROOT_INODE, "mknod-whiteout").await.unwrap(),
            marker
        );
        assert_eq!(restored.getattr(marker).await.unwrap().mode, S_IFCHR);
        assert!(restored.readdir(ROOT_INODE).await.unwrap().iter().any(|entry| {
            entry.inode_id == marker
                && entry.name == "mknod-whiteout"
                && entry.mode == S_IFCHR
        }));
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
    async fn hard_links_and_link_count_survive_reload() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");
        let inode;
        {
            let store = FileMetaStore::new(path.clone()).await.unwrap();
            inode = store
                .create(ROOT_INODE, "persistent-source", 0o644)
                .await
                .unwrap();
            assert_eq!(
                store
                    .link(ROOT_INODE, "persistent-alias", inode)
                    .await
                    .unwrap(),
                2
            );
        }

        let restored = FileMetaStore::new(path).await.unwrap();
        assert_eq!(
            restored
                .lookup(ROOT_INODE, "persistent-source")
                .await
                .unwrap(),
            inode
        );
        assert_eq!(
            restored
                .lookup(ROOT_INODE, "persistent-alias")
                .await
                .unwrap(),
            inode
        );
        assert_eq!(restored.getattr(inode).await.unwrap().nlink, 2);
        assert!(restored
            .unlink(ROOT_INODE, "persistent-source")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(restored.getattr(inode).await.unwrap().nlink, 1);
    }

    #[tokio::test]
    async fn deferred_orphan_and_final_gc_survive_reload() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");
        let slice = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::new_v4(),
            chunk_offset: 0,
            length: 32,
            written_at: current_unix_time(),
        };
        let key = slice.block_key(0);
        let inode;
        {
            let store = FileMetaStore::new(path.clone()).await.unwrap();
            inode = store.create(ROOT_INODE, "open-orphan", 0o644).await.unwrap();
            store.append_slice(inode, slice).await.unwrap();
            assert!(store
                .unlink_with_lifecycle(ROOT_INODE, "open-orphan", true)
                .await.unwrap().is_empty());
            let attrs = store
                .set_attrs(
                    inode,
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
        }
        {
            let restarted = FileMetaStore::new(path.clone()).await.unwrap();
            assert_eq!(restarted.getattr(inode).await.unwrap().nlink, 0);
            assert_eq!(restarted.getattr(inode).await.unwrap().mode, S_IFREG | 0o600);
            assert_eq!(restarted.getattr(inode).await.unwrap().uid, 1234);
            assert_eq!(restarted.getattr(inode).await.unwrap().gid, 2345);
            assert_eq!(restarted.getattr(inode).await.unwrap().atime, 1_577_836_800);
            assert_eq!(restarted.getattr(inode).await.unwrap().mtime, 1_577_836_801);
            assert!(restarted.pending_garbage().await.unwrap().is_empty());
            assert_eq!(restarted.finalize_orphan(inode).await.unwrap(), vec![key.clone()]);
        }
        let restarted = FileMetaStore::new(path).await.unwrap();
        assert!(matches!(restarted.getattr(inode).await, Err(MetaError::NotFound)));
        assert_eq!(restarted.pending_garbage().await.unwrap(), vec![key]);
    }

    #[tokio::test]
    async fn modes_and_directory_nlinks_survive_reload() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");
        let file;
        let left;
        let right;
        {
            let store = FileMetaStore::new(path.clone()).await.unwrap();
            file = store.create(ROOT_INODE, "mode-file", 0o2640).await.unwrap();
            left = store.mkdir(ROOT_INODE, "left", 0o1711).await.unwrap();
            right = store.mkdir(ROOT_INODE, "right", 0o750).await.unwrap();
            assert_eq!(store.set_mode(file, S_IFDIR | 0o6751).await.unwrap(), S_IFREG | 0o6751);
            assert_eq!(store.set_mode(left, S_IFREG | 0o1770).await.unwrap(), S_IFDIR | 0o1770);
            store.mkdir(left, "child", 0o700).await.unwrap();
            store
                .rename(left, "child", right, "moved-child")
                .await
                .unwrap();
        }

        let restored = FileMetaStore::new(path).await.unwrap();
        assert_eq!(restored.getattr(file).await.unwrap().mode, S_IFREG | 0o6751);
        assert_eq!(restored.getattr(left).await.unwrap().mode, S_IFDIR | 0o1770);
        assert_eq!(restored.getattr(left).await.unwrap().nlink, 2);
        assert_eq!(restored.getattr(right).await.unwrap().nlink, 3);
        assert_eq!(restored.getattr(ROOT_INODE).await.unwrap().nlink, 4);
    }

    #[tokio::test]
    async fn rename_noreplace_failure_is_persistently_unchanged() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");
        let source;
        let target;
        {
            let store = FileMetaStore::new(path.clone()).await.unwrap();
            source = store.create(ROOT_INODE, "source", 0o644).await.unwrap();
            target = store.create(ROOT_INODE, "target", 0o644).await.unwrap();
            assert!(matches!(
                store
                    .rename_with_flags(
                        ROOT_INODE,
                        "source",
                        ROOT_INODE,
                        "target",
                        crate::meta::RENAME_NOREPLACE,
                    )
                    .await,
                Err(MetaError::AlreadyExists)
            ));
        }

        let restored = FileMetaStore::new(path).await.unwrap();
        assert_eq!(restored.lookup(ROOT_INODE, "source").await.unwrap(), source);
        assert_eq!(restored.lookup(ROOT_INODE, "target").await.unwrap(), target);
        assert!(restored.pending_garbage().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rename_exchange_survives_reload_as_one_namespace_change() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");
        let left;
        let right;
        {
            let store = FileMetaStore::new(path.clone()).await.unwrap();
            left = store.create(ROOT_INODE, "exchange-left", 0o640).await.unwrap();
            right = store.create(ROOT_INODE, "exchange-right", 0o600).await.unwrap();
            store
                .rename_with_flags(
                    ROOT_INODE,
                    "exchange-left",
                    ROOT_INODE,
                    "exchange-right",
                    crate::meta::RENAME_EXCHANGE,
                )
                .await
                .unwrap();
        }

        let restored = FileMetaStore::new(path).await.unwrap();
        assert_eq!(restored.lookup(ROOT_INODE, "exchange-left").await.unwrap(), right);
        assert_eq!(restored.lookup(ROOT_INODE, "exchange-right").await.unwrap(), left);
        assert!(restored.pending_garbage().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rename_whiteout_marker_survives_reload() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");
        let source;
        {
            let store = FileMetaStore::new(path.clone()).await.unwrap();
            source = store.create(ROOT_INODE, "whiteout-old", 0o640).await.unwrap();
            store
                .rename_with_flags(
                    ROOT_INODE,
                    "whiteout-old",
                    ROOT_INODE,
                    "whiteout-new",
                    crate::meta::RENAME_WHITEOUT,
                )
                .await
                .unwrap();
        }

        let restored = FileMetaStore::new(path).await.unwrap();
        assert_eq!(restored.lookup(ROOT_INODE, "whiteout-new").await.unwrap(), source);
        let marker = restored.lookup(ROOT_INODE, "whiteout-old").await.unwrap();
        assert_ne!(marker, source);
        assert_eq!(restored.getattr(marker).await.unwrap().mode, S_IFCHR);
        assert!(restored
            .readdir(ROOT_INODE)
            .await
            .unwrap()
            .iter()
            .any(|entry| {
                entry.inode_id == marker
                    && entry.name == "whiteout-old"
                    && entry.mode == S_IFCHR
            }));
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
    async fn garbage_queue_survives_reload_and_ack_is_persistent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");
        let slice = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::new_v4(),
            chunk_offset: 0,
            length: 64,
            written_at: current_unix_time(),
        };
        let key = slice.block_key(0);

        {
            let store = FileMetaStore::new(path.clone()).await.unwrap();
            let inode = store.create(ROOT_INODE, "queued-gc", 0o644).await.unwrap();
            store.append_slice(inode, slice).await.unwrap();
            assert_eq!(
                store.unlink(ROOT_INODE, "queued-gc").await.unwrap(),
                vec![key.clone()]
            );
        }

        {
            let restarted = FileMetaStore::new(path.clone()).await.unwrap();
            assert_eq!(restarted.pending_garbage().await.unwrap(), vec![key.clone()]);
            restarted
                .acknowledge_garbage(std::slice::from_ref(&key))
                .await
                .unwrap();
        }

        let restarted = FileMetaStore::new(path).await.unwrap();
        assert!(restarted.pending_garbage().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn symlink_and_renamed_path_survive_reload() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");
        let target = "../persistent-target.txt";
        let inode;
        {
            let store = FileMetaStore::new(path.clone()).await.unwrap();
            inode = store
                .symlink(ROOT_INODE, "persistent-link", target)
                .await
                .unwrap();
            store
                .rename(
                    ROOT_INODE,
                    "persistent-link",
                    ROOT_INODE,
                    "persistent-renamed-link",
                )
                .await
                .unwrap();
        }

        let restored = FileMetaStore::new(path).await.unwrap();
        let found = restored
            .lookup(ROOT_INODE, "persistent-renamed-link")
            .await
            .unwrap();
        assert_eq!(found, inode);
        assert_eq!(restored.readlink(found).await.unwrap(), target);
        let attrs = restored.getattr(found).await.unwrap();
        assert_eq!(attrs.mode & 0o170000, S_IFLNK);
        assert_eq!(attrs.size, target.len() as u64);
    }

    #[tokio::test]
    async fn step58_invalid_json_fails_closed() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");

        fs::write(&path, b"{ invalid json }").await.unwrap();

        let error = FileMetaStore::new(path).await.err().unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        println!("STEP58_FAILCLOSED_CORRUPT_PASS");
    }

    #[tokio::test]
    async fn step58_half_commit_fails_closed() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");
        {
            let store = FileMetaStore::new(path.clone()).await.unwrap();
            store
                .create(ROOT_INODE, "committed-before-half", 0o644)
                .await
                .unwrap();
        }
        fs::write(dir.path().join("meta.json.tmp.999"), b"partial")
            .await
            .unwrap();

        let error = FileMetaStore::new(path).await.err().unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        println!("STEP58_FAILCLOSED_HALF_COMMIT_PASS");
    }

    #[tokio::test]
    async fn step58_different_inode_persistence_overlaps_and_recovers() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");
        let store = Arc::new(FileMetaStore::new(path.clone()).await.unwrap());
        let first = store.create(ROOT_INODE, "parallel-meta-a", 0o644).await.unwrap();
        let second = store.create(ROOT_INODE, "parallel-meta-b", 0o644).await.unwrap();
        store.set_persist_barrier(Some(Arc::new(tokio::sync::Barrier::new(2))));

        let first_slice = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::new_v4(),
            chunk_offset: 0,
            length: 4096,
            written_at: current_unix_time(),
        };
        let second_slice = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::new_v4(),
            chunk_offset: 0,
            length: 8192,
            written_at: current_unix_time(),
        };
        let first_task = {
            let store = Arc::clone(&store);
            let slice = first_slice.clone();
            tokio::spawn(async move { store.append_slice(first, slice).await })
        };
        let second_task = {
            let store = Arc::clone(&store);
            let slice = second_slice.clone();
            tokio::spawn(async move { store.append_slice(second, slice).await })
        };
        first_task.await.unwrap().unwrap();
        second_task.await.unwrap().unwrap();
        store.set_persist_barrier(None);
        assert!(store.persistence_peak() >= 2);
        drop(store);

        let restored = FileMetaStore::new(path).await.unwrap();
        let restored_first = restored.read_slices(first, 0).await.unwrap();
        let restored_second = restored.read_slices(second, 0).await.unwrap();
        assert_eq!(restored_first.len(), 1);
        assert_eq!(restored_first[0].slice_id, first_slice.slice_id);
        assert_eq!(restored_second.len(), 1);
        assert_eq!(restored_second[0].slice_id, second_slice.slice_id);
        println!("STEP58_META_PARALLEL_UNIT_PASS peak=2");
    }

    #[tokio::test]
    async fn step58_same_inode_mutations_remain_ordered_and_durable() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");
        let store = Arc::new(FileMetaStore::new(path.clone()).await.unwrap());
        let inode = store.create(ROOT_INODE, "ordered-meta", 0o644).await.unwrap();
        let first = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::new_v4(),
            chunk_offset: 0,
            length: 4096,
            written_at: 1,
        };
        let second = Slice {
            chunk_index: 0,
            slice_id: uuid::Uuid::new_v4(),
            chunk_offset: 4096,
            length: 4096,
            written_at: 2,
        };
        let (left, right) = tokio::join!(
            store.append_slice(inode, first.clone()),
            store.append_slice(inode, second.clone())
        );
        left.unwrap();
        right.unwrap();
        drop(store);

        let restored = FileMetaStore::new(path).await.unwrap();
        let slices = restored.read_slices(inode, 0).await.unwrap();
        assert_eq!(slices.len(), 2);
        assert_eq!(slices[0].slice_id, first.slice_id);
        assert_eq!(slices[1].slice_id, second.slice_id);
        assert_eq!(restored.getattr(inode).await.unwrap().size, 8192);
        println!("STEP58_META_SAME_INODE_ORDER_PASS");
    }

    #[tokio::test]
    async fn step58_semantically_invalid_snapshot_fails_closed() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("meta.json");
        let store = FileMetaStore::new(path.clone()).await.unwrap();
        store.sync_persistence().await.unwrap();
        drop(store);

        let bytes = fs::read(&path).await.unwrap();
        let mut snapshot: MetaSnapshot = serde_json::from_slice(&bytes).unwrap();
        snapshot.next_inode_id = ROOT_INODE;
        fs::write(&path, serde_json::to_vec_pretty(&snapshot).unwrap())
            .await
            .unwrap();

        let error = FileMetaStore::new(path).await.err().unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        println!("STEP58_FAILCLOSED_SEMANTIC_PASS");
    }
}
