use std::{
    borrow::Cow,
    collections::HashSet,
    ffi::OsStr,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use dag::{TryDag, iterator::DagIteratorMapFuncArgs};
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
    pub(crate) fn default_cachedir(&self) -> PathBuf {
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

    /// token 認証の対象となる GitHub HTTPS URL かどうか。
    /// `GitHub` バリアントは常に true（`github::url()` が HTTPS を生成）。
    /// `Git` バリアントは `https://github.com/` で始まる場合 true。
    pub fn is_github_https(&self) -> bool {
        match self {
            RepoSource::GitHub { .. } => true,
            RepoSource::Git { url, .. } => url.starts_with("https://github.com/"),
        }
    }
}

// 新しい cache layout のパスヘルパ群 (PLANS §5, §15.2)。
// `repos/<repo>/source.git`（fetch 用 object store）と
// `repos/<repo>/worktrees/<snapshot_key>/`（plugin 実体として読む固定 worktree）を基準にする。

/// repo cache の root: `<cache_dir>/<repo.default_cachedir()>`。
pub(super) fn repo_root(cache_dir: &Path, repo: &RepoSource) -> PathBuf {
    cache_dir.join(repo.default_cachedir())
}

/// fetch 対象の Git object store: `<repo_root>/source.git`。
pub(super) fn source_git_dir(repo_root: &Path) -> PathBuf {
    repo_root.join("source.git")
}

/// snapshot worktree の親 directory: `<repo_root>/worktrees`。
pub(super) fn worktrees_dir(repo_root: &Path) -> PathBuf {
    repo_root.join("worktrees")
}

/// plugin 実体として読む固定 worktree: `<repo_root>/worktrees/<snapshot_key>`。
pub(super) fn snapshot_root(repo_root: &Path, snapshot_key: &str) -> PathBuf {
    worktrees_dir(repo_root).join(snapshot_key)
}

/// `worktrees/` に `<oid>` または `<oid>__...` の snapshot が既にあるか。
/// GitFetch（source.git 不要）で既存 snapshot を再利用する判定に使う。
async fn snapshot_exists_for_oid(worktrees: &Path, oid: &str) -> bool {
    let Ok(mut rd) = tokio::fs::read_dir(worktrees).await else {
        return false;
    };
    let prefix_exact = oid.to_string();
    let prefix_under = format!("{oid}__");
    while let Ok(Some(entry)) = rd.next_entry().await {
        if let Some(name) = entry.file_name().to_str()
            && (name == prefix_exact || name.starts_with(&prefix_under))
        {
            return true;
        }
    }
    false
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
    pub fn new(config: Config) -> Result<impl Iterator<Item = Plugin>, Error> {
        let Config { mut plugins } = config;

        // Phase 3A: 内部的同一性 id を各プラグインに算出して格納する。
        // id = name ?? repo basename ?? script 内容ハッシュ（無名 script-only 用）。
        // ユーザーには公開しない内部表現。無名 script-only（start スクリプト等）も許す。
        for plug in &mut plugins {
            plug.id = Some(plug.compute_internal_id());
        }

        // order は (depth, config_index) の複合キー。depth と index は dag 側で
        // 計算して map 関数に渡される。tiebreak は config 出現順。
        let total = plugins.len();
        // 依存先の cachedir を（内部 id → index で）引くための表。
        // 重複チェック・UnknownDependency 検出は dag 側に委譲する。
        let mut id_to_index = hashbrown::HashMap::new();
        for (index, plug) in plugins.iter().enumerate() {
            if let Some(id) = plug.id.as_deref() {
                id_to_index.insert(id.to_string(), index);
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
        semaphore: adaptive_semaphore::AdaptiveSemaphore,
        http_client: reqwest::Client,
    ) -> Result<Option<(LoadedPlugin, Option<(String, String)>)>, Error> {
        use super::util::git;
        use crate::{
            log::{Message, msg},
            rsplug::util::{execute, git::RSPLUG_BUILD_SUCCESS_FILE, truncate},
        };
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

        let CacheConfig {
            repo,
            dotgit,
            build,
            lua_build,
            lua_post_update,
        } = cache;

        let Some(repo) = repo else {
            let loaded = LoadedPlugin {
                source_names: source_name.into_iter().collect(),
                files: HowToPlaceFiles::CopyEachFile(Default::default()),
                lazy_type,
                script,
                order,
                merge_enabled,
                is_plugctl: false,
                dotgit: false,
            };
            return Ok(Some((loaded, None)));
        };

        // `repo` は直後の match で move されるため、論理 identity に使う相対 cachedir を先に捕捉する。
        // 絶対パス (`proj_root`) は配置用であり identity には含めない。
        let cachedir = repo.default_cachedir();
        let cache_dir = cache_dir.as_ref().to_path_buf();
        let r_root = repo_root(&cache_dir, &repo);
        let source_git = source_git_dir(&r_root);
        let worktrees = worktrees_dir(&r_root);
        let url: Arc<str> = Arc::from(repo.url());

        // GitHub HTTPS URL かつ環境変数に token があれば認証フェッチする。
        let token = if repo.is_github_https() {
            util::github::token().map(Arc::<str>::from)
        } else {
            None
        };

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

        // --- target commit 解決 (PLANS §8 step 5) ---
        // まずインストール状態（= 既存 snapshot worktree の有無）で分岐する。
        //   - locked : lockfile の rev をそのまま使う（cache 不足は後段でエラー）。
        //   - update : インストール済みならリモートの最新を fetch して更新する。
        //              ※未インストールは対象外（スキップ）。`-u` 単独で新規 install はしない。
        //   - install: 未インストールならリモートから新規 fetch する。
        //   - それ以外(通常起動): 既存 snapshot の commit をそのまま使い、無ければスキップ。
        let (oid, was_updated, was_installed) = if let Some(locked_rev) = locked_rev.as_deref() {
            if !is_full_hex_hash(locked_rev) {
                return Err(invalid_data(format!(
                    "Locked revision must be full hash for {}: got {}",
                    url, locked_rev
                )));
            }
            (
                Oid::from_str(locked_rev).map_err(Error::Git2)?,
                false,
                false,
            )
        } else {
            match latest_snapshot_oid(&worktrees).await? {
                Some(existing) if update => {
                    let _permit = semaphore.acquire().await;
                    msg(Message::Cache("Updating", url.clone()));
                    let oid = resolve_remote_oid(&http_client, &url, &rev, &token).await?;
                    msg(Message::Cache("Updating:done", url.clone()));
                    // リモートの最新 rev が既存 snapshot と異なれば「実際に更新された」。
                    (oid, existing != oid, false)
                }
                Some(existing) => (existing, false, false),
                None if install => {
                    let _permit = semaphore.acquire().await;
                    msg(Message::Cache("Updating", url.clone()));
                    let oid = resolve_remote_oid(&http_client, &url, &rev, &token).await?;
                    msg(Message::Cache("Updating:done", url.clone()));
                    (oid, false, true)
                }
                None => {
                    // 未インストール。install も update(既存更新) も対象外なのでスキップ。
                    msg(Message::PluginNotInstalled(display_name(
                        &source_name,
                        &logid,
                    )));
                    return Ok(None);
                }
            }
        };
        let head_rev_str = oid.to_string();

        let _running_guard = RunningGuard::new();

        // --- フェッチ戦略の選択 (Phase 2) ---
        // token があって GitHub HTTPS URL なら TarballFetch（source.git 不要）。
        // それ以外は従来の GitFetch（source.git）パス。TarballFetch 失敗時は GitFetch にフォールバック。
        // dotgit=true は .git 複製が必要なため TarballFetch（.git を作れない）を無効化し GitFetch に強制する。
        let use_tarball = !dotgit && token.is_some() && util::github::supports_tarball(&url);
        let locked = locked_rev.is_some();

        // フェッチヘルパーへ渡すコンテキスト。GitFetch/TarballFetch で共有し、引数過多を避ける。
        let ctx = FetchCtx {
            url: &url,
            oid,
            source_git: &source_git,
            token: &token,
            source_name: &source_name,
            semaphore: &semaphore,
            http_client: &http_client,
            install,
            update,
            locked,
            logid: &logid,
        };

        // GitFetch（非 tarball）の場合だけ source.git を確保する。ただし既存 snapshot
        // （worktrees/ に <oid> または <oid>__... がある）があれば source.git 不要で再利用する
        // （dotgit=true で TarballFetch 由来の snapshot を流用する移行ケース等）。
        if !use_tarball
            && !snapshot_exists_for_oid(&worktrees, &head_rev_str).await
            && !ensure_source_git(&ctx).await?
        {
            return Ok(None);
        }

        // --- snapshot worktree の用意 (PLANS §7, §8 step 7-14) ---
        tokio::fs::create_dir_all(&worktrees).await?;
        let has_build = !build.is_empty() || lua_build.is_some();
        let pre_identity = RepoSnapshotIdentity::new(
            cachedir.clone(),
            head_rev_str.as_bytes().to_vec(),
            None,
            Arc::from(build.as_slice()),
            lua_build.as_deref().map(Into::into),
        );
        let final_root: Arc<Path> = Arc::from(snapshot_root(&r_root, &pre_identity.snapshot_key()));

        let (snapshot_root_path, repository): (Arc<Path>, MaterializedRepo) = if final_root.exists()
        {
            // 同一 key の snapshot が既存 → 再利用（build/lua_post_update をスキップ）。
            // Phase 7 以降の tarball snapshot は `.git` 無し → Plain、旧 snapshot は Git。
            let repo = if final_root.join(".git").exists() {
                MaterializedRepo::Git(git::open(final_root.clone()).await?)
            } else {
                MaterializedRepo::Plain
            };
            (final_root.clone(), repo)
        } else if has_build {
            // build がある: 一時 worktree で build → dirty 計算 → final key → rename/reuse。
            // dirty_diff を snapshot_key に含めるため、build 後でないと最終 key が確定しない。
            let building = building_worktree_dir(&worktrees);
            let _ = tokio::fs::remove_dir_all(&building).await;
            let building: Arc<Path> = Arc::from(building);
            let repo = match materialize(&ctx, building.as_ref(), use_tarball).await? {
                Some(r) => r,
                None => return Ok(None),
            };
            // tarball（Plain）かを記憶: rename 後 Git は開き直すが Plain は git::open 不要。
            let is_plain = matches!(repo, MaterializedRepo::Plain);

            // lua_post_update は update 検知時のみ building worktree で実行。
            if update && let Some(lua_post_update) = lua_post_update.as_deref() {
                let rtp = build_runtimepaths(&building, &cache_dir, &dependency_cachedirs).await;
                let id = Arc::new(format!("{logid} (lua_post_update)"));
                let result: Result<(), Error> = {
                    let id = id.clone();
                    async {
                        let path = create_lua_build_script(lua_post_update, &rtp).await?;
                        let _build = super::util::resources::build().await?;
                        let code = execute(
                            lua_build_nvim_command(path.as_os_str()),
                            building.clone(),
                            move |(stdtype, line)| {
                                msg(Message::CacheBuildProgress {
                                    id: id.clone(),
                                    stdtype,
                                    line,
                                });
                            },
                        )
                        .await;
                        let _ = tokio::fs::remove_file(&path).await;
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

            let rtp = build_runtimepaths(&building, &cache_dir, &dependency_cachedirs).await;
            run_repo_build(
                &build,
                lua_build.as_deref(),
                building.clone(),
                rtp,
                &logid,
                &repo_name,
            )
            .await?;

            // build 後 dirty を反映した最終 identity → key
            // final_root（= pre_identity の key）へ原子リネーム。失敗しても final_root は作られない。
            drop(repo);
            tokio::fs::rename(building.as_ref(), final_root.as_ref()).await?;
            // Plain は `.git` 無しで開き直せない。Git のみ git::open する。
            let repo = if is_plain {
                MaterializedRepo::Plain
            } else {
                MaterializedRepo::Git(git::open(final_root.clone()).await?)
            };
            (final_root, repo)
        } else {
            // build 無し: key は確定（dirty=None）。final があれば再利用、なければ作成。
            let repo = match materialize(&ctx, final_root.as_ref(), use_tarball).await? {
                Some(r) => r,
                None => return Ok(None),
            };
            (final_root.clone(), repo)
        };

        // --- 最終 identity と build 成功 marker (snapshot_root 単位, PLANS §11) ---
        let identity = build_repo_snapshot_identity(
            &repository,
            snapshot_root_path.as_ref(),
            cachedir.clone(),
            head_rev_str.as_bytes().to_vec(),
            &build,
            lua_build.as_deref(),
        )
        .await?;
        if has_build {
            tokio::fs::write(
                snapshot_root_path.join(RSPLUG_BUILD_SUCCESS_FILE),
                identity.plugin_id().as_str().as_bytes(),
            )
            .await?;
        }

        // Phase 2: snapshot が ready になったので manifest を記録する（best-effort cache）。
        // 以降の merge/copy 計画は manifest からパス集合を引ける（Part B）。欠損時は filesystem
        // fallback。既存 snapshot の再利用時は書き込みを省き、重複 walk を避ける。
        let manifest_path = snapshot_root_path.join(MANIFEST_FILE);
        if !tokio::fs::try_exists(&manifest_path).await.unwrap_or(false) {
            let _ = SnapshotManifest::build_and_write(
                snapshot_root_path.as_ref(),
                dotgit,
                RSPLUG_BUILD_SUCCESS_FILE,
            )
            .await;
        }
        // per-repo latest-snapshot index: `<repo>/latest-snapshot` に snapshot_key を記録する。
        // worktrees/ を scan せずに最新 snapshot を特定できる（best-effort）。
        if let (Some(worktrees), Some(key)) =
            (snapshot_root_path.parent(), snapshot_root_path.file_name())
            && let Some(repo_root) = worktrees.parent()
        {
            let key = key.to_string_lossy();
            let _ = tokio::fs::write(repo_root.join("latest-snapshot"), key.as_bytes()).await;
        }

        let filesource = Arc::new(FileSource::Directory {
            path: snapshot_root_path.clone(),
        });
        // ls-files 列挙を廃止し、snapshot ルート直下を read_dir で1階層列挙する。
        // ディレクトリ（lua/plugin 等）も1エントリにまとめ、install で copy_tree する
        //（ファイル数分の syscall を削減）。`doc` だけは盗み集約のため個別ファイルに展開する（下記）。
        // target/ 等の build 成果物は ignore 対象外なので pack に残る（旧 ls_files_with_untracked と同等）。
        // .rsplug_build_success は ignore.gitignore で除外される。`.git` は通常 ignore 対象だが、
        // dotgit=true のときは例外扱いせず通常ディレクトリと同じくエントリに含める
        //（pack への copy・plugin_id への反映は他のディレクトリと同一経路。
        // snapshot に `.git` が無い場合は install で検知し警告）。
        let mut entries: Vec<PathBuf> = Vec::new();
        {
            let mut rd = tokio::fs::read_dir(snapshot_root_path.as_ref()).await?;
            while let Some(entry) = rd.next_entry().await? {
                entries.push(PathBuf::from(entry.file_name()));
            }
        }
        entries.sort(); // 決定論的順序（plugin_id 安定化）
        let mut lazy_type = lazy_type.clone();
        for luam in extract_unique_lua_modules_from_snapshot(snapshot_root_path.as_ref()).await {
            lazy_type &= LoadEvent::LuaModule(LuaModule(luam.into()));
        }
        // 各トップレベルエントリを配置対象に組み立てる。`doc` だけは例外:
        // 「盗んで」`_rsplug:doc` start プラグインへ集約するため、sealed-dir 1エントリではなく
        // 個別ファイル（`doc/<rel>`）に展開する（`PlugCtl::create` の `starts_with("doc/")` 盗みを効かせる）。
        // それ以外は sealed-dir のまま（install で copy_tree が clonefile/per-file copy で配置）。
        let mut file_entries: Vec<(PathBuf, FileItem)> = Vec::with_capacity(entries.len());
        for name in &entries {
            // dotgit=true なら `.git` を ignore から救出して通常エントリに含める。
            if !(dotgit && name == Path::new(".git") || !merge.ignore.matched(name)) {
                continue;
            }
            if name == Path::new("doc") {
                file_entries.extend(
                    doc_file_entries(snapshot_root_path.as_ref(), &filesource, &identity).await,
                );
            } else {
                file_entries.push((
                    name.clone(),
                    FileItem::new(
                        filesource.clone(),
                        FileIdentity::RepoFile(RepoFileIdentity::new(
                            identity.clone(),
                            name.clone(),
                        )),
                        MergeType::Conflict,
                    ),
                ));
            }
        }
        let files: HowToPlaceFiles =
            HowToPlaceFiles::CopyEachFile(file_entries.into_iter().collect());

        // ロード成功が確定したので、実際に更新/新規インストールされたプラグインを
        // サマリーへ通知する。早帰り(Ok(None))経路には到達しない＝スキップしたものは報告しない。
        if was_updated {
            msg(Message::PluginUpdated(display_name(&source_name, &logid)));
        } else if was_installed {
            msg(Message::PluginInstalled(display_name(&source_name, &logid)));
        }

        let loaded = LoadedPlugin {
            source_names: source_name.into_iter().collect(),
            files,
            lazy_type,
            script,
            order,
            merge_enabled,
            is_plugctl: false,
            dotgit,
        };
        let lock_info = Some((url.to_string(), head_rev_str));

        Ok(Some((loaded, lock_info)))
    }
}

/// フェッチに必要なコンテキスト（GitFetch / TarballFetch 共通）。
/// `Plugin::load` で組み立ててヘルパーに渡すことで引数過多を避ける。
struct FetchCtx<'a> {
    url: &'a Arc<str>,
    oid: Oid,
    source_git: &'a Path,
    token: &'a Option<Arc<str>>,
    source_name: &'a Option<String>,
    semaphore: &'a adaptive_semaphore::AdaptiveSemaphore,
    http_client: &'a reqwest::Client,
    install: bool,
    update: bool,
    locked: bool,
    logid: &'a str,
}

/// リモートの最新コミットハッシュを解決する。
/// GitHub HTTPS + token の場合は REST API（軽量・1リクエスト）を試行し、
/// 失敗・ワイルドカード ref・レートリミット残量不足時は git protocol にフォールバックする。
/// それ以外は常に git protocol (`ls_remote`) を使う。
async fn resolve_remote_oid(
    http_client: &reqwest::Client,
    url: &Arc<str>,
    rev: &Option<Arc<str>>,
    token: &Option<Arc<str>>,
) -> Result<Oid, Error> {
    use super::util::{git, github};

    // GitHub HTTPS + token なら REST API を試行
    if github::supports_tarball(url) && token.is_some() {
        // ワイルドカード ref（`*` を含む）は REST API で解決できない → git protocol
        let is_wildcard = rev.as_deref().is_some_and(|r| r.contains('*'));
        if !is_wildcard {
            match github::resolve_rev_via_api(http_client, url, rev.as_deref(), token.as_deref())
                .await
            {
                Ok(oid_str) => {
                    return Oid::from_str(&oid_str).map_err(Error::Git2);
                }
                Err(github::ApiError::RateLimited) => {
                    // レートリミット残量不足 → git protocol にフォールバック
                }
                Err(github::ApiError::Other(_)) => {
                    // API エラー（404, ネットワーク等）→ git protocol にフォールバック
                }
            }
        }
    }

    // フォールバック: git smart HTTP protocol (ls_remote)
    git::ls_remote(url.clone(), rev.clone(), token.clone()).await
}

/// 戻り値 `Ok(true)` = source.git に oid を fetch 済み、`Ok(false)` = 未インストールなのでスキップ。
/// `Err` = locked で cache 不足、または fetch 失敗。
async fn ensure_source_git(ctx: &FetchCtx<'_>) -> Result<bool, Error> {
    use super::util::git;
    use crate::log::{Message, msg};

    let mut repo = match git::open_source(ctx.source_git).await {
        Ok(r) => r,
        Err(_) if ctx.install || ctx.update => {
            let _git = super::util::resources::git().await?;
            msg(Message::Cache("Initializing", ctx.url.clone()));
            git::init_source(ctx.source_git, ctx.url).await?
        }
        Err(_) if ctx.locked => {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Missing cached repository for locked revision: {}", ctx.url),
            )));
        }
        Err(_) => {
            msg(Message::PluginNotInstalled(display_name(
                ctx.source_name,
                ctx.logid,
            )));
            return Ok(false);
        }
    };
    {
        let _git = super::util::resources::git().await?;
        msg(Message::Cache("Fetching", ctx.url.clone()));
        repo.fetch_oid(ctx.oid, ctx.token.clone()).await?;
        msg(Message::Cache("Fetching:done", ctx.url.clone()));
    }
    Ok(true)
}

/// 実体化された snapshot のバックエンド。
/// `Git` = source.git 経由の worktree（`.git` 有り）。`Plain` = tarball 展開のみ（`.git` 無し、Phase 7）。
/// identity/dirty の計算が異なる: `Git` は git diff、`Plain` はファイル内容ハッシュ。
enum MaterializedRepo {
    Git(util::git::Repository),
    Plain,
}

/// snapshot worktree を `dest` に実体化し、そのバックエンド [`MaterializedRepo`] を返す。
/// `use_tarball` なら TarballFetch を試行（成功すれば `.git` を作らない `Plain`）、
/// 失敗時は GitFetch（source.git）にフォールバック（`Git`）。
/// `use_tarball` でない場合は source.git 経由で init_snapshot する（呼出元で source.git 確保済みが前提、`Git`）。
/// 戻り値 `Ok(None)` = 未インストールスキップ。
async fn materialize(
    ctx: &FetchCtx<'_>,
    dest: &Path,
    use_tarball: bool,
) -> Result<Option<MaterializedRepo>, Error> {
    use super::util::{fetch::TarballFetch, git};
    use crate::log::{Message, msg};

    if use_tarball {
        let tarball_ok = {
            let _permit = ctx.semaphore.acquire().await;
            msg(Message::Cache("Fetching", ctx.url.clone()));
            let head_rev = ctx.oid.to_string();
            match TarballFetch
                .fetch_to_snapshot(
                    ctx.http_client,
                    ctx.url.as_ref(),
                    &head_rev,
                    dest,
                    ctx.token.as_deref(),
                )
                .await
            {
                Ok(()) => {
                    msg(Message::Cache("Fetching:done", ctx.url.clone()));
                    true
                }
                Err(_) => false,
            }
        };
        if tarball_ok {
            // Phase 7: tarball は `.git` を作らない。identity/dirty は内容ハッシュで計算。
            return Ok(Some(MaterializedRepo::Plain));
        }
        // TarballFetch 失敗 → GitFetch（source.git）にフォールバック
        if !ensure_source_git(ctx).await? {
            return Ok(None);
        }
    }

    let _git = super::util::resources::git().await?;
    Ok(Some(MaterializedRepo::Git(
        git::init_snapshot(dest.to_path_buf(), ctx.source_git, ctx.oid).await?,
    )))
}

/// 未インストール警告の表示名。`source_name`（`dep_name` 由来）を優先し、
/// script-only プラグインなどで `None` の場合は logid にフォールバックする。
fn display_name(source_name: &Option<String>, logid: &str) -> Arc<str> {
    match source_name {
        Some(name) => Arc::from(name.as_str()),
        None => Arc::from(logid),
    }
}

/// Loading 全体進捗バーの「稼働中」区間を +1 / -1 する RAII ガード。
///
/// fetch 許可を取得してネットワーク実作業に入った時点で `new()` が +1 し
/// （`LoadPluginRunning`）、`load()` の戻りとともに `Drop` が -1 する
/// （`LoadPluginRunningDone`）。成功・エラー全経路で対になる。
struct RunningGuard;

impl RunningGuard {
    fn new() -> Self {
        crate::log::msg(crate::log::Message::LoadPluginRunning);
        RunningGuard
    }
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        crate::log::msg(crate::log::Message::LoadPluginRunningDone);
    }
}

/// build 中の一時 worktree: `worktrees/.building-<pid>-<nonce>` (PLANS §7)。
/// `worktrees/` 内の hidden directory なので scan 対象にならない（先頭 `.`）。
fn building_worktree_dir(worktrees: &Path) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    worktrees.join(format!(".building-{}-{}", std::process::id(), nonce))
}

/// repo の `build`(sh) と `lua_build` を `workdir` で実行する。
/// `runtimepaths` は lua_build 実行時に nvim の runtimepath に追加する（依存先 snapshot 含む）。
async fn run_repo_build(
    build: &[String],
    lua_build: Option<&str>,
    workdir: Arc<Path>,
    runtimepaths: Vec<PathBuf>,
    logid: &str,
    repo_name: &Arc<str>,
) -> Result<(), Error> {
    use crate::{
        log::{Message, msg},
        rsplug::util::execute,
    };
    // build プロセスは CPU+IO 重めなので CPU の半分（最低1）に制限する（Phase 1）。
    // run_repo_build は build(sh) と lua_build を順に実行するので、関数先頭で1つ取得する。
    let _build = super::util::resources::build().await?;

    if !build.is_empty() {
        let id = Arc::new(format!("{logid} (sh)"));
        let result: Result<(), Error> = {
            let id = id.clone();
            let build = build.to_vec();
            async {
                let code = execute(build.iter(), workdir.clone(), move |(stdtype, line)| {
                    msg(Message::CacheBuildProgress {
                        id: id.clone(),
                        stdtype,
                        line,
                    });
                })
                .await?;
                if code != 0 {
                    return Err(Error::BuildScriptFailed {
                        code,
                        build,
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

    if let Some(lua_build) = lua_build {
        let id = Arc::new(format!("{logid} (lua)"));
        let result: Result<(), Error> = {
            let id = id.clone();
            async {
                let lua_build_path = create_lua_build_script(lua_build, &runtimepaths).await?;
                let code = execute(
                    lua_build_nvim_command(lua_build_path.as_os_str()),
                    workdir.clone(),
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

    Ok(())
}

/// build 用 runtimepath を組み立てる: 自 snapshot を先頭に、依存先 snapshot を重複なしで追加。
/// 依存先 snapshot は best-effort で各依存先 repo の `worktrees/` から最新を探す
/// （DAG は build 順序を表さないため、ロード順に依存しない）。
async fn build_runtimepaths(
    own: &Path,
    cache_dir: &Path,
    dependency_cachedirs: &[PathBuf],
) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    seen.insert(own.to_path_buf());
    out.push(own.to_path_buf());
    for dep_cachedir in dependency_cachedirs {
        let dep_worktrees = cache_dir.join(dep_cachedir).join("worktrees");
        if let Some(dep_snap) = latest_snapshot_dir(&dep_worktrees).await
            && seen.insert(dep_snap.clone())
        {
            out.push(dep_snap);
        }
    }
    out
}

/// `worktrees/` 配下の snapshot のうち最新（mtime 順）の path を返す。
/// hidden (`.building-*`) と先頭 40hex(commit) でない名前は無視する。
async fn latest_snapshot_dir(worktrees: &Path) -> Option<PathBuf> {
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    let Ok(mut rd) = tokio::fs::read_dir(worktrees).await else {
        return None;
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let Some(commit) = name.split("__").next() else {
            continue;
        };
        if Oid::from_str(commit).is_err() {
            continue;
        }
        let mtime = tokio::fs::metadata(entry.path())
            .await
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        match &newest {
            Some((t, _)) if *t >= mtime => {}
            _ => newest = Some((mtime, entry.path())),
        }
    }
    newest.map(|(_, p)| p)
}

/// 既存 snapshot worktree のうち最新（mtime 順）の commit を返す (PLANS §8 step 5 の
/// install/update/locked 以外の case)。snapshot_dir 名の先頭 40hex を commit とみなす。
async fn latest_snapshot_oid(worktrees: &Path) -> Result<Option<Oid>, Error> {
    let Some(dir) = latest_snapshot_dir(worktrees).await else {
        return Ok(None);
    };
    let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
        return Ok(None);
    };
    let Some(commit) = name.split("__").next() else {
        return Ok(None);
    };
    Ok(Oid::from_str(commit).ok())
}

/// snapshot の `lua/` 直下から Lua module 名を抽出する。
/// `lua/<name>`（ディレクトリ）or `lua/<name>.lua`（ファイル）の stem を取る。
/// ls-files 廃止に伴い read_dir で取得する（`lua/` が無ければ空）。
/// `doc/` を再帰走査し、個別ファイルエントリ（key = `doc/<rel>`）を返す。
/// `PlugCtl::create` が `doc/**` を盗んで `_rsplug:doc` start プラグインへ集約できるよう、
/// `doc` を sealed-dir 1エントリではなく個別ファイルとして列挙する
///（origin/main `collect_doc_files_from_root` 相当。同期 IO を避けるため async で走査）。
/// doc_root が無い/ディレクトリでなければ空。エラーは寛容に skip し得た分だけ返す。
async fn doc_file_entries(
    snapshot_root: &Path,
    filesource: &Arc<FileSource>,
    identity: &RepoSnapshotIdentity,
) -> Vec<(PathBuf, FileItem)> {
    let doc_root = snapshot_root.join("doc");
    let is_dir = tokio::fs::metadata(&doc_root)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false);
    if !is_dir {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut seen_dirs: hashbrown::HashSet<PathBuf> = hashbrown::HashSet::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(doc_root.clone(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 128 {
            continue;
        }
        // symlink ループ保護（origin/main collect_doc_files_from_root 準拠）。
        if let Ok(canonical) = tokio::fs::canonicalize(&dir).await
            && !seen_dirs.insert(canonical)
        {
            continue;
        }
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            let ft = match entry.file_type().await {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push((path, depth + 1));
                continue;
            }
            let is_file = if ft.is_symlink() {
                tokio::fs::metadata(&path)
                    .await
                    .map(|m| m.is_file())
                    .unwrap_or(false)
            } else {
                ft.is_file()
            };
            if !is_file {
                continue;
            }
            let Ok(rel) = path.strip_prefix(&doc_root) else {
                continue;
            };
            let key = PathBuf::from("doc").join(rel);
            out.push((
                key.clone(),
                FileItem::new(
                    filesource.clone(),
                    FileIdentity::RepoFile(RepoFileIdentity::new(identity.clone(), key)),
                    MergeType::Conflict,
                ),
            ));
        }
    }
    out
}

async fn extract_unique_lua_modules_from_snapshot(snapshot_root: &Path) -> Vec<String> {
    let mut rd = match tokio::fs::read_dir(snapshot_root.join("lua")).await {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    let mut seen = hashbrown::HashSet::new();
    let mut out = Vec::new();
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = match entry.file_name().to_str() {
            Some(n) => n.to_string(),
            None => continue,
        };
        let stem = if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            name
        } else {
            Path::new(&name)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string()
        };
        if !stem.is_empty() && seen.insert(stem.clone()) {
            out.push(stem);
        }
    }
    out
}

async fn build_repo_snapshot_identity(
    repo: &MaterializedRepo,
    snapshot_root: &Path,
    repo_cache_dir: PathBuf,
    head_rev: Vec<u8>,
    build: &[String],
    lua_build: Option<&str>,
) -> Result<RepoSnapshotIdentity, Error> {
    let has_build = !build.is_empty() || lua_build.is_some();
    // Git は git diff、tarball（Plain）は build があればファイル内容ハッシュ（Phase 7）。
    let dirty_diff = match repo {
        MaterializedRepo::Git(g) => {
            if g.is_dirty().await? {
                Some(g.diff_hash().await?)
            } else {
                None
            }
        }
        MaterializedRepo::Plain if has_build => {
            Some(util::dirty_diff_from_content(snapshot_root).await?)
        }
        MaterializedRepo::Plain => None,
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

fn lua_build_nvim_command(lua_script_path: &OsStr) -> [Cow<'_, OsStr>; 8] {
    [
        Cow::Borrowed(OsStr::new("nvim")),
        Cow::Borrowed(OsStr::new("--headless")),
        Cow::Borrowed(OsStr::new("-u")),
        Cow::Borrowed(OsStr::new("NONE")),
        // ShaDa 無効化: 並列 build で複数 nvim が main.shada を奪い合い E138 が出るのを防ぐ。
        Cow::Borrowed(OsStr::new("-i")),
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
    fn is_github_https_classifies_correctly() {
        // GitHub shorthand — always HTTPS
        assert!(
            RepoSource::from_str("owner/repo")
                .unwrap()
                .is_github_https()
        );
        assert!(
            RepoSource::from_str("owner/repo@v1.0")
                .unwrap()
                .is_github_https()
        );

        // Git variant — HTTPS GitHub URL
        assert!(
            RepoSource::from_str("https://github.com/owner/repo.git")
                .unwrap()
                .is_github_https()
        );

        // Git variant — non-GitHub HTTPS
        assert!(
            !RepoSource::from_str("https://gitlab.com/owner/repo.git")
                .unwrap()
                .is_github_https()
        );

        // Git variant — SSH (not HTTPS, even if GitHub)
        assert!(
            !RepoSource::from_str("ssh://git@github.com/owner/repo.git")
                .unwrap()
                .is_github_https()
        );
    }

    #[test]
    fn path_helpers_lay_out_source_git_and_worktrees() {
        let repo = RepoSource::from_str("owner/repo").unwrap();
        let cache_dir = Path::new("cache");
        let root = repo_root(cache_dir, &repo);
        assert_eq!(root, PathBuf::from("cache/github.com/owner/repo"));
        assert_eq!(
            source_git_dir(&root),
            PathBuf::from("cache/github.com/owner/repo/source.git")
        );
        assert_eq!(
            worktrees_dir(&root),
            PathBuf::from("cache/github.com/owner/repo/worktrees")
        );
        assert_eq!(
            snapshot_root(&root, "deadbeef"),
            PathBuf::from("cache/github.com/owner/repo/worktrees/deadbeef")
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
        assert!(out.contains("\t'/path/with\\'quote',\n"));
        assert!(out.contains("\t'/normal',\n"));
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
    fn script_only_plugin_with_id_is_source_addressable_and_resolves_deps() {
        let config: Config = toml::from_str(
            r#"
            [[plugins]]
            name = "dep"
            repo = "owner/plugin"

            [[plugins]]
            name = "scriptdep"
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
        // name を持つ script-only は source addressable。
        assert_eq!(script_only.source_name.as_deref(), Some("scriptdep"));
        assert_eq!(script_only.dependency_cachedirs.len(), 1);
    }

    #[test]
    fn unnamed_script_only_plugin_is_allowed() {
        // Phase 3A: 無名 script-only（start スクリプト等、参照されないもの）は許容。
        // 内部 id は内容ハッシュから生成され、Plugin::new は成功する。
        let config: Config = toml::from_str(
            r#"
            [[plugins]]
            lua_start = "vim.g.anonymous = true"
            "#,
        )
        .unwrap();
        let plugins = Plugin::new(config).unwrap().collect::<Vec<_>>();
        assert_eq!(plugins.len(), 1);
        // dep_name 無し → source_name は None（on_source で参照されない）。
        assert_eq!(plugins[0].source_name, None);
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
                .load(
                    false,
                    false,
                    std::env::temp_dir(),
                    None,
                    adaptive_semaphore::AdaptiveSemaphore::new(),
                    reqwest::Client::new(),
                )
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

    #[tokio::test]
    async fn load_creates_snapshot_worktree_and_reuses_it() {
        // 実 git で install → source.git + worktrees/<key> を作り、RepoSnapshotLink target が
        // snapshot を指し、再実行で同じ snapshot を再利用することを検証する (PLANS §15.11)。
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("rsplug-load-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let remote = dir.join("remote");
        let cache = dir.join("cache");
        std::fs::create_dir_all(remote.join("plugin")).unwrap();
        std::fs::write(remote.join("plugin/init.vim"), "\"x\n").unwrap();
        let git = |args: &[&str]| {
            let s = Command::new("git")
                .current_dir(&remote)
                .args(args)
                .status()
                .unwrap();
            assert!(s.success(), "git {:?} failed", args);
        };
        git(&["init", "-q"]);
        git(&["add", "-A"]);
        let commit = Command::new("git")
            .current_dir(&remote)
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "init",
            ])
            .status()
            .unwrap();
        assert!(commit.success());
        let oid = String::from_utf8(
            Command::new("git")
                .current_dir(&remote)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let oid = oid.trim().to_string();
        let url = format!("file://{}", remote.display());

        let config: Config = toml::from_str(&format!(
            r#"
            [[plugins]]
            repo = "{url}"
            sym = true
            "#
        ))
        .unwrap();
        let plugin = Plugin::new(config).unwrap().next().unwrap();
        let cachedir = plugin.cache.repo.as_ref().unwrap().default_cachedir();
        let repo_root = cache.join(&cachedir);
        let (loaded, lock_info) = plugin
            .load(
                true,
                false,
                &cache,
                None,
                adaptive_semaphore::AdaptiveSemaphore::new(),
                reqwest::Client::new(),
            )
            .await
            .unwrap()
            .unwrap();

        // source.git と worktrees/<key> が作られる
        assert!(
            repo_root.join("source.git").is_dir(),
            "source.git missing at {}",
            repo_root.join("source.git").display()
        );
        // to_sym なので RepoSnapshotLink。target は worktrees/ 配下の固定 snapshot。
        let target = loaded
            .snapshot_root()
            .expect("repo plugin has snapshot_root");
        assert!(
            target.starts_with(repo_root.join("worktrees")),
            "target {} not under worktrees",
            target.display()
        );
        assert!(target.join("plugin/init.vim").is_file());
        // lock_info に full commit SHA
        assert_eq!(lock_info.expect("lock_info").1, oid);

        // 同じ入力で再 load すると同じ snapshot を再利用する（key が一致）
        let config2: Config = toml::from_str(&format!(
            r#"
            [[plugins]]
            repo = "{url}"
            sym = true
            "#
        ))
        .unwrap();
        let plugin2 = Plugin::new(config2).unwrap().next().unwrap();
        let (loaded2, _) = plugin2
            .load(
                true,
                false,
                &cache,
                None,
                adaptive_semaphore::AdaptiveSemaphore::new(),
                reqwest::Client::new(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded2.snapshot_root(),
            Some(target),
            "re-load should reuse the same snapshot"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn update_creates_new_snapshot_without_moving_old_one() {
        // install して commit A の snapshot を作り、remote を commit B に進めて --update すると:
        // 古い snapshot (A) は別 commit に動かず、新しい snapshot (B) が別途作られる。
        // これが本設計の主目的 (PLANS §15.11 item 4, §16.1)。
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("rsplug-load-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let remote = dir.join("remote");
        let cache = dir.join("cache");
        std::fs::create_dir_all(remote.join("plugin")).unwrap();
        std::fs::write(remote.join("plugin/a.vim"), "\"A\n").unwrap();
        let git = |args: &[&str]| {
            let s = Command::new("git")
                .current_dir(&remote)
                .args(args)
                .status()
                .unwrap();
            assert!(s.success(), "git {:?} failed", args);
        };
        let commit = || {
            let s = Command::new("git")
                .current_dir(&remote)
                .args([
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-q",
                    "-m",
                    "x",
                ])
                .status()
                .unwrap();
            assert!(s.success());
        };
        let head = || {
            String::from_utf8(
                Command::new("git")
                    .current_dir(&remote)
                    .args(["rev-parse", "HEAD"])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap()
            .trim()
            .to_string()
        };
        git(&["init", "-q"]);
        git(&["add", "-A"]);
        commit();
        let oid_a = head();
        let url = format!("file://{}", remote.display());

        // install → snapshot A
        let config: Config = toml::from_str(&format!(
            r#"
            [[plugins]]
            repo = "{url}"
            sym = true
            "#
        ))
        .unwrap();
        let plugin = Plugin::new(config).unwrap().next().unwrap();
        let (loaded_a, _) = plugin
            .load(
                true,
                false,
                &cache,
                None,
                adaptive_semaphore::AdaptiveSemaphore::new(),
                reqwest::Client::new(),
            )
            .await
            .unwrap()
            .unwrap();
        let snap_a = loaded_a.snapshot_root().expect("snapshot_root");
        assert_eq!(
            std::fs::read_to_string(snap_a.join("plugin/a.vim")).unwrap(),
            "\"A\n"
        );

        // remote を commit B に進める
        std::fs::write(remote.join("plugin/a.vim"), "\"B\n").unwrap();
        git(&["add", "-A"]);
        commit();
        let oid_b = head();
        assert_ne!(oid_a, oid_b);

        // update → snapshot B（A とは別）
        let config2: Config = toml::from_str(&format!(
            r#"
            [[plugins]]
            repo = "{url}"
            sym = true
            "#
        ))
        .unwrap();
        let plugin2 = Plugin::new(config2).unwrap().next().unwrap();
        let (loaded_b, lock_b) = plugin2
            .load(
                false,
                true,
                &cache,
                None,
                adaptive_semaphore::AdaptiveSemaphore::new(),
                reqwest::Client::new(),
            )
            .await
            .unwrap()
            .unwrap();
        let snap_b = loaded_b.snapshot_root().expect("snapshot_root");
        assert_ne!(snap_b, snap_a, "update should produce a different snapshot");
        assert_eq!(lock_b.expect("lock_info").1, oid_b);
        assert_eq!(
            std::fs::read_to_string(snap_b.join("plugin/a.vim")).unwrap(),
            "\"B\n"
        );

        // 古い generation の snapshot (A) は別 commit に動いていない — 本設計の主目的
        assert!(
            snap_a.join("plugin/a.vim").is_file(),
            "old generation snapshot must survive the update"
        );
        assert_eq!(
            std::fs::read_to_string(snap_a.join("plugin/a.vim")).unwrap(),
            "\"A\n",
            "old snapshot content must not move on update"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn update_alone_does_not_install_uninstalled_repo() {
        // 未インストールの repo に対し install=false, update=true で load すると:
        // 新規 install せず Ok(None) を返し、source.git も snapshot も作らない。
        // `-u` は既存(インストール済み)対象の更新のみで、新規 install は `-i` の役割。
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("rsplug-update-only-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let remote = dir.join("remote");
        let cache = dir.join("cache");
        std::fs::create_dir_all(remote.join("plugin")).unwrap();
        std::fs::write(remote.join("plugin/init.vim"), "\"x\n").unwrap();
        let git = |args: &[&str]| {
            let s = Command::new("git")
                .current_dir(&remote)
                .args(args)
                .status()
                .unwrap();
            assert!(s.success(), "git {:?} failed", args);
        };
        git(&["init", "-q"]);
        git(&["add", "-A"]);
        let commit = Command::new("git")
            .current_dir(&remote)
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "init",
            ])
            .status()
            .unwrap();
        assert!(commit.success());
        let url = format!("file://{}", remote.display());

        let mk_cfg = || {
            toml::from_str::<Config>(&format!(
                r#"
            [[plugins]]
            repo = "{url}"
            sym = true
            "#
            ))
            .unwrap()
        };

        // repo_root は load ごとに変化しないので先に算出（Config は Clone できない）。
        let repo_root = {
            let p = Plugin::new(mk_cfg()).unwrap().next().unwrap();
            cache.join(p.cache.repo.as_ref().unwrap().default_cachedir())
        };

        // (1) 未インストール + update 単独 → スキップ。キャッシュは一切作らない。
        let plugin = Plugin::new(mk_cfg()).unwrap().next().unwrap();
        let result = plugin
            .load(
                false,
                true,
                &cache,
                None,
                adaptive_semaphore::AdaptiveSemaphore::new(),
                reqwest::Client::new(),
            )
            .await
            .unwrap();
        assert!(
            result.is_none(),
            "update alone must skip an uninstalled repo, got {result:?}"
        );
        assert!(
            !repo_root.join("source.git").exists(),
            "update alone must not create source.git"
        );
        assert!(
            !repo_root.join("worktrees").exists(),
            "update alone must not create any snapshot"
        );

        // (2) 念のため `-u -i`（install+update）なら未インストールでも install する。
        let plugin2 = Plugin::new(mk_cfg()).unwrap().next().unwrap();
        let (loaded, _) = plugin2
            .load(
                true,
                true,
                &cache,
                None,
                adaptive_semaphore::AdaptiveSemaphore::new(),
                reqwest::Client::new(),
            )
            .await
            .unwrap()
            .expect("install+update should install an uninstalled repo");
        assert!(
            loaded.snapshot_root().is_some(),
            "install+update should produce a snapshot"
        );
        assert!(repo_root.join("source.git").is_dir());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn build_plugin_reuses_snapshot_and_skips_rebuild() {
        // build 付き plugin を install し、再度 load すると snapshot を再利用して build を
        // 再実行しないことを検証する（build 再利用の最適化）。
        // build は監査用 log に行を追記するので、再利用なら log 行数は 1 のままになる。
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("rsplug-build-reuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let remote = dir.join("remote");
        let cache = dir.join("cache");
        let log = dir.join("build.log");
        std::fs::create_dir_all(remote.join("plugin")).unwrap();
        std::fs::write(remote.join("plugin/init.vim"), "\"x\n").unwrap();
        let git = |args: &[&str]| {
            let s = Command::new("git")
                .current_dir(&remote)
                .args(args)
                .status()
                .unwrap();
            assert!(s.success(), "git {:?} failed", args);
        };
        git(&["init", "-q"]);
        git(&["add", "-A"]);
        let commit = Command::new("git")
            .current_dir(&remote)
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "init",
            ])
            .status()
            .unwrap();
        assert!(commit.success());
        let url = format!("file://{}", remote.display());
        let log_url = log.display();

        let mk_cfg = || {
            toml::from_str::<Config>(&format!(
                r#"
            [[plugins]]
            repo = "{url}"
            build = ["sh", "-c", "echo ran >> {log_url}"]
            "#
            ))
            .unwrap()
        };

        // 1 回目: install → build 実行 → log に 1 行
        let plugin = Plugin::new(mk_cfg()).unwrap().next().unwrap();
        let _ = plugin
            .load(
                true,
                false,
                &cache,
                None,
                adaptive_semaphore::AdaptiveSemaphore::new(),
                reqwest::Client::new(),
            )
            .await
            .unwrap()
            .unwrap();
        let lines_after_first = std::fs::read_to_string(&log).unwrap().lines().count();
        assert_eq!(
            lines_after_first, 1,
            "build should run once on first install"
        );

        // 2 回目: 同じ入力で再 load → snapshot 再利用 → build スキップ → log 行数は 1 のまま
        let plugin2 = Plugin::new(mk_cfg()).unwrap().next().unwrap();
        let _ = plugin2
            .load(
                true,
                false,
                &cache,
                None,
                adaptive_semaphore::AdaptiveSemaphore::new(),
                reqwest::Client::new(),
            )
            .await
            .unwrap()
            .unwrap();
        let lines_after_second = std::fs::read_to_string(&log).unwrap().lines().count();
        assert_eq!(
            lines_after_second, 1,
            "re-load must reuse the snapshot and skip build"
        );

        let _ = std::fs::remove_dir_all(&dir);
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

    #[tokio::test]
    async fn doc_file_entries_walks_doc_into_individual_keys() {
        // doc を sealed-dir ではなく個別ファイル（doc/<rel>）に展開する（doc 盗みの前提）。
        // ネスト・doc 外の除外・doc 無しの空 を検証する。
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        tokio::fs::create_dir_all(root.join("doc/sub"))
            .await
            .unwrap();
        tokio::fs::write(root.join("doc/foo.txt"), b"foo")
            .await
            .unwrap();
        tokio::fs::write(root.join("doc/sub/bar.txt"), b"bar")
            .await
            .unwrap();
        // doc 外は含まれない。
        tokio::fs::write(root.join("plugin.vim"), b"x")
            .await
            .unwrap();

        let filesource = Arc::new(FileSource::Directory {
            path: Arc::from(root.to_path_buf()),
        });
        let identity = RepoSnapshotIdentity::new(
            PathBuf::from("github.com/o/r"),
            b"deadbeef".to_vec(),
            None,
            Arc::<[String]>::from(Vec::<String>::new()),
            None,
        );

        let mut entries = doc_file_entries(root, &filesource, &identity).await;
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        let keys: Vec<&Path> = entries.iter().map(|(k, _)| k.as_path()).collect();
        assert_eq!(
            keys,
            vec![Path::new("doc/foo.txt"), Path::new("doc/sub/bar.txt")],
        );
        // 各 identity の relative_path がキーと一致（配置先の正確性）。
        for (k, item) in &entries {
            assert_eq!(item.identity.relative_path(), k.as_path());
        }

        // doc 無し snapshot は空。
        let nodoc = tempfile::tempdir().unwrap();
        assert!(
            doc_file_entries(nodoc.path(), &filesource, &identity)
                .await
                .is_empty()
        );
    }
}
