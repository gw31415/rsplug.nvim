use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::BinaryHeap,
    ffi::OsString,
    io,
    ops::Add,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::log::{Message, msg};
use adaptive_semaphore::AdaptiveSemaphore;
use hashbrown::{HashMap, HashSet};
use tokio::task::JoinSet;

use super::*;

/// プラグインファイルの配置方法。
// TODO: HowToPlaceFilesをenum { Root, Tree(HashMap<PathBuf, FileItem>) }にする。SymlinkDirectoryはFileItemに含める
// その方がマージもでき、SymlinkなPluginのdocにも対応できるため。
#[derive(Debug)]
pub(super) enum HowToPlaceFiles {
    CopyEachFile(HashMap<PathBuf, FileItem>),
    SymlinkDirectory(Arc<Path>),
}

impl Default for HowToPlaceFiles {
    fn default() -> Self {
        HowToPlaceFiles::CopyEachFile(HashMap::new())
    }
}

/// インストール単位となるプラグイン。
/// NOTE: 遅延実行されるプラグイン等は、インストール後に PlugCtl が生成される。PlugCtlはまとめて
/// PluginLoadedに変換する。
#[derive(Debug)]
pub struct LoadedPlugin {
    /// ID
    pub(super) id: PluginID,
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

#[derive(Debug)]
pub(super) struct FileItem {
    pub source: Arc<FileSource>,
    pub merge_type: MergeType,
}

impl PartialEq for LoadedPlugin {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for LoadedPlugin {}

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
            .then_with(|| self.id.cmp(&other.id))
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

            loop {
                groups.sort_by(Self::deterministic_cmp);
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
        if self.id.0.is_superset(&rhs.id.0) {
            return (self, None);
        } else if rhs.id.0.is_superset(&self.id.0) {
            return (rhs, None);
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
                        mut id,
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
                        id: rid,
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
                    id += rid;
                    script += rscript;
                    let order = order.min(r_order);

                    return (
                        Self {
                            id,
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
            (HowToPlaceFiles::CopyEachFile(_), HowToPlaceFiles::SymlinkDirectory(_))
            | (HowToPlaceFiles::SymlinkDirectory(_), HowToPlaceFiles::CopyEachFile(_))
            | (HowToPlaceFiles::SymlinkDirectory(_), HowToPlaceFiles::SymlinkDirectory(_)) => {}
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
    start_or_opt: &'static str,
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
        let LoadedPlugin {
            id,
            lazy_type,
            mut files,
            script,
            order,
            merge_enabled: _,
            is_plugctl,
        } = loaded_plugin;

        let id_str = id.as_str();
        let already_installed = !self.installing.insert(id_str.clone().into());
        if already_installed {
            return;
        }
        let start_or_opt = if is_plugctl { "start" } else { "opt" };

        if !is_plugctl {
            self.ctl += PlugCtl::create(id, lazy_type, script, order, &mut files);
        }
        match files {
            HowToPlaceFiles::CopyEachFile(files) => {
                for (path, item) in files {
                    let Files {
                        start_or_opt: _,
                        dir_type: DirectoryExtractionType::Files(tree),
                    } = self.files.entry(id_str.clone()).or_insert(Files {
                        start_or_opt,
                        dir_type: DirectoryExtractionType::Files(Vec::new()),
                    })
                    else {
                        unreachable!() // SAFETY: idは一意なので、ここに到達することはない
                    };
                    tree.push((path, item.source));
                }
            }
            HowToPlaceFiles::SymlinkDirectory(dir) => {
                self.files.insert(
                    id_str.clone(),
                    Files {
                        start_or_opt,
                        dir_type: DirectoryExtractionType::Symlink(dir),
                    },
                );
            }
        }
    }

    /// PackPathState を指定されたパスにインストールする。パスは Vim の 'packpath' に基づく。
    /// NOTE: インストール後のディレクトリ構成は以下のようになる。
    /// {packpath}/pack/_gen/{start_or_opt}/{id}/
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
        tokio::fs::write(
            packpath.join("init.lua"),
            include_bytes!("../../../templates/init.lua"),
        )
        .await?;
        let Self {
            installing: _,
            files,
            ctl: _,
        } = self;
        let installing_entries: HashSet<Box<[u8]>> = files
            .iter()
            .map(|(id, files)| {
                let mut key = Vec::with_capacity(files.start_or_opt.len() + 1 + id.len());
                key.extend_from_slice(files.start_or_opt.as_bytes());
                key.push(b'/');
                key.extend_from_slice(id.as_bytes());
                key.into_boxed_slice()
            })
            .collect();
        let mut tasks = JoinSet::new();
        let yank_semaphore = AdaptiveSemaphore::new();
        let symlink_semaphore = AdaptiveSemaphore::new();
        let cleanup_semaphore = AdaptiveSemaphore::new();

        for (
            id,
            Files {
                start_or_opt,
                dir_type,
            },
        ) in files
        {
            let id: Arc<str> = id.into();
            let dir = gen_root.join(start_or_opt).join(id.as_ref());
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
                        if start_or_opt == "start" {
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

        let installing_entries = Arc::new(installing_entries);
        for start_or_opt in ["start", "opt"] {
            let path = gen_root.join(start_or_opt);
            if let Ok(mut read_dir) = tokio::fs::read_dir(path).await {
                while let Some(entry) = read_dir.next_entry().await? {
                    let installing_entries = installing_entries.clone();
                    let cleanup_semaphore = cleanup_semaphore.clone();
                    let start_or_opt = start_or_opt.as_bytes().to_vec();
                    tasks.spawn(async move {
                        let file_name = os_string_to_install_key(entry.file_name());
                        let mut entry_key =
                            Vec::with_capacity(start_or_opt.len() + 1 + file_name.len());
                        entry_key.extend_from_slice(&start_or_opt);
                        entry_key.push(b'/');
                        entry_key.extend_from_slice(&file_name);
                        let not_installed_entry =
                            !installing_entries.contains(entry_key.as_slice());
                        let path = entry.path();
                        if not_installed_entry && path.is_dir() {
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

        let res = tasks
            .join_all()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .and(Ok(()));
        msg(Message::InstallDone);
        res
    }
}
