use std::{
    borrow::{Borrow, Cow},
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use crate::log::{Message, msg};
use hashbrown::HashMap;
use tokio::task::JoinSet;
use xxhash_rust::xxh3::xxh3_128;

use super::{
    util::{execute, git},
    *,
};

struct IntoStringSplit(String, char);

impl Iterator for IntoStringSplit {
    type Item = String;
    fn next(&mut self) -> Option<Self::Item> {
        let Self(data, c) = self;
        if data.is_empty() {
            return None;
        }
        let Some(pos) = data.rfind(|ch| &ch == c) else {
            return Some(std::mem::take(data));
        };
        let item = data.split_off(pos + 1);
        data.pop();
        Some(item)
    }
}

/// プラグインのキャッシュ
pub struct Cache {
    // キャッシュディレクトリのパス
    pub cachepath: Cow<'static, Path>,
}

impl Cache {
    pub fn new(path: impl Into<Cow<'static, Path>>) -> Self {
        Cache {
            cachepath: path.into(),
        }
    }
    /// キャッシュし、展開して Package のコレクションにする
    pub fn fetch<B: FromIterator<Package>>(
        self,
        unit: impl IntoIterator<Item = impl Into<Arc<Unit>> + Send + 'static> + Send + Sync + 'static,
        install: bool,
        update: bool,
    ) -> Pin<Box<dyn Future<Output = Result<B, Error>> + Send + Sync>> {
        Self::fetch_inner(self.into(), unit, install, update)
    }

    fn fetch_inner<B: FromIterator<Package>>(
        config: Arc<Self>,
        unit: impl IntoIterator<Item = impl Into<Arc<Unit>> + Send + 'static> + Send + Sync + 'static,
        install: bool,
        update: bool,
    ) -> Pin<Box<dyn Future<Output = Result<B, Error>> + Send + Sync>> {
        let config = config.clone();
        Box::pin(async move {
            let pkgs: B = unit
                .into_iter()
                .map(move |unit| {
                    let config = config.clone();
                    async move {
                        let unit: Arc<Unit> = unit.into();

                        let Unit {
                            source,
                            lazy_type,
                            depends,
                            script,
                            merge,
                        } = unit.borrow();
                        if !depends.is_empty() {
                            unimplemented!("依存関係の解決は未実装です");
                        }
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

                                    let url: Arc<str> =
                                        Arc::from(format!("https://github.com/{owner}/{repo}"));

                                    // リポジトリがない場合のインストール処理
                                    if !git::exists(proj_root).await {
                                        if install {
                                            msg(Message::Cache("Initializing", url.clone()));
                                            git::init(&url, proj_root).await?;
                                            msg(Message::Cache("Fetching", url.clone()));
                                            git::fetch(rev, proj_root).await?;
                                        } else {
                                            // インストールされていない場合はスキップ
                                            break 'add_pkg;
                                        }
                                    }

                                    // アップデート処理
                                    if update {
                                        msg(Message::Cache("Updating", url.clone()));
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

                                    let files: HashMap<PathBuf, _> = {
                                        let stdout = execute(
                                            tokio::process::Command::new("git")
                                                .current_dir(proj_root)
                                                .arg("ls-files")
                                                .arg("--full-name"),
                                        )
                                        .await?;
                                        IntoStringSplit(String::from_utf8(stdout)?, '\n')
                                    }
                                    .filter_map(|fname| {
                                        let fname = PathBuf::from(fname);
                                        let ignored = fname.iter().any(|k| {
                                            let k = k.to_str().unwrap(); // 上でUTF-8に変換済み
                                            merge.ignore.matched(k)
                                        });
                                        if !ignored && proj_root.join(&fname).is_file() {
                                            Some((
                                                fname,
                                                FileItem {
                                                    source: filesource.clone(),
                                                    merge_type: MergeType::Conflict,
                                                },
                                            ))
                                        } else {
                                            None
                                        }
                                    })
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
            cachepath: {
                let homedir = std::env::home_dir().unwrap();
                let cachedir = homedir.join(".cache");
                cachedir.join("rsplug").into()
            },
        }
    }
}
