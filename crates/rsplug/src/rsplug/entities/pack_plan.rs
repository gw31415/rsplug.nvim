use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, BinaryHeap},
    ffi::{OsStr, OsString},
    hash::{Hash, Hasher},
    io,
    ops::Add,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU64, Ordering as AtomicOrdering},
    },
};

use crate::log::{Message, msg};
use adaptive_semaphore::AdaptiveSemaphore;
use hashbrown::{HashMap, HashSet};
use sailfish::TemplateSimple;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use super::*;

#[path = "merge.rs"]
mod merge;

/// Git リポジトリ snapshot の論理 identity。
///
/// **絶対配置パス（cache root や `snapshot_root`）は含めない。** identity は
/// `repo_cache_dir`(相対)・`head_rev`・`dirty_diff`・`build`・`lua_build` のみで決まり、
/// `LoadedPlugin::plugin_id()` を通じて `_gen` id を決める。これらを構造体に集約することで、
/// identity に影響する入力の追加・変更がコンパイラによって検出される。
#[derive(Hash, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub(super) struct RepoSnapshotIdentity {
    /// `repos/` からの相対 repo パス（例: `github.com/owner/repo`）。どの repo かを識別する。
    repo_cache_dir: PathBuf,
    /// HEAD コミットハッシュ
    head_rev: Box<[u8]>,
    /// 作業ツリーに未コミット変更がある場合の差分ハッシュ。クリーンなら None。
    dirty_diff: Option<[u8; 16]>,
    /// TOML設定の build コマンド
    build: Arc<[String]>,
    /// TOML設定の lua_build スクリプト
    lua_build: Option<Arc<str>>,
}

impl RepoSnapshotIdentity {
    pub(super) fn new(
        repo_cache_dir: PathBuf,
        head_rev: Vec<u8>,
        dirty_diff: Option<[u8; 16]>,
        build: Arc<[String]>,
        lua_build: Option<Arc<str>>,
    ) -> Self {
        Self {
            repo_cache_dir,
            head_rev: head_rev.into_boxed_slice(),
            dirty_diff,
            build,
            lua_build,
        }
    }

    /// `worktrees/<snapshot_key>` の directory 名を生成する (PLANS §7)。
    ///
    /// **`dirty_diff` は含めない**: key は commit + build/lua_build 入力のみで決まり、
    /// build を実行する前に確定する。これにより「同じ入力の snapshot が既にあれば build を
    /// スキップして再利用」できる。build 成果物の差（dirty）は `RepoSnapshotIdentity`
    /// （ひいては `plugin_id`）に反映されるため、異なる成果物は別 `_gen` id になる。
    /// `build`・`lua_build` が共に無ければ `<head_rev>` のみ、あれば `<head_rev>__v1_<hash>`。
    /// `repo_cache_dir` は key に含めない（`worktrees/` は repo ごとに分かれているため暗黙）。
    pub(super) fn snapshot_key(&self) -> String {
        let head_rev = String::from_utf8_lossy(&self.head_rev);
        if self.build.is_empty() && self.lua_build.is_none() {
            head_rev.into_owned()
        } else {
            let input = SnapshotKeyInput {
                schema: SNAPSHOT_KEY_SCHEMA,
                head_rev: &self.head_rev,
                build: &self.build,
                lua_build: self.lua_build.as_deref(),
            };
            format!(
                "{head_rev}__v{}_{}",
                SNAPSHOT_KEY_SCHEMA,
                crate::rsplug::util::hash::digest_hash_hex_string(&input)
            )
        }
    }
}

/// Immutable snapshot placement and inventory handle shared by all file items
/// originating from one ready snapshot. The root is placement state; identity
/// remains the only logical input to package IDs.
#[derive(Debug)]
pub(super) struct SnapshotHandle {
    pub(super) root: Arc<Path>,
    #[allow(dead_code)]
    pub(super) identity: RepoSnapshotIdentity,
    pub(super) inventory: Arc<SnapshotManifest>,
}

/// `snapshot_key` の hash 入力 (PLANS §7)。絶対パス・`dirty_diff` は含めない。
#[derive(Hash)]
struct SnapshotKeyInput<'a> {
    schema: u8,
    head_rev: &'a [u8],
    build: &'a [String],
    lua_build: Option<&'a str>,
}

/// `snapshot_key` の schema 版。意味を変える変更時のみ上げる。
const SNAPSHOT_KEY_SCHEMA: u8 = 1;

impl std::fmt::Debug for RepoSnapshotIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoSnapshotIdentity")
            .field("repo_cache_dir", &self.repo_cache_dir)
            .field("head_rev", &String::from_utf8_lossy(&self.head_rev))
            .field("dirty_diff", &self.dirty_diff)
            .finish_non_exhaustive()
    }
}

/// repo snapshot 内の個別ファイルの identity。`relative_path` も identity に含む。
#[derive(Hash, PartialEq, Eq, PartialOrd, Ord, Clone, Debug)]
pub(super) struct RepoFileIdentity {
    snapshot: RepoSnapshotIdentity,
    relative_path: PathBuf,
}

impl RepoFileIdentity {
    pub(super) fn new(snapshot: RepoSnapshotIdentity, relative_path: PathBuf) -> Self {
        Self {
            snapshot,
            relative_path,
        }
    }
}

/// ファイルの論理 identity。repo 由来か生成ファイルかを区別する。
/// 絶対配置パスは含まず、生成ファイルは内容の `data_hash` で同一性を決める。
#[derive(Hash, PartialEq, Eq, PartialOrd, Ord, Clone, Debug)]
pub(super) enum FileIdentity {
    RepoFile(RepoFileIdentity),
    GeneratedFile { path: PathBuf, data_hash: [u8; 16] },
}

impl FileIdentity {
    /// snapshot root（あるいは配置ルート）からの相対パス。種別解決・copy 配置で使う。
    pub(super) fn relative_path(&self) -> &Path {
        match self {
            FileIdentity::RepoFile(r) => &r.relative_path,
            FileIdentity::GeneratedFile { path, .. } => path,
        }
    }
}

/// プラグインファイルの配置方法。
#[derive(Debug, PartialEq, Eq, Hash)]
pub(super) enum HowToPlaceFiles {
    CopyEachFile(BTreeMap<PathBuf, FileItem>),
}

impl Default for HowToPlaceFiles {
    fn default() -> Self {
        HowToPlaceFiles::CopyEachFile(BTreeMap::new())
    }
}

/// インストール単位となるプラグイン。
/// NOTE: 遅延実行されるプラグイン等は、インストール後に LazyRegistration が生成される。LazyRegistrationはまとめて
/// PluginLoadedに変換する。
#[derive(Debug, Hash, PartialEq, Eq)]
pub struct LoadedPlugin {
    /// `on_source` から参照される設定上の名前。マージ時に和集合で蓄積し、両側の
    /// 参照名をすべて残す（Phase 1: マージで source_name を潰さない）。
    pub(super) source_names: BTreeSet<String>,
    /// プラグインの遅延実行タイプ
    pub lazy_type: LazyType,
    /// 配置するファイル
    pub(super) files: HowToPlaceFiles,
    /// セットアップスクリプト
    pub(super) script: SetupScript,
    /// 設定/DAG後の読み込み順。特に controlled startup の順序維持に使う。
    pub(super) order: usize,
    /// マージを許可するか（TOMLの `merge` フィールド）
    pub(super) merge_enabled: bool,
    /// LazyRegistrationを元に作成されたかどうか
    pub(super) is_lazy_registration: bool,
    /// pack に `.git` を含めるか（git 利用プラグイン用）。`dotgit=true` なら `Plugin::load` が
    /// `.git` を通常 sealed-dir エントリとして列挙に含め（他ディレクトリと同一経路で copy される）、
    /// install で `.git` エントリが無ければ `PluginDotgitMissing` で skip する。
    pub(super) dotgit: bool,
}

impl LoadedPlugin {
    /// 全フィールドの [`Hash`] から [`PluginID`] を導出する。
    /// フィールド追加・変更は自動的に PluginID に反映される。
    pub fn plugin_id(&self) -> PluginID {
        <Self as HasPluginId>::plugin_id(self)
    }

    /// 配置（runtime）用の snapshot root。repo 由来でなければ（script-only や生成ファイルのみ
    /// なら）`None`。**配置情報であり `plugin_id` の hash には含まれない** (PLANS §10.3)。
    pub fn snapshot_root(&self) -> Option<Arc<Path>> {
        let HowToPlaceFiles::CopyEachFile(files) = &self.files;
        files
            .values()
            .next()
            .and_then(|item| match item.source.as_ref() {
                FileSource::Directory { path, .. } => Some(path.clone()),
                FileSource::File { .. } => None,
            })
    }

    /// self を `(rest, doc)` に分割する。`rest` は `doc/**` を除外した元プラグイン。
    /// `doc` は抜き出した `doc/**` を持つ `_rsplug:doc` プラグイン（doc が無ければ `None`）。
    /// doc を「LoadedPlugin のまま」扱い、control マージで rsplug-doc・lazy loader と統一的に
    /// 1つの `_rsplug:doc` に集約する（Phase 8: `LazyRegistration.overwrite_files` 中間表現を廃止）。
    pub(super) fn split_doc(self) -> (LoadedPlugin, Option<LoadedPlugin>) {
        let LoadedPlugin {
            source_names,
            lazy_type,
            files,
            script,
            order,
            merge_enabled,
            is_lazy_registration,
            dotgit,
        } = self;
        let HowToPlaceFiles::CopyEachFile(mut map) = files;
        let doc_keys: Vec<PathBuf> = map
            .keys()
            .filter(|p| p.starts_with("doc/") && p.as_path() != Path::new("doc"))
            .cloned()
            .collect();
        let mut doc_map = BTreeMap::new();
        for key in doc_keys {
            if let Some(mut file) = map.remove(&key) {
                file.merge_type = MergeType::Overwrite;
                doc_map.insert(key, file);
            }
        }
        let rest = LoadedPlugin {
            source_names,
            lazy_type,
            files: HowToPlaceFiles::CopyEachFile(map),
            script,
            order,
            merge_enabled,
            is_lazy_registration,
            dotgit,
        };
        let doc = if doc_map.is_empty() {
            None
        } else {
            Some(LoadedPlugin {
                source_names: BTreeSet::from([
                    super::lazy_registration::DOC_PLUGIN_NAME.to_string()
                ]),
                lazy_type: LazyType::Start,
                files: HowToPlaceFiles::CopyEachFile(doc_map),
                script: SetupScript::default(),
                order: usize::MAX,
                merge_enabled: true,
                is_lazy_registration: true,
                dotgit: false,
            })
        };
        (rest, doc)
    }
}

/// FileItem の種別（file/dir）。実行時に遅延解決され `FileItem.kind` にキャッシュされる。
/// 配置情報であり identity ではないため hash/eq には含めない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileKind {
    File,
    Directory,
}

/// 種別キャッシュの内部値（`FileItem.kind`）。
const KIND_UNCACHED: u8 = 0;
const KIND_FILE: u8 = 1;
const KIND_DIRECTORY: u8 = 2;

#[derive(Debug)]
pub(super) struct FileItem {
    pub source: Arc<FileSource>,
    /// ファイルの論理 identity。絶対配置パスは含まず、repo 由来か生成かで決まる。
    pub identity: FileIdentity,
    pub merge_type: MergeType,
    /// 種別キャッシュ（実行時）。`KIND_UNCACHED`/`KIND_FILE`/`KIND_DIRECTORY`。
    /// hash/eq から除外（plugin_id を非決定論化しない）。
    kind: AtomicU8,
}

// identity に関わる3フィールドのみで同値・hash を判定する。`kind`（実行時キャッシュ）は除外。
impl PartialEq for FileItem {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
            && self.identity == other.identity
            && self.merge_type == other.merge_type
    }
}

impl Eq for FileItem {}

impl Hash for FileItem {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.source.hash(state);
        self.identity.hash(state);
        self.merge_type.hash(state);
    }
}

impl FileItem {
    pub(super) fn new(
        source: Arc<FileSource>,
        identity: FileIdentity,
        merge_type: MergeType,
    ) -> Self {
        Self {
            source,
            identity,
            merge_type,
            kind: AtomicU8::new(KIND_UNCACHED),
        }
    }

    /// このエントリの種別（file/dir）。初回のみ filesystem で解決しキャッシュする。
    /// `FileSource::File`（生成ファイル）は常に file。
    pub(super) fn kind(&self) -> FileKind {
        let cached = self.kind.load(AtomicOrdering::Acquire);
        if cached != KIND_UNCACHED {
            return if cached == KIND_DIRECTORY {
                FileKind::Directory
            } else {
                FileKind::File
            };
        }
        let resolved = match self.source.as_ref() {
            FileSource::File { .. } => FileKind::File,
            FileSource::Directory {
                path,
                inventory,
                handle,
            } => {
                let relative = self.identity.relative_path();
                let manifest = handle
                    .as_ref()
                    .map(|handle| handle.inventory.as_ref())
                    .or(inventory.as_deref());
                let kind = manifest.and_then(|manifest| manifest.kind_of(relative));
                match kind {
                    Some(ManifestKind::Dir) => FileKind::Directory,
                    // A symlink's target kind still requires follow semantics. Keep
                    // that exceptional filesystem fallback, while ordinary entries
                    // never stat the snapshot during merge.
                    Some(ManifestKind::File) => FileKind::File,
                    Some(ManifestKind::Symlink) => match manifest
                        .and_then(|manifest| manifest.followed_kind_of(relative))
                    {
                        Some(ManifestKind::Dir) => FileKind::Directory,
                        Some(ManifestKind::File) | Some(ManifestKind::Symlink) => FileKind::File,
                        None if handle.is_some() => FileKind::File,
                        None => {
                            if path.join(relative).is_dir() {
                                FileKind::Directory
                            } else {
                                FileKind::File
                            }
                        }
                    },
                    None if handle.is_some() => FileKind::File,
                    None => {
                        if path.join(relative).is_dir() {
                            FileKind::Directory
                        } else {
                            FileKind::File
                        }
                    }
                }
            }
        };
        self.kind.store(
            match resolved {
                FileKind::File => KIND_FILE,
                FileKind::Directory => KIND_DIRECTORY,
            },
            AtomicOrdering::Release,
        );
        resolved
    }
}

impl PartialOrd for LoadedPlugin {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for LoadedPlugin {
    fn cmp(&self, other: &Self) -> Ordering {
        // lazy_type → order の順で比較する。
        // BinaryHeap (max-heap) では先頭に来るほど pop が早い。
        // startup load で order 昇順に取り出すため、
        // heap 上では order が小さいほど「大きい」と見なす必要がある。
        let cmp = self.lazy_type.cmp(&other.lazy_type);
        if let Ordering::Equal = cmp {
            // order は小さいほど先に取り出したいので逆順
            return self.order.cmp(&other.order).reverse();
        }
        cmp
    }
}

/// Pure merge-planning boundary. The planner owns deterministic ordering and
/// compatibility decisions; publication/copy code only consumes its result.
pub(super) struct MergePlanner;

impl MergePlanner {
    pub(super) fn plan(plugs: &mut BinaryHeap<LoadedPlugin>) {
        merge::MergePlanner::plan(plugs);
    }
}

/// Reference implementation retained only for randomized equivalence tests.
/// Production planning is owned by `entities::merge`.
#[cfg(test)]
impl LoadedPlugin {
    /// BinaryHeap に保存された PluginLoaded 群を可能な範囲でマージする。
    ///
    /// BinaryHeap の pop 順に基づいて貪欲にマージすると、同順位要素や
    /// マージ後に id/order が変化する要素によって、マージの組み合わせが
    /// 実行ごとに変わり得る。いったん決定的な順序に並べ、同じ順序で
    /// first-fit の fixed point を作ることで、マージパターンを一意にする。
    pub fn merge(plugs: &mut BinaryHeap<Self>) {
        #[derive(Clone, Eq, PartialEq, Ord, PartialOrd)]
        struct MergeSortKey {
            lazy_type: LazyType,
            order: usize,
            is_lazy_registration: bool,
            plugin_id: PluginID,
        }

        struct MergeEntry {
            plugin: LoadedPlugin,
            key: MergeSortKey,
        }

        fn key(plugin: &LoadedPlugin) -> MergeSortKey {
            MergeSortKey {
                lazy_type: plugin.lazy_type.clone(),
                order: plugin.order,
                is_lazy_registration: plugin.is_lazy_registration,
                plugin_id: plugin.plugin_id(),
            }
        }

        let mut items = Vec::with_capacity(plugs.len());
        while let Some(plug) = plugs.pop() {
            items.push(MergeEntry {
                key: key(&plug),
                plugin: plug,
            });
        }
        items.sort_by(|left, right| left.key.cmp(&right.key));

        // Keep group storage stable while the order is maintained by an index
        // set. Merging a group must not shift all later entries (the old
        // remove/insert loop made the cost quadratic and invalidated cached
        // references in instrumentation).
        let mut groups: Vec<Option<MergeEntry>> = Vec::with_capacity(items.len());
        let mut ordered = BTreeSet::<(MergeSortKey, usize)>::new();
        for item in items {
            let mut pending = Some(item);

            loop {
                let mut merged = false;
                let candidates = ordered.iter().map(|(_, index)| *index).collect::<Vec<_>>();
                for i in candidates {
                    crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::MergeAttempt);
                    let Some(candidate) = groups[i].take() else {
                        continue;
                    };
                    ordered.remove(&(candidate.key.clone(), i));
                    let current = pending
                        .take()
                        .expect("merge candidate loop must retain a pending plugin");
                    let pending_key = current.key;
                    let pending_plugin = current.plugin;
                    match candidate.plugin + pending_plugin {
                        (merged_group, None) => {
                            let entry = MergeEntry {
                                key: key(&merged_group),
                                plugin: merged_group,
                            };
                            pending = Some(entry);
                            merged = true;
                            break;
                        }
                        (candidate_plugin, Some(rest)) => {
                            let entry = MergeEntry {
                                key: candidate.key,
                                plugin: candidate_plugin,
                            };
                            ordered.insert((entry.key.clone(), i));
                            groups[i] = Some(entry);
                            pending = Some(MergeEntry {
                                key: pending_key,
                                plugin: rest,
                            });
                        }
                    }
                }

                if !merged {
                    break;
                }
            }

            let pending = pending
                .take()
                .expect("merge must retain an unmerged plugin");
            let index = groups.len();
            ordered.insert((pending.key.clone(), index));
            groups.push(Some(pending));
        }

        plugs.extend(
            ordered
                .into_iter()
                .filter_map(|(_, index)| groups[index].take())
                .map(|entry| entry.plugin),
        );
    }
}

impl Add for LoadedPlugin {
    type Output = (Self, Option<Self>);
    fn add(self, rhs: Self) -> Self::Output {
        if self.lazy_type != rhs.lazy_type {
            return (self, Some(rhs));
        }
        // `merge = false` は start/opt を問わずユーザプラグインのマージを阻止する。
        // 生成された LazyRegistration アーティファクトはユーザ設定によらず内部集約できるが、
        // ユーザプラグインと混ざってはならない。なお `merge` のデフォルトは true
        // （MergeConfig: Default + `#[serde(default)]`）なので、未指定ならマージする。
        if self.is_lazy_registration != rhs.is_lazy_registration
            || !(self.is_lazy_registration || self.merge_enabled && rhs.merge_enabled)
        {
            return (self, Some(rhs));
        }
        match (&self.files, &rhs.files) {
            (HowToPlaceFiles::CopyEachFile(files), HowToPlaceFiles::CopyEachFile(rfiles)) => {
                let mergeable = dirs_mergeable(files, rfiles);
                if mergeable {
                    let Self {
                        mut source_names,
                        lazy_type,
                        files: HowToPlaceFiles::CopyEachFile(mut files),
                        mut script,
                        order,
                        merge_enabled,
                        is_lazy_registration,
                        dotgit,
                    } = self;
                    let Self {
                        source_names: r_source_names,
                        lazy_type: _,
                        files: HowToPlaceFiles::CopyEachFile(rfiles),
                        script: rscript,
                        order: r_order,
                        merge_enabled: _,
                        is_lazy_registration: r_is_lazy_registration,
                        dotgit: r_dotgit,
                    } = rhs;
                    files = union_files(files, rfiles);
                    script += rscript;
                    let order = order.min(r_order);
                    // マージで source_name を潰さず、両側の on_source 参照名をすべて保持する。
                    source_names.extend(r_source_names);

                    return (
                        Self {
                            source_names,
                            lazy_type,
                            files: HowToPlaceFiles::CopyEachFile(files),
                            script,
                            order,
                            merge_enabled,
                            is_lazy_registration: is_lazy_registration || r_is_lazy_registration,
                            dotgit: dotgit || r_dotgit,
                        },
                        None,
                    );
                }
            }
        };
        (self, Some(rhs))
    }
}

/// `root` 配下の `rel` がディレクトリか。handle を持たない legacy/test
/// source のみがこの filesystem fallback を使う。
fn merge_is_dir(root: &Path, rel: &Path) -> bool {
    root.join(rel).is_dir()
}

fn source_manifest(source: &FileSource) -> Option<&SnapshotManifest> {
    match source {
        FileSource::Directory {
            inventory, handle, ..
        } => handle
            .as_ref()
            .map(|handle| handle.inventory.as_ref())
            .or(inventory.as_deref()),
        FileSource::File { .. } => None,
    }
}

fn source_is_dir(source: &FileSource, rel: &Path) -> bool {
    let Some(root) = snapshot_root_of(source) else {
        return false;
    };
    if let Some(manifest) = source_manifest(source)
        && let Some(kind) = manifest.kind_of(rel)
    {
        return match kind {
            ManifestKind::Dir => true,
            ManifestKind::File => false,
            ManifestKind::Symlink => match manifest.followed_kind_of(rel) {
                Some(ManifestKind::Dir) => true,
                Some(ManifestKind::File) | Some(ManifestKind::Symlink) => false,
                None if source_has_snapshot_handle(source) => false,
                // Legacy manifests do not persist the followed kind.
                None => root.join(rel).is_dir(),
            },
        };
    }
    if source_has_snapshot_handle(source) {
        return false;
    }
    merge_is_dir(root, rel)
}

/// `root/rel` 直下の子エントリ名集合。manifest があればそれを使い、無ければ `read_dir` に
/// fallback する（Phase 2 Part B）。`read_dir_children` を置き換える。
fn merge_children(root: &Path, rel: &Path) -> HashSet<OsString> {
    read_dir_children(&root.join(rel))
}

fn source_children(source: &FileSource, rel: &Path) -> HashSet<OsString> {
    if let Some(manifest) = source_manifest(source) {
        return manifest.child_names(rel).unwrap_or_default();
    }
    snapshot_root_of(source)
        .map(|root| merge_children(root, rel))
        .unwrap_or_default()
}

/// 同 path のエントリを 2a-2c で再帰的に競合判定し、全てマージ可能なら true。
fn dirs_mergeable(
    files: &BTreeMap<PathBuf, FileItem>,
    rfiles: &BTreeMap<PathBuf, FileItem>,
) -> bool {
    let (small, large) = if files.len() <= rfiles.len() {
        (files, rfiles)
    } else {
        (rfiles, files)
    };
    small.iter().all(|(path, item)| {
        let Some(other) = large.get(path) else {
            return true;
        };
        entries_mergeable(path, item, other)
    })
}

/// path 配下の X(item)・Y(other) がマージ可能か（2a-2c 再帰）。
///
/// - 2a: X/Y がディレクトリかファイルか（`root.join(path).is_dir()`）。FileSource::File
///   （GeneratedFile 等）は両方ファイル扱い。
/// - 2b: 種別違い（ディレクトリ vs ファイル）は競合。両方ファイルは merge_type で判定。
/// - 2c: 両方ディレクトリなら直下の子要素を取得し、共通の子で 2a-2c を再帰。
fn entries_mergeable(path: &Path, item: &FileItem, other: &FileItem) -> bool {
    let (Some(_x_root), Some(_y_root)) = (
        snapshot_root_of(&item.source),
        snapshot_root_of(&other.source),
    ) else {
        // FileSource::File 同士（GeneratedFile 等）は merge_type で判定。
        return !matches!(
            (&item.merge_type, &other.merge_type),
            (MergeType::Conflict, _) | (_, MergeType::Conflict)
        );
    };
    let x_dir = source_is_dir(&item.source, path);
    let y_dir = source_is_dir(&other.source, path);
    // 2b: 種別違い（ディレクトリ vs ファイル）は競合。
    if x_dir != y_dir {
        return false;
    }
    // 両方ファイル: 従来の merge_type 判定。
    if !x_dir {
        return !matches!(
            (&item.merge_type, &other.merge_type),
            (MergeType::Conflict, _) | (_, MergeType::Conflict)
        );
    }
    // 2c: 両方ディレクトリ。直下の子要素で再帰。
    let x_children = source_children(&item.source, path);
    if x_children.is_empty() {
        return true;
    }
    let y_children = source_children(&other.source, path);
    for child in x_children {
        if y_children.contains(&child) && !entries_mergeable(&path.join(&child), item, other) {
            return false;
        }
    }
    true
}

/// FileSource から snapshot_root（絶対パス）を取得。FileSource::File は None。
fn snapshot_root_of(source: &FileSource) -> Option<&Path> {
    match source {
        FileSource::Directory { path, handle, .. } => handle
            .as_ref()
            .map(|handle| handle.root.as_ref())
            .or(Some(path.as_ref())),
        FileSource::File { .. } => None,
    }
}

fn source_has_snapshot_handle(source: &FileSource) -> bool {
    matches!(
        source,
        FileSource::Directory {
            handle: Some(_),
            ..
        }
    )
}

/// ディレクトリ直下の子エントリ名を取得（merge の子要素再帰用）。
fn read_dir_children(dir: &Path) -> HashSet<OsString> {
    let mut set = HashSet::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            set.insert(entry.file_name());
        }
    }
    set
}

/// `files` と `rfiles` を union する。同 path の sealed-dir 衝突は1段展開して
/// 子 key に置換する（Phase 6a で入った `extend` 上書きによる片側消失を防ぐ）。
/// `dirs_mergeable(files, rfiles)` が true（全衝突が解決可能）の前提で呼ぶ。
/// 3+ プラグインのマージで sealed `X` と展開済み `X/子` が混在する（非推移的）のを防ぐため、
/// union 後に `normalize_sealed` で推移的正規化する（Phase 8）。
fn union_files(
    mut files: BTreeMap<PathBuf, FileItem>,
    rfiles: BTreeMap<PathBuf, FileItem>,
) -> BTreeMap<PathBuf, FileItem> {
    for (path, ritem) in rfiles {
        match files.remove(&path) {
            None => {
                files.insert(path, ritem);
            }
            Some(litem) => {
                if litem.kind() == FileKind::Directory && ritem.kind() == FileKind::Directory {
                    // 両者 sealed-dir: 1段展開して子 key で union。
                    expand_dir_union(&mut files, &path, litem, ritem);
                } else {
                    // file/file（predicate 済みで両者 Overwrite）: rhs 勝ち（旧 extend 互換）。
                    files.insert(path, ritem);
                }
            }
        }
    }
    normalize_sealed(&mut files);
    files
}

/// sealed-dir `X` が同一 map 内に子孫 `X/...` を持つ場合、その sealed `X` を展開して混在を解消する。
/// 3+ プラグインのマージで「sealed X」と「展開済み X/子」が混在する（Phase 6c の非推移性）のを
/// 正規化し、install copy での EEXIST を根本回避する（Phase 8）。子孫を持つ sealed のみ展開し、
/// nesting はループで処理。展開判定は IO 無し（BTreeMap range）、`read_dir` は展開時のみ。
fn normalize_sealed(files: &mut BTreeMap<PathBuf, FileItem>) {
    loop {
        // 子孫を持つ sealed-dir を1つ見つける（無ければ正規化完了）。
        let Some(sealed_path) = files
            .iter()
            .filter(|(_, v)| v.kind() == FileKind::Directory)
            .find(|(k, _)| has_descendant(files, k))
            .map(|(k, _)| k.clone())
        else {
            return;
        };
        let Some(sealed) = files.remove(&sealed_path) else {
            return;
        };
        expand_sealed_into(files, &sealed_path, sealed);
    }
}

/// `files` 内に `path` の子孫（`path/...`）が存在するか。IO 無し・BTreeMap range で O(log n)。
fn has_descendant(files: &BTreeMap<PathBuf, FileItem>, path: &Path) -> bool {
    use std::ops::Bound;
    files
        .range::<Path, _>((Bound::Excluded(path), Bound::Unbounded))
        .next()
        .map(|(k, _)| k.starts_with(path))
        .unwrap_or(false)
}

/// sealed-dir `sealed`（`path` にある）の子を `files` に union する。既存の子孫と衝突すれば
/// sealed 同士は `expand_dir_union`、file/file は rhs 勝ち。sealed `path` 自体は置かない。
fn expand_sealed_into(files: &mut BTreeMap<PathBuf, FileItem>, path: &Path, sealed: FileItem) {
    let Some(_root) = snapshot_root_of(&sealed.source) else {
        return;
    };
    let children = source_children(&sealed.source, path);
    for child in &children {
        let cpath = path.join(child);
        let citem = child_item(&sealed, cpath.clone());
        match files.remove(&cpath) {
            None => {
                files.insert(cpath, citem);
            }
            Some(existing) => {
                if existing.kind() == FileKind::Directory && citem.kind() == FileKind::Directory {
                    expand_dir_union(files, &cpath, existing, citem);
                } else {
                    files.insert(cpath, citem); // file/file: rhs 勝ち
                }
            }
        }
    }
}

/// 同 path の sealed-dir 同士（`litem`, `ritem`）を1段展開し、子 key を `out` に union する。
/// 共通の子が両方 directory なら更に1段展開（再帰）、file/file なら rhs 勝ち。
fn expand_dir_union(
    out: &mut BTreeMap<PathBuf, FileItem>,
    path: &Path,
    litem: FileItem,
    ritem: FileItem,
) {
    let _lroot = snapshot_root_of(&litem.source).expect("directory item has Directory source");
    let _rroot = snapshot_root_of(&ritem.source).expect("directory item has Directory source");
    let lchildren = source_children(&litem.source, path);
    let rchildren = source_children(&ritem.source, path);
    for child in &lchildren {
        let cpath = path.join(child);
        let lc = child_item(&litem, cpath.clone());
        if rchildren.contains(child) {
            let rc = child_item(&ritem, cpath.clone());
            if lc.kind() == FileKind::Directory && rc.kind() == FileKind::Directory {
                expand_dir_union(out, &cpath, lc, rc);
            } else {
                out.insert(cpath, rc);
            }
        } else {
            out.insert(cpath, lc);
        }
    }
    for child in &rchildren {
        if !lchildren.contains(child) {
            let cpath = path.join(child);
            let rc = child_item(&ritem, cpath.clone());
            out.insert(cpath, rc);
        }
    }
}

/// `parent`（sealed-dir エントリ）の直下の子 `child_path` に対応する FileItem を作る。
/// source は親と同じ snapshot root、identity は RepoFile で子パスを束ね直す。
fn child_item(parent: &FileItem, child_path: PathBuf) -> FileItem {
    let identity = match &parent.identity {
        FileIdentity::RepoFile(r) => {
            FileIdentity::RepoFile(RepoFileIdentity::new(r.snapshot.clone(), child_path))
        }
        // sealed-dir は Directory source（RepoFile）なので GeneratedFile には到達しない。
        FileIdentity::GeneratedFile { .. } => parent.identity.clone(),
    };
    FileItem::new(parent.source.clone(), identity, parent.merge_type)
}

/// ファイルの取得(生成)元。
#[derive(Debug)]
pub(super) enum FileSource {
    Directory {
        path: Arc<Path>,
        /// Validated immutable inventory carried with the snapshot. `None`
        /// is retained for hand-built test values and legacy callers.
        inventory: Option<Arc<SnapshotManifest>>,
        /// Production callers provide the complete immutable snapshot handle;
        /// `None` is retained only for legacy hand-built test fixtures.
        handle: Option<Arc<SnapshotHandle>>,
    },
    File {
        data: Cow<'static, [u8]>,
    },
}

impl PartialEq for FileSource {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Directory { path: l, .. }, Self::Directory { path: r, .. }) => l == r,
            (Self::File { data: l }, Self::File { data: r }) => l == r,
            _ => false,
        }
    }
}

impl Eq for FileSource {}

impl Hash for FileSource {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            // 絶対パスはマシン固有なのでハッシュに含めない。
            // 同一性は FileItem.identity (RepoSnapshotIdentity 等) が担保する。
            FileSource::Directory { .. } => 0u8.hash(state),
            FileSource::File { data } => {
                1u8.hash(state);
                data.hash(state);
            }
        }
    }
}

impl FileSource {
    /// `whichfile`（install_dir からの相対パス）にデータを配置する。
    /// Directory source はファイルシステム上の実際の種別（ディレクトリ・ファイル・symlink）に
    /// 応じて `place_path` に配置を一任し、File source はデータを書き出す。
    async fn yank(
        &self,
        whichfile: impl AsRef<Path>,
        install_dir: impl AsRef<Path>,
    ) -> io::Result<()> {
        crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::PackageCopy);
        match self {
            FileSource::Directory { path, .. } => {
                let src = path.join(&whichfile);
                let dst = install_dir.as_ref().join(&whichfile);
                place_path(&src, &dst).await
            }
            FileSource::File { data } => {
                let dst = install_dir.as_ref().join(whichfile);
                if let Some(parent) = dst.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(dst, data).await?;
                Ok(())
            }
        }
    }
}

struct Files {
    is_lazy_registration: bool,
    /// 配置エントリ（ファイル・sealed-dir 不分別）。install で各 `source.yank` に任せる。
    entries: Vec<(PathBuf, Arc<FileSource>)>,
    /// dotgit=true かつ repo 由来なら install 時に snapshot の .git を pack に copy する。
    dotgit: bool,
}

#[cfg(unix)]
fn os_string_to_install_key(name: OsString) -> Box<[u8]> {
    use std::os::unix::ffi::OsStringExt;

    name.into_vec().into_boxed_slice()
}

#[cfg(windows)]
fn os_string_to_install_key(name: OsString) -> Box<[u8]> {
    // PluginIDStr directory names are generated from lowercase hexadecimal
    // ASCII. Windows stores OsString as WTF-8/UTF-16 internally, but converting
    // the directory name back through UTF-8 is lossless for these IDs and avoids
    // Unix-only OsStringExt::into_vec().
    name.to_string_lossy()
        .into_owned()
        .into_bytes()
        .into_boxed_slice()
}

#[cfg(unix)]
async fn symlink_file(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    tokio::fs::symlink(original, link).await
}

#[cfg(windows)]
async fn symlink_file(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    tokio::fs::symlink_file(original, link).await
}

/// staging ディレクトリ名の衝突を避ける単調カウンタ（PID と組み合わせる）。
static STAGING_NONCE: AtomicU64 = AtomicU64::new(0);

/// 新世代を構築する一時ディレクトリ。
/// `pack/_gen/.staging-<control_id>-<pid>-<nonce>/` 配下にパッケージを置き、
/// 公開（`opt/` への rename）されるまで Neovim からは参照されない。失敗時は丸ごと破棄できる。
fn staging_root(gen_root: &Path, control_id: &str) -> PathBuf {
    let nonce = STAGING_NONCE.fetch_add(1, AtomicOrdering::Relaxed);
    gen_root.join(format!(
        ".staging-{}-{}-{}",
        control_id,
        std::process::id(),
        nonce
    ))
}

struct StagingGuard(PathBuf);

impl Drop for StagingGuard {
    fn drop(&mut self) {
        // This is intentionally scoped to the staging root owned by this run.
        // Cleanup is best-effort because the process may be exiting after an error.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 前回クラッシュ等で残った `.staging-*` を best-effort で削除する。
/// staging は init.lua / manifest のいずれからも参照されないため、任何時点で安全に消せる。
async fn cleanup_stale_staging(gen_root: &Path) {
    const STAGING_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);
    let Ok(mut rd) = tokio::fs::read_dir(gen_root).await else {
        return;
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        if let Some(name) = entry.file_name().to_str()
            && name.starts_with(".staging-")
        {
            let lease = entry.path().join(".lease");
            let owner_status = if let Ok(contents) = tokio::fs::read_to_string(&lease).await
                && let Ok(pid) = contents.trim().parse::<i32>()
            {
                #[cfg(unix)]
                {
                    // A live owner keeps its private staging tree. A stale owner
                    // (including a crashed process) may be reclaimed next run.
                    Some(unsafe { libc::kill(pid, 0) == 0 })
                }
                #[cfg(not(unix))]
                {
                    None
                }
            } else {
                None
            };
            if owner_status == Some(true) {
                continue;
            }
            if owner_status == Some(false) {
                let _ = tokio::fs::remove_dir_all(entry.path()).await;
                continue;
            }
            // A missing or unreadable lease is not proof that a process is
            // gone. Reclaim it only after the documented age threshold.
            let old_enough = tokio::fs::metadata(entry.path())
                .await
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age >= STAGING_MAX_AGE);
            if !old_enough {
                continue;
            }
            let _ = tokio::fs::remove_dir_all(entry.path()).await;
        }
    }
}

/// 並行 rsplug 実行による publish 競合を直列化するための排他ロック（Unix）。
/// `pack/_gen/.lock` に対する flock(LOCK_EX)。返した File が drop すると解放される。
#[cfg(unix)]
struct InstallLock {
    _file: std::fs::File,
    acquired_at: std::time::Instant,
}

#[cfg(unix)]
impl Drop for InstallLock {
    fn drop(&mut self) {
        crate::rsplug::perf::incr_duration_micros(
            crate::rsplug::perf::PerfOp::PublicationLockHoldMicros,
            self.acquired_at
                .elapsed()
                .as_micros()
                .min(u128::from(u64::MAX)) as u64,
        );
    }
}

#[cfg(unix)]
async fn acquire_install_lock(gen_root: &Path) -> io::Result<InstallLock> {
    use std::os::fd::AsRawFd;
    let lock_path = gen_root.join(".lock");
    let started_at = std::time::Instant::now();
    tokio::task::spawn_blocking(move || -> io::Result<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(file)
    })
    .await
    .map_err(|e| io::Error::other(format!("install lock join failed: {e}")))?
    .map(|file| {
        crate::rsplug::perf::incr_duration_micros(
            crate::rsplug::perf::PerfOp::PublicationLockWaitMicros,
            started_at.elapsed().as_micros().min(u128::from(u64::MAX)) as u64,
        );
        InstallLock {
            _file: file,
            acquired_at: std::time::Instant::now(),
        }
    })
}

const RETAIN_GENERATIONS: usize = 3;
const GENERATION_REGISTRY_FILE: &str = "registry.json";

/// 生成 manifest（R1: v2）。`runtime` は `#[serde(default)]` により、保持されている
/// v1 manifest（`runtime` 無し）もそのままデシリアライズできる。
#[derive(Serialize, Deserialize, Default, PartialEq, Eq)]
struct GenerationManifest {
    version: u8,
    entries: Vec<String>,
    #[serde(default)]
    generation_id: String,
    #[serde(default)]
    plan: Option<GenerationPlan>,
    #[serde(default)]
    runtime: RuntimeManifest,
}

#[derive(Serialize, Deserialize, Default)]
struct GenerationRegistry {
    generations: Vec<String>,
}

async fn read_generation_registry(gen_root: &Path) -> Vec<String> {
    let path = gen_root.join("generations").join(GENERATION_REGISTRY_FILE);
    let Ok(content) = tokio::fs::read(&path).await else {
        return Vec::new();
    };
    serde_json::from_slice::<GenerationRegistry>(&content)
        .ok()
        .map(|registry| registry.generations)
        .unwrap_or_default()
}

async fn write_generation_registry(gen_root: &Path, current: &str) -> io::Result<()> {
    let mut generations = read_generation_registry(gen_root).await;
    generations.retain(|name| name != current);
    generations.insert(0, current.to_string());
    generations.truncate(RETAIN_GENERATIONS);
    let registry =
        serde_json::to_vec_pretty(&GenerationRegistry { generations }).map_err(io::Error::other)?;
    let path = gen_root.join("generations").join(GENERATION_REGISTRY_FILE);
    let tmp = path.with_extension(format!(
        "json.tmp-{}",
        STAGING_NONCE.fetch_add(1, AtomicOrdering::Relaxed)
    ));
    crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::GenerationRegistryWrite);
    tokio::fs::write(&tmp, registry).await?;
    tokio::fs::rename(tmp, path).await
}

/// v2 manifest のランタイム側インデックス。現在は ftplugin のみ。
/// `ftplugin`: `ft -> id -> [opt/<id>/ftplugin/...]`（generation root 相対パス）。
#[derive(Clone, Debug, Hash, Serialize, Deserialize, Default, PartialEq, Eq)]
struct RuntimeManifest {
    #[serde(default)]
    ftplugin: BTreeMap<String, BTreeMap<String, Vec<String>>>,
}

/// Pure, deterministic input to generation publication. Filesystem paths are
/// generation-root-relative and all ordered collections use stable ordering.
#[derive(Clone, Debug, Hash, Serialize, Deserialize, PartialEq, Eq)]
struct GenerationPlan {
    schema: u8,
    merge_abi: u8,
    entries: Vec<String>,
    control_ids: Vec<String>,
    runtime: RuntimeManifest,
}

impl GenerationPlan {
    const SCHEMA: u8 = 1;
    const MERGE_ABI: u8 = 1;

    fn new(entries: Vec<String>, control_ids: &[PluginIDStr], runtime: RuntimeManifest) -> Self {
        Self {
            schema: Self::SCHEMA,
            merge_abi: Self::MERGE_ABI,
            entries,
            control_ids: control_ids.iter().map(ToString::to_string).collect(),
            runtime,
        }
    }

    fn id(&self) -> String {
        crate::rsplug::util::hash::digest_hash_hex_string(self)
    }
}

#[derive(TemplateSimple)]
#[template(path = "init.stpl")]
#[template(escape = false)]
struct InitTemplate<'a> {
    control_ids: &'a [PluginIDStr],
}

fn render_init(control_ids: &[PluginIDStr]) -> Vec<u8> {
    InitTemplate { control_ids }
        .render_once()
        .map(String::into_bytes)
        .unwrap_or_else(|_| Vec::new())
}

/// `true` は生成物を公開し、`false` は既存の完全な generation を再利用した。
pub type GenerationPublished = bool;

/// 既存 generation が今回の計画と完全に一致し、ローダーも正常なら再公開を省略する。
///
/// S1 前のため ft index の読み取り自体は必要だが、コピー・metadata/loader/init の書き込みと
/// retention/GC を避けられる。欠損や破損はすべて `false` に倒し通常 publish で修復する。
async fn generation_is_current(
    gen_root: &Path,
    control_ids: &[PluginIDStr],
    generation_entries: &[String],
    init_content: &[u8],
    packpath: &Path,
    desired_plan: Option<&GenerationPlan>,
) -> bool {
    for entry in generation_entries {
        let Ok(metadata) = tokio::fs::symlink_metadata(gen_root.join(entry)).await else {
            return false;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return false;
        }
    }
    // A script-only/empty plan has no control package or generation manifest;
    // its durable pointer is the plain root init.lua written by the publisher.
    if control_ids.is_empty() {
        return tokio::fs::read(packpath.join("init.lua"))
            .await
            .is_ok_and(|content| content == init_content)
            && tokio::fs::symlink_metadata(packpath.join("init.lua"))
                .await
                .is_ok_and(|metadata| !metadata.file_type().is_symlink());
    }
    let control_id = &control_ids[0];
    let generation_name = desired_plan
        .map(GenerationPlan::id)
        .unwrap_or_else(|| control_id.to_string());
    let manifest_path = gen_root
        .join("generations")
        .join(&generation_name)
        .with_extension("json");
    let Ok(content) = tokio::fs::read(&manifest_path).await else {
        return false;
    };
    let Ok(current) = serde_json::from_slice::<GenerationManifest>(&content) else {
        return false;
    };
    if current.version != 2 || current.entries != generation_entries {
        return false;
    }
    if let Some(plan) = desired_plan
        && (current.plan.as_ref() != Some(plan) || current.generation_id != plan.id())
    {
        return false;
    }
    let loader = packpath
        .join("generations")
        .join(&generation_name)
        .with_extension("lua");
    let Ok(loader_content) = tokio::fs::read(loader).await else {
        return false;
    };
    if loader_content != init_content {
        return false;
    }
    tokio::fs::symlink_metadata(packpath.join("init.lua"))
        .await
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
}

// ---- pack copy 戦略（Phase 6c） ----
// 配置入口を `FileSource::yank` → `place_path` → `copy_tree`/`copy_file_with_strategy`
// に統一し、実行時に reflink → copy へ単調昇格する。ハードリンクは
// スナップショット編集が公開 pack を変更するため、公開物には使わない。

/// 実行時 copy 戦略。失敗に応じて単調に昇格する。
/// `0` = reflink（macOS `clonefile` / Linux `FICLONE`）
/// `1` = hardlink（互換用に残すが、公開コピー戦略では選択しない）
/// `2` = copy（内容複製）
static COPY_STRATEGY: AtomicU8 = AtomicU8::new(INITIAL_COPY_STRATEGY);

#[cfg(any(target_os = "macos", target_os = "linux"))]
const INITIAL_COPY_STRATEGY: u8 = STRATEGY_REFLINK;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const INITIAL_COPY_STRATEGY: u8 = STRATEGY_COPY;

const STRATEGY_REFLINK: u8 = 0;
const STRATEGY_HARDLINK: u8 = 1;
const STRATEGY_COPY: u8 = 2;

/// 別 filesystem を跨ぐ errno（reflink も hardlink も不可）。
const EXDEV: i32 = libc::EXDEV;

fn copy_strategy() -> u8 {
    COPY_STRATEGY.load(AtomicOrdering::Relaxed)
}

/// 失敗に応じて戦略を昇格（単調）。ハードリンクは内容共有のため経由しない。
fn advance_strategy(_error: &io::Error) {
    COPY_STRATEGY.fetch_max(STRATEGY_COPY, AtomicOrdering::AcqRel);
}

/// reflink が「この環境では使えない」エラーか（内容複製へフォールバック）。
#[cfg(target_os = "macos")]
fn reflink_unsupported(e: &io::Error) -> bool {
    // EXDEV は別経路で copy まで jump するのでここでは除外。
    matches!(
        e.raw_os_error(),
        Some(libc::ENOTSUP) | Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP)
    )
}

#[cfg(target_os = "linux")]
fn reflink_unsupported(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENOTTY) | Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP)
    )
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn reflink_unsupported(_e: &io::Error) -> bool {
    // reflink 非対応プラットフォーム: 常に別方式へフォールバック。
    true
}

/// reflink 試行の失敗が「別方式へフォールバック」すべきものか。
fn reflink_should_fallback(e: &io::Error) -> bool {
    reflink_unsupported(e) || e.raw_os_error() == Some(EXDEV)
}

/// 1ファイルを reflink で CoW clone する。対応環境でのみ成功。
/// dst は未存在・親は存在が前提（実装が dst を新規作成する）。
#[cfg(target_os = "macos")]
async fn reflink_file(src: &Path, dst: &Path) -> io::Result<()> {
    clonefile(src, dst).await
}

#[cfg(target_os = "linux")]
async fn reflink_file(src: &Path, dst: &Path) -> io::Result<()> {
    ficlone_file(src, dst).await
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
async fn reflink_file(_src: &Path, _dst: &Path) -> io::Result<()> {
    // reflink 非対応: 内容複製へフォールバック。
    Err(io::Error::from_raw_os_error(38)) // ENOSYS
}

/// 1ファイルを現在の戦略で配置。未対応/`EXDev` エラーで戦略を昇格して再試行する。
async fn copy_file_with_strategy(src: &Path, dst: &Path) -> io::Result<()> {
    loop {
        match copy_strategy() {
            s if s == STRATEGY_REFLINK => match reflink_file(src, dst).await {
                Ok(()) => {
                    crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::ReflinkCopy);
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    // dst 既存在（マージで同名ファイルが複数 plugin 由来等）。copy で上書き。
                    // 戦略は変更しない（AlreadyExists は環境起因ではない）。
                    let bytes = tokio::fs::copy(src, dst).await?;
                    crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::FileCopied);
                    crate::rsplug::perf::incr_bytes(bytes);
                    return Ok(());
                }
                Err(e) if reflink_should_fallback(&e) => {
                    crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::FallbackFanout);
                    advance_strategy(&e);
                    continue;
                }
                Err(e) => return Err(e),
            },
            STRATEGY_HARDLINK => match tokio::fs::hard_link(src, dst).await {
                Ok(()) => {
                    crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::HardlinkCopy);
                    return Ok(());
                }
                Err(e) if e.raw_os_error() == Some(EXDEV) => {
                    crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::FallbackFanout);
                    advance_strategy(&e);
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let bytes = tokio::fs::copy(src, dst).await?;
                    crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::FileCopied);
                    crate::rsplug::perf::incr_bytes(bytes);
                    return Ok(());
                }
                Err(e) => return Err(e),
            },
            _ => {
                crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::PlainCopy);
                let bytes = tokio::fs::copy(src, dst).await?;
                crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::FileCopied);
                crate::rsplug::perf::incr_bytes(bytes);
                return Ok(());
            }
        }
    }
}

/// `src`（file/dir/symlink）を `dst` に配置する。ディレクトリは `copy_tree`、それ以外は `copy_leaf`。
async fn place_path(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = tokio::fs::symlink_metadata(src).await?;
    if meta.is_dir() {
        copy_tree(src, dst).await
    } else {
        copy_leaf(src, dst).await
    }
}

/// leaf（ファイル/symlink）を `dst` に配置する。ディレクトリは扱わない（呼出元が mkdir 済み）。
async fn copy_leaf(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = tokio::fs::symlink_metadata(src).await?;
    if meta.is_symlink() {
        let target = tokio::fs::read_link(src).await?;
        if let Some(parent) = dst.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        #[cfg(unix)]
        {
            if let Err(e) = tokio::fs::symlink(&target, dst).await {
                if e.kind() != io::ErrorKind::AlreadyExists {
                    return Err(e);
                }
                // dst 既存在（マージ衝突等）。除去してから再作成（ファイル/dir 混在に備え両方試す）。
                let _ = tokio::fs::remove_file(dst).await;
                let _ = tokio::fs::remove_dir_all(dst).await;
                tokio::fs::symlink(&target, dst).await?;
            }
        }
        // Windows は symlink 作成に権限が必要なため実体 copy にフォールバック。
        #[cfg(not(unix))]
        {
            tokio::fs::copy(src, dst).await?;
        }
        Ok(())
    } else {
        if let Some(parent) = dst.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        copy_file_with_strategy(src, dst).await
    }
}

/// ディレクトリを copy。macOS かつ reflink 戦略なら `clonefile(2)` でディレクトリ全体を
/// 1 syscall・CoW で clone し、失敗時は再帰 per-file copy にフォールバックする
/// （CoW かつ独立 inode なので元 snapshot を編集しても pack に影響しない）。
/// フォールバック時はスタックでディレクトリを walk して leaf のみ `JoinSet` で並列 copy する
/// （`copy_leaf` は非再帰なので、再帰的 future 型による Send 推論の破綻を避ける）。
async fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    if copy_strategy() == STRATEGY_REFLINK {
        // clonefile は dst を新規作成するので親だけ作る。
        if let Some(parent) = dst.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        match clonefile(src, dst).await {
            Ok(()) => return Ok(()),
            Err(e) if reflink_should_fallback(&e) => {
                advance_strategy(&e);
                // フォールバック: dst は未作成のまま walk へ。
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // dst 既存在（マージで sealed-dir と展開済み子エントリが同一 pack に混在等）。
                // 内容を再帰 merge する walk へフォールバック（戦略は変更しない: 環境起因ではない）。
            }
            Err(e) => return Err(e),
        }
    }
    tokio::fs::create_dir_all(dst).await?;
    // Keep traversal memory bounded.  A semaphore alone limits active copies,
    // but spawning one task per leaf still retains the whole snapshot in the
    // executor queue.  The channel is deliberately small so walking and
    // copying provide backpressure to one another.
    const COPY_QUEUE: usize = COPY_WORKERS * 2;
    const COPY_WORKERS: usize = 16;
    let (tx, rx) = tokio::sync::mpsc::channel::<(PathBuf, PathBuf)>(COPY_QUEUE);
    let mut workers = JoinSet::new();
    let shared_rx = std::sync::Arc::new(tokio::sync::Mutex::new(rx));
    for _ in 0..COPY_WORKERS {
        crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::SpawnedWorker);
        let worker_rx = shared_rx.clone();
        workers.spawn(async move {
            loop {
                let item = {
                    let mut rx = worker_rx.lock().await;
                    rx.recv().await
                };
                let Some((src, dst)) = item else { break };
                let permit = crate::rsplug::util::resources::COPY_LEAF
                    .acquire()
                    .await
                    .map_err(|e| io::Error::other(format!("copy leaf semaphore closed: {e}")))?;
                let result = copy_leaf(&src, &dst).await;
                drop(permit);
                result?;
            }
            Ok::<(), io::Error>(())
        });
    }
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((s, d)) = stack.pop() {
        let meta = tokio::fs::symlink_metadata(&s).await?;
        if meta.is_dir() {
            tokio::fs::create_dir_all(&d).await?;
            let mut entries = tokio::fs::read_dir(&s).await?;
            while let Some(entry) = entries.next_entry().await? {
                let name = entry.file_name();
                stack.push((s.join(&name), d.join(&name)));
            }
        } else {
            if tx.send((s, d)).await.is_err() {
                workers.abort_all();
                return Err(io::Error::other("copy workers stopped"));
            }
        }
    }
    drop(tx);
    while let Some(res) = workers.join_next().await {
        res.map_err(|e| io::Error::other(format!("copy join failed: {e}")))??;
    }
    Ok(())
}

/// Generate all staged helptags with one Neovim process. Package copying is
/// already complete, so the command can issue one `helptags` invocation per
/// doc directory while keeping process creation bounded to one per generation.
async fn run_staged_helptags(staging: &Path) -> io::Result<()> {
    let mut help_dirs = Vec::new();
    let opt = staging.join("opt");
    let Ok(mut packages) = tokio::fs::read_dir(opt).await else {
        return Ok(());
    };
    while let Some(package) = packages.next_entry().await? {
        let help_dir = package.path().join("doc");
        if tokio::fs::metadata(&help_dir)
            .await
            .is_ok_and(|metadata| metadata.is_dir())
        {
            help_dirs.push(help_dir);
        }
    }
    help_dirs.sort();
    if help_dirs.is_empty() {
        return Ok(());
    }

    let mut nvim = tokio::process::Command::new("nvim");
    nvim.arg("--headless")
        .arg("-u")
        .arg("NONE")
        .arg("-i")
        .arg("NONE")
        .arg("-n");
    for help_dir in &help_dirs {
        msg(Message::InstallHelp {
            help_dir: help_dir.clone(),
        });
        let escaped = help_dir
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace(' ', "\\ ")
            .replace('|', "\\|");
        nvim.arg("-c").arg(format!("helptags {escaped}"));
    }
    crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::HelptagsProcess);
    nvim.arg("-c").arg("q").status().await.and_then(|code| {
        if code.success() {
            Ok(())
        } else {
            Err(io::Error::other("Failed to generate staged helptags"))
        }
    })
}

// ---- プラットフォーム固有 reflink 実装 ----

/// macOS APFS の `clonefile(2)` で file/dir を1 syscall・CoW で clone する。
/// 非 APFS・別 volume・カーネル未対応では失敗し、呼び出し元が再帰 copy にフォールバックする。
/// dst は未存在・親は存在が前提（`clonefile` が dst を新規作成する）。
#[cfg(target_os = "macos")]
async fn clonefile(src: &Path, dst: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let src = src.to_path_buf();
    let dst = dst.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let s = CString::new(src.as_os_str().as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let d = CString::new(dst.as_os_str().as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        // SAFETY: `s`/`d` は有効な NUL 終端パス。`flags=0` はデフォルト挙動（CoW clone）。
        let ret = unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
    .await
    .map_err(|e| io::Error::other(format!("clonefile join failed: {e}")))?
}

/// Linux の `ioctl(FICLONE)` で1ファイルを CoW clone（reflink）する。
/// btrfs/xfs 等 reflink 対応 FS でのみ成功。dst は未存在・親は存在が前提。
#[cfg(target_os = "linux")]
async fn ficlone_file(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let src = src.to_path_buf();
    let dst = dst.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let src_f = std::fs::File::open(&src)?;
        let dst_f = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&dst)?;
        // SAFETY: FICLONE ioctl に src fd を渡し dst に reflink させる。3 引数固定呼出。
        let ret = unsafe { libc::ioctl(dst_f.as_raw_fd(), libc::FICLONE, src_f.as_raw_fd()) };
        if ret < 0 {
            let e = io::Error::last_os_error();
            let _ = std::fs::remove_file(&dst); // 部分作成した空 dst を掃除
            Err(e)
        } else {
            Ok(())
        }
    })
    .await
    .map_err(|e| io::Error::other(format!("ficlone join failed: {e}")))?
}

fn manifest_entries(manifest: &GenerationManifest) -> HashSet<Box<[u8]>> {
    manifest
        .entries
        .iter()
        .map(|entry| entry.as_bytes().to_vec().into_boxed_slice())
        .collect()
}

async fn retained_manifest_entries(
    gen_root: &Path,
    current_generation_names: &[String],
    current_manifest: &GenerationManifest,
) -> io::Result<HashSet<Box<[u8]>>> {
    let current_control_set: HashSet<Box<[u8]>> = current_generation_names
        .iter()
        .map(|id| id.as_bytes().to_vec().into_boxed_slice())
        .collect();
    let generations_root = gen_root.join("generations");
    let mut registry = read_generation_registry(gen_root).await;
    // Pre-registry installations have no ordering metadata. Reconstruct a
    // bounded, deterministic candidate list once during publication so an
    // upgrade does not immediately discard every older bootable generation.
    if registry.is_empty()
        && let Ok(mut entries) = tokio::fs::read_dir(&generations_root).await
    {
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Some(name) = path.file_stem().and_then(|name| name.to_str()) {
                registry.push(name.to_string());
            }
        }
        registry.sort();
    }

    let mut retained_entries = manifest_entries(current_manifest);
    let mut retained_count = current_generation_names.len();
    for generation_name in registry {
        if current_control_set.contains(generation_name.as_bytes()) {
            continue;
        }
        if retained_count >= RETAIN_GENERATIONS {
            break;
        }
        if generation_name.len() != 32
            || !generation_name.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            continue;
        }
        let path = generations_root
            .join(&generation_name)
            .with_extension("json");
        if let Ok(content) = tokio::fs::read(&path).await
            && let Ok(manifest) = serde_json::from_slice::<GenerationManifest>(&content)
        {
            crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::RetentionManifestRead);
            retained_entries.extend(manifest_entries(&manifest));
            retained_count += 1;
        }
    }
    Ok(retained_entries)
}

/// Fast publication-side check for whether cleanup can have any work. It only
/// inspects package-directory names and the already loaded retained index; the
/// expensive bounded leaf cleanup is skipped when every published package is
/// reachable.
async fn has_unreachable_packages(
    gen_root: &Path,
    retained_entries: &HashSet<Box<[u8]>>,
) -> io::Result<bool> {
    for root_name in ["start", "opt"] {
        let root = gen_root.join(root_name);
        let Ok(mut entries) = tokio::fs::read_dir(root).await else {
            continue;
        };
        while let Some(entry) = entries.next_entry().await? {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let prefix = format!("{root_name}/{name}");
            if !retained_entries.iter().any(|retained| {
                retained.as_ref() == prefix.as_bytes()
                    || retained.starts_with(format!("{prefix}/").as_bytes())
            }) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// R1: 公開済み `gen_root/opt/<id>/ftplugin/` を走査し、登録済み `(ft, id)` 対について
/// ftplugin ファイル一覧（`opt/<id>/ftplugin/...` = generation root 相対）を構築する。
/// 戻り値は `ft -> id -> [path]`。走査順は Invariants の ft 順序（exact → suffix → subdir）。
#[cfg_attr(not(test), allow(dead_code))]
async fn build_ft_index(
    gen_root: &Path,
    ft_pairs: &BTreeMap<String, Vec<String>>,
) -> io::Result<BTreeMap<String, BTreeMap<String, Vec<String>>>> {
    build_ft_index_from_roots(&gen_root.join("opt"), None, ft_pairs).await
}

/// Compatibility index scan over private staging plus already-published
/// packages. Staging wins for a package being rebuilt; the fallback root holds
/// reused content-addressed packages. This remains outside the publication
/// lock and is needed only when inventories cannot prove symlink semantics.
async fn build_ft_index_from_roots(
    primary_opt: &Path,
    fallback_opt: Option<&Path>,
    ft_pairs: &BTreeMap<String, Vec<String>>,
) -> io::Result<BTreeMap<String, BTreeMap<String, Vec<String>>>> {
    crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::FtIndexScan);
    let mut out: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    for (ft, ids) in ft_pairs {
        for id in ids {
            let package = primary_opt.join(id);
            let package = if package.is_dir() {
                package
            } else if let Some(fallback_opt) = fallback_opt {
                fallback_opt.join(id)
            } else {
                package
            };
            let pkg_ftplugin = package.join("ftplugin");
            let paths = collect_ftplugin_files(&pkg_ftplugin, ft, id).await?;
            if paths.is_empty() {
                continue;
            }
            out.entry(ft.clone()).or_default().insert(id.clone(), paths);
        }
    }
    Ok(out)
}

/// Build the ftplugin index from the immutable inventories carried by the
/// package entries.  `None` means that at least one package cannot prove the
/// required symlink-follow semantics from its inventory, so the caller must
/// use the conservative filesystem fallback.
fn build_ft_index_from_inventories(
    files: &HashMap<PluginIDStr, Files>,
    ft_pairs: &BTreeMap<String, Vec<String>>,
) -> Option<BTreeMap<String, BTreeMap<String, Vec<String>>>> {
    let mut out: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    for (ft, ids) in ft_pairs {
        for id in ids {
            let package = files
                .iter()
                .find(|(package_id, _)| <PluginIDStr as AsRef<str>>::as_ref(package_id) == id)
                .map(|(_, package)| package)?;
            let mut paths = BTreeSet::<PathBuf>::new();
            for (output, source) in &package.entries {
                let FileSource::Directory {
                    inventory: Some(inventory),
                    ..
                } = source.as_ref()
                else {
                    return None;
                };
                // Merged entries retain the snapshot-relative output key;
                // child expansion also rewrites the key to that same path.
                let source_rel = output.as_path();
                let source_kind = inventory.kind_of(source_rel)?;
                if source_kind == ManifestKind::Symlink
                    && source_rel
                        .components()
                        .any(|component| component.as_os_str() == OsStr::new("ftplugin"))
                {
                    // A symlink can be a directory under ftplugin. The
                    // persisted inventory intentionally does not follow it,
                    // so preserve the old resolver's follow behavior.
                    return None;
                }

                for relative in inventory.ftplugin_files() {
                    let Ok(suffix) = relative.strip_prefix(source_rel) else {
                        continue;
                    };
                    let kind = inventory.kind_of(relative)?;
                    if kind != ManifestKind::File && kind != ManifestKind::Symlink {
                        continue;
                    }
                    let candidate = if suffix.as_os_str().is_empty() {
                        output.clone()
                    } else {
                        output.join(suffix)
                    };
                    if candidate.starts_with("ftplugin") {
                        paths.insert(candidate);
                    }
                }
            }

            let prefix = Path::new("ftplugin");
            let mut exact = Vec::new();
            for ext in ["vim", "lua"] {
                let path = prefix.join(format!("{ft}.{ext}"));
                if paths.contains(&path) {
                    exact.push(path);
                }
            }
            let mut suffix = paths
                .iter()
                .filter(|path| {
                    let Some(name) = path.strip_prefix(prefix).ok().and_then(Path::file_name)
                    else {
                        return false;
                    };
                    let name = name.to_string_lossy();
                    name.starts_with(&format!("{ft}_"))
                        && matches!(
                            path.extension().and_then(|ext| ext.to_str()),
                            Some("vim" | "lua")
                        )
                })
                .cloned()
                .collect::<Vec<_>>();
            suffix.sort();
            let mut subdir = paths
                .iter()
                .filter(|path| {
                    path.strip_prefix(prefix)
                        .ok()
                        .is_some_and(|rest| rest.starts_with(ft) && rest != Path::new(ft))
                        && matches!(
                            path.extension().and_then(|ext| ext.to_str()),
                            Some("vim" | "lua")
                        )
                })
                .cloned()
                .collect::<Vec<_>>();
            subdir.sort();
            exact.extend(suffix);
            exact.extend(subdir);
            if !exact.is_empty() {
                let full = exact
                    .into_iter()
                    .map(|path| format!("opt/{id}/{}", path.to_string_lossy()))
                    .collect::<Vec<_>>();
                out.entry(ft.clone()).or_default().insert(id.clone(), full);
            }
        }
    }
    Some(out)
}

/// 1つの `(ft, id)` について `pkg_ftplugin`（=`gen_root/opt/<id>/ftplugin`）配下の
/// ftplugin ファイルを収集する。3 グループ（exact / `<ft>_*` / `<ft>/`）をこの順で連結し
/// stable-dedup する。`ftplugin/` または `<ft>/` が無ければ該当グループは空。
/// ファイル・1段ディレクトリの symlink follow を許容し、それ以外の読み取りエラーは
/// 不完全な v2 を避けるため `Err` で公開を中断する。
async fn collect_ftplugin_files(
    pkg_ftplugin: &Path,
    ft: &str,
    id: &str,
) -> io::Result<Vec<String>> {
    let prefix = format!("opt/{id}/ftplugin");
    let mut paths: Vec<String> = Vec::new();

    // ftplugin/ が無ければ ft 依存ファイルは一切無い。
    if !is_dir_follow(pkg_ftplugin).await? {
        return Ok(paths);
    }

    // (1) exact: <ft>.vim, <ft>.lua （この順）
    for ext in ["vim", "lua"] {
        let name = format!("{ft}.{ext}");
        if is_file_follow(&pkg_ftplugin.join(&name)).await? {
            push_unique(&mut paths, &format!("{prefix}/{name}"));
        }
    }

    // (2) 直下の <ft>_*.vim|lua を相対パス昇順
    let mut suffix_names = read_dir_matching(pkg_ftplugin, ft, true).await?;
    suffix_names.sort();
    for name in &suffix_names {
        push_unique(&mut paths, &format!("{prefix}/{name}"));
    }

    // (3) <ft>/ ディレクトリ直下の *.vim|lua を相対パス昇順
    let ft_dir = pkg_ftplugin.join(ft);
    if is_dir_follow(&ft_dir).await? {
        let mut child_names = read_dir_matching(&ft_dir, "", false).await?;
        child_names.sort();
        for name in &child_names {
            push_unique(&mut paths, &format!("{prefix}/{ft}/{name}"));
        }
    }

    Ok(paths)
}

/// `dir` 直下のファイル名のうち条件を満たすものを返す。
/// `ft_prefix=true` なら `<ft>_*.vim|lua`、`false` なら `*.vim|lua`（`ft` 無視）。
/// `dir` が見つからない（NotFound）場合は空。それ以外の読み取りエラーは伝播する。
async fn read_dir_matching(dir: &Path, ft: &str, ft_prefix: bool) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    let required_prefix = ft_prefix.then(|| format!("{ft}_"));
    let rd = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    let mut entries = Vec::new();
    let mut rd = rd;
    while let Some(entry) = rd.next_entry().await? {
        entries.push(entry);
    }
    for entry in entries {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if let Some(p) = &required_prefix
            && !name.starts_with(p)
        {
            continue;
        }
        if !(name.ends_with(".vim") || name.ends_with(".lua")) {
            continue;
        }
        // symlink を1段 follow して実体がファイルか確認する。
        if !is_file_follow(&entry.path()).await? {
            continue;
        }
        out.push(name);
    }
    Ok(out)
}

/// `path` がファイルか（symlink は follow）。NotFound は false、それ以外は Err を伝播。
async fn is_file_follow(path: &Path) -> io::Result<bool> {
    match tokio::fs::metadata(path).await {
        Ok(m) => Ok(m.is_file()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// `path` がディレクトリか（symlink は follow）。NotFound は false、それ以外は Err を伝播。
async fn is_dir_follow(path: &Path) -> io::Result<bool> {
    match tokio::fs::metadata(path).await {
        Ok(m) => Ok(m.is_dir()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// `paths` へ `p` を stable-dedup で追加する（既存なら何もしない）。
fn push_unique(paths: &mut Vec<String>, p: &str) {
    if !paths.iter().any(|x| x == p) {
        paths.push(p.to_string());
    }
}

/// PackPath の象徴となる状態。この構造体に PluginLoaded をインサートしていき、最後に実際のパスを指定して install を行う。
#[derive(Default)]
pub struct PackPlan {
    installing: HashSet<Box<[u8]>>,
    files: HashMap<PluginIDStr, Files>,
    ctl: LazyRegistration,
    /// `split_doc` で分割された doc プラグイン群（LoadedPlugin のまま）。install の control
    /// マージで rsplug-doc・lazy loader と統一マージされ、1つの `_rsplug:doc` に集約される（Phase 8）。
    doc_plugins: Vec<LoadedPlugin>,
}

impl PackPlan {
    pub fn len(&self) -> usize {
        self.installing.len()
    }
    /// 空の PackPlan を生成する。
    pub fn new() -> Self {
        Default::default()
    }
    /// source プラグイン群を受け取る。**マージ前に各プラグインを `split_doc` で (rest, doc) に分割**し、
    /// doc 無しの rest 群をマージして登録する。doc 部は LoadedPlugin のまま `doc_plugins` に集め、
    /// install の control マージで rsplug-doc・lazy loader と統一的に1つの `_rsplug:doc` に集約する
    /// （Phase 8: doc をマージ対象から外し、LoadedPlugin 分割インタフェースで統一表現）。
    pub fn load(&mut self, mut plugins: BinaryHeap<LoadedPlugin>) {
        let drained: Vec<LoadedPlugin> = plugins.drain().collect();
        for p in drained {
            let (rest, doc) = p.split_doc();
            if let Some(doc) = doc {
                self.doc_plugins.push(doc);
            }
            plugins.push(rest);
        }
        MergePlanner::plan(&mut plugins);
        // `BinaryHeap` の IntoIterator は heap 内部順であり、`Ord` が表す
        // dependency/config order ではない。lazy trigger の id 配列はこの順序を
        // そのまま `packadd` 順として公開するため、必ず pop 順で登録する。
        // これにより依存プラグインの runtimepath が、依存元の lua_after hook より
        // 前に追加される。
        while let Some(plugin) = plugins.pop() {
            self.insert(plugin);
        }
    }
    /// PluginLoaded をインサートする。その PluginLoaded の実行制御や設定に必要な LazyRegistration を返す。
    pub fn insert(&mut self, loaded_plugin: LoadedPlugin) {
        let id = loaded_plugin.plugin_id();
        let id_str = id.as_str();
        let already_installed = !self.installing.insert(id_str.clone().into());
        if already_installed {
            return;
        }

        let LoadedPlugin {
            source_names,
            lazy_type,
            files,
            script,
            order,
            merge_enabled: _,
            is_lazy_registration,
            dotgit,
        } = loaded_plugin;

        if !is_lazy_registration {
            // doc 盗みはマージ前に `PackPlan::load` → `LoadedPlugin::steal_doc` で済ませているため、
            // ここでは lazy 実行制御（LazyRegistration）の生成のみ。files は変更しない。
            self.ctl += LazyRegistration::create(id, source_names, lazy_type, script, order);
        }
        match files {
            HowToPlaceFiles::CopyEachFile(files) => {
                for (path, item) in files {
                    let entry = self.files.entry(id_str.clone()).or_insert(Files {
                        is_lazy_registration,
                        entries: Vec::new(),
                        dotgit,
                    });
                    // 同 id に複数 LoadedPlugin が統合される場合、最初にエントリを作った
                    // LoadedPlugin の is_lazy_registration/dotgit が or_insert で固定されるのを防ぐため、
                    // 既存エントリのフラグを update する（どれか1つでも true なら true）。
                    entry.is_lazy_registration = entry.is_lazy_registration || is_lazy_registration;
                    entry.dotgit = entry.dotgit || dotgit;
                    // ファイル・sealed-dir を事前分類せずそのまま保持。
                    // install で `source.yank` が種別（file/dir/symlink）を判定して配置する。
                    entry.entries.push((path, item.source));
                }
            }
        }
    }

    /// PackPlan を指定されたパスにインストールする。パスは Vim の 'packpath' に基づく。
    /// NOTE: インストール後のディレクトリ構成は以下のようになる。
    /// {packpath}/pack/_gen/opt/{id}/
    pub async fn install(mut self, packpath: &Path) -> io::Result<GenerationPublished> {
        // R1: control マージが self.ctl を消費する前に、on_ft の (ft,id) を取り出す。
        // 公開後に gen_root/opt/<id>/ を走査して ftplugin インデックスを構築する。
        let ft_pairs = self.ctl.ft_index_pairs();
        {
            // LazyRegistration（lazy 実行制御）と分割された doc プラグイン群を control マージで統一する。
            // rsplug-doc・lazy loader・doc 分割群が1つの `_rsplug:doc`（+ 制御パック）に集約される。
            let plugins = {
                let plugins: Vec<LoadedPlugin> = std::mem::take(&mut self.ctl).into();
                let mut heap: BinaryHeap<_> = plugins.into();
                for doc in std::mem::take(&mut self.doc_plugins) {
                    heap.push(doc);
                }
                MergePlanner::plan(&mut heap);
                heap
            };
            for plugin in plugins {
                self.insert(plugin);
            }
        }
        let gen_root = packpath.join("pack").join("_gen");
        tokio::fs::create_dir_all(&gen_root).await?;
        // Staging is private to this run. The global publication lock is acquired
        // only after all copies and helptags have completed below.
        let Self {
            installing: _,
            files,
            ctl: _,
            doc_plugins: _,
        } = self;
        let mut generation_entries: Vec<String> = files
            .iter()
            .map(|(id, _)| {
                let mut key = String::with_capacity(4 + id.len());
                key.push_str("opt/");
                key.push_str(id);
                key
            })
            .collect();
        generation_entries.sort();
        // R1: manifest の生成は publish 後（ftplugin インデックス構築の後）へ移動した。
        let mut control_ids: Vec<PluginIDStr> = files
            .iter()
            .filter(|(_, files)| files.is_lazy_registration)
            .map(|(id, _)| id.clone())
            .collect();
        control_ids.sort();
        let init_content = render_init(&control_ids);
        let inventory_ftplugin_index = build_ft_index_from_inventories(&files, &ft_pairs);
        let desired_plan = inventory_ftplugin_index.as_ref().map(|ftplugin| {
            GenerationPlan::new(
                generation_entries.clone(),
                &control_ids,
                RuntimeManifest {
                    ftplugin: ftplugin.clone(),
                },
            )
        });
        // 全パッケージが既にある場合だけ、既存 generation と比較する。足りない package が
        // あれば ft index は構築不能なので、通常の staging/publish 経路へ進む。
        if (ft_pairs.is_empty() || desired_plan.is_some() || generation_entries.is_empty())
            && generation_is_current(
                &gen_root,
                &control_ids,
                &generation_entries,
                &init_content,
                packpath,
                desired_plan.as_ref(),
            )
            .await
        {
            msg(Message::InstallDone);
            return Ok(false);
        }
        // 新世代は staging 配下に構築し、copy/manifest/loader 全成功後に opt/ へ rename で
        // 公開する（publication 失敗が公開ツリーを壊さないようにする）。
        // A true no-op has already returned above, so stale staging cleanup is
        // not part of the no-op mutation/probe path.
        cleanup_stale_staging(&gen_root).await;
        let staging_control_id = control_ids
            .first()
            .map(|id| id.to_string())
            .unwrap_or_else(|| "none".to_string());
        let staging = staging_root(&gen_root, &staging_control_id);
        tokio::fs::create_dir_all(staging.join("opt")).await?;
        tokio::fs::write(
            staging.join(".lease"),
            std::process::id().to_string().as_bytes(),
        )
        .await?;
        let _staging_guard = StagingGuard(staging.clone());
        // copy 予算 min(16, max(2, CPU*2))。entry（パッケージ単位の yank）の fan-out 上限。
        // 旧実装は AdaptiveSemaphore::new()（上限256）で copy が過剰 fan-out していたのを抑える。
        // leaf コピーの fan-out は copy_tree 内で COPY_LEAF で別途抑える（Phase 1）。
        let copy_budget = (crate::rsplug::util::resources::available_cpus() * 2).clamp(2, 16);
        let yank_semaphore = AdaptiveSemaphore::with_limits(
            copy_budget,
            copy_budget,
            copy_budget,
            std::time::Duration::from_millis(64),
        );
        struct PackageCopyJob {
            id: Arc<str>,
            entries: Vec<(PathBuf, Arc<FileSource>)>,
            dir: Arc<Path>,
        }
        let (package_tx, package_rx) =
            tokio::sync::mpsc::channel::<PackageCopyJob>(copy_budget * 2);
        let shared_package_rx = Arc::new(tokio::sync::Mutex::new(package_rx));
        let mut package_workers = JoinSet::new();
        for _ in 0..copy_budget {
            crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::SpawnedWorker);
            let worker_rx = Arc::clone(&shared_package_rx);
            let yank_semaphore = yank_semaphore.clone();
            package_workers.spawn(async move {
                loop {
                    let job = {
                        let mut rx = worker_rx.lock().await;
                        rx.recv().await
                    };
                    let Some(PackageCopyJob { id, entries, dir }) = job else {
                        break;
                    };
                    for (which, source) in entries {
                        let permit = yank_semaphore.acquire().await;
                        let result = source.yank(&which, dir.as_ref()).await;
                        let is_error = result.is_err();
                        permit.finish(is_error);
                        result?;
                        msg(Message::InstallYank {
                            id: id.clone(),
                            which,
                        });
                    }
                }
                Ok::<(), io::Error>(())
            });
        }

        for (
            id,
            Files {
                is_lazy_registration: _,
                entries,
                dotgit,
            },
        ) in files
        {
            let id: Arc<str> = id.into();
            let published = gen_root.join("opt").join(id.as_ref());
            // 既存パッケージは内容ハッシュで識別（同じ id ≡ 同じ内容）なので再利用し copy を skip。
            // 公開 opt/ には触らず、新規パッケージを staging に構築することで、copy 失敗が
            // 公開ツリーを壊さないようにする。
            if tokio::fs::symlink_metadata(&published)
                .await
                .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            {
                msg(Message::InstallSkipped(id));
                continue;
            }
            let dir = staging.join("opt").join(id.as_ref());
            // dotgit=true だが `.git` エントリが無い（snapshot に `.git` が無い）場合は、
            // `.git` 無しで install すると git 利用プラグインが壊れるため pack install を skip し、
            // `-u` での再 materialize を促す（plugin は lock に残る）。`.git` があれば通常エントリとして yank される。
            if dotgit
                && !entries
                    .iter()
                    .any(|(p, _)| p.as_path() == Path::new(".git"))
            {
                msg(Message::PluginDotgitMissing(id.clone()));
                continue;
            }
            let dir: Arc<Path> = Arc::from(dir);
            crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::QueuedJob);
            package_tx
                .send(PackageCopyJob { id, entries, dir })
                .await
                .map_err(|_| io::Error::other("package copy workers stopped"))?;
        }
        drop(package_tx);
        while let Some(result) = package_workers.join_next().await {
            result.map_err(|e| io::Error::other(format!("package worker join failed: {e}")))??;
        }
        run_staged_helptags(&staging).await?;
        // The compatibility filesystem scan is also private planning work and
        // must not extend the publication lock window.
        let ftplugin_index = match inventory_ftplugin_index {
            Some(index) => index,
            None => {
                build_ft_index_from_roots(
                    &staging.join("opt"),
                    Some(&gen_root.join("opt")),
                    &ft_pairs,
                )
                .await?
            }
        };

        // Commit window: re-read the winner after private work completed. A
        // concurrent identical run may already have published everything.
        #[cfg(unix)]
        let _install_lock = acquire_install_lock(&gen_root).await?;
        if generation_is_current(
            &gen_root,
            &control_ids,
            &generation_entries,
            &init_content,
            packpath,
            desired_plan.as_ref(),
        )
        .await
        {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            msg(Message::InstallDone);
            return Ok(false);
        }

        // === Publish: staging で構築した新規パッケージを opt/ へ原子 rename で公開する。 ===
        // パッケージ id は内容ハッシュなので、staging にあるものは全て「新規」（既存は再利用され
        // staging に無い）で、opt/ との衝突はない。各 rename は POSIX 原子。
        tokio::fs::create_dir_all(gen_root.join("opt")).await?;
        if let Ok(mut rd) = tokio::fs::read_dir(staging.join("opt")).await {
            while let Some(entry) = rd.next_entry().await? {
                let name = entry.file_name();
                let destination = gen_root.join("opt").join(&name);
                if tokio::fs::symlink_metadata(&destination).await.is_ok() {
                    // Another publisher won this content-addressed package while
                    // this run was staging. Reuse only a complete directory.
                    if !destination.is_dir() || destination.is_symlink() {
                        return Err(io::Error::other(format!(
                            "published package is not a directory: {}",
                            destination.display()
                        )));
                    }
                    tokio::fs::remove_dir_all(entry.path()).await?;
                    continue;
                }
                crate::rsplug::perf::failpoint("package_rename_before")?;
                tokio::fs::rename(entry.path(), destination).await?;
                crate::rsplug::perf::failpoint("package_rename_after")?;
            }
        }

        let plan = GenerationPlan::new(
            generation_entries.clone(),
            &control_ids,
            RuntimeManifest {
                ftplugin: ftplugin_index.clone(),
            },
        );
        let plan_id = plan.id();
        let manifest = GenerationManifest {
            version: 2,
            entries: generation_entries,
            generation_id: plan_id.clone(),
            plan: Some(plan),
            runtime: RuntimeManifest {
                ftplugin: ftplugin_index,
            },
        };
        let manifest_content = serde_json::to_vec_pretty(&manifest).map_err(io::Error::other)?;
        let publication_name = desired_plan
            .as_ref()
            .map(GenerationPlan::id)
            .unwrap_or_else(|| {
                control_ids
                    .first()
                    .map(ToString::to_string)
                    .unwrap_or(plan_id)
            });

        let generations_dir = packpath.join("generations");
        tokio::fs::create_dir_all(&generations_dir).await?;
        // Generation metadata is mutable publication state, not package content.
        // Keep it outside immutable content-addressed opt/<id> directories.
        if !control_ids.is_empty() {
            let manifest_path = gen_root
                .join("generations")
                .join(&publication_name)
                .with_extension("json");
            tokio::fs::create_dir_all(manifest_path.parent().unwrap()).await?;
            let manifest_tmp = manifest_path.parent().unwrap().join(format!(
                ".{}.json.tmp-{}",
                publication_name,
                STAGING_NONCE.fetch_add(1, AtomicOrdering::Relaxed)
            ));
            crate::rsplug::perf::failpoint("generation_metadata_before")?;
            crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::GenerationManifestWrite);
            tokio::fs::write(&manifest_tmp, &manifest_content).await?;
            tokio::fs::rename(&manifest_tmp, &manifest_path).await?;
            crate::rsplug::perf::failpoint("generation_metadata_after")?;
        }
        if control_ids.is_empty() {
            // ponytail: no control package to anchor a generation file; fall back to a plain init.lua.
            // temp 経由の rename で原子置換する。
            let tmp = packpath.join(".init.lua.swap");
            crate::rsplug::perf::failpoint("pointer_swap_before")?;
            crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::InitLuaSwap);
            tokio::fs::write(&tmp, &init_content).await?;
            tokio::fs::rename(&tmp, packpath.join("init.lua")).await?;
            crate::rsplug::perf::failpoint("pointer_swap_after")?;
        } else {
            // Each generation's loader lives at generations/<control_id>.lua; init.lua is a
            // pure symlink to it, so older retained generations stay addressable by name.
            let gen_path = generations_dir
                .join(&publication_name)
                .with_extension("lua");
            tokio::fs::write(&gen_path, &init_content).await?;
            // remove+symlink には init.lua が一時消失する窓があるため、temp symlink を作って
            // rename で原子置換する（POSIX では既存ファイルへの rename は原子）。これが唯一の
            // ブータビリティ公開点で、これより前の失敗は init.lua → 旧世代のまま（公開ツリー intact）。
            let init_path = packpath.join("init.lua");
            let tmp = packpath.join(".init.lua.swap");
            crate::rsplug::perf::failpoint("pointer_swap_before")?;
            crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::InitLuaSwap);
            symlink_file(&gen_path, &tmp).await?;
            if tokio::fs::rename(&tmp, &init_path).await.is_err() {
                // Windows 等で既存ファイルへの rename が失敗する場合は remove+rename に fallback。
                let _ = tokio::fs::remove_file(&init_path).await;
                tokio::fs::rename(&tmp, &init_path).await?;
            }
            crate::rsplug::perf::failpoint("pointer_swap_after")?;
        }

        if !control_ids.is_empty() {
            write_generation_registry(&gen_root, &publication_name).await?;
        }

        #[cfg(unix)]
        drop(_install_lock);

        let retained_entries = retained_manifest_entries(
            &gen_root,
            std::slice::from_ref(&publication_name),
            &manifest,
        )
        .await?;

        let retained_entries = Arc::new(retained_entries);
        let res = if has_unreachable_packages(&gen_root, retained_entries.as_ref()).await? {
            let cleanup_semaphore = AdaptiveSemaphore::new();
            const CLEANUP_WORKERS: usize = 8;
            let (cleanup_tx, cleanup_rx) =
                tokio::sync::mpsc::channel::<(PathBuf, Arc<[u8]>)>(CLEANUP_WORKERS * 2);
            let cleanup_rx = Arc::new(tokio::sync::Mutex::new(cleanup_rx));
            let cleanup_error = Arc::new(std::sync::Mutex::new(None::<String>));
            let mut cleanup_tasks = JoinSet::new();
            for _ in 0..CLEANUP_WORKERS {
                crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::SpawnedWorker);
                let cleanup_rx = cleanup_rx.clone();
                let retained_entries = retained_entries.clone();
                let cleanup_semaphore = cleanup_semaphore.clone();
                let cleanup_error = cleanup_error.clone();
                cleanup_tasks.spawn(async move {
                    loop {
                        let Some((path, start_or_opt_key)) = ({
                            let mut rx = cleanup_rx.lock().await;
                            rx.recv().await
                        }) else {
                            break;
                        };
                        let Some(file_name) = path
                            .file_name()
                            .map(|name| os_string_to_install_key(name.to_os_string()))
                        else {
                            continue;
                        };
                        let mut entry_key =
                            Vec::with_capacity(start_or_opt_key.len() + 1 + file_name.len());
                        entry_key.extend_from_slice(&start_or_opt_key);
                        entry_key.push(b'/');
                        entry_key.extend_from_slice(&file_name);
                        if retained_entries.contains(entry_key.as_slice()) {
                            continue;
                        }
                        match tokio::fs::symlink_metadata(&path).await {
                            Ok(meta) if meta.is_dir() => {
                                let result = async {
                                    crate::rsplug::perf::failpoint("gc_before")?;
                                    let permit = cleanup_semaphore.acquire().await;
                                    let result = tokio::fs::remove_dir_all(&path).await;
                                    crate::rsplug::perf::incr(
                                        crate::rsplug::perf::PerfOp::GcDelete,
                                    );
                                    permit.finish(result.is_err());
                                    result
                                }
                                .await;
                                if let Err(error) = result {
                                    let mut first = cleanup_error.lock().unwrap();
                                    if first.is_none() {
                                        *first = Some(error.to_string());
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                            Err(error) => {
                                let mut first = cleanup_error.lock().unwrap();
                                if first.is_none() {
                                    *first = Some(error.to_string());
                                }
                            }
                        }
                    }
                    Ok::<(), io::Error>(())
                });
            }
            const GC_MAX_CANDIDATES: usize = 4096;
            const GC_MAX_TIME: std::time::Duration = std::time::Duration::from_millis(100);
            let gc_deadline = std::time::Instant::now() + GC_MAX_TIME;
            let mut gc_candidates = 0usize;
            'gc: for start_or_opt in ["start", "opt"] {
                let path = gen_root.join(start_or_opt);
                let start_or_opt_key: Arc<[u8]> = Arc::from(start_or_opt.as_bytes());
                if let Ok(mut read_dir) = tokio::fs::read_dir(path).await {
                    while let Some(entry) = read_dir.next_entry().await? {
                        if gc_candidates >= GC_MAX_CANDIDATES
                            || std::time::Instant::now() >= gc_deadline
                        {
                            break 'gc;
                        }
                        gc_candidates += 1;
                        crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::GcCandidate);
                        cleanup_tx
                            .send((entry.path(), Arc::clone(&start_or_opt_key)))
                            .await
                            .map_err(|_| io::Error::other("cleanup worker queue closed"))?;
                    }
                }
            }
            drop(cleanup_tx);

            let res = cleanup_tasks
                .join_all()
                .await
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
                .and(Ok(()));
            res.and_then(|_| {
                cleanup_error
                    .lock()
                    .unwrap()
                    .take()
                    .map_or(Ok(()), |error| Err(io::Error::other(error)))
            })
        } else {
            Ok(())
        };
        // Best-effort: drop retained generation loaders whose anchor control package was pruned.
        if res.is_ok()
            && let Ok(mut read_dir) = tokio::fs::read_dir(&generations_dir).await
        {
            while let Ok(Some(entry)) = read_dir.next_entry().await {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("lua") {
                    continue;
                }
                let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if !gen_root
                    .join("generations")
                    .join(id)
                    .with_extension("json")
                    .is_file()
                {
                    tokio::fs::remove_file(&path).await.ok();
                }
            }
        }
        msg(Message::InstallDone);
        res.map(|()| true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_template_packadds_control_packages() {
        let control_id = b"control-package".plugin_id().as_str();
        let script = String::from_utf8(render_init(std::slice::from_ref(&control_id))).unwrap();

        // The control id is emitted into the ids table and looped over with vim.cmd.packadd(id).
        assert!(
            script.contains(&format!("\"{control_id}\"")),
            "control id must appear in the ids table: {script:?}"
        );
        assert!(script.contains("vim.cmd.packadd(id)"));
        assert!(!script.contains("packloadall"));
    }

    #[test]
    fn init_template_emits_exact_packadd_block() {
        let a = b"aaaa".plugin_id().as_str();
        let b = b"bbbb".plugin_id().as_str();
        let script = String::from_utf8(render_init(&[a.clone(), b.clone()])).unwrap();
        // locks in the exact ids-table shape; break whitespace here if the template changes.
        let actual = script
            .split("vim.opt.packpath:prepend(root)\n\n")
            .nth(1)
            .unwrap();
        let expected = format!(
            "local requested = vim.env.RSPLUG_GENERATION\nlocal ids = {{ \"{a}\",\"{b}\", }}\n"
        );
        assert!(
            actual.starts_with(&expected),
            "unexpected init template output: {actual:?}\nexpected prefix: {expected:?}"
        );
    }

    #[test]
    fn init_template_resolves_symlink_and_goes_up_two_levels() {
        let id = b"gen".plugin_id().as_str();
        let script = String::from_utf8(render_init(std::slice::from_ref(&id))).unwrap();
        // init.lua is a symlink into generations/; resolve + :h:h recovers ~/.cache/rsplug
        // whether loaded through the symlink or directly as a generation file.
        assert!(
            script.contains("vim.fn.resolve"),
            "must resolve the init.lua symlink"
        );
        assert!(
            script.contains(":h:h"),
            "must go up two levels from generations/<id>.lua"
        );
    }

    #[test]
    fn init_template_empty_control_ids_is_safe_without_rsplug_runtime() {
        // control_ids が空（プラグイン0件）のとき _rsplug ランタイムモジュールは
        // 生成されない。init.lua が無条件で require('_rsplug') すると nvim 起動が
        // クラッシュするため、require/startup ブロックを出力しない。
        let script = String::from_utf8(render_init(&[])).unwrap();
        assert!(
            !script.contains("require, '_rsplug'"),
            "empty control_ids must not require _rsplug: {script:?}"
        );
        assert!(
            !script.contains("rsplug.startup()"),
            "empty control_ids must not call startup: {script:?}"
        );
        assert!(
            !script.contains("RSPLUG_GENERATION"),
            "empty control_ids must not emit the generation override block: {script:?}"
        );
    }

    #[test]
    fn init_template_supports_rsplug_generation_override() {
        let id = b"gen".plugin_id().as_str();
        let script = String::from_utf8(render_init(std::slice::from_ref(&id))).unwrap();
        // Reads RSPLUG_GENERATION and prefers it over the default ids when valid.
        assert!(script.contains("vim.env.RSPLUG_GENERATION"));
        // Guards the override id: hex-only and exactly 32 chars (no path traversal).
        assert!(script.contains("^[0-9a-fA-F]+$"));
        assert!(script.contains("#requested == 32"));
        // Confirms the generation file exists before switching to it.
        assert!(script.contains("vim.fn.filereadable"));
        // Falls back with a warning when the override is unusable.
        assert!(script.contains("vim.notify"));
        assert!(script.contains("vim.log.levels.WARN"));
    }

    #[test]
    fn manifest_entries_are_parsed_from_json_model() {
        let manifest = GenerationManifest {
            version: 1,
            entries: vec![
                "opt/22222222222222222222222222222222".to_string(),
                "opt/11111111111111111111111111111111".to_string(),
            ],
            ..Default::default()
        };
        let parsed = manifest_entries(&manifest);

        assert!(parsed.contains("opt/22222222222222222222222222222222".as_bytes()));
        assert!(parsed.contains("opt/11111111111111111111111111111111".as_bytes()));
    }

    // ---- R1: generation manifest v2 + ft index ----

    #[test]
    fn manifest_v2_roundtrips_runtime_ftplugin() {
        let manifest = GenerationManifest {
            version: 2,
            entries: vec!["opt/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()],
            generation_id: String::new(),
            plan: None,
            runtime: RuntimeManifest {
                ftplugin: BTreeMap::from([(
                    "lua".to_string(),
                    BTreeMap::from([(
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                        vec![
                            "opt/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/ftplugin/lua.vim".to_string(),
                            "opt/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/ftplugin/lua/settings.lua"
                                .to_string(),
                        ],
                    )]),
                )]),
            },
        };
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let back: GenerationManifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.version, 2);
        let lua = back.runtime.ftplugin.get("lua").unwrap();
        let paths = lua.get("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert_eq!(
            paths,
            &vec![
                "opt/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/ftplugin/lua.vim".to_string(),
                "opt/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/ftplugin/lua/settings.lua".to_string(),
            ]
        );
    }

    #[test]
    fn retained_v1_manifest_deserializes_without_runtime() {
        // 保持されている v1 manifest には `runtime` が無い。`#[serde(default)]` で空になる。
        let v1_json = r#"{"version":1,"entries":["opt/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]}"#;
        let parsed: GenerationManifest = serde_json::from_str(v1_json).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.entries, vec!["opt/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]);
        assert!(parsed.runtime.ftplugin.is_empty());
    }

    #[test]
    fn generation_plan_id_is_deterministic_and_runtime_sensitive() {
        let id = b"generation-control".plugin_id().as_str();
        let runtime = RuntimeManifest {
            ftplugin: BTreeMap::from([(
                "lua".to_string(),
                BTreeMap::from([(id.to_string(), vec![format!("opt/{id}/ftplugin/lua.vim")])]),
            )]),
        };
        let first = GenerationPlan::new(
            vec![format!("opt/{id}")],
            std::slice::from_ref(&id),
            runtime.clone(),
        );
        let second = GenerationPlan::new(
            vec![format!("opt/{id}")],
            std::slice::from_ref(&id),
            runtime,
        );
        assert_eq!(first, second);
        assert_eq!(first.id(), second.id());
        let changed = GenerationPlan::new(
            vec![format!("opt/{id}")],
            std::slice::from_ref(&id),
            RuntimeManifest::default(),
        );
        assert_ne!(first.id(), changed.id());
    }

    const FT_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// `gen_root/opt/<id>/ftplugin/` を作り、指定ファイルを配置する。
    async fn ft_fixture(files: &[(&str, &[u8])]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("opt").join(FT_ID).join("ftplugin");
        for (rel, data) in files {
            let p = base.join(rel);
            tokio::fs::create_dir_all(p.parent().unwrap())
                .await
                .unwrap();
            tokio::fs::write(p, data).await.unwrap();
        }
        tmp
    }

    #[tokio::test]
    async fn build_ft_index_collects_three_groups_sorted_and_dedup() {
        // exact: lua.vim, lua.lua (この順) / suffix: lua_a.lua, lua_b.vim (ソート) /
        // subdir: lua/a.lua, lua/x.vim (ソート) / 関係ない foo.txt は除外。
        let tmp = ft_fixture(&[
            ("lua.lua", b"x"),
            ("lua.vim", b"x"),
            ("lua_b.vim", b"x"),
            ("lua_a.lua", b"x"),
            ("lua/x.vim", b"x"),
            ("lua/a.lua", b"x"),
            ("foo.txt", b"x"),
        ])
        .await;
        let ft_pairs = BTreeMap::from([("lua".to_string(), vec![FT_ID.to_string()])]);
        let idx = build_ft_index(tmp.path(), &ft_pairs).await.unwrap();
        let paths = idx.get("lua").unwrap().get(FT_ID).unwrap();
        let expected: Vec<String> = [
            "opt/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/ftplugin/lua.vim",
            "opt/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/ftplugin/lua.lua",
            "opt/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/ftplugin/lua_a.lua",
            "opt/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/ftplugin/lua_b.vim",
            "opt/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/ftplugin/lua/a.lua",
            "opt/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/ftplugin/lua/x.vim",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(paths, &expected);
    }

    #[tokio::test]
    async fn build_ft_index_empty_when_ftplugin_missing() {
        // ftplugin/ が無ければ空。新規・再利用パッケージ問わず opt/<id> を見る。
        let tmp = tempfile::tempdir().unwrap();
        let ft_pairs = BTreeMap::from([("lua".to_string(), vec![FT_ID.to_string()])]);
        let idx = build_ft_index(tmp.path(), &ft_pairs).await.unwrap();
        assert!(idx.is_empty());
    }

    #[tokio::test]
    async fn build_ft_index_skips_unrelated_extensions() {
        let tmp = ft_fixture(&[("lua.vim", b"x"), ("lua_notes.md", b"x")]).await;
        let ft_pairs = BTreeMap::from([("lua".to_string(), vec![FT_ID.to_string()])]);
        let idx = build_ft_index(tmp.path(), &ft_pairs).await.unwrap();
        let paths = idx.get("lua").unwrap().get(FT_ID).unwrap();
        // .md は対象外。lua_notes.md は lua_* だが .vim/.lua でないので除外。
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("/ftplugin/lua.vim"));
    }

    #[tokio::test]
    async fn inventory_ft_index_matches_exact_suffix_and_subdir_order() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(tmp.path().join("ftplugin/lua"))
            .await
            .unwrap();
        for name in ["lua.vim", "lua.lua", "lua_b.vim", "lua_a.lua", "lua/x.vim"] {
            tokio::fs::write(tmp.path().join("ftplugin").join(name), b"x")
                .await
                .unwrap();
        }
        let inventory = Arc::new(
            SnapshotManifest::build(tmp.path(), false, ".rsplug_build_success")
                .await
                .unwrap(),
        );
        let id = b"inventory-ft".plugin_id().as_str();
        let files = HashMap::from([(
            id.clone(),
            Files {
                is_lazy_registration: false,
                entries: vec![(
                    PathBuf::from("ftplugin"),
                    Arc::new(FileSource::Directory {
                        path: Arc::from(tmp.path().to_path_buf()),
                        inventory: Some(inventory),
                        handle: None,
                    }),
                )],
                dotgit: false,
            },
        )]);
        let pairs = BTreeMap::from([(String::from("lua"), vec![id.to_string()])]);
        let index = build_ft_index_from_inventories(&files, &pairs).unwrap();
        let id_string = id.to_string();
        let paths = &index["lua"][&id_string];
        assert_eq!(
            paths,
            &vec![
                format!("opt/{id}/ftplugin/lua.vim"),
                format!("opt/{id}/ftplugin/lua.lua"),
                format!("opt/{id}/ftplugin/lua_a.lua"),
                format!("opt/{id}/ftplugin/lua_b.vim"),
                format!("opt/{id}/ftplugin/lua/x.vim"),
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn build_ft_index_aborts_on_read_error() {
        // root はディレクトリ権限を無視して読めるため、このテストの前提が成立しない。
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        // <ft>/ ディレクトリを読めない（権限なし）場合は Err で公開を中断する。
        let tmp = ft_fixture(&[("lua.vim", b"x")]).await;
        let ft_dir = tmp
            .path()
            .join("opt")
            .join(FT_ID)
            .join("ftplugin")
            .join("lua");
        tokio::fs::create_dir_all(&ft_dir).await.unwrap();
        tokio::fs::write(ft_dir.join("a.lua"), b"x").await.unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perms = tokio::fs::metadata(&ft_dir).await.unwrap().permissions();
        perms.set_mode(0o000);
        tokio::fs::set_permissions(&ft_dir, perms).await.unwrap();
        let ft_pairs = BTreeMap::from([("lua".to_string(), vec![FT_ID.to_string()])]);
        let result = build_ft_index(tmp.path(), &ft_pairs).await;
        // tempdir の cleanup が失敗しないよう権限を戻す。
        let mut perms = tokio::fs::metadata(&ft_dir).await.unwrap().permissions();
        perms.set_mode(0o755);
        let _ = tokio::fs::set_permissions(&ft_dir, perms).await;
        assert!(result.is_err(), "unreadable ft dir must abort publication");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn build_ft_index_follows_symlink_dir() {
        // <ft>/ がディレクトリへの symlink でも1段 follow して子を収集する。
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("opt").join(FT_ID).join("ftplugin");
        tokio::fs::create_dir_all(&base).await.unwrap();
        // 実ディレクトリを別場所に置き、ftplugin/lua を symlink にする。
        let real = tmp.path().join("real-lua");
        tokio::fs::create_dir_all(&real).await.unwrap();
        tokio::fs::write(real.join("a.lua"), b"x").await.unwrap();
        tokio::fs::symlink(&real, base.join("lua")).await.unwrap();
        let ft_pairs = BTreeMap::from([("lua".to_string(), vec![FT_ID.to_string()])]);
        let idx = build_ft_index(tmp.path(), &ft_pairs).await.unwrap();
        let paths = idx.get("lua").unwrap().get(FT_ID).unwrap();
        assert!(paths.iter().any(|p| p.ends_with("/ftplugin/lua/a.lua")));
    }

    // ---- identity / hash 安全性 (PLANS §15.1) ----

    fn snap(repo: &str, rev: &[u8]) -> RepoSnapshotIdentity {
        RepoSnapshotIdentity::new(
            PathBuf::from(repo),
            rev.to_vec(),
            None,
            Arc::<[String]>::from([]),
            None,
        )
    }

    /// `(install_path, FileItem)` を repo 由来ファイルとして組み立てる。`dir` は
    /// `FileSource::Directory` の絶対パス（identity には含まれないはずの配置パス）。
    fn repo_file(snapshot: RepoSnapshotIdentity, rel: &str, dir: &str) -> (PathBuf, FileItem) {
        (
            PathBuf::from(rel),
            FileItem::new(
                Arc::new(FileSource::Directory {
                    path: Arc::from(PathBuf::from(dir)),
                    inventory: None,
                    handle: None,
                }),
                FileIdentity::RepoFile(RepoFileIdentity::new(snapshot, PathBuf::from(rel))),
                MergeType::Conflict,
            ),
        )
    }

    fn synth(files: HowToPlaceFiles) -> LoadedPlugin {
        LoadedPlugin {
            source_names: BTreeSet::new(),
            lazy_type: LazyType::Start,
            files,
            script: SetupScript::default(),
            order: 0,
            merge_enabled: true,
            is_lazy_registration: false,
            dotgit: false,
        }
    }

    #[test]
    fn load_keeps_dependency_order_in_lazy_trigger_records() {
        use std::borrow::Cow;

        let event: Autocmd = "VimEnter".parse().unwrap();
        let make_plugin = |order, file, data: &'static [u8]| LoadedPlugin {
            source_names: BTreeSet::new(),
            lazy_type: LazyType::Opt(BTreeSet::from([LoadEvent::Autocmd(event.clone())])),
            files: HowToPlaceFiles::CopyEachFile(BTreeMap::from([(
                PathBuf::from(file),
                FileItem::new(
                    Arc::new(FileSource::File {
                        data: Cow::Borrowed(data),
                    }),
                    FileIdentity::GeneratedFile {
                        path: PathBuf::from(file),
                        data_hash: crate::rsplug::util::hash::digest_hash(data),
                    },
                    MergeType::Conflict,
                ),
            )])),
            script: SetupScript::default(),
            order,
            merge_enabled: false,
            is_lazy_registration: false,
            dotgit: false,
        };

        // `depends` の DAG 順に相当する order 0 の dependency を、order 1 の
        // dependent より先に登録しなければ `lua_after` が dependency の Lua
        // module を require できない。
        let dependency = make_plugin(0, "plugin/dependency.lua", b"dependency");
        let dependent = make_plugin(1, "plugin/dependent.lua", b"dependent");
        let dependency_id = dependency.plugin_id().as_str().to_string();
        let dependent_id = dependent.plugin_id().as_str().to_string();
        let mut plugins = BinaryHeap::new();
        plugins.push(dependent);
        plugins.push(dependency);

        let mut plan = PackPlan::new();
        plan.load(plugins);
        assert_eq!(
            plan.ctl.event_ids_for_test(&event),
            vec![dependency_id, dependent_id],
            "lazy event registration must preserve dependency order"
        );
    }

    /// Independent small reference engine used by the merge property gate.
    /// It intentionally retains the old shifting-vector algorithm; the test
    /// compares only final package identities, never private group layout.
    fn reference_merge(mut items: Vec<LoadedPlugin>) -> Vec<String> {
        items.sort_by(|left, right| {
            left.lazy_type
                .cmp(&right.lazy_type)
                .then(left.order.cmp(&right.order))
                .then(left.is_lazy_registration.cmp(&right.is_lazy_registration))
                .then(left.plugin_id().cmp(&right.plugin_id()))
        });
        let mut groups = Vec::new();
        for item in items {
            let mut pending = item;
            loop {
                let mut merged = false;
                for i in 0..groups.len() {
                    let candidate = groups.remove(i);
                    match candidate + pending {
                        (merged_group, None) => {
                            pending = merged_group;
                            merged = true;
                            break;
                        }
                        (candidate, Some(rest)) => {
                            groups.insert(i, candidate);
                            pending = rest;
                        }
                    }
                }
                if !merged {
                    break;
                }
            }
            groups.push(pending);
        }
        let mut ids = groups
            .into_iter()
            .map(|plugin| plugin.plugin_id().as_str().to_string())
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }

    fn property_plugins(order: &[usize]) -> Vec<LoadedPlugin> {
        order
            .iter()
            .map(|&index| {
                let path = PathBuf::from(format!("lua/mod{}/init.lua", index % 3));
                let source = Arc::new(FileSource::File {
                    data: Cow::Owned(vec![index as u8, (index * 17) as u8]),
                });
                let item = FileItem::new(
                    source,
                    FileIdentity::GeneratedFile {
                        path: path.clone(),
                        data_hash: [index as u8; 16],
                    },
                    if index % 4 == 0 {
                        MergeType::Overwrite
                    } else {
                        MergeType::Conflict
                    },
                );
                let mut plugin = synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([(
                    path, item,
                )])));
                plugin.order = index % 4;
                plugin.merge_enabled = index % 7 != 0;
                plugin
            })
            .collect()
    }

    #[test]
    fn merge_reference_property_matches_randomized_small_inputs() {
        let mut state = 0x9e3779b9u32;
        for _case in 0..128 {
            let mut order = (0..8usize).collect::<Vec<_>>();
            for i in (1..order.len()).rev() {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let j = (state as usize) % (i + 1);
                order.swap(i, j);
            }
            let reference = reference_merge(property_plugins(&order));
            let mut heap = property_plugins(&order)
                .into_iter()
                .collect::<BinaryHeap<_>>();
            MergePlanner::plan(&mut heap);
            let mut actual = heap
                .into_iter()
                .map(|plugin| plugin.plugin_id().as_str().to_string())
                .collect::<Vec<_>>();
            actual.sort();
            assert_eq!(
                actual, reference,
                "merge mismatch for permutation {order:?}"
            );
        }
    }

    #[test]
    fn merge_disabled_start_user_plugins_are_not_merged() {
        let snapshot_a = snap("github.com/owner/a", b"rev-a");
        let snapshot_b = snap("github.com/owner/b", b"rev-b");
        let mut disabled = synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([repo_file(
            snapshot_a,
            "plugin/a.lua",
            "/cache/a",
        )])));
        disabled.merge_enabled = false;
        let enabled = synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([repo_file(
            snapshot_b,
            "plugin/b.lua",
            "/cache/b",
        )])));

        let (_left, rest) = disabled + enabled;
        assert!(
            rest.is_some(),
            "merge=false must prevent start-plugin merging"
        );
    }

    #[test]
    fn merge_disabled_opt_user_plugins_are_not_merged() {
        let snapshot_a = snap("github.com/owner/a", b"rev-a");
        let snapshot_b = snap("github.com/owner/b", b"rev-b");
        let mut disabled = synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([repo_file(
            snapshot_a,
            "plugin/a.lua",
            "/cache/a",
        )])));
        disabled.lazy_type = LazyType::Opt(Default::default());
        disabled.merge_enabled = false;
        let mut enabled = synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([repo_file(
            snapshot_b,
            "plugin/b.lua",
            "/cache/b",
        )])));
        enabled.lazy_type = LazyType::Opt(Default::default());

        let (_left, rest) = disabled + enabled;
        assert!(
            rest.is_some(),
            "merge=false must prevent opt-plugin merging"
        );
    }

    #[test]
    fn merge_preserves_all_source_names() {
        // マージで片側の on_source 参照名 (source_name) を潰さないこと（Phase 1）。
        let snapshot_a = snap("github.com/owner/a", b"rev-a");
        let snapshot_b = snap("github.com/owner/b", b"rev-b");
        let mut a = synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([repo_file(
            snapshot_a,
            "plugin/a.lua",
            "/cache/a",
        )])));
        a.source_names.insert("a".to_string());
        let mut b = synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([repo_file(
            snapshot_b,
            "plugin/b.lua",
            "/cache/b",
        )])));
        b.source_names.insert("b".to_string());

        let (merged, rest) = a + b;
        assert!(rest.is_none(), "disjoint start plugins should merge");
        assert_eq!(
            merged.source_names,
            BTreeSet::from(["a".to_string(), "b".to_string()]),
            "merge must preserve both sides' on_source names"
        );
    }

    #[tokio::test]
    async fn manifest_driven_merge_probes_prefer_manifest_over_filesystem() {
        // Phase 2 Part B: manifest があれば種別/子集合を manifest から引き、filesystem
        // walk（is_dir / read_dir）を省く。manifest 無し・symlink・不在は fallback。
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        tokio::fs::create_dir_all(root.join("lua/sub"))
            .await
            .unwrap();
        tokio::fs::write(root.join("lua/a.lua"), b"a")
            .await
            .unwrap();
        tokio::fs::write(root.join("lua/sub/b.lua"), b"b")
            .await
            .unwrap();
        tokio::fs::write(root.join("file.txt"), b"f").await.unwrap();
        SnapshotManifest::build_and_write(&root, false, ".rsplug_build_success")
            .await
            .unwrap();

        // manifest 由来の種別・子集合が filesystem と一致する。
        assert!(merge_is_dir(&root, Path::new("lua")));
        assert!(!merge_is_dir(&root, Path::new("file.txt")));
        let lua = merge_children(&root, Path::new("lua"));
        assert!(lua.contains(&OsString::from("a.lua")));
        assert!(lua.contains(&OsString::from("sub")));
        let sub = merge_children(&root, Path::new("lua/sub"));
        assert!(sub.contains(&OsString::from("b.lua")));
        // manifest に無いパスは filesystem fallback（実パスを参照）。
        assert!(!merge_is_dir(&root, Path::new("does-not-exist")));
    }

    #[test]
    fn copy_plugin_id_is_independent_of_absolute_cache_path() {
        let snapshot = snap(
            "github.com/owner/repo",
            b"0123456789012345678901234567890123456789",
        );
        let id_a = synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([repo_file(
            snapshot.clone(),
            "plugin/init.lua",
            "/machineA/cache/owner/repo",
        )])))
        .plugin_id();
        let id_b = synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([repo_file(
            snapshot,
            "plugin/init.lua",
            "/machineB/cache/owner/repo",
        )])))
        .plugin_id();
        assert_eq!(id_a, id_b, "absolute cache path must not affect plugin_id");
    }

    #[test]
    fn copy_plugin_id_reflects_repo_cache_dir_and_head_rev() {
        let make = |repo: &str, rev: &[u8]| {
            let snapshot = snap(repo, rev);
            synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([repo_file(
                snapshot,
                "plugin/init.lua",
                "/cache",
            )])))
            .plugin_id()
        };
        let base = make(
            "github.com/owner/repo",
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        // repo_cache_dir が違うと id が変わる
        assert_ne!(
            base,
            make(
                "github.com/owner/other",
                b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            )
        );
        // head_rev が違うと id が変わる
        assert_ne!(
            base,
            make(
                "github.com/owner/repo",
                b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            )
        );
    }

    #[test]
    fn merged_copy_plugin_id_reflects_all_repos() {
        // 異なる 2 repo の CopyEachFile を merge すると、両 repo の identity が反映される。
        // 旧設計 (repo_meta: Option で merge 時 .or()) だと片方しか残らず、
        // merged id が単独 repo の id に一致してしまっていた（回帰テスト）。
        let snap_a = snap(
            "github.com/owner/a",
            b"revAaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        let snap_b = snap(
            "github.com/owner/b",
            b"revBbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );

        let both = synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([
            repo_file(snap_a.clone(), "plugin/a.lua", "/cache/a"),
            repo_file(snap_b.clone(), "plugin/b.lua", "/cache/b"),
        ])));

        let mut heap = BinaryHeap::new();
        heap.push(synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([
            repo_file(snap_a.clone(), "plugin/a.lua", "/cache/a"),
        ]))));
        heap.push(synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([
            repo_file(snap_b.clone(), "plugin/b.lua", "/cache/b"),
        ]))));
        MergePlanner::plan(&mut heap);

        let merged: Vec<_> = heap.into_iter().collect();
        assert_eq!(
            merged.len(),
            1,
            "two mergeable start plugins should merge into one"
        );
        let merged_id = merged[0].plugin_id();

        assert_eq!(
            merged_id,
            both.plugin_id(),
            "merged id must equal the directly-constructed both-files plugin"
        );
        assert_ne!(
            merged_id,
            synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([repo_file(
                snap_a,
                "plugin/a.lua",
                "/cache/a",
            )])))
            .plugin_id(),
            "merged id must differ from repo-a-only id"
        );
        assert_ne!(
            merged_id,
            synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([repo_file(
                snap_b,
                "plugin/b.lua",
                "/cache/b",
            )])))
            .plugin_id(),
            "merged id must differ from repo-b-only id"
        );
    }

    #[test]
    fn snapshot_key_is_plain_rev_when_no_build_inputs() {
        let id = snap(
            "github.com/o/r",
            b"0123456789012345678901234567890123456789",
        );
        // build/lua_build/dirty_diff が全て無ければ head_rev のみ。
        assert_eq!(
            id.snapshot_key(),
            "0123456789012345678901234567890123456789"
        );
    }

    #[test]
    fn snapshot_key_has_v1_suffix_and_tracks_build_inputs() {
        let rev = b"0123456789012345678901234567890123456789";
        let with_build = RepoSnapshotIdentity::new(
            PathBuf::from("github.com/o/r"),
            rev.to_vec(),
            None,
            Arc::<[String]>::from(["make".to_string()]),
            None,
        );
        let key = with_build.snapshot_key();
        assert!(
            key.starts_with("0123456789012345678901234567890123456789__v1_"),
            "got {key}"
        );
        // build が変わると suffix が変わる
        let with_other_build = RepoSnapshotIdentity::new(
            PathBuf::from("github.com/o/r"),
            rev.to_vec(),
            None,
            Arc::<[String]>::from(["cmake".to_string()]),
            None,
        );
        assert_ne!(key, with_other_build.snapshot_key());
    }

    #[test]
    fn snapshot_key_ignores_dirty_but_reflects_lua_build() {
        let rev = b"0123456789012345678901234567890123456789";
        let mk = |dirty, lua| {
            RepoSnapshotIdentity::new(
                PathBuf::from("r"),
                rev.to_vec(),
                dirty,
                Arc::<[String]>::from(["make".to_string()]),
                lua,
            )
        };
        // dirty_diff は snapshot_key に含まれない（build 前に key を確定し再利用を可能にするため）。
        let base = mk(None, None);
        let with_dirty = mk(Some([1u8; 16]), None);
        assert_eq!(base.snapshot_key(), with_dirty.snapshot_key());
        // lua_build が変われば key も変わる。
        let with_lua = mk(None, Some(Arc::from("vim.cmd('x')")));
        assert_ne!(base.snapshot_key(), with_lua.snapshot_key());
    }

    #[test]
    fn snapshot_key_ignores_repo_cache_dir() {
        // worktrees/ は repo ごとに分かれるため、key 自体は repo_cache_dir に依存しない。
        let rev = b"0123456789012345678901234567890123456789";
        let a = RepoSnapshotIdentity::new(
            PathBuf::from("github.com/o/a"),
            rev.to_vec(),
            None,
            Arc::<[String]>::from([]),
            None,
        );
        let b = RepoSnapshotIdentity::new(
            PathBuf::from("github.com/o/b"),
            rev.to_vec(),
            None,
            Arc::<[String]>::from([]),
            None,
        );
        assert_eq!(a.snapshot_key(), b.snapshot_key());
    }

    #[test]
    fn generated_file_id_reflects_path_and_data() {
        let make = |path: &'static str, data: &'static [u8]| {
            let data_hash = crate::rsplug::util::hash::digest_hash(data);
            synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([(
                PathBuf::from(path),
                FileItem::new(
                    Arc::new(FileSource::File {
                        data: Cow::Borrowed(data),
                    }),
                    FileIdentity::GeneratedFile {
                        path: PathBuf::from(path),
                        data_hash,
                    },
                    MergeType::Overwrite,
                ),
            )])))
            .plugin_id()
        };
        assert_ne!(make("plugin/a.lua", b"x"), make("plugin/b.lua", b"x")); // path 違い
        assert_ne!(make("plugin/a.lua", b"x"), make("plugin/a.lua", b"y")); // data 違い
        assert_eq!(make("plugin/a.lua", b"x"), make("plugin/a.lua", b"x")); // 同一
    }

    #[tokio::test]
    async fn dotgit_missing_snapshot_skips_install() {
        let dir =
            std::env::temp_dir().join(format!("rsplug-dotgit-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let snapshot_root = dir.join("snapshot");
        let packpath = dir.join("packpath");
        std::fs::create_dir_all(snapshot_root.join("plugin")).unwrap();
        std::fs::write(snapshot_root.join("plugin/init.lua"), "print('x')\n").unwrap();

        let snapshot = RepoSnapshotIdentity::new(
            PathBuf::from("github.com/owner/repo"),
            b"0123456789012345678901234567890123456789".to_vec(),
            None,
            Arc::<[String]>::from([]),
            None,
        );
        let files = BTreeMap::from([(
            PathBuf::from("plugin/init.lua"),
            FileItem::new(
                Arc::new(FileSource::Directory {
                    path: Arc::from(snapshot_root.clone()),
                    inventory: None,
                    handle: None,
                }),
                FileIdentity::RepoFile(RepoFileIdentity::new(
                    snapshot,
                    PathBuf::from("plugin/init.lua"),
                )),
                MergeType::Conflict,
            ),
        )]);
        let loaded = LoadedPlugin {
            source_names: BTreeSet::new(),
            lazy_type: LazyType::Start,
            files: HowToPlaceFiles::CopyEachFile(files),
            script: SetupScript::default(),
            order: 0,
            merge_enabled: true,
            is_lazy_registration: false,
            dotgit: true,
        };
        let plugin_id = loaded.plugin_id();

        let mut state = PackPlan::new();
        state.insert(loaded);
        state.install(&packpath).await.unwrap();

        assert!(
            !packpath
                .join("pack/_gen/opt")
                .join(plugin_id.as_str())
                .exists(),
            "dotgit-missing plugin must not be copied into pack"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn copy_tree_preserves_files_dirs_and_symlinks() {
        let root = std::env::temp_dir().join(format!("rsplug-copytree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("src");
        let dst = root.join("dst");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"hello").unwrap();
        std::fs::write(src.join("sub/b.txt"), b"world").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("a.txt", src.join("link.txt")).unwrap();

        copy_tree(&src, &dst).await.unwrap();

        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(dst.join("sub/b.txt")).unwrap(), b"world");
        #[cfg(unix)]
        {
            let meta = std::fs::symlink_metadata(dst.join("link.txt")).unwrap();
            assert!(meta.file_type().is_symlink());
            assert_eq!(
                std::fs::read_link(dst.join("link.txt")).unwrap(),
                Path::new("a.txt")
            );
        }

        // 元 src を編集しても dst に影響しない（reflink(clonefile) の CoW・独立 inode、
        // または copy フォールバックの独立実体）。reflink 戦略（macOS/APFS 同 volume）を前提とする。
        std::fs::write(src.join("a.txt"), b"changed").unwrap();
        assert_eq!(
            std::fs::read(dst.join("a.txt")).unwrap(),
            b"hello",
            "editing source must not mutate the pack copy"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn copy_tree_merges_into_existing_destination() {
        // マージで sealed-dir と展開済み子エントリが同一 pack に混在した場合など、
        // `copy_tree` が既存在の dst ディレクトリへ配置される場合は EEXIST せず
        // 内容を再帰 merge することを検証する（EEXIST 回帰対策）。
        let root =
            std::env::temp_dir().join(format!("rsplug-copytree-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("src");
        let dst = root.join("dst");
        // dst は既に存在し、別プラグイン由来（展開済み子）のファイルを含む。
        std::fs::create_dir_all(dst.join("mstdn")).unwrap();
        std::fs::write(dst.join("edisch.vim"), b"edisch").unwrap();
        // src は異なるファイルを持つ sealed-dir。
        std::fs::create_dir_all(src.join("gin")).unwrap();
        std::fs::write(src.join("gin/util.vim"), b"gin").unwrap();
        std::fs::write(src.join("README.md"), b"gin-readme").unwrap();

        copy_tree(&src, &dst).await.unwrap();

        // dst は元のファイルと src のファイルの両方（union）を持つ。
        assert_eq!(std::fs::read(dst.join("edisch.vim")).unwrap(), b"edisch");
        assert!(dst.join("mstdn").is_dir());
        assert_eq!(std::fs::read(dst.join("gin/util.vim")).unwrap(), b"gin");
        assert_eq!(std::fs::read(dst.join("README.md")).unwrap(), b"gin-readme");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn dotgit_copies_git_dir_into_pack() {
        let dir = std::env::temp_dir().join(format!("rsplug-dotgit-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let snapshot_root = dir.join("snapshot");
        let packpath = dir.join("packpath");
        std::fs::create_dir_all(snapshot_root.join("plugin")).unwrap();
        std::fs::write(snapshot_root.join("plugin/init.lua"), "print('x')\n").unwrap();
        // snapshot に .git を用意（dotgit で通常エントリとして pack に copy される）。
        std::fs::create_dir_all(snapshot_root.join(".git/refs/heads")).unwrap();
        std::fs::write(snapshot_root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            snapshot_root.join(".git/refs/heads/main"),
            "0123456789012345678901234567890123456789\n",
        )
        .unwrap();

        let snapshot = RepoSnapshotIdentity::new(
            PathBuf::from("github.com/owner/repo"),
            b"0123456789012345678901234567890123456789".to_vec(),
            None,
            Arc::<[String]>::from([]),
            None,
        );
        // Plugin::load が dotgit=true で `.git` を通常 sealed-dir エントリとして加えた状態を再現。
        let root_source = Arc::new(FileSource::Directory {
            path: Arc::from(snapshot_root.clone()),
            inventory: None,
            handle: None,
        });
        let files = BTreeMap::from([
            (
                PathBuf::from(".git"),
                FileItem::new(
                    root_source.clone(),
                    FileIdentity::RepoFile(RepoFileIdentity::new(
                        snapshot.clone(),
                        PathBuf::from(".git"),
                    )),
                    MergeType::Conflict,
                ),
            ),
            (
                PathBuf::from("plugin/init.lua"),
                FileItem::new(
                    root_source,
                    FileIdentity::RepoFile(RepoFileIdentity::new(
                        snapshot,
                        PathBuf::from("plugin/init.lua"),
                    )),
                    MergeType::Conflict,
                ),
            ),
        ]);
        let loaded = LoadedPlugin {
            source_names: BTreeSet::new(),
            lazy_type: LazyType::Start,
            files: HowToPlaceFiles::CopyEachFile(files),
            script: SetupScript::default(),
            order: 0,
            merge_enabled: true,
            is_lazy_registration: false,
            dotgit: true,
        };
        let plugin_id = loaded.plugin_id();

        let mut state = PackPlan::new();
        state.insert(loaded);
        state.install(&packpath).await.unwrap();

        let git_dir = packpath
            .join("pack/_gen/opt")
            .join(plugin_id.as_str())
            .join(".git");
        assert!(git_dir.is_dir(), ".git must be copied into pack for dotgit");
        assert_eq!(
            std::fs::read(git_dir.join("HEAD")).unwrap(),
            b"ref: refs/heads/main\n"
        );
        assert_eq!(
            std::fs::read(git_dir.join("refs/heads/main")).unwrap(),
            b"0123456789012345678901234567890123456789\n"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn directory_entry_is_cloned_into_pack() {
        // read_dir 化（Plugin::load）を模倣し、ルート直下のディレクトリエントリ（lua）と
        // ファイルエントリ（init.lua）が混在する LoadedPlugin を install する。
        // ディレクトリは clone_dir で中身ごと copy、ファイルは yank されることを検証する。
        let dir = std::env::temp_dir().join(format!("rsplug-direntry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let snapshot_root = dir.join("snapshot");
        let packpath = dir.join("packpath");
        std::fs::create_dir_all(snapshot_root.join("lua/mymod")).unwrap();
        std::fs::write(snapshot_root.join("lua/mymod/init.lua"), "print('lua')\n").unwrap();
        std::fs::write(snapshot_root.join("init.lua"), "print('root')\n").unwrap();

        let snapshot = RepoSnapshotIdentity::new(
            PathBuf::from("github.com/owner/repo"),
            b"0123456789012345678901234567890123456789".to_vec(),
            None,
            Arc::<[String]>::from([]),
            None,
        );
        let root = snapshot_root.to_string_lossy().into_owned();
        let files = BTreeMap::from([
            repo_file(snapshot.clone(), "lua", &root),
            repo_file(snapshot, "init.lua", &root),
        ]);
        let loaded = synth(HowToPlaceFiles::CopyEachFile(files));
        let plugin_id = loaded.plugin_id();

        let mut state = PackPlan::new();
        state.insert(loaded);
        state.install(&packpath).await.unwrap();

        let pkg = packpath.join("pack/_gen/opt").join(plugin_id.as_str());
        // ディレクトリエントリ（lua）は clone_dir で中身ごと copy される
        assert_eq!(
            std::fs::read(pkg.join("lua/mymod/init.lua")).unwrap(),
            b"print('lua')\n",
            "directory entry must be cloned recursively"
        );
        // ファイルエントリ（init.lua）は yank される
        assert_eq!(
            std::fs::read(pkg.join("init.lua")).unwrap(),
            b"print('root')\n",
            "file entry must be yanked"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2a-2c: 同 path ディレクトリで子要素が重複しない → マージ成立。
    #[test]
    fn merge_disjoint_directory_children() {
        let (a, b, dir) = two_dir_plugins("disjoint", "a.lua", "b.lua");
        let (_merged, rest) = a + b;
        assert!(rest.is_none(), "disjoint directory children should merge");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Phase 2 回帰防止: manifest が存在しても filesystem と同じマージ結果（disjoint は成立）
    /// になること。manifest 駆動プローブが filesystem とずれないか検証する。
    #[tokio::test]
    async fn manifest_driven_disjoint_directory_children_merge() {
        let (a, b, dir) = two_dir_plugins("disjoint-mfst", "a.lua", "b.lua");
        for root in ["a", "b"] {
            SnapshotManifest::build_and_write(&dir.join(root), false, ".rsplug_build_success")
                .await
                .unwrap();
        }
        let (_merged, rest) = a + b;
        assert!(
            rest.is_none(),
            "manifest present: disjoint directory children should still merge"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Phase 1 回帰防止: opt プラグイン（merge_enabled=true, デフォルト）はマージすること。
    /// main では opt の merge_enabled ガードをスキップしていたが、Phase 1 で両方に適用した
    /// ことで merge=true の opt マージまで壊れていないか検証する。
    #[test]
    fn opt_plugins_with_default_merge_still_merge() {
        let (mut a, mut b, dir) = two_dir_plugins("opt-merge", "a.lua", "b.lua");
        a.lazy_type = LazyType::Opt(Default::default());
        b.lazy_type = LazyType::Opt(Default::default());
        let (_merged, rest) = a + b;
        assert!(rest.is_none(), "opt plugins with merge=true should merge");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Phase 6c データロス回帰テスト: 同 path の sealed-dir（`lua`）を子 disjoint で merge し、
    /// install 後に**両方の** repo の子が pack に届くことを検証する。
    /// 旧 `files.extend` は片側を上書きして消していた。
    #[tokio::test]
    async fn merge_union_directory_children_into_pack() {
        let dir = std::env::temp_dir().join(format!("rsplug-mergeunion-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let snap_a = dir.join("a");
        let snap_b = dir.join("b");
        std::fs::create_dir_all(snap_a.join("lua")).unwrap();
        std::fs::write(snap_a.join("lua/a.lua"), "-- a\n").unwrap();
        std::fs::create_dir_all(snap_b.join("lua")).unwrap();
        std::fs::write(snap_b.join("lua/b.lua"), "-- b\n").unwrap();

        let snap_a_id = RepoSnapshotIdentity::new(
            PathBuf::from("github.com/owner/a"),
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_vec(),
            None,
            Arc::<[String]>::from([]),
            None,
        );
        let snap_b_id = RepoSnapshotIdentity::new(
            PathBuf::from("github.com/owner/b"),
            b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_vec(),
            None,
            Arc::<[String]>::from([]),
            None,
        );
        let root_a = snap_a.to_string_lossy().into_owned();
        let root_b = snap_b.to_string_lossy().into_owned();
        let a = synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([repo_file(
            snap_a_id, "lua", &root_a,
        )])));
        let b = synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([repo_file(
            snap_b_id, "lua", &root_b,
        )])));
        let (merged, rest) = a + b;
        assert!(rest.is_none(), "disjoint lua children should merge");

        let packpath = dir.join("packpath");
        let plugin_id = merged.plugin_id();
        let mut state = PackPlan::new();
        state.insert(merged);
        state.install(&packpath).await.unwrap();

        let pkg = packpath.join("pack/_gen/opt").join(plugin_id.as_str());
        assert_eq!(
            std::fs::read(pkg.join("lua/a.lua")).unwrap(),
            b"-- a\n",
            "plugin A's lua child must land in pack after merge"
        );
        assert_eq!(
            std::fs::read(pkg.join("lua/b.lua")).unwrap(),
            b"-- b\n",
            "plugin B's lua child must land in pack after merge"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Phase 8: sealed-dir `X` と子孫 `X/...` が混在する map を正規化（sealed 側を展開）。
    /// 3+ プラグインのマージで生じる非推移的混在（EEXIST 原因）を解消する。
    #[test]
    fn normalize_sealed_expands_sealed_dir_coexisting_with_descendants() {
        let dir = std::env::temp_dir().join(format!("rsplug-normsealed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // snapA: autoload/gin/util.vim（sealed `autoload` として表現）
        std::fs::create_dir_all(dir.join("a/autoload/gin")).unwrap();
        std::fs::write(dir.join("a/autoload/gin/util.vim"), "gin\n").unwrap();
        // snapB: autoload/edisch.vim（展開済み子として表現）
        std::fs::create_dir_all(dir.join("b/autoload")).unwrap();
        std::fs::write(dir.join("b/autoload/edisch.vim"), "edisch\n").unwrap();

        let snap_a = snap(
            "github.com/owner/a",
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        let snap_b = snap(
            "github.com/owner/b",
            b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );
        let root_a = dir.join("a").to_string_lossy().into_owned();
        let root_b = dir.join("b").to_string_lossy().into_owned();

        // 非推移的マージ結果を模倣: sealed `autoload`(A) + 展開済み `autoload/edisch.vim`(B) が混在。
        let mut files = BTreeMap::from([
            repo_file(snap_a, "autoload", &root_a),
            repo_file(snap_b, "autoload/edisch.vim", &root_b),
        ]);

        normalize_sealed(&mut files);

        // sealed `autoload` は展開されて消え、子 `autoload/gin`(sealed) と `autoload/edisch.vim` になる。
        assert!(
            !files.contains_key(Path::new("autoload")),
            "sealed autoload must be expanded"
        );
        assert!(
            files.contains_key(Path::new("autoload/gin")),
            "autoload/gin (A's child) must remain"
        );
        assert!(
            files.contains_key(Path::new("autoload/edisch.vim")),
            "autoload/edisch.vim (B's descendant) must remain"
        );
        // 最終状態で sealed/子混在は無い。
        for (k, v) in &files {
            if v.kind() == FileKind::Directory {
                assert!(
                    !has_descendant(&files, k),
                    "no sealed dir with descendants after normalize: {k:?}"
                );
            }
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Phase 8: `steal_doc` が files から `doc/**` を抜き出し（Overwrite）、非 doc を残す。
    /// Phase 8: `split_doc` が self を `(rest, doc)` に分割。rest は doc/** 以外、doc は
    /// `_rsplug:doc` プラグイン（doc/** を Overwrite 保持）。doc 無しは None。
    #[test]
    fn split_doc_separates_doc_into_own_plugin() {
        let snapshot = snap(
            "github.com/owner/repo",
            b"0123456789012345678901234567890123456789",
        );
        let files = HowToPlaceFiles::CopyEachFile(BTreeMap::from([
            repo_file(snapshot.clone(), "doc/foo.txt", "/cache"),
            repo_file(snapshot.clone(), "doc/sub/bar.txt", "/cache"),
            repo_file(snapshot, "lua/x.lua", "/cache"),
        ]));
        let loaded = synth(files);

        let (rest, doc) = loaded.split_doc();

        // rest は doc/** 以外のみ。
        let HowToPlaceFiles::CopyEachFile(rest_files) = &rest.files;
        assert_eq!(rest_files.len(), 1, "only lua/x.lua remains in rest");
        assert!(rest_files.contains_key(Path::new("lua/x.lua")));

        // doc は `_rsplug:doc` プラグイン（is_lazy_registration, Start 相当）で doc/** を Overwrite 保持。
        let doc = doc.expect("doc plugin must be Some when doc/** present");
        assert_eq!(
            doc.source_names,
            BTreeSet::from(["_rsplug:doc".to_string()])
        );
        assert!(
            doc.is_lazy_registration,
            "doc plugin must be a control package"
        );
        let HowToPlaceFiles::CopyEachFile(doc_files) = &doc.files;
        assert_eq!(doc_files.len(), 2, "doc/foo.txt and doc/sub/bar.txt");
        assert!(doc_files.contains_key(Path::new("doc/foo.txt")));
        assert!(doc_files.contains_key(Path::new("doc/sub/bar.txt")));
        for v in doc_files.values() {
            assert_eq!(
                v.merge_type,
                MergeType::Overwrite,
                "stolen doc must be Overwrite"
            );
        }
    }

    /// 2a-2c: 同 path ディレクトリで子要素が重複（両方ファイル・Conflict）→ 非マージ。
    #[test]
    fn merge_overlapping_directory_children() {
        let (a, b, dir) = two_dir_plugins("overlap", "x.lua", "x.lua");
        let (_a, rest) = a + b;
        assert!(rest.is_some(), "overlapping children should not merge");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2b: 同 path で片方ディレクトリ・片方ファイル → 種別違いで非マージ。
    #[test]
    fn merge_directory_vs_file() {
        let dir = std::env::temp_dir().join(format!("rsplug-mergedirfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let snap_a = dir.join("a");
        let snap_b = dir.join("b");
        // a: lua/ ディレクトリ
        std::fs::create_dir_all(snap_a.join("lua")).unwrap();
        // b: lua ファイル（snap_b を作ってから置く）
        std::fs::create_dir_all(&snap_b).unwrap();
        std::fs::write(snap_b.join("lua"), "not a dir\n").unwrap();

        let snapshot = RepoSnapshotIdentity::new(
            PathBuf::from("github.com/owner/repo"),
            b"0123456789012345678901234567890123456789".to_vec(),
            None,
            Arc::<[String]>::from([]),
            None,
        );
        let root_a = snap_a.to_string_lossy().into_owned();
        let root_b = snap_b.to_string_lossy().into_owned();
        let a = synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([repo_file(
            snapshot.clone(),
            "lua",
            &root_a,
        )])));
        let b = synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([repo_file(
            snapshot, "lua", &root_b,
        )])));

        let (_a, rest) = a + b;
        assert!(
            rest.is_some(),
            "directory vs file at same path should not merge"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// merge 2a-2c テスト用: 各 snapshot の `lua/<child>` を持つ2つの LoadedPlugin を組む。
    fn two_dir_plugins(
        tag: &str,
        child_a: &str,
        child_b: &str,
    ) -> (LoadedPlugin, LoadedPlugin, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("rsplug-mergedir-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let snap_a = dir.join("a");
        let snap_b = dir.join("b");
        std::fs::create_dir_all(snap_a.join("lua")).unwrap();
        std::fs::write(snap_a.join("lua").join(child_a), "").unwrap();
        std::fs::create_dir_all(snap_b.join("lua")).unwrap();
        std::fs::write(snap_b.join("lua").join(child_b), "").unwrap();

        let snapshot = RepoSnapshotIdentity::new(
            PathBuf::from("github.com/owner/repo"),
            b"0123456789012345678901234567890123456789".to_vec(),
            None,
            Arc::<[String]>::from([]),
            None,
        );
        let root_a = snap_a.to_string_lossy().into_owned();
        let root_b = snap_b.to_string_lossy().into_owned();
        let a = synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([repo_file(
            snapshot.clone(),
            "lua",
            &root_a,
        )])));
        let b = synth(HowToPlaceFiles::CopyEachFile(BTreeMap::from([repo_file(
            snapshot, "lua", &root_b,
        )])));
        (a, b, dir)
    }

    // === Atomic generation publication（staging + 原子 publish） ===

    /// 実スナップショットディレクトリ `src_root` から1ファイルの LoadedPlugin を作る。
    fn one_file_plugin(repo: &str, rev: &[u8], rel: &str, src_root: &Path) -> LoadedPlugin {
        let snapshot = RepoSnapshotIdentity::new(
            PathBuf::from(repo),
            rev.to_vec(),
            None,
            Arc::<[String]>::from([]),
            None,
        );
        let files = BTreeMap::from([(
            PathBuf::from(rel),
            FileItem::new(
                Arc::new(FileSource::Directory {
                    path: Arc::from(src_root.to_path_buf()),
                    inventory: None,
                    handle: None,
                }),
                FileIdentity::RepoFile(RepoFileIdentity::new(snapshot, PathBuf::from(rel))),
                MergeType::Conflict,
            ),
        )]);
        synth(HowToPlaceFiles::CopyEachFile(files))
    }

    /// `pack/_gen/` 配下に `.staging-*` が残っていないか。
    fn no_staging_dirs(gen_root: &Path) -> bool {
        let Ok(mut rd) = std::fs::read_dir(gen_root) else {
            return true;
        };
        while let Some(Ok(e)) = rd.next() {
            if let Some(n) = e.file_name().to_str()
                && n.starts_with(".staging-")
            {
                return false;
            }
        }
        true
    }

    /// 同一パッケージの2回目 install は既存を再利用し（copy skip）、staging も残さない。
    #[tokio::test]
    async fn install_reuses_existing_package_and_leaves_no_staging() {
        let dir = tempfile::tempdir().unwrap();
        let packpath = dir.path().to_path_buf();
        let genpath = packpath.join("pack/_gen");
        let snap_root = dir.path().join("snap");
        std::fs::create_dir_all(snap_root.join("plugin")).unwrap();
        std::fs::write(snap_root.join("plugin/a.lua"), b"-- a\n").unwrap();

        let plugin = one_file_plugin("github.com/owner/a", b"rev-a", "plugin/a.lua", &snap_root);
        let id = plugin.plugin_id().as_str().to_string();

        // 1回目: publish。
        let mut state = PackPlan::new();
        state.insert(plugin);
        state.install(&packpath).await.unwrap();
        let opt_a = packpath
            .join("pack/_gen/opt")
            .join(&id)
            .join("plugin/a.lua");
        assert_eq!(std::fs::read(&opt_a).unwrap(), b"-- a\n");

        // snapshot ソースを書き換えても、2回目は既存 opt/<id> を再利用（copy skip）するので
        // 公開内容は不変。staging にも残骸が残らない。
        std::fs::write(snap_root.join("plugin/a.lua"), b"-- CHANGED\n").unwrap();
        let plugin2 = one_file_plugin("github.com/owner/a", b"rev-a", "plugin/a.lua", &snap_root);
        let mut state2 = PackPlan::new();
        state2.insert(plugin2);
        state2.install(&packpath).await.unwrap();

        assert_eq!(
            std::fs::read(&opt_a).unwrap(),
            b"-- a\n",
            "reuse must not recopy the changed snapshot"
        );
        assert!(no_staging_dirs(&genpath), "no staging dirs must remain");
    }

    #[tokio::test]
    async fn concurrent_identical_publications_have_one_winner() {
        let _perf = crate::rsplug::perf::PerfGuard::install();
        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path().join("snapshot");
        tokio::fs::create_dir_all(snapshot.join("plugin"))
            .await
            .unwrap();
        tokio::fs::write(
            snapshot.join("plugin/init.lua"),
            b"vim.g.concurrent = true\n",
        )
        .await
        .unwrap();
        let packpath = dir.path().join("packpath");

        let mut first = PackPlan::new();
        first.insert(one_file_plugin(
            "github.com/example/concurrent",
            b"rev-concurrent",
            "plugin/init.lua",
            &snapshot,
        ));
        let mut second = PackPlan::new();
        second.insert(one_file_plugin(
            "github.com/example/concurrent",
            b"rev-concurrent",
            "plugin/init.lua",
            &snapshot,
        ));
        let (left, right) = tokio::join!(first.install(&packpath), second.install(&packpath));
        assert!(left.is_ok() && right.is_ok());
        assert_eq!(left.unwrap() as u8 + right.unwrap() as u8, 1);
        assert!(packpath.join("init.lua").exists());
        assert!(no_staging_dirs(&packpath.join("pack/_gen")));
        assert!(
            crate::rsplug::perf::PerfGuard::count(
                crate::rsplug::perf::PerfOp::PublicationLockWaitMicros
            ) > 0
        );
        assert!(
            crate::rsplug::perf::PerfGuard::count(
                crate::rsplug::perf::PerfOp::PublicationLockHoldMicros
            ) > 0
        );
    }

    #[tokio::test]
    async fn concurrent_different_publications_leave_complete_generations() {
        let dir = tempfile::tempdir().unwrap();
        let first_snapshot = dir.path().join("first-snapshot");
        let second_snapshot = dir.path().join("second-snapshot");
        tokio::fs::create_dir_all(first_snapshot.join("plugin"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(second_snapshot.join("plugin"))
            .await
            .unwrap();
        tokio::fs::write(first_snapshot.join("plugin/first.lua"), b"-- first\n")
            .await
            .unwrap();
        tokio::fs::write(second_snapshot.join("plugin/second.lua"), b"-- second\n")
            .await
            .unwrap();
        let packpath = dir.path().join("packpath");

        let mut first = PackPlan::new();
        first.insert(one_file_plugin(
            "github.com/example/first",
            b"rev-first",
            "plugin/first.lua",
            &first_snapshot,
        ));
        let mut second = PackPlan::new();
        second.insert(one_file_plugin(
            "github.com/example/second",
            b"rev-second",
            "plugin/second.lua",
            &second_snapshot,
        ));
        let (left, right) = tokio::join!(first.install(&packpath), second.install(&packpath));
        assert!(left.is_ok() && right.is_ok());
        assert!(packpath.join("init.lua").exists());
        let mut opt_entries = tokio::fs::read_dir(packpath.join("pack/_gen/opt"))
            .await
            .unwrap();
        assert!(opt_entries.next_entry().await.unwrap().is_some());
        assert!(
            tokio::fs::read_dir(packpath.join("generations"))
                .await
                .unwrap()
                .next_entry()
                .await
                .unwrap()
                .is_some()
        );
        assert!(no_staging_dirs(&packpath.join("pack/_gen")));
    }

    #[tokio::test]
    async fn install_skips_publication_when_generation_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let packpath = dir.path().to_path_buf();
        let snap_root = dir.path().join("snap");
        std::fs::create_dir_all(snap_root.join("plugin")).unwrap();
        std::fs::write(snap_root.join("plugin/a.lua"), b"-- a\n").unwrap();

        let mut plugin =
            one_file_plugin("github.com/owner/a", b"rev-a", "plugin/a.lua", &snap_root);
        plugin.lazy_type = LazyType::Opt(BTreeSet::from([LoadEvent::Autocmd(
            "BufEnter".parse().unwrap(),
        )]));

        let mut first = PackPlan::new();
        first.insert(plugin);
        assert!(first.install(&packpath).await.unwrap());

        let mut plugin =
            one_file_plugin("github.com/owner/a", b"rev-a", "plugin/a.lua", &snap_root);
        plugin.lazy_type = LazyType::Opt(BTreeSet::from([LoadEvent::Autocmd(
            "BufEnter".parse().unwrap(),
        )]));
        let mut second = PackPlan::new();
        second.insert(plugin);
        let _perf = crate::rsplug::perf::PerfGuard::install();
        assert!(
            !second.install(&packpath).await.unwrap(),
            "an identical generation must not be republished"
        );
        let operations = crate::rsplug::perf::PerfGuard::snapshot();
        for operation in [
            "package_copy",
            "generation_manifest_write",
            "init_lua_swap",
            "ft_index_scan",
            "helptags_process",
            "gc_delete",
            "retention_manifest_read",
        ] {
            assert!(
                !operations.iter().any(|(name, _)| *name == operation),
                "unchanged generation performed {operation}: {operations:?}"
            );
        }
    }

    #[tokio::test]
    async fn empty_generation_is_a_true_noop_on_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let first = PackPlan::new().install(dir.path()).await.unwrap();
        assert!(first);
        let init = dir.path().join("init.lua");
        let before = tokio::fs::metadata(&init)
            .await
            .unwrap()
            .modified()
            .unwrap();
        let second = PackPlan::new().install(dir.path()).await.unwrap();
        assert!(!second);
        assert_eq!(
            tokio::fs::metadata(&init)
                .await
                .unwrap()
                .modified()
                .unwrap(),
            before
        );
        assert!(!dir.path().join("pack/_gen/.staging-none").exists());
    }

    /// install 入口で前回クラッシュの `.staging-*` 残骸が掃除される。
    #[tokio::test]
    async fn install_cleans_stale_staging_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let packpath = dir.path().to_path_buf();
        let genpath = packpath.join("pack/_gen");
        std::fs::create_dir_all(genpath.join(".staging-leftover/opt")).unwrap();
        std::fs::write(genpath.join(".staging-leftover/opt/junk"), b"x").unwrap();
        // An explicitly dead owner is reclaimable immediately; an unleased
        // recent directory is intentionally preserved by the stale cleaner.
        std::fs::write(genpath.join(".staging-leftover/.lease"), b"999999999").unwrap();

        // プラグインが空でも install 入口の staging 掃除は走る。
        let state = PackPlan::new();
        state.install(&packpath).await.unwrap();

        assert!(
            !genpath.join(".staging-leftover").exists(),
            "stale staging must be cleaned at install entry"
        );
        assert!(no_staging_dirs(&genpath));
    }

    /// 別パッケージの copy 失敗で install が Err になっても、公開済み世代は破壊されない。
    #[tokio::test]
    async fn install_failure_preserves_published_generation() {
        let dir = tempfile::tempdir().unwrap();
        let packpath = dir.path().to_path_buf();
        let snap_a = dir.path().join("snapA");
        std::fs::create_dir_all(snap_a.join("plugin")).unwrap();
        std::fs::write(snap_a.join("plugin/a.lua"), b"-- a\n").unwrap();

        // 1回目: A を publish しておく。
        let plugin_a = one_file_plugin("github.com/owner/a", b"rev-a", "plugin/a.lua", &snap_a);
        let id_a = plugin_a.plugin_id().as_str().to_string();
        let mut state = PackPlan::new();
        state.insert(plugin_a);
        state.install(&packpath).await.unwrap();
        let opt_a = packpath.join("pack/_gen/opt").join(&id_a);
        assert!(opt_a.is_dir(), "A must be published");

        // 2回目: B を staged copy するがコピー元を削除し ENOENT 失敗を起こす。
        let snap_b = dir.path().join("snapB");
        std::fs::create_dir_all(snap_b.join("plugin")).unwrap();
        std::fs::write(snap_b.join("plugin/b.lua"), b"-- b\n").unwrap();
        let plugin_b = one_file_plugin("github.com/owner/b", b"rev-b", "plugin/b.lua", &snap_b);
        std::fs::remove_file(snap_b.join("plugin/b.lua")).unwrap();

        let mut state2 = PackPlan::new();
        state2.insert(plugin_b);
        let result = state2.install(&packpath).await;
        assert!(result.is_err(), "copy failure must fail install");

        // 公開済みの A は破壊されず残る（publication 失敗が公開ツリーを壊さない）。
        assert!(
            opt_a.is_dir(),
            "published generation A must survive a failed B install"
        );
        assert_eq!(
            std::fs::read(opt_a.join("plugin/a.lua")).unwrap(),
            b"-- a\n"
        );
    }

    #[tokio::test]
    async fn publication_failpoints_leave_a_bootable_generation() {
        const STAGES: &[&str] = &[
            "package_rename_before",
            "package_rename_after",
            "generation_metadata_before",
            "generation_metadata_after",
            "pointer_swap_before",
            "pointer_swap_after",
        ];

        for stage in STAGES {
            let _perf = crate::rsplug::perf::PerfGuard::install();
            let dir = tempfile::tempdir().unwrap();
            let old_snapshot = dir.path().join("old-snapshot");
            tokio::fs::create_dir_all(old_snapshot.join("plugin"))
                .await
                .unwrap();
            tokio::fs::write(old_snapshot.join("plugin/old.lua"), b"-- old\n")
                .await
                .unwrap();
            let mut old = PackPlan::new();
            old.insert(one_file_plugin(
                "github.com/example/old",
                b"old-rev",
                "plugin/old.lua",
                &old_snapshot,
            ));
            assert!(old.install(dir.path()).await.unwrap());
            let init_before = tokio::fs::read_link(dir.path().join("init.lua"))
                .await
                .unwrap();

            let new_snapshot = dir.path().join("new-snapshot");
            tokio::fs::create_dir_all(new_snapshot.join("plugin"))
                .await
                .unwrap();
            tokio::fs::write(new_snapshot.join("plugin/new.lua"), b"-- new\n")
                .await
                .unwrap();
            let mut new = PackPlan::new();
            new.insert(one_file_plugin(
                "github.com/example/new",
                b"new-rev",
                "plugin/new.lua",
                &new_snapshot,
            ));

            crate::rsplug::perf::arm_failpoint(stage);
            let result = new.install(dir.path()).await;
            crate::rsplug::perf::disarm_failpoint(stage);
            assert!(result.is_err(), "{stage} must fail the publication");

            let init = dir.path().join("init.lua");
            let target = tokio::fs::read_link(&init).await.unwrap();
            let target = if target.is_absolute() {
                target
            } else {
                init.parent().unwrap().join(target)
            };
            assert!(
                tokio::fs::symlink_metadata(&target).await.is_ok(),
                "{stage} left a dangling init.lua"
            );
            if stage != &"pointer_swap_after" {
                assert_eq!(target, dir.path().join(init_before));
            }
        }
    }
}
