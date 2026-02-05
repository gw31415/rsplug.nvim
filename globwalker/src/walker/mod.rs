mod init;

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use hashbrown::HashSet;
use tokio::fs;
use tokio::task::JoinSet;

use crate::fs_resolver::{DirectoryScanResult, DirectoryTask, FileEntry};
use crate::pattern::{CompiledRules, could_match_subtree, matches_last_rule};

pub struct GlobWalker {
    root: PathBuf,
    compiled_rules: CompiledRules,
    pending_directories: VecDeque<DirectoryTask>,
    visited_directories: HashSet<PathBuf>,
    seen_files: HashSet<PathBuf>,
    ready_paths: Vec<String>,
    deferred_scan_error: Option<io::Error>,
    deadline: Option<Instant>,
}

impl GlobWalker {
    pub async fn new(patterns: impl IntoIterator<Item = String>, cwd: &Path) -> io::Result<Self> {
        let root = init::resolve_root(cwd)?;
        let patterns: Vec<String> = patterns.into_iter().collect();
        let cwd_prefixes = init::build_prefixes_for_pattern_resolution(cwd, &patterns)?;
        let compiled_rules = init::compile_rules_with_limits(patterns, &cwd_prefixes)?;
        let mut pending_directories = VecDeque::new();
        let mut visited_directories = HashSet::new();

        if !compiled_rules.include_patterns.is_empty() {
            let seeded = init::seed_start_directories(
                &compiled_rules.include_patterns,
                &mut pending_directories,
                &mut visited_directories,
            )
            .await?;
            if !seeded {
                visited_directories.insert(root.clone());
                pending_directories.push_back(DirectoryTask {
                    absolute_path: root.clone(),
                    relative_path: String::new(),
                });
            }
        }

        Ok(Self {
            root,
            compiled_rules,
            pending_directories,
            visited_directories,
            seen_files: HashSet::new(),
            ready_paths: Vec::new(),
            deferred_scan_error: None,
            deadline: None,
        })
    }

    pub async fn next(&mut self) -> io::Result<Option<String>> {
        loop {
            if self.is_timed_out() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "globwalker timed out",
                ));
            }
            if let Some(path) = self.ready_paths.pop() {
                return Ok(Some(path));
            }

            if self.pending_directories.is_empty() {
                if let Some(error) = self.deferred_scan_error.take() {
                    return Err(error);
                }
                return Ok(None);
            }

            self.process_pending_batch().await?;
        }
    }

    pub fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = Some(deadline);
    }

    fn defer_scan_error(&mut self, error: io::Error) {
        if self.deferred_scan_error.is_none() {
            self.deferred_scan_error = Some(error);
        }
    }

    async fn process_scan_result(&mut self, scan_result: DirectoryScanResult) -> io::Result<()> {
        match scan_result {
            DirectoryScanResult::ChildDirectory(dir) => self.enqueue_pruned_children(dir).await,
            DirectoryScanResult::File(file) => self.collect_matched_files(file).await,
        }
    }

    async fn collect_matched_files(&mut self, file: FileEntry) -> io::Result<()> {
        let absolute_for_match = normalize_path_for_match(file.absolute_path.as_path());
        if !matches_last_rule(&absolute_for_match, &self.compiled_rules.ordered_rules) {
            return Ok(());
        }

        let identity = fs::canonicalize(&file.absolute_path).await?;

        if self.seen_files.insert(identity) {
            self.ready_paths.push(render_output_path(
                self.root.as_path(),
                file.absolute_path.as_path(),
            ));
        }

        Ok(())
    }

    async fn enqueue_pruned_children(&mut self, child_dir: DirectoryTask) -> io::Result<()> {
        let absolute_for_match = normalize_path_for_match(child_dir.absolute_path.as_path());
        if !could_match_subtree(&absolute_for_match, &self.compiled_rules.include_prefixes) {
            return Ok(());
        }

        let identity = fs::canonicalize(child_dir.absolute_path.as_path()).await?;

        if self.visited_directories.insert(identity) {
            self.pending_directories.push_back(child_dir);
        }

        Ok(())
    }

    fn is_timed_out(&mut self) -> bool {
        let Some(deadline) = self.deadline else {
            return false;
        };
        Instant::now() >= deadline
    }

    async fn process_pending_batch(&mut self) -> io::Result<()> {
        let tasks = self.pending_directories.drain(..).collect::<Vec<_>>();
        if tasks.is_empty() {
            return Ok(());
        }

        let mut join_set = JoinSet::new();
        for task in tasks {
            join_set.spawn(scan_directory_task(task));
        }

        while !join_set.is_empty() {
            if self.is_timed_out() {
                join_set.abort_all();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "globwalker timed out",
                ));
            }

            let Some(joined) = join_set.join_next().await else {
                break;
            };
            let batch = match joined {
                Ok(batch) => batch,
                Err(error) => {
                    self.defer_scan_error(io::Error::other(format!(
                        "scan task join error: {error}"
                    )));
                    continue;
                }
            };

            for scan_result in batch.results {
                self.process_scan_result(scan_result).await?;
            }
            if let Some(error) = batch.error {
                self.defer_scan_error(error);
            }
        }

        Ok(())
    }
}

#[derive(Debug)]
struct ScanBatch {
    results: Vec<DirectoryScanResult>,
    error: Option<io::Error>,
}

async fn scan_directory_task(task: DirectoryTask) -> ScanBatch {
    let mut stream = match task.stream().await {
        Ok(stream) => stream,
        Err(error) => {
            return ScanBatch {
                results: Vec::new(),
                error: Some(error),
            };
        }
    };

    let mut results = Vec::new();
    loop {
        match stream.next().await {
            Ok(Some(scan_result)) => results.push(scan_result),
            Ok(None) => {
                return ScanBatch {
                    results,
                    error: None,
                };
            }
            Err(error) => {
                return ScanBatch {
                    results,
                    error: Some(error),
                };
            }
        }
    }
}

fn normalize_path_for_match(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches('/')
        .to_string()
}

fn render_output_path(root: &Path, absolute_path: &Path) -> String {
    if let Ok(relative) = absolute_path.strip_prefix(root)
        && !relative.as_os_str().is_empty()
    {
        return relative.to_string_lossy().replace('\\', "/");
    }
    absolute_path.to_string_lossy().replace('\\', "/")
}
