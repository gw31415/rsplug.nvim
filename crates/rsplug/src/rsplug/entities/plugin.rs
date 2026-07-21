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
    /// 内部 id（BFS 依存スケジューリング用）。`plugin_id`（`LoadedPlugin` の Hash）には
    /// 含まれない。`Plugin::new` で `PluginConfig.id` から移行する。`run_load_scheduler`
    /// が fan-out 完了追跡のキーとして使う。
    pub id: String,
    /// 依存先 id リスト（BFS 用）。`plugin_id` には含まれない。
    pub depends: Vec<String>,
}

/// プラグインの取得元
#[derive(DeserializeFromStr, Clone)]
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
    /// git url（fetch/tarball/error 表示用の実際 URL）。
    pub fn url(&self) -> String {
        match self {
            RepoSource::GitHub { owner, repo, .. } => util::github::url(owner, repo),
            RepoSource::Git { url, .. } => url.to_string(),
        }
    }

    /// rev（branch/tag/commit/wildcard）。未指定は None（default branch）。
    pub(crate) fn rev(&self) -> Option<Arc<str>> {
        match self {
            RepoSource::GitHub { rev, .. } => rev.clone(),
            RepoSource::Git { rev, .. } => rev.clone(),
        }
    }

    /// Canonical repository identity（PLANS「Model and repository identity」）。
    /// `host[:port]/path` 形式で、host は小文字化、デフォルトポート・userinfo・scheme・
    /// 末尾 `.git` は削除される。GitHub shorthand は `github.com/owner/repo`。
    /// lock key と cache path をこの同一 identity で統一し、表記揺れによる重複を防ぐ。
    pub(crate) fn canonical(&self) -> String {
        match self {
            RepoSource::GitHub { owner, repo, .. } => {
                format!("github.com/{}/{}", owner, repo)
            }
            RepoSource::Git { url, .. } => util::repo::canonicalize_url(url),
        }
    }

    /// Relative to the `repos` cache namespace.
    /// GitHub: `github.com/owner/repo`; URL: `host[:port]/path`。[`canonical`] の PathBuf 版。
    pub(crate) fn default_cachedir(&self) -> PathBuf {
        let mut path = PathBuf::new();
        for comp in self.canonical().split('/').filter(|s| !s.is_empty()) {
            path.push(comp);
        }
        path
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

/// DAG 解決の結果（PLANS.md "ResolvedGraph" モデル）。`Config`（PluginSpec 集合）
/// を `try_dag` にかけ、各ノードの `order`・`dependency_cachedirs`・依存元集約
/// `lazy_type` を確定した中間表現。この段階では fetch/ビルド（Materialization）
/// も runtime registration（LazyRegistration）も行わない。`Plugin::new` は
/// `Config → ResolvedGraph → Plugin` の2段階に分かれ、ResolvedGraph が
/// 「DAG 解決結果」を、Plugin が「マテリアライズ入力 + ライセンス登録材料」を
/// それぞれ明示的に担う。将来的に order/depends を load fan-out 後に組み立て直す
/// （per-TOML ストリーミング化）際の明示的な足場でもある。
struct ResolvedGraph {
    /// トポロジカル順 + (depth, original_index) tiebreak で並んだ解決済みノード。
    nodes: Vec<ResolvedNode>,
}

/// 1つの解決済みプラグインノード。DAG 解決由来のフィールド（`order`,
/// `dependency_cachedirs`, 集約 `lazy_type`）と、PluginConfig（PluginSpec）由来の
/// マテリアライズ材料を保持する。`From<ResolvedNode> for Plugin` で純粋に
/// フィールド移動される（計算は一切行わない）。
struct ResolvedNode {
    /// DAGトポロジカル順。`order = depth * (total + 1) + index`（旧 Plugin::new と同一式）。
    order: usize,
    /// `depends` で指定された依存先プラグインのキャッシュ相対パス。
    /// 依存先が script-only（リポジトリなし）なら除外済み（同一セマンティクス）。
    dependency_cachedirs: Vec<PathBuf>,
    /// 依存元の lazy_type を集約した結果（同一 fold）。
    lazy_type: LazyType,
    /// マテリアライズ材料（PluginSpec = PluginConfig 由来）。
    source_name: Option<String>,
    cache: CacheConfig,
    script: SetupScript,
    merge: MergeConfig,
    merge_enabled: bool,
    /// 内部 id（BFS 用）。Plugin への移行のみ。
    id: String,
    /// 依存先 id リスト（BFS 用）。
    depends: Vec<String>,
}

/// 段階2: `ResolvedNode` → `Plugin`。純粋なフィールド移動（計算なし）。
/// これにより「DAG 解決」と「マテリアライズ材料の詰め替え」が型レベルで分離される。
impl From<ResolvedNode> for Plugin {
    fn from(n: ResolvedNode) -> Self {
        Plugin {
            source_name: n.source_name,
            cache: n.cache,
            lazy_type: n.lazy_type,
            script: n.script,
            merge: n.merge,
            dependency_cachedirs: n.dependency_cachedirs,
            merge_enabled: n.merge_enabled,
            order: n.order,
            id: n.id,
            depends: n.depends,
        }
    }
}

impl Plugin {
    /// 設定ファイルから Plugin のコレクションを構築する。
    /// `Config → ResolvedGraph`（DAG 解決）→ `Plugin`（フィールド移動）の2段階。
    pub fn new(config: Config) -> Result<impl Iterator<Item = Plugin>, Error> {
        let resolved = Self::resolve(config)?;
        Ok(resolved.nodes.into_iter().map(Plugin::from))
    }

    /// Pre-resolve Plugin for EARLY-only execution（Step 4 到着順ストリーミング）。
    ///
    /// FACT 1 により `load_early` は `order`/`lazy_type`/`dependency_cachedirs`/
    /// `merge_enabled`/`depends`/`id` を一切読まないので、これらは dummy でよい。
    /// LATE（`load_late`）は `Plugin::resolve` で確定した resolved Plugin で実行され、
    /// 最終的な `order`/`lazy_type` が `plugin_id` に焼き込まれる。
    ///
    /// `id = compute_internal_id()` は resolve 後の Plugin の id と完全一致する
    /// （`try_dag` が重複 id を拒否するため、id は一意）。これが EARLY↔LATE 紐付けキー。
    /// EARLY はこの dummy Plugin、LATE は resolved Plugin だが、id で橋渡しする。
    #[allow(dead_code)] // Step 4 で run_load_scheduler が使用開始
    pub(crate) fn from_config(pc: PluginConfig) -> Plugin {
        // dep_name/compute_internal_id は &self 呼び出し。フィールド move の前に済ませる。
        let id = pc.compute_internal_id();
        let source_name = pc.dep_name().map(str::to_string);
        Plugin {
            source_name,
            cache: pc.cache,
            lazy_type: pc.lazy_type, // ユーザー指定値（aggregate 前）。load_early は無視。
            script: pc.script,
            merge: pc.merge,
            dependency_cachedirs: Vec::new(), // dummy。LATE は resolved Plugin の値を使う。
            merge_enabled: false,             // dummy。load_early は読まない。
            order: 0,                         // dummy。load_early は読まない。
            id,
            depends: pc.depends,
        }
    }

    /// 段階1: DAG 解決。`Config`（PluginSpec 集合）を `ResolvedGraph` に変換する。
    /// `order`/`dependency_cachedirs`/依存元集約 `lazy_type` をここで確定する。
    /// 重複チェック・UnknownDependency・閉路検出は dag クレートの `try_dag`
    /// （Kahn法 O(V+E)）に委譲する。計算式は旧 `Plugin::new` と完全同一。
    fn resolve(config: Config) -> Result<ResolvedGraph, Error> {
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

        let nodes = plugins
            .try_dag()?
            .into_map_iter(
                move |DagIteratorMapFuncArgs {
                          inner,
                          index,
                          depth,
                          dependents_iter,
                      }| {
                    let order = depth * (total + 1) + index;
                    let source_name = inner.dep_name().map(str::to_string);
                    let id = inner.id.clone().unwrap_or_default();
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
                        .iter()
                        .filter_map(|dep_id| {
                            id_to_index
                                .get(dep_id)
                                .and_then(|&dep_index| cachedirs[dep_index].clone())
                        })
                        .collect();
                    let merge_enabled = merge.merge;
                    ResolvedNode {
                        order,
                        dependency_cachedirs,
                        lazy_type,
                        source_name,
                        cache,
                        script,
                        merge,
                        merge_enabled,
                        id,
                        depends,
                    }
                },
            )
            .collect();

        Ok(ResolvedGraph { nodes })
    }

    /// キャッシュに既存 snapshot があるか（= インストール済み）。
    /// GraphQL preresolve の対象選別（main.rs）で未インストール + --update を除外するために使う。
    /// `repo` 無し（script-only）はインストール概念がないので false。
    pub(crate) async fn is_installed(&self, cache_dir: &Path) -> bool {
        let Some(repo) = self.cache.repo.as_ref() else {
            return false;
        };
        let r_root = repo_root(cache_dir, repo);
        let worktrees = worktrees_dir(&r_root);
        latest_snapshot_dir(&worktrees).await.is_some()
    }

    #[cfg(test)]
    /// キャッシュからPluginを読み込む。オプションでインストールやアップデートも行う。
    /// インストールされていない場合は `Ok(None)` を返す。
    ///
    /// 内部は EARLY 相（rev 解決 + fetch/materialize、依存情報不要）と LATE 相
    /// （BUILD + assemble、全DAG確定後）に分割。ストリーミング（run_load_scheduler）では
    /// EARLY/LATE を別タスクで管理するが、ここでは同一タスクで直列に呼び出すため振る舞いは
    /// 従来の `Plugin::load` と同一（pack/lock/generation byte-identical）。
    pub async fn load(
        self,
        install: bool,
        update: bool,
        cache_dir: impl AsRef<Path>,
        locked_rev: Option<Arc<str>>,
        semaphore: adaptive_semaphore::AdaptiveSemaphore,
        http_client: reqwest::Client,
    ) -> Result<Option<(LoadedPlugin, Option<(String, String)>)>, Error> {
        let cache_dir = cache_dir.as_ref().to_path_buf();
        let early = self
            .load_early(
                install,
                update,
                &cache_dir,
                locked_rev.as_deref(),
                &semaphore,
                &http_client,
            )
            .await?;
        self.load_late(early, &cache_dir, update).await
    }

    /// EARLY 相: rev 解決 → fetch/materialize まで。`dependency_cachedirs`・`order`・集約
    /// `lazy_type` を一切使わない（FACT 1）。ストリーミングでは per-TOML 到着時・rev 解決直後
    /// から実行可能。`&self` で repo/source 情報を読み、materialize 成果物を [`EarlyOutcome`]
    /// で返す（戻りは `self` に依存しない owned データのみ）。
    pub(crate) async fn load_early(
        &self,
        install: bool,
        update: bool,
        cache_dir: &Path,
        locked_rev: Option<&str>,
        semaphore: &adaptive_semaphore::AdaptiveSemaphore,
        http_client: &reqwest::Client,
    ) -> Result<EarlyOutcome, Error> {
        use super::util::git;
        use crate::rsplug::util::truncate;
        use unicode_width::UnicodeWidthStr;

        // script-only（repo 無し）は EARLY では何もしない。LATE で LoadedPlugin を構築。
        let Some(repo) = self.cache.repo.as_ref() else {
            return Ok(EarlyOutcome::ScriptOnly);
        };

        // `repo` は借りるので、論理 identity に使う相対 cachedir を先に捕捉する。
        let cachedir = repo.default_cachedir();
        let r_root = repo_root(cache_dir, repo);
        let source_git = source_git_dir(&r_root);
        let worktrees = worktrees_dir(&r_root);
        let url: Arc<str> = Arc::from(repo.url());
        // lock key 用の canonical identity（fetch/tarball/error 表示用の実 URL とは別）。
        let canonical = repo.canonical();

        // GitHub HTTPS URL かつ環境変数に token があれば認証フェッチする。
        let token = if repo.is_github_https() {
            util::github::token().map(Arc::<str>::from)
        } else {
            None
        };

        // バリアント固有のフィールドを抽出（ログ・エラー表示用）。repo は借りるので clone。
        let (rev, logid, repo_name): (Option<Arc<str>>, String, Arc<str>) = match repo {
            RepoSource::GitHub { owner, repo, rev } => {
                const MAX_LOGID_LEN: usize = 20;
                let repo_t = truncate(repo, MAX_LOGID_LEN);
                let len = MAX_LOGID_LEN.saturating_sub(repo_t.width_cjk() + 1);
                let logid = if len < 2 {
                    repo_t
                } else {
                    let mut o = truncate(owner, len);
                    o.push('/');
                    o.push_str(&repo_t);
                    o
                };
                (rev.clone(), logid, repo.clone())
            }
            RepoSource::Git { url, rev } => {
                const MAX_LOGID_LEN: usize = 20;
                (rev.clone(), truncate(url, MAX_LOGID_LEN), url.clone())
            }
        };

        // build/lua_build/dotgit は EARLY で materialize 戦略・has_build 判定に読む。
        // 所有権は LATE 相で self.cache を消費して取り出すので、ここでは参照。
        let build = &self.cache.build;
        let lua_build = self.cache.lua_build.as_deref();
        let dotgit = self.cache.dotgit;

        // --- ステージ1: target commit 解決（install/update/locked の分岐とリモート解決） ---
        let RevOutcome {
            oid,
            was_updated,
            was_installed,
        } = match resolve_target_commit(
            &worktrees,
            &url,
            &rev,
            &token,
            semaphore,
            http_client,
            install,
            update,
            locked_rev,
            &self.source_name,
            &logid,
        )
        .await?
        {
            Some(o) => o,
            None => return Ok(EarlyOutcome::Skipped),
        };
        let head_rev_str = oid.to_string();
        let guard = RunningGuard::new();

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
            source_name: &self.source_name,
            semaphore,
            http_client,
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
            return Ok(EarlyOutcome::Skipped);
        }

        // --- snapshot worktree の用意 (PLANS §7, §8 step 7-14) ---
        tokio::fs::create_dir_all(&worktrees).await?;
        let has_build = !build.is_empty() || lua_build.is_some();
        let pre_identity = RepoSnapshotIdentity::new(
            cachedir.clone(),
            head_rev_str.as_bytes().to_vec(),
            None,
            Arc::from(build.as_slice()),
            lua_build.map(Into::into),
        );
        let final_root: Arc<Path> = Arc::from(snapshot_root(&r_root, &pre_identity.snapshot_key()));

        // EARLY 相の materialize。BUILD（lua_post_update/run_repo_build）と rename は LATE 相へ
        // 回す。has_build の場合、一時 building worktree に materialize して止める。
        let (worktree_path, repository, is_plain, needs_build): (
            Arc<Path>,
            MaterializedRepo,
            bool,
            bool,
        ) = if final_root.exists() {
            // 同一 key の snapshot が既存 → 再利用（build/lua_post_update をスキップ）。
            // Phase 7 以降の tarball snapshot は `.git` 無し → Plain、旧 snapshot は Git。
            let repo = if final_root.join(".git").exists() {
                MaterializedRepo::Git(git::open(final_root.clone()).await?)
            } else {
                MaterializedRepo::Plain
            };
            (final_root.clone(), repo, false, false)
        } else if has_build {
            // build がある: 一時 worktree で materialize まで。build/dirty/rename は LATE。
            // dirty_diff を snapshot_key に含めるため、build 後でないと最終 key が確定しない。
            let building = building_worktree_dir(&worktrees);
            let _ = tokio::fs::remove_dir_all(&building).await;
            let building: Arc<Path> = Arc::from(building);
            let repo = match materialize(&ctx, building.as_ref(), use_tarball).await? {
                Some(r) => r,
                None => return Ok(EarlyOutcome::Skipped),
            };
            // tarball（Plain）かを記憶: LATE の rename 後 Git は開き直すが Plain は git::open 不要。
            let is_plain = matches!(repo, MaterializedRepo::Plain);
            (building, repo, is_plain, true)
        } else {
            // build 無し: key は確定（dirty=None）。final に materialize。
            let repo = match materialize(&ctx, final_root.as_ref(), use_tarball).await? {
                Some(r) => r,
                None => return Ok(EarlyOutcome::Skipped),
            };
            (final_root.clone(), repo, false, false)
        };

        Ok(EarlyOutcome::Materialized {
            guard,
            outcome: MaterializeOutcome {
                worktree_path,
                repository,
                has_build,
                needs_build,
                is_plain,
                final_root,
                cachedir,
                head_rev_str,
                was_updated,
                was_installed,
                logid,
                repo_name,
                canonical,
            },
        })
    }

    /// LATE 相: BUILD（`dependency_cachedirs` 使用）→ identity → manifest → assemble。
    /// 全DAG確定後（`order`/集約 `lazy_type`/`dependency_cachedirs` が resolve で確定済み）
    /// に呼ばれることが前提。`self` を消費して Plugin 由来フィールドを取り出し、`order`/集約
    /// `lazy_type` を `LoadedPlugin` の plugin_id に焼き込む（FACT 2: 構築後の上書きは不可）。
    pub(crate) async fn load_late(
        self,
        early: EarlyOutcome,
        cache_dir: &Path,
        update: bool,
    ) -> Result<Option<(LoadedPlugin, Option<(String, String)>)>, Error> {
        use super::util::git;
        use crate::{
            log::{Message, msg},
            rsplug::util::{execute, git::RSPLUG_BUILD_SUCCESS_FILE},
        };

        match early {
            EarlyOutcome::ScriptOnly => {
                // script-only（repo 無し）: order/lazy_type は resolve 確定済みなので plugin_id 安定。
                let Plugin {
                    source_name,
                    lazy_type,
                    script,
                    merge_enabled,
                    order,
                    ..
                } = self;
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
                Ok(Some((loaded, None)))
            }
            EarlyOutcome::Skipped => Ok(None),
            EarlyOutcome::Materialized { guard, outcome } => {
                // RunningGuard は LATE 相の終了まで保持（従来 Plugin::load スコープと同一ライフサイクル）。
                let _guard = guard;
                let MaterializeOutcome {
                    worktree_path,
                    mut repository,
                    has_build,
                    needs_build,
                    is_plain,
                    final_root,
                    cachedir,
                    head_rev_str,
                    was_updated,
                    was_installed,
                    logid,
                    repo_name,
                    canonical,
                } = outcome;

                let Plugin {
                    source_name,
                    cache,
                    lazy_type,
                    script,
                    merge,
                    dependency_cachedirs,
                    merge_enabled,
                    order,
                    ..
                } = self;
                let CacheConfig {
                    repo: _,
                    dotgit,
                    build,
                    lua_build,
                    lua_post_update,
                } = cache;

                // --- BUILD（has_build なら building worktree で実行 → rename → reopen） ---
                let snapshot_root_path: Arc<Path> = if needs_build {
                    // lua_post_update は update 検知時のみ building worktree で実行。
                    if update && let Some(lua_post_update) = lua_post_update.as_deref() {
                        let rtp =
                            build_runtimepaths(&worktree_path, cache_dir, &dependency_cachedirs)
                                .await;
                        let id = Arc::new(format!("{logid} (lua_post_update)"));
                        let result: Result<(), Error> = {
                            let id = id.clone();
                            async {
                                let path = create_lua_build_script(lua_post_update, &rtp).await?;
                                let _build = super::util::resources::build().await?;
                                let code = execute(
                                    lua_build_nvim_command(path.as_os_str()),
                                    worktree_path.clone(),
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

                    let rtp =
                        build_runtimepaths(&worktree_path, cache_dir, &dependency_cachedirs).await;
                    run_repo_build(
                        &build,
                        lua_build.as_deref(),
                        worktree_path.clone(),
                        rtp,
                        &logid,
                        &repo_name,
                    )
                    .await?;

                    // build 後 dirty を反映した最終 identity → key へ原子リネーム。
                    // 失敗しても final_root は作られない。
                    drop(repository);
                    tokio::fs::rename(worktree_path.as_ref(), final_root.as_ref()).await?;
                    // Plain は `.git` 無しで開き直せない。Git のみ git::open する。
                    repository = if is_plain {
                        MaterializedRepo::Plain
                    } else {
                        MaterializedRepo::Git(git::open(final_root.clone()).await?)
                    };
                    final_root.clone()
                } else {
                    worktree_path.clone()
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
                    let _ =
                        tokio::fs::write(repo_root.join("latest-snapshot"), key.as_bytes()).await;
                }

                // --- ステージ6: LoadedPlugin 構築（plugin_id 決定の核心） ---
                // filesource/entries/lazy_type 合成/FileItem 構築/通知は assemble_loaded_plugin 内へ
                // 抽出した。計算式・順序は旧インライン実装と完全同一（plugin_id 安定性）。
                let loaded = assemble_loaded_plugin(
                    &snapshot_root_path,
                    &identity,
                    dotgit,
                    &merge,
                    lazy_type,
                    source_name,
                    script,
                    order,
                    merge_enabled,
                    was_updated,
                    was_installed,
                    &logid,
                )
                .await?;
                let lock_info = Some((canonical, head_rev_str));

                Ok(Some((loaded, lock_info)))
            }
        }
    }
}

/// target commit 解決の結果（ステージ1）。`resolve_target_commit` の `Ok(None)` が
/// `Plugin::load` のスキップ（`Ok(None)`）を表す。
struct RevOutcome {
    oid: Oid,
    was_updated: bool,
    was_installed: bool,
}

/// EARLY 相（rev 解決 → fetch/materialize）の成果物。BUILD・identity 計算・assemble は
/// 未実行で LATE 相へ回す。**依存情報（`dependency_cachedirs`）・`order`・集約 `lazy_type`
/// は一切含まない**（FACT 1: fetch/materialize/identity-core は依存不要）。LATE 相がこれと
/// 確定済み `Plugin` フィールドで build/assemble を行う。
pub(crate) struct MaterializeOutcome {
    /// materialize 済み worktree。`has_build` なら一時 `building` worktree、
    /// それ以外（reuse/no-build）は `final_root` と同一。
    worktree_path: Arc<Path>,
    /// 実体化された snapshot バックエンド。BUILD 後に reopen される。
    repository: MaterializedRepo,
    /// build/lua_build があるか（build 成功 marker 書き込みの判定）。
    has_build: bool,
    /// LATE 相で実際に BUILD（lua_post_update/run_repo_build/rename）を実行するか。
    /// 新規 has-build materialize のみ true（reuse は既存 snapshot を使い build をスキップ）。
    needs_build: bool,
    /// tarball（Plain）由來か。`has_build` 時の rename 後 reopen 判定に使う。
    is_plain: bool,
    /// 最終配置先。`has_build` なら rename 先、それ以外は `worktree_path` と同一。
    final_root: Arc<Path>,
    /// repo のキャッシュ相対パス（identity 計算用）。
    cachedir: PathBuf,
    /// HEAD commit ハッシュ文字列（identity・lock_info 用）。
    head_rev_str: String,
    /// 更新検知（lua_post_update 実行・通知用）。
    was_updated: bool,
    was_installed: bool,
    /// ログ/エラー表示用。
    logid: String,
    repo_name: Arc<str>,
    /// lock key 用 canonical identity。
    canonical: String,
}

/// EARLY 相の結果。LATE 相への引き継ぎを3ケースで表現する。
pub(crate) enum EarlyOutcome {
    /// repo 無し（script-only）。LATE で `LoadedPlugin` を構築
    /// （`order`/`lazy_type` は resolve 確定済みなので plugin_id は安定）。
    ScriptOnly,
    /// `resolve_target_commit`/`materialize` で未インストール等によりスキップ（`Ok(None)`）。
    /// canon_to_remove 判定は main.rs の load_one が既存ロジック（repo_canon）で行うため、
    /// ここでは canonical を持たない。
    Skipped,
    /// materialize 完了。LATE で BUILD + assemble。`guard` は Loading 進捗バーの稼働区間
    /// （EARLY で確保し LATE 終了まで保持して、従来 `Plugin::load` の RunningGuard ライフサイクルを維持）。
    Materialized {
        guard: RunningGuard,
        outcome: MaterializeOutcome,
    },
}

/// ステージ1: target commit 解決。install/update/locked の分岐とリモート解決。
///
/// まずインストール状態（= 既存 snapshot worktree の有無）で分岐する。locked は lockfile
/// の rev をそのまま使い（cache 不足は後段でエラー）、update はインストール済みならリモートの
/// 最新を fetch して更新する（未インストールは対象外＝スキップ。`-u` 単独で新規 install はしない）。
/// install は未インストールならリモートから新規 fetch する。それ以外(通常起動) は既存 snapshot
/// の commit をそのまま使い、無ければスキップ。
///
/// `Ok(None)` = スキップ（未インストール等）。
// Plugin::load からの抽出。引数過多は LoadCtx 構造体化（巨大化）より局所的と判断し allow する。
#[allow(clippy::too_many_arguments)]
async fn resolve_target_commit(
    worktrees: &Path,
    url: &Arc<str>,
    rev: &Option<Arc<str>>,
    token: &Option<Arc<str>>,
    semaphore: &adaptive_semaphore::AdaptiveSemaphore,
    http_client: &reqwest::Client,
    install: bool,
    update: bool,
    locked_rev: Option<&str>,
    source_name: &Option<String>,
    logid: &str,
) -> Result<Option<RevOutcome>, Error> {
    use crate::log::{Message, msg};

    let invalid_data =
        |msg: String| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, msg));

    let (oid, was_updated, was_installed) = if let Some(locked_rev) = locked_rev {
        if !util::github::is_full_hex_hash(locked_rev) {
            return Err(invalid_data(format!(
                "Locked revision must be full hash for {}: got {}",
                url, locked_rev
            )));
        }
        let oid = Oid::from_str(locked_rev).map_err(Error::Git2)?;
        // locked_rev は --lock 由来（lock 固定）または preresolved（GraphQL、--install/--update）。
        // 未インストール時は通常経路と同じセマンティクス:
        //   - install     : 新規 fetch（was_installed=true で lua_post_update 実行）。
        //   - update 単独 : 新規 install しない（スキップ）。
        //   - --lock 単独 : キャッシュ前提なのでキャッシュ不足はエラー。
        match latest_snapshot_oid(worktrees).await? {
            // インストール済み。update=true なら preresolved: 既存 snapshot と比較して
            // 「実際に更新された」を立て、lua_post_update を実行させる（新規 fetch 時のみ）。
            Some(existing) => (oid, update && existing != oid, false),
            None if install => (oid, false, true),
            None if update => {
                // 未インストール + --update 単独: 新規 install はしない
                // （防御。通常は main.rs で GraphQL 対象外になり、この経路には来ない）。
                msg(Message::PluginNotInstalled(display_name(
                    source_name,
                    logid,
                )));
                return Ok(None);
            }
            None => {
                // 未インストール + --lock 単独: キャッシュ前提なのでエラー。
                return Err(invalid_data(format!(
                    "Missing cached repository for locked revision: {}",
                    url
                )));
            }
        }
    } else {
        match latest_snapshot_oid(worktrees).await? {
            Some(existing) if update => {
                let _permit = semaphore.acquire().await;
                msg(Message::Cache("Updating", url.clone()));
                let oid = resolve_remote_oid(http_client, url, rev, token).await?;
                msg(Message::Cache("Updating:done", url.clone()));
                // リモートの最新 rev が既存 snapshot と異なれば「実際に更新された」。
                (oid, existing != oid, false)
            }
            Some(existing) => (existing, false, false),
            None if install => {
                let _permit = semaphore.acquire().await;
                msg(Message::Cache("Updating", url.clone()));
                let oid = resolve_remote_oid(http_client, url, rev, token).await?;
                msg(Message::Cache("Updating:done", url.clone()));
                (oid, false, true)
            }
            None => {
                // 未インストール。install も update(既存更新) も対象外なのでスキップ。
                msg(Message::PluginNotInstalled(display_name(
                    source_name,
                    logid,
                )));
                return Ok(None);
            }
        }
    };
    Ok(Some(RevOutcome {
        oid,
        was_updated,
        was_installed,
    }))
}

/// ステージ6: snapshot から `LoadedPlugin` を構築する（**plugin_id 決定の核心**）。
///
/// `read_dir` → `entries.sort()`（決定論的順序）→ `extract_unique_lua_modules_from_snapshot`
/// による lazy_type 合成 → `FileItem` 構築 → `HowToPlaceFiles` → `LoadedPlugin` 生成。
/// 計算式・フィールド値・順序は旧 `Plugin::load` インライン実装と完全同一（plugin_id 安定性）。
/// 抽出により、この plugin_id 決定経路がダミー identity/snapshot_root で単体テスト可能になった。
// Plugin::load からの抽出。引数過多は LoadCtx 構造体化（巨大化）より局所的と判断し allow する。
#[allow(clippy::too_many_arguments)]
async fn assemble_loaded_plugin(
    snapshot_root_path: &Arc<Path>,
    identity: &RepoSnapshotIdentity,
    dotgit: bool,
    merge: &MergeConfig,
    mut lazy_type: LazyType,
    source_name: Option<String>,
    script: SetupScript,
    order: usize,
    merge_enabled: bool,
    was_updated: bool,
    was_installed: bool,
    logid: &str,
) -> Result<LoadedPlugin, Error> {
    use crate::log::{Message, msg};

    // ls-files 列挙を廃止し、snapshot ルート直下を read_dir で1階層列挙する。
    // ディレクトリ（lua/plugin 等）も1エントリにまとめ、install で copy_tree する
    //（ファイル数分の syscall を削減）。`doc` だけは盗み集約のため個別ファイルに展開する（下記）。
    // target/ 等の build 成果物は ignore 対象外なので pack に残る。.rsplug_build_success は
    // ignore.gitignore で除外される。`.git` は通常 ignore 対象だが、dotgit=true のときは
    // 例外扱いせず通常ディレクトリと同じくエントリに含める。
    let filesource = Arc::new(FileSource::Directory {
        path: snapshot_root_path.clone(),
    });
    let mut entries: Vec<PathBuf> = Vec::new();
    {
        let mut rd = tokio::fs::read_dir(snapshot_root_path.as_ref()).await?;
        while let Some(entry) = rd.next_entry().await? {
            entries.push(PathBuf::from(entry.file_name()));
        }
    }
    entries.sort(); // 決定論的順序（plugin_id 安定化）
    for luam in extract_unique_lua_modules_from_snapshot(snapshot_root_path.as_ref()).await {
        lazy_type &= LoadEvent::LuaModule(LuaModule(luam.into()));
    }
    // 各トップレベルエントリを配置対象に組み立てる。`doc` だけは例外:
    // 「盗んで」`_rsplug:doc` start プラグインへ集約するため、sealed-dir 1エントリではなく
    // 個別ファイル（`doc/<rel>`）に展開する（`PlugCtl::create` の `starts_with("doc/")` 盗みを効かせる）。
    // それ以外は sealed-dir のまま（install で copy_tree が clonefile/per-file copy で配置）。
    let mut file_entries: Vec<(PathBuf, FileItem)> = Vec::with_capacity(entries.len());
    for name in &entries {
        // Phase 2 の manifest は cache 内部ファイル。pack に含めず、全プラグインで
        // 同 path が衝突してマージを阻害する原因にもしないため、列挙から除外する。
        if name == Path::new(MANIFEST_FILE) {
            continue;
        }
        // dotgit=true なら `.git` を ignore から救出して通常エントリに含める。
        if !(dotgit && name == Path::new(".git") || !merge.ignore.matched(name)) {
            continue;
        }
        if name == Path::new("doc") {
            file_entries
                .extend(doc_file_entries(snapshot_root_path.as_ref(), &filesource, identity).await);
        } else {
            file_entries.push((
                name.clone(),
                FileItem::new(
                    filesource.clone(),
                    FileIdentity::RepoFile(RepoFileIdentity::new(identity.clone(), name.clone())),
                    MergeType::Conflict,
                ),
            ));
        }
    }
    let files: HowToPlaceFiles = HowToPlaceFiles::CopyEachFile(file_entries.into_iter().collect());

    // ロード成功が確定したので、実際に更新/新規インストールされたプラグインを
    // サマリーへ通知する。早帰り(Ok(None))経路には到達しない＝スキップしたものは報告しない。
    if was_updated {
        msg(Message::PluginUpdated(display_name(&source_name, logid)));
    } else if was_installed {
        msg(Message::PluginInstalled(display_name(&source_name, logid)));
    }

    Ok(LoadedPlugin {
        source_names: source_name.into_iter().collect(),
        files,
        lazy_type,
        script,
        order,
        merge_enabled,
        is_plugctl: false,
        dotgit,
    })
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
pub(crate) struct RunningGuard;

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
    fn canonical_normalizes_repo_identity() {
        // GitHub shorthand（owner/repo のケースは保持）
        assert_eq!(
            RepoSource::from_str("owner/repo").unwrap().canonical(),
            "github.com/owner/repo"
        );
        // HTTPS + 末尾 .git 削除
        assert_eq!(
            RepoSource::from_str("https://github.com/o/r.git")
                .unwrap()
                .canonical(),
            "github.com/o/r"
        );
        // SSH + userinfo + .git → 同一 identity
        assert_eq!(
            RepoSource::from_str("ssh://git@github.com/o/r.git")
                .unwrap()
                .canonical(),
            "github.com/o/r"
        );
        // host 小文字化
        assert_eq!(
            RepoSource::from_str("https://GitHub.COM/o/r")
                .unwrap()
                .canonical(),
            "github.com/o/r"
        );
        // デフォルトポート削除
        assert_eq!(
            RepoSource::from_str("https://gitlab.com:443/o/r")
                .unwrap()
                .canonical(),
            "gitlab.com/o/r"
        );
        // 非デフォルトポート保持
        assert_eq!(
            RepoSource::from_str("https://gitlab.com:2222/o/r")
                .unwrap()
                .canonical(),
            "gitlab.com:2222/o/r"
        );
    }

    #[test]
    fn default_cachedir_matches_canonical_components() {
        // canonical() の `/` 区切り == default_cachedir() のコンポーネント。
        for s in [
            "owner/repo",
            "https://github.com/o/r.git",
            "https://gitlab.com:2222/o/r",
            "ssh://git@GitHub.COM/o/r.git",
        ] {
            let repo = RepoSource::from_str(s).unwrap();
            let expected: PathBuf = repo
                .canonical()
                .split('/')
                .filter(|seg| !seg.is_empty())
                .collect();
            assert_eq!(repo.default_cachedir(), expected, "mismatch for {:?}", s);
        }
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
    async fn is_installed_reflects_snapshot_existence() {
        // is_installed: install 前 false、install 後 true。file:// Git リポジトリで検証。
        // これは main.rs の GraphQL preresolve 対象選別（未インストール + --update の除外）の判定根拠。
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("rsplug-is-installed-{}", std::process::id()));
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
        let c = Command::new("git")
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
        assert!(c.success());
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

        // (1) install 前: 未インストール。
        let p0 = Plugin::new(mk_cfg()).unwrap().next().unwrap();
        assert!(
            !p0.is_installed(&cache).await,
            "must report not-installed before load"
        );

        // (2) install 実行。
        let p1 = Plugin::new(mk_cfg()).unwrap().next().unwrap();
        let _ = p1
            .load(
                true,
                false,
                &cache,
                None,
                adaptive_semaphore::AdaptiveSemaphore::new(),
                reqwest::Client::new(),
            )
            .await
            .unwrap();

        // (3) install 後: インストール済み。Config は Clone できないので新プラグインで再判定。
        let p2 = Plugin::new(mk_cfg()).unwrap().next().unwrap();
        assert!(
            p2.is_installed(&cache).await,
            "must report installed after load"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn locked_rev_path_skips_installs_or_errors_for_uninstalled() {
        // locked_rev=Some（GraphQL preresolved / --lock 由来）+ 未インストールの分岐。
        //   - update 単独 → Ok(None) スキップ（本バグの核心: -u 単独は新規 install しない）
        //   - install    → 新規 install（preresolved rev で）
        //   - lock 単独(install=update=false) → キャッシュ不足エラー（Git 経路と整合）
        use std::{process::Command, sync::Arc};
        let dir = std::env::temp_dir().join(format!(
            "rsplug-lockedrev-uninstalled-{}",
            std::process::id()
        ));
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
        let c = Command::new("git")
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
        assert!(c.success());
        let oid = String::from_utf8(
            Command::new("git")
                .current_dir(&remote)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let oid: Arc<str> = Arc::from(oid.trim());
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

        // repo_root は load ごとに不変（キャッシュ未作成の確認用）。
        let repo_root = {
            let p = Plugin::new(mk_cfg()).unwrap().next().unwrap();
            cache.join(p.cache.repo.as_ref().unwrap().default_cachedir())
        };

        // (1) 未インストール + update 単独 (locked_rev=Some) → スキップ。何も作らない。
        let p = Plugin::new(mk_cfg()).unwrap().next().unwrap();
        let r = p
            .load(
                false,
                true,
                &cache,
                Some(oid.clone()),
                adaptive_semaphore::AdaptiveSemaphore::new(),
                reqwest::Client::new(),
            )
            .await
            .unwrap();
        assert!(
            r.is_none(),
            "update alone must skip an uninstalled repo even with preresolved rev"
        );
        assert!(
            !repo_root.join("source.git").exists(),
            "update alone must not create source.git"
        );
        assert!(
            !repo_root.join("worktrees").exists(),
            "update alone must not create any snapshot"
        );

        // (2) 未インストール + install (locked_rev=Some) → 新規 install。
        let p = Plugin::new(mk_cfg()).unwrap().next().unwrap();
        let (loaded, _) = p
            .load(
                true,
                false,
                &cache,
                Some(oid.clone()),
                adaptive_semaphore::AdaptiveSemaphore::new(),
                reqwest::Client::new(),
            )
            .await
            .unwrap()
            .expect("install with preresolved rev must install an uninstalled repo");
        assert!(
            loaded.snapshot_root().is_some(),
            "install must produce a snapshot"
        );

        // (3) のためキャッシュを削除して未インストールに戻す。
        let _ = std::fs::remove_dir_all(&repo_root);

        // (3) 未インストール + lock 単独 (install=false, update=false) → キャッシュ不足エラー。
        let p = Plugin::new(mk_cfg()).unwrap().next().unwrap();
        let err = p
            .load(
                false,
                false,
                &cache,
                Some(oid.clone()),
                adaptive_semaphore::AdaptiveSemaphore::new(),
                reqwest::Client::new(),
            )
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("Missing cached repository"),
            "lock alone must fail with cache-missing, got: {msg}"
        );

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

    /// Step 4: `Plugin::from_config`（EARLY 用 dummy Plugin）の id が、`Plugin::new` 経由の
    /// resolved Plugin の id と一致すること。EARLY（dummy）↔ LATE（resolved）を id で
    /// 橋渡しする根拠。`compute_internal_id` は単独 PluginConfig から計算可能で、resolve
    /// 前後で不変（`try_dag` が重複 id を拒否するため id は一意）。
    #[test]
    fn from_config_id_matches_resolved() {
        fn check(toml_src: &str) {
            let early: PluginConfig = toml::from_str(toml_src).unwrap();
            let resolved: PluginConfig = toml::from_str(toml_src).unwrap();
            let early_id = Plugin::from_config(early).id;
            let resolved_id = Plugin::new(Config {
                plugins: vec![resolved],
            })
            .unwrap()
            .next()
            .unwrap()
            .id;
            assert_eq!(early_id, resolved_id, "id mismatch for TOML:\n{toml_src}");
        }
        // GitHub 省略形（basename "plugin" が id）。
        check(r#"repo = "owner/plugin""#);
        // Git URL（basename "plugin.nvim" が id）。
        check(r#"repo = "https://gitlab.com/owner/plugin.nvim.git""#);
        // 無名 script-only（script 内容ハッシュが id）。
        check(r#"lua_start = "vim.g.x = true""#);
        // custom name が basename に勝つ。
        check(
            r#"
            repo = "owner/plugin"
            name = "my-plugin"
            "#,
        );
    }
}
