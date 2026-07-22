use std::{borrow::Cow, collections::BTreeMap, path::Path};

use serde::{Deserialize, Serialize};

use super::util;

/// Lock file structure that contains all necessary information to build the pack directory.
/// This is serialized to JSON format.
#[derive(Debug, Serialize, Deserialize)]
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

    /// 全 lock キーを canonical identity（`host[:port]/path`）に正規化し、
    /// 同一 identity への重複エントリを統合する（PLANS「Model and repository identity」）。
    ///
    /// 後方互換のため、旧形式の生 URL キー（`https://...`）も [`util::repo::canonicalize_lock_key`]
    /// で正規化する。同一 canonical に同じ rev があれば dedup、異なる rev があれば衝突として
    /// エラーを返す（PLANS「reject conflicting revisions」）。
    pub fn normalize_keys(mut self) -> Result<Self, std::io::Error> {
        // canonical キー → (resource, 代表の元キー) 。衝突エラー表示に元キーを使う。
        let mut canonical: BTreeMap<String, (LockedResource, String)> = BTreeMap::new();
        for (raw_key, res) in std::mem::take(&mut self.locked) {
            let canon = util::repo::canonicalize_lock_key(&raw_key);
            if let Some((existing, existing_raw)) = canonical.get(&canon) {
                if existing.rev != res.rev {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "Conflicting revisions for {}: {} (from lock key {:?}) vs {} (from lock key {:?})",
                            canon, existing.rev, existing_raw, res.rev, raw_key
                        ),
                    ));
                }
                // 同一 rev → dedup（最初のエントリを保持）
            } else {
                canonical.insert(canon, (res, raw_key));
            }
        }
        self.locked = canonical.into_iter().map(|(k, (v, _))| (k, v)).collect();
        Ok(self)
    }

    /// Write the lock file to disk
    pub async fn write(&self, path: impl AsRef<Path>) -> Result<(), std::io::Error> {
        let path = path.as_ref().to_path_buf();
        crate::rsplug::perf::failpoint("lock_write_before")
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::LockWrite);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn lock_entry(rev: &str) -> LockedResource {
        LockedResource {
            kind: LockedResourceType::Git,
            rev: rev.to_string(),
        }
    }

    #[test]
    fn normalize_keys_dedups_same_rev_across_url_forms() {
        // 同一リポジトリの異なる URL 表記が同一 canonical に正規化され、同 rev で dedup される。
        let mut locked = BTreeMap::new();
        locked.insert("https://github.com/o/r".to_string(), lock_entry("aaaa"));
        locked.insert(
            "ssh://git@github.com/o/r.git".to_string(),
            lock_entry("aaaa"),
        );
        let lf = LockFile {
            version: "1".into(),
            locked,
        };
        let lf = lf.normalize_keys().unwrap();
        assert_eq!(lf.locked.len(), 1);
        assert_eq!(lf.locked.get("github.com/o/r").unwrap().rev, "aaaa");
    }

    #[test]
    fn normalize_keys_rejects_conflicting_revisions() {
        let mut locked = BTreeMap::new();
        locked.insert("https://github.com/o/r".to_string(), lock_entry("aaaa"));
        locked.insert(
            "ssh://git@github.com/o/r.git".to_string(),
            lock_entry("bbbb"),
        );
        let lf = LockFile {
            version: "1".into(),
            locked,
        };
        let err = lf.normalize_keys().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        let msg = err.to_string();
        assert!(msg.contains("github.com/o/r"), "msg={}", msg);
        assert!(msg.contains("Conflicting"), "msg={}", msg);
    }

    #[test]
    fn normalize_keys_accepts_already_canonical_and_shorthand() {
        let mut locked = BTreeMap::new();
        locked.insert("github.com/o/r".to_string(), lock_entry("aaaa"));
        locked.insert("owner2/repo2".to_string(), lock_entry("bbbb"));
        let lf = LockFile {
            version: "2".into(),
            locked,
        };
        let lf = lf.normalize_keys().unwrap();
        assert_eq!(lf.locked.get("github.com/o/r").unwrap().rev, "aaaa");
        assert_eq!(
            lf.locked.get("github.com/owner2/repo2").unwrap().rev,
            "bbbb"
        );
    }

    #[test]
    fn normalize_keys_preserves_non_default_port() {
        let mut locked = BTreeMap::new();
        locked.insert(
            "https://gitlab.com:2222/o/r".to_string(),
            lock_entry("cccc"),
        );
        let lf = LockFile {
            version: "1".into(),
            locked,
        };
        let lf = lf.normalize_keys().unwrap();
        assert!(lf.locked.contains_key("gitlab.com:2222/o/r"));
    }
}
