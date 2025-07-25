use std::{
    cmp::Ordering,
    collections::BinaryHeap,
    ops::Add,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use hashbrown::{HashMap, HashSet};
use tokio::task::JoinSet;

use super::*;

pub struct Package {
    /// ID
    pub(super) id: PackageID,
    // PackageType
    pub(super) package_type: PackageType,
    // 配置するファイル
    pub(super) files: HashMap<PathBuf, Arc<FileSource>>,
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
        let cmp = self.package_type.cmp(&other.package_type);
        if let Ordering::Equal = cmp {
            return self.id.cmp(&other.id);
        }
        cmp
    }
}

impl Package {
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
        if self.package_type != rhs.package_type {
            return (self, Some(rhs));
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

pub enum FileSource {
    Directory { path: PathBuf },
    File { data: Vec<u8> },
}

impl FileSource {
    async fn yank(&self, whichfile: impl AsRef<Path>, install_dir: impl AsRef<Path>) -> MainResult {
        async fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> MainResult {
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

#[derive(Default)]
pub struct PackPathState {
    installing: HashSet<Box<[u8]>>,
    files: HashMap<PackageIDStr, PackageFiles>,
}

impl PackPathState {
    pub fn new() -> Self {
        Default::default()
    }
    pub fn insert(&mut self, pkg: Package) -> Option<Package> {
        let Package {
            id,
            package_type,
            files,
        } = pkg;

        let already_installed = !self.installing.insert(id.as_str().into());
        if already_installed {
            return None;
        }

        let pkg_type_str = if package_type.is_start() {
            "start"
        } else {
            "opt"
        };
        for (path, source) in files {
            let (_, files) = self
                .files
                .entry(id.as_str())
                .or_insert((pkg_type_str, Vec::new()));

            files.push((path, source));
        }

        Loader::create(id, package_type).map(Into::<Option<Package>>::into)?
    }

    pub async fn install(self, packpath: &Path) -> MainResult {
        let gen_root = packpath.join("pack").join("_gen");
        tokio::fs::create_dir_all(&gen_root).await?;
        let Self { installing, files } = self;
        let mut tasks = JoinSet::<MainResult>::new();

        for (id, (start_or_opt, files)) in files {
            let dir = Arc::new(gen_root.join(start_or_opt).join(id));
            if dir.is_dir() {
                // println!("Skipped: {dir:?}");
            } else {
                for (which, source) in files {
                    let dir = dir.clone();
                    tasks.spawn(async move { source.yank(which, dir.as_path()).await });
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
