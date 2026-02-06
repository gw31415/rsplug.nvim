use std::io;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use ignore::{WalkBuilder, WalkState};
use tokio::task::spawn_blocking;
use tokio::time::Instant as TokioInstant;
use walker::compiled_glob::CompiledGlob;
use walker::walker::{EntryKind, Walker, WalkerOptions};

use crate::bench_rules::matches_compiled_rules;
use crate::bench_types::{AttemptOutcome, AttemptResult, BenchmarkKind};

pub(crate) async fn run_benchmark_attempt(
    kind: BenchmarkKind,
    cwd: &Path,
    raw_patterns: &[String],
    rules: &Arc<CompiledGlob>,
    timeout: Duration,
) -> io::Result<AttemptOutcome> {
    match kind {
        BenchmarkKind::IgnoreParallel => {
            let cwd = cwd.to_path_buf();
            let rules = Arc::clone(rules);
            run_benchmark(timeout, move || measure_ignore(cwd, rules, timeout)).await
        }
        BenchmarkKind::Walker => {
            let patterns = raw_patterns.to_vec();
            run_benchmark(timeout, move || measure_walker(patterns, timeout)).await
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

async fn measure_ignore(
    cwd: PathBuf,
    rules: Arc<CompiledGlob>,
    timeout: Duration,
) -> io::Result<AttemptOutcome> {
    spawn_blocking(move || {
        let started = Instant::now();
        let deadline = started + timeout;
        let seen = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let timed_out = Arc::new(AtomicBool::new(false));
        let deferred_error = Arc::new(std::sync::Mutex::new(None));

        for start_root in [cwd] {
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
                let deferred_error = Arc::clone(&deferred_error);
                let rules = Arc::clone(&rules);
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
                            if !matches_compiled_rules(path.as_path(), &rules) {
                                return WalkState::Continue;
                            }
                            let identity = match std::fs::canonicalize(path) {
                                Ok(canonical) => canonical,
                                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                                    return WalkState::Continue;
                                }
                                Err(error) => {
                                    if let Ok(mut stored_error) = deferred_error.lock()
                                        && stored_error.is_none()
                                    {
                                        *stored_error = Some(error);
                                    }
                                    return WalkState::Continue;
                                }
                            };
                            if let Ok(mut seen_paths) = seen.lock() {
                                seen_paths.insert(identity);
                            } else {
                                if let Ok(mut stored_error) = deferred_error.lock() {
                                    *stored_error =
                                        Some(io::Error::other("ignore benchmark lock poisoned"));
                                }
                                return WalkState::Quit;
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            if let Ok(mut stored_error) = deferred_error.lock()
                                && stored_error.is_none()
                            {
                                *stored_error = Some(io::Error::other(error.to_string()));
                            }
                            return WalkState::Continue;
                        }
                    }
                    WalkState::Continue
                })
            });

            if timed_out.load(Ordering::Relaxed) || Instant::now() >= deadline {
                return Ok(AttemptOutcome::TimedOut);
            }
        }
        if let Ok(mut stored_error) = deferred_error.lock()
            && let Some(error) = stored_error.take()
        {
            return Err(error);
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

async fn measure_walker(patterns: Vec<String>, timeout: Duration) -> io::Result<AttemptOutcome> {
    let mut compiled = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        compiled.push(CompiledGlob::new(&pattern)?);
    }
    let options = WalkerOptions {
        files_only: true,
        ..WalkerOptions::default()
    };
    let mut rx = Walker::spawn_many_with_options(compiled, options);
    let started = Instant::now();
    let deadline = TokioInstant::now() + timeout;
    let mut matched_files = 0usize;

    loop {
        let msg = tokio::time::timeout_at(deadline, rx.recv()).await;
        let Some(msg) = (match msg {
            Ok(msg) => msg,
            Err(_) => return Ok(AttemptOutcome::TimedOut),
        }) else {
            break;
        };

        match msg {
            Ok(event) => {
                if event.kind == EntryKind::File {
                    matched_files += 1;
                }
            }
            Err(error)
                if matches!(
                    &error,
                    walker::walker::WalkError::Io { source, .. }
                        if source.kind() == io::ErrorKind::PermissionDenied
                            || source.kind() == io::ErrorKind::NotFound
                ) => {}
            Err(error) => return Err(io::Error::other(error.to_string())),
        }
    }

    Ok(AttemptOutcome::Completed(AttemptResult {
        elapsed: started.elapsed(),
        matched_files,
    }))
}
