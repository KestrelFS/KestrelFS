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
use std::sync::{Arc, RwLock};
use thiserror::Error;

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
    ///
    /// Not yet returned by `MemObjectStore` (which is pure in-memory and
    /// never fails beyond NotFound), but required by future Redis/S3
    /// backends - allowed as dead_code until then.
    #[allow(dead_code)]
    #[error("I/O error: {0}")]
    Io(String),
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
///   or MVCC yet - a later Phase 4 concern once we add garbage
///   collection and need to track object reference counts).
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
    async fn clone_shares_underlying_data() {
        let store1 = MemObjectStore::new();
        let store2 = store1.clone();

        store1.put("shared_key".to_string(), b"data".to_vec()).await.unwrap();
        let retrieved = store2.get("shared_key").await.unwrap();

        assert_eq!(retrieved, b"data");
    }

    #[tokio::test]
    async fn concurrent_gets_do_not_block() {
        let store = MemObjectStore::new();
        store.put("k".to_string(), b"v".to_vec()).await.unwrap();

        let (r1, r2, r3) = tokio::join!(
            store.get("k"),
            store.get("k"),
            store.get("k")
        );

        assert_eq!(r1.unwrap(), b"v");
        assert_eq!(r2.unwrap(), b"v");
        assert_eq!(r3.unwrap(), b"v");
    }
}
