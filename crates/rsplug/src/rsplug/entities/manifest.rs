//! Phase 2: 不変 snapshot manifest。
//!
//! snapshot が ready になった時点でツリーを1回 walk し、フラットな
//! (相対パス, 種別, symlink target) リストを `.rsplug-manifest-v1.json` に記録する。
//! 以降の merge/copy 計画はこの manifest からパス集合を引けるため、繰り返しの
//! filesystem walk（stat / read_dir）を省ける（Part B）。
//!
//! manifest はあくまで **cache** である。欠損・陳腐化・parse 失敗時は呼出元が
//! filesystem に fallback するため、manifest の不正が install の正当性を損ねることはない。
//!
//! `copy_eligible`・build 成果物の `build_digest` は filesystem 事象ではなく
//! 設定・build cache 由来の値なので本 schema には含めない（必要になった時点で
//! schema を上げて追加する）。manifest は純粋な filesystem record に徹する。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use hashbrown::HashSet;
use serde::{Deserialize, Serialize};

/// snapshot ルート直下の manifest ファイル名。
pub(super) const MANIFEST_FILE: &str = ".rsplug-manifest-v1.json";
/// manifest schema 版。意味を変える変更時のみ上げる。
pub(super) const MANIFEST_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SnapshotManifest {
    pub(super) schema: u32,
    pub(super) entries: Vec<ManifestEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct ManifestEntry {
    /// snapshot ルートからの相対パス（例: `lua/foo.lua`）。
    pub(super) path: PathBuf,
    pub(super) kind: ManifestKind,
    /// symlink の場合の link target。それ以外は `None`。
    pub(super) symlink_target: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum ManifestKind {
    File,
    Dir,
    Symlink,
}

impl SnapshotManifest {
    /// `root` を再帰 walk して manifest を構築する。
    /// `.git`（`dotgit=false` のとき）・build 成功 marker・manifest 自体を除外する。
    /// symlink は種別のみ記録し、follow しない（ループ回避・pack copy は leaf 扱い）。
    pub(super) async fn build(
        root: &Path,
        dotgit: bool,
        build_success_file: &str,
    ) -> std::io::Result<Self> {
        crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::InventoryBuild);
        let mut entries = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let mut rd = tokio::fs::read_dir(&dir).await?;
            while let Some(entry) = rd.next_entry().await? {
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
                let meta = tokio::fs::symlink_metadata(&path).await?;
                let (kind, symlink_target) = if meta.is_symlink() {
                    let target = tokio::fs::read_link(&path).await.ok();
                    (ManifestKind::Symlink, target)
                } else if meta.is_dir() {
                    (ManifestKind::Dir, None)
                } else {
                    (ManifestKind::File, None)
                };
                entries.push(ManifestEntry {
                    path: rel,
                    kind,
                    symlink_target,
                });
                if kind == ManifestKind::Dir {
                    stack.push(path);
                }
            }
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Self {
            schema: MANIFEST_SCHEMA,
            entries,
        })
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
            .join(format!(".rsplug-manifest-{}.tmp", std::process::id()));
        tokio::fs::write(&tmp, &content).await?;
        tokio::fs::rename(&tmp, &final_path).await?;
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

    /// `rel` の種別。manifest に無ければ `None`（呼出元は filesystem に fallback）。
    pub(super) fn kind_of(&self, rel: &Path) -> Option<ManifestKind> {
        crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::ManifestLinearScan);
        self.entries
            .iter()
            .find(|e| e.path.as_os_str() == rel.as_os_str())
            .map(|e| e.kind)
    }

    /// `parent` 直下の子エントリ名集合。`parent` が Dir ならその子（空も可）、File なら空集合、
    /// **Symlink または不在なら `None`**（呼出元は filesystem に fallback する）。
    /// symlink は manifest が follow しないため子が不明であり、`is_dir()`/`read_dir`（共に
    /// follow する）と一致させるには filesystem に問うしかない。
    pub(super) fn child_names(&self, parent: &Path) -> Option<HashSet<OsString>> {
        crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::ManifestLinearScan);
        match self.kind_of(parent) {
            Some(ManifestKind::Dir) => {}
            Some(ManifestKind::File) => return Some(HashSet::new()),
            Some(ManifestKind::Symlink) | None => return None,
        }
        let mut set = HashSet::new();
        for e in &self.entries {
            if e.path
                .parent()
                .is_some_and(|p| p.as_os_str() == parent.as_os_str())
                && let Some(name) = e.path.file_name()
            {
                set.insert(name.to_owned());
            }
        }
        Some(set)
    }
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
}
