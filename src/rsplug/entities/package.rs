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

use hashbrown::{HashMap, HashSet};
use tokio::task::JoinSet;

use super::*;

/// インストール単位となるプラグイン。
/// NOTE: 遅延実行されるプラグイン等は、インストール後に Loader が生成される。Loaderはまとめて
/// Packageに変換する。
pub struct Package {
    /// ID
    pub(super) id: PackageID,
    /// プラグインの遅延実行タイプ
    pub lazy_type: LazyType,
    /// 配置するファイル
    pub(super) files: HashMap<PathBuf, Arc<FileSource>>,
    /// セットアップスクリプト
    pub(super) script: SetupScript,
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
                (tail, _) => {
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
        let mergeable = {
            let (sfname, rfname): (HashSet<_>, HashSet<_>) =
                (self.files.keys().collect(), rhs.files.keys().collect());
            sfname.is_disjoint(&rfname)
        };
        if mergeable {
            let mut pkg = self;
            pkg.files.extend(rhs.files);
            pkg.id += rhs.id;

            (pkg, None)
        } else {
            (self, Some(rhs))
        }
    }
}

/// ファイルの取得(生成)元。
pub(super) enum FileSource {
    Directory { path: PathBuf },
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

type PackageFiles = (&'static str, Vec<(PathBuf, Arc<FileSource>)>);

/// PackPath の象徴となる状態。この構造体に Package をインサートしていき、最後に実際のパスを指定して install を行う。
#[derive(Default)]
pub struct PackPathState {
    installing: HashSet<Box<[u8]>>,
    files: HashMap<PackageIDStr, PackageFiles>,
}

impl PackPathState {
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
        for (path, source) in files {
            let (_, files) = self
                .files
                .entry(id.as_str())
                .or_insert((pkg_type_str, Vec::new()));

            files.push((path, source));
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

        for (id, (start_or_opt, files)) in files {
            let log_target = Arc::new({
                let mut res = "install:yank:".to_string();
                res.push_str(&id);
                res
            });
            let dir = Arc::new(gen_root.join(start_or_opt).join(id));
            if dir.is_dir() {
                log::info!(target: "install:skipped", "{dir:?}");
            } else {
                for (which, source) in files {
                    let dir = dir.clone();
                    let log_target = log_target.clone();
                    tasks.spawn(async move {
                        log::info!(target: &log_target, "{}", which.to_string_lossy());
                        source.yank(which, dir.as_path()).await
                    });
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

        tasks
            .join_all()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .and(Ok(()))
    }
}
