//! Snapshot inventory construction and manifest-derived assembly inputs.
//!
//! This module owns the persisted-inventory read path and the legacy filesystem
//! fallback helpers. Repository acquisition and plugin identity calculation
//! consume these functions but do not own traversal policy.

use super::*;

pub(super) async fn load_snapshot_manifest(snapshot_root: &Path) -> Option<SnapshotManifest> {
    crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::InventoryParse);
    let bytes = tokio::fs::read(snapshot_root.join(MANIFEST_FILE))
        .await
        .ok()?;
    let mut manifest = serde_json::from_slice::<SnapshotManifest>(&bytes).ok()?;
    if !manifest.validate() {
        return None;
    }
    manifest.reindex();
    Some(manifest)
}

pub(super) fn manifest_doc_entries(
    manifest: &SnapshotManifest,
    filesource: &Arc<FileSource>,
    identity: &RepoSnapshotIdentity,
) -> Vec<(PathBuf, FileItem)> {
    manifest
        .doc_files()
        .iter()
        .map(|key| {
            let key = key.clone();
            (
                key.clone(),
                FileItem::new(
                    filesource.clone(),
                    FileIdentity::RepoFile(RepoFileIdentity::new(identity.clone(), key)),
                    MergeType::Conflict,
                ),
            )
        })
        .collect()
}

pub(super) fn manifest_lua_modules(manifest: &SnapshotManifest) -> Vec<String> {
    manifest.lua_roots().to_vec()
}

pub(super) async fn doc_file_entries(
    snapshot_root: &Path,
    filesource: &Arc<FileSource>,
    identity: &RepoSnapshotIdentity,
) -> Vec<(PathBuf, FileItem)> {
    let doc_root = snapshot_root.join("doc");
    let is_dir = tokio::fs::metadata(&doc_root)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false);
    if !is_dir {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut seen_dirs: hashbrown::HashSet<PathBuf> = hashbrown::HashSet::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(doc_root.clone(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 128 {
            continue;
        }
        if let Ok(canonical) = tokio::fs::canonicalize(&dir).await
            && !seen_dirs.insert(canonical)
        {
            continue;
        }
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            let ft = match entry.file_type().await {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push((path, depth + 1));
                continue;
            }
            let is_file = if ft.is_symlink() {
                tokio::fs::metadata(&path)
                    .await
                    .map(|m| m.is_file())
                    .unwrap_or(false)
            } else {
                ft.is_file()
            };
            if !is_file {
                continue;
            }
            let Ok(rel) = path.strip_prefix(&doc_root) else {
                continue;
            };
            let key = PathBuf::from("doc").join(rel);
            out.push((
                key.clone(),
                FileItem::new(
                    filesource.clone(),
                    FileIdentity::RepoFile(RepoFileIdentity::new(identity.clone(), key)),
                    MergeType::Conflict,
                ),
            ));
        }
    }
    out
}

pub(super) async fn extract_unique_lua_modules_from_snapshot(snapshot_root: &Path) -> Vec<String> {
    let mut rd = match tokio::fs::read_dir(snapshot_root.join("lua")).await {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    let mut seen = hashbrown::HashSet::new();
    let mut out = Vec::new();
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = match entry.file_name().to_str() {
            Some(n) => n.to_string(),
            None => continue,
        };
        let stem = if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            name
        } else {
            Path::new(&name)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string()
        };
        if !stem.is_empty() && seen.insert(stem.clone()) {
            out.push(stem);
        }
    }
    out
}
