use std::{
    borrow::{Borrow, Cow},
    collections::BTreeSet,
    ops::{Add, AddAssign, BitAndAssign},
    path::{Path, PathBuf},
    pin::Pin,
    process::Output,
    sync::Arc,
};

use hashbrown::{HashMap, HashSet};
use itertools::Itertools;
use rand::RngCore;
use regex::RegexSet;
use tokio::task::JoinSet;
use xxhash_rust::xxh3::xxh3_128;

#[derive(Clone)]
pub struct GlobalConfig {
    pub cachepath: PathBuf,
    pub packpath: PathBuf,
    pub merge: MergeConfig,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        let homedir = std::env::home_dir().unwrap();
        let cachedir = homedir.join(".cache");
        let appdir = cachedir.join("rsplug");
        GlobalConfig {
            cachepath: appdir.clone(),
            packpath: appdir,
            merge: Default::default(),
        }
    }
}

#[derive(Clone)]
pub struct MergeConfig {
    // Regexパターンで、マージ時に無視するファイルを指定する
    ignore: Vec<String>,
}

impl Default for MergeConfig {
    fn default() -> Self {
        MergeConfig {
            ignore: vec![
                r"^README\.md$".to_string(),
                r"^LICENSE$".to_string(),
                r"^LICENSE\.txt$".to_string(),
                r"^LICENSE\.md$".to_string(),
                r"^COPYING$".to_string(),
                r"^COPYING\.txt$".to_string(),
                r"^\.gitignore$".to_string(),
                r"^\.tool-versions$".to_string(),
                r"^\.vscode$".to_string(),
                r"^deno\.json$".to_string(),
                r"^deno\.lock$".to_string(),
                r"^deno\.jsonc$".to_string(),
                r"^\.gitmessage$".to_string(),
                r"^\.gitattributes$".to_string(),
                r"^\.github$".to_string(),
            ],
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error(transparent)]
    Regex(#[from] regex::Error),
}

type MainResult<T = ()> = Result<T, Error>;

/// 1つのプラグインを表す。GitHubかもしれないし、Hookスクリプトかもしれない
pub struct Unit {
    pub source: UnitSource,
    pub package_type: PackageType,
    pub depends: Vec<Arc<Unit>>,
}

pub enum UnitSource {
    GitHub {
        owner: String,
        repo: String,
        rev: Option<String>,
    },
}

impl Unit {
    /// キャッシュし、展開して Vec<Package> にする
    pub fn unpack(
        unit: impl IntoIterator<Item = impl Into<Arc<Unit>> + Send + 'static> + Send + Sync + 'static,
        install: bool,
        update: bool,
        config: impl Into<Arc<GlobalConfig>>,
    ) -> Pin<Box<dyn Future<Output = MainResult<Vec<Package>>> + Send + Sync>> {
        let config = config.into();
        Box::pin(async move {
            let pkgs = unit
                .into_iter()
                .map(move |unit| {
                    let config: Arc<GlobalConfig> = config.clone();
                    async move {
                        let unit: Arc<Unit> = unit.into();

                        let Unit {
                            source,
                            package_type,
                            depends,
                        } = unit.borrow();
                        let mut pkgs: Vec<_> = depends
                            .iter()
                            .map(|dep| Self::unpack([dep.clone()], install, update, config.clone()))
                            .collect::<JoinSet<_>>()
                            .join_all()
                            .await
                            .into_iter()
                            .collect::<Result<Vec<_>, _>>()?
                            .into_iter()
                            .flatten()
                            .collect();
                        for pkg in pkgs.iter_mut() {
                            pkg.package_type &= package_type;
                        }

                        'add_pkg: {
                            let pkg: Package = match &source {
                                UnitSource::GitHub { owner, repo, rev } => {
                                    let download_dir = config
                                        .cachepath
                                        .join("repos")
                                        .join("github.com")
                                        .join(owner)
                                        .join(repo);

                                    tokio::fs::create_dir_all(&download_dir).await?;
                                    let download_dir = download_dir.canonicalize()?;

                                    if let Ok(true) = tokio::fs::try_exists(
                                        &download_dir.join(".git").join("HEAD"),
                                    )
                                    .await
                                    {
                                        // インストール済みは無視
                                    } else if install {
                                        let _ =
                                            tokio::fs::remove_dir_all(&download_dir.join(".git"))
                                                .await;
                                        tokio::process::Command::new("git")
                                            .current_dir(&download_dir)
                                            .arg("init")
                                            .spawn()?
                                            .wait()
                                            .await?;

                                        tokio::process::Command::new("git")
                                            .current_dir(&download_dir)
                                            .arg("remote")
                                            .arg("add")
                                            .arg("origin")
                                            .arg(format!("https://github.com/{owner}/{repo}"))
                                            .spawn()?
                                            .wait()
                                            .await?;
                                    } else {
                                        // インストールされていない場合はスキップ
                                        break 'add_pkg;
                                    }
                                    if update {
                                        // アップデート処理

                                        let rev: &[&str] = if let Some(rev) = rev.as_ref() {
                                            &[rev]
                                        } else {
                                            &[]
                                        };
                                        tokio::process::Command::new("git")
                                            .current_dir(&download_dir)
                                            .arg("fetch")
                                            .arg("--depth=1")
                                            .arg("origin")
                                            .args(rev)
                                            .spawn()?
                                            .wait()
                                            .await?;

                                        tokio::process::Command::new("git")
                                            .current_dir(&download_dir)
                                            .arg("switch")
                                            .arg("--detach")
                                            .arg("FETCH_HEAD")
                                            .spawn()?
                                            .wait()
                                            .await?;
                                    }

                                    let id: BTreeSet<[u8; 16]> = BTreeSet::from([{
                                        let (head, diff) = tokio::join!(
                                            async {
                                                'a: {
                                                    // HEAD のハッシュ
                                                    let Ok(Output { stdout, status, .. }) =
                                                        tokio::process::Command::new("git")
                                                            .current_dir(&download_dir)
                                                            .arg("rev-parse")
                                                            .arg("HEAD")
                                                            .output()
                                                            .await
                                                    else {
                                                        break 'a Err(());
                                                    };
                                                    if status.success() {
                                                        Ok(stdout)
                                                    } else {
                                                        Err(())
                                                    }
                                                }
                                            },
                                            async {
                                                'a: {
                                                    // HEAD とワーキングツリーの差分
                                                    let Ok(Output { stdout, status, .. }) =
                                                        tokio::process::Command::new("git")
                                                            .current_dir(&download_dir)
                                                            .arg("diff")
                                                            .arg("HEAD")
                                                            .output()
                                                            .await
                                                    else {
                                                        break 'a Err(());
                                                    };
                                                    if status.success() {
                                                        Ok(stdout)
                                                    } else {
                                                        Err(())
                                                    }
                                                }
                                            }
                                        );
                                        if let (Ok(mut head), Ok(diff)) = (head, diff) {
                                            head.extend(diff);
                                            u128::to_ne_bytes(xxh3_128(&head))
                                        } else {
                                            unsafe {
                                                std::mem::transmute::<[u64; 2], [u8; 16]>([
                                                    rand::rng().next_u64(),
                                                    rand::rng().next_u64(),
                                                ])
                                            }
                                        }
                                    }]);

                                    let files = {
                                        let std::process::Output {
                                            stdout,
                                            status,
                                            stderr,
                                        } = tokio::process::Command::new("git")
                                            .current_dir(&download_dir)
                                            .arg("ls-files")
                                            .arg("--full-name")
                                            .output()
                                            .await?;
                                        if !status.success() {
                                            return Err(std::io::Error::new(
                                                std::io::ErrorKind::Interrupted,
                                                String::from_utf8_lossy(&stderr),
                                            ))?;
                                        }
                                        String::from_utf8(stdout)?
                                    };
                                    let ignore = RegexSet::new(&config.merge.ignore)?;
                                    let files: Vec<_> = files
                                        .split('\n')
                                        .filter(|fname| {
                                            let fname = Path::new(fname);
                                            let ignore = fname.iter().any(|k| {
                                                let Some(k) = k.to_str() else {
                                                    // Utf8でないファイル名を持つファイルも無視
                                                    return true;
                                                };
                                                ignore.is_match(k)
                                            });
                                            !ignore && download_dir.join(fname).is_file()
                                        })
                                        .collect();

                                    let sourcefile = Arc::new(FileSource {
                                        source_dir: download_dir,
                                    });

                                    let files: HashMap<PathBuf, Arc<FileSource>> = files
                                        .into_iter()
                                        .map(|fname| (fname.to_owned().into(), sourcefile.clone()))
                                        .collect();
                                    Package {
                                        id: PackageID(id),
                                        files,
                                        package_type: package_type.clone(),
                                    }
                                }
                            };
                            pkgs.push(pkg);
                        }

                        Ok::<_, Error>(pkgs)
                    }
                })
                .collect::<JoinSet<_>>()
                .join_all()
                .await
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            Ok(pkgs)
        })
    }
}

/// Startプラグインとするか、Optプラグインとするか
#[derive(PartialEq, Eq, Clone, Hash)]
pub enum PackageType {
    /// Startプラグイン。起動時に読み込まれる。
    Start,
    /// Optプラグイン。読み込みのタイミングがある。
    Opt(BTreeSet<LoadEvent>),
}

impl PartialOrd for PackageType {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PackageType {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if let PackageType::Start = self
            && let PackageType::Start = other
        {
            return std::cmp::Ordering::Equal;
        }
        if let PackageType::Opt(l_opt) = self
            && let PackageType::Opt(r_opt) = other
        {
            let len_cmp = l_opt.len().cmp(&r_opt.len());
            if len_cmp != std::cmp::Ordering::Equal {
                return len_cmp;
            }

            return l_opt.iter().cmp(r_opt.iter());
        }

        if let PackageType::Start = self {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        }
    }
}

impl<'a> From<&'a PackageType> for Cow<'a, PackageType> {
    fn from(val: &'a PackageType) -> Self {
        Cow::Borrowed(val)
    }
}

impl From<PackageType> for Cow<'_, PackageType> {
    fn from(value: PackageType) -> Self {
        Cow::Owned(value)
    }
}

impl<'a, Rhs: Into<Cow<'a, PackageType>>> BitAndAssign<Rhs> for PackageType {
    fn bitand_assign(&mut self, rhs: Rhs) {
        let rhs: Cow<'a, PackageType> = rhs.into();
        if let PackageType::Opt(events) = self {
            if let PackageType::Opt(events_rhs) = rhs.borrow() {
                events.extend(events_rhs.clone());
            } else {
                *self = rhs.into_owned();
            }
        }
    }
}

/// Optプラグインの読み込みイベントを表す。
#[derive(Hash, Clone, PartialOrd, Ord, PartialEq, Eq)]
pub enum LoadEvent {}

pub struct Package {
    /// ID
    id: PackageID,
    // PackageType
    package_type: PackageType,
    // 配置するファイル
    files: HashMap<PathBuf, Arc<FileSource>>,
}

#[derive(Hash, Clone)]
struct PackageID(pub BTreeSet<[u8; 16]>);

impl Add for PackageID {
    type Output = Self;
    fn add(mut self, rhs: Self) -> Self::Output {
        self += rhs;
        self
    }
}

impl AddAssign for PackageID {
    fn add_assign(&mut self, rhs: Self) {
        self.0.extend(rhs.0);
    }
}

impl From<PackageID> for String {
    fn from(val: PackageID) -> Self {
        let PackageID(inner) = val;
        let bytes = inner.into_iter().flatten().collect_vec();
        let hash = xxh3_128(&bytes).to_ne_bytes();
        let mut res = String::new();
        const TABLE: &[u8; 16] = b"0123456789abcdef";
        for b in hash {
            let (a, r) = (b / 16u8, b % 16u8);
            res.push(TABLE[a as usize] as char);
            res.push(TABLE[r as usize] as char);
        }
        res
    }
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
        config: impl Into<Arc<GlobalConfig>>,
    ) -> MainResult {
        let packpath = config.into().packpath.join("pack").join("merged");
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
                            .join(Into::<String>::into(pkg.id)),
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

struct FileSource {
    source_dir: PathBuf,
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

        let from = self.source_dir.join(&whichfile);
        let to = install_dir.as_ref().join(&whichfile);
        copy(from, to).await
    }
}
