mod init;

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use hashbrown::HashSet;
use tokio::fs;

use crate::fs_resolver::{DirectoryScanResult, DirectoryTask, FileEntry};
use crate::pattern::{CompiledRules, could_match_subtree, matches_last_rule};

pub struct GlobWalker {
    compiled_rules: CompiledRules,
    pending_directories: VecDeque<DirectoryTask>,
    visited_directories: HashSet<PathBuf>,
    seen_files: HashSet<PathBuf>,
    ready_paths: Vec<String>,
    deferred_scan_error: Option<io::Error>,
    deadline: Option<Instant>,
}

impl GlobWalker {
    pub fn new(patterns: impl IntoIterator<Item = String>, cwd: &Path) -> io::Result<Self> {
        let root = init::resolve_root(cwd)?;
        let patterns: Vec<String> = patterns.into_iter().collect();
        let cwd_prefixes = init::build_prefixes_for_pattern_resolution(cwd, &patterns)?;
        let compiled_rules = init::compile_rules_with_limits(patterns, &cwd_prefixes)?;
        let mut pending_directories = VecDeque::new();
        let mut visited_directories = HashSet::new();

        if !compiled_rules.include_prefixes.is_empty() {
            let seeded = init::seed_start_directories(
                root.as_path(),
                &compiled_rules.include_prefixes,
                &mut pending_directories,
                &mut visited_directories,
            )?;
            if !seeded {
                visited_directories.insert(root.clone());
                pending_directories.push_back(DirectoryTask {
                    absolute_path: root.clone(),
                    relative_path: String::new(),
                });
            }
        }

        Ok(Self {
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

            let Some(task) = self.pending_directories.pop_front() else {
                if let Some(error) = self.deferred_scan_error.take() {
                    return Err(error);
                }
                return Ok(None);
            };

            let mut stream = match task.stream().await {
                Ok(stream) => stream,
                Err(error) => {
                    self.defer_scan_error(error);
                    continue;
                }
            };

            loop {
                if self.is_timed_out() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "globwalker timed out",
                    ));
                }
                match stream.next().await {
                    Ok(Some(scan_result)) => self.process_scan_result(scan_result).await?,
                    Ok(None) => break,
                    Err(error) => {
                        self.defer_scan_error(error);
                        break;
                    }
                }
            }
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
        if !matches_last_rule(&file.relative_path, &self.compiled_rules.ordered_rules) {
            return Ok(());
        }

        let identity = fs::canonicalize(&file.absolute_path).await?;

        if self.seen_files.insert(identity) {
            self.ready_paths.push(file.relative_path);
        }

        Ok(())
    }

    async fn enqueue_pruned_children(&mut self, child_dir: DirectoryTask) -> io::Result<()> {
        if !could_match_subtree(
            &child_dir.relative_path,
            &self.compiled_rules.include_prefixes,
        ) {
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
}
