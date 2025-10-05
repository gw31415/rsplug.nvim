use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::BinaryHeap,
    io,
    ops::Add,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::log::{Message, msg};
use hashbrown::{HashMap, HashSet};
use tokio::task::JoinSet;

use super::*;

/// プラグインファイルの配置方法。
pub(super) enum HowToPlaceFiles {
    CopyEachFile(HashMap<PathBuf, FileItem>),
    SymlinkDirectory(PathBuf),
}

/// インストール単位となるプラグイン。
/// NOTE: 遅延実行されるプラグイン等は、インストール後に Loader が生成される。Loaderはまとめて
/// Packageに変換する。
pub struct Package {
    /// ID
    pub(super) id: PackageID,
    /// プラグインの遅延実行タイプ
    pub lazy_type: LazyType,
    /// 配置するファイル
    pub(super) files: HowToPlaceFiles,
    /// セットアップスクリプト
    pub(super) script: SetupScript,
}

pub(super) struct FileItem {
    pub source: Arc<FileSource>,
    pub merge_type: MergeType,
}

impl PartialEq for Package {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Package {}

impl PartialOrd for Package {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Package {
    fn cmp(&self, other: &Self) -> Ordering {
        let cmp = self.lazy_type.cmp(&other.lazy_type);
        if let Ordering::Equal = cmp {
            return self.id.cmp(&other.id);
        }
        cmp
    }
}

impl Package {
    /// BinaryHeap に保存された Package 群を可能な範囲でマージする
    pub fn merge(pkgs: &mut BinaryHeap<Self>) {
        let mut done_items = Vec::new();

        while pkgs.len() > 1 {
            let (tail, tail2) = (pkgs.pop().unwrap(), pkgs.pop().unwrap());
            match tail + tail2 {
                (tail, Some(tail2)) => {
                    done_items.push(tail);
                    pkgs.push(tail2);
                }
                (tail, None) => {
                    pkgs.push(tail);
                }
            }
        }

        pkgs.extend(done_items);
    }
}

impl Add for Package {
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
        match (&self.files, &rhs.files) {
            (HowToPlaceFiles::CopyEachFile(files), HowToPlaceFiles::CopyEachFile(rfiles)) => {
                let mergeable = {
                    let (sfname, rfname): (HashSet<_>, HashSet<_>) =
                        (files.keys().collect(), rfiles.keys().collect());
                    sfname.intersection(&rfname).all(|path| {
                        let a = &files.get(*path).unwrap().merge_type;
                        let b = &rfiles.get(*path).unwrap().merge_type;
                        !matches!((a, b), (MergeType::Conflict, _) | (_, MergeType::Conflict))
                    })
                };
                if mergeable {
                    let Self {
                        mut id,
                        lazy_type,
                        files: HowToPlaceFiles::CopyEachFile(mut files),
                        mut script,
                    } = self
                    else {
                        unreachable!() // SAFETY: Because self.files is verified to be a CopyEachFile
                    };
                    let Self {
                        id: rid,
                        lazy_type: _,
                        files: HowToPlaceFiles::CopyEachFile(rfiles),
                        script: rscript,
                    } = rhs
                    else {
                        unreachable!() // SAFETY: Because rhs.files is verified to be a CopyEachFile
                    };
                    files.extend(rfiles);
                    id += rid;
                    script += rscript;

                    return (
                        Self {
                            id,
                            lazy_type,
                            files: HowToPlaceFiles::CopyEachFile(files),
                            script,
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

struct PackageFiles {
    start_or_opt: &'static str,
    dir: PackageDirectory,
}

enum PackageDirectory {
    Files(Vec<(PathBuf, Arc<FileSource>)>),
    Symlink(PathBuf),
}

/// PackPath の象徴となる状態。この構造体に Package をインサートしていき、最後に実際のパスを指定して install を行う。
#[derive(Default)]
pub struct PackPathState {
    installing: HashSet<Box<[u8]>>,
    files: HashMap<PackageIDStr, PackageFiles>,
}

impl PackPathState {
    pub fn len(&self) -> usize {
        self.installing.len()
    }
    /// 空の PackPathState を生成する。
    pub fn new() -> Self {
        Default::default()
    }
    /// Package をインサートする。その Package の実行制御や設定に必要な Loader を返す。
    pub fn insert(&mut self, pkg: Package) -> Loader {
        let Package {
            id,
            lazy_type,
            files,
            script,
        } = pkg;

        let already_installed = !self.installing.insert(id.as_str().into());
        if already_installed {
            return Default::default();
        }

        let pkg_type_str = if lazy_type.is_start() { "start" } else { "opt" };
        match files {
            HowToPlaceFiles::CopyEachFile(files) => {
                for (path, item) in files {
                    let PackageFiles {
                        start_or_opt: _,
                        dir: PackageDirectory::Files(tree),
                    } = self.files.entry(id.as_str()).or_insert(PackageFiles {
                        start_or_opt: pkg_type_str,
                        dir: PackageDirectory::Files(Vec::new()),
                    })
                    else {
                        unreachable!() // SAFETY: idは一意なので、ここに到達することはない
                    };
                    tree.push((path, item.source));
                }
            }
            HowToPlaceFiles::SymlinkDirectory(dir) => {
                self.files.insert(
                    id.as_str(),
                    PackageFiles {
                        start_or_opt: pkg_type_str,
                        dir: PackageDirectory::Symlink(dir),
                    },
                );
            }
        }

        Loader::create(id, lazy_type, script)
    }

    /// PackPathState を指定されたパスにインストールする。パスは Vim の 'packpath' に基づく。
    /// NOTE: インストール後のディレクトリ構成は以下のようになる。
    /// {packpath}/pack/_gen/{start_or_opt}/{id}/
    pub async fn install(self, packpath: &Path) -> io::Result<()> {
        let gen_root = packpath.join("pack").join("_gen");
        tokio::fs::create_dir_all(&gen_root).await?;
        let Self { installing, files } = self;
        let mut tasks = JoinSet::new();

        for (
            id,
            PackageFiles {
                start_or_opt,
                dir: pkgdir,
            },
        ) in files
        {
            let id: Arc<str> = id.into();
            let dir = gen_root.join(start_or_opt).join(id.as_ref());
            if dir.is_dir() {
                msg(Message::InstallSkipped(id));
            } else {
                match pkgdir {
                    PackageDirectory::Files(files) => {
                        let dir = Arc::new(dir);
                        for (which, source) in files {
                            let dir = dir.clone();
                            let id = id.clone();
                            tasks.spawn(async move {
                                source.yank(&which, dir.as_path()).await?;
                                msg(Message::InstallYank { id, which });
                                Ok(())
                            });
                        }
                    }
                    PackageDirectory::Symlink(sym) => {
                        tasks.spawn(async move {
                            tokio::fs::create_dir_all(dir.parent().unwrap()).await?;
                            tokio::fs::symlink(sym, &dir).await?;
                            Ok(())
                        });
                    }
                }
            }
        }

        let installing = Arc::new(installing);
        for start_or_opt in ["start", "opt"] {
            let path = gen_root.join(start_or_opt);
            if let Ok(mut read_dir) = tokio::fs::read_dir(path).await {
                while let Some(entry) = read_dir.next_entry().await? {
                    let installing = installing.clone();
                    tasks.spawn(async move {
                        let not_installed_entry =
                            !installing.contains(&entry.file_name().into_vec().into_boxed_slice());
                        let path = entry.path();
                        if not_installed_entry && path.is_dir() {
                            tokio::fs::remove_dir_all(path).await?;
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
