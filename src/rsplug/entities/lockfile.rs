use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Lock file structure that contains all necessary information to build the pack directory.
/// This is serialized to JSON format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockFile {
    /// Version of the lock file format
    pub version: String,
    /// Embedded TOML configuration content
    /// This allows building from lock file alone without requiring separate TOML files
    pub toml_configs: Vec<TomlConfig>,
    /// Locked plugin information
    pub plugins: Vec<LockedPlugin>,
}

/// TOML configuration embedded in the lock file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TomlConfig {
    /// Path to the original TOML file (for reference)
    pub path: PathBuf,
    /// Content of the TOML file
    pub content: String,
}

/// Information about a locked plugin
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedPlugin {
    /// Plugin identifier (e.g., "owner/repo" or custom name)
    pub id: String,
    /// Repository source information
    pub repo: RepoSourceLock,
    /// Git commit hash that was resolved
    pub resolved_rev: String,
    /// Whether this plugin should be symlinked
    pub to_sym: bool,
    /// Build commands
    pub build: Vec<String>,
}

/// Locked repository source information
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum RepoSourceLock {
    GitHub {
        owner: String,
        repo: String,
        /// Requested revision (can be a wildcard or specific tag/branch/commit)
        requested_rev: Option<String>,
        /// URL of the repository
        url: String,
    },
}

impl LockFile {
    /// Create a new lock file with the current version
    pub fn new() -> Self {
        Self {
            version: "1".to_string(),
            toml_configs: Vec::new(),
            plugins: Vec::new(),
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
