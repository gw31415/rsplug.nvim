use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::{BTreeMap, BinaryHeap},
    ffi::OsString,
    hash::{Hash, Hasher},
    io,
    ops::Add,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use crate::log::{Message, msg};
use adaptive_semaphore::AdaptiveSemaphore;
use hashbrown::{HashMap, HashSet};
use sailfish::TemplateSimple;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use super::*;

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
    /// `dirty_diff`・`build`・`lua_build` が全て無ければ `<head_rev>` のみ。
    /// いずれかがあれば `<head_rev>__v1_<input_hash>`。`schema` byte を hash 入力に
    /// 含めることで、key の意味を変える将来の変更に対して別 prefix で migration できる。
    /// `repo_cache_dir` は key に含めない（`worktrees/` は repo ごとに分かれているため暗黙）。
    pub(super) fn snapshot_key(&self) -> String {
        let input = SnapshotKeyInput {
            schema: SNAPSHOT_KEY_SCHEMA,
            head_rev: &self.head_rev,
            dirty_diff: self.dirty_diff,
            build: &self.build,
            lua_build: self.lua_build.as_deref(),
        };
        let head_rev = String::from_utf8_lossy(&self.head_rev);
        if self.dirty_diff.is_none() && self.build.is_empty() && self.lua_build.is_none() {
            head_rev.into_owned()
        } else {
            format!(
                "{head_rev}__v{}_{}",
                SNAPSHOT_KEY_SCHEMA,
                crate::rsplug::util::hash::digest_hash_hex_string(&input)
            )
        }
    }
}

/// `snapshot_key` の hash 入力 (PLANS §7)。絶対パスは含めない。
#[derive(Hash)]
struct SnapshotKeyInput<'a> {
    schema: u8,
    head_rev: &'a [u8],
    dirty_diff: Option<[u8; 16]>,
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

/// プラグインファイルの配置方法。
#[derive(Debug)]
pub(super) enum HowToPlaceFiles {
    CopyEachFile(BTreeMap<PathBuf, FileItem>),
    /// Git repo snapshot への symlink。`target` は配置用の絶対 runtime path、
    /// `identity` が論理 identity。下記 `Hash`/`PartialEq`/`Eq` は `target` を除外し
    /// `identity` のみで同一性を決める（cache root が違っても同じ snapshot なら同じ id）。
    RepoSnapshotLink {
        target: Arc<Path>,
        identity: RepoSnapshotIdentity,
    },
}

impl Default for HowToPlaceFiles {
    fn default() -> Self {
        HowToPlaceFiles::CopyEachFile(BTreeMap::new())
    }
}

impl PartialEq for HowToPlaceFiles {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::CopyEachFile(a), Self::CopyEachFile(b)) => a == b,
            (
                Self::RepoSnapshotLink { identity: la, .. },
                Self::RepoSnapshotLink { identity: lb, .. },
            ) => la == lb,
            _ => false,
        }
    }
}

impl Eq for HowToPlaceFiles {}

impl Hash for HowToPlaceFiles {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::CopyEachFile(map) => {
                0u8.hash(state);
                map.hash(state);
            }
            Self::RepoSnapshotLink { identity, .. } => {
                1u8.hash(state);
                identity.hash(state);
            }
        }
    }
}

/// インストール単位となるプラグイン。
/// NOTE: 遅延実行されるプラグイン等は、インストール後に PlugCtl が生成される。PlugCtlはまとめて
/// PluginLoadedに変換する。
#[derive(Debug, Hash, PartialEq, Eq)]
pub struct LoadedPlugin {
    /// `on_source` から参照される設定上の名前
    pub(super) source_name: Option<String>,
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
    /// PlugCtlを元に作成されたかどうか
    pub(super) is_plugctl: bool,
}

impl LoadedPlugin {
    /// 全フィールドの [`Hash`] から [`PluginID`] を導出する。
    /// フィールド追加・変更は自動的に PluginID に反映される。
    pub fn plugin_id(&self) -> PluginID {
        <Self as HasPluginId>::plugin_id(self)
    }

    /// 配置（runtime）用の snapshot root。repo 由来でなければ（script-only や生成ファイルのみ
    /// なら）`None`。**配置情報であり `plugin_id` の hash には含まれない**。依存元 plugin の
    /// build 用 runtimepath 解決に使う (PLANS §10.3)。
    pub fn snapshot_root(&self) -> Option<Arc<Path>> {
        match &self.files {
            HowToPlaceFiles::RepoSnapshotLink { target, .. } => Some(target.clone()),
            HowToPlaceFiles::CopyEachFile(files) => {
                files
                    .values()
                    .next()
                    .and_then(|item| match item.source.as_ref() {
                        FileSource::Directory { path } => Some(path.clone()),
                        FileSource::File { .. } => None,
                    })
            }
        }
    }
}

#[derive(Debug, Hash, PartialEq, Eq)]
pub(super) struct FileItem {
    pub source: Arc<FileSource>,
    /// ファイルの論理 identity。絶対配置パスは含まず、repo 由来か生成かで決まる。
    pub identity: FileIdentity,
    pub merge_type: MergeType,
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

impl LoadedPlugin {
    fn deterministic_cmp(&self, other: &Self) -> Ordering {
        self.lazy_type
            .cmp(&other.lazy_type)
            .then_with(|| self.order.cmp(&other.order))
            .then_with(|| self.is_plugctl.cmp(&other.is_plugctl))
            .then_with(|| self.plugin_id().cmp(&other.plugin_id()))
    }

    /// BinaryHeap に保存された PluginLoaded 群を可能な範囲でマージする。
    ///
    /// BinaryHeap の pop 順に基づいて貪欲にマージすると、同順位要素や
    /// マージ後に id/order が変化する要素によって、マージの組み合わせが
    /// 実行ごとに変わり得る。いったん決定的な順序に並べ、同じ順序で
    /// first-fit の fixed point を作ることで、マージパターンを一意にする。
    pub fn merge(plugs: &mut BinaryHeap<Self>) {
        let mut items = Vec::with_capacity(plugs.len());
        while let Some(plug) = plugs.pop() {
            items.push(plug);
        }
        items.sort_by(Self::deterministic_cmp);

        let mut groups: Vec<Self> = Vec::with_capacity(items.len());
        for item in items {
            let mut pending = item;
            groups.sort_by(Self::deterministic_cmp);

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

        groups.sort_by(Self::deterministic_cmp);
        plugs.extend(groups);
    }
}

impl Add for LoadedPlugin {
    type Output = (Self, Option<Self>);
    fn add(self, rhs: Self) -> Self::Output {
        if self.lazy_type != rhs.lazy_type {
            return (self, Some(rhs));
        }
        if self.lazy_type.is_start()
            && (self.is_plugctl != rhs.is_plugctl || !(self.merge_enabled && rhs.merge_enabled))
        {
            return (self, Some(rhs));
        }
        match (&self.files, &rhs.files) {
            (HowToPlaceFiles::CopyEachFile(files), HowToPlaceFiles::CopyEachFile(rfiles)) => {
                let mergeable = {
                    let (small, large) = if files.len() <= rfiles.len() {
                        (files, rfiles)
                    } else {
                        (rfiles, files)
                    };
                    // 重複ファイルの検出
                    small.iter().all(|(path, item)| {
                        let Some(other) = large.get(path) else {
                            return true;
                        };
                        // 重複ファイルがあった場合
                        let a = &item.merge_type;
                        let b = &other.merge_type;
                        !matches!((a, b), (MergeType::Conflict, _) | (_, MergeType::Conflict))
                    })
                };
                if mergeable {
                    let Self {
                        source_name,
                        lazy_type,
                        files: HowToPlaceFiles::CopyEachFile(mut files),
                        mut script,
                        order,
                        merge_enabled,
                        is_plugctl,
                    } = self
                    else {
                        unreachable!() // SAFETY: Because self.files is verified to be a CopyEachFile
                    };
                    let Self {
                        source_name: _,
                        lazy_type: _,
                        files: HowToPlaceFiles::CopyEachFile(rfiles),
                        script: rscript,
                        order: r_order,
                        merge_enabled: _,
                        is_plugctl: r_is_plugctl,
                    } = rhs
                    else {
                        unreachable!() // SAFETY: Because rhs.files is verified to be a CopyEachFile
                    };
                    files.extend(rfiles);
                    script += rscript;
                    let order = order.min(r_order);

                    return (
                        Self {
                            source_name,
                            lazy_type,
                            files: HowToPlaceFiles::CopyEachFile(files),
                            script,
                            order,
                            merge_enabled,
                            is_plugctl: is_plugctl || r_is_plugctl,
                        },
                        None,
                    );
                }
            }
            (HowToPlaceFiles::CopyEachFile(_), HowToPlaceFiles::RepoSnapshotLink { .. })
            | (HowToPlaceFiles::RepoSnapshotLink { .. }, HowToPlaceFiles::CopyEachFile(_))
            | (
                HowToPlaceFiles::RepoSnapshotLink { .. },
                HowToPlaceFiles::RepoSnapshotLink { .. },
            ) => {}
        };
        (self, Some(rhs))
    }
}

/// ファイルの取得(生成)元。
#[derive(Debug)]
pub(super) enum FileSource {
    Directory { path: Arc<Path> },
    File { data: Cow<'static, [u8]> },
}

impl PartialEq for FileSource {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Directory { path: l }, Self::Directory { path: r }) => l == r,
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
    /// whichfile が install_dir からの相対パスとなるようにデータを配置する。
    async fn yank(
        &self,
        whichfile: impl AsRef<Path>,
        install_dir: impl AsRef<Path>,
    ) -> io::Result<()> {
        async fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
            tokio::fs::create_dir_all(to.as_ref().parent().unwrap()).await?;
            #[cfg(target_os = "macos")]
            tokio::fs::copy(from, to).await?;
            #[cfg(not(target_os = "macos"))]
            tokio::fs::hard_link(from, to).await?;
            Ok(())
        }

        use FileSource::*;
        match self {
            Directory { path } => {
                let from = path.join(&whichfile);
                let to = install_dir.as_ref().join(&whichfile);
                copy(from, to).await
            }
            File { data } => {
                let path = install_dir.as_ref().join(whichfile);
                tokio::fs::create_dir_all(path.parent().unwrap()).await?;
                tokio::fs::write(path, data).await?;
                Ok(())
            }
        }
    }
}

struct Files {
    is_plugctl: bool,
    dir_type: DirectoryExtractionType,
}

enum DirectoryExtractionType {
    Files(Vec<(PathBuf, Arc<FileSource>)>),
    Symlink(Arc<Path>),
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
async fn symlink_plugin_dir(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    tokio::fs::symlink(original, link).await
}

#[cfg(windows)]
async fn symlink_plugin_dir(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    tokio::fs::symlink_dir(original, link).await
}

#[cfg(unix)]
async fn symlink_file(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    tokio::fs::symlink(original, link).await
}

#[cfg(windows)]
async fn symlink_file(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    tokio::fs::symlink_file(original, link).await
}

const RETAIN_GENERATIONS: usize = 3;

#[derive(Serialize, Deserialize)]
struct GenerationManifest {
    version: u8,
    entries: Vec<String>,
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

fn manifest_entries(manifest: &GenerationManifest) -> HashSet<Box<[u8]>> {
    manifest
        .entries
        .iter()
        .map(|entry| entry.as_bytes().to_vec().into_boxed_slice())
        .collect()
}

async fn retained_manifest_entries(
    gen_root: &Path,
    current_control_ids: &[PluginIDStr],
    current_manifest: &GenerationManifest,
) -> io::Result<HashSet<Box<[u8]>>> {
    let mut manifests = Vec::new();
    let current_control_set: HashSet<Box<[u8]>> = current_control_ids
        .iter()
        .map(|id| id.as_bytes().to_vec().into_boxed_slice())
        .collect();
    let opt_root = gen_root.join("opt");
    if let Ok(mut read_dir) = tokio::fs::read_dir(opt_root).await {
        while let Some(entry) = read_dir.next_entry().await? {
            let path = entry.path().join("manifest.json");
            if !path.is_file() {
                continue;
            }
            let modified = tokio::fs::metadata(&path)
                .await
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            manifests.push((modified, entry.file_name(), path));
        }
    }
    manifests.sort_by(|(l_time, l_name, _), (r_time, r_name, _)| {
        r_time.cmp(l_time).then_with(|| r_name.cmp(l_name))
    });

    let mut retained_entries = manifest_entries(current_manifest);
    let mut retained_count = current_control_ids.len();
    for (_, control_id, path) in manifests {
        if current_control_set.contains(control_id.as_encoded_bytes()) {
            continue;
        }
        if retained_count >= RETAIN_GENERATIONS {
            break;
        }
        if let Ok(content) = tokio::fs::read(&path).await
            && let Ok(manifest) = serde_json::from_slice::<GenerationManifest>(&content)
        {
            retained_entries.extend(manifest_entries(&manifest));
            retained_count += 1;
        }
    }
    Ok(retained_entries)
}

/// PackPath の象徴となる状態。この構造体に PluginLoaded をインサートしていき、最後に実際のパスを指定して install を行う。
#[derive(Default)]
pub struct PackPathState {
    installing: HashSet<Box<[u8]>>,
    files: HashMap<PluginIDStr, Files>,
    ctl: PlugCtl,
}

impl PackPathState {
    pub fn len(&self) -> usize {
        self.installing.len()
    }
    /// 空の PackPathState を生成する。
    pub fn new() -> Self {
        Default::default()
    }
    /// PluginLoaded をインサートする。その PluginLoaded の実行制御や設定に必要な PlugCtl を返す。
    pub fn insert(&mut self, loaded_plugin: LoadedPlugin) {
        let id = loaded_plugin.plugin_id();
        let id_str = id.as_str();
        let already_installed = !self.installing.insert(id_str.clone().into());
        if already_installed {
            return;
        }

        let LoadedPlugin {
            source_name,
            lazy_type,
            mut files,
            script,
            order,
            merge_enabled: _,
            is_plugctl,
        } = loaded_plugin;

        if !is_plugctl {
            self.ctl += PlugCtl::create(id, source_name, lazy_type, script, order, &mut files);
        }
        match files {
            HowToPlaceFiles::CopyEachFile(files) => {
                for (path, item) in files {
                    let Files {
                        is_plugctl: _,
                        dir_type: DirectoryExtractionType::Files(tree),
                    } = self.files.entry(id_str.clone()).or_insert(Files {
                        is_plugctl,
                        dir_type: DirectoryExtractionType::Files(Vec::new()),
                    })
                    else {
                        unreachable!() // SAFETY: idは一意なので、ここに到達することはない
                    };
                    tree.push((path, item.source));
                }
            }
            HowToPlaceFiles::RepoSnapshotLink { target, .. } => {
                self.files.insert(
                    id_str.clone(),
                    Files {
                        is_plugctl,
                        dir_type: DirectoryExtractionType::Symlink(target),
                    },
                );
            }
        }
    }

    /// PackPathState を指定されたパスにインストールする。パスは Vim の 'packpath' に基づく。
    /// NOTE: インストール後のディレクトリ構成は以下のようになる。
    /// {packpath}/pack/_gen/opt/{id}/
    pub async fn install(mut self, packpath: &Path) -> io::Result<()> {
        {
            // Load PlugCtl
            let plugins = {
                let plugins: Vec<LoadedPlugin> = std::mem::take(&mut self.ctl).into();
                let mut plugins: BinaryHeap<_> = plugins.into();
                LoadedPlugin::merge(&mut plugins);
                plugins
            };
            for plugin in plugins {
                self.insert(plugin);
            }
        }
        let gen_root = packpath.join("pack").join("_gen");
        tokio::fs::create_dir_all(&gen_root).await?;
        let Self {
            installing: _,
            files,
            ctl: _,
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
        let manifest = GenerationManifest {
            version: 1,
            entries: generation_entries,
        };
        let manifest_content = serde_json::to_vec_pretty(&manifest).map_err(io::Error::other)?;
        let mut control_ids: Vec<PluginIDStr> = files
            .iter()
            .filter(|(_, files)| files.is_plugctl)
            .map(|(id, _)| id.clone())
            .collect();
        control_ids.sort();
        let init_content = render_init(&control_ids);
        let mut tasks = JoinSet::new();
        let yank_semaphore = AdaptiveSemaphore::new();
        let symlink_semaphore = AdaptiveSemaphore::new();

        for (
            id,
            Files {
                is_plugctl: _,
                dir_type,
            },
        ) in files
        {
            let id: Arc<str> = id.into();
            let dir = gen_root.join("opt").join(id.as_ref());
            let installed = {
                let dir_is_symlink = dir.is_symlink();
                match &dir_type {
                    DirectoryExtractionType::Files(_) => dir.is_dir() && !dir_is_symlink,
                    DirectoryExtractionType::Symlink(_) => dir_is_symlink,
                }
            };
            if installed {
                msg(Message::InstallSkipped(id));
            } else {
                let helptags = {
                    // NOTE: make helptags closure FnOnce forcely.
                    // Because multiple asynchronous starts do not work properly
                    let nvim = tokio::process::Command::new("nvim");
                    async move |dir: &Path| -> io::Result<()> {
                        let mut nvim = nvim;
                        let help_dir = dir.join("doc/");
                        if help_dir.is_dir() {
                            let cmd = format!("helptags {}", help_dir.to_string_lossy());
                            msg(Message::InstallHelp { help_dir });
                            nvim.arg("--headless")
                                .arg("-u")
                                .arg("NONE")
                                .arg("-c")
                                // TODO: escape help_dir properly
                                .arg(&cmd)
                                .arg("-c")
                                .arg("q")
                                .status()
                                .await
                                .and_then(|code| {
                                    if code.success() {
                                        Ok(())
                                    } else {
                                        Err(io::Error::other(format!(
                                            "Failed to run nvim command: {}",
                                            cmd
                                        )))
                                    }
                                })?;
                        }
                        Ok(())
                    }
                };
                match dir_type {
                    DirectoryExtractionType::Files(files) => {
                        tokio::fs::remove_file(dir.as_path()).await.ok();
                        let dir = Arc::new(dir);
                        // パッケージ単位でも JoinSet に載せ、複数パッケージのコピーを直列化しない。
                        let yank_semaphore = yank_semaphore.clone();
                        tasks.spawn(async move {
                            let mut copies = files
                                .into_iter()
                                .map(|(which, source)| {
                                    let dir = dir.clone();
                                    let id = id.clone();
                                    let yank_semaphore = yank_semaphore.clone();
                                    async move {
                                        let permit = yank_semaphore.acquire().await;
                                        let result = source.yank(&which, dir.as_path()).await;
                                        let is_error = result.is_err();
                                        permit.finish(is_error);
                                        result?;
                                        msg(Message::InstallYank { id, which });
                                        Ok::<_, io::Error>(())
                                    }
                                })
                                .collect::<JoinSet<_>>();
                            while let Some(res) = copies.join_next().await {
                                res??;
                            }
                            helptags(dir.as_path()).await
                        });
                    }
                    DirectoryExtractionType::Symlink(sym) => {
                        let symlink_semaphore = symlink_semaphore.clone();
                        tasks.spawn(async move {
                            let permit = symlink_semaphore.acquire().await;
                            let result = async {
                                tokio::fs::remove_dir_all(&dir).await.ok();
                                tokio::fs::create_dir_all(dir.parent().unwrap()).await?;
                                symlink_plugin_dir(sym, dir.as_path()).await?;
                                Ok::<_, io::Error>(())
                            }
                            .await;
                            let is_error = result.is_err();
                            permit.finish(is_error);
                            result?;
                            // Avoid mutating symlink source; helptags are generated
                            // from PlugCtl's copied doc files instead.
                            Ok(())
                        });
                    }
                }
            }
        }

        tasks
            .join_all()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;

        for id in &control_ids {
            let manifest_path = gen_root
                .join("opt")
                .join(<PluginIDStr as AsRef<Path>>::as_ref(id))
                .join("manifest.json");
            tokio::fs::create_dir_all(manifest_path.parent().unwrap()).await?;
            tokio::fs::write(manifest_path, &manifest_content).await?;
        }
        let generations_dir = packpath.join("generations");
        tokio::fs::create_dir_all(&generations_dir).await?;
        if control_ids.is_empty() {
            // ponytail: no control package to anchor a generation file; fall back to a plain init.lua.
            tokio::fs::remove_file(packpath.join("init.lua")).await.ok();
            tokio::fs::write(packpath.join("init.lua"), &init_content).await?;
        } else {
            // Each generation's loader lives at generations/<control_id>.lua; init.lua is a
            // pure symlink to it, so older retained generations stay addressable by name.
            let gen_path = generations_dir
                .join(<PluginIDStr as AsRef<Path>>::as_ref(&control_ids[0]))
                .with_extension("lua");
            tokio::fs::write(&gen_path, &init_content).await?;
            let init_path = packpath.join("init.lua");
            tokio::fs::remove_file(&init_path).await.ok();
            symlink_file(&gen_path, &init_path).await?;
        }

        let retained_entries =
            retained_manifest_entries(&gen_root, &control_ids, &manifest).await?;

        let retained_entries = Arc::new(retained_entries);
        let cleanup_semaphore = AdaptiveSemaphore::new();
        let mut cleanup_tasks = JoinSet::new();
        for start_or_opt in ["start", "opt"] {
            let path = gen_root.join(start_or_opt);
            let start_or_opt_key: Arc<[u8]> = Arc::from(start_or_opt.as_bytes());
            if let Ok(mut read_dir) = tokio::fs::read_dir(path).await {
                while let Some(entry) = read_dir.next_entry().await? {
                    let retained_entries = retained_entries.clone();
                    let cleanup_semaphore = cleanup_semaphore.clone();
                    let start_or_opt_key = Arc::clone(&start_or_opt_key);
                    cleanup_tasks.spawn(async move {
                        let file_name = os_string_to_install_key(entry.file_name());
                        let mut entry_key =
                            Vec::with_capacity(start_or_opt_key.len() + 1 + file_name.len());
                        entry_key.extend_from_slice(&start_or_opt_key);
                        entry_key.push(b'/');
                        entry_key.extend_from_slice(&file_name);
                        let not_retained_entry = !retained_entries.contains(entry_key.as_slice());
                        let path = entry.path();
                        if not_retained_entry && path.is_dir() {
                            let permit = cleanup_semaphore.acquire().await;
                            let result = tokio::fs::remove_dir_all(path).await;
                            let is_error = result.is_err();
                            permit.finish(is_error);
                            result?;
                        }
                        Ok(())
                    });
                }
            }
        }

        let res = cleanup_tasks
            .join_all()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .and(Ok(()));
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
                if !gen_root.join("opt").join(id).is_dir() {
                    tokio::fs::remove_file(&path).await.ok();
                }
            }
        }
        msg(Message::InstallDone);
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_template_packadds_control_packages() {
        let control_id = b"control-package".plugin_id().as_str();
        let script = String::from_utf8(render_init(std::slice::from_ref(&control_id))).unwrap();

        assert!(script.contains(&format!("vim.cmd.packadd '{control_id}'")));
        assert!(!script.contains("packloadall"));
    }

    #[test]
    fn init_template_emits_exact_packadd_block() {
        let a = b"aaaa".plugin_id().as_str();
        let b = b"bbbb".plugin_id().as_str();
        let script = String::from_utf8(render_init(&[a.clone(), b.clone()])).unwrap();
        // ponytail: locks in the exact packadd block shape; break whitespace here if the template changes.
        let actual = script
            .split("vim.opt.packpath:prepend(root)\n\n")
            .nth(1)
            .unwrap();
        let expected = format!("vim.cmd.packadd '{a}'\nvim.cmd.packadd '{b}'\n\nlocal ok, rsplug");
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
    fn manifest_entries_are_parsed_from_json_model() {
        let manifest = GenerationManifest {
            version: 1,
            entries: vec![
                "opt/22222222222222222222222222222222".to_string(),
                "opt/11111111111111111111111111111111".to_string(),
            ],
        };
        let parsed = manifest_entries(&manifest);

        assert!(parsed.contains("opt/22222222222222222222222222222222".as_bytes()));
        assert!(parsed.contains("opt/11111111111111111111111111111111".as_bytes()));
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
            FileItem {
                source: Arc::new(FileSource::Directory {
                    path: Arc::from(PathBuf::from(dir)),
                }),
                identity: FileIdentity::RepoFile(RepoFileIdentity::new(
                    snapshot,
                    PathBuf::from(rel),
                )),
                merge_type: MergeType::Conflict,
            },
        )
    }

    fn synth(files: HowToPlaceFiles) -> LoadedPlugin {
        LoadedPlugin {
            source_name: None,
            lazy_type: LazyType::Start,
            files,
            script: SetupScript::default(),
            order: 0,
            merge_enabled: true,
            is_plugctl: false,
        }
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
    fn snapshot_link_id_is_independent_of_absolute_target() {
        let identity = snap(
            "github.com/owner/repo",
            b"0123456789012345678901234567890123456789",
        );
        let make = |target: &str| {
            synth(HowToPlaceFiles::RepoSnapshotLink {
                target: Arc::from(PathBuf::from(target)),
                identity: identity.clone(),
            })
            .plugin_id()
        };
        assert_eq!(make("/A/repos/owner/repo"), make("/B/repos/owner/repo"));

        // 同一 identity で target だけ違うなら == でも等しい（identity のみで同一性を判定する）
        let a = synth(HowToPlaceFiles::RepoSnapshotLink {
            target: Arc::from(PathBuf::from("/A/r")),
            identity: identity.clone(),
        });
        let b = synth(HowToPlaceFiles::RepoSnapshotLink {
            target: Arc::from(PathBuf::from("/B/r")),
            identity,
        });
        assert_eq!(a, b);
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
        LoadedPlugin::merge(&mut heap);

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
    fn snapshot_key_reflects_dirty_diff_and_lua_build() {
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
        let base = mk(None, None);
        let with_dirty = mk(Some([1u8; 16]), None);
        let with_lua = mk(None, Some(Arc::from("vim.cmd('x')")));
        assert_ne!(base.snapshot_key(), with_dirty.snapshot_key());
        assert_ne!(base.snapshot_key(), with_lua.snapshot_key());
        // dirty と lua が両方違っても互いに違う
        assert_ne!(with_dirty.snapshot_key(), with_lua.snapshot_key());
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
                FileItem {
                    source: Arc::new(FileSource::File {
                        data: Cow::Borrowed(data),
                    }),
                    identity: FileIdentity::GeneratedFile {
                        path: PathBuf::from(path),
                        data_hash,
                    },
                    merge_type: MergeType::Overwrite,
                },
            )])))
            .plugin_id()
        };
        assert_ne!(make("plugin/a.lua", b"x"), make("plugin/b.lua", b"x")); // path 違い
        assert_ne!(make("plugin/a.lua", b"x"), make("plugin/a.lua", b"y")); // data 違い
        assert_eq!(make("plugin/a.lua", b"x"), make("plugin/a.lua", b"x")); // 同一
    }
}
