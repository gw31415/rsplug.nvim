use std::{
    borrow::{Borrow, Cow},
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use hashbrown::HashMap;
use regex::RegexSet;
use tokio::task::JoinSet;
use xxhash_rust::xxh3::xxh3_128;

use super::*;

/// プラグインのキャッシュ
pub struct Cache {
    // インストールを無視するファイル名パターン (Regexパターン)
    pub ignore: Vec<String>,
    // キャッシュディレクトリのパス
    pub cachepath: Cow<'static, Path>,
}

impl Cache {
    pub fn new(path: impl Into<Cow<'static, Path>>) -> Self {
        Cache {
            cachepath: path.into(),
            ..Default::default()
        }
    }
    /// キャッシュし、展開して Package のコレクションにする
    pub fn fetch<B: FromIterator<Package>>(
        self,
        unit: impl IntoIterator<Item = impl Into<Arc<Unit>> + Send + 'static> + Send + Sync + 'static,
        install: bool,
        update: bool,
    ) -> Pin<Box<dyn Future<Output = MainResult<B>> + Send + Sync>> {
        Self::fetch_inner(self.into(), unit, install, update)
    }

    fn fetch_inner<B: FromIterator<Package>>(
        config: Arc<Self>,
        unit: impl IntoIterator<Item = impl Into<Arc<Unit>> + Send + 'static> + Send + Sync + 'static,
        install: bool,
        update: bool,
    ) -> Pin<Box<dyn Future<Output = MainResult<B>> + Send + Sync>> {
        let config = config.clone();
        Box::pin(async move {
            let ignore = Arc::new(RegexSet::new(&config.ignore)?);
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
                            script,
                        } = unit.borrow();
                        let mut pkgs: Vec<_> = depends
                            .iter()
                            .map(|dep| {
                                Self::fetch_inner::<Vec<_>>(
                                    config.clone(),
                                    [dep.clone()],
                                    install,
                                    update,
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
                                        match (head, diff) {
                                            (Ok(mut head), Ok(diff)) => {
                                                head.extend(diff);
                                                u128::to_ne_bytes(xxh3_128(&head))
                                            }
                                            (Err(err), _) | (_, Err(err)) => Err(err)?,
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
                                        script: script.clone(),
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

impl Default for Cache {
    fn default() -> Self {
        Cache {
            ignore: vec![
                r"^COPYING$".to_string(),
                r"^COPYING\.txt$".to_string(),
                r"^LICENSE$".to_string(),
                r"^LICENSE\.md$".to_string(),
                r"^LICENSE\.txt$".to_string(),
                r"^Makefile$".to_string(),
                r"^README$".to_string(),
                r"^README\.md$".to_string(),
                r"^README\.txt$".to_string(),
                r"^\.gitattributes$".to_string(),
                r"^\.github$".to_string(),
                r"^\.gitignore$".to_string(),
                r"^\.gitmessage$".to_string(),
                r"^\.luacheckrc$".to_string(),
                r"^\.tool-versions$".to_string(),
                r"^\.vscode$".to_string(),
                r"^deno\.json$".to_string(),
                r"^deno\.jsonc$".to_string(),
                r"^deno\.lock$".to_string(),
            ],
            cachepath: {
                let homedir = std::env::home_dir().unwrap();
                let cachedir = homedir.join(".cache");
                cachedir.join("rsplug").into()
            },
        }
    }
}
