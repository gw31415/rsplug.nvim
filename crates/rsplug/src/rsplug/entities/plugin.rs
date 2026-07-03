use std::{
    borrow::Cow,
    collections::HashSet,
    ffi::OsStr,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use dag::{DagError, TryDag, iterator::DagIteratorMapFuncArgs};
use git2::Oid;
use once_cell::sync::Lazy;
use regex::Regex;
use sailfish::TemplateSimple;
use serde::{Serialize, Serializer};
use serde_with::DeserializeFromStr;

use super::*;

/// 設定を構成する基本単位
pub struct Plugin {
    /// `on_source` から参照される設定上の名前
    pub source_name: Option<String>,
    /// 取得元
    pub cache: CacheConfig,
    /// Pluginに対応する読み込みタイプ
    pub lazy_type: LazyType,
    /// セットアップスクリプト
    pub script: SetupScript,
    /// マージ設定
    pub merge: MergeConfig,
    /// `depends` で指定された依存プラグインのキャッシュ相対パス
    pub dependency_cachedirs: Vec<PathBuf>,
    /// マージを許可するかどうか（TOML の `merge` フィールドから設定）
    pub merge_enabled: bool,
    /// DAGトポロジカル順。controlled startup の順序維持に使う。
    pub order: usize,
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
    /// 任意の Git リポジトリ URL
    Git {
        /// リポジトリの URL
        url: Arc<str>,
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
            RepoSource::Git { url, rev } => {
                if let Some(r) = rev {
                    format!("{}@{}", url, r)
                } else {
                    url.to_string()
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
            RepoSource::Git { url, .. } => url.to_string(),
        }
    }

    /// Relative to the `repos` cache namespace.
    /// GitHub: `github.com/owner/repo`; URL: `host/path`.
    pub(super) fn default_cachedir(&self) -> PathBuf {
        match self {
            RepoSource::GitHub { owner, repo, .. } => {
                let mut path = PathBuf::new();
                path.push("github.com");
                path.push(owner);
                path.push(repo.as_ref());
                path
            }
            RepoSource::Git { url, .. } => {
                // scheme://[userinfo@]host[:port]/path → host/path (no scheme, auth, port, or .git)
                let s = url.as_ref();
                let after_scheme = s.find("://").map(|i| &s[i + 3..]).unwrap_or(s);
                let normalized = if let Some(slash) = after_scheme.find('/') {
                    let authority = &after_scheme[..slash];
                    let host_start = authority.rfind('@').map(|at| at + 1).unwrap_or(0);
                    let host = authority[host_start..]
                        .split(':')
                        .next()
                        .unwrap_or(&authority[host_start..]);
                    format!("{}{}", host, &after_scheme[slash..])
                } else {
                    let host_start = after_scheme.rfind('@').map(|at| at + 1).unwrap_or(0);
                    after_scheme[host_start..]
                        .split(':')
                        .next()
                        .unwrap_or(&after_scheme[host_start..])
                        .to_string()
                };
                let path_str = normalized.trim_end_matches(".git");
                let mut result = PathBuf::new();
                for comp in path_str.split('/').filter(|s| !s.is_empty()) {
                    result.push(comp);
                }
                result
            }
        }
    }
}

/// URL末尾の `@rev` を分離する。authority部（`://` 〜 最初の `/`）内の `@` は無視する。
fn split_url_rev(s: &str) -> (&str, Option<&str>) {
    let path_start = s
        .find("://")
        .and_then(|i| s[i + 3..].find('/').map(|j| i + 3 + j))
        .unwrap_or(s.len());
    if let Some(at_offset) = s[path_start..].rfind('@') {
        let at_pos = path_start + at_offset;
        (&s[..at_pos], Some(&s[at_pos + 1..]))
    } else {
        (s, None)
    }
}

impl FromStr for RepoSource {
    type Err = &'static str;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.contains("://") {
            let (url, rev) = split_url_rev(s);
            return Ok(RepoSource::Git {
                url: url.into(),
                rev: rev.map(Into::into),
            });
        }
        static GITHUB_REPO_REGEX: Lazy<Regex> = Lazy::new(|| {
            Regex::new(r"^(?<owner>[a-zA-Z0-9]([a-zA-Z0-9]?|[\-]?([a-zA-Z0-9])){0,38})/(?<repo>[a-zA-Z0-9][a-zA-Z0-9_.-]{0,38})(@(?<rev>\S+))?$").unwrap()
        });
        let Some(cap) = GITHUB_REPO_REGEX.captures(s) else {
            return Err(
                "GitHub repository format must be 'owner/repo[@rev]' or a URL containing '://'",
            );
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

        // order は (depth, config_index) の複合キー。depth と index は dag 側で
        // 計算して map 関数に渡される。tiebreak は config 出現順。
        let total = plugins.len();
        // 依存先の cachedir を（dep_name → index で）引くための表。
        // 重複チェック・UnknownDependency 検出は dag 側に委譲する。
        let mut id_to_index = hashbrown::HashMap::new();
        for (index, plug) in plugins.iter().enumerate() {
            if let Some(dep_name) = plug.dep_name() {
                id_to_index.insert(dep_name.to_string(), index);
            }
        }
        let cachedirs = plugins
            .iter()
            .map(|plug| plug.cache.repo.as_ref().map(RepoSource::default_cachedir))
            .collect::<Vec<_>>();

        Ok(plugins.try_dag()?.into_map_iter(
            move |DagIteratorMapFuncArgs {
                      inner,
                      index,
                      depth,
                      dependents_iter,
                  }| {
                let order = depth * (total + 1) + index;
                let source_name = inner.dep_name().map(str::to_string);
                let PluginConfig {
                    cache,
                    lazy_type,
                    depends,
                    custom_name: _,
                    script,
                    merge,
                    ..
                } = inner;
                // 依存元の lazy_type を集約
                let lazy_type = dependents_iter
                    .flatten()
                    .fold(lazy_type, |dep, plug| dep & &plug.lazy_type);
                // 依存先が script-only（リポジトリなし）の場合はキャッシュディレクトリが
                // 存在しないため除外する（runtimepath に追加すべきパスがない）。
                let dependency_cachedirs = depends
                    .into_iter()
                    .filter_map(|dep_id| {
                        id_to_index
                            .get(&dep_id)
                            .and_then(|&dep_index| cachedirs[dep_index].clone())
                    })
                    .collect();
                let merge_enabled = merge.merge;
                Plugin {
                    source_name,
                    cache,
                    lazy_type,
                    script,
                    merge,
                    dependency_cachedirs,
                    merge_enabled,
                    order,
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
    ) -> Result<Option<(LoadedPlugin, Option<(String, String)>)>, Error> {
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
            source_name,
            cache,
            lazy_type,
            script,
            merge,
            dependency_cachedirs,
            merge_enabled,
            order,
        } = self;

        let to_sym = cache.to_sym();
        let CacheConfig {
            repo,
            manually_to_sym: _,
            build,
            lua_build,
            lua_post_update,
        } = cache;

        let Some(repo) = repo else {
            let loaded = LoadedPlugin {
                source_name,
                files: HowToPlaceFiles::CopyEachFile(Default::default()),
                lazy_type,
                script,
                order,
                merge_enabled,
                is_plugctl: false,
            };
            return Ok(Some((loaded, None)));
        };

        // `repo` は直後の match で move されるため、論理 identity に使う相対 cachedir を先に捕捉する。
        // 絶対パス (`proj_root`) は配置用であり identity には含めない。
        let cachedir = repo.default_cachedir();
        let proj_root = cache_dir.as_ref().join(&cachedir);
        let url: Arc<str> = Arc::from(repo.url());

        // バリアント固有のフィールドを抽出（ログ・エラー表示用）
        let (rev, logid, repo_name): (Option<Arc<str>>, String, Arc<str>) = match repo {
            RepoSource::GitHub { owner, repo, rev } => {
                const MAX_LOGID_LEN: usize = 20;
                let repo_t = truncate(&repo, MAX_LOGID_LEN);
                let len = MAX_LOGID_LEN.saturating_sub(repo_t.width_cjk() + 1);
                let logid = if len < 2 {
                    repo_t
                } else {
                    let mut o = truncate(&owner, len);
                    o.push('/');
                    o.push_str(&repo_t);
                    o
                };
                (rev, logid, repo)
            }
            RepoSource::Git { url, rev } => {
                const MAX_LOGID_LEN: usize = 20;
                (rev, truncate(&url, MAX_LOGID_LEN), url)
            }
        };

        tokio::fs::create_dir_all(&proj_root).await?;
        let proj_root: Arc<Path> = tokio::fs::canonicalize(&proj_root).await?.into();
        let filesource = Arc::new(FileSource::Directory {
            path: proj_root.clone(),
        });

        let mut did_update = false;
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
                    git::ls_remote(url.clone(), rev.clone()).await?
                } else {
                    break 'repo repo;
                };
                repo.fetch(oid).await?;
                did_update = update && locked_rev.is_none();
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

        if did_update && let Some(lua_post_update) = lua_post_update.as_deref() {
            let mut lua_runtimepaths = Vec::new();
            let mut seen_runtimepaths = HashSet::new();
            let mut add_runtimepath = |path: PathBuf| {
                if seen_runtimepaths.insert(path.clone()) {
                    lua_runtimepaths.push(path);
                }
            };
            for dep_cachedir in &dependency_cachedirs {
                let dep_path = cache_dir.as_ref().join(dep_cachedir);
                if let Ok(dep_path) = tokio::fs::canonicalize(dep_path).await {
                    add_runtimepath(dep_path);
                }
            }
            add_runtimepath(proj_root.to_path_buf());

            let id = Arc::new(format!("{logid} (lua_post_update)"));
            let result: Result<(), Error> = {
                let id = id.clone();
                async {
                    let lua_post_update_path =
                        create_lua_build_script(lua_post_update, &lua_runtimepaths).await?;
                    let code = execute(
                        lua_build_nvim_command(lua_post_update_path.as_os_str()),
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
                    let _ = tokio::fs::remove_file(&lua_post_update_path).await;
                    let code = code?;
                    if code != 0 {
                        return Err(Error::BuildLuaScriptFailed {
                            code,
                            repo: repo_name.clone(),
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

        let mut identity = build_repo_snapshot_identity(
            &repository,
            cachedir.clone(),
            head_rev,
            &build,
            lua_build.as_deref(),
        )
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

            let next_build_success_id = identity.plugin_id().as_str();
            let rsplug_build_success_file = proj_root.join(RSPLUG_BUILD_SUCCESS_FILE);
            if let Some(ref prev_build_success_id) =
                tokio::fs::read(&rsplug_build_success_file).await.ok()
                && prev_build_success_id == next_build_success_id.as_bytes()
            {
                // ビルド成功の痕跡があればビルドをスキップ
            } else {
                let exec = async {
                    let _ = tokio::fs::remove_file(&rsplug_build_success_file).await;

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
                                        repo: repo_name.clone(),
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
                                    create_lua_build_script(lua_build, &lua_runtimepaths).await?;
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
                                    return Err(Error::BuildLuaScriptFailed {
                                        code,
                                        repo: repo_name.clone(),
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

                    Ok::<_, Error>(())
                };
                exec.await?;
                identity = build_repo_snapshot_identity(
                    &repository,
                    cachedir.clone(),
                    repository.head_hash().await?,
                    &build,
                    lua_build.as_deref(),
                )
                .await?;
                tokio::fs::write(
                    rsplug_build_success_file,
                    identity.plugin_id().as_str().as_bytes(),
                )
                .await?;
            }
        }

        let files = repository.ls_files().await?;
        let mut lazy_type = lazy_type.clone();
        for luam in extract_unique_lua_modules(files.iter()) {
            lazy_type &= LoadEvent::LuaModule(LuaModule(luam.into()));
        }
        let files: HowToPlaceFiles = if to_sym {
            HowToPlaceFiles::RepoSnapshotLink {
                target: proj_root.clone(),
                identity: identity.clone(),
            }
        } else {
            HowToPlaceFiles::CopyEachFile(
                files
                    .into_iter()
                    .filter_map(|path| {
                        let ignored = merge.ignore.matched(&path);
                        if !ignored && proj_root.join(&path).is_file() {
                            Some((
                                path.clone(),
                                FileItem {
                                    source: filesource.clone(),
                                    identity: FileIdentity::RepoFile(RepoFileIdentity::new(
                                        identity.clone(),
                                        path,
                                    )),
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
            source_name,
            files,
            lazy_type,
            script: script.clone(),
            order,
            merge_enabled,
            is_plugctl: false,
        };
        let lock_info = Some((url.to_string(), head_rev_str));

        Ok(Some((loaded, lock_info)))
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

async fn build_repo_snapshot_identity(
    repository: &util::git::Repository,
    repo_cache_dir: PathBuf,
    head_rev: Vec<u8>,
    build: &[String],
    lua_build: Option<&str>,
) -> Result<RepoSnapshotIdentity, Error> {
    let dirty_diff = if repository.is_dirty().await? {
        Some(repository.diff_hash().await?)
    } else {
        None
    };

    Ok(RepoSnapshotIdentity::new(
        repo_cache_dir,
        head_rev,
        dirty_diff,
        Arc::<[String]>::from(build),
        lua_build.map(Into::into),
    ))
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

#[derive(TemplateSimple)]
#[template(path = "lua_build.stpl")]
#[template(escape = false)]
struct LuaBuildTemplate<'a> {
    runtimepaths: Vec<String>,
    lua_script: &'a str,
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
    let quoted: Vec<String> = runtimepaths
        .iter()
        .map(|p| lua_single_quoted(p.to_string_lossy().as_ref()))
        .collect();
    LuaBuildTemplate {
        runtimepaths: quoted,
        lua_script,
    }
    .render_once()
    .unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_url_default_cachedir_is_relative_to_repos_namespace() {
        let repo = RepoSource::from_str("https://gitlab.com/owner/plugin").unwrap();

        assert_eq!(
            repo.default_cachedir(),
            PathBuf::from("gitlab.com/owner/plugin")
        );
    }

    #[test]
    fn lua_build_wrapper_wraps_script_and_runtimepaths() {
        let script = "vim.cmd('echo hi')";
        let rtp = vec![PathBuf::from("/path/with'quote"), PathBuf::from("/normal")];
        let out = lua_build_wrapper(script, &rtp);

        // ponytail: locks in the xpcall/ipairs wrapper shape and single-quote escaping.
        assert!(out.starts_with("local ok, err = xpcall(function()\n"));
        assert!(out.contains("for _, rtp in ipairs({"));
        // single-quote escaping applied to runtimepath entries
        assert!(out.contains("  '/path/with\\'quote',\n"));
        assert!(out.contains("  '/normal',\n"));
        // the user script body is embedded verbatim
        assert!(out.contains("vim.cmd('echo hi')"));
        assert!(out.contains("end, debug.traceback)"));
        assert!(out.contains("os.exit(1)"));
    }

    #[test]
    fn lua_build_wrapper_omits_runtimepath_block_when_empty() {
        let out = lua_build_wrapper("do return end", &[]);
        // no ipairs block when there are no runtimepaths
        assert!(!out.contains("ipairs"));
        assert!(out.contains("local ok, err = xpcall(function()"));
        assert!(out.contains("do return end"));
    }

    #[test]
    fn unnamed_script_only_plugin_is_not_source_addressable() {
        let config: Config = toml::from_str(
            r#"
            [[plugins]]
            name = "dep"
            repo = "owner/plugin"

            [[plugins]]
            depends = ["dep"]
            lua_start = "vim.g.script_only = true"
            "#,
        )
        .unwrap();

        let plugins = Plugin::new(config).unwrap().collect::<Vec<_>>();

        let script_only = plugins
            .iter()
            .find(|plugin| plugin.cache.repo.is_none())
            .unwrap();
        assert_eq!(script_only.source_name, None);
        assert_eq!(script_only.dependency_cachedirs.len(), 1);
    }

    #[tokio::test]
    async fn script_only_plugin_load_derives_id_from_hash_input() {
        async fn make_loaded(lua_start: &str) -> LoadedPlugin {
            let config: Config = toml::from_str(&format!(
                r#"
                [[plugins]]
                name = "script_only"
                lua_start = {lua_start:?}
                "#
            ))
            .unwrap();
            let plugin = Plugin::new(config).unwrap().next().unwrap();

            plugin
                .load(false, false, std::env::temp_dir(), None)
                .await
                .unwrap()
                .unwrap()
                .0
        }

        let first = make_loaded("vim.g.script_only = true").await;
        let same = make_loaded("vim.g.script_only = true").await;
        let different_script = make_loaded("vim.g.script_only = false").await;

        assert_eq!(first.plugin_id(), same.plugin_id());
        assert_ne!(first.plugin_id(), different_script.plugin_id());
    }

    #[test]
    fn depending_on_named_script_only_plugin_is_excluded_from_cachedirs() {
        // 依存先が名前付きの script-only（リポジトリなし）の場合、キャッシュディレクトリが
        // 存在しない。依存元の dependency_cachedirs からは除外され、panic しない。
        let config: Config = toml::from_str(
            r#"
            [[plugins]]
            name = "script_only"
            lua_start = "vim.g.script_only = true"

            [[plugins]]
            repo = "owner/repo"
            depends = ["script_only"]
            "#,
        )
        .unwrap();

        let plugins = Plugin::new(config).unwrap().collect::<Vec<_>>();

        let with_repo = plugins
            .iter()
            .find(|plugin| plugin.cache.repo.is_some())
            .unwrap();
        assert!(
            with_repo.dependency_cachedirs.is_empty(),
            "script-only dependency must not contribute a cache dir"
        );
    }
}
