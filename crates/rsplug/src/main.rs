mod log;
mod osc94;
mod rsplug;

use clap::Parser;
use console::style;
use log::{Message, close, msg};
use once_cell::sync::Lazy;
use rsplug::config_walker::ConfigWalker;
use std::{
    collections::{BTreeMap, BinaryHeap, HashMap},
    path::PathBuf,
    sync::Arc,
};

#[derive(clap::Parser, Debug)]
#[command(about)]
struct Args {
    /// Install plugins which are not installed yet
    #[arg(short, long)]
    install: bool,
    /// Access remote and update repositories
    #[arg(conflicts_with = "locked", short, long)]
    update: bool,
    /// Fix the repo version with rev in the lockfile
    #[arg(long)]
    locked: bool,
    /// Specify the lockfile path
    #[arg(long)]
    lockfile: Option<PathBuf>,
    /// Glob-patterns of the config files. Split by ':' to specify multiple patterns
    #[arg(
        required = true,
        env = "RSPLUG_CONFIG_FILES",
        value_delimiter = ':',
        hide_env_values = true
    )]
    config_files: Vec<String>,
}

/// per-plugin load のコンテキスト（Clone 可能）。各 load タスクに clone して渡す。
#[derive(Clone)]
struct LoadCtx {
    locked: bool,
    install: bool,
    update: bool,
    locked_map: Arc<BTreeMap<String, rsplug::LockedResource>>,
    fetch_semaphore: adaptive_semaphore::AdaptiveSemaphore,
    http_client: reqwest::Client,
    cache_dir: PathBuf,
    catalogs: Arc<rsplug::SnapshotCatalogCache>,
}

/// load_one への locked_rev 指定。
enum LoadRev {
    /// --locked なら locked_map から（エラー含め既存通り）、それ以外は None（load 内 resolve）。
    Auto,
    /// 40-hex seeded または GraphQL 解決済み OID（None は per-repo fallback）。
    Resolved(Option<Arc<str>>),
}

#[allow(clippy::result_large_err)]
/// (locked_rev, repo_canon) を LoadRev と Plugin から展開。旧 load_one 前半。
/// EARLY 相で rev を確定し、Skipped の canon_to_remove 用に repo_canon を返す。
fn expand_rev(
    plugin: &rsplug::Plugin,
    ctx: &LoadCtx,
    rev: LoadRev,
) -> Result<(Option<Arc<str>>, Option<String>), Error> {
    Ok(match rev {
        LoadRev::Auto => {
            if let Some(repo) = plugin.cache.repo.as_ref() {
                let canonical = repo.canonical();
                if ctx.locked
                    && let Some(entry) = ctx.locked_map.get(&canonical)
                {
                    if entry.kind != rsplug::LockedResourceType::Git {
                        return Err(Error::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("Unsupported lock type for {}: {:?}", canonical, entry.kind),
                        )));
                    }
                    (Some(Arc::<str>::from(entry.rev.as_str())), Some(canonical))
                } else if ctx.locked {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("Missing locked revision for {}", canonical),
                    )));
                } else {
                    (None, Some(canonical))
                }
            } else {
                (None, None)
            }
        }
        LoadRev::Resolved(oid) => {
            let repo_canon = plugin.cache.repo.as_ref().map(|r| r.canonical());
            (oid, repo_canon)
        }
    })
}

/// EARLY 相（rev 解決 → fetch/materialize）。`&plugin` で呼び plugin を消費しない。
/// 戻りは `(EarlyOutcome, repo_canon)`。repo_canon は Skipped の canon_to_remove 用。
async fn run_load_early(
    plugin: &rsplug::Plugin,
    ctx: &LoadCtx,
    rev: LoadRev,
) -> Result<(rsplug::EarlyOutcome, Option<String>), Error> {
    let (locked_rev, repo_canon) = expand_rev(plugin, ctx, rev)?;
    let early = plugin
        .load_early(
            ctx.install,
            ctx.update,
            &ctx.cache_dir,
            locked_rev.as_deref(),
            &ctx.fetch_semaphore,
            &ctx.http_client,
            &ctx.catalogs,
        )
        .await?;
    Ok((early, repo_canon))
}

/// LATE 相（BUILD + assemble）。plugin を消費。canon_to_remove 用に repo_canon を再計算。
async fn run_load_late(
    plugin: rsplug::Plugin,
    early: rsplug::EarlyOutcome,
    ctx: &LoadCtx,
) -> Result<
    (
        Option<(rsplug::LoadedPlugin, Option<(String, String)>)>,
        Option<String>,
    ),
    Error,
> {
    let repo_canon = plugin.cache.repo.as_ref().map(|r| r.canonical());
    let result = plugin
        .load_late(early, &ctx.cache_dir, ctx.update, &ctx.catalogs)
        .await;
    msg(Message::LoadPluginDone);
    let canon_to_remove =
        if repo_canon.is_some() && result.is_ok() && result.as_ref().unwrap().is_none() {
            repo_canon
        } else {
            None
        };
    Ok((result?, canon_to_remove))
}

/// EARLY 相の進行状態。EARLY 完了結果（`EarlyOutcome`）を保持する。
enum EarlySlot {
    InFlight,
    Done(rsplug::EarlyOutcome),
}

/// EARLY タスクの識別子。staging 段階（pre-ParsePhaseDone）は id（文字列）、
/// promotion 後（`nodes` 構築後）はノード index。EARLY 完了の routing に使う。
enum EarlyKey {
    Node(usize),
    Staged(String),
}

/// staging 段階（pre-ParsePhaseDone）の EARLY 進行状態。
enum StagedState {
    /// rev 待ち等、EARLY 未開始。
    Pending,
    /// EARLY タスク進行中。
    InFlight,
    /// EARLY 完了（成功/エラー）。promotion で `nodes` へ持ち込む。
    Done(Result<(rsplug::EarlyOutcome, Option<String>), Error>),
}

/// pre-ParsePhaseDone のプラグイン staging エントリ（id-keyed）。EARLY を到着順で
/// kick するため、`Plugin::resolve` 前の dummy Plugin を保持する。
struct StagedEarly {
    /// dummy Plugin（`Plugin::from_config`）。EARLY kick 時に take。
    plugin: Option<rsplug::Plugin>,
    state: StagedState,
    /// EARLY ゲート用の rev。`None` = GraphQL chunk 解決待ち。
    rev: Option<LoadRev>,
}

/// EARLY 相タスクの完了結果。EARLY 完了後も plugin を戻し、LATE 相へ渡す。
struct EarlyDone {
    key: EarlyKey,
    plugin: rsplug::Plugin,
    outcome: Result<(rsplug::EarlyOutcome, Option<String>), Error>,
}

struct CatalogDone {
    id: String,
    canonical: String,
    owner: String,
    repo: String,
    rev: Option<Arc<str>>,
    installed: bool,
}

async fn app() -> Result<(), Error> {
    let Args {
        install,
        update,
        lockfile,
        locked,
        config_files,
    } = Args::parse();
    let lockfile = lockfile.unwrap_or_else(|| DEFAULT_APP_DIR.join("rsplug.lock.json"));

    // Ensure the app cache dir exists up front. `Plugin::load` creates it as a
    // side effect of `--install`/`--update` (via `init_source`), but a flagless
    // run that skips every plugin (fresh cache, nothing to reuse) would never
    // touch the dir and then fail with ENOENT when writing the lockfile or the
    // packpath below.
    tokio::fs::create_dir_all(DEFAULT_APP_DIR.as_path()).await?;

    // パース生産者: walker → sort → 並列パース（spawn_blocking）→ SchedEvent 送信。
    // 完了順に関わらず index を添えて送り、最後に ParsePhaseDone{total} で確定通知する。
    // スケジューラ（run_load_scheduler）がこれを消費して load fan-out を統括する。
    let (parse_tx, parse_rx) = tokio::sync::mpsc::unbounded_channel::<SchedEvent>();
    let parse_prod = tokio::spawn({
        let parse_tx = parse_tx.clone();
        async move {
            let mut config_paths = Vec::new();
            let mut walker = match ConfigWalker::new(config_files).await {
                Ok(w) => w,
                Err(e) => {
                    let _ = parse_tx.send(SchedEvent::ParseError(Error::Io(e)));
                    return;
                }
            };
            while let Some(item) = walker.recv().await {
                match item {
                    Ok(path) => {
                        msg(Message::ConfigFound(path.clone()));
                        config_paths.push(path);
                    }
                    Err(e) => {
                        let _ = parse_tx.send(SchedEvent::ParseError(Error::Io(e)));
                        return;
                    }
                }
            }
            config_paths.sort();
            msg(Message::ConfigWalkFinish);
            let total = config_paths.len();
            let mut parse_tasks = tokio::task::JoinSet::new();
            for (index, path) in config_paths.into_iter().enumerate() {
                let parse_tx = parse_tx.clone();
                parse_tasks.spawn(async move {
                    let input = tokio::fs::read_to_string(&path).await.map_err(|source| {
                        Error::ConfigRead {
                            path: path.clone(),
                            source,
                        }
                    })?;
                    // Error::Parse が大きいので Box に詰める（clippy::result_large_err 回避）。
                    let parsed = tokio::task::spawn_blocking(move || {
                        toml::from_str::<rsplug::Config>(&input).map_err(|source| {
                            Box::new(Error::Parse {
                                source,
                                path,
                                input,
                            })
                        })
                    })
                    .await
                    .map_err(|e| {
                        Error::Io(std::io::Error::other(format!(
                            "config parse task failed: {e}"
                        )))
                    })?
                    .map_err(|boxed| *boxed)?;
                    let _ = parse_tx.send(SchedEvent::Parsed {
                        index,
                        config: parsed,
                    });
                    Ok::<_, Error>(())
                });
            }
            while let Some(res) = parse_tasks.join_next().await {
                match res {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        let _ = parse_tx.send(SchedEvent::ParseError(e));
                        return;
                    }
                    Err(e) => {
                        let _ = parse_tx.send(SchedEvent::ParseError(Error::Io(
                            std::io::Error::other(format!("config parse task panicked: {e}")),
                        )));
                        return;
                    }
                }
            }
            let _ = parse_tx.send(SchedEvent::ParsePhaseDone { total });
        }
    });

    // Always read the lock file as the baseline. `--locked` uses it to pin
    // revisions; non-`--locked` runs use it as the starting point for the
    // output lock file so that entries for plugins not in the config are
    // preserved.
    let locked_map = match rsplug::LockFile::read(lockfile.as_path()).await {
        Ok(lock) => {
            // 全キーを canonical identity に正規化（旧形式の生 URL キーを含む）。
            // 以降の get/insert/remove はすべて canonical キーで行う。
            let rsplug::LockFile { locked: map, .. } = lock.normalize_keys()?;
            if locked {
                msg(Message::DetectLockFile(lockfile.clone()));
            }
            map
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(e) => return Err(e.into()),
    };

    let locked_map = Arc::new(locked_map);

    // 全 plugin のネットワークフェッチ並列度を制限する（初期 CPU*2・最大 64・エラー時半減）。
    let cpu_count = rsplug::util::resources::available_cpus();
    let fetch_initial_limit = (cpu_count * 2).min(16);
    let fetch_semaphore = adaptive_semaphore::AdaptiveSemaphore::with_limits(
        fetch_initial_limit,
        1,
        64,
        std::time::Duration::from_millis(64),
    );

    // プロセス全体で共有する HTTP クライアント（接続プール・HTTP/2 再利用）。
    let http_client = reqwest::Client::builder()
        .user_agent(concat!("rsplug/", env!("CARGO_PKG_VERSION")))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .pool_max_idle_per_host(64)
        .tcp_keepalive(std::time::Duration::from_secs(60))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| {
            Error::Io(std::io::Error::other(format!(
                "failed to build HTTP client: {e}"
            )))
        })?;

    let ctx = LoadCtx {
        locked,
        install,
        update,
        locked_map: Arc::clone(&locked_map),
        fetch_semaphore: fetch_semaphore.clone(),
        http_client: http_client.clone(),
        cache_dir: DEFAULT_REPOCACHE_DIR.clone(),
        catalogs: Arc::new(rsplug::SnapshotCatalogCache::new()),
    };

    let token = rsplug::util::github::token();
    let do_graphql = !locked && (install || update) && token.is_some();

    // スケジューラがパースイベントを消費しつつ load fan-out を統括する。
    // ctx を消費して返るので、ここ以降 locked_map の Arc はスケジューラ内でのみ保持される。
    let (plugins, lock_infos, remove_canons) =
        run_load_scheduler(parse_rx, ctx, token.map(Arc::<str>::from), do_graphql).await?;
    // パース生産者タスクは ParsePhaseDone 送信後に終了しているはず。join して panic を拾う。
    let _ = parse_prod.await;
    let total_count = plugins.len();

    // Create PackPlan and load packages into it.
    // doc 盗みはマージ前に行う（doc が source 間マージの対象にならないよう）。
    let mut state = rsplug::PackPlan::new();
    state.load(plugins);
    msg(Message::MergeFinished {
        total: total_count,
        merged: state.len(),
    });

    // Install the packages into the packpath.
    state
        .install(DEFAULT_APP_DIR.as_path())
        .await
        .map_err(rsplug::Error::Io)?;

    // lock の更新は publication 成功の後に行う（pack と lock の一貫）。install が失敗した場合は
    // lock を更新せず、次回実行で再試行できるようにする（PLANS「tie lockfile-write timing to
    // successful publication」）。
    if !locked {
        let mut merged_locked =
            Arc::try_unwrap(locked_map).expect("No other references to locked_map");
        // Remove entries for plugins that are in the config but not installed
        // (cache missing). This keeps the lock file in sync with the cache.
        for canon in &remove_canons {
            merged_locked.remove(canon);
        }
        for (url, resolved_rev) in lock_infos {
            merged_locked.insert(
                url,
                rsplug::LockedResource {
                    kind: rsplug::LockedResourceType::Git,
                    rev: resolved_rev,
                },
            );
        }
        rsplug::LockFile {
            version: "2".into(),
            locked: merged_locked,
        }
        .write(lockfile.as_path())
        .await?;
    }
    Ok(())
}

/// 1 プラグインの LATE 相成功ペイロード（`run_load_late` の成功値）。
struct LoadPayload {
    loaded: Option<(rsplug::LoadedPlugin, Option<(String, String)>)>,
    canon_to_remove: Option<String>,
}

/// 集約用の LATE 相結果（`run_load_late` の戻り値そのもの）。`finished` に蓄積する。
type LoadOutcome = Result<
    (
        Option<(rsplug::LoadedPlugin, Option<(String, String)>)>,
        Option<String>,
    ),
    Error,
>;

/// 1 プラグインの LATE 相完了結果。BFS の依存完了追跡用にノード index を添える。
/// エラーでも index は分かるので、依存元の `pending_deps` を進められる。
struct LoadDone {
    idx: usize,
    outcome: Result<LoadPayload, Error>,
}

/// GraphQL chunk 解決タスクの結果（oid_map と chunk の canonical リスト）。
type GraphqlChunkResult = (
    Result<HashMap<(String, String), Option<String>>, rsplug::util::github::ApiError>,
    Vec<String>,
);

/// 2相 load スケジューリング用の各プラグインノード状態。
///
/// 各プラグインは EARLY 相（rev 解決 → fetch/materialize、依存不要）と LATE 相
/// （BUILD + assemble、全DAG確定後）に分かれ、それぞれ独立したゲートを通過する:
/// (1) EARLY ゲート: `rev` 確定 + `early` 未開始。rev 解決直後に fetch/materialize を開始
///     （FACT 1: 依存情報不要）。
/// (2) LATE ゲート: `finalized`(resolve 完了) + `early` Done + `pending_deps == 0`
///     （依存先の LATE 完了）。order/lazy_type が resolve で確定後にしか load_late は呼ばれない
///     （FACT 2: plugin_id 安定性）。build-runtimepath race も依存 LATE 完了待ちで排除。
struct NodeState {
    /// EARLY 前・EARLY 完了後に保持。EARLY/LATE タスクが take する。
    plugin: Option<rsplug::Plugin>,
    /// EARLY ゲート用の rev。`None` = GraphQL chunk 解決待ち。
    rev: Option<LoadRev>,
    /// EARLY 相の進行状態。`None` = 未開始、`InFlight` = 進行中、`Done` = 完了。
    early: Option<EarlySlot>,
    /// resolve() 完了（ParsePhaseDone で order/lazy_type/dependency_cachedirs 確定）。
    finalized: bool,
    /// 未完了の依存先 LATE 数。依存先 LATE 完了ごとに減る。
    pending_deps: usize,
    /// このプラグインの LATE 完了を待つ（依存している）ノード index。
    dependents: Vec<usize>,
}

/// ノードが EARLY fan-out 可能（rev 確定 + EARLY 未開始 + plugin 有り）なら load_early タスクを spawn。
fn try_schedule_early(
    nodes: &mut [NodeState],
    early_tasks: &mut tokio::task::JoinSet<EarlyDone>,
    ctx: &LoadCtx,
    idx: usize,
) {
    let Some(state) = nodes.get_mut(idx) else {
        return;
    };
    if state.rev.is_none() || state.early.is_some() || state.plugin.is_none() {
        return;
    }
    let plugin = state.plugin.take().expect("checked above");
    let rev = state.rev.take().expect("rev is Some (checked above)");
    state.early = Some(EarlySlot::InFlight);
    let ctx2 = ctx.clone();
    early_tasks.spawn(async move {
        let outcome = run_load_early(&plugin, &ctx2, rev).await;
        EarlyDone {
            key: EarlyKey::Node(idx),
            plugin,
            outcome,
        }
    });
}

/// staging エントリが EARLY fan-out 可能（rev 確定 + `Pending` + plugin 有り）なら
/// load_early タスクを spawn。EARLY 完了は `EarlyKey::Staged(id)` で識別し、
/// promotion 済みか否かで `nodes` か `staging` へ routing する。
fn try_schedule_staged_early(
    staging: &mut HashMap<String, StagedEarly>,
    early_tasks: &mut tokio::task::JoinSet<EarlyDone>,
    ctx: &LoadCtx,
    id: &str,
) {
    let Some(staged) = staging.get_mut(id) else {
        return;
    };
    if staged.rev.is_none()
        || !matches!(staged.state, StagedState::Pending)
        || staged.plugin.is_none()
    {
        return;
    }
    let plugin = staged.plugin.take().expect("checked above");
    let rev = staged.rev.take().expect("rev is Some (checked above)");
    staged.state = StagedState::InFlight;
    let id_owned = id.to_string();
    let ctx2 = ctx.clone();
    early_tasks.spawn(async move {
        let outcome = run_load_early(&plugin, &ctx2, rev).await;
        EarlyDone {
            key: EarlyKey::Staged(id_owned),
            plugin,
            outcome,
        }
    });
}

/// graphql_batch を chunk（25）に分けて chunk task を spawn する。
/// `all=false` は rolling flush（25 到達分のみ）、`all=true` は残り全て（ParsePhaseDone 用）。
fn flush_graphql_chunks(
    batch: &mut Vec<rsplug::util::github::GithubRev>,
    chunk_tasks: &mut tokio::task::JoinSet<GraphqlChunkResult>,
    http_client: &reqwest::Client,
    token_str: &str,
    all: bool,
) {
    while batch.len() >= 25 || (all && !batch.is_empty()) {
        let take = batch.len().min(25);
        let chunk: Vec<_> = batch.drain(..take).collect();
        let chunk_canonicals: Vec<String> = chunk
            .iter()
            .map(|g| format!("github.com/{}/{}", g.owner, g.repo))
            .collect();
        let client = http_client.clone();
        let token_owned = token_str.to_string();
        chunk_tasks.spawn(async move {
            rsplug::perf::incr(rsplug::perf::PerfOp::GraphqlRequest);
            let result =
                rsplug::util::github::resolve_graphql_chunk(client, token_owned, chunk).await;
            (result, chunk_canonicals)
        });
    }
}

/// ノードが LATE fan-out 可能（finalized + EARLY 完了 + 依存 LATE 完了）なら load_late タスクを spawn。
fn try_schedule_late(
    nodes: &mut [NodeState],
    load_tasks: &mut tokio::task::JoinSet<LoadDone>,
    ctx: &LoadCtx,
    idx: usize,
) {
    let Some(state) = nodes.get_mut(idx) else {
        return;
    };
    // FACT 2 の核心: finalized（resolve 確定）でなければ LATE しない。
    if !state.finalized || state.pending_deps != 0 || state.plugin.is_none() {
        return;
    }
    let Some(EarlySlot::Done(early)) = state.early.take() else {
        return;
    };
    let plugin = state.plugin.take().expect("checked above");
    let ctx2 = ctx.clone();
    load_tasks.spawn(async move {
        let outcome = run_load_late(plugin, early, &ctx2)
            .await
            .map(|(loaded, canon_to_remove)| LoadPayload {
                loaded,
                canon_to_remove,
            });
        LoadDone { idx, outcome }
    });
}

/// per-TOML パース結果をスケジューラへ流すイベント。
enum SchedEvent {
    /// TOML 1 ファイルのパース完了。`index` は `config_paths.sort()` 後の位置。
    Parsed {
        index: usize,
        config: rsplug::Config,
    },
    /// パースフェーズ全体の完了（全TOML揃い = `total`/order 確定）。
    ParsePhaseDone { total: usize },
    /// パース中のエラー（ConfigRead/Parse）。即時終了する。
    ParseError(Error),
}

/// load fan-out を統括するスケジューラ（2相 EARLY/LATE + BFS 依存スケジューリング）。
///
/// EARLY 相（fetch/materialize）は rev 解決直後に開始し、LATE 相（BUILD/assemble）は
/// resolve() 完了（`finalized`）+ 依存 LATE 完了後に実行する。これにより:
/// - FACT 1: fetch/materialize は依存情報不要なので EARLY で並列実行。
/// - FACT 2: order/lazy_type は resolve() で確定後にしか load_late に渡らない（plugin_id 安定性）。
/// - build-runtimepath race: LATE(build) は依存先の LATE 完了を待つ（`pending_deps`）。
///
/// 本段階では GraphQL 衝突解決・conflict 検出は ParsePhaseDone に留める（per-TOML ストリーミングは
/// 次段階）。最終出力（pack/lock）は LoadedPlugin の Ord と merge() の決定的ソートが
/// fan-out 順序に依存しないため byte-identical。
async fn run_load_scheduler(
    mut parse_rx: tokio::sync::mpsc::UnboundedReceiver<SchedEvent>,
    ctx: LoadCtx,
    token: Option<Arc<str>>,
    do_graphql: bool,
) -> Result<
    (
        BinaryHeap<rsplug::LoadedPlugin>,
        Vec<(String, String)>,
        Vec<String>,
    ),
    Error,
> {
    use tokio::task::JoinSet;

    let token_str = token.as_deref().unwrap_or("").to_string();
    let mut configs: HashMap<usize, rsplug::Config> = HashMap::new();
    let mut parse_done = false;
    let mut early_tasks: JoinSet<EarlyDone> = JoinSet::new();
    let mut load_tasks: JoinSet<LoadDone> = JoinSet::new();
    let mut chunk_tasks: JoinSet<GraphqlChunkResult> = JoinSet::new();
    let mut catalog_tasks: JoinSet<CatalogDone> = JoinSet::new();
    // Catalog resolution is local filesystem I/O. Bound it without blocking
    // the scheduler's parse and GraphQL event loop.
    let catalog_io = Arc::new(tokio::sync::Semaphore::new(8));
    let mut finished: Vec<LoadOutcome> = Vec::new();
    let mut nodes: Vec<NodeState> = Vec::new();
    // pre-ParsePhaseDone の staging（id-keyed）と conflict 検出表。両方 loop scope。
    let mut staging: HashMap<String, StagedEarly> = HashMap::new();
    let mut seen_rev: HashMap<String, Option<Arc<str>>> = HashMap::new();
    // canonical → EARLY キー群（GraphQL chunk 完了で routing）。pre/post promotion 両対応。
    let mut canonical_to_keys: HashMap<String, Vec<EarlyKey>> = HashMap::new();
    // rolling GraphQL batch（25 到達ごとに flush、残りは ParsePhaseDone で）。
    let mut graphql_batch: Vec<rsplug::util::github::GithubRev> = Vec::new();
    // promotion（ParsePhaseDone）で id → node index を記録。in-flight EARLY の再 routing 用。
    let mut staged_id_to_node_idx: HashMap<String, usize> = HashMap::new();
    let mut graphql_total = 0usize;
    let mut graphql_resolved = 0usize;
    let mut graphql_progress_sent = false;

    loop {
        tokio::select! {
            ev = parse_rx.recv(), if !parse_done => match ev {
                Some(SchedEvent::Parsed { index, config }) => {
                    // config.plugins を到着順で staging に積み、EARLY を即時 kick する。
                    // config 自体は merge 用に保持（Plugin::new(merged) のため）。
                    for pc in &config.plugins {
                        let id = pc.compute_internal_id();
                        // conflict 検出（同一 canonical・異 rev）→ 即エラー。
                        if let Some(repo) = pc.cache.repo.as_ref() {
                            let canonical = repo.canonical();
                            let rev = repo.rev();
                            match seen_rev.get(&canonical) {
                                Some(existing) if *existing != rev => {
                                    return Err(Error::ConflictingRevisions {
                                        canonical,
                                        rev_a: existing.clone(),
                                        rev_b: rev,
                                    });
                                }
                                None => {
                                    seen_rev.insert(canonical, rev);
                                }
                                _ => {}
                            }
                        }
                        let dummy = rsplug::Plugin::from_config(pc.clone());
                        // rev 決定（conflict は存在し得ない）。決定木は旧 ParsePhaseDone と同一。
                        let rev = match pc.cache.repo.as_ref() {
                            None => Some(LoadRev::Auto),
                            Some(repo) => {
                                let canonical = repo.canonical();
                                let repo_rev = repo.rev();
                                if ctx.locked || !do_graphql {
                                    Some(LoadRev::Auto)
                                } else if repo_rev
                                    .as_deref()
                                    .is_some_and(rsplug::util::github::is_full_hex_hash)
                                {
                                    Some(LoadRev::Resolved(repo_rev))
                                } else if repo.is_github_https()
                                    && !repo_rev.as_deref().is_some_and(|r| r.contains('*'))
                                    && let Some((owner, rname)) =
                                        rsplug::util::github::parse_github_url(&repo.url())
                                {
                                    if ctx.update {
                                        let io = Arc::clone(&catalog_io);
                                        let catalogs = Arc::clone(&ctx.catalogs);
                                        let repo_root = ctx.cache_dir.join(repo.default_cachedir());
                                        let id_for_catalog = id.clone();
                                        let canonical_for_catalog = canonical.clone();
                                        let owner = owner.to_string();
                                        let repo_name = rname.to_string();
                                        let rev_for_catalog = repo_rev.clone();
                                        catalog_tasks.spawn(async move {
                                            let _permit = io.acquire_owned().await.expect("catalog semaphore");
                                            let installed = catalogs
                                                .is_installed(repo_root, canonical_for_catalog.clone())
                                                .await;
                                            CatalogDone {
                                                id: id_for_catalog,
                                                canonical: canonical_for_catalog,
                                                owner,
                                                repo: repo_name,
                                                rev: rev_for_catalog,
                                                installed,
                                            }
                                        });
                                        None
                                    } else {
                                        graphql_batch.push(rsplug::util::github::GithubRev {
                                            owner,
                                            repo: rname,
                                            rev: repo_rev.as_deref().map(ToString::to_string),
                                        });
                                        graphql_total += 1;
                                        canonical_to_keys
                                            .entry(canonical)
                                            .or_default()
                                            .push(EarlyKey::Staged(id.clone()));
                                        flush_graphql_chunks(
                                            &mut graphql_batch,
                                            &mut chunk_tasks,
                                            &ctx.http_client,
                                            &token_str,
                                            false,
                                        );
                                        None
                                    }
                                } else {
                                    Some(LoadRev::Resolved(None))
                                }
                            }
                        };
                        let should_schedule = rev.is_some();
                        staging.insert(
                            id.clone(),
                            StagedEarly {
                                plugin: Some(dummy),
                                state: StagedState::Pending,
                                rev,
                            },
                        );
                        if should_schedule {
                            try_schedule_staged_early(&mut staging, &mut early_tasks, &ctx, &id);
                        }
                    }
                    configs.insert(index, config);
                }
                Some(SchedEvent::ParsePhaseDone { total: t }) => {
                    parse_done = true;
                    let merged: rsplug::Config = (0..t)
                        .map(|i| configs.remove(&i).expect("parsed config"))
                        .collect::<Vec<_>>()
                        .into_iter()
                        .sum();
                    let plugins: Vec<rsplug::Plugin> = rsplug::Plugin::new(merged)?.collect();
                    msg(Message::LoadBegin {
                        total: plugins.len(),
                    });

                    // BFS ノード表を構築。finalized=true（resolve 完了済み = order/lazy_type 確定）。
                    nodes = (0..plugins.len())
                        .map(|_| NodeState {
                            plugin: None,
                            rev: None,
                            early: None,
                            finalized: true,
                            pending_deps: 0,
                            dependents: Vec::new(),
                        })
                        .collect();
                    let mut id_to_index: HashMap<String, usize> = HashMap::new();
                    for (idx, p) in plugins.iter().enumerate() {
                        id_to_index.insert(p.id.clone(), idx);
                    }
                    for (idx, p) in plugins.iter().enumerate() {
                        let mut dep_indices: Vec<usize> = p
                            .depends
                            .iter()
                            .filter_map(|d| id_to_index.get(d).copied())
                            .filter(|&di| di != idx)
                            .collect();
                        dep_indices.sort_unstable();
                        dep_indices.dedup();
                        nodes[idx].pending_deps = dep_indices.len();
                        for di in &dep_indices {
                            nodes[*di].dependents.push(idx);
                        }
                    }

                    // promotion: staging（dummy Plugin + EARLY 状態）を resolved Plugin の
                    // ノードへ持ち込む。EARLY が Done なら nodes[idx].early へ、InFlight なら
                    // staged_id_to_node_idx に記録（EARLY-done で routing）、Pending（rev 待ち）
                    // は rev=None のまま（GraphQL chunk 完了で try_schedule_early）。
                    for (idx, p) in plugins.into_iter().enumerate() {
                        let id = p.id.clone();
                        nodes[idx].plugin = Some(p);
                        if let Some(mut staged) = staging.remove(&id) {
                            staged_id_to_node_idx.insert(id, idx);
                            nodes[idx].rev = staged.rev.take();
                            match staged.state {
                                StagedState::Done(Ok((early, repo_canon))) => match early {
                                    rsplug::EarlyOutcome::Skipped => {
                                        finished.push(Ok((None, repo_canon)));
                                    }
                                    early => {
                                        nodes[idx].early = Some(EarlySlot::Done(early));
                                    }
                                },
                                StagedState::Done(Err(e)) => {
                                    finished.push(Err(e));
                                }
                                StagedState::InFlight => {
                                    // EARLY-done で nodes[idx] へ routing（staged_id_to_node_idx 経由）。
                                }
                                StagedState::Pending => {
                                    // rev 待ち（GraphQL pending）。try_schedule_early で処理（rev 設定後）。
                                }
                            }
                            // staged.plugin（dummy）が残れば破棄（LATE は resolved Plugin を使う）。
                        }
                    }

                    // 残り GraphQL batch を flush（25 未満の端数）。
                    flush_graphql_chunks(
                        &mut graphql_batch,
                        &mut chunk_tasks,
                        &ctx.http_client,
                        &token_str,
                        true,
                    );
                    if graphql_total > 0 && !graphql_progress_sent {
                        msg(Message::GraphQLResolveProgress {
                            resolved: graphql_resolved,
                            total: graphql_total,
                        });
                        graphql_progress_sent = true;
                    }

                    // rev 確定済みノードを EARLY fan-out し、LATE gate を評価。
                    for idx in 0..nodes.len() {
                        try_schedule_early(&mut nodes, &mut early_tasks, &ctx, idx);
                        try_schedule_late(&mut nodes, &mut load_tasks, &ctx, idx);
                    }
                }
                Some(SchedEvent::ParseError(e)) => return Err(e),
                None => {
                    // parse 生産者が ParsePhaseDone を送る前にチャネルを閉じた（異常）。
                    if !parse_done {
                        return Err(Error::Io(std::io::Error::other(
                            "parse producer closed before ParsePhaseDone",
                        )));
                    }
                }
            },
            Some(jr) = early_tasks.join_next(), if !early_tasks.is_empty() => {
                let EarlyDone {
                    key,
                    plugin,
                    outcome,
                } = jr.map_err(|e| {
                    Error::Io(std::io::Error::other(format!("early load task panicked: {e}")))
                })?;
                match key {
                    EarlyKey::Node(idx) => {
                        // 既存ロジック。plugin（resolved）を nodes[idx] に戻す。
                        match outcome {
                            // EARLY エラー: finished へ。依存の pending_deps を進める。
                            Err(e) => {
                                finished.push(Err(e));
                                let dependents = std::mem::take(&mut nodes[idx].dependents);
                                for dep_idx in dependents {
                                    if nodes[dep_idx].pending_deps > 0 {
                                        nodes[dep_idx].pending_deps -= 1;
                                    }
                                    try_schedule_late(&mut nodes, &mut load_tasks, &ctx, dep_idx);
                                }
                            }
                            // Skipped（未インストール等）: finished へ Ok(None) + canon_to_remove。
                            Ok((rsplug::EarlyOutcome::Skipped, repo_canon)) => {
                                finished.push(Ok((None, repo_canon)));
                                let dependents = std::mem::take(&mut nodes[idx].dependents);
                                for dep_idx in dependents {
                                    if nodes[dep_idx].pending_deps > 0 {
                                        nodes[dep_idx].pending_deps -= 1;
                                    }
                                    try_schedule_late(&mut nodes, &mut load_tasks, &ctx, dep_idx);
                                }
                            }
                            // ScriptOnly / Materialized: LATE 相へ。plugin と early を nodes へ戻す。
                            Ok((early, _repo_canon)) => {
                                nodes[idx].plugin = Some(plugin);
                                nodes[idx].early = Some(EarlySlot::Done(early));
                                try_schedule_late(&mut nodes, &mut load_tasks, &ctx, idx);
                            }
                        }
                    }
                    EarlyKey::Staged(id) => {
                        // promotion 済み（in-flight だった）なら nodes[idx] へ routing。
                        // まだ promotion 前なら staging に Done を格納（promotion で nodes へ）。
                        if let Some(&idx) = staged_id_to_node_idx.get(&id) {
                            match outcome {
                                Err(e) => {
                                    finished.push(Err(e));
                                    let dependents = std::mem::take(&mut nodes[idx].dependents);
                                    for dep_idx in dependents {
                                        if nodes[dep_idx].pending_deps > 0 {
                                            nodes[dep_idx].pending_deps -= 1;
                                        }
                                        try_schedule_late(&mut nodes, &mut load_tasks, &ctx, dep_idx);
                                    }
                                }
                                Ok((rsplug::EarlyOutcome::Skipped, repo_canon)) => {
                                    finished.push(Ok((None, repo_canon)));
                                    let dependents = std::mem::take(&mut nodes[idx].dependents);
                                    for dep_idx in dependents {
                                        if nodes[dep_idx].pending_deps > 0 {
                                            nodes[dep_idx].pending_deps -= 1;
                                        }
                                        try_schedule_late(&mut nodes, &mut load_tasks, &ctx, dep_idx);
                                    }
                                }
                                Ok((early, _)) => {
                                    // nodes[idx].plugin は resolved（promotion 済み）。dummy は破棄。
                                    nodes[idx].early = Some(EarlySlot::Done(early));
                                    try_schedule_late(&mut nodes, &mut load_tasks, &ctx, idx);
                                }
                            }
                            drop(plugin);
                        } else if let Some(staged) = staging.get_mut(&id) {
                            staged.state = StagedState::Done(outcome);
                            drop(plugin);
                        } else {
                            drop(plugin);
                        }
                    }
                }
            }
            Some(jr) = load_tasks.join_next(), if !load_tasks.is_empty() => {
                let LoadDone { idx, outcome } = jr.map_err(|e| {
                    Error::Io(std::io::Error::other(format!("load task panicked: {e}")))
                })?;
                // LATE 完了（成功/エラー問わず）を格納し、依存元の pending_deps を進める。
                finished.push(outcome.map(|p| (p.loaded, p.canon_to_remove)));
                let dependents = std::mem::take(&mut nodes[idx].dependents);
                for dep_idx in dependents {
                    if nodes[dep_idx].pending_deps > 0 {
                        nodes[dep_idx].pending_deps -= 1;
                    }
                    try_schedule_late(&mut nodes, &mut load_tasks, &ctx, dep_idx);
                }
            }
            Some(jr) = chunk_tasks.join_next(), if !chunk_tasks.is_empty() => {
                let (result, chunk_canonicals) = match jr {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!(
                            "[rsplug] GraphQL chunk join error: {e}; affected plugins fall back to per-repo"
                        );
                        continue;
                    }
                };
                let err_reason = match &result {
                    Ok(_) => None,
                    Err(rsplug::util::github::ApiError::RateLimited) => {
                        Some("rate-limited".to_string())
                    }
                    Err(rsplug::util::github::ApiError::Other(s)) => Some(s.clone()),
                };
                let oid_map = result.unwrap_or_default();
                let chunk_size = chunk_canonicals.len();
                for canon in &chunk_canonicals {
                    let oid = if err_reason.is_some() {
                        None
                    } else {
                        let rest = canon.strip_prefix("github.com/").unwrap_or(canon);
                        let mut parts = rest.split('/');
                        let owner = parts.next().unwrap_or("");
                        let repo = parts.next().unwrap_or("");
                        oid_map
                            .get(&(owner.to_string(), repo.to_string()))
                            .cloned()
                            .flatten()
                            .map(Arc::<str>::from)
                    };
                    // 該当キー（Node/Staged）の rev を確定し、EARLY fan-out。
                    if let Some(keys) = canonical_to_keys.remove(canon) {
                        for key in keys {
                            match key {
                                EarlyKey::Node(idx) => {
                                    nodes[idx].rev = Some(LoadRev::Resolved(oid.clone()));
                                    try_schedule_early(&mut nodes, &mut early_tasks, &ctx, idx);
                                }
                                EarlyKey::Staged(id) => {
                                    if let Some(&idx) = staged_id_to_node_idx.get(&id) {
                                        // promotion 済み。nodes へ。
                                        nodes[idx].rev = Some(LoadRev::Resolved(oid.clone()));
                                        try_schedule_early(
                                            &mut nodes,
                                            &mut early_tasks,
                                            &ctx,
                                            idx,
                                        );
                                    } else if let Some(staged) = staging.get_mut(&id) {
                                        // promotion 前。staging へ。
                                        staged.rev = Some(LoadRev::Resolved(oid.clone()));
                                        try_schedule_staged_early(
                                            &mut staging,
                                            &mut early_tasks,
                                            &ctx,
                                            &id,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                if let Some(reason) = err_reason {
                    msg(Message::GraphQLBatchFailed { reason });
                }
                graphql_resolved += chunk_size;
                if graphql_total > 0 && graphql_progress_sent {
                    msg(Message::GraphQLResolveProgress {
                        resolved: graphql_resolved,
                        total: graphql_total,
                    });
                }
            }
            Some(jr) = catalog_tasks.join_next(), if !catalog_tasks.is_empty() => {
                let done = jr.map_err(|e| {
                    Error::Io(std::io::Error::other(format!("catalog task panicked: {e}")))
                })?;
                if done.installed {
                    graphql_batch.push(rsplug::util::github::GithubRev {
                        owner: done.owner,
                        repo: done.repo,
                        rev: done.rev.as_deref().map(ToString::to_string),
                    });
                    graphql_total += 1;
                    canonical_to_keys
                        .entry(done.canonical)
                        .or_default()
                        .push(EarlyKey::Staged(done.id));
                    flush_graphql_chunks(
                        &mut graphql_batch,
                        &mut chunk_tasks,
                        &ctx.http_client,
                        &token_str,
                        false,
                    );
                } else if let Some(staged) = staging.get_mut(&done.id) {
                    staged.rev = Some(LoadRev::Resolved(None));
                    try_schedule_staged_early(&mut staging, &mut early_tasks, &ctx, &done.id);
                } else if let Some(&idx) = staged_id_to_node_idx.get(&done.id) {
                    nodes[idx].rev = Some(LoadRev::Resolved(None));
                    try_schedule_early(&mut nodes, &mut early_tasks, &ctx, idx);
                }
            }
            else => {
                // parse 完了 + 全 chunk/EARLY/LATE 完了。
                // staging が残っていれば promotion 忘れ（論理バグ）。
                if !staging.is_empty() {
                    return Err(Error::Io(std::io::Error::other(
                        "scheduler shutdown with unpromoted staging entries",
                    )));
                }
                // rev 未確定（chunk panic 等）のノードを救済: per-repo fallback で EARLY。
                let mut rescued = false;
                for idx in 0..nodes.len() {
                    if nodes[idx].rev.is_none()
                        && nodes[idx].plugin.is_some()
                        && nodes[idx].early.is_none()
                    {
                        nodes[idx].rev = Some(LoadRev::Resolved(None));
                        try_schedule_early(&mut nodes, &mut early_tasks, &ctx, idx);
                        rescued = true;
                    }
                }
                if !rescued {
                    // 依存グラフは DAG（try_dag で循環検出済み）なので、全 chunk/EARLY/LATE
                    // 完了後に未処理が残ることは理論上ない。残っていればデッドロック。
                    for state in &nodes {
                        if state.plugin.is_some() || state.early.is_some() {
                            return Err(Error::Io(std::io::Error::other(
                                "scheduler deadlock: plugin never became ready",
                            )));
                        }
                    }
                    break;
                }
            }
        }
    }
    msg(Message::LoadDone);
    let (plugins, lock_infos, remove_canons) = finished
        .into_iter()
        .try_fold(
            (BinaryHeap::new(), Vec::new(), Vec::new()),
            |(mut plugins, mut locks, mut remove_canons), res| {
                let (result, canon_to_remove) = res?;
                if let Some((loaded, lock_info)) = result {
                    plugins.push(loaded);
                    if let Some(lock_info) = lock_info {
                        locks.push(lock_info);
                    }
                }
                if let Some(canon) = canon_to_remove {
                    remove_canons.push(canon);
                }
                Ok::<_, Box<Error>>((plugins, locks, remove_canons))
            },
        )
        .map_err(|e| *e)?;
    Ok((plugins, lock_infos, remove_canons))
}

static DEFAULT_APP_DIR: Lazy<PathBuf> = Lazy::new(|| {
    let homedir = std::env::home_dir().expect("Failed to get home directory");
    let cachedir = homedir.join(".cache");
    cachedir.join("rsplug")
});

static DEFAULT_REPOCACHE_DIR: Lazy<PathBuf> = Lazy::new(|| DEFAULT_APP_DIR.join("repos"));

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("{}", format_toml_parse_error(path, input, source))]
    Parse {
        source: toml::de::Error,
        path: PathBuf,
        input: String,
    },
    #[error("failed to read config {}: {source}", path.display())]
    ConfigRead {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Rsplug(#[from] rsplug::Error),
    /// 同一 canonical リポジトリに複数の異なる rev が指定された（設定ミス）。
    /// conflict は存在し得ないことが、到着順ストリーミングで rev を確定できる根拠（Step 4）。
    #[error("Conflicting revisions for {canonical}: {rev_a:?} vs {rev_b:?}")]
    ConflictingRevisions {
        canonical: String,
        rev_a: Option<Arc<str>>,
        rev_b: Option<Arc<str>>,
    },
}

fn format_toml_parse_error(
    path: &std::path::Path,
    input: &str,
    source: &toml::de::Error,
) -> String {
    let Some(orig_span) = source.span() else {
        return format!(
            "failed to parse config {}: {}",
            path.display(),
            source.message()
        );
    };
    // ponytail: `toml` reports the enclosing table-header span for any error
    // inside a table. Narrow to the actual offending field where possible.
    let span = refine_parse_error_span(input, orig_span.clone()).unwrap_or(orig_span);
    let (line_no, col_no, line_start, line_end) = line_info(input, span.start);
    let line = &input[line_start..line_end];
    let span_start_col = span.start.saturating_sub(line_start) + 1;
    let span_end_col = span
        .end
        .min(line_end)
        .saturating_sub(line_start)
        .max(span_start_col);
    let caret_len = span_end_col.saturating_sub(span_start_col).max(1);
    let gutter = style("|").blue();
    let line_no_styled = style(format!("{line_no:>2}")).blue();
    let caret_msg = format!(
        "{} {}",
        style("^".repeat(caret_len)).red().bold(),
        source.message()
    );
    format!(
        "failed to parse config\n {} {}:{}:{}\n   {}\n{} {} {}\n   {} {}{}",
        style("-->").blue(),
        path.display(),
        line_no,
        col_no,
        gutter,
        line_no_styled,
        gutter,
        highlight_toml_line(line, span_start_col, span_end_col),
        gutter,
        " ".repeat(span_start_col.saturating_sub(1)),
        caret_msg,
    )
}

/// The `toml` crate reports the enclosing table-header span (e.g. `[[plugins]]`)
/// for *any* error inside that table, so the caret points at the header instead
/// of the bad value. Re-parse with `toml_edit` (which keeps per-value spans) and
/// isolate which field actually fails.
///
/// `repo` is the only required field of a plugin entry. We test it alone first;
/// if it parses, we probe every other value-typed field against a known-valid
/// repo. Works for both serde type errors and custom `RepoSource` errors.
fn refine_parse_error_span(
    input: &str,
    header_span: std::ops::Range<usize>,
) -> Option<std::ops::Range<usize>> {
    let doc = toml_edit::Document::parse(input).ok()?;
    let plugins = doc.get("plugins")?.as_array_of_tables()?;
    let entry = plugins
        .iter()
        .find(|t| t.span() == Some(header_span.clone()))?;

    // (key, raw value text, value span) for each value-typed field.
    let fields: Vec<(&str, &str, std::ops::Range<usize>)> = entry
        .iter()
        .filter_map(|(key, item)| {
            let span = item.as_value()?.span()?;
            Some((key, &input[span.start..span.end], span))
        })
        .collect();

    let (_, repo_text, repo_span) = fields.iter().find(|(k, _, _)| *k == "repo")?;
    let base = format!("[[plugins]]\nrepo = {}\n", repo_text);

    if toml::from_str::<rsplug::Config>(&base).is_err() {
        return Some(repo_span.clone());
    }
    for (key, text, span) in &fields {
        if *key == "repo" {
            continue;
        }
        let probe = format!("{}{} = {}\n", base, key, text);
        if toml::from_str::<rsplug::Config>(&probe).is_err() {
            return Some(span.clone());
        }
    }
    None
}

fn line_info(input: &str, offset: usize) -> (usize, usize, usize, usize) {
    let mut line_no = 1;
    let mut line_start = 0;
    for (idx, ch) in input.char_indices() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line_no += 1;
            line_start = idx + 1;
        }
    }
    let line_end = input[line_start..]
        .find('\n')
        .map(|idx| line_start + idx)
        .unwrap_or(input.len());
    (
        line_no,
        offset.saturating_sub(line_start) + 1,
        line_start,
        line_end,
    )
}

fn highlight_toml_line(line: &str, span_start_col: usize, span_end_col: usize) -> String {
    let span_start = span_start_col.saturating_sub(1).min(line.len());
    let span_end = span_end_col.min(line.len());
    let mut rendered = String::new();
    rendered.push_str(&line[..span_start]);
    rendered.push_str(&style(&line[span_start..span_end]).red().bold().to_string());
    rendered.push_str(&line[span_end..]);
    rendered
}

#[tokio::main]
async fn main() {
    if let Err(e) = app().await {
        msg(Message::Error(e.into()));
        close(1).await;
    }
    close(0).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_error_span_points_at_offending_value_not_header() {
        let cases: &[(&str, &str, &str)] = &[
            // input, expected snippet line, exact offending token
            ("[[plugins]]\nrepo = 1 #\"x\"\n", "repo = 1", "1"),
            (
                "[[plugins]]\nrepo = \"badformat\"\n",
                "\"badformat\"",
                "\"badformat\"",
            ),
            (
                "[[plugins]]\nrepo = \"owner/x\"\non_ft = 7\n",
                "on_ft = 7",
                "7",
            ),
        ];
        for (input, want_line, want_token) in cases {
            let err = match toml::from_str::<rsplug::Config>(input) {
                Ok(_) => panic!("expected parse error for {input:?}"),
                Err(e) => e,
            };
            let refined = refine_parse_error_span(input, err.span().unwrap());
            let span = refined.expect("span should be refined");
            let bad = &input[span.start..span.end];
            assert_eq!(bad, *want_token, "for {input:?}");
            let rendered = format_toml_parse_error(std::path::Path::new("x"), input, &err);
            // Colorization is TTY-dependent (console emits ANSI under a terminal or
            // CLICOLOR_FORCE); assert against the plain-text structure only.
            let rendered = console::strip_ansi_codes(&rendered);
            assert!(
                rendered.contains(want_line),
                "missing line {want_line:?} in:\n{rendered}"
            );
            // Header line must NOT be the one carrying the code snippet anymore.
            assert!(
                !rendered.contains(" 1 | [[plugins]]"),
                "still pointing at header:\n{rendered}"
            );
        }
    }

    #[test]
    fn toml_parse_error_format_includes_source_snippet() {
        let input = "[[plugins]]\nrepo = \"owner/plugin\"\nstart = tru\n";
        let err = match toml::from_str::<rsplug::Config>(input) {
            Ok(_) => panic!("expected parse error"),
            Err(err) => err,
        };
        let rendered = format_toml_parse_error(std::path::Path::new("bad.toml"), input, &err);
        // Colorization is TTY-dependent (console emits ANSI under a terminal or
        // CLICOLOR_FORCE); assert against the plain-text structure only.
        let rendered = console::strip_ansi_codes(&rendered);

        assert!(rendered.contains("failed to parse config"));
        assert!(rendered.contains("bad.toml:3:9"));
        assert!(rendered.contains("start = tru"));
        assert!(rendered.contains("^"));
        assert!(rendered.contains("invalid boolean"));

        // ponytail: gutter `|` must align across all lines (lock indent fix).
        for line in rendered.lines() {
            if let Some(pos) = line.find('|') {
                assert_eq!(pos, 3, "gutter pipe misaligned: {line:?}");
            }
        }
    }

    /// Step 4: `run_load_scheduler` が Parsed 到着順で EARLY を kick し、ParsePhaseDone
    /// 後に LATE を実行して、正しい LoadedPlugin を生成することを検証する。
    /// script-only プラグイン（repo なし）でネットワーク・キャッシュ不要。
    #[tokio::test]
    async fn run_load_scheduler_streams_script_only_plugins() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = LoadCtx {
            locked: false,
            install: false,
            update: false,
            locked_map: Arc::new(BTreeMap::new()),
            fetch_semaphore: adaptive_semaphore::AdaptiveSemaphore::new(),
            http_client: reqwest::Client::new(),
            cache_dir: tmp.path().to_path_buf(),
            catalogs: Arc::new(rsplug::SnapshotCatalogCache::new()),
        };
        let (parse_tx, parse_rx) = tokio::sync::mpsc::unbounded_channel::<SchedEvent>();
        let c1: rsplug::Config =
            toml::from_str("[[plugins]]\nlua_start = \"vim.g.a = 1\"").unwrap();
        let c2: rsplug::Config =
            toml::from_str("[[plugins]]\nlua_start = \"vim.g.b = 2\"").unwrap();
        parse_tx
            .send(SchedEvent::Parsed {
                index: 0,
                config: c1,
            })
            .unwrap();
        parse_tx
            .send(SchedEvent::Parsed {
                index: 1,
                config: c2,
            })
            .unwrap();
        parse_tx
            .send(SchedEvent::ParsePhaseDone { total: 2 })
            .unwrap();
        drop(parse_tx);

        let (plugins, _locks, _remove) = run_load_scheduler(parse_rx, ctx, None, false)
            .await
            .unwrap();

        // script-only 2個 → 2つの LoadedPlugin（異なる plugin_id）。
        // EARLY（ScriptOnly）→ LATE（assemble）が Parsed 到着順 + ParsePhaseDone promotion
        // で正しく完了した証拠。
        assert_eq!(plugins.len(), 2, "expected 2 loaded plugins");
        let ids: Vec<_> = plugins.iter().map(|p| p.plugin_id()).collect();
        assert_ne!(ids[0], ids[1], "plugin_ids must differ");
    }

    /// Step 4 × Step 2: 依存（depends）のあるプラグインが、streaming と BFS の協調で
    /// デッドロックなく正しくロードされること。child は base に依存し、到着順は逆
    /// （child 先、base 後）。promotion 後の `pending_deps` ゲートが正しく働く証拠。
    #[tokio::test]
    async fn run_load_scheduler_resolves_dependency_with_streaming() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = LoadCtx {
            locked: false,
            install: false,
            update: false,
            locked_map: Arc::new(BTreeMap::new()),
            fetch_semaphore: adaptive_semaphore::AdaptiveSemaphore::new(),
            http_client: reqwest::Client::new(),
            cache_dir: tmp.path().to_path_buf(),
            catalogs: Arc::new(rsplug::SnapshotCatalogCache::new()),
        };
        let (parse_tx, parse_rx) = tokio::sync::mpsc::unbounded_channel::<SchedEvent>();
        // 到着順は依存逆順（child → base）。Plugin::resolve が topo 順に並べ直す。
        let child: rsplug::Config = toml::from_str(
            "[[plugins]]\nname = \"child\"\nlua_start = \"vim.g.child = true\"\ndepends = [\"base\"]",
        )
        .unwrap();
        let base: rsplug::Config =
            toml::from_str("[[plugins]]\nname = \"base\"\nlua_start = \"vim.g.base = true\"")
                .unwrap();
        parse_tx
            .send(SchedEvent::Parsed {
                index: 0,
                config: child,
            })
            .unwrap();
        parse_tx
            .send(SchedEvent::Parsed {
                index: 1,
                config: base,
            })
            .unwrap();
        parse_tx
            .send(SchedEvent::ParsePhaseDone { total: 2 })
            .unwrap();
        drop(parse_tx);

        let (plugins, _locks, _remove) = run_load_scheduler(parse_rx, ctx, None, false)
            .await
            .unwrap();

        // base と child の2つがデッドロックなくロードされる（依存 LATE 完了ゲート正常）。
        assert_eq!(plugins.len(), 2, "dependency did not resolve cleanly");
        let ids: Vec<_> = plugins.iter().map(|p| p.plugin_id()).collect();
        assert_ne!(ids[0], ids[1], "plugin_ids must differ");
    }
}
