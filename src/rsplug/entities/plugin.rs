use std::{
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use dag::{DagError, TryDag, iterator::DagIteratorMapFuncArgs};
use git2::Oid;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Serialize, Serializer};
use serde_with::DeserializeFromStr;

use super::*;

/// Result of loading a plugin with lock information
pub struct PluginLoadResult {
    /// The loaded plugin (if successfully loaded)
    pub loaded: LoadedPlugin,
    /// Lock information for the lock file
    pub lock_info: PluginLockInfo,
}

/// Information needed for the lock file
pub struct PluginLockInfo {
    /// Repository URL
    pub url: String,
    /// Resolved commit SHA
    pub resolved_rev: String,
}

/// 設定を構成する基本単位
pub struct Plugin {
    /// 取得元
    pub cache: CacheConfig,
    /// Pluginに対応する読み込みタイプ
    pub lazy_type: LazyType,
    /// セットアップスクリプト
    pub script: SetupScript,
    /// マージ設定
    pub merge: MergeConfig,
}

/// プラグインの取得元
#[derive(DeserializeFromStr, Debug)]
pub enum RepoSource {
    /// GitHub リポジトリ
    GitHub {
        /// リポジトリの所有者
        owner: String,
        /// リポジトリ
        repo: Arc<str>,
        /// リビジョン
        rev: Option<Arc<str>>,
    },
}

impl Serialize for RepoSource {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let s = match self {
            RepoSource::GitHub { owner, repo, rev } => {
                if let Some(r) = rev {
                    format!("{}/{}@{}", owner, repo, r)
                } else {
                    format!("{}/{}", owner, repo)
                }
            }
        };
        serializer.serialize_str(&s)
    }
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
        let rev = cap.name("rev").map(|rev| rev.as_str().into());
        Ok(RepoSource::GitHub { owner, repo, rev })
    }
}

impl Plugin {
    /// 設定ファイルから Plugin のコレクションを構築する
    pub fn new(config: Config) -> Result<impl Iterator<Item = Plugin>, DagError> {
        let Config { plugins } = config;
        Ok(plugins.try_dag()?.into_map_iter(
            |DagIteratorMapFuncArgs {
                 inner,
                 dependents_iter,
             }| {
                let PluginConfig {
                    cache,
                    lazy_type,
                    with: _,
                    custom_name: _,
                    script,
                    merge,
                } = inner;
                // 依存元の lazy_type を集約
                let lazy_type = dependents_iter
                    .flatten()
                    .fold(lazy_type, |dep, plug| dep & plug.lazy_type.clone());
                Plugin {
                    cache,
                    lazy_type,
                    script,
                    merge,
                }
            },
        ))
    }

    /// キャッシュからPluginを読み込む。オプションでインストールやアップデートも行う。
    /// インストールされていない場合は `Ok(None)` を返す。
    pub async fn load(
        self,
        install: bool,
        update: bool,
        offline: bool,
        cache_dir: impl AsRef<Path>,
        locked_rev: Option<Arc<str>>,
    ) -> Result<Option<PluginLoadResult>, Error> {
        use super::{util::git, *};
        use crate::{
            log::{Message, msg},
            rsplug::util::{execute, git::RSPLUG_BUILD_SUCCESS_FILE, hash, truncate},
        };
        use std::sync::Arc;
        use unicode_width::UnicodeWidthStr;

        let Plugin {
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
        let (loaded_plugin, lock_info) = match repo {
            RepoSource::GitHub { owner, repo, rev } => {
                let resolved_rev = if install || update {
                    let locked_rev = if let Some(locked_rev) = locked_rev.as_deref() {
                        locked_rev.to_string()
                    } else if let Some(rev) = rev.as_deref() {
                        if is_full_hex_hash(rev) {
                            rev.to_string()
                        } else {
                            if offline {
                                return Err(Error::Io(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    format!("Offline mode requires full revision for {}", url),
                                )));
                            }
                            git::ls_remote(Arc::clone(&url), Some(rev.to_string()))
                                .await?
                                .to_string()
                        }
                    } else {
                        if offline {
                            return Err(Error::Io(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!("Offline mode requires locked revision for {}", url),
                            )));
                        }
                        git::ls_remote(Arc::clone(&url), None::<String>)
                            .await?
                            .to_string()
                    };
                    Some(locked_rev)
                } else {
                    None
                };
                let fetch_oid = if install || update {
                    let rev = resolved_rev.as_deref().ok_or_else(|| {
                        Error::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("Missing locked revision for {}", url),
                        ))
                    })?;
                    Some(Oid::from_str(rev).map_err(Error::Git2)?)
                } else {
                    None
                };

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
                let repository = if let Ok(mut repo) = git::open(proj_root.clone()).await {
                    // アップデート処理
                    if update {
                        msg(Message::Cache("Updating", url.clone()));
                        let fetch_oid = fetch_oid.ok_or_else(|| {
                            Error::Io(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!("Missing locked revision for {}", url),
                            ))
                        })?;
                        repo.fetch(fetch_oid, offline).await?;
                    }
                    repo
                } else if install {
                    if offline {
                        return Err(Error::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("Offline mode requires cached repository for {}", url),
                        )));
                    }
                    msg(Message::Cache("Initializing", url.clone()));
                    let mut repo = git::init(proj_root.clone(), url.clone()).await?;
                    msg(Message::Cache("Fetching", url.clone()));
                    let fetch_oid = fetch_oid.ok_or_else(|| {
                        Error::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("Missing locked revision for {}", url),
                        ))
                    })?;
                    repo.fetch(fetch_oid, offline).await?;
                    repo
                } else {
                    if locked_rev.is_some() {
                        return Err(Error::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("Missing cached repository for locked revision: {}", url),
                        )));
                    }
                    // 見つからない場合はスキップ
                    return Ok(None);
                };

                let head_rev = repository.head_hash().await?;
                let head_rev = String::from_utf8_lossy(&head_rev).to_string();

                if let Some(locked_rev) = locked_rev.as_deref()
                    && head_rev != locked_rev
                {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "Locked revision mismatch for {}: expected {}, got {}",
                            url, locked_rev, head_rev
                        ),
                    )));
                }

                // ディレクトリ内容からのIDの決定
                let id = PluginID::new({
                    let (head, diff) = tokio::join!(repository.head_hash(), repository.diff_hash());
                    let mut head = match (head, diff) {
                        (Ok(mut head), Ok(diff)) => {
                            head.extend(diff);
                            head
                        }
                        (Err(err), _) | (_, Err(err)) => Err(err)?,
                    };
                    for (i, comp) in build.iter().enumerate() {
                        head.extend(i.to_ne_bytes());
                        head.extend(comp.as_bytes());
                    }
                    hash::digest(&head)
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

                let loaded = LoadedPlugin {
                    id,
                    files,
                    lazy_type,
                    script: script.clone(),
                    is_plugctl: false,
                };
                // TODO: 実際にUpdateやInstallが行われたかどうかを判定してLockFileの更新の要不要を決定する
                // Always write the actual checked-out HEAD to the lockfile.
                let lock_info = PluginLockInfo {
                    url: url.to_string(),
                    resolved_rev: head_rev,
                };

                (loaded, lock_info)
            }
        };

        Ok(Some(PluginLoadResult {
            loaded: loaded_plugin,
            lock_info,
        }))
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

fn is_full_hex_hash(value: &str) -> bool {
    value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit())
}
