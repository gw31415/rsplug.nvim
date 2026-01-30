use std::path::Path;

use serde::{Deserialize, Serialize};

use super::config::PluginConfig;

/// Lock file structure that contains all necessary information to build the pack directory.
/// This is serialized to JSON format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockFile {
    /// Version of the lock file format
    pub version: String,
    /// Plugin configurations (collection of PluginConfigs as described in TOML)
    pub plugins: Vec<PluginConfig>,
    /// Locked information for resources requiring network connection
    pub locked: Vec<LockedResource>,
}

/// Locked resource information for network-dependent resources
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedResource {
    /// Repository URL
    pub url: String,
    /// Git commit hash
    pub rev: String,
}

impl LockFile {
    /// Create a new lock file with the current version
    pub fn new() -> Self {
        Self {
            version: "1".to_string(),
            plugins: Vec::new(),
            locked: Vec::new(),
        }
    }

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
        let path = path.as_ref();
        let content = serde_json::to_string_pretty(self).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Failed to serialize lock file {:?}: {}", path, e),
            )
        })?;
        tokio::fs::write(path, content).await
    }
}

impl Default for LockFile {
    fn default() -> Self {
        Self::new()
    }
}
