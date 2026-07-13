mod log;
mod osc94;
mod rsplug;

use clap::Parser;
use console::style;
use log::{Message, close, msg};
use once_cell::sync::Lazy;
use std::{
    collections::{BTreeMap, BinaryHeap, HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};
use tokio::task::JoinSet;

use rsplug::config_walker::ConfigWalker;

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
}

/// load_one への locked_rev 指定。
enum LoadRev {
    /// --locked なら locked_map から（エラー含め既存通り）、それ以外は None（load 内 resolve）。
    Auto,
    /// 40-hex seeded または GraphQL 解決済み OID（None は per-repo fallback）。
    Resolved(Option<Arc<str>>),
}

/// 1 plugin の load と後処理（LoadPluginDone・canon_to_remove 判定）。
/// 既存 fan-out クロージャ本体を抽出。plugin.rs load はゼロ変更。
async fn load_one(
    plugin: rsplug::Plugin,
    ctx: LoadCtx,
    rev: LoadRev,
) -> Result<
    (
        Option<(rsplug::LoadedPlugin, Option<(String, String)>)>,
        Option<String>,
    ),
    Error,
> {
    let (locked_rev, repo_canon): (Option<Arc<str>>, Option<String>) = match rev {
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
    };
    let result = plugin
        .load(
            ctx.install,
            ctx.update,
            ctx.cache_dir.as_path(),
            locked_rev,
            ctx.fetch_semaphore.clone(),
            ctx.http_client.clone(),
        )
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

    // Parse config files in deterministic path order.
    // `order` uses config_index, so walker receive order must not affect config order.
    let config = {
        let mut config_paths = Vec::new();
        let mut walker = ConfigWalker::new(config_files).await?;
        while let Some(item) = walker.recv().await {
            match item {
                Ok(path) => {
                    msg(Message::ConfigFound(path.clone()));
                    config_paths.push(path);
                }
                Err(e) => return Err(Error::Io(e)),
            }
        }
        config_paths.sort();
        log::msg(Message::ConfigWalkFinish);

        let mut configs = Vec::with_capacity(config_paths.len());
        for path in config_paths {
            let input =
                tokio::fs::read_to_string(&path)
                    .await
                    .map_err(|source| Error::ConfigRead {
                        path: path.clone(),
                        source,
                    })?;
            configs.push(toml::from_str::<rsplug::Config>(&input).map_err(|source| {
                Error::Parse {
                    source,
                    path,
                    input,
                }
            })?);
        }
        configs.into_iter().sum::<rsplug::Config>()
    };

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

    let plugins = rsplug::Plugin::new(config)?;
    let plugins: Vec<_> = plugins.collect();
    msg(Message::LoadBegin {
        total: plugins.len(),
    });

    // Load plugins through Cache based on the Units.
    // 全 plugin を並列に load/build する（DAG は runtime 読み込み順であり build 依存順ではない）。
    // 依存先 snapshot は各依存先 repo の worktrees/ から best-effort で解決する (PLANS §10.3)。
    let locked_map = Arc::new(locked_map);
    // 全 plugin のネットワークフェッチ並列度を制限する。
    // 初期値は CPU 数に応じて抑え、最大 64 に制限する。tarball は CDN 経由でも
    // download 後に展開・materialize が続くため、数百本の同時接続は総スループットを
    // 下げ、FD/RSS を不必要に消費する。
    // エラー率上昇時に自動的に並列度を半減させる。
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
        // GitHub API は User-Agent 無しを 403 で拒否する（administrative rules）。
        // reqwest はデフォルトで User-Agent を送らないため明示的に設定する。
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
    // reqwest::Client は内部が Arc なので clone は安価。FnMut closure 内に move するため先に clone。
    let http_client = http_client.clone();
    // --update/--install 時、GitHub リポジトリ（Git バックエンドの GitHub HTTPS URL 含む）の最新 rev を
    // GraphQL で解決する。ただし **GraphQL chunk の完了ごとに該当 plugin の load を段階的に spawn** し、
    // GraphQL のレイテンシを fetch と重ねる（パイプライン）。非 GitHub・wildcard・40-hex・token なし・
    // 衝突・--locked・flagless は即 load（GraphQL と並行）。plugin.rs load はゼロ変更（locked_rev 経由）。

    // 衝突検出（同一 canonical・異 rev は batch 対外＝個別 fallback）。
    let mut seen_rev: HashMap<String, Option<Arc<str>>> = HashMap::new();
    let mut conflicts: HashSet<String> = HashSet::new();
    for p in plugins.iter() {
        if let Some(repo) = p.cache.repo.as_ref() {
            let canonical = repo.canonical();
            let rev = repo.rev();
            match seen_rev.get(&canonical) {
                Some(existing) if *existing != rev => {
                    conflicts.insert(canonical);
                }
                None => {
                    seen_rev.insert(canonical, rev);
                }
                _ => {}
            }
        }
    }

    let token = rsplug::util::github::token();
    let do_graphql = !locked && (install || update) && token.is_some();

    // partition（by-value）。各 plugin は immediate（即 load）か graphql_pending（chunk 完了後に load）へ。
    let mut immediate: Vec<(rsplug::Plugin, LoadRev)> = Vec::new();
    let mut graphql_pending: Vec<Option<(rsplug::Plugin, String)>> = Vec::new();
    let mut canonical_to_pending: HashMap<String, Vec<usize>> = HashMap::new();
    let mut graphql_batch: Vec<rsplug::util::github::GithubRev> = Vec::new();
    for p in plugins {
        let Some(repo) = p.cache.repo.as_ref() else {
            // script-only: 即 load（Auto、rev は不要）。
            immediate.push((p, LoadRev::Auto));
            continue;
        };
        let canonical = repo.canonical();
        if locked || !do_graphql || conflicts.contains(&canonical) {
            // --locked / flagless / 衝突: 即 load（Auto、--locked なら locked_map lookup）。
            immediate.push((p, LoadRev::Auto));
            continue;
        }
        let rev = repo.rev();
        if rev
            .as_deref()
            .is_some_and(rsplug::util::github::is_full_hex_hash)
        {
            // 40-hex commit: 即 load、OID seed 済み。
            immediate.push((p, LoadRev::Resolved(rev)));
        } else if repo.is_github_https()
            && !rev.as_deref().is_some_and(|r| r.contains('*'))
            && let Some((owner, rname)) = rsplug::util::github::parse_github_url(&repo.url())
        {
            // GitHub GraphQL 対象: chunk 完了後に load。
            graphql_batch.push(rsplug::util::github::GithubRev {
                owner,
                repo: rname,
                rev: rev.as_deref().map(ToString::to_string),
            });
            let idx = graphql_pending.len();
            graphql_pending.push(Some((p, canonical.clone())));
            canonical_to_pending.entry(canonical).or_default().push(idx);
        } else {
            // 非 GitHub・wildcard: 即 load（Resolved(None)、load 内 resolve_remote_oid）。
            immediate.push((p, LoadRev::Resolved(None)));
        }
    }

    let ctx = LoadCtx {
        locked,
        install,
        update,
        locked_map: Arc::clone(&locked_map),
        fetch_semaphore: fetch_semaphore.clone(),
        http_client: http_client.clone(),
        cache_dir: DEFAULT_REPOCACHE_DIR.clone(),
    };

    let mut load_tasks = JoinSet::new();

    // immediate plugin を即 spawn（GraphQL と並行）。
    for (plugin, rev) in immediate {
        let ctx = ctx.clone();
        load_tasks.spawn(async move { load_one(plugin, ctx, rev).await });
    }

    // GraphQL chunk を並列送信し、完了ごとに該当 plugin の load を spawn（段階公開）。
    let token_str = token.unwrap_or("");
    let graphql_total = graphql_batch.len();
    let mut graphql_resolved = 0usize;
    if graphql_total > 0 {
        msg(Message::GraphQLResolveProgress {
            resolved: 0,
            total: graphql_total,
        });
    }
    let mut chunk_tasks = JoinSet::new();
    for chunk in graphql_batch.chunks(25) {
        let chunk_canonicals: Vec<String> = chunk
            .iter()
            .map(|g| format!("github.com/{}/{}", g.owner, g.repo))
            .collect();
        let client = http_client.clone();
        let token_owned = token_str.to_string();
        let chunk_owned = chunk.to_vec();
        chunk_tasks.spawn(async move {
            let result =
                rsplug::util::github::resolve_graphql_chunk(client, token_owned, chunk_owned).await;
            (result, chunk_canonicals)
        });
    }
    while let Some(join_res) = chunk_tasks.join_next().await {
        let (result, chunk_canonicals) = match join_res {
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
            Err(rsplug::util::github::ApiError::RateLimited) => Some("rate-limited".to_string()),
            Err(rsplug::util::github::ApiError::Other(s)) => Some(s.clone()),
        };
        let oid_map = result.unwrap_or_default();
        let chunk_size = chunk_canonicals.len();
        for canon in chunk_canonicals {
            let oid = if err_reason.is_some() {
                None
            } else {
                // canonical = github.com/{owner}/{repo} → (owner, repo) で oid_map を引く。
                let rest = canon.strip_prefix("github.com/").unwrap_or(&canon);
                let mut parts = rest.split('/');
                let owner = parts.next().unwrap_or("");
                let repo = parts.next().unwrap_or("");
                oid_map
                    .get(&(owner.to_string(), repo.to_string()))
                    .cloned()
                    .flatten()
                    .map(Arc::<str>::from)
            };
            if let Some(indices) = canonical_to_pending.remove(&canon) {
                for idx in indices {
                    if let Some((plugin, _)) = graphql_pending[idx].take() {
                        let ctx = ctx.clone();
                        let rev = LoadRev::Resolved(oid.clone());
                        load_tasks.spawn(async move { load_one(plugin, ctx, rev).await });
                    }
                }
            }
        }
        if let Some(reason) = err_reason {
            msg(Message::GraphQLBatchFailed { reason });
        }
        graphql_resolved += chunk_size;
        if graphql_total > 0 {
            msg(Message::GraphQLResolveProgress {
                resolved: graphql_resolved,
                total: graphql_total,
            });
        }
    }
    // 残存（chunk panic/JoinError で canonical_to_pending に残った分）を救済: None で load。
    for (_, indices) in canonical_to_pending.drain() {
        graphql_resolved += indices.len();
        for idx in indices {
            if let Some((plugin, _)) = graphql_pending[idx].take() {
                let ctx = ctx.clone();
                load_tasks
                    .spawn(async move { load_one(plugin, ctx, LoadRev::Resolved(None)).await });
            }
        }
    }
    if graphql_total > 0 {
        msg(Message::GraphQLResolveProgress {
            resolved: graphql_resolved,
            total: graphql_total,
        });
    }
    let res = load_tasks.join_all().await;
    // LoadCtx が locked_map の Arc クローンを保持しているため、ここで捨てないと
    // 後段の Arc::try_unwrap(locked_map) が失敗（panic）する。
    drop(ctx);
    // Wait until all loading is complete.
    // NOTE: It does not abort if an error occurs (because of the build process).
    msg(Message::LoadDone);
    let (plugins, lock_infos, remove_canons) = res
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
    let total_count = plugins.len();

    // Create PackPathState and load packages into it.
    // doc 盗みはマージ前に行う（doc が source 間マージの対象にならないよう）。
    let mut state = rsplug::PackPathState::new();
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
}
