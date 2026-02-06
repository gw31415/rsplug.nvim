use crate::compiled_glob::CompiledGlob;
use crate::walker::{EntryKind, WalkError, WalkEvent, WalkMessage, WalkerOptions};
use hashbrown::HashSet;
use std::cmp::max;
use std::fs::FileType;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::JoinSet;

struct MatchProgram {
    compiled: CompiledGlob,
}

impl MatchProgram {
    fn new(compiled: CompiledGlob) -> Self {
        Self { compiled }
    }

    fn initial_states(&self) -> Vec<usize> {
        self.compiled.initial_states()
    }

    fn states_for_path(&self, path: &Path) -> Vec<usize> {
        self.compiled.states_for_path(path)
    }

    fn advance_states(&self, current: &[usize], part: &str) -> Vec<usize> {
        self.compiled.advance_states(current, part)
    }

    fn is_match_state(&self, current: &[usize]) -> bool {
        self.compiled.is_match_state(current)
    }

    fn literal_candidates(&self, current: &[usize]) -> Vec<String> {
        self.compiled.literal_candidates(current)
    }

    fn needs_directory_scan(&self, current: &[usize]) -> bool {
        self.compiled.needs_directory_scan(current)
    }
}

#[derive(Clone)]
struct TraversalCtx {
    program: Arc<MatchProgram>,
    visited: Arc<Mutex<HashSet<VisitKey>>>,
    tx: mpsc::Sender<WalkMessage>,
    files_only: bool,
}

#[derive(Clone)]
struct State {
    path: PathBuf,
    match_states: Vec<usize>,
    kind_hint: Option<EntryKind>,
}

type DirIdentity = PathBuf;
type VisitKey = (DirIdentity, u64);

pub(super) fn spawn_single_with_options(
    compiled: CompiledGlob,
    options: WalkerOptions,
) -> mpsc::Receiver<WalkMessage> {
    let (tx, rx) = mpsc::channel(options.channel_capacity.max(1));
    let max_parallelism = options.max_parallelism.unwrap_or_else(default_parallelism);
    let sem = Arc::new(Semaphore::new(max_parallelism.max(1)));
    let ctx = TraversalCtx {
        program: Arc::new(MatchProgram::new(compiled)),
        visited: Arc::new(Mutex::new(HashSet::new())),
        tx,
        files_only: options.files_only,
    };

    let seed_paths = ctx.program.compiled.start_paths();
    let mut seeded = Vec::new();
    for path in seed_paths {
        let states = ctx.program.states_for_path(path.as_path());
        if states.is_empty() {
            continue;
        }
        seeded.push(State {
            path,
            match_states: states,
            kind_hint: Some(EntryKind::Dir),
        });
    }

    if seeded.is_empty() {
        seeded.push(State {
            path: PathBuf::from(std::path::MAIN_SEPARATOR.to_string()),
            match_states: ctx.program.initial_states(),
            kind_hint: None,
        });
    }

    tokio::spawn(async move {
        let mut frontier = seeded;

        while !frontier.is_empty() && !ctx.tx.is_closed() {
            let current_level = std::mem::take(&mut frontier);
            let mut join_set = JoinSet::new();

            for state in current_level {
                if ctx.tx.is_closed() {
                    break;
                }
                let permit = match sem.clone().acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => break,
                };
                let child_ctx = ctx.clone();
                join_set.spawn(async move { process_state(child_ctx, state, permit).await });
            }

            while let Some(joined) = join_set.join_next().await {
                match joined {
                    Ok(next_states) => frontier.extend(next_states),
                    Err(err) => {
                        let _ = ctx
                            .tx
                            .send(Err(WalkError::Io {
                                path: PathBuf::from("<join>"),
                                source: io::Error::other(err.to_string()),
                            }))
                            .await;
                    }
                }
            }
        }
    });

    rx
}

fn default_parallelism() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|x| x.get())
        .unwrap_or(1);
    max(4, cores.saturating_mul(4))
}

fn states_signature(states: &[usize]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for state in states {
        state.hash(&mut hasher);
    }
    hasher.finish()
}

async fn process_state(
    ctx: TraversalCtx,
    state: State,
    _permit: OwnedSemaphorePermit,
) -> Vec<State> {
    if ctx.tx.is_closed() || state.match_states.is_empty() {
        return Vec::new();
    }

    if !ctx.files_only || !matches!(state.kind_hint, Some(EntryKind::Dir | EntryKind::Other)) {
        if ctx.program.is_match_state(&state.match_states) {
            finalize_match(&ctx, state.path.clone(), state.kind_hint).await;
        }
    }

    if matches!(state.kind_hint, Some(EntryKind::File | EntryKind::Other)) {
        return Vec::new();
    }

    let signature = states_signature(&state.match_states);
    let mut out = Vec::new();
    let literal_candidates = ctx.program.literal_candidates(&state.match_states);
    let mut handled_names = HashSet::new();
    for literal in literal_candidates {
        handled_names.insert(literal.clone());
        let next_states = ctx.program.advance_states(&state.match_states, &literal);
        if next_states.is_empty() {
            continue;
        }
        let candidate_path = state.path.join(&literal);
        match tokio::fs::symlink_metadata(&candidate_path).await {
            Ok(metadata) => out.push(State {
                path: candidate_path,
                match_states: next_states,
                kind_hint: Some(entry_kind_from_file_type(metadata.file_type())),
            }),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::PermissionDenied
                        | io::ErrorKind::NotADirectory
                ) => {}
            Err(err) => {
                send_error(&ctx.tx, candidate_path, err).await;
            }
        }
    }

    if !ctx.program.needs_directory_scan(&state.match_states) {
        return out;
    }

    if !mark_dir_visited(&ctx.visited, &state.path, state.kind_hint, signature).await {
        return out;
    }

    let mut dir = match tokio::fs::read_dir(&state.path).await {
        Ok(d) => d,
        Err(err) if err.kind() == io::ErrorKind::NotADirectory => {
            return out;
        }
        Err(err) => {
            send_error(&ctx.tx, state.path, err).await;
            return out;
        }
    };

    while let Ok(Some(entry)) = dir.next_entry().await {
        if ctx.tx.is_closed() {
            break;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if handled_names.contains(name) {
            continue;
        }
        let next_states = ctx.program.advance_states(&state.match_states, name);
        if next_states.is_empty() {
            continue;
        }
        let mut kind_hint = None;
        if ctx.files_only {
            if let Ok(file_type) = entry.file_type().await {
                let kind = entry_kind_from_file_type(file_type);
                kind_hint = Some(kind);
                if kind == EntryKind::File && !ctx.program.is_match_state(&next_states) {
                    kind_hint = None;
                }
            }
        } else if ctx.program.is_match_state(&next_states)
            && let Ok(file_type) = entry.file_type().await
        {
            kind_hint = Some(entry_kind_from_file_type(file_type));
        }
        out.push(State {
            path: entry.path(),
            match_states: next_states,
            kind_hint,
        });
    }

    out
}

async fn finalize_match(ctx: &TraversalCtx, path: PathBuf, kind_hint: Option<EntryKind>) {
    let kind = match kind_hint {
        Some(kind) => Ok(kind),
        None => entry_kind(&path).await,
    };
    match kind {
        Ok(kind) => {
            if ctx.files_only && kind != EntryKind::File {
                return;
            }
            let _ = ctx.tx.send(Ok(WalkEvent { path, kind })).await;
        }
        Err(err) => {
            send_error(&ctx.tx, path, err).await;
        }
    }
}

async fn entry_kind(path: &Path) -> io::Result<EntryKind> {
    let symlink_meta = tokio::fs::symlink_metadata(path).await?;
    Ok(entry_kind_from_file_type(symlink_meta.file_type()))
}

fn entry_kind_from_file_type(file_type: FileType) -> EntryKind {
    if file_type.is_symlink() {
        return EntryKind::Symlink;
    }
    if file_type.is_dir() {
        return EntryKind::Dir;
    }
    if file_type.is_file() {
        return EntryKind::File;
    }
    EntryKind::Other
}

async fn send_error(tx: &mpsc::Sender<WalkMessage>, path: PathBuf, source: io::Error) {
    let _ = tx.send(Err(WalkError::Io { path, source })).await;
}

async fn mark_dir_visited(
    visited: &Arc<Mutex<HashSet<VisitKey>>>,
    path: &Path,
    kind_hint: Option<EntryKind>,
    signature: u64,
) -> bool {
    if matches!(kind_hint, Some(EntryKind::File | EntryKind::Other)) {
        return false;
    }

    let metadata = match tokio::fs::metadata(path).await {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    if !metadata.is_dir() {
        return false;
    }
    let key = path.to_path_buf();
    let mut guard = visited.lock().expect("visited lock poisoned");
    guard.insert((key, signature))
}
