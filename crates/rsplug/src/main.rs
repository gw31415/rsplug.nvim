mod log;
mod osc94;
mod rsplug;

use clap::Parser;
use console::style;
use log::{Message, close, msg};
use once_cell::sync::Lazy;
use std::{
    collections::{BTreeMap, BinaryHeap},
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

    let locked_map = if locked {
        match rsplug::LockFile::read(lockfile.as_path()).await {
            Ok(rsplug::LockFile { locked, .. }) => {
                msg(Message::DetectLockFile(lockfile.clone()));
                locked
            }
            Err(e) => return Err(e.into()),
        }
    } else {
        BTreeMap::new()
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
    // 初期値8、min=1、max=256。エラー率上昇時に自動的に並列度を半減させる。
    let fetch_semaphore = adaptive_semaphore::AdaptiveSemaphore::with_limits(
        8,
        1,
        256,
        std::time::Duration::from_millis(64),
    );
    let (mut plugins, lock_infos) = {
        let res = plugins
            .into_iter()
            .map(|plugin| {
                let locked_map = Arc::clone(&locked_map);
                let fetch_semaphore = fetch_semaphore.clone();
                async move {
                    let locked_rev = if let Some(repo) = plugin.cache.repo.as_ref() {
                        let url = repo.url();
                        if locked {
                            if let Some(entry) = locked_map.get(&url) {
                                if entry.kind != rsplug::LockedResourceType::Git {
                                    return Err(Error::Io(std::io::Error::new(
                                        std::io::ErrorKind::InvalidData,
                                        format!(
                                            "Unsupported lock type for {}: {:?}",
                                            url, entry.kind
                                        ),
                                    )));
                                }
                                Some(Arc::<str>::from(entry.rev.as_str()))
                            } else {
                                return Err(Error::Io(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    format!("Missing locked revision for {}", url),
                                )));
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    let result = plugin
                        .load(
                            install,
                            update,
                            DEFAULT_REPOCACHE_DIR.as_path(),
                            locked_rev,
                            fetch_semaphore,
                        )
                        .await;
                    msg(Message::LoadPluginDone);
                    Ok(result?)
                }
            })
            .collect::<JoinSet<_>>()
            .join_all()
            .await;
        // Wait until all loading is complete.
        // NOTE: It does not abort if an error occurs (because of the build process).
        msg(Message::LoadDone);
        let (plugins, locks) = res
            .into_iter()
            .try_fold(
                (BinaryHeap::new(), Vec::new()),
                |(mut plugins, mut locks), res| {
                    if let Some((loaded, lock_info)) = res? {
                        plugins.push(loaded);
                        if let Some(lock_info) = lock_info {
                            locks.push(lock_info);
                        }
                    }
                    Ok::<_, Box<Error>>((plugins, locks))
                },
            )
            .map_err(|e| *e)?;
        (plugins, locks)
    };
    let total_count = plugins.len();

    if !locked {
        let mut merged_locked =
            Arc::try_unwrap(locked_map).expect("No other references to locked_map");
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
            version: "1".into(),
            locked: merged_locked,
        }
        .write(lockfile.as_path())
        .await?;
    }

    // Create PackPathState and insert packages into it
    let mut state = rsplug::PackPathState::new();
    rsplug::LoadedPlugin::merge(&mut plugins);
    for plugin in plugins {
        state.insert(plugin);
    }
    msg(Message::MergeFinished {
        total: total_count,
        merged: state.len(),
    });

    // Install the packages into the packpath.
    state
        .install(DEFAULT_APP_DIR.as_path())
        .await
        .map_err(rsplug::Error::Io)?;
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
    #[error(transparent)]
    Dag(#[from] dag::DagError),
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
