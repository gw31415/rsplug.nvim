use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use crate::{
    log::{Message, msg},
    rsplug::util::{execute, git::RSPLUG_BUILD_SUCCESS_FILE, truncate},
};
use hashbrown::{HashMap, HashSet, hash_map::Entry};
use tokio::{sync::RwLock, task::JoinSet};
use unicode_width::UnicodeWidthStr;
use xxhash_rust::xxh3::xxh3_128;

use super::{util::git, *};

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
    pub async fn fetch(
        self,
        unit: impl IntoIterator<Item = impl Into<Arc<Unit>> + Send + 'static> + Send + Sync + 'static,
        install: bool,
        update: bool,
    ) -> Result<impl Iterator<Item = Package>, Error> {
        let pkgmap: Arc<RwLock<HashMap<usize, Option<Package>>>> = Default::default();
        Self::fetch_inner(pkgmap.clone(), self.into(), unit, install, update).await?;
        msg(Message::CacheDone);
        Ok(Arc::into_inner(pkgmap)
            .unwrap()
            .into_inner()
            .into_values()
            .flatten())
    }

    fn fetch_inner(
        pkgmap: Arc<RwLock<HashMap<usize, Option<Package>>>>,
        config: Arc<Self>,
        unit: impl IntoIterator<Item = impl Into<Arc<Unit>> + Send + 'static> + Send + Sync + 'static,
        install: bool,
        update: bool,
    ) -> Pin<Box<dyn Future<Output = Result<HashSet<usize>, Error>> + Send + Sync>> {
        let config = config.clone();
        Box::pin(async move {
            let depends: HashSet<usize> = unit
                .into_iter()
                .map(move |unit| {
                    let config = config.clone();
                    let pkgmap = pkgmap.clone();
                    async move {
                        let unit: Arc<Unit> = unit.into();
                        let key = Arc::as_ptr(&unit) as usize;

                        let Unit {
                            source,
                            lazy_type,
                            depends,
                            script,
                            merge,
                        } = unit.as_ref();
                        let mut depends = depends
                            .iter()
                            .map(|dep| {
                                Self::fetch_inner(
                                    pkgmap.clone(),
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
                            .collect::<HashSet<_>>();

                        for dep in depends.iter() {
                            while pkgmap.read().await.get(dep).unwrap().is_none() {
                                // Wait for dependent packages to finish processing
                                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                            }
                        }

                        for key in &depends {
                            pkgmap
                                .write()
                                .await
                                .get_mut(key)
                                .unwrap()
                                .as_mut()
                                .unwrap()
                                .lazy_type &= lazy_type;
                        }
                        depends.insert(key);
                        let depends = depends; // make into immutable

                        match pkgmap.write().await.entry(key) {
                            Entry::Occupied(_) => {
                                return Ok(depends);
                            }
                            pkg => {
                                pkg.insert(None);
                            }
                        }

                        'add_pkg: {
                            let PluginSource {
                                base,
                                manually_to_sym: _,
                                build,
                            } = source;
                            let pkg: Package = match base {
                                UnitSource::GitHub { owner, repo, rev } => {
                                    let proj_root = config.cachepath.join(base.cachedir());

                                    tokio::fs::create_dir_all(&proj_root).await?;
                                    let proj_root = proj_root.canonicalize()?;
                                    let filesource = Arc::new(FileSource::Directory {
                                        path: proj_root.into(),
                                    });
                                    let FileSource::Directory { path: proj_root } =
                                        filesource.as_ref()
                                    else {
                                        // SAFETY: すぐ上の行で `sourcefile` を `Directory` として宣言している。
                                        unsafe { std::hint::unreachable_unchecked() };
                                    };

                                    let url: Arc<str> = Arc::from(base.url());

                                    // リポジトリがない場合のインストール処理
                                    let repository = if let Ok(mut repo) =
                                        git::open(&proj_root).await
                                    {
                                        // アップデート処理
                                        if update {
                                            msg(Message::Cache("Updating", url.clone()));
                                            repo.fetch(git::ls_remote(url.clone(), rev).await?)
                                                .await?;
                                        }
                                        repo
                                    } else if install {
                                        msg(Message::Cache("Initializing", url.clone()));
                                        let mut repo =
                                            git::init(proj_root.clone(), url.clone()).await?;
                                        msg(Message::Cache("Fetching", url.clone()));
                                        repo.fetch(git::ls_remote(url.clone(), rev).await?).await?;
                                        repo
                                    } else {
                                        // 見つからない場合はスキップ
                                        break 'add_pkg;
                                    };

                                    // ディレクトリ内容からのIDの決定
                                    let id = PackageID::new({
                                        let (head, diff) = tokio::join!(
                                            repository.head_hash(),
                                            repository.diff_hash()
                                        );
                                        match (head, diff) {
                                            (Ok(mut head), Ok(diff)) => {
                                                head.extend(diff);
                                                for (i, comp) in build.iter().enumerate() {
                                                    head.extend(i.to_ne_bytes());
                                                    head.extend(comp.as_bytes());
                                                }
                                                u128::to_ne_bytes(xxh3_128(&head))
                                            }
                                            (Err(err), _) | (_, Err(err)) => Err(err)?,
                                        }
                                    });

                                    // ビルド実行
                                    if !build.is_empty() {
                                        let next_build_success_id = id.as_str();
                                        let rsplug_build_success_file =
                                            proj_root.join(RSPLUG_BUILD_SUCCESS_FILE);
                                        if let Some(ref prev_build_success_id) =
                                            tokio::fs::read(&rsplug_build_success_file).await.ok()
                                            && prev_build_success_id
                                                == next_build_success_id.as_bytes()
                                        {
                                            // ビルド成功の痕跡があればビルドをスキップ
                                        } else {
                                            let exec = async move {
                                                let _ = tokio::fs::remove_file(
                                                    &rsplug_build_success_file,
                                                )
                                                .await;
                                                let logid = {
                                                    const MAX_LOGID_LEN: usize = 20;
                                                    let repo = truncate(repo, MAX_LOGID_LEN);

                                                    let len = MAX_LOGID_LEN
                                                        .saturating_sub(repo.width_cjk() + 1);
                                                    if len < 2 {
                                                        repo
                                                    } else {
                                                        let mut owner = truncate(owner, len);
                                                        owner.push('/');
                                                        owner.push_str(&repo);
                                                        owner
                                                    }
                                                };
                                                let code = execute(build.iter(), proj_root, {
                                                    move |(stdtype, line)| {
                                                        msg(Message::CacheBuildProgress {
                                                            id: logid.clone(),
                                                            stdtype,
                                                            line,
                                                        });
                                                    }
                                                })
                                                .await?;
                                                if code == 0 {
                                                    tokio::fs::write(
                                                        rsplug_build_success_file,
                                                        next_build_success_id.as_bytes(),
                                                    )
                                                    .await?;
                                                    Ok::<_, Error>(())
                                                } else {
                                                    Err(Error::BuildScriptFailed {
                                                        code,
                                                        build: build.clone(),
                                                    })
                                                }
                                            };
                                            exec.await?;
                                        }
                                    }

                                    let files = repository.ls_files().await?;
                                    let mut lazy_type = lazy_type.clone();
                                    for luam in extract_unique_lua_modules(files.iter()) {
                                        lazy_type &= LoadEvent::LuaModule(LuaModule(luam.into()));
                                    }
                                    let files: HowToPlaceFiles = if source.to_sym() {
                                        HowToPlaceFiles::SymlinkDirectory(proj_root.clone())
                                    } else {
                                        HowToPlaceFiles::CopyEachFile(
                                            files
                                                .into_iter()
                                                .filter_map(|path| {
                                                    let ignored = path.iter().any(|k| {
                                                        let k = k.to_str().unwrap(); // 上でUTF-8に変換済み
                                                        merge.ignore.matched(k)
                                                    });
                                                    if !ignored && proj_root.join(&path).is_file() {
                                                        Some((
                                                            path,
                                                            FileItem {
                                                                source: filesource.clone(),
                                                                merge_type: MergeType::Conflict,
                                                            },
                                                        ))
                                                    } else {
                                                        None
                                                    }
                                                })
                                                .collect(),
                                        )
                                    };
                                    Package {
                                        id,
                                        files,
                                        lazy_type,
                                        script: script.clone(),
                                    }
                                }
                            };
                            pkgmap.write().await.insert(key, Some(pkg));
                        }

                        Ok::<_, Error>(depends)
                    }
                })
                .collect::<JoinSet<_>>()
                .join_all()
                .await
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .flatten()
                .collect();
            Ok(depends)
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

fn extract_unique_lua_modules<'a>(
    files: impl Iterator<Item = &'a PathBuf> + 'a,
) -> impl Iterator<Item = String> + 'a {
    let mut seen = hashbrown::HashSet::new();

    files.filter_map(move |path| {
        let mut comps = path.components();

        // 先頭が "lua" でなければ対象外
        match comps.next().and_then(|c| c.as_os_str().to_str()) {
            Some("lua") => {}
            _ => return None,
        }

        // lua/ の直後を取得
        let comp = comps.next()?;

        let name = Path::new(comp.as_os_str())
            .file_stem() // hoge2.lua → hoge2
            .and_then(|s| s.to_str())?
            .to_string();

        if !name.is_empty() && seen.insert(name.clone()) {
            Some(name)
        } else {
            None
        }
    })
}
