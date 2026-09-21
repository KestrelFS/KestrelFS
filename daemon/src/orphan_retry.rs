// SPDX-License-Identifier: Apache-2.0
//! Crash-safe handoff for kernel-proven final-close orphan retries.
//!
//! The kernel is the only component that can prove its mount-local open-handle
//! count reached zero.  It keeps that proof queued (and pins the module) until
//! the daemon has atomically copied the inode id into this data-dir state file.
//! A later daemon instance can therefore finish `FINALIZE_ORPHAN` without
//! guessing from Redis session expiry or scanning unlinked metadata.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const STATE_VERSION: u32 = 1;
pub(crate) const STATE_FILE_NAME: &str = ".orphan-retries-v1.json";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    inode_ids: Vec<u64>,
}

/// Small, atomically replaced set stored beside the daemon's persistent data.
#[derive(Debug)]
pub(crate) struct DurableOrphanRetries {
    path: PathBuf,
    inode_ids: BTreeSet<u64>,
}

impl DurableOrphanRetries {
    pub(crate) fn open(data_dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(data_dir)?;
        let path = data_dir.join(STATE_FILE_NAME);
        let inode_ids = match fs::read(&path) {
            Ok(bytes) => {
                let snapshot: Snapshot = serde_json::from_slice(&bytes).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid orphan retry state {}: {error}", path.display()),
                    )
                })?;
                if snapshot.version != STATE_VERSION {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "unsupported orphan retry state version {} in {}",
                            snapshot.version,
                            path.display()
                        ),
                    ));
                }
                let ids: BTreeSet<_> = snapshot.inode_ids.into_iter().collect();
                if ids.contains(&0) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("zero inode id in orphan retry state {}", path.display()),
                    ));
                }
                ids
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeSet::new(),
            Err(error) => return Err(error),
        };
        Ok(Self { path, inode_ids })
    }

    pub(crate) fn snapshot(&self) -> Vec<u64> {
        self.inode_ids.iter().copied().collect()
    }

    /// Durably records proof before the daemon acknowledges the kernel queue.
    pub(crate) fn record(&mut self, inode_id: u64) -> io::Result<bool> {
        if inode_id == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot persist zero orphan inode id",
            ));
        }
        if self.inode_ids.contains(&inode_id) {
            return Ok(false);
        }
        let mut updated = self.inode_ids.clone();
        updated.insert(inode_id);
        self.persist(&updated)?;
        self.inode_ids = updated;
        Ok(true)
    }

    /// Removes proof only after finalize committed (or returned NotFound).
    pub(crate) fn remove(&mut self, inode_id: u64) -> io::Result<bool> {
        if !self.inode_ids.contains(&inode_id) {
            return Ok(false);
        }
        let mut updated = self.inode_ids.clone();
        updated.remove(&inode_id);
        self.persist(&updated)?;
        self.inode_ids = updated;
        Ok(true)
    }

    fn persist(&self, inode_ids: &BTreeSet<u64>) -> io::Result<()> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let snapshot = Snapshot {
            version: STATE_VERSION,
            inode_ids: inode_ids.iter().copied().collect(),
        };
        let bytes = serde_json::to_vec_pretty(&snapshot)
            .map_err(|error| io::Error::other(error.to_string()))?;
        let tmp = self
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp)?;
            file.write_all(&bytes)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp, &self.path)?;
            File::open(parent)?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_set_survives_reopen_and_removal() {
        let dir = tempfile::tempdir().unwrap();
        let mut retries = DurableOrphanRetries::open(dir.path()).unwrap();
        assert!(retries.record(91).unwrap());
        assert!(retries.record(17).unwrap());
        assert!(!retries.record(91).unwrap());

        let mut reopened = DurableOrphanRetries::open(dir.path()).unwrap();
        assert_eq!(reopened.snapshot(), vec![17, 91]);
        assert!(reopened.remove(17).unwrap());
        assert!(!reopened.remove(17).unwrap());
        assert_eq!(
            DurableOrphanRetries::open(dir.path()).unwrap().snapshot(),
            vec![91]
        );
    }

    #[test]
    fn invalid_state_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(STATE_FILE_NAME), b"not-json").unwrap();
        let error = DurableOrphanRetries::open(dir.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
