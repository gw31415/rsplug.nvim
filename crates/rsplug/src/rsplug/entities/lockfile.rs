use std::{borrow::Cow, collections::BTreeMap, path::Path};

use serde::{Deserialize, Serialize};

/// Lock file structure that contains all necessary information to build the pack directory.
/// This is serialized to JSON format.
#[derive(Serialize, Deserialize)]
pub struct LockFile {
    /// Version of the lock file format
    pub version: Cow<'static, str>,
    /// Locked resources by repository URL
    pub locked: BTreeMap<String, LockedResource>,
}

/// Locked resource information for network-dependent resources
#[derive(Debug, Serialize, Deserialize)]
pub struct LockedResource {
    /// Resource type (e.g. git)
    #[serde(rename = "type")]
    pub kind: LockedResourceType,
    /// Git commit hash
    pub rev: String,
}

/// Resource type discriminator for lock entries
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LockedResourceType {
    Git,
}

impl LockFile {
    /// Read a lock file from disk
    pub async fn read(path: impl AsRef<Path>) -> Result<Self, std::io::Error> {
        let path = path.as_ref();
        let content = tokio::fs::read(path).await?;
        serde_json::from_slice(&content).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Failed to parse lock file {:?}: {}", path, e),
            )
        })
    }

    /// Write the lock file to disk
    pub async fn write(&self, path: impl AsRef<Path>) -> Result<(), std::io::Error> {
        let path = path.as_ref().to_path_buf();
        let content = serde_json::to_string_pretty(self).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Failed to serialize lock file {:?}: {}", path, e),
            )
        })?;
        // A directly-written lockfile can be observed half-written by a second
        // rsplug invocation (or by Nix tooling).  Publish a fully synced temp
        // file with rename instead.  The blocking filesystem calls are kept off
        // the async runtime because sync_all can stall on networked homes.
        tokio::task::spawn_blocking(move || {
            use std::io::Write;

            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            std::fs::create_dir_all(parent)?;
            let mut temp = tempfile::NamedTempFile::new_in(parent)?;
            temp.write_all(content.as_bytes())?;
            temp.as_file().sync_all()?;
            temp.persist(&path).map_err(|e| e.error)?;
            // Persisting does not guarantee the directory entry itself reached
            // durable storage.  On platforms where directories cannot be
            // opened/synced this is best-effort, while the atomic rename still
            // protects readers.
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
            Ok(())
        })
        .await
        .map_err(|e| std::io::Error::other(format!("lockfile write join failed: {e}")))?
    }
}
