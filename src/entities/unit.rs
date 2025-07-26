use std::{
    borrow::Borrow,
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

/// 設定を構成する基本単位
pub struct Unit {
    /// 取得元
    pub source: UnitSource,
    /// Unitに対応する読み込みタイプ
    pub lazy_type: LazyType,
    /// 依存する Unit のリスト
    pub depends: Vec<Arc<Unit>>,
}

/// プラグインの取得元
pub enum UnitSource {
    /// GitHub リポジトリ
    GitHub {
        /// リポジトリの所有者
        owner: String,
        /// リポジトリ
        repo: String,
        /// リビジョン
        rev: Option<String>,
    },
}

impl Unit {
    /// キャッシュし、展開して Package のコレクションにする
    pub fn unpack<B: FromIterator<Package>>(
        unit: impl IntoIterator<Item = impl Into<Arc<Unit>> + Send + 'static> + Send + Sync + 'static,
        install: bool,
        update: bool,
        config: impl Into<Arc<Config>>,
    ) -> Pin<Box<dyn Future<Output = MainResult<B>> + Send + Sync>> {
        let config = config.into();
        Box::pin(async move {
            let ignore = Arc::new(RegexSet::new(&config.install.ignore)?);
            let pkgs: B = unit
                .into_iter()
                .map(move |unit| {
                    let (config, ignore) = (config.clone(), ignore.clone());
                    async move {
                        let unit: Arc<Unit> = unit.into();

                        let Unit {
                            source,
                            lazy_type,
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
                            pkg.lazy_type &= lazy_type;
                        }

                        'add_pkg: {
                            let pkg: Package = match &source {
                                UnitSource::GitHub { owner, repo, rev } => {
                                    let proj_root = config
                                        .cachepath
                                        .join("repos")
                                        .join("github.com")
                                        .join(owner)
                                        .join(repo);

                                    tokio::fs::create_dir_all(&proj_root).await?;
                                    let proj_root = proj_root.canonicalize()?;
                                    let filesource =
                                        Arc::new(FileSource::Directory { path: proj_root });
                                    let FileSource::Directory { path: proj_root } =
                                        filesource.as_ref()
                                    else {
                                        // SAFETY: すぐ上の行で `sourcefile` を `Directory` として宣言している。
                                        unsafe { std::hint::unreachable_unchecked() };
                                    };

                                    // リポジトリがない場合のインストール処理
                                    if !git::exists(proj_root).await {
                                        if install {
                                            git::init(
                                                format!("https://github.com/{owner}/{repo}"),
                                                proj_root,
                                            )
                                            .await?;
                                            // 初期インストール時はfetchも行う
                                            git::fetch(rev, proj_root).await?;
                                        } else {
                                            // インストールされていない場合はスキップ
                                            break 'add_pkg;
                                        }
                                    }

                                    // アップデート処理
                                    if update {
                                        git::fetch(rev, proj_root).await?;
                                    }

                                    // ディレクトリ内容からのIDの決定
                                    let id = PackageID::new({
                                        let (head, diff) = tokio::join!(
                                            git::head(proj_root),
                                            git::diff(proj_root),
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
                                    });

                                    let files: HashMap<PathBuf, Arc<FileSource>> = {
                                        let std::process::Output {
                                            stdout,
                                            status,
                                            stderr,
                                        } = tokio::process::Command::new("git")
                                            .current_dir(proj_root)
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
                                    }
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
                                        !ignore && proj_root.join(fname).is_file()
                                    })
                                    .map(|fname| (fname.to_owned().into(), filesource.clone()))
                                    .collect();
                                    Package {
                                        id,
                                        files,
                                        lazy_type: lazy_type.clone(),
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
    //! 各種 Git 操作を行うモジュール

    use std::{path::Path, process::Output};

    use super::MainResult;

    /// リポジトリが存在するかどうか
    pub async fn exists(dir: &Path) -> bool {
        matches!(
            tokio::fs::try_exists(dir.join(".git").join("HEAD")).await,
            Ok(true)
        )
    }

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

    /// HEAD のハッシュ
    pub async fn head(dir: &Path) -> Option<Vec<u8>> {
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

    /// diff の出力
    pub async fn diff(dir: &Path) -> Option<Vec<u8>> {
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
