use std::{
    ops::Add,
    path::{Path, PathBuf},
    sync::Arc,
};

use hashbrown::{HashMap, HashSet};
use itertools::Itertools;
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

impl Package {
    pub fn merge(pkgs: impl IntoIterator<Item = Self>) -> Vec<Self> {
        let mut items = pkgs.into_iter().collect_vec();

        let mut done_items = Vec::new();

        while items.len() > 1 {
            let (tail, tail2) = (items.pop().unwrap(), items.pop().unwrap());
            match tail + tail2 {
                (tail, Some(tail2)) => {
                    done_items.push(tail);
                    items.push(tail2);
                }
                (tail, _) => {
                    items.push(tail);
                }
            }
        }

        while let Some(item) = done_items.pop() {
            items.push(item);
        }
        items
    }

    pub async fn install(
        pkgs: impl IntoIterator<Item = Self>,
        config: impl Into<Arc<Config>>,
    ) -> MainResult {
        let packpath: PathBuf = config.into().packpath.join("pack").join("merged");
        // if let Ok(exists) = std::fs::exists(&packpath)
        //     && exists
        // {
        //     panic!("packpath already exists: {}", packpath.display());
        // }
        // let _ = tokio::fs::remove_dir_all(packpath.as_path()).await;
        tokio::fs::create_dir_all(packpath.as_path()).await?;
        let mut registered = HashSet::new();
        let mut io_tasks: JoinSet<_> = pkgs
            .into_iter()
            .flat_map({
                |pkg| {
                    let dir: Arc<PathBuf> = Arc::new(
                        packpath
                            .join(match pkg.package_type {
                                PackageType::Start => "start",
                                PackageType::Opt(_) => "opt",
                            })
                            .join(pkg.id.into_str()),
                    );
                    registered.insert(dir.clone());
                    if dir.is_dir() {
                        println!("Skipped: {dir:?}");
                        return Vec::new();
                    }
                    pkg.files
                        .into_iter()
                        .map(move |(p, s)| (p, s, dir.clone()))
                        .collect::<Vec<_>>()
                }
            })
            .map(|(path, source, dir)| async move {
                println!("Yanking: {:?}", dir.join(&path));
                source.yank(path, dir.as_path()).await
            })
            .collect::<JoinSet<_>>();
        {
            let clean = |dir: PathBuf| {
                let registered = registered.clone();
                async move {
                    let dir = Arc::new(dir);
                    let mut read_dir = tokio::fs::read_dir(dir.as_ref()).await?;
                    let mut rm_task = JoinSet::new();
                    while let Some(entry) = read_dir.next_entry().await? {
                        let dir = dir.clone();
                        let registered = registered.clone();
                        rm_task.spawn(async move {
                            let name = entry.file_name();
                            let path = dir.join(name);
                            if !registered.contains(&path) && path.is_dir() {
                                tokio::fs::remove_dir_all(&path).await?;
                            }
                            Ok::<_, Error>(())
                        });
                    }
                    rm_task
                        .join_all()
                        .await
                        .into_iter()
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok::<_, Error>(())
                }
            };
            let start = packpath.join("start");
            if start.is_dir() {
                io_tasks.spawn(clean(start));
            }
            let opt = packpath.join("opt");
            if opt.is_dir() {
                io_tasks.spawn(clean(opt));
            }
        }
        io_tasks.join_all().await.into_iter().collect()
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
        }
    }
}
