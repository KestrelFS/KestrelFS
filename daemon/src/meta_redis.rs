// SPDX-License-Identifier: Apache-2.0
//! Redis-backed metadata store.
//!
//! Schema v2 stores metadata as independent records in Redis hashes instead of
//! serializing the whole filesystem into one value. Point reads fetch only the
//! inode, dirent, slice list, or symlink target they need. Mutations still
//! reuse the well-tested [`MemStore`] semantics, then publish only the changed
//! records with one revision-checked Lua transaction. This keeps inode
//! allocation, rename replacement, truncate, and the durable GC queue atomic.

use std::collections::{BTreeMap, HashMap, HashSet};

use async_trait::async_trait;
use redis::aio::MultiplexedConnection;
use serde::{Deserialize, Serialize};

use crate::fs_model::{Inode, Slice, S_IFDIR};
use crate::meta::{CoherenceProbe, MemStore, MetaError, MetaStore, Result};
use crate::meta_persist::MetaSnapshot;

const LEGACY_SNAPSHOT_KEY_SUFFIX: &str = "meta:v1";
const SCHEMA_VERSION: &str = "2";
const MAX_CAS_RETRIES: usize = 64;
const COHERENCE_DIRTY_INODE_MAX: usize = 64;
const COHERENCE_REVISION_WINDOW: u64 = 256;

const INIT_SCRIPT: &str = r#"
local function apply_hash(key, sets, dels)
    for _, pair in ipairs(sets) do
        redis.call('HSET', key, pair[1], pair[2])
    end
    for _, field in ipairs(dels) do
        redis.call('HDEL', key, field)
    end
end

if redis.call('EXISTS', KEYS[8]) ~= 0 then
    return -1
end

local schema = redis.call('HGET', KEYS[1], 'schema_version')
if schema then
    if schema ~= ARGV[1] then
        return -2
    end
    return 0
end

for index = 1, 7 do
    if redis.call('EXISTS', KEYS[index]) ~= 0 then
        return -3
    end
end

local patch = cjson.decode(ARGV[3])
apply_hash(KEYS[2], patch.inodes_set, patch.inodes_del)
apply_hash(KEYS[3], patch.dirents_set, patch.dirents_del)
apply_hash(KEYS[4], patch.slices_set, patch.slices_del)
apply_hash(KEYS[5], patch.symlinks_set, patch.symlinks_del)
for _, value in ipairs(patch.gc_add) do
    redis.call('SADD', KEYS[6], value)
end
for _, value in ipairs(patch.gc_del) do
    redis.call('SREM', KEYS[6], value)
end
redis.call('HSET', KEYS[1],
    'schema_version', ARGV[1],
    'revision', '0',
    'next_inode_id', ARGV[2])
return 1
"#;

const MUTATE_SCRIPT: &str = r#"
local function apply_hash(key, sets, dels)
    for _, pair in ipairs(sets) do
        redis.call('HSET', key, pair[1], pair[2])
    end
    for _, field in ipairs(dels) do
        redis.call('HDEL', key, field)
    end
end

local schema = redis.call('HGET', KEYS[1], 'schema_version')
if not schema or schema ~= ARGV[1] then
    return -1
end
local revision = redis.call('HGET', KEYS[1], 'revision')
if not revision then
    return -2
end
if revision ~= ARGV[2] then
    return 0
end

local patch = cjson.decode(ARGV[5])
apply_hash(KEYS[2], patch.inodes_set, patch.inodes_del)
apply_hash(KEYS[3], patch.dirents_set, patch.dirents_del)
apply_hash(KEYS[4], patch.slices_set, patch.slices_del)
apply_hash(KEYS[5], patch.symlinks_set, patch.symlinks_del)
for _, value in ipairs(patch.gc_add) do
    redis.call('SADD', KEYS[6], value)
end
for _, value in ipairs(patch.gc_del) do
    redis.call('SREM', KEYS[6], value)
end
redis.call('HSET', KEYS[1],
    'revision', ARGV[3],
    'next_inode_id', ARGV[4])
redis.call('HSET', KEYS[7], ARGV[3], ARGV[6])
local expired = tonumber(ARGV[3]) - tonumber(ARGV[7])
if expired >= 1 then
    redis.call('HDEL', KEYS[7], tostring(expired))
end
return 1
"#;

const CHECK_REVISION_SCRIPT: &str = r#"
local schema = redis.call('HGET', KEYS[1], 'schema_version')
if not schema or schema ~= ARGV[1] then
    return -1
end
local revision = redis.call('HGET', KEYS[1], 'revision')
if not revision then
    return -2
end
if revision == ARGV[2] then
    return 1
end
return 0
"#;

const COHERENCE_PROBE_SCRIPT: &str = r#"
local schema = redis.call('HGET', KEYS[1], 'schema_version')
if not schema or schema ~= ARGV[1] then
    return {'error', '-1'}
end
local current_text = redis.call('HGET', KEYS[1], 'revision')
local current = tonumber(current_text)
if not current then
    return {'error', '-2'}
end
if ARGV[2] == '' then
    return {'full', current_text}
end
local observed = tonumber(ARGV[2])
local window = tonumber(ARGV[3])
if not observed or observed > current or current - observed > window then
    return {'full', current_text}
end
if observed == current then
    return {'unchanged', current_text}
end
local result = {'records', current_text}
for revision = observed + 1, current do
    local record = redis.call('HGET', KEYS[2], tostring(revision))
    if not record then
        return {'full', current_text}
    end
    table.insert(result, record)
end
return result
"#;

/// Errors that prevent constructing a Redis metadata backend.
#[derive(Debug, thiserror::Error)]
pub enum RedisMetaStoreError {
    #[error("Redis metadata URL must start with redis://")]
    InvalidUrl,
    #[error("Redis prefix must be non-empty and contain no whitespace/control characters")]
    InvalidPrefix,
    #[error("legacy Redis metadata schema v1 exists; automatic migration is not supported")]
    LegacySchema,
    #[error("unsupported Redis metadata schema: {0}")]
    UnsupportedSchema(String),
    #[error("invalid Redis metadata layout: {0}")]
    InvalidLayout(String),
    #[error("Redis error: {0}")]
    Redis(#[from] redis::RedisError),
    #[error("Redis metadata record is invalid: {0}")]
    Snapshot(#[from] serde_json::Error),
}

#[derive(Clone)]
struct RedisKeys {
    control: String,
    inodes: String,
    dirents: String,
    slices: String,
    symlinks: String,
    garbage: String,
    dirty: String,
    legacy_snapshot: String,
}

impl RedisKeys {
    fn new(prefix: &str) -> Self {
        let base = format!("{prefix}:meta:v2");
        Self {
            control: format!("{base}:control"),
            inodes: format!("{base}:inodes"),
            dirents: format!("{base}:dirents"),
            slices: format!("{base}:slices"),
            symlinks: format!("{base}:symlinks"),
            garbage: format!("{base}:gc"),
            dirty: format!("{base}:dirty"),
            legacy_snapshot: format!("{prefix}:{LEGACY_SNAPSHOT_KEY_SUFFIX}"),
        }
    }

    fn data_keys(&self) -> [&str; 7] {
        [
            &self.control,
            &self.inodes,
            &self.dirents,
            &self.slices,
            &self.symlinks,
            &self.garbage,
            &self.dirty,
        ]
    }
}

/// A record-oriented Redis implementation of [`MetaStore`].
pub struct RedisMetaStore {
    connection: MultiplexedConnection,
    keys: RedisKeys,
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
    SetAttrs {
        inode: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        atime: Option<u64>,
        mtime: Option<u64>,
    },
    Mkdir {
        parent: u64,
        name: String,
        mode: u32,
    },
    Link {
        parent: u64,
        name: String,
        inode: u64,
    },
    Unlink {
        parent: u64,
        name: String,
        defer_reclaim: bool,
    },
    FinalizeOrphan {
        inode: u64,
    },
    Rename {
        old_parent: u64,
        old_name: String,
        new_parent: u64,
        new_name: String,
        flags: u32,
        defer_reclaim: bool,
    },
    AcknowledgeGarbage {
        keys: Vec<String>,
    },
}

enum MutationOutput {
    Inode(u64),
    Nlink(u32),
    Attributes(Inode),
    Unit,
    Garbage(Vec<String>),
}

struct LoadedSnapshot {
    revision: u64,
    snapshot: MetaSnapshot,
}

type SnapshotWire = (
    HashMap<String, String>,
    HashMap<String, Vec<u8>>,
    HashMap<String, String>,
    HashMap<String, Vec<u8>>,
    HashMap<String, String>,
    Vec<String>,
);

#[derive(Default, Serialize)]
struct RedisPatch {
    inodes_set: Vec<(String, String)>,
    inodes_del: Vec<String>,
    dirents_set: Vec<(String, String)>,
    dirents_del: Vec<String>,
    slices_set: Vec<(String, String)>,
    slices_del: Vec<String>,
    symlinks_set: Vec<(String, String)>,
    symlinks_del: Vec<String>,
    gc_add: Vec<String>,
    gc_del: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct DirtyRecord {
    overflow: bool,
    inode_ids: Vec<u64>,
}

impl RedisPatch {
    fn initial(snapshot: &MetaSnapshot) -> std::result::Result<Self, serde_json::Error> {
        Ok(Self::between(
            &SnapshotRecords::default(),
            &SnapshotRecords::from_snapshot(snapshot)?,
        ))
    }

    fn between(old: &SnapshotRecords, new: &SnapshotRecords) -> Self {
        let (inodes_set, inodes_del) = diff_map(&old.inodes, &new.inodes);
        let (dirents_set, dirents_del) = diff_map(&old.dirents, &new.dirents);
        let (slices_set, slices_del) = diff_map(&old.slices, &new.slices);
        let (symlinks_set, symlinks_del) = diff_map(&old.symlinks, &new.symlinks);
        let gc_add = new.garbage.difference(&old.garbage).cloned().collect();
        let gc_del = old.garbage.difference(&new.garbage).cloned().collect();
        Self {
            inodes_set,
            inodes_del,
            dirents_set,
            dirents_del,
            slices_set,
            slices_del,
            symlinks_set,
            symlinks_del,
            gc_add,
            gc_del,
        }
    }

    fn is_empty(&self) -> bool {
        self.inodes_set.is_empty()
            && self.inodes_del.is_empty()
            && self.dirents_set.is_empty()
            && self.dirents_del.is_empty()
            && self.slices_set.is_empty()
            && self.slices_del.is_empty()
            && self.symlinks_set.is_empty()
            && self.symlinks_del.is_empty()
            && self.gc_add.is_empty()
            && self.gc_del.is_empty()
    }

    fn dirty_record(&self) -> DirtyRecord {
        let mut inode_ids = std::collections::BTreeSet::new();
        for field in self
            .inodes_set
            .iter()
            .map(|(field, _)| field)
            .chain(self.inodes_del.iter())
        {
            match field.parse::<u64>() {
                Ok(inode_id) => {
                    inode_ids.insert(inode_id);
                }
                Err(_) => {
                    return DirtyRecord {
                        overflow: true,
                        inode_ids: Vec::new(),
                    };
                }
            }
        }
        for field in self
            .slices_set
            .iter()
            .map(|(field, _)| field)
            .chain(self.slices_del.iter())
        {
            match parse_slice_field(field) {
                Ok((inode_id, _)) => {
                    inode_ids.insert(inode_id);
                }
                Err(_) => {
                    return DirtyRecord {
                        overflow: true,
                        inode_ids: Vec::new(),
                    };
                }
            }
        }
        let overflow = inode_ids.len() > COHERENCE_DIRTY_INODE_MAX;
        DirtyRecord {
            overflow,
            inode_ids: if overflow {
                Vec::new()
            } else {
                inode_ids.into_iter().collect()
            },
        }
    }
}

#[derive(Default)]
struct SnapshotRecords {
    inodes: BTreeMap<String, String>,
    dirents: BTreeMap<String, String>,
    slices: BTreeMap<String, String>,
    symlinks: BTreeMap<String, String>,
    garbage: HashSet<String>,
}

impl SnapshotRecords {
    fn from_snapshot(snapshot: &MetaSnapshot) -> std::result::Result<Self, serde_json::Error> {
        let mut records = Self::default();
        for (inode_id, inode) in &snapshot.inodes {
            records
                .inodes
                .insert(inode_id.to_string(), serde_json::to_string(inode)?);
        }
        for (parent, entries) in &snapshot.dir_entries {
            for (name, inode_id) in entries {
                records
                    .dirents
                    .insert(dirent_field(*parent, name), inode_id.to_string());
            }
        }
        for (inode_id, chunks) in &snapshot.slices {
            for (chunk_index, slices) in chunks {
                records.slices.insert(
                    slice_field(*inode_id, *chunk_index),
                    serde_json::to_string(slices)?,
                );
            }
        }
        for (inode_id, target) in &snapshot.symlink_targets {
            records
                .symlinks
                .insert(inode_id.to_string(), target.clone());
        }
        records.garbage.clone_from(&snapshot.pending_garbage);
        Ok(records)
    }
}

fn diff_map(
    old: &BTreeMap<String, String>,
    new: &BTreeMap<String, String>,
) -> (Vec<(String, String)>, Vec<String>) {
    let sets = new
        .iter()
        .filter(|(field, value)| old.get(field.as_str()) != Some(*value))
        .map(|(field, value)| (field.clone(), value.clone()))
        .collect();
    let dels = old
        .keys()
        .filter(|field| !new.contains_key(field.as_str()))
        .cloned()
        .collect();
    (sets, dels)
}

fn encode_name(name: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(name.len() * 2);
    for byte in name.as_bytes() {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_name(encoded: &str) -> std::result::Result<String, RedisMetaStoreError> {
    if !encoded.len().is_multiple_of(2) {
        return Err(RedisMetaStoreError::InvalidLayout(
            "dirent name has odd-length hex encoding".to_owned(),
        ));
    }
    let mut bytes = Vec::with_capacity(encoded.len() / 2);
    for pair in encoded.as_bytes().as_chunks::<2>().0 {
        let pair = std::str::from_utf8(pair).map_err(|error| {
            RedisMetaStoreError::InvalidLayout(format!("dirent hex is not UTF-8: {error}"))
        })?;
        bytes.push(u8::from_str_radix(pair, 16).map_err(|error| {
            RedisMetaStoreError::InvalidLayout(format!("dirent hex is invalid: {error}"))
        })?);
    }
    String::from_utf8(bytes).map_err(|error| {
        RedisMetaStoreError::InvalidLayout(format!("dirent name is not UTF-8: {error}"))
    })
}

fn dirent_field(parent: u64, name: &str) -> String {
    format!("{parent}:{}", encode_name(name))
}

fn parse_dirent_field(field: &str) -> std::result::Result<(u64, String), RedisMetaStoreError> {
    let (parent, name) = field.split_once(':').ok_or_else(|| {
        RedisMetaStoreError::InvalidLayout(format!("invalid dirent field {field:?}"))
    })?;
    let parent = parent.parse().map_err(|error| {
        RedisMetaStoreError::InvalidLayout(format!("invalid dirent parent {parent:?}: {error}"))
    })?;
    Ok((parent, decode_name(name)?))
}

fn slice_field(inode: u64, chunk_index: u32) -> String {
    format!("{inode}:{chunk_index}")
}

fn parse_slice_field(field: &str) -> std::result::Result<(u64, u32), RedisMetaStoreError> {
    let (inode, chunk) = field.split_once(':').ok_or_else(|| {
        RedisMetaStoreError::InvalidLayout(format!("invalid slice field {field:?}"))
    })?;
    let inode = inode.parse().map_err(|error| {
        RedisMetaStoreError::InvalidLayout(format!("invalid slice inode {inode:?}: {error}"))
    })?;
    let chunk = chunk.parse().map_err(|error| {
        RedisMetaStoreError::InvalidLayout(format!("invalid slice chunk {chunk:?}: {error}"))
    })?;
    Ok((inode, chunk))
}

impl RedisMetaStore {
    /// Connects to Redis and initializes the v2 record schema if absent.
    ///
    /// A legacy v1 snapshot or an unversioned partial v2 layout is rejected;
    /// this implementation never silently migrates or wipes metadata.
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
        let connection = client.get_multiplexed_async_connection().await?;
        let store = Self {
            connection,
            keys: RedisKeys::new(prefix),
        };
        store.initialize().await?;
        store.load_snapshot_backend().await?;
        Ok(store)
    }

    fn report_backend_error(context: &str, error: impl std::fmt::Display) -> MetaError {
        eprintln!("[meta_redis] {context}: {error}");
        MetaError::Io
    }

    #[cfg(test)]
    async fn coherence_revision(&self) -> Result<Option<u64>> {
        Ok(self.coherence_probe(None).await?.revision())
    }

    fn decode_coherence_response(response: &[String]) -> Result<CoherenceProbe> {
        if response.len() < 2 {
            return Err(Self::report_backend_error(
                "coherence probe failed",
                "short Redis response",
            ));
        }
        let revision = response[1].parse::<u64>().map_err(|error| {
            Self::report_backend_error("coherence probe revision invalid", error)
        })?;
        match response[0].as_str() {
            "unchanged" if response.len() == 2 => Ok(CoherenceProbe::Unchanged { revision }),
            "full" if response.len() == 2 => Ok(CoherenceProbe::Full { revision }),
            "records" => {
                let mut inode_ids = std::collections::BTreeSet::new();
                for encoded in &response[2..] {
                    let record: DirtyRecord = serde_json::from_str(encoded).map_err(|error| {
                        Self::report_backend_error("coherence dirty record invalid", error)
                    })?;
                    if record.overflow {
                        return Ok(CoherenceProbe::Full { revision });
                    }
                    inode_ids.extend(record.inode_ids);
                    if inode_ids.len() > COHERENCE_DIRTY_INODE_MAX {
                        return Ok(CoherenceProbe::Full { revision });
                    }
                }
                Ok(CoherenceProbe::Inodes {
                    revision,
                    inode_ids: inode_ids.into_iter().collect(),
                })
            }
            "error" => Err(Self::report_backend_error(
                "coherence probe failed",
                format_args!("invalid schema/status {}", response[1]),
            )),
            status => Err(Self::report_backend_error(
                "coherence probe failed",
                format_args!("unexpected response status {status:?}"),
            )),
        }
    }

    fn parse_control(
        control: &HashMap<String, String>,
    ) -> std::result::Result<(u64, u64), RedisMetaStoreError> {
        let schema = control.get("schema_version").ok_or_else(|| {
            RedisMetaStoreError::InvalidLayout("missing schema_version".to_owned())
        })?;
        if schema != SCHEMA_VERSION {
            return Err(RedisMetaStoreError::UnsupportedSchema(schema.clone()));
        }
        let revision = control
            .get("revision")
            .ok_or_else(|| RedisMetaStoreError::InvalidLayout("missing revision".to_owned()))?
            .parse()
            .map_err(|error| {
                RedisMetaStoreError::InvalidLayout(format!("invalid revision: {error}"))
            })?;
        let next_inode_id = control
            .get("next_inode_id")
            .ok_or_else(|| RedisMetaStoreError::InvalidLayout("missing next_inode_id".to_owned()))?
            .parse()
            .map_err(|error| {
                RedisMetaStoreError::InvalidLayout(format!("invalid next_inode_id: {error}"))
            })?;
        Ok((revision, next_inode_id))
    }

    async fn initialize(&self) -> std::result::Result<(), RedisMetaStoreError> {
        let initial = MemStore::new().snapshot().await;
        let patch = serde_json::to_string(&RedisPatch::initial(&initial)?)?;
        let mut connection = self.connection.clone();
        let keys = self.keys.data_keys();
        let result: i32 = redis::cmd("EVAL")
            .arg(INIT_SCRIPT)
            .arg(8)
            .arg(&keys)
            .arg(&self.keys.legacy_snapshot)
            .arg(SCHEMA_VERSION)
            .arg(initial.next_inode_id)
            .arg(patch)
            .query_async(&mut connection)
            .await?;
        match result {
            0 | 1 => Ok(()),
            -1 => Err(RedisMetaStoreError::LegacySchema),
            -2 => Err(RedisMetaStoreError::UnsupportedSchema(
                "existing control record".to_owned(),
            )),
            -3 => Err(RedisMetaStoreError::InvalidLayout(
                "partial v2 keys exist without a control record".to_owned(),
            )),
            value => Err(RedisMetaStoreError::InvalidLayout(format!(
                "initializer returned unexpected status {value}"
            ))),
        }
    }

    async fn load_snapshot_backend(
        &self,
    ) -> std::result::Result<LoadedSnapshot, RedisMetaStoreError> {
        let mut connection = self.connection.clone();
        let (control, inode_records, dirent_records, slice_records, symlink_records, garbage):
            SnapshotWire = redis::pipe()
            .atomic()
            .cmd("HGETALL")
            .arg(&self.keys.control)
            .cmd("HGETALL")
            .arg(&self.keys.inodes)
            .cmd("HGETALL")
            .arg(&self.keys.dirents)
            .cmd("HGETALL")
            .arg(&self.keys.slices)
            .cmd("HGETALL")
            .arg(&self.keys.symlinks)
            .cmd("SMEMBERS")
            .arg(&self.keys.garbage)
            .query_async(&mut connection)
            .await?;
        let (revision, next_inode_id) = Self::parse_control(&control)?;

        let mut inodes = HashMap::new();
        for (field, encoded) in inode_records {
            let inode_id: u64 = field.parse().map_err(|error| {
                RedisMetaStoreError::InvalidLayout(format!(
                    "invalid inode field {field:?}: {error}"
                ))
            })?;
            let inode: Inode = serde_json::from_slice(&encoded)?;
            if inode.inode_id != inode_id {
                return Err(RedisMetaStoreError::InvalidLayout(format!(
                    "inode field {inode_id} contains inode {}",
                    inode.inode_id
                )));
            }
            inodes.insert(inode_id, inode);
        }

        let mut dir_entries = HashMap::new();
        for (inode_id, inode) in &inodes {
            if inode.mode & 0o170000 == S_IFDIR {
                dir_entries.insert(*inode_id, HashMap::new());
            }
        }
        for (field, child) in dirent_records {
            let (parent, name) = parse_dirent_field(&field)?;
            let child = child.parse().map_err(|error| {
                RedisMetaStoreError::InvalidLayout(format!(
                    "invalid dirent child {child:?}: {error}"
                ))
            })?;
            dir_entries.entry(parent).or_default().insert(name, child);
        }

        let mut slices: HashMap<u64, HashMap<u32, Vec<Slice>>> = HashMap::new();
        for (field, encoded) in slice_records {
            let (inode, chunk) = parse_slice_field(&field)?;
            let records = serde_json::from_slice(&encoded)?;
            slices.entry(inode).or_default().insert(chunk, records);
        }

        let mut symlink_targets = HashMap::new();
        for (field, target) in symlink_records {
            let inode = field.parse().map_err(|error| {
                RedisMetaStoreError::InvalidLayout(format!(
                    "invalid symlink inode {field:?}: {error}"
                ))
            })?;
            symlink_targets.insert(inode, target);
        }

        Ok(LoadedSnapshot {
            revision,
            snapshot: MetaSnapshot {
                inodes,
                dir_entries,
                slices,
                symlink_targets,
                pending_garbage: garbage.into_iter().collect(),
                next_inode_id,
            },
        })
    }

    async fn load_snapshot(&self) -> Result<LoadedSnapshot> {
        self.load_snapshot_backend()
            .await
            .map_err(|error| Self::report_backend_error("snapshot load failed", error))
    }

    async fn load_inode(&self, inode: u64) -> Result<Inode> {
        let mut connection = self.connection.clone();
        let (control, encoded): (HashMap<String, String>, Option<Vec<u8>>) = redis::pipe()
            .atomic()
            .cmd("HGETALL")
            .arg(&self.keys.control)
            .cmd("HGET")
            .arg(&self.keys.inodes)
            .arg(inode)
            .query_async(&mut connection)
            .await
            .map_err(|error| Self::report_backend_error("inode read failed", error))?;
        Self::parse_control(&control)
            .map_err(|error| Self::report_backend_error("control record invalid", error))?;
        let encoded = encoded.ok_or(MetaError::NotFound)?;
        serde_json::from_slice(&encoded)
            .map_err(|error| Self::report_backend_error("inode decode failed", error))
    }

    async fn check_revision(&self, expected: u64) -> Result<bool> {
        let mut connection = self.connection.clone();
        let result: i32 = redis::cmd("EVAL")
            .arg(CHECK_REVISION_SCRIPT)
            .arg(1)
            .arg(&self.keys.control)
            .arg(SCHEMA_VERSION)
            .arg(expected)
            .query_async(&mut connection)
            .await
            .map_err(|error| Self::report_backend_error("revision check failed", error))?;
        match result {
            1 => Ok(true),
            0 => Ok(false),
            value => Err(Self::report_backend_error(
                "revision check failed",
                format_args!("invalid schema/status {value}"),
            )),
        }
    }

    async fn apply_patch(
        &self,
        expected_revision: u64,
        next_inode_id: u64,
        patch: &RedisPatch,
    ) -> Result<bool> {
        let new_revision = expected_revision.checked_add(1).ok_or_else(|| {
            Self::report_backend_error("mutation failed", "metadata revision overflow")
        })?;
        let encoded = serde_json::to_string(patch)
            .map_err(|error| Self::report_backend_error("patch encode failed", error))?;
        let dirty = serde_json::to_string(&patch.dirty_record())
            .map_err(|error| Self::report_backend_error("dirty record encode failed", error))?;
        let keys = self.keys.data_keys();
        let mut connection = self.connection.clone();
        let result: i32 = redis::cmd("EVAL")
            .arg(MUTATE_SCRIPT)
            .arg(7)
            .arg(&keys)
            .arg(SCHEMA_VERSION)
            .arg(expected_revision)
            .arg(new_revision)
            .arg(next_inode_id)
            .arg(encoded)
            .arg(dirty)
            .arg(COHERENCE_REVISION_WINDOW)
            .query_async(&mut connection)
            .await
            .map_err(|error| Self::report_backend_error("mutation transaction failed", error))?;
        match result {
            1 => Ok(true),
            0 => Ok(false),
            value => Err(Self::report_backend_error(
                "mutation transaction failed",
                format_args!("invalid schema/status {value}"),
            )),
        }
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
            Mutation::SetAttrs {
                inode,
                mode,
                uid,
                gid,
                atime,
                mtime,
            } => mem
                .set_attrs(*inode, *mode, *uid, *gid, *atime, *mtime)
                .await
                .map(MutationOutput::Attributes),
            Mutation::Mkdir { parent, name, mode } => mem
                .mkdir(*parent, name, *mode)
                .await
                .map(MutationOutput::Inode),
            Mutation::Link {
                parent,
                name,
                inode,
            } => mem
                .link(*parent, name, *inode)
                .await
                .map(MutationOutput::Nlink),
            Mutation::Unlink { parent, name, defer_reclaim } => mem
                .unlink_with_lifecycle(*parent, name, *defer_reclaim)
                .await
                .map(MutationOutput::Garbage),
            Mutation::FinalizeOrphan { inode } => mem
                .finalize_orphan(*inode)
                .await
                .map(MutationOutput::Garbage),
            Mutation::Rename {
                old_parent,
                old_name,
                new_parent,
                new_name,
                flags,
                defer_reclaim,
            } => mem
                .rename_with_lifecycle(
                    *old_parent, old_name, *new_parent, new_name, *flags, *defer_reclaim,
                )
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
            let loaded = self.load_snapshot().await?;
            let old_records = SnapshotRecords::from_snapshot(&loaded.snapshot)
                .map_err(|error| Self::report_backend_error("snapshot encode failed", error))?;
            let mem = MemStore::from_snapshot(loaded.snapshot);

            match Self::apply(&mem, &mutation).await {
                Ok(output) => {
                    let snapshot = mem.snapshot().await;
                    let new_records =
                        SnapshotRecords::from_snapshot(&snapshot).map_err(|error| {
                            Self::report_backend_error("snapshot encode failed", error)
                        })?;
                    let patch = RedisPatch::between(&old_records, &new_records);
                    let committed = if patch.is_empty() {
                        self.check_revision(loaded.revision).await?
                    } else {
                        self.apply_patch(loaded.revision, snapshot.next_inode_id, &patch)
                            .await?
                    };
                    if committed {
                        return Ok(output);
                    }
                }
                Err(error) => {
                    // Linearize semantic failures too: only return an error if
                    // the revision on which it was evaluated is still current.
                    if self.check_revision(loaded.revision).await? {
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
        let mut connection = self.connection.clone();
        let (control, parent_record, child): (
            HashMap<String, String>,
            Option<Vec<u8>>,
            Option<String>,
        ) = redis::pipe()
            .atomic()
            .cmd("HGETALL")
            .arg(&self.keys.control)
            .cmd("HGET")
            .arg(&self.keys.inodes)
            .arg(parent)
            .cmd("HGET")
            .arg(&self.keys.dirents)
            .arg(dirent_field(parent, name))
            .query_async(&mut connection)
            .await
            .map_err(|error| Self::report_backend_error("lookup failed", error))?;
        Self::parse_control(&control)
            .map_err(|error| Self::report_backend_error("control record invalid", error))?;
        let parent_record = parent_record.ok_or(MetaError::NotFound)?;
        let parent_inode: Inode = serde_json::from_slice(&parent_record)
            .map_err(|error| Self::report_backend_error("parent inode decode failed", error))?;
        if parent_inode.mode & 0o170000 != S_IFDIR {
            return Err(MetaError::NotADirectory);
        }
        child
            .ok_or(MetaError::NotFound)?
            .parse()
            .map_err(|error| Self::report_backend_error("dirent child is invalid", error))
    }

    async fn getattr(&self, inode: u64) -> Result<Inode> {
        self.load_inode(inode).await
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
        match self
            .mutate(Mutation::SetAttrs {
                inode,
                mode,
                uid,
                gid,
                atime,
                mtime,
            })
            .await?
        {
            MutationOutput::Attributes(attrs) => Ok(attrs),
            _ => unreachable!("set_attrs mutation returned wrong output"),
        }
    }

    async fn referenced_keys(&self, inode: u64) -> Result<Vec<String>> {
        let loaded = self.load_snapshot().await?;
        MemStore::from_snapshot(loaded.snapshot)
            .referenced_keys(inode)
            .await
    }

    async fn all_referenced_keys(&self) -> Result<Vec<String>> {
        let loaded = self.load_snapshot().await?;
        MemStore::from_snapshot(loaded.snapshot)
            .all_referenced_keys()
            .await
    }

    async fn read_slices(&self, inode: u64, chunk_idx: u32) -> Result<Vec<Slice>> {
        let mut connection = self.connection.clone();
        let (control, inode_record, encoded): (
            HashMap<String, String>,
            Option<Vec<u8>>,
            Option<Vec<u8>>,
        ) = redis::pipe()
            .atomic()
            .cmd("HGETALL")
            .arg(&self.keys.control)
            .cmd("HGET")
            .arg(&self.keys.inodes)
            .arg(inode)
            .cmd("HGET")
            .arg(&self.keys.slices)
            .arg(slice_field(inode, chunk_idx))
            .query_async(&mut connection)
            .await
            .map_err(|error| Self::report_backend_error("slice read failed", error))?;
        Self::parse_control(&control)
            .map_err(|error| Self::report_backend_error("control record invalid", error))?;
        if inode_record.is_none() {
            return Err(MetaError::NotFound);
        }
        match encoded {
            Some(encoded) => serde_json::from_slice(&encoded)
                .map_err(|error| Self::report_backend_error("slice decode failed", error)),
            None => Ok(Vec::new()),
        }
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
        let mut connection = self.connection.clone();
        let (control, inode_record, target): (
            HashMap<String, String>,
            Option<Vec<u8>>,
            Option<String>,
        ) = redis::pipe()
            .atomic()
            .cmd("HGETALL")
            .arg(&self.keys.control)
            .cmd("HGET")
            .arg(&self.keys.inodes)
            .arg(inode)
            .cmd("HGET")
            .arg(&self.keys.symlinks)
            .arg(inode)
            .query_async(&mut connection)
            .await
            .map_err(|error| Self::report_backend_error("readlink failed", error))?;
        Self::parse_control(&control)
            .map_err(|error| Self::report_backend_error("control record invalid", error))?;
        let inode_record = inode_record.ok_or(MetaError::NotFound)?;
        let inode_record: Inode = serde_json::from_slice(&inode_record)
            .map_err(|error| Self::report_backend_error("symlink inode decode failed", error))?;
        if inode_record.mode & 0o170000 != crate::fs_model::S_IFLNK {
            return Err(MetaError::NotASymlink);
        }
        target.ok_or_else(|| Self::report_backend_error("readlink failed", "target is missing"))
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
        let loaded = self.load_snapshot().await?;
        MemStore::from_snapshot(loaded.snapshot)
            .readdir(inode)
            .await
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
        self.unlink_with_lifecycle(parent, name, false).await
    }

    async fn unlink_with_lifecycle(
        &self,
        parent: u64,
        name: &str,
        defer_reclaim: bool,
    ) -> Result<Vec<String>> {
        match self
            .mutate(Mutation::Unlink {
                parent,
                name: name.to_owned(),
                defer_reclaim,
            })
            .await?
        {
            MutationOutput::Garbage(keys) => Ok(keys),
            _ => unreachable!("unlink mutation returned wrong output"),
        }
    }

    async fn finalize_orphan(&self, inode: u64) -> Result<Vec<String>> {
        match self.mutate(Mutation::FinalizeOrphan { inode }).await? {
            MutationOutput::Garbage(keys) => Ok(keys),
            _ => unreachable!("finalize orphan mutation returned wrong output"),
        }
    }

    async fn link(&self, parent: u64, name: &str, inode: u64) -> Result<u32> {
        match self
            .mutate(Mutation::Link {
                parent,
                name: name.to_owned(),
                inode,
            })
            .await?
        {
            MutationOutput::Nlink(nlink) => Ok(nlink),
            _ => unreachable!("link mutation returned wrong output"),
        }
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
            old_parent, old_name, new_parent, new_name, flags, false,
        ).await
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
        match self
            .mutate(Mutation::Rename {
                old_parent,
                old_name: old_name.to_owned(),
                new_parent,
                new_name: new_name.to_owned(),
                flags,
                defer_reclaim,
            })
            .await?
        {
            MutationOutput::Garbage(keys) => Ok(keys),
            _ => unreachable!("rename mutation returned wrong output"),
        }
    }

    async fn pending_garbage(&self) -> Result<Vec<String>> {
        let loaded = self.load_snapshot().await?;
        MemStore::from_snapshot(loaded.snapshot)
            .pending_garbage()
            .await
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

    async fn coherence_probe(&self, observed_revision: Option<u64>) -> Result<CoherenceProbe> {
        let mut connection = self.connection.clone();
        let response: Vec<String> = redis::cmd("EVAL")
            .arg(COHERENCE_PROBE_SCRIPT)
            .arg(2)
            .arg(&self.keys.control)
            .arg(&self.keys.dirty)
            .arg(SCHEMA_VERSION)
            .arg(observed_revision.map_or_else(String::new, |revision| revision.to_string()))
            .arg(COHERENCE_REVISION_WINDOW)
            .query_async(&mut connection)
            .await
            .map_err(|error| Self::report_backend_error("coherence probe failed", error))?;
        Self::decode_coherence_response(&response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_model::{ROOT_INODE, S_IFDIR, S_IFREG};
    use uuid::Uuid;

    #[test]
    fn record_field_encodings_round_trip_unicode_and_numeric_ids() {
        let field = dirent_field(42, "长名字:with/slash-like-bytes");
        assert_eq!(
            parse_dirent_field(&field).unwrap(),
            (42, "长名字:with/slash-like-bytes".to_owned())
        );
        assert_eq!(parse_slice_field(&slice_field(99, 17)).unwrap(), (99, 17));
    }

    #[tokio::test]
    async fn patch_contains_only_changed_records() {
        let old = MemStore::new().snapshot().await;
        let mem = MemStore::from_snapshot(old.clone());
        let created = mem.create(ROOT_INODE, "one-record", 0o640).await.unwrap();
        let new = mem.snapshot().await;
        let old_records = SnapshotRecords::from_snapshot(&old).unwrap();
        let new_records = SnapshotRecords::from_snapshot(&new).unwrap();
        let patch = RedisPatch::between(&old_records, &new_records);

        assert_eq!(patch.inodes_set.len(), 1);
        assert_eq!(patch.inodes_set[0].0, created.to_string());
        assert_eq!(patch.dirents_set.len(), 1);
        assert_eq!(
            patch.dirents_set[0].0,
            dirent_field(ROOT_INODE, "one-record")
        );
        assert!(patch.slices_set.is_empty());
        assert!(patch.gc_add.is_empty());
        let dirty = patch.dirty_record();
        assert!(!dirty.overflow);
        assert_eq!(dirty.inode_ids, vec![created]);
    }

    #[test]
    fn coherence_dirty_records_accumulate_and_overflow_to_full() {
        let records = vec![
            "records".to_owned(),
            "12".to_owned(),
            serde_json::to_string(&DirtyRecord {
                overflow: false,
                inode_ids: vec![9, 2],
            })
            .unwrap(),
            serde_json::to_string(&DirtyRecord {
                overflow: false,
                inode_ids: vec![9, 7],
            })
            .unwrap(),
        ];
        assert_eq!(
            RedisMetaStore::decode_coherence_response(&records).unwrap(),
            CoherenceProbe::Inodes {
                revision: 12,
                inode_ids: vec![2, 7, 9],
            }
        );

        let overflow = vec![
            "records".to_owned(),
            "13".to_owned(),
            serde_json::to_string(&DirtyRecord {
                overflow: true,
                inode_ids: Vec::new(),
            })
            .unwrap(),
        ];
        assert_eq!(
            RedisMetaStore::decode_coherence_response(&overflow).unwrap(),
            CoherenceProbe::Full { revision: 13 }
        );

        let too_many = vec![
            "records".to_owned(),
            "14".to_owned(),
            serde_json::to_string(&DirtyRecord {
                overflow: false,
                inode_ids: (1..=COHERENCE_DIRTY_INODE_MAX as u64).collect(),
            })
            .unwrap(),
            serde_json::to_string(&DirtyRecord {
                overflow: false,
                inode_ids: vec![u64::MAX],
            })
            .unwrap(),
        ];
        assert_eq!(
            RedisMetaStore::decode_coherence_response(&too_many).unwrap(),
            CoherenceProbe::Full { revision: 14 }
        );
    }

    #[tokio::test]
    async fn rename_overwrite_patch_contains_one_atomic_namespace_and_gc_change() {
        let mem = MemStore::new();
        let source = mem.create(ROOT_INODE, "source", 0o644).await.unwrap();
        let victim = mem.create(ROOT_INODE, "victim", 0o644).await.unwrap();
        let victim_slice = Slice {
            chunk_index: 0,
            slice_id: Uuid::new_v4(),
            chunk_offset: 0,
            length: 16,
            written_at: 1,
        };
        let victim_key = victim_slice.block_key(0);
        mem.append_slice(victim, victim_slice).await.unwrap();
        let old = mem.snapshot().await;

        assert_eq!(
            mem.rename(ROOT_INODE, "source", ROOT_INODE, "victim")
                .await
                .unwrap(),
            vec![victim_key.clone()]
        );
        let new = mem.snapshot().await;
        let patch = RedisPatch::between(
            &SnapshotRecords::from_snapshot(&old).unwrap(),
            &SnapshotRecords::from_snapshot(&new).unwrap(),
        );

        assert!(patch
            .dirents_del
            .contains(&dirent_field(ROOT_INODE, "source")));
        assert!(patch
            .dirents_set
            .contains(&(dirent_field(ROOT_INODE, "victim"), source.to_string())));
        assert!(patch.inodes_del.contains(&victim.to_string()));
        assert!(patch.slices_del.contains(&slice_field(victim, 0)));
        assert_eq!(patch.gc_add, vec![victim_key]);
    }

    #[tokio::test]
    async fn rename_noreplace_failure_produces_no_patch_or_gc() {
        let mem = MemStore::new();
        let source = mem.create(ROOT_INODE, "source", 0o644).await.unwrap();
        let target = mem.create(ROOT_INODE, "target", 0o644).await.unwrap();
        let old = mem.snapshot().await;

        assert!(matches!(
            mem.rename_with_flags(
                ROOT_INODE,
                "source",
                ROOT_INODE,
                "target",
                crate::meta::RENAME_NOREPLACE,
            )
            .await,
            Err(MetaError::AlreadyExists)
        ));
        let new = mem.snapshot().await;
        let patch = RedisPatch::between(
            &SnapshotRecords::from_snapshot(&old).unwrap(),
            &SnapshotRecords::from_snapshot(&new).unwrap(),
        );
        assert_eq!(mem.lookup(ROOT_INODE, "source").await.unwrap(), source);
        assert_eq!(mem.lookup(ROOT_INODE, "target").await.unwrap(), target);
        assert!(patch.is_empty());
        assert!(mem.pending_garbage().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rename_exchange_patch_atomically_sets_both_dirents_only() {
        let mem = MemStore::new();
        let left = mem.create(ROOT_INODE, "exchange-left", 0o640).await.unwrap();
        let right = mem.create(ROOT_INODE, "exchange-right", 0o600).await.unwrap();
        let old = mem.snapshot().await;

        mem.rename_with_flags(
            ROOT_INODE,
            "exchange-left",
            ROOT_INODE,
            "exchange-right",
            crate::meta::RENAME_EXCHANGE,
        )
        .await
        .unwrap();
        let new = mem.snapshot().await;
        let patch = RedisPatch::between(
            &SnapshotRecords::from_snapshot(&old).unwrap(),
            &SnapshotRecords::from_snapshot(&new).unwrap(),
        );

        assert!(patch.dirents_del.is_empty());
        assert!(patch.inodes_set.is_empty());
        assert!(patch.inodes_del.is_empty());
        assert!(patch.slices_set.is_empty());
        assert!(patch.slices_del.is_empty());
        assert!(patch.gc_add.is_empty());
        assert_eq!(
            patch.dirents_set,
            vec![
                (dirent_field(ROOT_INODE, "exchange-left"), right.to_string()),
                (dirent_field(ROOT_INODE, "exchange-right"), left.to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn hard_link_patch_atomically_updates_inode_and_dirent() {
        let mem = MemStore::new();
        let inode = mem.create(ROOT_INODE, "source", 0o644).await.unwrap();
        let old = mem.snapshot().await;
        assert_eq!(mem.link(ROOT_INODE, "alias", inode).await.unwrap(), 2);
        let new = mem.snapshot().await;
        let patch = RedisPatch::between(
            &SnapshotRecords::from_snapshot(&old).unwrap(),
            &SnapshotRecords::from_snapshot(&new).unwrap(),
        );

        assert_eq!(patch.inodes_set.len(), 1);
        assert_eq!(patch.inodes_set[0].0, inode.to_string());
        assert!(patch
            .dirents_set
            .contains(&(dirent_field(ROOT_INODE, "alias"), inode.to_string())));
        assert!(patch.gc_add.is_empty());
        assert!(patch.gc_del.is_empty());
    }

    #[tokio::test]
    async fn ownership_patch_changes_only_its_inode_record() {
        let mem = MemStore::new();
        let inode = mem.create(ROOT_INODE, "owned", 0o640).await.unwrap();
        let old_records = SnapshotRecords::from_snapshot(&mem.snapshot().await).unwrap();
        let attrs = mem
            .set_attrs(inode, None, Some(1234), Some(2345), None, None)
            .await
            .unwrap();
        let new_records = SnapshotRecords::from_snapshot(&mem.snapshot().await).unwrap();
        let patch = RedisPatch::between(&old_records, &new_records);

        assert_eq!((attrs.uid, attrs.gid), (1234, 2345));
        assert_eq!(patch.inodes_set.len(), 1);
        assert_eq!(patch.inodes_set[0].0, inode.to_string());
        assert!(patch.inodes_del.is_empty());
        assert!(patch.dirents_set.is_empty());
        assert!(patch.dirents_del.is_empty());
        assert!(patch.slices_set.is_empty());
        assert!(patch.slices_del.is_empty());
        assert!(patch.symlinks_set.is_empty());
        assert!(patch.symlinks_del.is_empty());
        assert!(patch.gc_add.is_empty());
        assert!(patch.gc_del.is_empty());
    }

    #[tokio::test]
    async fn timestamp_patch_changes_only_its_inode_record() {
        let mem = MemStore::new();
        let inode = mem.create(ROOT_INODE, "timed", 0o640).await.unwrap();
        let old_records = SnapshotRecords::from_snapshot(&mem.snapshot().await).unwrap();
        let attrs = mem
            .set_attrs(
                inode,
                None,
                None,
                None,
                Some(1_577_836_800),
                Some(1_577_836_801),
            )
            .await
            .unwrap();
        let new_records = SnapshotRecords::from_snapshot(&mem.snapshot().await).unwrap();
        let patch = RedisPatch::between(&old_records, &new_records);

        assert_eq!((attrs.atime, attrs.mtime), (1_577_836_800, 1_577_836_801));
        assert_eq!(patch.inodes_set.len(), 1);
        assert_eq!(patch.inodes_set[0].0, inode.to_string());
        assert!(patch.inodes_del.is_empty());
        assert!(patch.dirents_set.is_empty());
        assert!(patch.dirents_del.is_empty());
        assert!(patch.slices_set.is_empty());
        assert!(patch.slices_del.is_empty());
        assert!(patch.symlinks_set.is_empty());
        assert!(patch.symlinks_del.is_empty());
        assert!(patch.gc_add.is_empty());
        assert!(patch.gc_del.is_empty());
    }

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
        let legacy_prefix = format!("kestrelfs:test:legacy:{}", Uuid::new_v4());
        let client = redis::Client::open(url.as_str()).unwrap();
        let mut legacy_connection = client.get_multiplexed_async_connection().await.unwrap();
        let legacy_key = format!("{legacy_prefix}:{LEGACY_SNAPSHOT_KEY_SUFFIX}");
        let _: () = redis::cmd("SET")
            .arg(&legacy_key)
            .arg("{}")
            .query_async(&mut legacy_connection)
            .await
            .unwrap();
        assert!(matches!(
            RedisMetaStore::new(&url, &legacy_prefix).await,
            Err(RedisMetaStoreError::LegacySchema)
        ));
        let _: usize = redis::cmd("DEL")
            .arg(&legacy_key)
            .query_async(&mut legacy_connection)
            .await
            .unwrap();

        let prefix = format!("kestrelfs:test:{}", Uuid::new_v4());
        let store = RedisMetaStore::new(&url, &prefix).await.unwrap();
        let initial_revision = store.coherence_revision().await.unwrap().unwrap();

        let directory = store.mkdir(ROOT_INODE, "redis-dir", 0o1711).await.unwrap();
        let peer = RedisMetaStore::new(&url, &prefix).await.unwrap();
        let first_dirty = peer
            .coherence_probe(Some(initial_revision))
            .await
            .unwrap();
        match first_dirty {
            CoherenceProbe::Inodes {
                revision,
                inode_ids,
            } => {
                assert!(revision > initial_revision);
                assert!(inode_ids.contains(&ROOT_INODE));
                assert!(inode_ids.contains(&directory));
            }
            other => panic!("expected bounded dirty inode probe, got {other:?}"),
        }
        assert!(peer.coherence_revision().await.unwrap().unwrap() > initial_revision);
        assert_eq!(
            peer.getattr(directory).await.unwrap().mode,
            S_IFDIR | 0o1711
        );
        assert_eq!(peer.getattr(directory).await.unwrap().nlink, 2);
        assert_eq!(peer.getattr(ROOT_INODE).await.unwrap().nlink, 3);

        let other_directory = store.mkdir(ROOT_INODE, "redis-other", 0o750).await.unwrap();
        store.mkdir(directory, "redis-child", 0o700).await.unwrap();
        assert_eq!(peer.getattr(directory).await.unwrap().nlink, 3);
        store
            .rename(directory, "redis-child", other_directory, "moved-child")
            .await
            .unwrap();
        assert_eq!(peer.getattr(directory).await.unwrap().nlink, 2);
        assert_eq!(peer.getattr(other_directory).await.unwrap().nlink, 3);
        assert_eq!(peer.getattr(ROOT_INODE).await.unwrap().nlink, 4);

        let (left, right) = tokio::join!(
            store.create(directory, "concurrent-left", 0o644),
            peer.create(directory, "concurrent-right", 0o644)
        );
        assert_ne!(left.unwrap(), right.unwrap());
        store.lookup(directory, "concurrent-left").await.unwrap();
        store.lookup(directory, "concurrent-right").await.unwrap();

        let file = store.create(directory, "data", 0o2640).await.unwrap();
        assert_eq!(peer.getattr(file).await.unwrap().mode, S_IFREG | 0o2640);
        let persistent_mode_file = store
            .create(directory, "mode-persist", 0o2750)
            .await
            .unwrap();
        let revision_before_chmod = peer.coherence_revision().await.unwrap().unwrap();
        assert_eq!(
            store
                .set_mode(persistent_mode_file, S_IFDIR | 0o6751)
                .await
                .unwrap(),
            S_IFREG | 0o6751
        );
        assert_eq!(
            peer.getattr(persistent_mode_file).await.unwrap().mode,
            S_IFREG | 0o6751
        );
        assert!(peer.coherence_revision().await.unwrap().unwrap() > revision_before_chmod);
        let revision_before_chown = peer.coherence_revision().await.unwrap().unwrap();
        let owned = store
            .set_attrs(
                persistent_mode_file,
                None,
                Some(1234),
                Some(2345),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!((owned.uid, owned.gid), (1234, 2345));
        let peer_owned = peer.getattr(persistent_mode_file).await.unwrap();
        assert_eq!((peer_owned.uid, peer_owned.gid), (1234, 2345));
        assert!(peer.coherence_revision().await.unwrap().unwrap() > revision_before_chown);
        let revision_before_times = peer.coherence_revision().await.unwrap().unwrap();
        let timed = store
            .set_attrs(
                persistent_mode_file,
                None,
                None,
                None,
                Some(1_577_836_800),
                Some(1_577_836_801),
            )
            .await
            .unwrap();
        assert_eq!((timed.atime, timed.mtime), (1_577_836_800, 1_577_836_801));
        let peer_timed = peer.getattr(persistent_mode_file).await.unwrap();
        assert_eq!(
            (peer_timed.atime, peer_timed.mtime),
            (1_577_836_800, 1_577_836_801)
        );
        assert!(peer.coherence_revision().await.unwrap().unwrap() > revision_before_times);
        assert_eq!(store.link(directory, "data-alias", file).await.unwrap(), 2);
        assert_eq!(peer.lookup(directory, "data-alias").await.unwrap(), file);
        assert_eq!(peer.getattr(file).await.unwrap().nlink, 2);
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

        let orphan = store.create(directory, "open-orphan", 0o644).await.unwrap();
        let orphan_slice = Slice {
            chunk_index: 0,
            slice_id: Uuid::new_v4(),
            chunk_offset: 0,
            length: 11,
            written_at: 4,
        };
        let orphan_key = orphan_slice.block_key(0);
        store.append_slice(orphan, orphan_slice).await.unwrap();
        assert!(store
            .unlink_with_lifecycle(directory, "open-orphan", true)
            .await.unwrap().is_empty());
        assert_eq!(peer.getattr(orphan).await.unwrap().nlink, 0);
        assert_eq!(peer.finalize_orphan(orphan).await.unwrap(), vec![orphan_key.clone()]);
        assert!(matches!(store.getattr(orphan).await, Err(MetaError::NotFound)));

        let noreplace_source = store
            .create(directory, "noreplace-source", 0o644)
            .await
            .unwrap();
        let noreplace_target = store
            .create(directory, "noreplace-target", 0o644)
            .await
            .unwrap();
        assert!(matches!(
            store
                .rename_with_flags(
                    directory,
                    "noreplace-source",
                    directory,
                    "noreplace-target",
                    crate::meta::RENAME_NOREPLACE,
                )
                .await,
            Err(MetaError::AlreadyExists)
        ));
        assert_eq!(
            peer.lookup(directory, "noreplace-source").await.unwrap(),
            noreplace_source
        );
        assert_eq!(
            peer.lookup(directory, "noreplace-target").await.unwrap(),
            noreplace_target
        );
        store
            .link(directory, "noreplace-alias", noreplace_source)
            .await
            .unwrap();
        store
            .rename_with_flags(
                directory,
                "noreplace-source",
                directory,
                "noreplace-alias",
                crate::meta::RENAME_NOREPLACE,
            )
            .await
            .unwrap();
        assert_eq!(peer.getattr(noreplace_source).await.unwrap().nlink, 2);

        let exchange_left = store
            .create(directory, "exchange-left", 0o640)
            .await
            .unwrap();
        let exchange_right = store
            .create(directory, "exchange-right", 0o600)
            .await
            .unwrap();
        store
            .rename_with_flags(
                directory,
                "exchange-left",
                directory,
                "exchange-right",
                crate::meta::RENAME_EXCHANGE,
            )
            .await
            .unwrap();
        assert_eq!(
            peer.lookup(directory, "exchange-left").await.unwrap(),
            exchange_right
        );
        assert_eq!(
            peer.lookup(directory, "exchange-right").await.unwrap(),
            exchange_left
        );

        let source = store.create(directory, "source", 0o644).await.unwrap();
        assert_eq!(
            store
                .rename(directory, "source", directory, "victim")
                .await
                .unwrap(),
            vec![victim_key.clone()]
        );
        assert!(matches!(
            peer.lookup(directory, "source").await,
            Err(MetaError::NotFound)
        ));
        assert_eq!(peer.lookup(directory, "victim").await.unwrap(), source);
        assert!(store.unlink(directory, "data").await.unwrap().is_empty());
        assert_eq!(peer.getattr(file).await.unwrap().nlink, 1);
        assert_eq!(
            store.unlink(directory, "data-alias").await.unwrap(),
            vec![older_key.clone()]
        );

        let pending = store.pending_garbage().await.unwrap();
        assert!(pending.contains(&newer_key));
        assert!(pending.contains(&victim_key));
        assert!(pending.contains(&older_key));
        assert!(pending.contains(&orphan_key));

        let mut connection = store.connection.clone();
        let schema: String = redis::cmd("HGET")
            .arg(&store.keys.control)
            .arg("schema_version")
            .query_async(&mut connection)
            .await
            .unwrap();
        let inode_count: usize = redis::cmd("HLEN")
            .arg(&store.keys.inodes)
            .query_async(&mut connection)
            .await
            .unwrap();
        let legacy_exists: usize = redis::cmd("EXISTS")
            .arg(&store.keys.legacy_snapshot)
            .query_async(&mut connection)
            .await
            .unwrap();
        assert_eq!(schema, SCHEMA_VERSION);
        assert!(inode_count > 4);
        assert_eq!(legacy_exists, 0);

        drop(store);
        let restarted = RedisMetaStore::new(&url, &prefix).await.unwrap();
        let restored_link = restarted.lookup(directory, "renamed-link").await.unwrap();
        assert_eq!(
            restarted.readlink(restored_link).await.unwrap(),
            "../target-with-redis"
        );
        assert_eq!(restarted.getattr(directory).await.unwrap().mode, S_IFDIR | 0o1711);
        assert_eq!(restarted.getattr(directory).await.unwrap().nlink, 2);
        assert_eq!(restarted.getattr(other_directory).await.unwrap().nlink, 3);
        assert_eq!(restarted.getattr(ROOT_INODE).await.unwrap().nlink, 4);
        assert_eq!(
            restarted.getattr(persistent_mode_file).await.unwrap().mode,
            S_IFREG | 0o6751
        );
        assert_eq!(restarted.lookup(directory, "victim").await.unwrap(), source);
        assert_eq!(
            restarted.lookup(directory, "exchange-left").await.unwrap(),
            exchange_right
        );
        assert_eq!(
            restarted.lookup(directory, "exchange-right").await.unwrap(),
            exchange_left
        );
        assert_eq!(restarted.pending_garbage().await.unwrap(), pending);
        restarted.acknowledge_garbage(&pending).await.unwrap();
        assert!(restarted.pending_garbage().await.unwrap().is_empty());

        let mut connection = restarted.connection.clone();
        let keys: Vec<String> = redis::cmd("KEYS")
            .arg(format!("{prefix}:meta:*"))
            .query_async(&mut connection)
            .await
            .unwrap();
        if !keys.is_empty() {
            let _: usize = redis::cmd("DEL")
                .arg(keys)
                .query_async(&mut connection)
                .await
                .unwrap();
        }
    }
}
