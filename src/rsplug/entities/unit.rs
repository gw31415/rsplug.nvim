use std::{
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use dag::{DagError, TryDag, iterator::DagIteratorMapFuncArgs};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_with::DeserializeFromStr;

use super::*;

/// 設定を構成する基本単位
pub struct Unit {
    /// 取得元
    pub cache: CacheConfig,
    /// Unitに対応する読み込みタイプ
    pub lazy_type: LazyType,
    /// セットアップスクリプト
    pub script: SetupScript,
    /// マージ設定
    pub merge: MergeConfig,
}

/// プラグインの取得元
#[derive(DeserializeFromStr)]
pub enum RepoSource {
    /// GitHub リポジトリ
    GitHub {
        /// リポジトリの所有者
        owner: String,
        /// リポジトリ
        repo: Arc<str>,
        /// リビジョン
        rev: Option<String>,
    },
}

impl RepoSource {
    /// git url
    pub fn url(&self) -> String {
        match self {
            RepoSource::GitHub { owner, repo, .. } => util::github::url(owner, repo),
        }
    }

    /// Such as [Given: ~/.cache/rsplug/]./github.com/owner/repo
    pub(super) fn default_cachedir(&self) -> PathBuf {
        match self {
            RepoSource::GitHub { owner, repo, .. } => {
                let mut path = PathBuf::new();
                path.push("repos");
                path.push("github.com");
                path.push(owner);
                path.push(repo.as_ref());
                path
            }
        }
    }
}

impl FromStr for RepoSource {
    type Err = &'static str;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        static GITHUB_REPO_REGEX: Lazy<Regex> = Lazy::new(|| {
            Regex::new(r"^(?<owner>[a-zA-Z0-9]([a-zA-Z0-9]?|[\-]?([a-zA-Z0-9])){0,38})/(?<repo>[a-zA-Z0-9][a-zA-Z0-9_.-]{0,38})(@(?<rev>\S+))?$").unwrap()
        });
        let Some(cap) = GITHUB_REPO_REGEX.captures(s) else {
            return Err("GitHub repository format must be 'owner/repo[@rev]'");
        };
        let owner = cap["owner"].to_string();
        let repo = cap["repo"].into();
        let rev = cap.name("rev").map(|rev| rev.as_str().to_string());
        Ok(RepoSource::GitHub { owner, repo, rev })
    }
}

impl Unit {
    /// 設定ファイルから Unit のコレクションを構築する
    pub fn new(config: Config) -> Result<impl Iterator<Item = Unit>, DagError> {
        let Config { plugins } = config;
        Ok(plugins.try_dag()?.into_map_iter(
            |DagIteratorMapFuncArgs {
                 inner,
                 dependents_iter,
             }| {
                let Plugin {
                    cache,
                    lazy_type,
                    depends: _,
                    custom_name: _,
                    script,
                    merge,
                } = inner;
                // 依存元の lazy_type を集約
                let lazy_type = dependents_iter
                    .flatten()
                    .fold(lazy_type, |dep, plug| dep & plug.lazy_type.clone());
                Unit {
                    cache,
                    lazy_type,
                    script,
                    merge,
                }
            },
        ))
    }

    /// キャッシュからファイルを読み込み、Package のコレクションにする
    pub async fn fetch(
        self,
        install: bool,
        update: bool,
        cache_dir: impl AsRef<Path>,
    ) -> Result<Option<Package>, Error> {
        use super::{util::git, *};
        use crate::{
            log::{Message, msg},
            rsplug::util::{execute, git::RSPLUG_BUILD_SUCCESS_FILE, truncate},
        };
        use std::sync::Arc;
        use unicode_width::UnicodeWidthStr;
        use xxhash_rust::xxh3::xxh3_128;

        let Unit {
            cache,
            lazy_type,
            script,
            merge,
        } = self;

        let to_sym = cache.to_sym();
        let CacheConfig {
            repo,
            manually_to_sym: _,
            build,
        } = cache;
        let proj_root = cache_dir.as_ref().join(repo.default_cachedir());
        let url: Arc<str> = Arc::from(repo.url());
        let pkg: Package = match repo {
            RepoSource::GitHub { owner, repo, rev } => {
                tokio::fs::create_dir_all(&proj_root).await?;
                let proj_root = proj_root.canonicalize()?;
                let filesource = Arc::new(FileSource::Directory {
                    path: proj_root.into(),
                });
                let FileSource::Directory { path: proj_root } = filesource.as_ref() else {
                    // SAFETY: すぐ上の行で `sourcefile` を `Directory` として宣言している。
                    unsafe { std::hint::unreachable_unchecked() };
                };

                // リポジトリがない場合のインストール処理
                let repository = if let Ok(mut repo) = git::open(&proj_root).await {
                    // アップデート処理
                    if update {
                        msg(Message::Cache("Updating", url.clone()));
                        repo.fetch(git::ls_remote(url.clone(), &rev).await?).await?;
                    }
                    repo
                } else if install {
                    msg(Message::Cache("Initializing", url.clone()));
                    let mut repo = git::init(proj_root.clone(), url.clone()).await?;
                    msg(Message::Cache("Fetching", url.clone()));
                    repo.fetch(git::ls_remote(url.clone(), &rev).await?).await?;
                    repo
                } else {
                    // 見つからない場合はスキップ
                    return Ok(None);
                };

                // ディレクトリ内容からのIDの決定
                let id = PackageID::new({
                    let (head, diff) = tokio::join!(repository.head_hash(), repository.diff_hash());
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
                    let rsplug_build_success_file = proj_root.join(RSPLUG_BUILD_SUCCESS_FILE);
                    if let Some(ref prev_build_success_id) =
                        tokio::fs::read(&rsplug_build_success_file).await.ok()
                        && prev_build_success_id == next_build_success_id.as_bytes()
                    {
                        // ビルド成功の痕跡があればビルドをスキップ
                    } else {
                        let exec = async move {
                            let _ = tokio::fs::remove_file(&rsplug_build_success_file).await;
                            let logid = {
                                const MAX_LOGID_LEN: usize = 20;
                                let repo = truncate(&repo, MAX_LOGID_LEN);

                                let len = MAX_LOGID_LEN.saturating_sub(repo.width_cjk() + 1);
                                if len < 2 {
                                    repo
                                } else {
                                    let mut owner = truncate(&owner, len);
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
                let files: HowToPlaceFiles = if to_sym {
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

        Ok::<_, Error>(Some(pkg))
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
