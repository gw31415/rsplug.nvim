use std::{
    borrow::Cow,
    collections::HashSet,
    ffi::OsStr,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use dag::DagNode;
use dag::{DagError, TryDag, iterator::DagIteratorMapFuncArgs};
use git2::Oid;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Serialize, Serializer};
use serde_with::DeserializeFromStr;

use super::*;

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
    /// `with` で指定された依存プラグインのキャッシュ相対パス
    pub dependency_cachedirs: Vec<PathBuf>,
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
        let id_to_cachedir = plugins
            .iter()
            .map(|plug| (plug.id().to_string(), plug.cache.repo.default_cachedir()))
            .collect::<hashbrown::HashMap<_, _>>();
        Ok(plugins.try_dag()?.into_map_iter(
            move |DagIteratorMapFuncArgs {
                      inner,
                      dependents_iter,
                  }| {
                let PluginConfig {
                    cache,
                    lazy_type,
                    with,
                    custom_name: _,
                    script,
                    merge,
                } = inner;
                // 依存元の lazy_type を集約
                let lazy_type = dependents_iter
                    .flatten()
                    .fold(lazy_type, |dep, plug| dep & plug.lazy_type.clone());
                let dependency_cachedirs = with
                    .into_iter()
                    .map(|dep_id| {
                        id_to_cachedir
                            .get(&dep_id)
                            .cloned()
                            .expect("dependency id must be resolvable in DAG")
                    })
                    .collect();
                Plugin {
                    cache,
                    lazy_type,
                    script,
                    merge,
                    dependency_cachedirs,
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
        cache_dir: impl AsRef<Path>,
        locked_rev: Option<Arc<str>>,
    ) -> Result<Option<(LoadedPlugin, (String, String))>, Error> {
        use super::{util::git, *};
        use crate::{
            log::{Message, msg},
            rsplug::util::{execute, git::RSPLUG_BUILD_SUCCESS_FILE, truncate},
        };
        use std::sync::Arc;
        use unicode_width::UnicodeWidthStr;

        let invalid_data =
            |msg: String| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, msg));

        let Plugin {
            cache,
            lazy_type,
            script,
            merge,
            dependency_cachedirs,
        } = self;

        let to_sym = cache.to_sym();
        let CacheConfig {
            repo,
            manually_to_sym: _,
            build,
            lua_build,
        } = cache;

        let proj_root = cache_dir.as_ref().join(repo.default_cachedir());
        let url: Arc<str> = Arc::from(repo.url());
        let (loaded_plugin, lock_info) = match repo {
            RepoSource::GitHub { owner, repo, rev } => {
                tokio::fs::create_dir_all(&proj_root).await?;
                let proj_root = tokio::fs::canonicalize(&proj_root).await?;
                let filesource = Arc::new(FileSource::Directory {
                    path: proj_root.into(),
                });
                let FileSource::Directory { path: proj_root } = filesource.as_ref() else {
                    // SAFETY: すぐ上の行で `sourcefile` を `Directory` として宣言している。
                    unsafe { std::hint::unreachable_unchecked() };
                };

                let repository = 'repo: {
                    if let Ok(mut repo) = git::open(proj_root.clone()).await {
                        // リポジトリが存在する場合

                        // アップデート処理
                        let oid = if let Some(locked_rev) = locked_rev.as_deref() {
                            // locked モード
                            if !is_full_hex_hash(locked_rev) {
                                return Err(invalid_data(format!(
                                    "Locked revision must be full hash for {}: got {}",
                                    url, locked_rev
                                )));
                            }
                            Oid::from_str(locked_rev).map_err(Error::Git2)?
                        } else if update {
                            msg(Message::Cache("Updating", url.clone()));
                            git::ls_remote(url.clone(), rev).await?
                        } else {
                            break 'repo repo;
                        };
                        repo.fetch(oid).await?;
                        msg(Message::Cache("Updating:done", url.clone()));
                        repo
                    } else if install {
                        // リポジトリがない場合のインストール処理
                        msg(Message::Cache("Initializing", url.clone()));
                        let mut repo = git::init(proj_root.clone(), url.clone()).await?;
                        msg(Message::Cache("Fetching", url.clone()));
                        let oid = if let Some(locked_rev) = locked_rev.as_deref() {
                            // locked モード
                            if !is_full_hex_hash(locked_rev) {
                                return Err(invalid_data(format!(
                                    "Locked revision must be full hash for {}: got {}",
                                    url, locked_rev
                                )));
                            }
                            Oid::from_str(locked_rev).map_err(Error::Git2)?
                        } else {
                            git::ls_remote(url.clone(), rev).await?
                        };
                        repo.fetch(oid).await?;
                        repo
                    } else {
                        if locked_rev.is_some() {
                            return Err(invalid_data(format!(
                                "Missing cached repository for locked revision: {}",
                                url
                            )));
                        }
                        // 見つからない場合はスキップ
                        return Ok(None);
                    }
                };

                let head_rev = repository.head_hash().await?;
                let head_rev_str = String::from_utf8_lossy(&head_rev).to_string();

                if let Some(locked_rev) = locked_rev.as_deref()
                    && head_rev_str != locked_rev
                {
                    return Err(invalid_data(format!(
                        "Locked revision mismatch for {}: expected {}, got {}",
                        url, locked_rev, head_rev_str
                    )));
                }

                let mut id =
                    build_aware_plugin_id(&repository, head_rev, &build, lua_build.as_deref())
                        .await?;

                // ビルド実行
                if !build.is_empty() || lua_build.is_some() {
                    let mut lua_runtimepaths = Vec::new();
                    let mut seen_runtimepaths = HashSet::new();
                    let mut add_runtimepath = |path: PathBuf| {
                        if seen_runtimepaths.insert(path.clone()) {
                            lua_runtimepaths.push(path);
                        }
                    };
                    add_runtimepath(proj_root.to_path_buf());
                    for dep_cachedir in &dependency_cachedirs {
                        let dep_path = cache_dir.as_ref().join(dep_cachedir);
                        if let Ok(dep_path) = tokio::fs::canonicalize(dep_path).await {
                            add_runtimepath(dep_path);
                        }
                    }

                    let next_build_success_id = id.as_str();
                    let rsplug_build_success_file = proj_root.join(RSPLUG_BUILD_SUCCESS_FILE);
                    if let Some(ref prev_build_success_id) =
                        tokio::fs::read(&rsplug_build_success_file).await.ok()
                        && prev_build_success_id == next_build_success_id.as_bytes()
                    {
                        // ビルド成功の痕跡があればビルドをスキップ
                    } else {
                        let exec = async {
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

                            if !build.is_empty() {
                                let id = Arc::new(format!("{logid} (sh)"));
                                let result: Result<(), Error> = {
                                    let id = id.clone();
                                    async {
                                        let code = execute(build.iter(), proj_root.clone(), {
                                            move |(stdtype, line)| {
                                                msg(Message::CacheBuildProgress {
                                                    id: id.clone(),
                                                    stdtype,
                                                    line,
                                                });
                                            }
                                        })
                                        .await?;
                                        if code != 0 {
                                            return Err(Error::BuildScriptFailed {
                                                code,
                                                build: build.clone(),
                                                repo: repo.clone(),
                                            });
                                        }
                                        Ok(())
                                    }
                                }
                                .await;
                                msg(Message::CacheBuildFinished {
                                    id,
                                    success: result.is_ok(),
                                });
                                result?;
                            }

                            if let Some(lua_build) = lua_build.as_deref() {
                                let id = Arc::new(format!("{logid} (lua)"));
                                let result: Result<(), Error> = {
                                    let id = id.clone();
                                    async {
                                        let lua_build_path =
                                            create_lua_build_script(lua_build, &lua_runtimepaths)
                                                .await?;
                                        let code = execute(
                                            lua_build_nvim_command(lua_build_path.as_os_str()),
                                            proj_root.clone(),
                                            move |(stdtype, line)| {
                                                msg(Message::CacheBuildProgress {
                                                    id: id.clone(),
                                                    stdtype,
                                                    line,
                                                });
                                            },
                                        )
                                        .await;
                                        let _ = tokio::fs::remove_file(&lua_build_path).await;
                                        let code = code?;
                                        if code != 0 {
                                            return Err(Error::BuildLuaScriptFailed { code, repo });
                                        }
                                        Ok(())
                                    }
                                }
                                .await;
                                msg(Message::CacheBuildFinished {
                                    id,
                                    success: result.is_ok(),
                                });
                                result?;
                            }

                            Ok::<_, Error>(())
                        };
                        exec.await?;
                        id = build_aware_plugin_id(
                            &repository,
                            repository.head_hash().await?,
                            &build,
                            lua_build.as_deref(),
                        )
                        .await?;
                        tokio::fs::write(rsplug_build_success_file, id.as_str().as_bytes()).await?;
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
                let lock_info = (url.to_string(), head_rev_str);

                (loaded, lock_info)
            }
        };

        Ok(Some((loaded_plugin, lock_info)))
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

async fn build_aware_plugin_id(
    repository: &util::git::Repository,
    mut head_rev: Vec<u8>,
    build: &[String],
    lua_build: Option<&str>,
) -> Result<PluginID, Error> {
    use crate::rsplug::util::hash;

    if repository.is_dirty().await? {
        head_rev.extend(repository.diff_hash().await?);
    }
    for (i, comp) in build.iter().enumerate() {
        head_rev.extend(i.to_ne_bytes());
        head_rev.extend(comp.as_bytes());
    }
    if let Some(lua_build) = lua_build {
        head_rev.extend(b"lua_build");
        head_rev.extend(lua_build.as_bytes());
    }
    Ok(PluginID::new(hash::digest(&head_rev)))
}

fn is_full_hex_hash(value: &str) -> bool {
    value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit())
}

fn lua_build_nvim_command(lua_script_path: &OsStr) -> [Cow<'_, OsStr>; 6] {
    [
        Cow::Borrowed(OsStr::new("nvim")),
        Cow::Borrowed(OsStr::new("--headless")),
        Cow::Borrowed(OsStr::new("-u")),
        Cow::Borrowed(OsStr::new("NONE")),
        Cow::Borrowed(OsStr::new("-l")),
        Cow::Borrowed(lua_script_path),
    ]
}

fn lua_build_wrapper(lua_script: &str, runtimepaths: &[PathBuf]) -> String {
    fn lua_single_quoted(s: &str) -> String {
        let mut escaped = String::with_capacity(s.len() + 8);
        escaped.push('\'');
        for ch in s.chars() {
            match ch {
                '\\' => escaped.push_str("\\\\"),
                '\'' => escaped.push_str("\\'"),
                '\n' => escaped.push_str("\\n"),
                '\r' => escaped.push_str("\\r"),
                _ => escaped.push(ch),
            }
        }
        escaped.push('\'');
        escaped
    }
    fn lua_runtimepath_setup(runtimepaths: &[PathBuf]) -> String {
        let runtimepaths = runtimepaths
            .iter()
            .map(|path| format!("  {},", lua_single_quoted(path.to_string_lossy().as_ref())))
            .collect::<Vec<_>>()
            .join("\n");
        if runtimepaths.is_empty() {
            String::new()
        } else {
            format!(
                "for _, rtp in ipairs({{\n{}\n}}) do\n  vim.opt.runtimepath:prepend(rtp)\nend\n",
                runtimepaths
            )
        }
    }
    format!(
        "local ok, err = xpcall(function()\n{}\n{}\nend, debug.traceback)\nif not ok then\n  vim.api.nvim_err_writeln(err)\n  os.exit(1)\nend\n",
        lua_runtimepath_setup(runtimepaths),
        lua_script
    )
}

async fn create_lua_build_script(
    lua_script: &str,
    runtimepaths: &[PathBuf],
) -> Result<PathBuf, std::io::Error> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let filename = format!("rsplug-build-lua-{}-{}.lua", std::process::id(), nonce);
    let path = std::env::temp_dir().join(filename);
    tokio::fs::write(&path, lua_build_wrapper(lua_script, runtimepaths)).await?;
    Ok(path)
}
