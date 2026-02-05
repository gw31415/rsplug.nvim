use std::collections::HashSet;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use glob::glob;
use globwalker::GlobWalker;
use ignore::{WalkBuilder, WalkState};
use tokio::task::spawn_blocking;

use crate::bench_rules::{matches_compiled_rules, to_relative_unix_path};
use crate::bench_types::{AttemptOutcome, AttemptResult, BenchmarkKind};
use globwalker::pattern::CompiledRules;

pub(crate) async fn run_benchmark_attempt(
    kind: BenchmarkKind,
    cwd: &Path,
    raw_patterns: &[String],
    rules: &Arc<CompiledRules>,
    timeout: Duration,
) -> io::Result<AttemptOutcome> {
    match kind {
        BenchmarkKind::Globwalker => {
            let cwd = cwd.to_path_buf();
            let patterns = raw_patterns.to_vec();
            run_benchmark(timeout, move || measure_globwalker(cwd, patterns, timeout)).await
        }
        BenchmarkKind::IgnoreParallel => {
            let cwd = cwd.to_path_buf();
            let rules = Arc::clone(rules);
            run_benchmark(timeout, move || measure_ignore(cwd, rules, timeout)).await
        }
        BenchmarkKind::Glob => {
            let cwd = cwd.to_path_buf();
            let rules = Arc::clone(rules);
            run_benchmark(timeout, move || measure_glob(cwd, rules, timeout)).await
        }
    }
}

pub(crate) async fn run_benchmark<F, Fut>(
    attempt_timeout: Duration,
    runner: F,
) -> io::Result<AttemptOutcome>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = io::Result<AttemptOutcome>>,
{
    match tokio::time::timeout(attempt_timeout, runner()).await {
        Err(_) => Ok(AttemptOutcome::TimedOut),
        Ok(result) => result,
    }
}

async fn measure_globwalker(
    cwd: PathBuf,
    patterns: Vec<String>,
    timeout: Duration,
) -> io::Result<AttemptOutcome> {
    let mut matched_files = 0usize;
    let mut walker = GlobWalker::new(patterns, &cwd)?;
    let started = Instant::now();
    walker.set_deadline(started + timeout);

    loop {
        match walker.next().await {
            Ok(Some(_)) => matched_files += 1,
            Ok(None) => break,
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                return Ok(AttemptOutcome::TimedOut);
            }
            Err(error) => return Err(error),
        }
    }

    Ok(AttemptOutcome::Completed(AttemptResult {
        elapsed: started.elapsed(),
        matched_files,
    }))
}

async fn measure_ignore(
    cwd: PathBuf,
    rules: Arc<CompiledRules>,
    timeout: Duration,
) -> io::Result<AttemptOutcome> {
    spawn_blocking(move || {
        let started = Instant::now();
        let deadline = started + timeout;
        let start_roots = build_start_roots(cwd.as_path(), &rules.include_prefixes)?;
        let seen = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let timed_out = Arc::new(AtomicBool::new(false));
        let shared_error = Arc::new(std::sync::Mutex::new(None));

        for start_root in start_roots {
            let mut walker = WalkBuilder::new(start_root.as_path());
            walker
                .hidden(false)
                .ignore(false)
                .git_ignore(false)
                .git_exclude(false)
                .git_global(false);

            walker.build_parallel().run(|| {
                let seen = Arc::clone(&seen);
                let timed_out = Arc::clone(&timed_out);
                let shared_error = Arc::clone(&shared_error);
                let rules = Arc::clone(&rules);
                let cwd = cwd.clone();
                Box::new(move |entry| {
                    if Instant::now() >= deadline {
                        timed_out.store(true, Ordering::Relaxed);
                        return WalkState::Quit;
                    }
                    match entry {
                        Ok(entry)
                            if entry
                                .file_type()
                                .is_some_and(|file_type| file_type.is_file()) =>
                        {
                            let path = entry.path().to_path_buf();
                            let Some(relative) = path_to_relative_unix(cwd.as_path(), &path) else {
                                return WalkState::Continue;
                            };
                            if !matches_compiled_rules(relative.as_str(), &rules.ordered_rules) {
                                return WalkState::Continue;
                            }
                            let identity = match std::fs::canonicalize(path) {
                                Ok(canonical) => canonical,
                                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                                    return WalkState::Continue;
                                }
                                Err(error) => {
                                    if let Ok(mut stored_error) = shared_error.lock() {
                                        *stored_error = Some(error);
                                    }
                                    return WalkState::Quit;
                                }
                            };
                            if let Ok(mut seen_paths) = seen.lock() {
                                seen_paths.insert(identity);
                            } else {
                                if let Ok(mut stored_error) = shared_error.lock() {
                                    *stored_error =
                                        Some(io::Error::other("ignore benchmark lock poisoned"));
                                }
                                return WalkState::Quit;
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            if let Ok(mut stored_error) = shared_error.lock() {
                                *stored_error = Some(io::Error::other(error.to_string()));
                            }
                            return WalkState::Quit;
                        }
                    }
                    WalkState::Continue
                })
            });

            if let Ok(mut stored_error) = shared_error.lock()
                && let Some(error) = stored_error.take()
            {
                return Err(error);
            }
            if timed_out.load(Ordering::Relaxed) || Instant::now() >= deadline {
                return Ok(AttemptOutcome::TimedOut);
            }
        }

        Ok(AttemptOutcome::Completed(AttemptResult {
            elapsed: started.elapsed(),
            matched_files: seen
                .lock()
                .map_err(|_| io::Error::other("ignore benchmark lock poisoned"))?
                .len(),
        }))
    })
    .await
    .map_err(|error| io::Error::other(format!("ignore task join error: {error}")))?
}

async fn measure_glob(
    cwd: PathBuf,
    rules: Arc<CompiledRules>,
    timeout: Duration,
) -> io::Result<AttemptOutcome> {
    spawn_blocking(move || {
        let started = Instant::now();
        let deadline = started + timeout;
        let mut seen = std::collections::HashSet::new();
        let start_roots = build_start_roots(cwd.as_path(), &rules.include_prefixes)?;

        for start_root in start_roots {
            let patterns = [
                format!("{}/**/*", start_root.display()),
                format!("{}/*", start_root.display()),
            ];

            for pattern in patterns {
                let entries = glob(&pattern).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid glob pattern `{pattern}`: {error}"),
                    )
                })?;

                for path in entries.flatten() {
                    if Instant::now() >= deadline {
                        return Ok(AttemptOutcome::TimedOut);
                    }
                    if !path.is_file() {
                        continue;
                    }
                    let Some(relative) = path_to_relative_unix(cwd.as_path(), &path) else {
                        continue;
                    };
                    if !matches_compiled_rules(relative.as_str(), &rules.ordered_rules) {
                        continue;
                    }
                    let canonical = match std::fs::canonicalize(&path) {
                        Ok(path) => path,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(error),
                    };
                    seen.insert(canonical);
                }
            }
        }

        Ok(AttemptOutcome::Completed(AttemptResult {
            elapsed: started.elapsed(),
            matched_files: seen.len(),
        }))
    })
    .await
    .map_err(|error| io::Error::other(format!("glob task join error: {error}")))?
}

fn build_start_roots(cwd: &Path, include_prefixes: &[String]) -> io::Result<Vec<PathBuf>> {
    if include_prefixes.is_empty() || include_prefixes.iter().any(|prefix| prefix.is_empty()) {
        return Ok(vec![cwd.to_path_buf()]);
    }

    let mut start_roots = Vec::new();
    let mut seen_roots = HashSet::new();
    for prefix in include_prefixes {
        let candidate = cwd.join(prefix);
        let metadata = match std::fs::metadata(candidate.as_path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if metadata.is_dir() && seen_roots.insert(candidate.clone()) {
            start_roots.push(candidate);
        }
    }

    if start_roots.is_empty() {
        start_roots.push(cwd.to_path_buf());
    }
    Ok(start_roots)
}

fn path_to_relative_unix(cwd: &Path, path: &Path) -> Option<String> {
    if let Some(relative) = to_relative_unix_path(cwd, path) {
        return Some(relative);
    }

    let cwd_components = cwd
        .components()
        .filter(|component| !matches!(component, Component::CurDir))
        .collect::<Vec<_>>();
    let path_components = path
        .components()
        .filter(|component| !matches!(component, Component::CurDir))
        .collect::<Vec<_>>();

    if cwd_components.first() != path_components.first() {
        return None;
    }

    let mut common_length = 0usize;
    while common_length < cwd_components.len()
        && common_length < path_components.len()
        && cwd_components[common_length] == path_components[common_length]
    {
        common_length += 1;
    }

    let mut relative_parts = Vec::new();
    for component in &cwd_components[common_length..] {
        if matches!(component, Component::Normal(_) | Component::ParentDir) {
            relative_parts.push("..".to_string());
        }
    }
    for component in &path_components[common_length..] {
        match component {
            Component::Normal(value) => relative_parts.push(value.to_string_lossy().into_owned()),
            Component::ParentDir => relative_parts.push("..".to_string()),
            _ => {}
        }
    }

    Some(relative_parts.join("/"))
}
