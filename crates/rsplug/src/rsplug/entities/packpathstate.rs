use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::{BTreeMap, BinaryHeap},
    ffi::OsString,
    hash::{Hash, Hasher},
    io,
    ops::Add,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
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
    /// pack に `.git` を複製するか（git 利用プラグイン用）。`dotgit=true` かつ repo 由来なら
    /// snapshot の `.git` を `pack/_gen/opt/<id>/.git` に copy する。
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
                FileSource::Directory { path } => Some(path.clone()),
                FileSource::File { .. } => None,
            })
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
                let mergeable = dirs_mergeable(files, rfiles);
                if mergeable {
                    let Self {
                        source_name,
                        lazy_type,
                        files: HowToPlaceFiles::CopyEachFile(mut files),
                        mut script,
                        order,
                        merge_enabled,
                        is_plugctl,
                        dotgit,
                    } = self;
                    let Self {
                        source_name: _,
                        lazy_type: _,
                        files: HowToPlaceFiles::CopyEachFile(rfiles),
                        script: rscript,
                        order: r_order,
                        merge_enabled: _,
                        is_plugctl: r_is_plugctl,
                        dotgit: r_dotgit,
                    } = rhs;
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
    let (Some(x_root), Some(y_root)) = (
        snapshot_root_of(&item.source),
        snapshot_root_of(&other.source),
    ) else {
        // FileSource::File 同士（GeneratedFile 等）は merge_type で判定。
        return !matches!(
            (&item.merge_type, &other.merge_type),
            (MergeType::Conflict, _) | (_, MergeType::Conflict)
        );
    };
    let x_dir = x_root.join(path).is_dir();
    let y_dir = y_root.join(path).is_dir();
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
    let x_children = read_dir_children(&x_root.join(path));
    if x_children.is_empty() {
        return true;
    }
    let y_children = read_dir_children(&y_root.join(path));
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
        FileSource::Directory { path } => Some(path),
        FileSource::File { .. } => None,
    }
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
            {
                tokio::fs::copy(from, to).await?;
            }
            #[cfg(not(target_os = "macos"))]
            {
                // hard_link は同一ファイルシステムのみ。別FS（Nix store 等）へ配置すると
                // ExDev (errno 18) で失敗するため、そのときは copy にフォールバックする。
                // copy はディスクを消費するが、pack を自己完結させる（sym 廃止）前提では必須。
                const EXDEV: i32 = 18;
                if let Err(e) = tokio::fs::hard_link(from.as_ref(), to.as_ref()).await {
                    if e.raw_os_error() == Some(EXDEV) {
                        tokio::fs::copy(from.as_ref(), to.as_ref()).await?;
                    } else {
                        return Err(e);
                    }
                }
            }
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
    /// ディレクトリエントリ: 同 path に複数 source（マージされたディレクトリ）。
    /// install で各 source を順次 copy（1個目 `clone_dir`・2個目以降 `merge_copy_dir`）。
    dirs: Vec<(PathBuf, Vec<Arc<FileSource>>)>,
    /// ファイルエントリ: `source.yank` で copy。GeneratedFile 含む。
    files: Vec<(PathBuf, Arc<FileSource>)>,
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

/// macOS APFS の `clonefile(2)` でディレクトリ階層全体を1 syscall・CoW で clone する。
/// ファイル数に比例した syscall を削減できる（`.git` の object store 等で効果大）。
/// 非 APFS・別 volume・カーネル未対応では失敗し、呼び出し元が `copy_dir_all` にフォールバックする。
/// dst は未存在・親は存在が前提（`clonefile` が dst を新規作成する）。
#[cfg(target_os = "macos")]
async fn clonefile_dir(src: &Path, dst: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int};
    use std::os::unix::ffi::OsStrExt;
    unsafe extern "C" {
        fn clonefile(src: *const c_char, dst: *const c_char, flags: u32) -> c_int;
    }
    let src = src.to_path_buf();
    let dst = dst.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let s = CString::new(src.as_os_str().as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let d = CString::new(dst.as_os_str().as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        // SAFETY: `s`/`d` は有効な NUL 終端パス。`flags=0` はデフォルト挙動（CoW clone）。
        let ret = unsafe { clonefile(s.as_ptr(), d.as_ptr(), 0) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
    .await
    .map_err(|e| io::Error::other(format!("clonefile join failed: {e}")))?
}

/// `clonefile` が「この環境では使えない」エラーか（フォールバック対象）。
/// EXDEV=別 volume, ENOTSUP=非APFS/属性未対応, ENOSYS=未実装, EOPNOTSUPP=未サポート。
/// いずれかなら以降のディレクトリ copy を再帰 copy に固定する。
#[cfg(target_os = "macos")]
fn clonefile_unsupported(e: &io::Error) -> bool {
    const EXDEV: i32 = 18;
    const ENOTSUP: i32 = 45;
    const ENOSYS: i32 = 78;
    const EOPNOTSUPP: i32 = 102;
    matches!(
        e.raw_os_error(),
        Some(EXDEV) | Some(ENOTSUP) | Some(ENOSYS) | Some(EOPNOTSUPP)
    )
}

/// `clonefile` が効くか（APFS・同 volume）を実行時にキャッシュ。
/// 一度でも unsupported エラーなら false に固定し、無駄な syscall を避ける。
#[cfg(target_os = "macos")]
static CLONEFILE_AVAILABLE: AtomicBool = AtomicBool::new(true);

/// ディレクトリを copy する。macOS かつ APFS 同 volume なら `clonefile(2)` で
/// ディレクトリ全体を1 syscall・CoW で clone し、ファイル数分の syscall を削減する
/// （CoW かつ独立 inode なので、元 snapshot を編集しても pack に影響しない）。
/// clonefile 非対応環境（非 APFS・別 volume・非 macOS）では `copy_dir_all`（再帰 copy）にフォールバックする。
async fn clone_dir(src: &Path, dst: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    if CLONEFILE_AVAILABLE.load(AtomicOrdering::Relaxed) {
        // dst の親を作成（dst 自体は clonefile が新規作成するので未存在のまま）。
        if let Some(parent) = dst.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        match clonefile_dir(src, dst).await {
            Ok(()) => return Ok(()),
            Err(e) if clonefile_unsupported(&e) => {
                CLONEFILE_AVAILABLE.store(false, AtomicOrdering::Relaxed);
                // フォールバック: dst は未作成のまま copy_dir_all へ（先頭で create_dir_all する）。
            }
            Err(e) => return Err(e),
        }
    }
    copy_dir_all(src, dst).await
}

/// ディレクトリを再帰的に copy（ファイル・ディレクトリ・symlink を保持）。
/// `clone_dir` のフォールバック先、および非 macOS 既定のディレクトリ copy。
async fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
    tokio::fs::create_dir_all(dst).await?;
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
        } else if meta.is_symlink() {
            let target = tokio::fs::read_link(&s).await?;
            #[cfg(unix)]
            tokio::fs::symlink(&target, &d).await?;
            // Windows では symlink 作成に権限が必要なためファイル copy にフォールバック
            #[cfg(not(unix))]
            {
                tokio::fs::copy(&s, &d).await?;
            }
        } else {
            tokio::fs::copy(&s, &d).await?;
        }
    }
    Ok(())
}

/// マージされたディレクトリを統合 copy。dst は既存（clone_dir 済み）の前提で、
/// src の各エントリを dst に上書きする。dirs_mergeable で子競合無しなので、
/// 上書きされるのは「片方にだけ存在するエントリ」のみ。ファイル・ディレクトリ・symlink を再帰。
async fn merge_copy_dir(src: &Path, dst: &Path) -> io::Result<()> {
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
        } else if meta.is_symlink() {
            let target = tokio::fs::read_link(&s).await?;
            #[cfg(unix)]
            {
                // dst が既存（ファイル/symlink）なら削除してから再作成（ディレクトリは残す）。
                let _ = tokio::fs::remove_file(&d).await;
                tokio::fs::symlink(&target, &d).await?;
            }
            #[cfg(not(unix))]
            {
                tokio::fs::copy(&s, &d).await?;
            }
        } else {
            tokio::fs::copy(&s, &d).await?;
        }
    }
    Ok(())
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
            dotgit,
        } = loaded_plugin;

        if !is_plugctl {
            self.ctl += PlugCtl::create(id, source_name, lazy_type, script, order, &mut files);
        }
        match files {
            HowToPlaceFiles::CopyEachFile(files) => {
                for (path, item) in files {
                    let entry = self.files.entry(id_str.clone()).or_insert(Files {
                        is_plugctl,
                        dirs: Vec::new(),
                        files: Vec::new(),
                        dotgit,
                    });
                    // 同 id に複数 LoadedPlugin が統合される場合、最初にエントリを作った
                    // LoadedPlugin の is_plugctl/dotgit が or_insert で固定されるのを防ぐため、
                    // 既存エントリのフラグを update する（どれか1つでも true なら true）。
                    entry.is_plugctl = entry.is_plugctl || is_plugctl;
                    entry.dotgit = entry.dotgit || dotgit;
                    // ディレクトリエントリ（snapshot_root/path がディレクトリ）は dirs、
                    // ファイルエントリは files に振り分け。install で dirs→files 順に copy する。
                    let is_dir = match item.source.as_ref() {
                        FileSource::Directory { path: root } => std::fs::metadata(root.join(&path))
                            .map(|m| m.is_dir())
                            .unwrap_or(false),
                        FileSource::File { .. } => false,
                    };
                    if is_dir {
                        // 同 path のディレクトリエントリ（マージされた複数 source）は source を追加。
                        if let Some(slot) = entry.dirs.iter_mut().find(|(p, _)| p == &path) {
                            slot.1.push(item.source);
                        } else {
                            entry.dirs.push((path, vec![item.source]));
                        }
                    } else {
                        entry.files.push((path, item.source));
                    }
                }
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

        for (
            id,
            Files {
                is_plugctl: _,
                dirs,
                files,
                dotgit,
            },
        ) in files
        {
            let id: Arc<str> = id.into();
            let dir = gen_root.join("opt").join(id.as_ref());
            let installed = dir.is_dir() && !dir.is_symlink();
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
                {
                    // clone_dir は dst 未存在前提なので残骸を掃除（ディレクトリ/ファイル混在に備え remove_dir_all）。
                    tokio::fs::remove_dir_all(dir.as_path()).await.ok();
                    // dotgit=true なら .git 複製元の snapshot_root を事前取得（dirs/files は into_iter で消費するため）
                    let snapshot_root = if dotgit {
                        dirs.iter()
                            .flat_map(|(_, srcs)| srcs.iter())
                            .chain(files.iter().map(|(_, s)| s))
                            .find_map(|s| match s.as_ref() {
                                FileSource::Directory { path } => Some(path.clone()),
                                _ => None,
                            })
                    } else {
                        None
                    };
                    if dotgit
                        && snapshot_root
                            .as_ref()
                            .is_none_or(|root| !root.join(".git").is_dir())
                    {
                        // dotgit copy cannot be faked from a snapshot without `.git`;
                        // skip the pack install and let the user refresh the cache with `-u`.
                        msg(Message::PluginDotgitMissing(id.clone()));
                        continue;
                    }
                    let dir = Arc::new(dir);
                    // パッケージ単位でも JoinSet に載せ、複数パッケージのコピーを直列化しない。
                    let yank_semaphore = yank_semaphore.clone();
                    tasks.spawn(async move {
                        // 1. ディレクトリエントリを先に clone_dir / merge_copy_dir。
                        //    clone_dir が後の yank ファイルを上書きしないよう、ディレクトリを先に配置する。
                        for (which, sources) in &dirs {
                            let dst = dir.join(which);
                            for (i, source) in sources.iter().enumerate() {
                                if let FileSource::Directory { path: root } = source.as_ref() {
                                    let src = root.join(which);
                                    if i == 0 {
                                        clone_dir(&src, &dst).await?;
                                    } else {
                                        // マージされた2個目以降: dst が既存なので中身を統合 copy。
                                        merge_copy_dir(&src, &dst).await?;
                                    }
                                }
                            }
                            msg(Message::InstallYank {
                                id: id.clone(),
                                which: which.clone(),
                            });
                        }
                        // 2. ファイルエントリを yank（GeneratedFile 含む）。
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
                        // 3. dotgit=true なら snapshot の .git を pack に全体 copy（git 利用プラグイン用）。
                        //    clone_dir は macOS/APFS 同 volume で clonefile(2) により .git 全体を
                        //    1 syscall・CoW で配置する（object store 等のファイル数分の syscall を削減）。
                        if let Some(root) = snapshot_root {
                            let src_git = root.join(".git");
                            if src_git.is_dir() {
                                clone_dir(&src_git, &dir.join(".git")).await?;
                            }
                        }
                        helptags(dir.as_path()).await
                    });
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

        // The control id is emitted into the ids table and looped over with vim.cmd.packadd(id).
        assert!(
            script.contains(&format!("'{control_id}'")),
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
            "local requested = vim.env.RSPLUG_GENERATION\nlocal ids = {{ '{a}','{b}', }}\n"
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
            dotgit: false,
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
            FileItem {
                source: Arc::new(FileSource::Directory {
                    path: Arc::from(snapshot_root.clone()),
                }),
                identity: FileIdentity::RepoFile(RepoFileIdentity::new(
                    snapshot,
                    PathBuf::from("plugin/init.lua"),
                )),
                merge_type: MergeType::Conflict,
            },
        )]);
        let loaded = LoadedPlugin {
            source_name: None,
            lazy_type: LazyType::Start,
            files: HowToPlaceFiles::CopyEachFile(files),
            script: SetupScript::default(),
            order: 0,
            merge_enabled: true,
            is_plugctl: false,
            dotgit: true,
        };
        let plugin_id = loaded.plugin_id();

        let mut state = PackPathState::new();
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
    async fn clone_dir_preserves_files_dirs_and_symlinks() {
        let root = std::env::temp_dir().join(format!("rsplug-clonedir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("src");
        let dst = root.join("dst");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"hello").unwrap();
        std::fs::write(src.join("sub/b.txt"), b"world").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("a.txt", src.join("link.txt")).unwrap();

        clone_dir(&src, &dst).await.unwrap();

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

        // 元 src を編集しても dst に影響しない（clonefile の CoW・独立 inode、
        // または copy フォールバックの独立実体。hardlink で inode を共有しない）。
        std::fs::write(src.join("a.txt"), b"changed").unwrap();
        assert_eq!(
            std::fs::read(dst.join("a.txt")).unwrap(),
            b"hello",
            "editing source must not mutate the pack copy"
        );

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
        // snapshot に .git を用意（dotgit copy の対象）。clone_dir が全体 copy する。
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
        let files = BTreeMap::from([(
            PathBuf::from("plugin/init.lua"),
            FileItem {
                source: Arc::new(FileSource::Directory {
                    path: Arc::from(snapshot_root.clone()),
                }),
                identity: FileIdentity::RepoFile(RepoFileIdentity::new(
                    snapshot,
                    PathBuf::from("plugin/init.lua"),
                )),
                merge_type: MergeType::Conflict,
            },
        )]);
        let loaded = LoadedPlugin {
            source_name: None,
            lazy_type: LazyType::Start,
            files: HowToPlaceFiles::CopyEachFile(files),
            script: SetupScript::default(),
            order: 0,
            merge_enabled: true,
            is_plugctl: false,
            dotgit: true,
        };
        let plugin_id = loaded.plugin_id();

        let mut state = PackPathState::new();
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

        let mut state = PackPathState::new();
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
}
