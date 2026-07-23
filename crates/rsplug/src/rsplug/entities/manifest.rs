//! Phase 2: 不変 snapshot manifest。
//!
//! snapshot が ready になった時点でツリーを1回 walk し、フラットな
//! (相対パス, 種別, symlink target) リストを `.rsplug-manifest-v1.json` に記録する。
//! 以降の merge/copy 計画はこの manifest からパス集合を引けるため、繰り返しの
//! filesystem walk（stat / read_dir）を省ける（Part B）。
//!
//! manifest はあくまで **cache** である。欠損・陳腐化・parse 失敗時は呼出元が
//! filesystem に fallback するため、manifest の不正が install の正当性を損ねることはない。
//! 検証済みの `SnapshotHandle` 経路では、merge に filesystem fallback を許さない。
//!
//! `copy_eligible`・build 成果物の `build_digest` は filesystem 事象ではなく
//! 設定・build cache 由来の値なので本 schema には含めない（必要になった時点で
//! schema を上げて追加する）。manifest は純粋な filesystem record に徹する。

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hashbrown::{HashMap, HashSet};
use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::Xxh3;

/// snapshot ルート直下の manifest ファイル名。
pub(super) const MANIFEST_FILE: &str = ".rsplug-manifest-v1.json";
/// manifest schema 版。意味を変える変更時のみ上げる。
pub(super) const MANIFEST_SCHEMA: u32 = 1;
static MANIFEST_TEMP_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SnapshotManifest {
    pub(super) schema: u32,
    pub(super) entries: Vec<ManifestEntry>,
    /// Optional plain-tree content identity. It is populated only for a
    /// built tarball snapshot so inventory and identity do not require two
    /// recursive walks.
    #[serde(default)]
    pub(super) content_digest: Option<[u8; 16]>,
    /// Runtime-only indices rebuilt from the deterministic serialized vector.
    /// They are deliberately skipped by serde so the on-disk format remains compact
    /// and independent of hash-table iteration order.
    #[serde(skip)]
    children: HashMap<PathBuf, HashSet<OsString>>,
    /// Virtual entries visible through symlinks whose targets resolve inside
    /// the snapshot.  Keeping these in the inventory preserves follow-dir
    /// merge semantics without a merge-time stat/read_dir.
    #[serde(skip)]
    virtual_kinds: HashMap<PathBuf, ManifestKind>,
    #[serde(skip)]
    virtual_children: HashMap<PathBuf, HashSet<OsString>>,
    /// Derived once from `entries`; never serialized, so hash-map order cannot
    /// affect the durable cache format.
    #[serde(skip)]
    top_level: Vec<PathBuf>,
    #[serde(skip)]
    lua_roots: Vec<String>,
    #[serde(skip)]
    doc_files: Vec<PathBuf>,
    #[serde(skip)]
    ftplugin_files: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct ManifestEntry {
    /// snapshot ルートからの相対パス（例: `lua/foo.lua`）。
    pub(super) path: PathBuf,
    pub(super) kind: ManifestKind,
    /// symlink の場合の link target。それ以外は `None`。
    pub(super) symlink_target: Option<PathBuf>,
    /// Followed target kind, when the target exists. This avoids a later
    /// merge-time stat while retaining the symlink itself as a leaf entry.
    #[serde(default)]
    pub(super) followed_kind: Option<ManifestKind>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum ManifestKind {
    File,
    Dir,
    Symlink,
}

impl SnapshotManifest {
    pub(super) fn reindex(&mut self) {
        self.children.clear();
        self.virtual_kinds.clear();
        self.virtual_children.clear();
        self.top_level.clear();
        self.lua_roots.clear();
        self.doc_files.clear();
        self.ftplugin_files.clear();
        let mut lua_seen = HashSet::new();
        let mut last_top_level: Option<PathBuf> = None;
        for entry in &self.entries {
            if let Some(parent) = entry.path.parent()
                && let Some(name) = entry.path.file_name()
            {
                self.children
                    .entry(parent.to_path_buf())
                    .or_default()
                    .insert(name.to_owned());
            }
            if let Some(first) = entry.path.components().next() {
                let first = PathBuf::from(first.as_os_str());
                if last_top_level.as_ref() != Some(&first) {
                    last_top_level = Some(first.clone());
                    self.top_level.push(first);
                }
            }
            if entry.path.starts_with("doc/") && entry.kind != ManifestKind::Dir {
                self.doc_files.push(entry.path.clone());
            }
            if entry.path.starts_with("ftplugin/") && entry.kind != ManifestKind::Dir {
                self.ftplugin_files.push(entry.path.clone());
            }
            if let Ok(relative) = entry.path.strip_prefix("lua")
                && let Some(first) = relative.components().next()
            {
                // A directory is a module root as-is (`lua/foo/...` -> `foo`),
                // while a top-level Lua file is required without its suffix
                // (`lua/foo.lua` -> `foo`).  The filesystem fallback has always
                // applied this distinction; preserve it when deriving from the
                // persisted inventory too.
                let root = if entry.kind == ManifestKind::Dir {
                    first.as_os_str().to_str()
                } else {
                    Path::new(first.as_os_str()).file_stem().and_then(|stem| stem.to_str())
                };
                if let Some(root) = root
                    && !root.is_empty()
                    && lua_seen.insert(root.to_owned())
                {
                    self.lua_roots.push(root.to_owned());
                }
            }
        }
        self.top_level.sort();
        self.lua_roots.sort();
        self.doc_files.sort();
        self.ftplugin_files.sort();

        // Materialize the portion of an in-snapshot directory target visible
        // through each symlink. External targets remain opaque, which is the
        // safe, deterministic representation for merge planning.
        let entries = self.entries.clone();
        for link in entries.iter().filter(|entry| {
            entry.kind == ManifestKind::Symlink && entry.followed_kind == Some(ManifestKind::Dir)
        }) {
            let Some(target) = link.symlink_target.as_ref() else {
                continue;
            };
            let Some(target) = normalize_relative_path(
                link.path.parent().unwrap_or_else(|| Path::new("")),
                target,
            ) else {
                continue;
            };
            for target_entry in &entries {
                let Ok(suffix) = target_entry.path.strip_prefix(&target) else {
                    continue;
                };
                let virtual_path = if suffix.as_os_str().is_empty() {
                    link.path.clone()
                } else {
                    link.path.join(suffix)
                };
                if virtual_path == link.path {
                    continue;
                }
                self.virtual_kinds
                    .entry(virtual_path.clone())
                    .or_insert(target_entry.kind);
                if let Some(parent) = virtual_path.parent()
                    && let Some(name) = virtual_path.file_name()
                {
                    self.virtual_children
                        .entry(parent.to_path_buf())
                        .or_default()
                        .insert(name.to_owned());
                }
            }
        }
    }

    /// Validate persisted paths before allowing the index to answer queries.
    /// Invalid or ambiguous manifests are treated as cache misses by callers.
    pub(super) fn validate(&self) -> bool {
        if self.schema != MANIFEST_SCHEMA {
            return false;
        }
        let mut previous: Option<&Path> = None;
        for entry in &self.entries {
            let path = entry.path.as_path();
            if path.is_absolute() || path.as_os_str().is_empty() {
                return false;
            }
            if path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::CurDir
                        | std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            }) {
                return false;
            }
            if previous.is_some_and(|old| old >= path) {
                return false;
            }
            previous = Some(path);
        }
        true
    }

    /// `root` を再帰 walk して manifest を構築する。
    /// `.git`（`dotgit=false` のとき）・build 成功 marker・manifest 自体を除外する。
    /// symlink は種別のみ記録し、follow しない（ループ回避・pack copy は leaf 扱い）。
    pub(super) async fn build(
        root: &Path,
        dotgit: bool,
        build_success_file: &str,
    ) -> std::io::Result<Self> {
        Ok(
            Self::build_with_content_digest(root, dotgit, build_success_file, false)
                .await?
                .0,
        )
    }

    pub(super) async fn build_with_content_digest(
        root: &Path,
        dotgit: bool,
        build_success_file: &str,
        include_content_digest: bool,
    ) -> std::io::Result<(Self, Option<[u8; 16]>)> {
        crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::InventoryBuild);
        let mut entries = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let mut rd = tokio::fs::read_dir(&dir).await?;
            while let Some(entry) = rd.next_entry().await? {
                crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::DirectoryEntry);
                let name = entry.file_name();
                let path = entry.path();
                if name == MANIFEST_FILE || name == build_success_file {
                    continue;
                }
                if name == ".git" && !dotgit {
                    continue;
                }
                let rel = path
                    .strip_prefix(root)
                    .map_err(|e| {
                        std::io::Error::other(format!("manifest path not under root: {e}"))
                    })?
                    .to_path_buf();
                let file_type = entry.file_type().await?;
                let (kind, symlink_target, followed_kind) = if file_type.is_symlink() {
                    let target = tokio::fs::read_link(&path).await.ok();
                    let followed_kind = tokio::fs::metadata(&path).await.ok().map(|metadata| {
                        if metadata.is_dir() {
                            ManifestKind::Dir
                        } else {
                            ManifestKind::File
                        }
                    });
                    (ManifestKind::Symlink, target, followed_kind)
                } else if file_type.is_dir() {
                    (ManifestKind::Dir, None, None)
                } else {
                    (ManifestKind::File, None, None)
                };
                entries.push(ManifestEntry {
                    path: rel,
                    kind,
                    symlink_target,
                    followed_kind,
                });
                if kind == ManifestKind::Dir {
                    stack.push(path);
                }
            }
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let mut manifest = Self {
            schema: MANIFEST_SCHEMA,
            entries,
            content_digest: None,
            children: HashMap::new(),
            virtual_kinds: HashMap::new(),
            virtual_children: HashMap::new(),
            top_level: Vec::new(),
            lua_roots: Vec::new(),
            doc_files: Vec::new(),
            ftplugin_files: Vec::new(),
        };
        if include_content_digest {
            let mut hasher = Xxh3::new();
            for entry in &manifest.entries {
                if entry.kind != ManifestKind::File && entry.kind != ManifestKind::Symlink {
                    continue;
                }
                hasher.update(entry.path.to_string_lossy().as_bytes());
                hasher.update(b"\0");
                let content = tokio::fs::read(root.join(&entry.path)).await?;
                crate::rsplug::perf::incr_content_bytes(content.len() as u64);
                hasher.update(&content);
                hasher.update(b"\0");
            }
            let digest = hasher.digest128().to_ne_bytes();
            manifest.content_digest = Some(digest);
        }
        manifest.reindex();
        let digest = manifest.content_digest;
        Ok((manifest, digest))
    }

    /// manifest を `snapshot_root` に原子書き込みする（temp + rename）。
    /// tmp は snapshot と同じ fs の `worktrees/` に置き、rename で原子公開する。
    /// manifest は cache なので、呼出元は本関数のエラーを無視（best-effort）してよい。
    pub(super) async fn write(&self, snapshot_root: &Path) -> std::io::Result<()> {
        let content = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::other(format!("manifest serialize failed: {e}")))?;
        let final_path = snapshot_root.join(MANIFEST_FILE);
        let tmp = snapshot_root
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!(
                ".rsplug-manifest-{}-{}.tmp",
                std::process::id(),
                MANIFEST_TEMP_NONCE.fetch_add(1, Ordering::Relaxed)
            ));
        tokio::fs::write(&tmp, &content).await?;
        if let Err(error) = tokio::fs::rename(&tmp, &final_path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(error);
        }
        Ok(())
    }

    /// `root` を walk して manifest を構築し、`snapshot_root` に書き込む（best-effort cache）。
    pub(super) async fn build_and_write(
        snapshot_root: &Path,
        dotgit: bool,
        build_success_file: &str,
    ) -> std::io::Result<()> {
        let manifest = Self::build(snapshot_root, dotgit, build_success_file).await?;
        manifest.write(snapshot_root).await
    }

    /// `rel` の種別。通常の entries に無ければ symlink overlay も検索する。
    pub(super) fn kind_of(&self, rel: &Path) -> Option<ManifestKind> {
        crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::ManifestPathLookup);
        self.entries
            .binary_search_by(|entry| entry.path.as_path().cmp(rel))
            .ok()
            .map(|index| self.entries[index].kind)
            .or_else(|| self.virtual_kinds.get(rel).copied())
    }

    pub(super) fn followed_kind_of(&self, rel: &Path) -> Option<ManifestKind> {
        self.entries
            .binary_search_by(|entry| entry.path.as_path().cmp(rel))
            .ok()
            .and_then(|index| self.entries[index].followed_kind)
    }

    /// `parent` 直下の子エントリ名集合。symlink は in-snapshot target の
    /// overlay children を返し、不在は空集合。
    pub(super) fn child_names(&self, parent: &Path) -> Option<HashSet<OsString>> {
        match self.kind_of(parent) {
            Some(ManifestKind::Dir) => {}
            Some(ManifestKind::File) => return Some(HashSet::new()),
            Some(ManifestKind::Symlink) => {
                return Some(
                    self.virtual_children
                        .get(parent)
                        .cloned()
                        .unwrap_or_default(),
                );
            }
            None => return Some(HashSet::new()),
        }
        Some(self.children.get(parent).cloned().unwrap_or_default())
    }

    #[allow(dead_code)]
    pub(super) fn top_level(&self) -> &[PathBuf] {
        &self.top_level
    }

    pub(super) fn lua_roots(&self) -> &[String] {
        &self.lua_roots
    }

    pub(super) fn doc_files(&self) -> &[PathBuf] {
        &self.doc_files
    }

    #[allow(dead_code)]
    pub(super) fn ftplugin_files(&self) -> &[PathBuf] {
        &self.ftplugin_files
    }
}

fn normalize_relative_path(parent: &Path, target: &Path) -> Option<PathBuf> {
    let mut result = PathBuf::new();
    let joined = if target.is_absolute() {
        return None;
    } else {
        parent.join(target)
    };
    for component in joined.components() {
        match component {
            std::path::Component::Normal(value) => result.push(value),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !result.pop() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    (!result.as_os_str().is_empty()).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn build_records_kinds_recursion_and_exclusions() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // lua/foo.lua, lua/sub/bar.lua, plugin.vim, link -> ../elsewhere, .git/config
        tokio::fs::create_dir_all(root.join("lua/sub"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(root.join(".git")).await.unwrap();
        tokio::fs::write(root.join("lua/foo.lua"), b"x")
            .await
            .unwrap();
        tokio::fs::write(root.join("lua/sub/bar.lua"), b"y")
            .await
            .unwrap();
        tokio::fs::write(root.join("plugin.vim"), b"z")
            .await
            .unwrap();
        #[cfg(unix)]
        tokio::fs::symlink("../elsewhere", root.join("link"))
            .await
            .unwrap();
        tokio::fs::write(root.join(".rsplug_build_success"), b"id")
            .await
            .unwrap();

        let m = SnapshotManifest::build(root, false, ".rsplug_build_success")
            .await
            .unwrap();

        let by_path = |p: &str| m.entries.iter().find(|e| e.path.as_path() == Path::new(p));

        // .git と build marker は除外。
        assert!(by_path(".git").is_none());
        assert!(by_path(".rsplug_build_success").is_none());
        // ディレクトリ自身と再帰内容の両方を記録。
        assert_eq!(by_path("lua").unwrap().kind, ManifestKind::Dir);
        assert_eq!(by_path("lua/foo.lua").unwrap().kind, ManifestKind::File);
        assert_eq!(by_path("lua/sub").unwrap().kind, ManifestKind::Dir);
        assert_eq!(by_path("lua/sub/bar.lua").unwrap().kind, ManifestKind::File);
        assert_eq!(by_path("plugin.vim").unwrap().kind, ManifestKind::File);
        // symlink は follow せず、target を記録。
        #[cfg(unix)]
        {
            let link = by_path("link").unwrap();
            assert_eq!(link.kind, ManifestKind::Symlink);
            assert_eq!(
                link.symlink_target.as_deref(),
                Some(std::path::Path::new("../elsewhere"))
            );
        }
        // ソート済み。
        let mut sorted = m.entries.clone();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(m.entries, sorted);
        assert_eq!(m.schema, MANIFEST_SCHEMA);
    }

    #[tokio::test]
    async fn build_includes_dotgit_when_dotgit_true() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        tokio::fs::create_dir_all(root.join(".git")).await.unwrap();
        tokio::fs::write(root.join(".git/config"), b"c")
            .await
            .unwrap();

        let m = SnapshotManifest::build(root, true, ".rsplug_build_success")
            .await
            .unwrap();
        assert_eq!(
            m.entries
                .iter()
                .find(|e| e.path.as_path() == Path::new(".git"))
                .unwrap()
                .kind,
            ManifestKind::Dir
        );
        assert!(
            m.entries
                .iter()
                .any(|e| e.path.as_path() == Path::new(".git/config"))
        );
    }

    #[tokio::test]
    async fn write_roundtrips_through_json() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        tokio::fs::create_dir_all(root.join("plugin"))
            .await
            .unwrap();
        tokio::fs::write(root.join("plugin/init.lua"), b"x")
            .await
            .unwrap();

        SnapshotManifest::build_and_write(root, false, ".rsplug_build_success")
            .await
            .unwrap();

        let on_disk = std::fs::read_to_string(root.join(MANIFEST_FILE)).unwrap();
        let parsed: SnapshotManifest = serde_json::from_str(&on_disk).unwrap();
        assert_eq!(parsed.schema, MANIFEST_SCHEMA);
        assert!(
            parsed
                .entries
                .iter()
                .any(|e| e.path.as_path() == Path::new("plugin/init.lua"))
        );
        // manifest 自体は記録されない。
        assert!(
            parsed
                .entries
                .iter()
                .all(|e| e.path.as_path() != Path::new(MANIFEST_FILE))
        );
    }

    #[test]
    fn indexed_queries_do_not_use_serialized_vector_scans() {
        let mut manifest = SnapshotManifest {
            schema: MANIFEST_SCHEMA,
            entries: vec![
                ManifestEntry {
                    path: PathBuf::from("lua"),
                    kind: ManifestKind::Dir,
                    symlink_target: None,
                    followed_kind: None,
                },
                ManifestEntry {
                    path: PathBuf::from("lua/init.lua"),
                    kind: ManifestKind::File,
                    symlink_target: None,
                    followed_kind: None,
                },
            ],
            content_digest: None,
            children: HashMap::new(),
            virtual_kinds: HashMap::new(),
            virtual_children: HashMap::new(),
            top_level: Vec::new(),
            lua_roots: Vec::new(),
            doc_files: Vec::new(),
            ftplugin_files: Vec::new(),
        };
        manifest.reindex();
        assert_eq!(
            manifest.kind_of(Path::new("lua/init.lua")),
            Some(ManifestKind::File)
        );
        assert_eq!(
            manifest.child_names(Path::new("lua")).unwrap(),
            HashSet::from([OsString::from("init.lua")])
        );
        assert_eq!(manifest.top_level(), &[PathBuf::from("lua")]);
        assert_eq!(manifest.lua_roots(), &["init".to_string()]);
        assert!(manifest.validate());
    }

    #[test]
    fn indexed_queries_include_in_snapshot_symlink_children() {
        let mut manifest = SnapshotManifest {
            schema: MANIFEST_SCHEMA,
            entries: vec![
                ManifestEntry {
                    path: PathBuf::from("link"),
                    kind: ManifestKind::Symlink,
                    symlink_target: Some(PathBuf::from("target")),
                    followed_kind: Some(ManifestKind::Dir),
                },
                ManifestEntry {
                    path: PathBuf::from("target"),
                    kind: ManifestKind::Dir,
                    symlink_target: None,
                    followed_kind: None,
                },
                ManifestEntry {
                    path: PathBuf::from("target/init.lua"),
                    kind: ManifestKind::File,
                    symlink_target: None,
                    followed_kind: None,
                },
            ],
            content_digest: None,
            children: HashMap::new(),
            virtual_kinds: HashMap::new(),
            virtual_children: HashMap::new(),
            top_level: Vec::new(),
            lua_roots: Vec::new(),
            doc_files: Vec::new(),
            ftplugin_files: Vec::new(),
        };
        manifest.reindex();
        assert_eq!(
            manifest.child_names(Path::new("link")).unwrap(),
            HashSet::from([OsString::from("init.lua")])
        );
        assert_eq!(
            manifest.kind_of(Path::new("link/init.lua")),
            Some(ManifestKind::File)
        );
    }

    #[test]
    fn invalid_schema_and_traversal_are_cache_misses() {
        let mut manifest = SnapshotManifest {
            schema: MANIFEST_SCHEMA + 1,
            entries: vec![ManifestEntry {
                path: PathBuf::from("../escape"),
                kind: ManifestKind::File,
                symlink_target: None,
                followed_kind: None,
            }],
            content_digest: None,
            children: HashMap::new(),
            virtual_kinds: HashMap::new(),
            virtual_children: HashMap::new(),
            top_level: Vec::new(),
            lua_roots: Vec::new(),
            doc_files: Vec::new(),
            ftplugin_files: Vec::new(),
        };
        assert!(!manifest.validate());
        manifest.schema = MANIFEST_SCHEMA;
        assert!(!manifest.validate());
    }

    #[tokio::test]
    async fn reindex_derives_lua_file_stems_as_module_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        tokio::fs::create_dir_all(root.join("lua/dir_module"))
            .await
            .unwrap();
        tokio::fs::write(root.join("lua/file_module.lua"), b"return {}")
            .await
            .unwrap();
        tokio::fs::write(root.join("lua/dir_module/init.lua"), b"return {}")
            .await
            .unwrap();

        let manifest = SnapshotManifest::build(root, false, ".rsplug_build_success")
            .await
            .unwrap();

        assert_eq!(
            manifest.lua_roots(),
            ["dir_module".to_string(), "file_module".to_string()]
        );
    }
}
