use crate::compiled_glob::CompiledGlob;
use crate::walker::{EntryKind, WalkError, WalkEvent, WalkMessage, WalkerOptions};
use fts::fts::{Fts, FtsInfo, FtsSetOption, fts_option};
use hashbrown::HashMap;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::JoinSet;

const TRANSITION_CACHE_CAPACITY: usize = 16 * 1024;

#[derive(Clone)]
struct RootJob {
    path: PathBuf,
    root_states: Vec<usize>,
}

pub(super) fn spawn_single_with_options(
    compiled: CompiledGlob,
    options: WalkerOptions,
) -> mpsc::Receiver<WalkMessage> {
    let (tx, rx) = mpsc::channel(options.channel_capacity.max(1));

    tokio::spawn(async move {
        let compiled = Arc::new(compiled);
        let tx_on_join = tx.clone();
        let files_only = options.files_only;
        let max_parallelism = options
            .max_parallelism
            .unwrap_or_else(default_parallelism)
            .max(1);

        let joined = tokio::task::spawn_blocking({
            let compiled = Arc::clone(&compiled);
            move || prepare_jobs(compiled.as_ref(), files_only)
        })
        .await;

        let (jobs, initial_events) = match joined {
            Ok(value) => value,
            Err(err) => {
                let _ = tx_on_join
                    .send(Err(WalkError::Io {
                        path: PathBuf::from("<prepare_jobs>"),
                        source: io::Error::other(err.to_string()),
                    }))
                    .await;
                return;
            }
        };

        for event in initial_events {
            if tx.send(Ok(event)).await.is_err() {
                return;
            }
        }

        if jobs.is_empty() {
            return;
        }

        let sem = Arc::new(Semaphore::new(max_parallelism));
        let mut join_set = JoinSet::new();

        for job in jobs {
            if tx.is_closed() {
                break;
            }
            let permit = match sem.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => break,
            };
            let tx_worker = tx.clone();
            let compiled = Arc::clone(&compiled);
            join_set.spawn(async move {
                run_job_blocking(compiled, files_only, tx_worker, job, permit).await;
            });
        }

        while let Some(joined) = join_set.join_next().await {
            if let Err(err) = joined {
                let _ = tx_on_join
                    .send(Err(WalkError::Io {
                        path: PathBuf::from("<join>"),
                        source: io::Error::other(err.to_string()),
                    }))
                    .await;
            }
        }
    });

    rx
}

async fn run_job_blocking(
    compiled: Arc<CompiledGlob>,
    files_only: bool,
    tx: mpsc::Sender<WalkMessage>,
    job: RootJob,
    _permit: OwnedSemaphorePermit,
) {
    let tx_on_join = tx.clone();
    let joined =
        tokio::task::spawn_blocking(move || run_fts_job(compiled, files_only, tx, job)).await;
    if let Err(err) = joined {
        let _ = tx_on_join
            .send(Err(WalkError::Io {
                path: PathBuf::from("<join_worker>"),
                source: io::Error::other(err.to_string()),
            }))
            .await;
    }
}

fn run_fts_job(
    compiled: Arc<CompiledGlob>,
    files_only: bool,
    tx: mpsc::Sender<WalkMessage>,
    job: RootJob,
) {
    let root_string = job.path.to_string_lossy().to_string();
    let mut fts = match Fts::new(
        vec![root_string],
        fts_option::Flags::PHYSICAL | fts_option::Flags::NOCHDIR,
        None,
    ) {
        Ok(fts) => fts,
        Err(err) => {
            let _ = tx.blocking_send(Err(WalkError::Io {
                path: job.path,
                source: io::Error::other(format!("failed to initialize fts: {err:?}")),
            }));
            return;
        }
    };

    let mut level_states: Vec<Vec<usize>> = Vec::new();
    let mut transition_cache: HashMap<u64, HashMap<String, Vec<usize>>> = HashMap::new();
    let mut transition_cache_len = 0usize;

    while let Some(entry) = fts.read() {
        let level = match usize::try_from(entry.level) {
            Ok(level) => level,
            Err(_) => continue,
        };

        match entry.info {
            FtsInfo::IsDot | FtsInfo::IsDirPost => {
                if level < level_states.len() {
                    level_states.truncate(level);
                }
                continue;
            }
            FtsInfo::IsErr | FtsInfo::IsDontRead | FtsInfo::IsNoStat => {
                let source = if entry.error == 0 {
                    io::Error::other("fts reported an unreadable entry")
                } else {
                    io::Error::from_raw_os_error(entry.error)
                };
                let _ = tx.blocking_send(Err(WalkError::Io {
                    path: entry.path.clone(),
                    source,
                }));
                continue;
            }
            _ => {}
        }

        let is_dir = matches!(entry.info, FtsInfo::IsDir | FtsInfo::IsDirCyclic);
        let states = if level == 0 {
            job.root_states.clone()
        } else {
            let parent = match level_states.get(level.saturating_sub(1)) {
                Some(parent) => parent,
                None => {
                    if is_dir {
                        let _ = fts.set(&entry, FtsSetOption::Skip);
                    }
                    continue;
                }
            };
            if parent.is_empty() {
                if is_dir {
                    let _ = fts.set(&entry, FtsSetOption::Skip);
                }
                continue;
            }

            let Some(name) = entry.name.to_str() else {
                if is_dir {
                    let _ = fts.set(&entry, FtsSetOption::Skip);
                }
                continue;
            };

            let signature = states_signature(parent);
            if let Some(by_name) = transition_cache.get(&signature)
                && let Some(cached) = by_name.get(name)
            {
                cached.clone()
            } else {
                let next = compiled.advance_states(parent, name);
                if transition_cache_len >= TRANSITION_CACHE_CAPACITY {
                    transition_cache.clear();
                    transition_cache_len = 0;
                }
                let by_name = transition_cache.entry(signature).or_default();
                if by_name.insert(name.to_owned(), next.clone()).is_none() {
                    transition_cache_len += 1;
                }
                next
            }
        };

        if level_states.len() <= level {
            level_states.resize(level + 1, Vec::new());
        }
        level_states[level] = states.clone();
        level_states.truncate(level + 1);

        if states.is_empty() {
            if is_dir {
                let _ = fts.set(&entry, FtsSetOption::Skip);
            }
            continue;
        }

        let kind = entry_kind(entry.info.clone());
        if compiled.is_match_state(&states) && (!files_only || kind == EntryKind::File) {
            let _ = tx.blocking_send(Ok(WalkEvent {
                path: entry.path.clone(),
                kind,
            }));
        }

        if is_dir && !compiled.needs_directory_scan(&states) {
            let _ = fts.set(&entry, FtsSetOption::Skip);
        }
    }
}

fn prepare_jobs(compiled: &CompiledGlob, files_only: bool) -> (Vec<RootJob>, Vec<WalkEvent>) {
    let roots = normalize_roots(compiled.start_paths());
    let mut jobs = Vec::new();
    let mut initial_events = Vec::new();

    for root in roots {
        let metadata = match std::fs::metadata(root.as_path()) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => continue,
        };
        if !metadata.is_dir() {
            continue;
        }

        let root_states = compiled.states_for_path(root.as_path());
        if root_states.is_empty() {
            continue;
        }

        // If root count is tiny, split by first-level entries to increase parallelism.
        // Fall back to root-only job when splitting is unsafe or unproductive.
        let sharded = shard_root_jobs(
            compiled,
            root.as_path(),
            &root_states,
            files_only,
            &mut jobs,
            &mut initial_events,
        );
        if !sharded {
            jobs.push(RootJob {
                path: root,
                root_states,
            });
        }
    }

    (jobs, initial_events)
}

fn shard_root_jobs(
    compiled: &CompiledGlob,
    root: &Path,
    root_states: &[usize],
    files_only: bool,
    jobs: &mut Vec<RootJob>,
    initial_events: &mut Vec<WalkEvent>,
) -> bool {
    let mut reader = match std::fs::read_dir(root) {
        Ok(reader) => reader,
        Err(_) => return false,
    };

    let mut local_jobs = Vec::new();
    let mut sharded = false;

    while let Some(entry) = reader.next().transpose().ok().flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return false;
        };

        let next_states = compiled.advance_states(root_states, name);
        if next_states.is_empty() {
            continue;
        }

        let mut kind = None;
        if let Ok(file_type) = entry.file_type() {
            if file_type.is_dir() {
                kind = Some(EntryKind::Dir);
            } else if file_type.is_file() {
                kind = Some(EntryKind::File);
            } else if file_type.is_symlink() {
                kind = symlink_kind(entry.path().as_path());
            } else {
                kind = Some(EntryKind::Other);
            }
        }

        if compiled.is_match_state(&next_states)
            && let Some(kind) = kind
            && (!files_only || kind == EntryKind::File)
        {
            initial_events.push(WalkEvent {
                path: entry.path(),
                kind,
            });
        }

        if kind == Some(EntryKind::Dir) && compiled.needs_directory_scan(&next_states) {
            local_jobs.push(RootJob {
                path: entry.path(),
                root_states: next_states,
            });
            sharded = true;
        }
    }

    if sharded {
        jobs.extend(local_jobs);
    }
    sharded
}

fn symlink_kind(path: &Path) -> Option<EntryKind> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Some(EntryKind::Dir),
        Ok(metadata) if metadata.is_file() => Some(EntryKind::File),
        Ok(_) => Some(EntryKind::Other),
        Err(_) => None,
    }
}

fn normalize_roots(mut roots: Vec<PathBuf>) -> Vec<PathBuf> {
    roots.sort();
    roots.dedup();

    let mut keep = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        if seen
            .iter()
            .any(|base: &PathBuf| is_same_or_child(base.as_path(), root.as_path()))
        {
            continue;
        }
        seen.insert(root.clone());
        keep.push(root);
    }
    keep
}

fn is_same_or_child(base: &Path, candidate: &Path) -> bool {
    candidate == base || candidate.starts_with(base)
}

fn states_signature(states: &[usize]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for state in states {
        state.hash(&mut hasher);
    }
    hasher.finish()
}

fn entry_kind(info: FtsInfo) -> EntryKind {
    match info {
        FtsInfo::IsDir | FtsInfo::IsDirCyclic | FtsInfo::IsDirPost => EntryKind::Dir,
        FtsInfo::IsFile => EntryKind::File,
        FtsInfo::IsSymlink | FtsInfo::IsSymlinkNone => EntryKind::Symlink,
        _ => EntryKind::Other,
    }
}

fn default_parallelism() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|x| x.get())
        .unwrap_or(1);
    std::cmp::max(4, cores.saturating_mul(2))
}
