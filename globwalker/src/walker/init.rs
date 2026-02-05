use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};

use hashbrown::HashSet;
use tokio::fs;
use tokio::task::JoinSet;
use wildmatch::WildMatch;

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
    _patterns: &[String],
) -> io::Result<Vec<String>> {
    Ok(build_cwd_prefixes(cwd))
}

pub(super) async fn seed_start_directories(
    include_patterns: &[String],
    pending_directories: &mut VecDeque<DirectoryTask>,
    visited_directories: &mut HashSet<PathBuf>,
) -> io::Result<bool> {
    if include_patterns.is_empty() {
        return Ok(false);
    }

    let mut seed_directories = Vec::new();
    for pattern in include_patterns {
        seed_directories.extend(find_seed_directories(pattern).await?);
    }
    if seed_directories.is_empty() {
        return Ok(false);
    }

    let mut seeded = false;
    for absolute_path in seed_directories {
        let metadata = match fs::metadata(&absolute_path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !metadata.is_dir() {
            continue;
        }

        let relative_path = normalize_path_for_match(absolute_path.as_path());
        if visited_directories.insert(absolute_path.clone()) {
            pending_directories.push_back(DirectoryTask {
                absolute_path,
                relative_path,
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

fn build_cwd_prefixes(cwd: &Path) -> Vec<String> {
    vec![cwd.to_string_lossy().replace('\\', "/")]
}

#[derive(Debug, Clone)]
enum SegmentsMatcher {
    AnyPath(String),
    Glob(WildMatch),
    Descends,
}

async fn find_seed_directories(pattern: &str) -> io::Result<Vec<PathBuf>> {
    let segments = parse_directory_segments(pattern);
    if segments.is_empty() {
        return Ok(Vec::new());
    }

    let mut candidates = vec![PathBuf::from("/")];
    let mut index = 0usize;
    while index < segments.len() {
        let seg = segments[index].clone();
        match seg {
            SegmentsMatcher::AnyPath(part) => {
                candidates = candidates
                    .into_iter()
                    .map(move |base| {
                        let candidate = base.join(part.clone());
                        async move {
                            match fs::metadata(&candidate).await {
                                Ok(meta) if meta.is_dir() => Some(candidate),
                                _ => None,
                            }
                        }
                    })
                    .collect::<JoinSet<_>>()
                    .join_all()
                    .await
                    .into_iter()
                    .flatten()
                    .collect();
                index += 1;
            }
            SegmentsMatcher::Glob(pattern) => {
                candidates = expand_glob_directories(candidates, &pattern).await?;
                index += 1;
            }
            SegmentsMatcher::Descends => {
                let Some(next) = segments.get(index + 1) else {
                    return Ok(candidates);
                };
                if matches!(next, SegmentsMatcher::Descends) {
                    index += 1;
                    continue;
                }
                candidates = expand_descends_directories(candidates, next).await?;
                index += 2;
            }
        }

        if candidates.is_empty() {
            return Ok(Vec::new());
        }
    }

    Ok(candidates)
}

fn parse_directory_segments(pattern: &str) -> Vec<SegmentsMatcher> {
    let parts = pattern.split('/').filter(|segment| !segment.is_empty());
    let mut segments = Vec::new();
    let mut all_parts = parts.collect::<Vec<_>>();
    if !all_parts.is_empty() && all_parts.last().copied() != Some("**") {
        all_parts.pop();
    }

    for part in all_parts {
        if part == "**" {
            if !matches!(segments.last(), Some(SegmentsMatcher::Descends)) {
                segments.push(SegmentsMatcher::Descends);
            }
            continue;
        }
        if segment_contains_wildcard(part) {
            segments.push(SegmentsMatcher::Glob(WildMatch::new(part)));
        } else {
            segments.push(SegmentsMatcher::AnyPath(part.to_string()));
        }
    }
    segments
}

async fn expand_glob_directories(
    bases: Vec<PathBuf>,
    pattern: &WildMatch,
) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for base in bases {
        let mut reader = match fs::read_dir(base.as_path()).await {
            Ok(reader) => reader,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };

        while let Some(entry) = reader.next_entry().await? {
            if !entry_is_dir(&entry).await? {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if pattern.matches(name.as_ref()) {
                out.push(entry.path());
            }
        }
    }
    Ok(out)
}

async fn expand_descends_directories(
    bases: Vec<PathBuf>,
    next: &SegmentsMatcher,
) -> io::Result<Vec<PathBuf>> {
    if bases.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();

    let res = bases
        .into_iter()
        .map(move |base| {
            let next = next.clone();
            async move { expand_descends_from_base(base, &next).await }
        })
        .collect::<JoinSet<_>>()
        .join_all();

    for res in res.await.into_iter().flatten() {
        out.extend(res);
    }

    Ok(out)
}

async fn expand_descends_from_base(
    base: PathBuf,
    next: &SegmentsMatcher,
) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut queue = VecDeque::from([base]);
    while let Some(current) = queue.pop_front() {
        let mut reader = match fs::read_dir(current.as_path()).await {
            Ok(reader) => reader,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };

        while let Some(entry) = reader.next_entry().await? {
            if !entry_is_dir(&entry).await? {
                continue;
            }
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();

            let matched = match next.clone() {
                SegmentsMatcher::AnyPath(expected) => name.as_ref() == expected,
                SegmentsMatcher::Glob(pattern) => pattern.matches(name.as_ref()),
                SegmentsMatcher::Descends => true,
            };
            if matched {
                out.push(path.clone());
            }
            queue.push_back(path);
        }
    }

    Ok(out)
}

async fn entry_is_dir(entry: &fs::DirEntry) -> io::Result<bool> {
    let file_type = entry.file_type().await?;
    if file_type.is_dir() {
        return Ok(true);
    }
    if !file_type.is_symlink() {
        return Ok(false);
    }
    match fs::metadata(entry.path()).await {
        Ok(meta) => Ok(meta.is_dir()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn segment_contains_wildcard(segment: &str) -> bool {
    segment.contains('*') || segment.contains('?')
}

fn normalize_path_for_match(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches('/')
        .to_string()
}
