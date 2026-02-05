use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use hashbrown::HashSet;

use crate::fs_resolver::DirectoryTask;
use crate::pattern::{CompiledRules, compile_rules, initialize_pattern};

pub(super) const MAX_PATTERN_COUNT: usize = 4096;
pub(super) const MAX_PATTERN_LENGTH: usize = 4096;

pub(super) fn resolve_root(cwd: &Path) -> io::Result<PathBuf> {
    absolute_root(cwd)
}

pub(super) fn compile_rules_with_limits(
    patterns: impl IntoIterator<Item = String>,
    cwd_prefixes: &[String],
) -> io::Result<CompiledRules> {
    let mut initialized_patterns = Vec::new();

    for (index, pattern) in patterns.into_iter().enumerate() {
        if index >= MAX_PATTERN_COUNT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Pattern count exceeds maximum of {MAX_PATTERN_COUNT}"),
            ));
        }
        if pattern.len() > MAX_PATTERN_LENGTH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Pattern length exceeds maximum of {MAX_PATTERN_LENGTH}"),
            ));
        }
        initialized_patterns.push(initialize_pattern(index, pattern, cwd_prefixes)?);
    }

    Ok(compile_rules(initialized_patterns))
}

pub(super) fn build_prefixes_for_pattern_resolution(
    cwd: &Path,
    patterns: &[String],
) -> io::Result<Vec<String>> {
    if !contains_absolute_pattern(patterns) {
        return Ok(Vec::new());
    }
    Ok(build_cwd_prefixes(cwd))
}

pub(super) fn seed_start_directories(
    root: &Path,
    include_prefixes: &[String],
    pending_directories: &mut VecDeque<DirectoryTask>,
    visited_directories: &mut HashSet<PathBuf>,
) -> io::Result<bool> {
    if include_prefixes.iter().any(|prefix| prefix.is_empty()) {
        return Ok(false);
    }

    let mut seeded = false;
    for prefix in include_prefixes {
        let absolute_path = root.join(prefix);
        let metadata = match fs::metadata(&absolute_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !metadata.is_dir() {
            continue;
        }

        if visited_directories.insert(absolute_path.clone()) {
            pending_directories.push_back(DirectoryTask {
                absolute_path,
                relative_path: prefix.clone(),
            });
            seeded = true;
        }
    }

    Ok(seeded)
}

fn absolute_root(cwd: &Path) -> io::Result<PathBuf> {
    if cwd.is_absolute() {
        return Ok(cwd.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(cwd))
}

fn contains_absolute_pattern(patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        let body = pattern.strip_prefix('!').unwrap_or(pattern);
        Path::new(body).is_absolute()
    })
}

fn build_cwd_prefixes(cwd: &Path) -> Vec<String> {
    vec![cwd.to_string_lossy().replace('\\', "/")]
}
