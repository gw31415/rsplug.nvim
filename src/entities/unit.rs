use std::{
    borrow::Borrow,
    collections::BTreeSet,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use hashbrown::HashMap;
use rand::RngCore;
use regex::RegexSet;
use tokio::task::JoinSet;
use xxhash_rust::xxh3::xxh3_128;

use super::*;

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
    pub fn unpack<B: FromIterator<Package>>(
        unit: impl IntoIterator<Item = impl Into<Arc<Unit>> + Send + 'static> + Send + Sync + 'static,
        install: bool,
        update: bool,
        config: impl Into<Arc<Config>>,
    ) -> Pin<Box<dyn Future<Output = MainResult<B>> + Send + Sync>> {
        let config = config.into();
        Box::pin(async move {
            let pkgs: B = unit
                .into_iter()
                .map(move |unit| {
                    let config: Arc<Config> = config.clone();
                    async move {
                        let unit: Arc<Unit> = unit.into();

                        let Unit {
                            source,
                            package_type,
                            depends,
                        } = unit.borrow();
                        let mut pkgs: Vec<_> = depends
                            .iter()
                            .map(|dep| {
                                Self::unpack::<Vec<_>>(
                                    [dep.clone()],
                                    install,
                                    update,
                                    config.clone(),
                                )
                            })
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
                                        git::init(
                                            format!("https://github.com/{owner}/{repo}"),
                                            &download_dir,
                                        )
                                        .await?;
                                    } else {
                                        // インストールされていない場合はスキップ
                                        break 'add_pkg;
                                    }
                                    // TODO: 初期インストールのみの場合はアップデート処理もする
                                    // Problem: git init しかされない
                                    if update {
                                        git::fetch(rev, &download_dir).await?;
                                    }

                                    let id: BTreeSet<[u8; 16]> = BTreeSet::from([{
                                        let (head, diff) = tokio::join!(
                                            git::head(&download_dir),
                                            git::diff(&download_dir),
                                        );
                                        if let (Some(mut head), Some(diff)) = (head, diff) {
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

                                    let sourcefile =
                                        Arc::new(FileSource::Directory { path: download_dir });

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
                .collect::<B>();
            Ok(pkgs)
        })
    }
}

mod git {
    use std::{path::Path, process::Output};

    use super::MainResult;

    /// リポジトリ初期化処理
    pub async fn init(repo: String, dir: &Path) -> MainResult {
        let _ = tokio::fs::remove_dir_all(dir.join(".git")).await;
        tokio::process::Command::new("git")
            .current_dir(dir)
            .arg("init")
            .spawn()?
            .wait()
            .await?;

        tokio::process::Command::new("git")
            .current_dir(dir)
            .arg("remote")
            .arg("add")
            .arg("origin")
            .arg(repo)
            .spawn()?
            .wait()
            .await?;
        Ok(())
    }

    /// リポジトリ同期処理
    pub async fn fetch(rev: &Option<String>, dir: &Path) -> MainResult {
        let rev: &[&str] = if let Some(rev) = rev { &[rev] } else { &[] };
        tokio::process::Command::new("git")
            .current_dir(dir)
            .arg("fetch")
            .arg("--depth=1")
            .arg("origin")
            .args(rev)
            .spawn()?
            .wait()
            .await?;

        tokio::process::Command::new("git")
            .current_dir(dir)
            .arg("switch")
            .arg("--detach")
            .arg("FETCH_HEAD")
            .spawn()?
            .wait()
            .await?;
        Ok(())
    }

    pub async fn head(dir: &Path) -> Option<Vec<u8>> {
        // HEAD のハッシュ
        let Ok(Output { stdout, status, .. }) = tokio::process::Command::new("git")
            .current_dir(dir)
            .arg("rev-parse")
            .arg("HEAD")
            .output()
            .await
        else {
            return None;
        };
        if status.success() { Some(stdout) } else { None }
    }

    pub async fn diff(dir: &Path) -> Option<Vec<u8>> {
        // HEAD のハッシュ
        let Ok(Output { stdout, status, .. }) = tokio::process::Command::new("git")
            .current_dir(dir)
            .arg("diff")
            .arg("HEAD")
            .output()
            .await
        else {
            return None;
        };
        if status.success() { Some(stdout) } else { None }
    }
}
