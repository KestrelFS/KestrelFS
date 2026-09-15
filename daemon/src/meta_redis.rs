// SPDX-License-Identifier: Apache-2.0
//! Redis-backed metadata prototype.
//!
//! The entire [`MetaSnapshot`] is stored as JSON in one Redis key. Mutations
//! load that snapshot, apply the already-tested [`MemStore`] operation, and
//! publish the replacement with a Lua compare-and-swap. Keeping one key makes
//! create/rename/unlink/truncate atomic without partially updating several
//! Redis structures. It is intentionally a correctness-first prototype: each
//! operation transfers and (for mutations) serializes the complete snapshot.

use async_trait::async_trait;
use redis::aio::MultiplexedConnection;

use crate::fs_model::{Inode, Slice};
use crate::meta::{MemStore, MetaError, MetaStore, Result};
use crate::meta_persist::MetaSnapshot;

const SNAPSHOT_KEY_SUFFIX: &str = "meta:v1";
const MAX_CAS_RETRIES: usize = 64;

const CAS_SCRIPT: &str = r#"
local current = redis.call('GET', KEYS[1])
if current == ARGV[1] then
    redis.call('SET', KEYS[1], ARGV[2])
    return 1
end
return 0
"#;

/// Errors that prevent constructing a Redis metadata backend.
#[derive(Debug, thiserror::Error)]
pub enum RedisMetaStoreError {
    #[error("Redis metadata URL must start with redis://")]
    InvalidUrl,
    #[error("Redis prefix must be non-empty and contain no whitespace/control characters")]
    InvalidPrefix,
    #[error("Redis error: {0}")]
    Redis(#[from] redis::RedisError),
    #[error("Redis metadata snapshot is invalid: {0}")]
    Snapshot(#[from] serde_json::Error),
}

/// A linearizable, single-key Redis implementation of [`MetaStore`].
pub struct RedisMetaStore {
    connection: MultiplexedConnection,
    snapshot_key: String,
}

#[derive(Clone)]
enum Mutation {
    Create {
        parent: u64,
        name: String,
        mode: u32,
    },
    Symlink {
        parent: u64,
        name: String,
        target: String,
    },
    AppendSlice {
        inode: u64,
        slice: Slice,
    },
    Truncate {
        inode: u64,
        new_size: u64,
    },
    Mkdir {
        parent: u64,
        name: String,
        mode: u32,
    },
    Unlink {
        parent: u64,
        name: String,
    },
    Rename {
        old_parent: u64,
        old_name: String,
        new_parent: u64,
        new_name: String,
    },
    AcknowledgeGarbage {
        keys: Vec<String>,
    },
}

enum MutationOutput {
    Inode(u64),
    Unit,
    Garbage(Vec<String>),
}

impl RedisMetaStore {
    /// Connects to Redis and initializes `<prefix>:meta:v1` if absent.
    ///
    /// Initialization uses `SET NX`, so concurrent first starters cannot
    /// overwrite one another. Existing malformed data is reported rather than
    /// silently replaced with a fresh filesystem.
    pub async fn new(url: &str, prefix: &str) -> std::result::Result<Self, RedisMetaStoreError> {
        if !url.starts_with("redis://") {
            return Err(RedisMetaStoreError::InvalidUrl);
        }
        if prefix.is_empty()
            || prefix
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err(RedisMetaStoreError::InvalidPrefix);
        }

        let client = redis::Client::open(url)?;
        let mut connection = client.get_multiplexed_async_connection().await?;
        let snapshot_key = format!("{prefix}:{SNAPSHOT_KEY_SUFFIX}");
        let initial = serde_json::to_vec(&MemStore::new().snapshot().await)?;

        let _: Option<String> = redis::cmd("SET")
            .arg(&snapshot_key)
            .arg(&initial)
            .arg("NX")
            .query_async(&mut connection)
            .await?;

        let existing: Vec<u8> = redis::cmd("GET")
            .arg(&snapshot_key)
            .query_async(&mut connection)
            .await?;
        let _: MetaSnapshot = serde_json::from_slice(&existing)?;

        Ok(Self {
            connection,
            snapshot_key,
        })
    }

    fn report_backend_error(context: &str, error: impl std::fmt::Display) -> MetaError {
        eprintln!("[meta_redis] {context}: {error}");
        MetaError::Io
    }

    async fn load_raw(&self) -> Result<Vec<u8>> {
        let mut connection = self.connection.clone();
        let raw: Option<Vec<u8>> = redis::cmd("GET")
            .arg(&self.snapshot_key)
            .query_async(&mut connection)
            .await
            .map_err(|error| Self::report_backend_error("GET failed", error))?;
        raw.ok_or_else(|| Self::report_backend_error("GET failed", "metadata key disappeared"))
    }

    async fn load_mem(&self) -> Result<MemStore> {
        let raw = self.load_raw().await?;
        let snapshot = serde_json::from_slice(&raw)
            .map_err(|error| Self::report_backend_error("snapshot decode failed", error))?;
        Ok(MemStore::from_snapshot(snapshot))
    }

    async fn compare_and_swap(&self, expected: &[u8], replacement: &[u8]) -> Result<bool> {
        let mut connection = self.connection.clone();
        let swapped: i32 = redis::cmd("EVAL")
            .arg(CAS_SCRIPT)
            .arg(1)
            .arg(&self.snapshot_key)
            .arg(expected)
            .arg(replacement)
            .query_async(&mut connection)
            .await
            .map_err(|error| Self::report_backend_error("Lua CAS failed", error))?;
        Ok(swapped == 1)
    }

    async fn apply(mem: &MemStore, mutation: &Mutation) -> Result<MutationOutput> {
        match mutation {
            Mutation::Create { parent, name, mode } => mem
                .create(*parent, name, *mode)
                .await
                .map(MutationOutput::Inode),
            Mutation::Symlink {
                parent,
                name,
                target,
            } => mem
                .symlink(*parent, name, target)
                .await
                .map(MutationOutput::Inode),
            Mutation::AppendSlice { inode, slice } => mem
                .append_slice(*inode, slice.clone())
                .await
                .map(|()| MutationOutput::Unit),
            Mutation::Truncate { inode, new_size } => mem
                .truncate(*inode, *new_size)
                .await
                .map(MutationOutput::Garbage),
            Mutation::Mkdir { parent, name, mode } => mem
                .mkdir(*parent, name, *mode)
                .await
                .map(MutationOutput::Inode),
            Mutation::Unlink { parent, name } => {
                mem.unlink(*parent, name).await.map(MutationOutput::Garbage)
            }
            Mutation::Rename {
                old_parent,
                old_name,
                new_parent,
                new_name,
            } => mem
                .rename(*old_parent, old_name, *new_parent, new_name)
                .await
                .map(MutationOutput::Garbage),
            Mutation::AcknowledgeGarbage { keys } => mem
                .acknowledge_garbage(keys)
                .await
                .map(|()| MutationOutput::Unit),
        }
    }

    async fn mutate(&self, mutation: Mutation) -> Result<MutationOutput> {
        for _ in 0..MAX_CAS_RETRIES {
            let old_raw = self.load_raw().await?;
            let snapshot: MetaSnapshot = serde_json::from_slice(&old_raw)
                .map_err(|error| Self::report_backend_error("snapshot decode failed", error))?;
            let mem = MemStore::from_snapshot(snapshot);

            match Self::apply(&mem, &mutation).await {
                Ok(output) => {
                    let new_raw = serde_json::to_vec(&mem.snapshot().await).map_err(|error| {
                        Self::report_backend_error("snapshot encode failed", error)
                    })?;
                    if self.compare_and_swap(&old_raw, &new_raw).await? {
                        return Ok(output);
                    }
                }
                Err(error) => {
                    // Linearize semantic failures too: return the error only if
                    // the snapshot on which it was evaluated is still current.
                    if self.compare_and_swap(&old_raw, &old_raw).await? {
                        return Err(error);
                    }
                }
            }
            tokio::task::yield_now().await;
        }

        Err(Self::report_backend_error(
            "mutation aborted",
            format_args!("exceeded {MAX_CAS_RETRIES} CAS retries"),
        ))
    }
}

#[async_trait]
impl MetaStore for RedisMetaStore {
    async fn lookup(&self, parent: u64, name: &str) -> Result<u64> {
        self.load_mem().await?.lookup(parent, name).await
    }

    async fn getattr(&self, inode: u64) -> Result<Inode> {
        self.load_mem().await?.getattr(inode).await
    }

    async fn read_slices(&self, inode: u64, chunk_idx: u32) -> Result<Vec<Slice>> {
        self.load_mem().await?.read_slices(inode, chunk_idx).await
    }

    async fn create(&self, parent: u64, name: &str, mode: u32) -> Result<u64> {
        match self
            .mutate(Mutation::Create {
                parent,
                name: name.to_owned(),
                mode,
            })
            .await?
        {
            MutationOutput::Inode(inode) => Ok(inode),
            _ => unreachable!("create mutation returned wrong output"),
        }
    }

    async fn symlink(&self, parent: u64, name: &str, target: &str) -> Result<u64> {
        match self
            .mutate(Mutation::Symlink {
                parent,
                name: name.to_owned(),
                target: target.to_owned(),
            })
            .await?
        {
            MutationOutput::Inode(inode) => Ok(inode),
            _ => unreachable!("symlink mutation returned wrong output"),
        }
    }

    async fn readlink(&self, inode: u64) -> Result<String> {
        self.load_mem().await?.readlink(inode).await
    }

    async fn append_slice(&self, inode: u64, slice: Slice) -> Result<()> {
        match self.mutate(Mutation::AppendSlice { inode, slice }).await? {
            MutationOutput::Unit => Ok(()),
            _ => unreachable!("append_slice mutation returned wrong output"),
        }
    }

    async fn truncate(&self, inode: u64, new_size: u64) -> Result<Vec<String>> {
        match self.mutate(Mutation::Truncate { inode, new_size }).await? {
            MutationOutput::Garbage(keys) => Ok(keys),
            _ => unreachable!("truncate mutation returned wrong output"),
        }
    }

    async fn readdir(&self, inode: u64) -> Result<Vec<(u64, String)>> {
        self.load_mem().await?.readdir(inode).await
    }

    async fn mkdir(&self, parent: u64, name: &str, mode: u32) -> Result<u64> {
        match self
            .mutate(Mutation::Mkdir {
                parent,
                name: name.to_owned(),
                mode,
            })
            .await?
        {
            MutationOutput::Inode(inode) => Ok(inode),
            _ => unreachable!("mkdir mutation returned wrong output"),
        }
    }

    async fn unlink(&self, parent: u64, name: &str) -> Result<Vec<String>> {
        match self
            .mutate(Mutation::Unlink {
                parent,
                name: name.to_owned(),
            })
            .await?
        {
            MutationOutput::Garbage(keys) => Ok(keys),
            _ => unreachable!("unlink mutation returned wrong output"),
        }
    }

    async fn rename(
        &self,
        old_parent: u64,
        old_name: &str,
        new_parent: u64,
        new_name: &str,
    ) -> Result<Vec<String>> {
        match self
            .mutate(Mutation::Rename {
                old_parent,
                old_name: old_name.to_owned(),
                new_parent,
                new_name: new_name.to_owned(),
            })
            .await?
        {
            MutationOutput::Garbage(keys) => Ok(keys),
            _ => unreachable!("rename mutation returned wrong output"),
        }
    }

    async fn pending_garbage(&self) -> Result<Vec<String>> {
        self.load_mem().await?.pending_garbage().await
    }

    async fn acknowledge_garbage(&self, keys: &[String]) -> Result<()> {
        match self
            .mutate(Mutation::AcknowledgeGarbage {
                keys: keys.to_vec(),
            })
            .await?
        {
            MutationOutput::Unit => Ok(()),
            _ => unreachable!("acknowledge_garbage mutation returned wrong output"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_model::ROOT_INODE;
    use uuid::Uuid;

    #[test]
    fn rejects_invalid_connection_settings_without_network_access() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        assert!(matches!(
            runtime.block_on(RedisMetaStore::new("http://localhost", "kestrelfs")),
            Err(RedisMetaStoreError::InvalidUrl)
        ));
        assert!(matches!(
            runtime.block_on(RedisMetaStore::new("redis://localhost", "")),
            Err(RedisMetaStoreError::InvalidPrefix)
        ));
    }

    /// Runs only when a developer/CI job supplies a real Redis endpoint.
    /// A random namespace makes the test safe to run concurrently.
    #[tokio::test]
    async fn redis_url_gated_full_semantics_and_restart() {
        let Ok(url) = std::env::var("REDIS_URL") else {
            eprintln!("REDIS_URL not set; skipping Redis integration assertions");
            return;
        };
        let prefix = format!("kestrelfs:test:{}", Uuid::new_v4());
        let store = RedisMetaStore::new(&url, &prefix).await.unwrap();

        let directory = store.mkdir(ROOT_INODE, "redis-dir", 0o755).await.unwrap();
        let peer = RedisMetaStore::new(&url, &prefix).await.unwrap();
        let (left, right) = tokio::join!(
            store.create(directory, "concurrent-left", 0o644),
            peer.create(directory, "concurrent-right", 0o644)
        );
        assert_ne!(left.unwrap(), right.unwrap());
        store.lookup(directory, "concurrent-left").await.unwrap();
        store.lookup(directory, "concurrent-right").await.unwrap();

        let file = store.create(directory, "data", 0o644).await.unwrap();
        let link = store
            .symlink(directory, "link", "../target-with-redis")
            .await
            .unwrap();
        assert_eq!(store.readlink(link).await.unwrap(), "../target-with-redis");
        store
            .rename(directory, "link", directory, "renamed-link")
            .await
            .unwrap();

        let older = Slice {
            chunk_index: 0,
            slice_id: Uuid::new_v4(),
            chunk_offset: 0,
            length: 100,
            written_at: 1,
        };
        let newer = Slice {
            chunk_index: 0,
            slice_id: Uuid::new_v4(),
            chunk_offset: 80,
            length: 20,
            written_at: 2,
        };
        let older_key = older.block_key(0);
        let newer_key = newer.block_key(0);
        store.append_slice(file, older).await.unwrap();
        store.append_slice(file, newer).await.unwrap();
        let truncate_gc = store.truncate(file, 80).await.unwrap();
        assert!(!truncate_gc.contains(&older_key));
        assert_eq!(truncate_gc, vec![newer_key.clone()]);

        let victim = store.create(directory, "victim", 0o644).await.unwrap();
        let victim_slice = Slice {
            chunk_index: 0,
            slice_id: Uuid::new_v4(),
            chunk_offset: 0,
            length: 10,
            written_at: 3,
        };
        let victim_key = victim_slice.block_key(0);
        store.append_slice(victim, victim_slice).await.unwrap();
        let source = store.create(directory, "source", 0o644).await.unwrap();
        assert_eq!(
            store
                .rename(directory, "source", directory, "victim")
                .await
                .unwrap(),
            vec![victim_key.clone()]
        );
        assert_eq!(
            store.unlink(directory, "data").await.unwrap(),
            vec![older_key.clone()]
        );

        let pending = store.pending_garbage().await.unwrap();
        assert!(pending.contains(&newer_key));
        assert!(pending.contains(&victim_key));
        assert!(pending.contains(&older_key));

        drop(store);
        let restarted = RedisMetaStore::new(&url, &prefix).await.unwrap();
        let restored_link = restarted.lookup(directory, "renamed-link").await.unwrap();
        assert_eq!(
            restarted.readlink(restored_link).await.unwrap(),
            "../target-with-redis"
        );
        assert_eq!(restarted.lookup(directory, "victim").await.unwrap(), source);
        assert_eq!(restarted.pending_garbage().await.unwrap(), pending);
        restarted.acknowledge_garbage(&pending).await.unwrap();
        assert!(restarted.pending_garbage().await.unwrap().is_empty());

        let mut connection = restarted.connection.clone();
        let deleted: i32 = redis::cmd("DEL")
            .arg(&restarted.snapshot_key)
            .query_async(&mut connection)
            .await
            .unwrap();
        assert_eq!(deleted, 1);
    }
}
