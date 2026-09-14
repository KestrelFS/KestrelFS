// SPDX-License-Identifier: GPL-2.0
//! Block/chunk storage abstraction for KestrelFS Phase 3 step 3.
//!
//! This module defines [`ObjectStore`], the trait every storage backend
//! must implement (whether in-memory [`MemObjectStore`], Redis, S3, or
//! a future NVMe-cached hybrid), and the first concrete implementation:
//! a pure-memory `HashMap` that holds blocks keyed by content-addressed
//! (or at least stable, caller-chosen) string keys.
//!
//! # Why "ObjectStore" instead of "BlockStore"
//!
//! The name deliberately mirrors S3/blob-store terminology rather than
//! traditional block-device language: each "object" here is a
//! variable-length byte blob identified by a string key (not a fixed
//! 4 KiB block at a numeric LBA), and the trait makes no atomicity or
//! ordering guarantees beyond "a `get` after a `put` of the same key
//! returns the most recent value". This matches what both S3 and Redis
//! actually provide, and avoids overloading "block" (which in
//! filesystem literature often implies a fixed size, alignment, and
//! on-disk addressing scheme that doesn't exist here yet).
//!
//! # Thread safety & async
//!
//! Like [`MetaStore`](crate::meta::MetaStore), every method is `async`
//! from day one (even though `MemObjectStore`'s `HashMap` is entirely
//! synchronous under a `RwLock`), so the trait's shape already matches
//! what a real network-backed store will require. `MemObjectStore` is
//! internally `Arc<RwLock<...>>` so it can be cheaply cloned and passed
//! to multiple async tasks/threads without fear of data races.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use thiserror::Error;
use tokio::fs;
use tokio::io::AsyncWriteExt;

/// Errors returned by [`ObjectStore`] implementations.
#[derive(Error, Debug, Clone)]
pub enum ObjectStoreError {
    /// The requested key does not exist in the store.
    #[error("object not found: {0}")]
    NotFound(String),

    /// A store-specific I/O or network error occurred (Redis timeout,
    /// S3 403, disk full, etc.). The inner string is a human-readable
    /// description; callers should generally surface this as `-EIO` to
    /// the kernel.
    #[error("I/O error: {0}")]
    Io(String),

    /// The provided key is invalid (e.g., contains ".." or is an absolute path).
    #[error("invalid key: {0}")]
    InvalidKey(String),
}

pub type Result<T> = std::result::Result<T, ObjectStoreError>;

/// Storage backend for fixed-size or variable-length data blocks.
///
/// Implementations may be in-memory ([`MemObjectStore`]), Redis-backed,
/// S3-backed, or a future NVMe-cached hybrid. Every method is `async`
/// to accommodate network I/O in real backends, even though the
/// in-memory version never actually awaits anything.
///
/// # Concurrency & consistency
///
/// - `get()` and `put()` for *different* keys can safely run
///   concurrently (no ordering guarantee between them).
/// - `put()` for the *same* key is last-write-wins (no compare-and-swap
///   or MVCC yet. Step 15 GC confirms references in MetaStore before calling
///   `delete`; distributed backends may later replace that scan with durable
///   reference counts.
/// - `get()` after `put()` of the same key sees the new value (no
///   stale-read window), modulo the usual async/.await interleaving
///   caveats: if two tasks both `put(k, ...)` concurrently, a
///   subsequent `get(k)` might see either value, but it will see *one*
///   of them, not uninitialized garbage or a torn write.
///
/// This is exactly the consistency model S3/Redis already provide, so
/// the trait doesn't invent stricter semantics that would be hard to
/// preserve once we swap in a real network backend.
#[async_trait::async_trait]
pub trait ObjectStore: Send + Sync {
    /// Retrieves the value associated with `key`.
    ///
    /// # Errors
    ///
    /// [`ObjectStoreError::NotFound`] if `key` does not exist.
    /// [`ObjectStoreError::Io`] for backend-specific failures (network
    /// timeout, disk read error, etc.).
    async fn get(&self, key: &str) -> Result<Vec<u8>>;

    /// Stores `value` under `key`, overwriting any existing value.
    ///
    /// # Errors
    ///
    /// [`ObjectStoreError::Io`] for backend-specific failures (out of
    /// memory, disk full, Redis READONLY mode, S3 write quota exceeded,
    /// etc.). A successful return means the value is durably stored
    /// according to whatever "durable" means for this backend (for
    /// `MemObjectStore`: held in RAM; for Redis: ACKed by the server;
    /// for S3: PUT request returned 200).
    async fn put(&self, key: String, value: Vec<u8>) -> Result<()>;

    /// Deletes `key`. Deletion is idempotent: an already-absent object is a
    /// successful outcome, which makes post-metadata-commit GC safe to retry.
    async fn delete(&self, key: &str) -> Result<()>;
}

/// In-memory, `HashMap`-backed [`ObjectStore`] for Phase 3
/// bootstrapping (no Redis/S3 dependency yet).
///
/// Internally `Arc<RwLock<HashMap<String, Vec<u8>>>>` so it can be
/// cheaply cloned and shared across async tasks without Arc-wrapping at
/// the call site. Every method acquires either a read lock (`get`) or
/// write lock (`put`), so concurrent calls for *different* keys don't
/// block each other beyond the lock's fairness overhead, and concurrent
/// `get`s for the *same* key run in parallel (RwLock allows multiple
/// readers).
///
/// # Not suitable for production
///
/// - No persistence (process restart loses everything).
/// - No memory bound (a write-heavy workload will OOM the daemon).
/// - No eviction policy (even if we add an LRU, Phase 3 has no
///   write-back to S3 yet, so eviction == data loss).
///
/// This is purely a Phase 3 step 3 scaffolding piece, exercised by
/// reading `/mnt/kestrelfs/remote.txt` after the daemon seeds a single
/// block at startup. Real deployments will use `RedisObjectStore` or
/// `S3CachedObjectStore` (future Phase 4+ work).
#[derive(Clone)]
pub struct MemObjectStore {
    inner: Arc<RwLock<HashMap<String, Vec<u8>>>>,
}

impl MemObjectStore {
    /// Creates an empty in-memory store.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for MemObjectStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ObjectStore for MemObjectStore {
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        let map = self.inner.read().unwrap();
        map.get(key)
            .cloned()
            .ok_or_else(|| ObjectStoreError::NotFound(key.to_string()))
    }

    async fn put(&self, key: String, value: Vec<u8>) -> Result<()> {
        let mut map = self.inner.write().unwrap();
        map.insert(key, value);
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.inner.write().unwrap().remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_nonexistent_key_is_not_found() {
        let store = MemObjectStore::new();
        let result = store.get("does_not_exist").await;
        assert!(matches!(result, Err(ObjectStoreError::NotFound(_))));
    }

    #[tokio::test]
    async fn put_then_get_returns_same_value() {
        let store = MemObjectStore::new();
        let key = "test_key".to_string();
        let value = b"hello, object store".to_vec();

        store.put(key.clone(), value.clone()).await.unwrap();
        let retrieved = store.get(&key).await.unwrap();

        assert_eq!(retrieved, value);
    }

    #[tokio::test]
    async fn put_overwrites_existing_value() {
        let store = MemObjectStore::new();
        let key = "overwrite_test".to_string();

        store.put(key.clone(), b"old".to_vec()).await.unwrap();
        store.put(key.clone(), b"new".to_vec()).await.unwrap();

        let retrieved = store.get(&key).await.unwrap();
        assert_eq!(retrieved, b"new");
    }

    #[tokio::test]
    async fn delete_removes_value_and_is_idempotent() {
        let store = MemObjectStore::new();
        let key = "delete_test";
        store.put(key.to_string(), b"value".to_vec()).await.unwrap();
        store.delete(key).await.unwrap();
        assert!(matches!(
            store.get(key).await,
            Err(ObjectStoreError::NotFound(_))
        ));
        store.delete(key).await.unwrap();
    }

    #[tokio::test]
    async fn clone_shares_underlying_data() {
        let store1 = MemObjectStore::new();
        let store2 = store1.clone();

        store1
            .put("shared_key".to_string(), b"data".to_vec())
            .await
            .unwrap();
        let retrieved = store2.get("shared_key").await.unwrap();

        assert_eq!(retrieved, b"data");
    }

    #[tokio::test]
    async fn concurrent_gets_do_not_block() {
        let store = MemObjectStore::new();
        store.put("k".to_string(), b"v".to_vec()).await.unwrap();

        let (r1, r2, r3) = tokio::join!(store.get("k"), store.get("k"), store.get("k"));

        assert_eq!(r1.unwrap(), b"v");
        assert_eq!(r2.unwrap(), b"v");
        assert_eq!(r3.unwrap(), b"v");
    }
}

/// Local filesystem-based object store for persistent block storage.
///
/// Stores blocks as individual files under a root directory, organized by key.
/// Keys are expected to be in the format "{uuid}/{block_idx}" and are sanitized
/// to prevent directory traversal attacks.
///
/// # Atomicity
///
/// Writes use atomic rename: data is first written to a temporary file (with
/// .tmp.{random} suffix), then atomically renamed to the final path. This ensures
/// readers never see partial writes, even if the daemon crashes mid-write.
///
/// # Directory Structure
///
/// Given root `/data` and key `abc-123/0`, the block is stored at:
///   `/data/abc-123/0`
///
/// The parent directory (`abc-123`) is created on-demand during put().
#[derive(Clone)]
pub struct LocalFsObjectStore {
    root: Arc<PathBuf>,
}

impl LocalFsObjectStore {
    /// Create a new LocalFsObjectStore with the given root directory.
    ///
    /// Creates the root directory if it doesn't exist. Returns an error if
    /// the path exists but is not a directory, or if creation fails.
    pub async fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();

        // Create root directory if it doesn't exist
        fs::create_dir_all(&root).await.map_err(|e| {
            ObjectStoreError::Io(format!("Failed to create root directory: {}", e))
        })?;

        Ok(Self {
            root: Arc::new(root),
        })
    }

    /// Sanitize a key to prevent directory traversal attacks.
    ///
    /// Rejects keys containing ".." or absolute paths. Returns the safe
    /// relative path within the root directory.
    fn sanitize_key(&self, key: &str) -> Result<PathBuf> {
        // Reject empty keys
        if key.is_empty() {
            return Err(ObjectStoreError::InvalidKey(
                "Key cannot be empty".to_string(),
            ));
        }

        // Reject absolute paths
        if key.starts_with('/') {
            return Err(ObjectStoreError::InvalidKey(
                "Key cannot be absolute path".to_string(),
            ));
        }

        // Check for ".." components (directory traversal)
        for component in key.split('/') {
            if component == ".." {
                return Err(ObjectStoreError::InvalidKey(
                    "Key cannot contain '..' component".to_string(),
                ));
            }
        }

        Ok(self.root.join(key))
    }
}

#[async_trait::async_trait]
impl ObjectStore for LocalFsObjectStore {
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        let path = self.sanitize_key(key)?;

        match fs::read(&path).await {
            Ok(data) => Ok(data),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(ObjectStoreError::NotFound(key.to_string()))
            }
            Err(e) => Err(ObjectStoreError::Io(format!(
                "Failed to read {}: {}",
                path.display(),
                e
            ))),
        }
    }

    async fn put(&self, key: String, value: Vec<u8>) -> Result<()> {
        let final_path = self.sanitize_key(&key)?;

        // Create parent directory if it doesn't exist
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent).await.map_err(|e| {
                ObjectStoreError::Io(format!(
                    "Failed to create parent directory for {}: {}",
                    key, e
                ))
            })?;
        }

        // Write to temporary file first (atomic write pattern)
        let temp_path = final_path.with_extension(format!(
            "tmp.{}",
            uuid::Uuid::new_v4().simple()
        ));

        let mut file = fs::File::create(&temp_path).await.map_err(|e| {
            ObjectStoreError::Io(format!("Failed to create temp file: {}", e))
        })?;

        file.write_all(&value).await.map_err(|e| {
            ObjectStoreError::Io(format!("Failed to write data: {}", e))
        })?;

        file.sync_all().await.map_err(|e| {
            ObjectStoreError::Io(format!("Failed to sync temp file: {}", e))
        })?;

        drop(file);

        // Atomic rename
        fs::rename(&temp_path, &final_path).await.map_err(|e| {
            ObjectStoreError::Io(format!("Failed to rename temp file: {}", e))
        })?;

        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let path = self.sanitize_key(key)?;
        match fs::remove_file(&path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(ObjectStoreError::Io(format!(
                    "Failed to delete {}: {}",
                    path.display(),
                    error
                )))
            }
        }

        // Block keys currently use <slice UUID>/<block index>. Removing the
        // now-empty UUID directory prevents directory-only garbage. A nonempty
        // or concurrently reused parent is intentionally left untouched.
        if let Some(parent) = path.parent() {
            if parent != self.root.as_path() {
                let _ = fs::remove_dir(parent).await;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod localfs_tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn new_creates_root_directory() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().join("new_root");

        assert!(!root.exists());

        let _store = LocalFsObjectStore::new(&root).await.unwrap();

        assert!(root.exists());
        assert!(root.is_dir());
    }

    #[tokio::test]
    async fn put_and_get_basic() {
        let temp_dir = TempDir::new().unwrap();
        let store = LocalFsObjectStore::new(temp_dir.path()).await.unwrap();

        store
            .put("test-key/0".to_string(), b"hello".to_vec())
            .await
            .unwrap();

        let retrieved = store.get("test-key/0").await.unwrap();
        assert_eq!(retrieved, b"hello");
    }

    #[tokio::test]
    async fn get_nonexistent_returns_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let store = LocalFsObjectStore::new(temp_dir.path()).await.unwrap();

        let result = store.get("nonexistent").await;

        assert!(matches!(result, Err(ObjectStoreError::NotFound(_))));
    }

    #[tokio::test]
    async fn put_overwrites_existing() {
        let temp_dir = TempDir::new().unwrap();
        let store = LocalFsObjectStore::new(temp_dir.path()).await.unwrap();

        store
            .put("key/0".to_string(), b"old".to_vec())
            .await
            .unwrap();
        store
            .put("key/0".to_string(), b"new".to_vec())
            .await
            .unwrap();

        let retrieved = store.get("key/0").await.unwrap();
        assert_eq!(retrieved, b"new");
    }

    #[tokio::test]
    async fn delete_removes_file_and_is_idempotent() {
        let temp_dir = TempDir::new().unwrap();
        let store = LocalFsObjectStore::new(temp_dir.path()).await.unwrap();
        let key = "delete-me/0";
        store.put(key.to_string(), b"data".to_vec()).await.unwrap();
        store.delete(key).await.unwrap();
        assert!(matches!(
            store.get(key).await,
            Err(ObjectStoreError::NotFound(_))
        ));
        assert!(!temp_dir.path().join("delete-me").exists());
        store.delete(key).await.unwrap();
    }

    #[tokio::test]
    async fn persistence_across_store_instances() {
        let temp_dir = TempDir::new().unwrap();

        {
            let store1 = LocalFsObjectStore::new(temp_dir.path()).await.unwrap();
            store1
                .put("persistent/0".to_string(), b"data".to_vec())
                .await
                .unwrap();
        }

        // Create new store instance pointing to same directory (simulates daemon restart)
        let store2 = LocalFsObjectStore::new(temp_dir.path()).await.unwrap();
        let retrieved = store2.get("persistent/0").await.unwrap();

        assert_eq!(retrieved, b"data");
    }

    #[tokio::test]
    async fn rejects_directory_traversal_dotdot() {
        let temp_dir = TempDir::new().unwrap();
        let store = LocalFsObjectStore::new(temp_dir.path()).await.unwrap();

        let result = store.put("../etc/passwd".to_string(), b"bad".to_vec()).await;

        assert!(matches!(result, Err(ObjectStoreError::InvalidKey(_))));
    }

    #[tokio::test]
    async fn rejects_absolute_paths() {
        let temp_dir = TempDir::new().unwrap();
        let store = LocalFsObjectStore::new(temp_dir.path()).await.unwrap();

        let result = store.put("/etc/passwd".to_string(), b"bad".to_vec()).await;

        assert!(matches!(result, Err(ObjectStoreError::InvalidKey(_))));
    }

    #[tokio::test]
    async fn handles_nested_keys() {
        let temp_dir = TempDir::new().unwrap();
        let store = LocalFsObjectStore::new(temp_dir.path()).await.unwrap();

        store
            .put("a/b/c/d".to_string(), b"nested".to_vec())
            .await
            .unwrap();

        let retrieved = store.get("a/b/c/d").await.unwrap();
        assert_eq!(retrieved, b"nested");
    }

    #[tokio::test]
    async fn concurrent_puts_do_not_corrupt() {
        let temp_dir = TempDir::new().unwrap();
        let store = LocalFsObjectStore::new(temp_dir.path()).await.unwrap();

        let (r1, r2, r3) = tokio::join!(
            store.put("key1".to_string(), b"data1".to_vec()),
            store.put("key2".to_string(), b"data2".to_vec()),
            store.put("key3".to_string(), b"data3".to_vec()),
        );

        assert!(r1.is_ok());
        assert!(r2.is_ok());
        assert!(r3.is_ok());

        assert_eq!(store.get("key1").await.unwrap(), b"data1");
        assert_eq!(store.get("key2").await.unwrap(), b"data2");
        assert_eq!(store.get("key3").await.unwrap(), b"data3");
    }
}
